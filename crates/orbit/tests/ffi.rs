//! The `ffi` library (ADR-9): binding C functions from the C library the
//! test process already has, converting arguments and results, buffers and
//! raw memory, and the errors scripts get.

#![cfg(all(feature = "ffi", unix))]

use orbit::{Chunk, Config, Error, Isolate, Runtime};

fn lua() -> Isolate {
    let mut iso = Runtime::new().new_isolate(Config::default());
    iso.enable_ffi();
    iso
}

/// Run `src` and show its results as `tostring` would, tab-separated.
fn run_in(iso: &mut Isolate, src: &str) -> String {
    let out = iso.scope(|s| {
        let out = s.load(Chunk::new(src).name("=t"))?.call(())?;
        out.iter()
            .map(|v| Ok(String::from_utf8_lossy(&v.to_display()?).into_owned()))
            .collect::<Result<Vec<_>, Error>>()
    });
    match out {
        Ok(v) => v.join("\t"),
        Err(e) => panic!("script failed: {e:?}"),
    }
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
    let mut lua = Runtime::new().new_isolate(Config::default());
    assert_eq!(
        run_in(&mut lua, "return ffi, package.loaded.ffi"),
        "nil\tnil"
    );
    lua.enable_ffi();
    lua.enable_ffi();
    assert_eq!(
        run_in(&mut lua, "return type(ffi), package.loaded.ffi == ffi"),
        "table\ttrue"
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

#[test]
fn from_orbitscript() {
    let mut iso = lua();
    let src = "let libc = ffi::open();\n\
               let strlen = libc.bind(\"strlen\", \"size\", \"cstr\");\n\
               return strlen(\"orbit\");\n";
    let n = iso.scope(|s| {
        let out = s.load(Chunk::new(src).name("@strlen.orb"))?.call(())?;
        out[0].get::<i64>()
    });
    assert_eq!(n.unwrap(), 5);
}

/// Signatures the C library has no function for: more arguments than there
/// are registers, and integers mixed with `float`s and `double`s. The
/// functions come from a small C file built with the system's `cc`; the
/// test is skipped without one.
#[test]
fn many_and_mixed_arguments() {
    let dir = std::env::temp_dir().join(format!("orbit-ffi-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let src = dir.join("many.c");
    let lib = dir.join("libmany.so");
    std::fs::write(
        &src,
        "long sum10(long a, long b, long c, long d, long e, long f, long g, long h, long i, long j) {
             return a + 2*b + 3*c + 4*d + 5*e + 6*f + 7*g + 8*h + 9*i + 10*j;
         }
         double mix(int a, double b, float c, long d, double e, float f, short g, double h,
                    double i, double j, double k, double l, double m, unsigned char n) {
             return a + b + c + d + e + f + g + h + i + j + k + l + m + n;
         }
         float half(float x) { return x / 2; }
         signed char neg(signed char x) { return -x; }",
    )
    .unwrap();
    let built = std::process::Command::new("cc")
        .args(["-shared", "-fPIC", "-o"])
        .arg(&lib)
        .arg(&src)
        .status();
    if !built.is_ok_and(|s| s.success()) {
        eprintln!("skipped: no C compiler");
        return;
    }
    let out = run(&format!(
        "local lib = ffi.open('{}')
         local sum10 = lib:bind('sum10', 'long', {})
         local mix = lib:bind('mix', 'double', 'int', 'double', 'float', 'long', 'double',
             'float', 'short', 'double', 'double', 'double', 'double', 'double', 'double', 'uchar')
         local half = lib:bind('half', 'float', 'float')
         local neg = lib:bind('neg', 'i8', 'i8')
         return sum10(1, 1, 1, 1, 1, 1, 1, 1, 1, 1), sum10(0, 0, 0, 0, 0, 0, 0, 0, 0, -1),
             mix(1, 0.5, 0.25, 2, 1, 0.75, -3, 1, 1, 1, 1, 1, 1, 255), half(3), neg(-128)",
        lib.display(),
        ["'long'"; 10].join(", ")
    ));
    std::fs::remove_dir_all(&dir).ok();
    assert_eq!(out, "55\t-10\t263.5\t1.5\t-128");
}
