//! The Lua 5.5 standard library.
//!
//! Every function is a [`NativeFn`] registered with the VM, one table per
//! library, so hosts can open only the libraries they want. The `string`,
//! `utf8`, `table` and `math` libraries are each behind a Cargo feature of
//! the same name ([design](https://github.com/tymlipari/OrbitScript/blob/main/docs/design/stdlib-string-table-math.md)).

mod base;
#[cfg(feature = "dump")]
pub mod checks;
mod chunk;
#[cfg(feature = "coroutine")]
mod coroutine;
#[cfg(feature = "debug")]
mod debug;
#[cfg(feature = "dump")]
mod dump;
#[cfg(feature = "io")]
mod io;
#[cfg(feature = "load")]
mod load;
#[cfg(feature = "math")]
mod math;
#[cfg(feature = "os")]
mod os;
#[cfg(feature = "package")]
mod package;
#[cfg(feature = "string")]
mod string;
#[cfg(any(feature = "os", feature = "io"))]
mod sys;
#[cfg(feature = "table")]
mod table;
#[cfg(feature = "os")]
mod time;
#[cfg(feature = "os")]
mod tz;
#[cfg(feature = "utf8")]
mod utf8;

use crate::vm::{NativeFn, Vm};

pub use chunk::{BINARY_SIGNATURE, chunk_id, is_orbitscript_file};

#[cfg(feature = "load")]
pub use load::{load_buffer, load_chunk_in, read_chunk_file};
#[cfg(feature = "package")]
pub use package::{DynLoader, LoadFn, lock_cpath};

/// The first result of a call a native requested with `CallThen` (nil if
/// none); the call's results are dropped from the window.
#[cfg(any(feature = "math", feature = "string", feature = "table"))]
pub(crate) fn first_result(
    ctx: &mut crate::vm::NativeCtx<'_>,
    out: crate::vm::CallOutcome,
) -> Result<crate::object::Value, crate::vm::LuaError> {
    let n = match out {
        crate::vm::CallOutcome::Ok(n) => n,
        crate::vm::CallOutcome::Err(v) => return Err(ctx.error_value(v)),
    };
    let len = ctx.len();
    let v = if n > 0 {
        ctx.arg(len - n)
    } else {
        crate::object::Value::Nil
    };
    ctx.truncate(len - n);
    Ok(v)
}

/// How [`open_libs_with`] sets the libraries up.
#[derive(Debug, Clone, Copy, Default)]
pub struct Options {
    /// Ignore environment variables (`LUA_PATH`, `LUA_CPATH` and their
    /// versioned forms, `ORBIT_VERIFY`), as `lua -E` does.
    pub ignore_env: bool,
    /// Lock the C module search path to the one `package.cpath` starts
    /// with (from `LUA_CPATH` or the default): see [`lock_cpath`].
    pub lock_cpath: bool,
}

/// Open every library with default [`Options`].
pub fn open_libs(vm: &mut Vm) {
    open_libs_with(vm, Options::default());
}

/// Open every library this build has: base functions as globals, plus the
/// library tables.
pub fn open_libs_with(vm: &mut Vm, options: Options) {
    for lib in Lib::OPEN_ORDER {
        open_one(vm, lib, options);
    }
}

/// One standard library, as Lua's C API names them (`luaopen_base`,
/// `luaopen_package`, ...).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lib {
    Base,
    Package,
    Coroutine,
    Debug,
    Io,
    Math,
    Os,
    String,
    Table,
    Utf8,
}

impl Lib {
    /// In the order of `lualib.h`'s masks: [`Lib::mask`] is `1 << index`.
    pub const ALL: [Lib; 10] = [
        Lib::Base,
        Lib::Package,
        Lib::Coroutine,
        Lib::Debug,
        Lib::Io,
        Lib::Math,
        Lib::Os,
        Lib::String,
        Lib::Table,
        Lib::Utf8,
    ];

    /// The order [`open_libs_with`] opens them in.
    pub const OPEN_ORDER: [Lib; 10] = [
        Lib::Base,
        Lib::String,
        Lib::Utf8,
        Lib::Table,
        Lib::Math,
        Lib::Os,
        Lib::Io,
        Lib::Package,
        Lib::Coroutine,
        Lib::Debug,
    ];

