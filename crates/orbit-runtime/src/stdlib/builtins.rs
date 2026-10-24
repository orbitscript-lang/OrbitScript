//! Built-in library functions written in OrbitScript, and the intrinsics
//! they use (ADR-11).
//!
//! The modules in `builtins/` are compiled by the build script and embedded
//! as binary chunks. Each loads with its own environment holding only the
//! intrinsics (taken with `use intrinsics::{...}`), `next` and `type`, so
//! scripts can neither reach the intrinsics nor change what a built-in
//! calls, and returns its exports. Their prototypes are
//! marked built-in, so the debug interface, error messages and tracebacks
//! show them as C functions.
//!
//! An intrinsic raises its errors as the built-in calling it would:
//! `bad argument #2 to 'insert'`, positioned at the built-in's caller.

use std::sync::OnceLock;

use crate::object::number::{self, float_to_int};
use crate::object::{LuaHeap, Table, Value};
use crate::vm::op::{UnlinkedChunk, format};
use crate::vm::{FastFn, LuaError, NativeCtx, NativeFn, NativeReturn, Vm};

type R = Result<NativeReturn, LuaError>;

/// A built-in module: its binary chunk, and its decoded form, shared by
/// every isolate in the process.
pub(crate) struct Module {
    bytes: &'static [u8],
    decoded: OnceLock<UnlinkedChunk>,
}

impl Module {
    const fn new(bytes: &'static [u8]) -> Module {
        Module {
            bytes,
            decoded: OnceLock::new(),
        }
    }

    fn chunk(&self) -> &UnlinkedChunk {
        self.decoded.get_or_init(|| {
            format::decode(self.bytes).expect("a built-in module the build script compiled")
        })
    }
}

pub(crate) static BASE: Module =
    Module::new(include_bytes!(concat!(env!("OUT_DIR"), "/base.orbc")));
#[cfg(feature = "table")]
pub(crate) static TABLE: Module =
    Module::new(include_bytes!(concat!(env!("OUT_DIR"), "/table.orbc")));

/// An intrinsic's name in the natives table: never shown, since a
/// built-in's errors name the built-in.
macro_rules! native_name {
    ($name:ident) => {
        concat!("(intrinsic ", stringify!($name), ")")
    };
}

/// An intrinsic: its name in the intrinsics table, its name in the natives
/// table, and the native.
macro_rules! intrinsic {
    ($name:ident) => {
        (stringify!($name), native_name!($name), $name as NativeFn)
    };
}

/// The intrinsics a built-in module takes with `use intrinsics::{...}`,
/// copying each once when the module loads. Its other globals are `next`
/// and `type`, the base library's own functions.
const INTRINSICS: &[(&str, &str, NativeFn)] = &[
    intrinsic!(argerror),
    intrinsic!(typeerror),
    intrinsic!(raise),
    intrinsic!(checktab),
    intrinsic!(checkint),
    intrinsic!(lenint),
    intrinsic!(ult),
    intrinsic!(newarray),
    intrinsic!(metafield),
    intrinsic!(plainlen),
    intrinsic!(sortprim),
];

/// Fast paths for the intrinsics built-ins call on every call (see
/// [`FastFn`]): a call through one skips the native's context, which costs
/// more than the intrinsic's own work.
const FAST_INTRINSICS: &[(&str, FastFn)] = &[
    (native_name!(checkint), checkint_fast),
    (native_name!(lenint), lenint_fast),
    (native_name!(ult), ult_fast),
    (native_name!(plainlen), plainlen_fast),
];

/// Where a module's environment keeps the intrinsics table for
/// [`require`]. Not a name a module can use: OrbitScript has no implicit
/// globals.
const INTRINSICS_KEY: &str = "(intrinsics)";

/// Built-ins that code they call may yield across, as their C counterparts
/// in the reference allow (`pairs` calls `__pairs` yieldably). Every other
/// built-in refuses a yield, as a C function without a continuation does.
const YIELDING: &[&str] = &["pairs"];

