//! The Lua 5.5 standard library.
//!
//! Every function is a [`NativeFn`] registered with the VM, one table per
//! library, so hosts can open only the libraries they want
//! ([design](https://github.com/orbitscript-lang/OrbitScript/blob/main/docs/design/stdlib-string-table-math.md)).
//! Only the `debug` library is behind a Cargo feature (`debug`); `io` and
//! `os` live in `orbit-std`.

mod base;
mod builtins;
pub mod checks;
mod chunk;
mod coroutine;
#[cfg(feature = "debug")]
mod debug;
mod dump;
mod intrinsics;
mod load;
pub(crate) mod string;
mod table;

use crate::stack::LibraryRow;
use crate::vm::Vm;

pub use chunk::{BINARY_SIGNATURE, chunk_id, is_orbitscript_file};

pub use load::load_chunk_in;

/// Call `__tostring` handler `h` on `v` now, nested, for `tostring` and
/// `string.format`'s `%s`: its result, which must be a string or a number.
pub(crate) fn tostring_now(
    ctx: &mut crate::vm::NativeCtx<'_>,
    h: crate::object::Value,
    v: crate::object::Value,
) -> Result<crate::object::Value, crate::vm::LuaError> {
    use crate::object::Value;
    let at = ctx.len();
    ctx.push(h);
    ctx.push(v);
    ctx.call_want(1, 1)?;
    let r = ctx.arg(at);
    ctx.truncate(at);
    match r {
        Value::Str(_) | Value::Int(_) | Value::Float(_) => Ok(r),
        _ => {
            let msg = ctx.new_str(b"'__tostring' must return a string");
            Err(ctx.error_value(msg))
        }
    }
}

/// The first result of a call a native requested with `CallThen` (nil if
/// none); the call's results are dropped from the window.
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

/// Open every library this build has: base functions as globals, plus the
/// library tables.
pub fn open_libs(vm: &mut Vm) {
    for lib in Lib::OPEN_ORDER {
        open_one(vm, lib);
    }
}

/// One standard library, as Lua's C API names them (`luaopen_base`,
/// `luaopen_package`, ...).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lib {
    Base,
    Coroutine,
    Debug,
    Io,
    Os,
    String,
    Table,
}

impl Lib {
    /// The order [`open_libs`] opens them in.
    pub const OPEN_ORDER: [Lib; 7] = [
        Lib::Base,
        Lib::String,
        Lib::Table,
        Lib::Os,
        Lib::Io,
        Lib::Coroutine,
        Lib::Debug,
    ];

    /// The library's global name (`_G` for the base library).
    pub(crate) fn name(self) -> &'static str {
        match self {
            Lib::Base => "_G",
            Lib::Coroutine => "coroutine",
            Lib::Debug => "debug",
            Lib::Io => "io",
            Lib::Os => "os",
            Lib::String => "string",
            Lib::Table => "table",
        }
    }
}

/// Open one library: set its global and its `package.loaded` entry, and
/// return its table. `None` when this build leaves the library out.
pub fn open_one(vm: &mut Vm, lib: Lib) -> Option<crate::object::Value> {
    use crate::object::Value;
    let opened = match lib {
        Lib::Base => {
            base::open(vm);
            for &(name, f) in load::FUNCTIONS {
                let v = vm.register_native(name, f);
                vm.set_global(name, v);
            }
            let g = vm.rt.globals;
            mark_loaded(vm, "_G", Value::Table(g));
            return Some(Value::Table(g));
        }
        Lib::String => {
            let st = open_lib(vm, "string", string::FUNCTIONS);
            string::set_metatable_index(vm, st);
            add_functions(vm, st, dump::FUNCTIONS);
            true
        }
        Lib::Table => {
            let t = open_lib(vm, "table", table::FUNCTIONS);
            if orbit_lua_version::LUA.is_5_5() {
                add_functions(vm, t, table::FUNCTIONS_5_5);
            }
            // `insert`, `remove` and `sort` are written in OrbitScript.
            for (name, f) in builtins::load(vm, &builtins::TABLE) {
                let k = vm.new_str(&name);
                vm.table_set(t, k, f);
            }
            true
        }
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
    opened.then(|| vm.global(lib.name()))
}

/// Flush standard output (`print`'s and `io.stdout`'s), as closing the
/// state does at exit.
pub fn flush_files(vm: &mut Vm) {
    let _ = vm.flush();
}

/// Registry key of the table `require` keeps loaded modules in
/// (`package.loaded`).
pub(crate) const LOADED: &[u8] = b"_LOADED";

/// The registry table under `key`, created on first use.
pub(crate) fn registry_table(vm: &mut Vm, key: &[u8]) -> crate::object::Gc<crate::object::Table> {
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
pub(crate) fn mark_loaded(vm: &mut Vm, name: &str, lib: crate::object::Value) {
    let loaded = registry_table(vm, LOADED);
    let k = vm.new_str(name.as_bytes());
    vm.table_set(loaded, k, lib);
}

/// Register `functions` (qualified names such as `"string.rep"`) in a new
/// global table `name`.
pub(crate) fn open_lib(
    vm: &mut Vm,
    name: &str,
    functions: &[LibraryRow],
) -> crate::object::Gc<crate::object::Table> {
    let t = vm.new_table();
    vm.pin(crate::object::Value::Table(t));
    add_functions(vm, t, functions);
    vm.set_global(name, crate::object::Value::Table(t));
    mark_loaded(vm, name, crate::object::Value::Table(t));
    vm.unpin(crate::object::Value::Table(t));
    t
}

/// Register the native in a library table's `row`, with its fast path.
pub(crate) fn register_row(vm: &mut Vm, row: &LibraryRow) -> crate::object::Value {
    let v = vm.register_native(row.name, row.entry);
    if let Some(fast) = row.fast {
        vm.set_native_fast_path(row.name, fast);
    }
    v
}

/// Make a native's arguments exactly `n`, dropping or nil-filling the
/// rest (`set_top` only when needed: it looks for variables to close).
fn keep_args(s: &mut crate::stack::Stack<'_>, n: i32) -> crate::stack::StackResult<()> {
    if s.top() != n {
        s.set_top(n)?;
    }
    Ok(())
}

/// Register more `functions` in an open library table.
fn add_functions(
    vm: &mut Vm,
    t: crate::object::Gc<crate::object::Table>,
    functions: &[LibraryRow],
) {
    for row in functions {
        let qualified = row.name;
        let short = qualified.rsplit('.').next().unwrap_or(qualified);
        let fv = register_row(vm, row);
        let k = vm.new_str(short.as_bytes());
        vm.table_set(t, k, fv);
    }
}
