//! The C API's operations in safe Rust, over a native's window.
//!
//! Indices follow the manual: positive from the bottom of the window (1 is
//! the first argument), negative from the top, plus the registry and
//! upvalue pseudo-indices. An index outside the window reads as "none";
//! writing to one raises an error instead of corrupting anything.

use std::cell::RefCell;

use orbit_vm::orbit_core::number::{float_to_int, fmt_number, str_to_number};
use orbit_vm::orbit_core::table as tbl;
use orbit_vm::orbit_core::value::raw_eq;
use orbit_vm::orbit_core::{Gc, LightUserData, LuaStr, Table, Value, gc};
use orbit_vm::{Event, LuaError, Meta, NativeCtx, Vm};

use crate::CBlock;

pub(crate) type Res<T> = Result<T, LuaError>;

/// Constants from the C ABI (`lua.h`).
pub(crate) mod abi {
    pub const REGISTRYINDEX: i32 = -(i32::MAX / 2 + 1000);
    pub const MULTRET: i32 = -1;
    pub const TNONE: i32 = -1;
    pub const TNIL: i32 = 0;
    pub const TBOOLEAN: i32 = 1;
    pub const TLIGHTUSERDATA: i32 = 2;
    pub const TNUMBER: i32 = 3;
    pub const TSTRING: i32 = 4;
    pub const TTABLE: i32 = 5;
    pub const TFUNCTION: i32 = 6;
    pub const TUSERDATA: i32 = 7;
    pub const TTHREAD: i32 = 8;
    pub const OK: i32 = 0;
    pub const ERRRUN: i32 = 2;
    pub const OPEQ: i32 = 0;
    pub const OPLT: i32 = 1;
    pub const OPLE: i32 = 2;
    pub const GCSTOP: i32 = 0;
    pub const GCRESTART: i32 = 1;
    pub const GCCOLLECT: i32 = 2;
    pub const GCCOUNT: i32 = 3;
    pub const GCCOUNTB: i32 = 4;
}

/// Per-VM state the API keeps between calls. Borrowed only for short
/// stretches, never across a call back into Lua, because that call can run
/// C functions that borrow it again.
#[derive(Debug, Default)]
pub(crate) struct State {
    /// The error a Rust entry point raised, for the trampoline's caller.
    pub pending: Option<Value>,
    /// NUL-terminated copies of strings handed to C. Each C call drops the
    /// copies made during it when it returns.
    pub strs: Vec<Box<[u8]>>,
    /// Runtime shells to run nested calls in (see [`nested`]), boxed so
    /// taking one out moves a pointer, not a whole runtime.
    #[allow(clippy::vec_box)]
    pub spares: Vec<Box<Vm>>,
}

pub(crate) type Shared = RefCell<State>;

/// Where an index points.
#[derive(Clone, Copy)]
enum Slot {
    Stack(usize),
    Registry,
    Upvalue(usize),
    None,
}

fn slot(c: &NativeCtx<'_>, idx: i32) -> Slot {
    let len = c.len() as i64;
    if idx > abi::REGISTRYINDEX && idx <= 0 {
        // Negative stack index (0 is invalid).
        let at = len + idx as i64;
        if idx < 0 && at >= 0 {
            Slot::Stack(at as usize)
        } else {
            Slot::None
        }
    } else if idx == abi::REGISTRYINDEX {
        Slot::Registry
    } else if idx < abi::REGISTRYINDEX {
        // lua_upvalueindex(i): C's upvalue i is the closure's upvalue i
        // (upvalue 0 holds the function pointer).
        Slot::Upvalue((abi::REGISTRYINDEX - idx) as usize)
    } else {
        let at = idx as i64 - 1;
        if at < len {
            Slot::Stack(at as usize)
        } else {
            Slot::None
        }
    }
}

fn get(c: &NativeCtx<'_>, idx: i32) -> Value {
    match slot(c, idx) {
        Slot::Stack(i) => c.arg(i),
        Slot::Registry => Value::Table(c.rt.registry),
        Slot::Upvalue(i) => c.upvalue(i),
        Slot::None => Value::Nil,
    }
}