/// Load `module` and return its exports, `(name, function)` pairs.
pub(crate) fn load(vm: &mut Vm, module: &Module) -> Vec<(Vec<u8>, Value)> {
    let env = vm.new_table_value();
    let Value::Table(env_t) = env else {
        unreachable!("a new table")
    };
    let intrinsics = vm.new_table_value();
    let Value::Table(intrinsics_t) = intrinsics else {
        unreachable!("a new table")
    };
    for &(name, native, f) in INTRINSICS {
        let v = vm.native_named(native, f);
        vm.raw_set_str(intrinsics_t, name, v);
    }
    for &(native, fast) in FAST_INTRINSICS {
        vm.set_native_fast_path(native, fast);
    }
    vm.raw_set_str(env_t, INTRINSICS_KEY, intrinsics);
    let require = vm.native_named(native_name!(require), require);
    vm.raw_set_str(env_t, "require", require);
    let shared: [(&'static str, NativeFn); 2] =
        [("next", super::base::next), ("type", super::base::type_)];
    for (name, f) in shared {
        let v = match vm.rt.native_ids.get(name) {
            Some(&id) => Value::Native(id),
            None => vm.register_native(name, f),
        };
        vm.raw_set_str(env_t, name, v);
    }
    let main = vm.load_builtin(module.chunk(), env);
    let exports = match vm.call(main, &[]) {
        Ok(r) => r.into_iter().next(),
        Err(_) => None,
    };
    let Some(Value::Table(t)) = exports else {
        panic!("a built-in module returns its exports");
    };
    let exports: Vec<(Vec<u8>, Value)> = vm
        .table_entries(t)
        .into_iter()
        .filter_map(|(k, v)| match k {
            Value::Str(s) => Some((vm.heap.get(s).as_bytes().to_vec(), v)),
            _ => None,
        })
        .collect();
    for (name, v) in &exports {
        if let Value::Closure(c) = *v {
            if YIELDING.iter().any(|y| y.as_bytes() == name.as_slice()) {
                vm.allow_yields(c);
            }
        }
    }
    exports
}

impl Vm {
    fn new_table_value(&mut self) -> Value {
        Value::Table(self.heap.alloc(Table::new()))
    }

    /// The native registered as `name`, registering `f` under it the first time.
    fn native_named(&mut self, name: &'static str, f: NativeFn) -> Value {
        match self.rt.native_ids.get(name) {
            Some(&id) => Value::Native(id),
            None => self.register_native(name, f),
        }
    }

    fn raw_set_str(&mut self, t: crate::object::Gc<Table>, name: &str, v: Value) {
        let k = self.new_str(name.as_bytes());
        self.table_set(t, k, v);
    }

    /// Let code `c` calls yield across it. `c` must be its prototype's only
    /// closure, as an exported built-in is when its module has just run.
    fn allow_yields(&mut self, c: crate::object::Gc<crate::object::Closure>) {
        let proto = self.heap.get(c).proto;
        let code = std::rc::Rc::new(self.heap.get(proto).code.allowing_yields());
        self.heap.get_mut(proto).code = code.clone();
        self.heap.get_mut(c).code = code;
    }

    fn table_entries(&self, t: crate::object::Gc<Table>) -> Vec<(Value, Value)> {
        let tb = self.heap.get(t);
        let mut out = Vec::new();
        let mut k = Value::Nil;
        while let Ok(Some((nk, v))) = tb.next(&self.heap, k) {
            out.push((nk, v));
            k = nk;
        }
        out
    }
}

/// The 1-based argument number intrinsics take, from slot `i`.
fn arg_number(ctx: &NativeCtx<'_>, i: usize) -> usize {
    match ctx.arg(i) {
        Value::Int(n) if n > 0 => n as usize,
        _ => 1,
    }
}

/// `argerror(n, msg)`: `bad argument #n to 'f' (msg)`.
fn argerror(ctx: &mut NativeCtx<'_>) -> R {
    let n = arg_number(ctx, 0);
    let mut msg = Vec::new();
    ctx.display(ctx.arg(1), &mut msg);
    Err(ctx.builtin_arg_error(n, &msg))
}

/// `typeerror(n, expected, v)`: `bad argument #n to 'f' (expected
/// expected, got type)`.
fn typeerror(ctx: &mut NativeCtx<'_>) -> R {
    let n = arg_number(ctx, 0);
    let mut expected = Vec::new();
    ctx.display(ctx.arg(1), &mut expected);
    let v = ctx.arg(2);
    Err(ctx.builtin_type_error(n, &String::from_utf8_lossy(&expected), Some(v)))
}

/// `raise(msg)`: an error positioned at the built-in's caller.
fn raise(ctx: &mut NativeCtx<'_>) -> R {
    let mut msg = Vec::new();
    ctx.display(ctx.arg(0), &mut msg);
    Err(ctx.builtin_error(&msg))
}

/// `checktab(t)`: argument 1 is a table, or has `__index` and `__newindex`
/// to stand in for one.
fn checktab(ctx: &mut NativeCtx<'_>) -> R {
    use crate::vm::MetaField;
    let v = ctx.arg(0);
    let ok = matches!(v, Value::Table(_))
        || (ctx.metatable(v).is_some()
            && !ctx.metamethod(v, MetaField::Index).is_nil()
            && !ctx.metamethod(v, MetaField::NewIndex).is_nil());
    if ok {
        Ok(NativeReturn::Return(0))
    } else {
        Err(ctx.builtin_type_error(1, "table", Some(v)))
    }
}

/// `checkint(v, n)`: argument `n` as an integer, as `luaL_checkinteger`
/// takes it (integral floats and numeric strings convert).
fn checkint(ctx: &mut NativeCtx<'_>) -> R {
    let n = arg_number(ctx, 1);
    let v = ctx.arg(0);
    let num = match v {
        Value::Int(_) | Value::Float(_) => Some(v),
        Value::Str(s) => number::str_to_number(ctx.bytes(s)),
        _ => None,
    };
    match num {
        Some(Value::Int(i)) => ctx.ret([Value::Int(i)]),
        Some(Value::Float(f)) => match float_to_int(f) {
            Some(i) => ctx.ret([Value::Int(i)]),
            None => Err(ctx.builtin_arg_error(n, "number has no integer representation")),
        },
        _ => Err(ctx.builtin_type_error(n, "number", Some(v))),
    }
}

/// `checkint`'s integer case.
fn checkint_fast(_: &LuaHeap, args: &[Value]) -> Option<Value> {
    match args.first() {
        Some(&v @ Value::Int(_)) => Some(v),
        _ => None,
    }
}

/// `lenint(len)`: a length (`#t`, which may come from `__len`) as an
/// integer.
fn lenint(ctx: &mut NativeCtx<'_>) -> R {
    match ctx.to_integer(ctx.arg(0)) {
        Some(n) => ctx.ret([Value::Int(n)]),
        None => Err(ctx.builtin_error("object length is not an integer")),
    }
}

/// `lenint`'s integer case.
fn lenint_fast(_: &LuaHeap, args: &[Value]) -> Option<Value> {
    match args.first() {
        Some(&v @ Value::Int(_)) => Some(v),
        _ => None,
    }
}

/// `ult(a, b)`: unsigned `a < b` for integers.
fn ult(ctx: &mut NativeCtx<'_>) -> R {
    let (Value::Int(a), Value::Int(b)) = (ctx.arg(0), ctx.arg(1)) else {
        return ctx.ret([Value::Bool(false)]);
    };
    ctx.ret([Value::Bool((a as u64) < (b as u64))])
}

fn ult_fast(_: &LuaHeap, args: &[Value]) -> Option<Value> {
    Some(Value::Bool(match args {
        [Value::Int(a), Value::Int(b), ..] => (*a as u64) < (*b as u64),
        _ => false,
    }))
}

/// `newarray(n)`: an empty table with room for `n` array elements.
fn newarray(ctx: &mut NativeCtx<'_>) -> R {
    /// Most slots reserved up front; bigger arrays grow as they fill.
    const MAX_RESERVE: usize = 1 << 24;
    let n = match ctx.arg(0) {
        Value::Int(n) if n > 0 => (n as usize).min(MAX_RESERVE),
        _ => 0,
    };
    ctx.reserve(n.saturating_mul(9))?;
    let Ok(t) = Table::try_with_capacity(n, 0) else {
        return Err(ctx.no_memory());
    };
    let t = ctx.new_table(t);
    ctx.ret([Value::Table(t)])
}

/// `metafield(v, name)`: field `name` of `v`'s metatable, raw, or nil.
fn metafield(ctx: &mut NativeCtx<'_>) -> R {
    let v = ctx.arg(0);
    let r = match (ctx.metatable(v), ctx.arg(1)) {
        (Some(mt), k @ Value::Str(_)) => ctx.vm.heap.get(mt).get(&ctx.vm.heap, k),
        _ => Value::Nil,
    };
    ctx.ret([r])
}

/// `plainlen(t)`: `#t` for a table without a metatable, so nothing can
/// change what indexing and `#` do; otherwise nil, and the built-in takes
/// its general path.
fn plainlen(ctx: &mut NativeCtx<'_>) -> R {
    let r = match ctx.arg(0) {
        Value::Table(t) if ctx.vm.heap.get(t).meta.is_none() => {
            Value::Int(ctx.vm.heap.get(t).border())
        }
        _ => Value::Nil,
    };
    ctx.ret([r])
}

fn plainlen_fast(heap: &LuaHeap, args: &[Value]) -> Option<Value> {
    Some(match args.first() {
        Some(&Value::Table(t)) if heap.get(t).meta.is_none() => Value::Int(heap.get(t).border()),
        _ => Value::Nil,
    })
}

/// `sortprim(a, n)`: sort `a[1..=n]` in place by `<` if they are all
/// numbers (none NaN) or all strings, where `<` is a total order and runs
/// no metamethod, and return true; otherwise leave `a` alone and return
/// false. The sort is stable, as the built-in's own merge sort is, so
/// equal elements (`1` and `1.0`) keep their order either way.
fn sortprim(ctx: &mut NativeCtx<'_>) -> R {
    let (Value::Table(a), Value::Int(n)) = (ctx.arg(0), ctx.arg(1)) else {
        return ctx.ret([Value::Bool(false)]);
    };
    let heap = &ctx.vm.heap;
    let tab = heap.get(a);
    let values: Vec<Value> = (1..=n).map(|i| tab.get_int(i)).collect();
    let numbers = values.iter().all(|v| match *v {
        Value::Int(_) => true,
        Value::Float(f) => !f.is_nan(),
        _ => false,
    });
    let strings = !numbers && values.iter().all(|v| matches!(v, Value::Str(_)));
    if !numbers && !strings {
        return ctx.ret([Value::Bool(false)]);
    }
    let mut values = values;
    values.sort_by(|&x, &y| {
        let lt = |x, y| crate::vm::ops::less_than(heap, &ctx.vm.rt.metas, x, y).unwrap_or(false);
        if lt(x, y) {
            std::cmp::Ordering::Less
        } else if lt(y, x) {
            std::cmp::Ordering::Greater
        } else {
            std::cmp::Ordering::Equal
        }
    });
    for (i, v) in values.into_iter().enumerate() {
        crate::object::table::set_int(&mut ctx.vm.heap, a, i as i64 + 1, v)
            .map_err(|_| ctx.no_memory())?;
    }
    ctx.ret([Value::Bool(true)])
}

/// `require(name)` inside a built-in module, which `use` calls: only
/// `intrinsics` exists.
fn require(ctx: &mut NativeCtx<'_>) -> R {
    let name = ctx.arg(0);
    let is_intrinsics = matches!(name, Value::Str(s) if ctx.bytes(s) == b"intrinsics");
    let found = match ctx.builtin_env() {
        Value::Table(env) if is_intrinsics => {
            let k = ctx.new_str(INTRINSICS_KEY.as_bytes());
            ctx.vm.heap.get(env).get(&ctx.vm.heap, k)
        }
        _ => Value::Nil,
    };
    if found.is_nil() {
        return Err(ctx.builtin_error("built-in modules can only use 'intrinsics'"));
    }
    ctx.ret([found])
}