    /// The library's global name (`_G` for the base library).
    pub fn name(self) -> &'static str {
        match self {
            Lib::Base => "_G",
            Lib::Package => "package",
            Lib::Coroutine => "coroutine",
            Lib::Debug => "debug",
            Lib::Io => "io",
            Lib::Math => "math",
            Lib::Os => "os",
            Lib::String => "string",
            Lib::Table => "table",
            Lib::Utf8 => "utf8",
        }
    }

    /// The library's bit in `luaL_openselectedlibs`'s masks.
    pub fn mask(self) -> u32 {
        let i = Lib::ALL.iter().position(|&l| l == self).unwrap_or(0);
        1 << i
    }
}

/// Open one library: set its global and its `package.loaded` entry, and
/// return its table. `None` when this build leaves the library out.
pub fn open_one(vm: &mut Vm, lib: Lib, options: Options) -> Option<crate::object::Value> {
    use crate::object::Value;
    let opened = match lib {
        Lib::Base => {
            base::open(vm);
            #[cfg(feature = "load")]
            for &(name, f) in load::FUNCTIONS {
                let v = vm.register_native(name, f);
                vm.set_global(name, v);
            }
            #[cfg(feature = "dump")]
            checks::set_level(
                vm,
                if options.ignore_env {
                    checks::strongest()
                } else {
                    checks::from_env()
                },
            );
            let g = vm.rt.globals;
            mark_loaded(vm, "_G", Value::Table(g));
            return Some(Value::Table(g));
        }
        #[cfg(feature = "string")]
        Lib::String => {
            let st = open_lib(vm, "string", string::FUNCTIONS);
            string::set_metatable_index(vm, st);
            #[cfg(feature = "dump")]
            add_functions(vm, st, dump::FUNCTIONS);
            true
        }
        #[cfg(feature = "utf8")]
        Lib::Utf8 => {
            let u = open_lib(vm, "utf8", utf8::FUNCTIONS);
            utf8::constants(vm, u);
            true
        }
        #[cfg(feature = "table")]
        Lib::Table => {
            open_lib(vm, "table", table::FUNCTIONS);
            true
        }
        #[cfg(feature = "math")]
        Lib::Math => {
            let m = open_lib(vm, "math", math::FUNCTIONS);
            math::constants(vm, m);
            for &(name, fast) in math::FAST {
                vm.set_native_fast_path(name, fast);
            }
            true
        }
        #[cfg(feature = "os")]
        Lib::Os => {
            open_lib(vm, "os", os::FUNCTIONS);
            true
        }
        #[cfg(feature = "io")]
        Lib::Io => {
            io::open(vm);
            true
        }
        #[cfg(feature = "package")]
        Lib::Package => {
            package::open(vm, options.ignore_env, options.lock_cpath);
            true
        }
        #[cfg(feature = "coroutine")]
        Lib::Coroutine => {
            open_lib(vm, "coroutine", coroutine::FUNCTIONS);
            true
        }
        #[cfg(feature = "debug")]
        Lib::Debug => {
            open_lib(vm, "debug", debug::FUNCTIONS);
            debug::open(vm);
            true
        }
        #[allow(unreachable_patterns)]
        _ => false,
    };
    let _ = options;
    opened.then(|| vm.global(lib.name()))
}

/// Flush every open file's buffer and `print`'s output, as closing the
/// state does at exit.
pub fn flush_files(vm: &mut Vm) {
    #[cfg(feature = "io")]
    io::flush_all(&mut *vm.rt.out);
    let _ = vm.flush();
}

/// Registry key of the table `require` keeps loaded modules in
/// (`package.loaded`).
pub const LOADED: &[u8] = b"_LOADED";

/// The registry table under `key`, created on first use.
pub fn registry_table(vm: &mut Vm, key: &[u8]) -> crate::object::Gc<crate::object::Table> {
    use crate::object::Value;
    let k = vm.new_str(key);
    match crate::object::table::get(&vm.heap, vm.rt.registry, k) {
        Value::Table(t) => t,
        _ => {
            let t = vm.new_table();
            let reg = vm.rt.registry;
            vm.table_set(reg, k, Value::Table(t));
            t
        }
    }
}

