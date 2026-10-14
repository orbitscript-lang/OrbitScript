//! The `ffi` library (ADR-9): binding C functions from the C library the
//! test process already has, converting arguments and results, buffers and
//! raw memory, and the errors scripts get.

#![cfg(all(feature = "ffi", unix, target_pointer_width = "64"))]
#![cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]

use orbit::Lua;

fn lua() -> Lua {
    let mut lua = Lua::new();
    lua.enable_ffi();
    lua
}

/// Run `src` and show its results as `tostring` would, tab-separated.
fn run_in(lua: &mut Lua, src: &str) -> String {
    let out = match lua.exec(src.as_bytes(), "t") {
        Ok(v) => v,
        Err(e) => panic!("script failed: {e:?}"),
    };
    let vm = lua.vm();
    out.iter()
        .map(|&v| String::from_utf8_lossy(&vm.to_display(v)).into_owned())
        .collect::<Vec<_>>()
        .join("\t")
}

fn run(src: &str) -> String {
    run_in(&mut lua(), src)
}

/// The error message `src` raises.
fn error(src: &str) -> String {
    run(&format!(
        "local ok, e = pcall(function() {src} end) assert(not ok) return e"
    ))
}

const LIBC: &str = "local libc = ffi.open() ";

#[test]
fn absent_until_the_host_enables_it() {
    let mut lua = Lua::new();
    assert_eq!(
        run_in(&mut lua, "return ffi, package.loaded.ffi"),
        "nil\tnil"
    );
    lua.enable_ffi();
    lua.enable_ffi();
    assert_eq!(
        run_in(
            &mut lua,
            "return type(ffi), package.loaded.ffi == ffi, ffi.supported"
        ),
        "table\ttrue\ttrue"
    );
}

#[test]
fn integer_arguments_and_results() {
    let out = run(&format!(
        "{LIBC}
         local abs = libc:bind('abs', 'int', 'int')
         local labs = ffi.bind(libc, 'labs', 'long', 'long')
         local toupper = libc:bind('toupper', 'int', 'int')
         return abs(-7), labs(-(1 << 40)), toupper(97), abs(-2147483648)"
    ));
    assert_eq!(out, "7\t1099511627776\t65\t-2147483648");
}

#[test]
fn narrow_results_are_extended_from_their_own_width() {
    // `toupper` returns an int; read through narrower result types the
    // same register gives the low bits, sign- or zero-extended.
    let out = run(&format!(
        "{LIBC}
         local as_i8 = libc:bind('abs', 'i8', 'int')
         local as_u8 = libc:bind('abs', 'u8', 'int')
         local as_u32 = libc:bind('abs', 'u32', 'int')
         return as_i8(200), as_u8(200), as_u8(-1 & 0xff), as_u32(-5)"
    ));
    assert_eq!(out, "-56\t200\t255\t5");
}

#[test]
fn strings_in_and_out() {
    let out = run(&format!(
        "{LIBC}
         local strlen = libc:bind('strlen', 'size', 'cstr')
         local strcmp = libc:bind('strcmp', 'int', 'cstr', 'cstr')
         local getenv = libc:bind('getenv', 'cstr', 'cstr')
         local long = string.rep('x', 1000)
         return strlen('hello'), strlen(12345), strlen(long), strcmp('a', 'b') < 0,
                getenv('ORBIT_FFI_TEST_NO_SUCH_VARIABLE')"
    ));
    assert_eq!(out, "5\t5\t1000\ttrue\tnil");
}

#[test]
fn bytes_pass_without_a_copy_or_nul() {
    let out = run(&format!(
        "{LIBC}
         local memchr = libc:bind('memchr', 'ptr', 'bytes', 'int', 'size')
         local memcmp = libc:bind('memcmp', 'int', 'bytes', 'bytes', 'size')
         return memchr('abc', 0x7a, 3), memcmp('a\\0b', 'a\\0c', 3) < 0"
    ));
    assert_eq!(out, "nil\ttrue");
}

#[test]
fn floating_point() {
    let out = run(&format!(
        "{LIBC}
         local fabs = libc:bind('fabs', 'double', 'double')
         local fmax = libc:bind('fmax', 'double', 'double', 'double')
         local ldexp = libc:bind('ldexp', 'double', 'double', 'int')
         local fabsf = libc:bind('fabsf', 'float', 'float')
         local strtod = libc:bind('strtod', 'double', 'cstr', 'ptr')
         return fabs(-2.5), fmax(1, 3), ldexp(1.5, 4), fabsf(-0.5), strtod('6.25', nil)"
    ));
    assert_eq!(out, "2.5\t3.0\t24.0\t0.5\t6.25");
}

#[test]
fn out_parameters_through_buffers() {
    let out = run(&format!(
        "{LIBC}
         -- The text is a buffer, not a 'cstr' (a copy that lasts only for
         -- the call), because the end pointer points into it.
         local strtol = libc:bind('strtol', 'long', 'ptr', 'ptr', 'int')
         local frexp = libc:bind('frexp', 'double', 'double', 'ptr')
         local text = ffi.buffer('123abc')
         local endp = ffi.buffer(ffi.sizeof('ptr'))
         local n = strtol(text, endp, 10)
         local e = ffi.buffer(4)
         local m = frexp(8, e)
         return n, ffi.string(ffi.get(endp, 'ptr')), m, ffi.get(e, 'int')"
    ));
    assert_eq!(out, "123\tabc\t0.5\t4");
}