fn is_none(c: &NativeCtx<'_>, idx: i32) -> bool {
    matches!(slot(c, idx), Slot::None)
}

fn put(c: &mut NativeCtx<'_>, idx: i32, v: Value) -> Res<()> {
    match slot(c, idx) {
        Slot::Stack(i) => c.set(i, v),
        Slot::Upvalue(i) => c.set_upvalue(i, v),
        Slot::Registry | Slot::None => return Err(c.error("invalid index in C API call")),
    }
    Ok(())
}

fn pop(c: &mut NativeCtx<'_>) -> Res<Value> {
    let len = c.len();
    if len == 0 {
        return Err(c.error("C API: not enough elements in the stack"));
    }
    let v = c.arg(len - 1);
    c.truncate(len - 1);
    Ok(v)
}

fn table_at(c: &mut NativeCtx<'_>, idx: i32) -> Res<Gc<Table>> {
    match get(c, idx) {
        Value::Table(t) => Ok(t),
        _ => Err(c.error("C API: table expected")),
    }
}

pub(crate) fn type_of(v: Value) -> i32 {
    match v {
        Value::Nil => abi::TNIL,
        Value::Bool(_) => abi::TBOOLEAN,
        Value::Int(_) | Value::Float(_) => abi::TNUMBER,
        Value::Str(_) => abi::TSTRING,
        Value::Table(_) => abi::TTABLE,
        Value::Closure(_) | Value::Native(_) => abi::TFUNCTION,
        Value::UserData(_) => abi::TUSERDATA,
        Value::Thread(_) => abi::TTHREAD,
        Value::LightUserData(_) => abi::TLIGHTUSERDATA,
    }
}

fn store_error(c: &mut NativeCtx<'_>, e: tbl::StoreError) -> LuaError {
    match e {
        tbl::StoreError::NoMemory => c.no_memory(),
        e => c.error(e.message()),
    }
}

// ---- nested calls ----------------------------------------------------------

/// Call `f` with `args` and return all its results, from inside a native.
///
/// The VM's entry point (`Vm::call`) needs a `Vm`, and a native only has
/// its parts. So the parts are swapped into a spare shell for the call and
/// back afterwards. A production version would give `orbit-vm` a re-entry
/// point on `NativeCtx` instead.
pub(crate) fn nested(
    c: &mut NativeCtx<'_>,
    s: &Shared,
    f: Value,
    args: &[Value],
) -> Res<Vec<Value>> {
    let spare = s.borrow_mut().spares.pop();
    let mut spare = spare.unwrap_or_else(|| Box::new(Vm::new()));
    let len = c.len();
    std::mem::swap(c.heap, &mut spare.heap);
    std::mem::swap(c.rt, &mut spare.rt);
    let r = spare.call(f, args);
    std::mem::swap(c.heap, &mut spare.heap);
    std::mem::swap(c.rt, &mut spare.rt);
    // The call leaves the top where it pushed the function; back to ours.
    c.truncate(len);
    s.borrow_mut().spares.push(spare);
    r
}

