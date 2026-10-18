//! `require` and the `package` library
//! ([design](https://github.com/tymlipari/OrbitScript/blob/main/docs/design/chunks-io-os-cli.md#modules-require-and-package)).
//!
//! `package.loaded` and `package.preload` are also kept in the registry
//! (`_LOADED`, `_PRELOAD`), and the `package` table itself (`_PACKAGE`), so
//! `require` keeps working when scripts replace those fields, as in Lua.
//! The runtime reads no files, so the only searcher here is the preload
//! one: `orbit-std` adds `package.path`, `package.searchpath` and the Lua
//! searcher, and `orbit-capi` the C searchers and `package.loadlib`.

use crate::object::table as tbl;
use crate::object::{Gc, Table, Value};
use crate::vm::{CallOutcome, LuaError, NativeCtx, NativeReturn, Vm};

use crate::stdlib::{LOADED, registry_table};

type R = Result<NativeReturn, LuaError>;

const DIRSEP: &str = if cfg!(windows) { "\\" } else { "/" };

/// Registry keys.
const PRELOAD: &[u8] = b"_PRELOAD";
const PACKAGE: &[u8] = b"_PACKAGE";

fn reg_get(ctx: &mut NativeCtx<'_>, key: &[u8]) -> Value {
    let k = ctx.new_str(key);
    tbl::get(&ctx.vm.heap, ctx.vm.rt.registry, k)
}

/// Open `package` and the global `require`.
pub(crate) fn open(vm: &mut Vm) {
    let pkg = crate::stdlib::open_lib(vm, "package", &[]);
    vm.pin(Value::Table(pkg));
    let set = |vm: &mut Vm, k: &str, v: Value| {
        let k = vm.new_str(k.as_bytes());
        vm.table_set(pkg, k, v);
    };
    let config = format!("{DIRSEP}\n;\n?\n!\n-\n");
    let v = vm.new_str(config.as_bytes());
    set(vm, "config", v);
    let loaded = registry_table(vm, LOADED);
    set(vm, "loaded", Value::Table(loaded));
    let preload = registry_table(vm, PRELOAD);
    set(vm, "preload", Value::Table(preload));
    let searchers = vm.new_table();
    set(vm, "searchers", Value::Table(searchers));
    let fv = vm.register_native("package.searchers[1]", search_preload);
    vm.table_set(searchers, Value::Int(1), fv);
    let k = vm.new_str(PACKAGE);
    let reg = vm.rt.registry;
    vm.table_set(reg, k, Value::Table(pkg));
    let req = vm.register_native("require", require);
    vm.set_global("require", req);
    vm.unpin(Value::Table(pkg));
}

fn str_arg(ctx: &mut NativeCtx<'_>, i: usize) -> Result<Vec<u8>, LuaError> {
    let s = ctx.check_str(i)?;
    Ok(ctx.bytes(s).to_vec())
}

/// A field of the `package` table (the one `require` was opened with).
fn package_field(ctx: &mut NativeCtx<'_>, field: &[u8]) -> Value {
    match reg_get(ctx, PACKAGE) {
        Value::Table(p) => {
            let k = ctx.new_str(field);
            tbl::get(&ctx.vm.heap, p, k)
        }
        _ => Value::Nil,
    }
}

/// Return one message string from a searcher.
fn ret_msg(ctx: &mut NativeCtx<'_>, msg: Vec<u8>) -> R {
    let m = ctx.new_str_owned(msg);
    ctx.ret1(m)
}

fn search_preload(ctx: &mut NativeCtx<'_>) -> R {
    let name = str_arg(ctx, 0)?;
    let f = match reg_get(ctx, PRELOAD) {
        Value::Table(t) => {
            let k = ctx.new_str(&name);
            tbl::get(&ctx.vm.heap, t, k)
        }
        _ => return Err(ctx.error("'package.preload' must be a table")),
    };
    if f.is_nil() {
        let mut msg = b"no field package.preload['".to_vec();
        msg.extend_from_slice(&name);
        msg.extend_from_slice(b"']");
        return ret_msg(ctx, msg);
    }
    let data = ctx.new_str(b":preload:");
    ctx.push(f);
    ctx.push(data);
    Ok(NativeReturn::Return(2))
}

// `require`'s window while it runs searchers and the loader: the name, the
// searchers table, the next searcher's index and the messages so far; then
// the loader's extra value once a searcher succeeds.
const NAME: usize = 0;
const SEARCHERS_AT: usize = 1;
const NEXT: usize = 2;
const MSGS: usize = 3;
const EXTRA: usize = 4;