#[test]
fn c_writes_into_a_buffer() {
    let out = run(&format!(
        "{LIBC}
         local memset = libc:bind('memset', 'ptr', 'ptr', 'int', 'size')
         local b = ffi.buffer(6)
         memset(b, 0x41, 3)
         return #b, ffi.string(b), ffi.string(b, 4) == 'AAA\\0'"
    ));
    assert_eq!(out, "6\tAAA\ttrue");
}

#[test]
fn buffer_memory() {
    let out = run("local b = ffi.buffer(16)
         ffi.set(b, 'i32', 0, -2)
         ffi.set(b, 'double', 8, 1.5)
         ffi.set(b, 'u16', 4, 0xbeef)
         local p = ffi.offset(b, 8)
         return ffi.get(b, 'i32'), ffi.get(b, 'u32'), ffi.get(b, 'u16', 4),
                ffi.get(b, 'double', 8), ffi.get(p, 'double'), ffi.string(ffi.buffer('hi'))");
    assert_eq!(out, "-2\t4294967294\t48879\t1.5\t1.5\thi");
}

#[test]
fn buffers_are_bounds_checked() {
    assert_eq!(
        error("ffi.get(ffi.buffer(8), 'i64', 4)"),
        "t:1: bad argument #3 to 'get' (out of the buffer's bounds)"
    );
    assert_eq!(
        error("ffi.string(ffi.buffer(4), 5)"),
        "t:1: bad argument #3 to 'string' (out of the buffer's bounds)"
    );
    assert_eq!(
        error("ffi.offset(ffi.buffer(4), -1)"),
        "t:1: bad argument #2 to 'offset' (out of the buffer's bounds)"
    );
    assert_eq!(
        error("ffi.set(ffi.buffer(4), 'u8', 0, 256)"),
        "t:1: bad argument #4 to 'set' (value out of range)"
    );
}

#[test]
fn null_pointers_are_nil() {
    let out = run(&format!(
        "{LIBC}
         local fopen = libc:bind('fopen', 'ptr', 'cstr', 'cstr')
         local strerror = libc:bind('strerror', 'cstr', 'int')
         local f = fopen('/nonexistent/orbit-ffi-test', 'r')
         local e = ffi.errno()
         return f, e > 0, strerror(e) ~= nil"
    ));
    assert_eq!(out, "nil\ttrue\ttrue");
}

#[test]
fn binding_twice_gives_the_same_function() {
    let out = run(&format!(
        "{LIBC}
         return libc:bind('abs', 'int', 'int') == libc:bind('abs', 'int', 'int'),
                libc:bind('abs', 'int', 'int') == libc:bind('abs', 'long', 'int'),
                type(libc:bind('abs', 'int', 'int'))"
    ));
    assert_eq!(out, "true\tfalse\tfunction");
}

#[test]
fn argument_errors() {
    let bind = format!("{LIBC} local abs = libc:bind('abs', 'int', 'int') ");
    assert_eq!(
        error(&format!("{bind} abs('x')")),
        "t:1: bad argument #1 to 'abs' (number expected, got string)"
    );
    assert_eq!(
        error(&format!("{bind} abs(1 << 40)")),
        "t:1: bad argument #1 to 'abs' (value out of range)"
    );
    assert_eq!(
        error(&format!("{bind} abs()")),
        "t:1: bad argument #1 to 'abs' (number expected, got no value)"
    );
    assert_eq!(
        error(&format!(
            "{LIBC} local strlen = libc:bind('strlen', 'size', 'cstr') strlen('a\\0b')"
        )),
        "t:1: bad argument #1 to 'strlen' (string contains a NUL byte)"
    );
    assert_eq!(
        error(&format!(
            "{LIBC} local free = libc:bind('free', 'void', 'ptr') free(ffi.open())"
        )),
        "t:1: bad argument #1 to 'free' (pointer expected, got ffi.library)"
    );
}

#[test]
fn binding_errors() {
    assert_eq!(
        error(&format!("{LIBC} libc:bind('abs', 'integer', 'int')")),
        "t:1: bad argument #2 to 'bind' (unknown C type 'integer')"
    );
    assert_eq!(
        error(&format!("{LIBC} libc:bind('abs', 'int', 'void')")),
        "t:1: bad argument #3 to 'bind' ('void' is a result type only)"
    );
    assert_eq!(
        error(&format!("{LIBC} libc:bind('abs', 'bytes', 'int')")),
        "t:1: bad argument #2 to 'bind' ('bytes' is an argument type only)"
    );
    assert!(
        error(&format!(
            "{LIBC} libc:bind('abs', 'int', 'int', 'int', 'int', 'int', 'int', 'int', 'int')"
        ))
        .contains("too many arguments")
    );
    assert!(
        error(&format!(
            "{LIBC} libc:bind('orbit_ffi_no_such_function', 'int')"
        ))
        .starts_with("t:1: cannot find C function 'orbit_ffi_no_such_function': ")
    );
    assert!(
        error("ffi.open('liborbit-ffi-no-such-library.so')")
            .starts_with("t:1: cannot open library 'liborbit-ffi-no-such-library.so': ")
    );
    assert_eq!(
        error("ffi.bind({}, 'abs', 'int')"),
        "t:1: bad argument #1 to 'bind' (ffi.library expected, got table)"
    );
}

#[cfg(feature = "orbitscript")]
#[test]
fn from_orbitscript() {
    let dir = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("ffi-orbitscript");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("strlen.orb");
    std::fs::write(
        &path,
        "let libc = ffi::open();\n\
         let strlen = libc.bind(\"strlen\", \"size\", \"cstr\");\n\
         strlen(\"orbit\")\n",
    )
    .unwrap();
    let src = format!("return dofile({:?})", path.to_str().unwrap());
    assert_eq!(run(&src), "5");
}