/// Finish an operation that may need a metamethod. For a call, its
/// arguments are the last `nargs` values of the window.
fn finish<T>(
    c: &mut NativeCtx<'_>,
    s: &Shared,
    m: Meta<T>,
    conv: impl FnOnce(&NativeCtx<'_>, Value) -> T,
) -> Res<T> {
    match m {
        Meta::Done(v) => Ok(v),
        Meta::Call { f, nargs } => {
            let len = c.len();
            let args = c.window()[len - nargs..].to_vec();
            c.truncate(len - nargs);
            let r = nested(c, s, f, &args)?;
            Ok(conv(c, r.first().copied().unwrap_or(Value::Nil)))
        }
    }
}

fn index(c: &mut NativeCtx<'_>, s: &Shared, obj: Value, key: Value) -> Res<Value> {
    let m = c.index(obj, key)?;
    finish(c, s, m, |_, v| v)
}

fn set_index(c: &mut NativeCtx<'_>, s: &Shared, obj: Value, key: Value, v: Value) -> Res<()> {
    let m = c.set_index(obj, key, v)?;
    finish(c, s, m, |_, _| ())
}

fn truthy(v: Value) -> bool {
    !matches!(v, Value::Nil | Value::Bool(false))
}

// ---- stack -----------------------------------------------------------------

pub(crate) fn absindex(c: &NativeCtx<'_>, idx: i32) -> i32 {
    if idx > 0 || idx <= abi::REGISTRYINDEX {
        idx
    } else {
        c.len() as i32 + idx + 1
    }
}

pub(crate) fn gettop(c: &NativeCtx<'_>) -> i32 {
    c.len() as i32
}

pub(crate) fn settop(c: &mut NativeCtx<'_>, idx: i32) -> Res<()> {
    let len = c.len() as i64;
    let new = if idx >= 0 {
        idx as i64
    } else {
        len + idx as i64 + 1
    };
    if new < 0 {
        return Err(c.error("C API: invalid new top"));
    }
    let new = new as usize;
    if new > c.len() {
        c.set(new - 1, Value::Nil);
    }
    c.truncate(new);
    Ok(())
}

pub(crate) fn pushvalue(c: &mut NativeCtx<'_>, idx: i32) {
    let v = get(c, idx);
    c.push(v);
}

pub(crate) fn rotate(c: &mut NativeCtx<'_>, idx: i32, n: i32) -> Res<()> {
    let Slot::Stack(start) = slot(c, idx) else {
        return Err(c.error("C API: invalid index to rotate"));
    };
    let mut vals = c.window()[start..].to_vec();
    let len = vals.len() as i64;
    if len > 0 {
        let k = (n as i64).rem_euclid(len) as usize;
        vals.rotate_right(k);
        for (i, v) in vals.into_iter().enumerate() {
            c.set(start + i, v);
        }
    }
    Ok(())
}

pub(crate) fn copy(c: &mut NativeCtx<'_>, from: i32, to: i32) -> Res<()> {
    let v = get(c, from);
    put(c, to, v)
}

// ---- reading values ----------------------------------------------------------

pub(crate) fn type_(c: &NativeCtx<'_>, idx: i32) -> i32 {
    if is_none(c, idx) {
        abi::TNONE
    } else {
        type_of(get(c, idx))
    }
}

pub(crate) fn tonumber(c: &NativeCtx<'_>, idx: i32) -> Option<f64> {
    match get(c, idx) {
        Value::Int(i) => Some(i as f64),
        Value::Float(f) => Some(f),
        Value::Str(s) => match str_to_number(c.bytes(s)) {
            Some(Value::Int(i)) => Some(i as f64),
            Some(Value::Float(f)) => Some(f),
            _ => None,
        },
        _ => None,
    }
}

pub(crate) fn tointeger(c: &NativeCtx<'_>, idx: i32) -> Option<i64> {
    let v = match get(c, idx) {
        Value::Str(s) => str_to_number(c.bytes(s))?,
        v => v,
    };
    match v {
        Value::Int(i) => Some(i),
        Value::Float(f) => float_to_int(f),
        _ => None,
    }
}

pub(crate) fn isstring(c: &NativeCtx<'_>, idx: i32) -> bool {
    matches!(get(c, idx), Value::Str(_) | Value::Int(_) | Value::Float(_))
}

pub(crate) fn isinteger(c: &NativeCtx<'_>, idx: i32) -> bool {
    matches!(get(c, idx), Value::Int(_))
}

pub(crate) fn isuserdata(c: &NativeCtx<'_>, idx: i32) -> bool {
    matches!(get(c, idx), Value::UserData(_) | Value::LightUserData(_))
}

pub(crate) fn iscfunction(c: &NativeCtx<'_>, idx: i32) -> bool {
    matches!(get(c, idx), Value::Native(_))
}

pub(crate) fn toboolean(c: &NativeCtx<'_>, idx: i32) -> bool {
    truthy(get(c, idx))
}

/// The string at `idx`, converting a number in place as `lua_tolstring`
/// does; `None` for other types.
pub(crate) fn tolstring(c: &mut NativeCtx<'_>, idx: i32) -> Res<Option<Gc<LuaStr>>> {
    match get(c, idx) {
        Value::Str(s) => Ok(Some(s)),
        v @ (Value::Int(_) | Value::Float(_)) => {
            let mut out = String::new();
            fmt_number(&mut out, v);
            let Value::Str(s) = c.new_str(out.as_bytes()) else {
                unreachable!("new_str makes a string")
            };
            if let Slot::Stack(_) | Slot::Upvalue(_) = slot(c, idx) {
                put(c, idx, Value::Str(s))?;
            }
            Ok(Some(s))
        }
        _ => Ok(None),
    }
}

pub(crate) fn rawlen(c: &NativeCtx<'_>, idx: i32) -> u64 {
    match get(c, idx) {
        Value::Str(s) => c.bytes(s).len() as u64,
        Value::Table(t) => c.heap.get(t).border() as u64,
        Value::UserData(u) => c
            .heap
            .get(u)
            .downcast_ref::<CBlock>()
            .map_or(0, |b| b.size as u64),
        _ => 0,
    }
}

/// The memory behind a userdata C created, or a light userdata's pointer.
/// Userdata a Rust library created (an `io` file) has no C memory: null.
pub(crate) fn touserdata(c: &NativeCtx<'_>, idx: i32) -> *mut std::ffi::c_void {
    match get(c, idx) {
        Value::UserData(u) => c
            .heap
            .get(u)
            .downcast_ref::<CBlock>()
            .map_or(std::ptr::null_mut(), CBlock::as_ptr),
        Value::LightUserData(p) => p.get() as usize as *mut std::ffi::c_void,
        _ => std::ptr::null_mut(),
    }
}

/// A value's identity as a pointer (`lua_topointer`), for hashing and
/// printing only.
pub(crate) fn topointer(c: &NativeCtx<'_>, idx: i32) -> usize {
    match get(c, idx) {
        Value::UserData(_) | Value::LightUserData(_) => touserdata(c, idx) as usize,
        v => v.identity().unwrap_or(0) as usize,
    }
}

pub(crate) fn rawequal(c: &NativeCtx<'_>, a: i32, b: i32) -> bool {
    !is_none(c, a) && !is_none(c, b) && raw_eq(c.heap, get(c, a), get(c, b))
}

pub(crate) fn compare(c: &mut NativeCtx<'_>, s: &Shared, a: i32, b: i32, op: i32) -> Res<bool> {
    if is_none(c, a) || is_none(c, b) {
        return Ok(false);
    }
    let (x, y) = (get(c, a), get(c, b));
    let truth = |_: &NativeCtx<'_>, v: Value| truthy(v);
    match op {
        abi::OPEQ => {
            let m = c.equal(x, y)?;
            finish(c, s, m, truth)
        }
        abi::OPLT => {
            let m = c.less_than(x, y)?;
            finish(c, s, m, truth)
        }
        abi::OPLE => {
            // a <= b as not (b < a); exact for everything but NaN.
            let m = c.less_than(y, x)?;
            Ok(!finish(c, s, m, truth)?)
        }
        _ => Err(c.error("C API: invalid comparison option")),
    }
}

// ---- pushing values ----------------------------------------------------------

pub(crate) fn pushstr(c: &mut NativeCtx<'_>, bytes: &[u8]) -> Gc<LuaStr> {
    let v = c.new_str(bytes);
    c.push(v);
    let Value::Str(s) = v else {
        unreachable!("new_str makes a string")
    };
    s
}

pub(crate) fn pushcclosure(c: &mut NativeCtx<'_>, f: u64, n: i32) -> Res<()> {
    let n = n.max(0) as usize;
    if n > c.len() {
        return Err(c.error("C API: not enough upvalues on the stack"));
    }
    let len = c.len();
    let ups = c.window()[len - n..].to_vec();
    let v = crate::c_function(c, f, &ups);
    c.truncate(len - n);
    c.push(v);
    Ok(())
}

pub(crate) fn light(p: usize) -> Value {
    Value::LightUserData(LightUserData::new(p as u64))
}

// ---- getting and setting ----------------------------------------------------------

pub(crate) fn gettable_with(c: &mut NativeCtx<'_>, s: &Shared, obj: Value, key: Value) -> Res<i32> {
    let v = index(c, s, obj, key)?;
    c.push(v);
    Ok(type_of(v))
}

pub(crate) fn gettable(c: &mut NativeCtx<'_>, s: &Shared, idx: i32) -> Res<i32> {
    let obj = get(c, idx);
    let key = pop(c)?;
    gettable_with(c, s, obj, key)
}

pub(crate) fn getglobal(c: &mut NativeCtx<'_>, s: &Shared, name: &[u8]) -> Res<i32> {
    let g = Value::Table(c.rt.globals);
    let k = c.new_str(name);
    gettable_with(c, s, g, k)
}

pub(crate) fn rawget_with(c: &mut NativeCtx<'_>, idx: i32, key: Value) -> Res<i32> {
    let t = table_at(c, idx)?;
    let v = tbl::get(c.heap, t, key);
    c.push(v);
    Ok(type_of(v))
}

pub(crate) fn rawget(c: &mut NativeCtx<'_>, idx: i32) -> Res<i32> {
    let t = get(c, idx);
    let key = pop(c)?;
    let Value::Table(t) = t else {
        return Err(c.error("C API: table expected"));
    };
    let v = tbl::get(c.heap, t, key);
    c.push(v);
    Ok(type_of(v))
}

pub(crate) fn createtable(c: &mut NativeCtx<'_>, narr: i32, nrec: i32) {
    let t = c.new_table(Table::with_capacity(
        narr.max(0) as usize,
        nrec.max(0) as usize,
    ));
    c.push(Value::Table(t));
}

pub(crate) fn newuserdata(c: &mut NativeCtx<'_>, size: usize) -> Res<*mut std::ffi::c_void> {
    c.reserve(size)?;
    let block = CBlock::new(size);
    let p = block.as_ptr();
    let u = c.new_userdata(block);
    c.push(Value::UserData(u));
    Ok(p)
}

pub(crate) fn getmetatable(c: &mut NativeCtx<'_>, idx: i32) -> bool {
    match c.metatable(get(c, idx)) {
        Some(mt) => {
            c.push(Value::Table(mt));
            true
        }
        None => false,
    }
}

pub(crate) fn settable_with(
    c: &mut NativeCtx<'_>,
    s: &Shared,
    obj: Value,
    key: Value,
    v: Value,
) -> Res<()> {
    set_index(c, s, obj, key, v)
}

pub(crate) fn settable(c: &mut NativeCtx<'_>, s: &Shared, idx: i32) -> Res<()> {
    let obj = get(c, idx);
    let v = pop(c)?;
    let key = pop(c)?;
    // Keep both rooted until the store is done.
    c.push(key);
    c.push(v);
    let r = settable_with(c, s, obj, key, v);
    let len = c.len();
    c.truncate(len - 2);
    r
}

/// `t[k] = top` and pop it, for setfield, seti and setglobal.
pub(crate) fn set_popping(c: &mut NativeCtx<'_>, s: &Shared, obj: Value, key: Value) -> Res<()> {
    let len = c.len();
    if len == 0 {
        return Err(c.error("C API: not enough elements in the stack"));
    }
    let v = c.arg(len - 1);
    c.push(key);
    let r = settable_with(c, s, obj, key, v);
    let len = c.len();
    c.truncate(len - 2);
    r
}

pub(crate) fn setfield(c: &mut NativeCtx<'_>, s: &Shared, idx: i32, k: &[u8]) -> Res<()> {
    let obj = get(c, idx);
    let key = c.new_str(k);
    set_popping(c, s, obj, key)
}

pub(crate) fn setglobal(c: &mut NativeCtx<'_>, s: &Shared, name: &[u8]) -> Res<()> {
    let g = Value::Table(c.rt.globals);
    let key = c.new_str(name);
    set_popping(c, s, g, key)
}

pub(crate) fn rawset_with(c: &mut NativeCtx<'_>, idx: i32, key: Value) -> Res<()> {
    let t = table_at(c, idx)?;
    let v = pop(c)?;
    c.push(key);
    c.push(v);
    let r = tbl::set(c.heap, t, key, v);
    let len = c.len();
    c.truncate(len - 2);
    r.map_err(|e| store_error(c, e))
}

pub(crate) fn rawset(c: &mut NativeCtx<'_>, idx: i32) -> Res<()> {
    let t = table_at(c, idx)?;
    let v = pop(c)?;
    let key = pop(c)?;
    c.push(key);
    c.push(v);
    let r = tbl::set(c.heap, t, key, v);
    let len = c.len();
    c.truncate(len - 2);
    r.map_err(|e| store_error(c, e))
}

pub(crate) fn setmetatable(c: &mut NativeCtx<'_>, idx: i32) -> Res<()> {
    let target = get(c, idx);
    let mt = match pop(c)? {
        Value::Table(t) => Some(t),
        Value::Nil => None,
        _ => return Err(c.error("C API: table expected for metatable")),
    };
    if !gc::set_metatable(c.heap, target, mt) {
        return Err(c.error("C API: cannot set a metatable for this type (spike)"));
    }
    Ok(())
}

// ---- calls ----------------------------------------------------------------

fn take_call(c: &mut NativeCtx<'_>, nargs: i32) -> Res<(Value, Vec<Value>)> {
    let nargs = nargs.max(0) as usize;
    let len = c.len();
    if nargs + 1 > len {
        return Err(c.error("C API: not enough elements for the call"));
    }
    let base = len - nargs - 1;
    let f = c.arg(base);
    let args = c.window()[base + 1..].to_vec();
    // Leave the function and arguments in the window, so they stay rooted
    // until the call takes them.
    Ok((f, args))
}

fn push_results(c: &mut NativeCtx<'_>, nargs: i32, vals: &[Value], nresults: i32) {
    let len = c.len();
    c.truncate(len - nargs.max(0) as usize - 1);
    let want = if nresults == abi::MULTRET {
        vals.len()
    } else {
        nresults.max(0) as usize
    };
    for i in 0..want {
        c.push(vals.get(i).copied().unwrap_or(Value::Nil));
    }
}

pub(crate) fn call(c: &mut NativeCtx<'_>, s: &Shared, nargs: i32, nresults: i32) -> Res<()> {
    let (f, args) = take_call(c, nargs)?;
    let vals = nested(c, s, f, &args)?;
    push_results(c, nargs, &vals, nresults);
    Ok(())
}

pub(crate) fn pcall(
    c: &mut NativeCtx<'_>,
    s: &Shared,
    nargs: i32,
    nresults: i32,
    errfunc: i32,
) -> Res<i32> {
    let (f, args) = take_call(c, nargs)?;
    let handler = (errfunc != 0).then(|| get(c, errfunc));
    match nested(c, s, f, &args) {
        Ok(vals) => {
            push_results(c, nargs, &vals, nresults);
            Ok(abi::OK)
        }
        Err(e) => {
            // Spike: the handler runs after the stack unwound, so it can't
            // see the frames that raised (a traceback handler sees fewer
            // levels than in lua5.5).
            let mut err = e.value;
            let len = c.len();
            c.truncate(len - nargs.max(0) as usize - 1);
            if let Some(h) = handler {
                c.push(err);
                err = match nested(c, s, h, &[err]) {
                    Ok(r) => r.first().copied().unwrap_or(Value::Nil),
                    Err(e2) => e2.value,
                };
                let len = c.len();
                c.truncate(len - 1);
            }
            c.push(err);
            Ok(abi::ERRRUN)
        }
    }
}

// ---- miscellaneous ----------------------------------------------------------

pub(crate) fn error(c: &mut NativeCtx<'_>) -> LuaError {
    let len = c.len();
    let v = if len > 0 { c.arg(len - 1) } else { Value::Nil };
    c.error_value(v)
}

pub(crate) fn next(c: &mut NativeCtx<'_>, idx: i32) -> Res<bool> {
    let t = table_at(c, idx)?;
    let k = pop(c)?;
    match c.heap.get(t).next(c.heap, k) {
        Ok(Some((k, v))) => {
            c.push(k);
            c.push(v);
            Ok(true)
        }
        Ok(None) => Ok(false),
        Err(_) => {
            let msg = c.new_str(b"invalid key to 'next'");
            Err(c.error_value(msg))
        }
    }
}

pub(crate) fn concat(c: &mut NativeCtx<'_>, n: i32) -> Res<()> {
    let n = n.max(0) as usize;
    let len = c.len();
    if n > len {
        return Err(c.error("C API: not enough elements to concatenate"));
    }
    let mut out = Vec::new();
    for i in len - n..len {
        match c.arg(i) {
            v @ (Value::Str(_) | Value::Int(_) | Value::Float(_)) => c.display(v, &mut out),
            v => {
                return Err(c.error(format!(
                    "attempt to concatenate a {} value (spike: no __concat from C)",
                    v.type_name()
                )));
            }
        }
    }
    c.truncate(len - n);
    pushstr(c, &out);
    Ok(())
}

pub(crate) fn len(c: &mut NativeCtx<'_>, s: &Shared, idx: i32) -> Res<()> {
    let v = get(c, idx);
    let m = c.length(v)?;
    let r = finish(c, s, m, |_, v| v)?;
    c.push(r);
    Ok(())
}

pub(crate) fn stringtonumber(c: &mut NativeCtx<'_>, bytes: &[u8]) -> usize {
    match str_to_number(bytes) {
        Some(v) => {
            c.push(v);
            bytes.len() + 1
        }
        None => 0,
    }
}

pub(crate) fn gc(c: &mut NativeCtx<'_>, what: i32) -> i32 {
    match what {
        abi::GCCOLLECT => {
            c.full_collect();
            0
        }
        abi::GCCOUNT => (c.heap.bytes() / 1024) as i32,
        abi::GCCOUNTB => (c.heap.bytes() % 1024) as i32,
        abi::GCSTOP | abi::GCRESTART => 0,
        _ => 0,
    }
}

// ---- auxiliary library pieces that need the VM ------------------------------

pub(crate) fn argerror(c: &mut NativeCtx<'_>, arg: i32, msg: &str) -> LuaError {
    c.arg_error((arg.max(1) - 1) as usize, msg)
}

pub(crate) fn typeerror(c: &mut NativeCtx<'_>, arg: i32, tname: &str) -> LuaError {
    c.type_error((arg.max(1) - 1) as usize, tname)
}

pub(crate) fn where_(c: &mut NativeCtx<'_>, lvl: i32) {
    let w = c.where_(lvl.max(0) as usize);
    pushstr(c, w.as_bytes());
}

/// `luaL_tolstring`: `__tostring`, else the plain conversion (with
/// `__name`). Pushes the result.
pub(crate) fn tolstring_meta(c: &mut NativeCtx<'_>, s: &Shared, idx: i32) -> Res<Gc<LuaStr>> {
    let v = get(c, idx);
    let mm = c.metamethod(v, Event::ToString);
    if !mm.is_nil() {
        let r = nested(c, s, mm, &[v])?;
        return match r.first() {
            Some(&Value::Str(st)) => {
                c.push(Value::Str(st));
                Ok(st)
            }
            _ => Err(c.error("'__tostring' must return a string")),
        };
    }
    let mut out = Vec::new();
    c.display(v, &mut out);
    Ok(pushstr(c, &out))
}