/// Record a library as loaded, so `require "name"` returns it.
pub fn mark_loaded(vm: &mut Vm, name: &str, lib: crate::object::Value) {
    let loaded = registry_table(vm, LOADED);
    let k = vm.new_str(name.as_bytes());
    vm.table_set(loaded, k, lib);
}

/// Register `functions` (qualified names such as `"string.rep"`) in a new
/// global table `name`.
pub fn open_lib(
    vm: &mut Vm,
    name: &str,
    functions: &[(&'static str, NativeFn)],
) -> crate::object::Gc<crate::object::Table> {
    let t = vm.new_table();
    vm.pin(crate::object::Value::Table(t));
    for &(qualified, f) in functions {
        let short = qualified.rsplit('.').next().unwrap_or(qualified);
        let fv = vm.register_native(qualified, f);
        let k = vm.new_str(short.as_bytes());
        vm.table_set(t, k, fv);
    }
    vm.set_global(name, crate::object::Value::Table(t));
    mark_loaded(vm, name, crate::object::Value::Table(t));
    vm.unpin(crate::object::Value::Table(t));
    t
}

/// Register more `functions` in an open library table.
#[allow(dead_code)]
fn add_functions(
    vm: &mut Vm,
    t: crate::object::Gc<crate::object::Table>,
    functions: &[(&'static str, NativeFn)],
) {
    for &(qualified, f) in functions {
        let short = qualified.rsplit('.').next().unwrap_or(qualified);
        let fv = vm.register_native(qualified, f);
        let k = vm.new_str(short.as_bytes());
        vm.table_set(t, k, fv);
    }
}

/// A file name from Lua bytes. On Unix any bytes are a valid name; elsewhere
/// names must be text, and invalid UTF-8 is replaced.
#[allow(dead_code)]
pub(crate) fn os_path(bytes: &[u8]) -> std::path::PathBuf {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        std::ffi::OsStr::from_bytes(bytes).into()
    }
    #[cfg(not(unix))]
    {
        String::from_utf8_lossy(bytes).into_owned().into()
    }
}

/// The bytes of an OS string (lossy outside Unix).
#[allow(dead_code)]
pub(crate) fn os_bytes(s: &std::ffi::OsStr) -> Vec<u8> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        s.as_bytes().to_vec()
    }
    #[cfg(not(unix))]
    {
        s.to_string_lossy().into_owned().into_bytes()
    }
}

/// Create a new, empty file with a unique name in the temporary directory
/// (`os.tmpname`, `io.tmpfile`). Creating it, rather than only picking a
/// name, means no other process can take the name first.
#[allow(dead_code)]
pub(crate) fn temp_file() -> std::io::Result<(std::path::PathBuf, std::fs::File)> {
    use std::hash::{BuildHasher, Hasher};
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    const CHARS: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    let dir = std::env::temp_dir();
    let mut last = None;
    for _ in 0..100 {
        // RandomState is seeded randomly per process; mix in a counter so
        // every attempt differs.
        let mut h = std::collections::hash_map::RandomState::new().build_hasher();
        h.write_u64(COUNTER.fetch_add(1, Ordering::Relaxed));
        h.write_u32(std::process::id());
        let mut bits = h.finish();
        let mut name = String::from("orbit_");
        for _ in 0..6 {
            name.push(CHARS[(bits % CHARS.len() as u64) as usize] as char);
            bits /= CHARS.len() as u64;
        }
        let path = dir.join(name);
        match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(f) => return Ok((path, f)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => last = Some(e),
            Err(e) => return Err(e),
        }
    }
    Err(last.unwrap_or_else(|| std::io::Error::other("no unique name")))
}

/// The C library's text for an I/O error (`No such file or directory`),
/// without the ` (os error 2)` that Rust appends.
#[allow(dead_code)]
pub(crate) fn os_error_text(e: &std::io::Error) -> String {
    let s = e.to_string();
    match s.rfind(" (os error ") {
        Some(i) => s[..i].to_string(),
        None => s,
    }
}