fn loaded_table(ctx: &mut NativeCtx<'_>) -> Result<Gc<Table>, LuaError> {
    match reg_get(ctx, LOADED) {
        Value::Table(t) => Ok(t),
        _ => Err(ctx.error("'package.loaded' must be a table")),
    }
}

fn require(ctx: &mut NativeCtx<'_>) -> R {
    let s = ctx.check_str(0)?;
    let loaded = loaded_table(ctx)?;
    let v = tbl::get(&ctx.vm.heap, loaded, Value::Str(s));
    if !v.is_falsy() {
        return ctx.ret1(v);
    }
    let Value::Table(searchers) = package_field(ctx, b"searchers") else {
        return Err(ctx.error("'package.searchers' must be a table"));
    };
    ctx.truncate(1);
    ctx.push(Value::Table(searchers));
    ctx.push(Value::Int(1));
    let empty = ctx.new_str(b"");
    ctx.push(empty);
    next_searcher(ctx)
}

/// Call the next searcher.
fn next_searcher(ctx: &mut NativeCtx<'_>) -> R {
    let (Value::Table(searchers), Value::Int(i)) = (ctx.arg(SEARCHERS_AT), ctx.arg(NEXT)) else {
        unreachable!("require's window")
    };
    let f = tbl::get(&ctx.vm.heap, searchers, Value::Int(i));
    if f.is_nil() {
        let Value::Str(name) = ctx.arg(NAME) else {
            unreachable!("require's window")
        };
        let Value::Str(msgs) = ctx.arg(MSGS) else {
            unreachable!("require's window")
        };
        let mut msg = b"module '".to_vec();
        msg.extend_from_slice(ctx.bytes(name));
        msg.extend_from_slice(b"' not found:");
        msg.extend_from_slice(ctx.bytes(msgs));
        let text = String::from_utf8_lossy(&msg).into_owned();
        let where_ = ctx.where_(1);
        let m = ctx.new_str(format!("{where_}{text}").as_bytes());
        return Err(ctx.error_value(m));
    }
    ctx.set(NEXT, Value::Int(i + 1));
    ctx.truncate(MSGS + 1);
    ctx.push(ctx.arg(NAME));
    Ok(NativeReturn::CallThen {
        f,
        nargs: 1,
        k: searcher_done,
        protected: false,
    })
}

fn searcher_done(ctx: &mut NativeCtx<'_>, outcome: CallOutcome) -> R {
    let CallOutcome::Ok(n) = outcome else {
        unreachable!("unprotected call")
    };
    let first = ctx.len() - n;
    let r0 = if n > 0 { ctx.arg(first) } else { Value::Nil };
    let r1 = if n > 1 {
        ctx.arg(first + 1)
    } else {
        Value::Nil
    };
    ctx.truncate(MSGS + 1);
    match r0 {
        Value::Closure(_) | Value::Native(_) => {
            // [.., extra, name, extra] -> loader(name, extra)
            ctx.push(r1);
            ctx.push(ctx.arg(NAME));
            ctx.push(r1);
            Ok(NativeReturn::CallThen {
                f: r0,
                nargs: 2,
                k: loader_done,
                protected: false,
            })
        }
        Value::Str(_) | Value::Int(_) | Value::Float(_) => {
            let Value::Str(msgs) = ctx.arg(MSGS) else {
                unreachable!("require's window")
            };
            let mut all = ctx.bytes(msgs).to_vec();
            all.extend_from_slice(b"\n\t");
            ctx.display(r0, &mut all);
            let m = ctx.new_str_owned(all);
            ctx.set(MSGS, m);
            next_searcher(ctx)
        }
        _ => next_searcher(ctx),
    }
}

fn loader_done(ctx: &mut NativeCtx<'_>, outcome: CallOutcome) -> R {
    let CallOutcome::Ok(n) = outcome else {
        unreachable!("unprotected call")
    };
    let first = ctx.len() - n;
    let result = if n > 0 { ctx.arg(first) } else { Value::Nil };
    let name = ctx.arg(NAME);
    let loaded = loaded_table(ctx)?;
    if !result.is_nil() {
        tbl::set(&mut ctx.vm.heap, loaded, name, result).expect("string keys are valid");
    }
    let mut v = tbl::get(&ctx.vm.heap, loaded, name);
    if v.is_nil() {
        v = Value::Bool(true);
        tbl::set(&mut ctx.vm.heap, loaded, name, v).expect("string keys are valid");
    }
    let extra = ctx.arg(EXTRA);
    ctx.truncate(0);
    ctx.push(v);
    ctx.push(extra);
    Ok(NativeReturn::Return(2))
}
