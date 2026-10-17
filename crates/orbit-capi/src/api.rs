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
use orbit_vm::{LuaError, Meta, MetaField, NativeCtx, Operator};

use crate::CBlock;

pub(crate) type Res<T> = Result<T, LuaError>;

/// Constants from the C ABI (`lua.h`).
pub(crate) mod abi {
    /// Hook events (`lua_Debug.event`).
    pub const HOOKCALL: i32 = 0;
    pub const HOOKRET: i32 = 1;
    pub const HOOKLINE: i32 = 2;
    pub const HOOKCOUNT: i32 = 3;
    pub const HOOKTAILCALL: i32 = 4;
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
    pub const ERRSYNTAX: i32 = 3;
    pub const ERRMEM: i32 = 4;
    pub const ERRERR: i32 = 5;
    pub const OPEQ: i32 = 0;
    pub const OPLT: i32 = 1;
    pub const OPLE: i32 = 2;
    pub const GCSTOP: i32 = 0;
    pub const GCRESTART: i32 = 1;
    pub const GCCOLLECT: i32 = 2;
    pub const GCCOUNT: i32 = 3;
    pub const GCCOUNTB: i32 = 4;
    pub const GCSTEP: i32 = 5;
    pub const GCISRUNNING: i32 = 6;
    pub const GCGEN: i32 = 7;
    pub const GCINC: i32 = 8;
    pub const GCPARAM: i32 = 9;
    /// `LUA_RIDX_MAINTHREAD`: where the registry keeps the main thread.
    pub const RIDX_MAINTHREAD: i64 = 3;
}

/// Per-VM state the API keeps between calls. Borrowed only for short
/// stretches, never across a call back into Lua, because that call can run
/// C functions that borrow it again.
#[derive(Debug, Default)]
pub(crate) struct State {
    /// The error a Rust entry point raised, for the trampoline's caller.
    pub pending: Option<Value>,
    /// NUL-terminated copies of strings handed to C. Each C call drops the
    /// copies made during it when it returns. Between calls, in a state C
    /// created (ADR-10), copies of string values (`lua_tolstring`) last
    /// while an equal string is on the host's stack; see
    /// [`State::prune`].
    pub strs: Vec<CStrCopy>,
    /// How many copies the last [`State::prune`] kept.
    pub kept: usize,
}

/// A string handed to C.
#[derive(Debug)]
pub(crate) struct CStrCopy {
    /// The bytes and a trailing NUL.
    pub bytes: Box<[u8]>,
    /// A copy of a string value, which may go once no equal string is on
    /// the host's stack. Other copies (`lua_getinfo`'s source) stay until
    /// the state closes.
    pub value: bool,
}

impl State {
    /// Drop the copies of string values that no string on `stack` equals.
    /// Runs when the copies have doubled since the last pruning, so each
    /// costs O(1) amortized.
    pub fn prune(&mut self, stack: &[&[u8]]) {
        if self.strs.len() < 2 * self.kept + 16 {
            return;
        }
        self.strs.retain(|s| {
            !s.value || {
                let text = &s.bytes[..s.bytes.len() - 1];
                stack.contains(&text)
            }
        });
        self.kept = self.strs.len();
    }
}

pub(crate) type Shared = RefCell<State>;

/// Where an index points.
#[derive(Clone, Copy)]
enum Slot {
    Stack(usize),
    Registry,
    /// One of the running C function's upvalues (0-based).
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
        // lua_upvalueindex(i), 1-based. An index past the function's
        // upvalues is acceptable but reads as none.
        let i = (abi::REGISTRYINDEX as i64 - idx as i64) as usize;
        if i <= c.upvalue_count() {
            Slot::Upvalue(i - 1)
        } else {
            Slot::None
        }
    } else {
        let at = idx as i64 - 1;
        if at < len {
            Slot::Stack(at as usize)
        } else {
            Slot::None
        }
    }
}

pub(crate) fn get(c: &NativeCtx<'_>, idx: i32) -> Value {
    match slot(c, idx) {
        Slot::Stack(i) => c.arg(i),
        Slot::Registry => Value::Table(c.vm.rt.registry),
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

fn need(c: &mut NativeCtx<'_>, n: usize) -> Res<()> {
    if c.len() < n {
        return Err(c.error("C API: not enough elements in the stack"));
    }
    Ok(())
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

pub(crate) fn light(p: usize) -> Value {
    Value::LightUserData(LightUserData::new(p as u64))
}

// ---- calls back into Lua ---------------------------------------------------

/// Finish an operation that may need a metamethod: run it now. For a call,
/// its arguments are the last `nargs` values of the window.
fn finish<T>(
    c: &mut NativeCtx<'_>,
    m: Meta<T>,
    conv: impl FnOnce(&NativeCtx<'_>, Value) -> T,
) -> Res<T> {
    match m {
        Meta::Done(v) => Ok(v),
        Meta::Call { f, nargs } => {
            let at = c.len() - nargs;
            c.insert(at, f);
            let n = c.call(nargs, None)?;
            let r = if n > 0 { c.arg(at) } else { Value::Nil };
            c.truncate(at);
            Ok(conv(c, r))
        }
    }
}

fn index(c: &mut NativeCtx<'_>, obj: Value, key: Value) -> Res<Value> {
    let m = c.index(obj, key)?;
    finish(c, m, |_, v| v)
}

fn set_index(c: &mut NativeCtx<'_>, obj: Value, key: Value, v: Value) -> Res<()> {
    let m = c.set_index(obj, key, v)?;
    finish(c, m, |_, _| ())
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
    // Variables marked with lua_toclose above the new top close first.
    c.close_from(new)?;
    c.truncate(new);
    Ok(())
}

/// The window slot a stack index names, or an error.
fn stack_slot(c: &mut NativeCtx<'_>, idx: i32) -> Res<usize> {
    match slot(c, idx) {
        Slot::Stack(i) => Ok(i),
        _ => Err(c.error("C API: invalid stack index")),
    }
}

/// `lua_toclose`.
pub(crate) fn toclose(c: &mut NativeCtx<'_>, idx: i32) -> Res<()> {
    let i = stack_slot(c, idx)?;
    c.to_be_closed(i)
}

/// `lua_closeslot`: close the variable at `idx`, the newest one marked
/// with `lua_toclose`, and set the slot to nil.
pub(crate) fn closeslot(c: &mut NativeCtx<'_>, idx: i32) -> Res<()> {
    let i = stack_slot(c, idx)?;
    if c.is_newest_to_be_closed(i) {
        c.close_from(i)?;
    }
    c.set(i, Value::Nil);
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

/// The most values a C function's stack may hold, as `LUAI_MAXSTACK`.
const MAX_STACK: usize = 1_000_000;

pub(crate) fn checkstack(c: &NativeCtx<'_>, n: i32) -> bool {
    n >= 0 && c.len() + n as usize <= MAX_STACK
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

/// Whether `idx` holds a function written in C, or in Rust: anything that
/// isn't a Lua function, as `lua_iscfunction` sees natives.
pub(crate) fn iscfunction(c: &NativeCtx<'_>, idx: i32) -> bool {
    c.native_of(get(c, idx)).is_some()
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

/// The number at `idx` as Lua prints it (`lua_numbertocstring`), or `None`
/// if it isn't a number.
pub(crate) fn number_text(c: &NativeCtx<'_>, idx: i32) -> Option<String> {
    match get(c, idx) {
        v @ (Value::Int(_) | Value::Float(_)) => {
            let mut out = String::new();
            fmt_number(&mut out, v);
            Some(out)
        }
        _ => None,
    }
}

pub(crate) fn rawlen(c: &NativeCtx<'_>, idx: i32) -> u64 {
    match get(c, idx) {
        Value::Str(s) => c.bytes(s).len() as u64,
        Value::Table(t) => c.vm.heap.get(t).border() as u64,
        Value::UserData(u) => {
            c.vm.heap
                .get(u)
                .downcast_ref::<CBlock>()
                .map_or(0, |b| b.size as u64)
        }
        _ => 0,
    }
}

/// The memory behind a userdata C created, or a light userdata's pointer.
/// Userdata a Rust library created (an `io` file) has no C memory: null.
pub(crate) fn touserdata(c: &NativeCtx<'_>, idx: i32) -> *mut std::ffi::c_void {
    match get(c, idx) {
        Value::UserData(u) => {
            c.vm.heap
                .get(u)
                .downcast_ref::<CBlock>()
                .map_or(std::ptr::null_mut(), CBlock::as_ptr)
        }
        Value::LightUserData(p) => p.get() as usize as *mut std::ffi::c_void,
        _ => std::ptr::null_mut(),
    }
}

/// Whether `idx` is a userdata that a Rust library made, so it has no C
/// memory (for `luaL_checkudata`'s message).
pub(crate) fn is_rust_userdata(c: &NativeCtx<'_>, idx: i32) -> bool {
    match get(c, idx) {
        Value::UserData(u) => !c.vm.heap.get(u).is::<CBlock>(),
        _ => false,
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

/// The C function behind the value at `idx`, if it is one: its address.
pub(crate) fn tocfunction(c: &mut NativeCtx<'_>, idx: i32) -> Option<u64> {
    let v = get(c, idx);
    if c.native_of(v) != Some(crate::c_native_id(c)) {
        return None;
    }
    match c.hidden_upvalue_of(v, 0) {
        Value::LightUserData(p) => Some(p.get()),
        _ => None,
    }
}

pub(crate) fn rawequal(c: &NativeCtx<'_>, a: i32, b: i32) -> bool {
    !is_none(c, a) && !is_none(c, b) && raw_eq(&c.vm.heap, get(c, a), get(c, b))
}

pub(crate) fn compare(c: &mut NativeCtx<'_>, a: i32, b: i32, op: i32) -> Res<bool> {
    if is_none(c, a) || is_none(c, b) {
        return Ok(false);
    }
    let (x, y) = (get(c, a), get(c, b));
    let truth = |_: &NativeCtx<'_>, v: Value| truthy(v);
    let m = match op {
        abi::OPEQ => c.equal(x, y)?,
        abi::OPLT => c.less_than(x, y)?,
        abi::OPLE => c.less_equal(x, y)?,
        _ => return Err(c.error("C API: invalid comparison option")),
    };
    finish(c, m, truth)
}

// ---- arithmetic and strings ------------------------------------------------

pub(crate) fn arith(c: &mut NativeCtx<'_>, op: i32) -> Res<()> {
    const OPS: [Operator; 14] = [
        Operator::Add,
        Operator::Sub,
        Operator::Mul,
        Operator::Mod,
        Operator::Pow,
        Operator::Div,
        Operator::IDiv,
        Operator::BAnd,
        Operator::BOr,
        Operator::BXor,
        Operator::Shl,
        Operator::Shr,
        Operator::Unm,
        Operator::BNot,
    ];
    let Some(&o) = OPS.get(op as usize) else {
        return Err(c.error("C API: invalid arithmetic operator"));
    };
    let unary = matches!(o, Operator::Unm | Operator::BNot);
    need(c, if unary { 1 } else { 2 })?;
    let len = c.len();
    let (a, b) = if unary {
        (c.arg(len - 1), c.arg(len - 1))
    } else {
        (c.arg(len - 2), c.arg(len - 1))
    };
    let m = c.arith(o, a, b)?;
    let r = finish(c, m, |_, v| v)?;
    // The operands stay on the stack (rooted) until the result replaces them.
    let len = c.len();
    c.truncate(len - if unary { 1 } else { 2 });
    c.push(r);
    Ok(())
}

pub(crate) fn concat(c: &mut NativeCtx<'_>, n: i32) -> Res<()> {
    let n = n.max(0) as usize;
    need(c, n)?;
    if n == 0 {
        pushstr(c, b"");
        return Ok(());
    }
    // Right to left, as `..` associates: each step joins the last two.
    let mut left = n;
    while left > 1 {
        let len = c.len();
        let (a, b) = (c.arg(len - 2), c.arg(len - 1));
        let m = c.concat(a, b)?;
        let r = finish(c, m, |_, v| v)?;
        let len = c.len();
        c.truncate(len - 2);
        c.push(r);
        left -= 1;
    }
    Ok(())
}

pub(crate) fn len(c: &mut NativeCtx<'_>, idx: i32) -> Res<()> {
    let v = get(c, idx);
    let m = c.length(v)?;
    let r = finish(c, m, |_, v| v)?;
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

// ---- pushing values ----------------------------------------------------------

pub(crate) fn pushstr(c: &mut NativeCtx<'_>, bytes: &[u8]) -> Gc<LuaStr> {
    let v = c.new_str(bytes);
    c.push(v);
    let Value::Str(s) = v else {
        unreachable!("new_str makes a string")
    };
    s
}

/// A registry key no module can make: the address of a static of ours.
fn private_key(key: &'static u8) -> Value {
    light(std::ptr::from_ref(key) as usize)
}

static C_FUNCTIONS: u8 = 0;

pub(crate) fn pushcclosure(c: &mut NativeCtx<'_>, f: u64, n: i32) -> Res<()> {
    let n = n.max(0) as usize;
    need(c, n)?;
    if n == 0 {
        // One function value per C function, so pushing it twice gives
        // equal values, as in Lua (light C functions).
        let reg = c.vm.rt.registry;
        let cache = match tbl::get(&c.vm.heap, reg, private_key(&C_FUNCTIONS)) {
            Value::Table(t) => t,
            _ => {
                let t = c.new_table(Table::new());
                c.push(Value::Table(t));
                let r = tbl::set(
                    &mut c.vm.heap,
                    reg,
                    private_key(&C_FUNCTIONS),
                    Value::Table(t),
                );
                let len = c.len();
                c.truncate(len - 1);
                r.map_err(|e| store_error(c, e))?;
                t
            }
        };
        let key = light(f as usize);
        let v = match tbl::get(&c.vm.heap, cache, key) {
            v @ Value::Closure(_) => v,
            _ => {
                let v = crate::c_function(c, f, &[]);
                c.push(v);
                let r = tbl::set(&mut c.vm.heap, cache, key, v);
                let len = c.len();
                c.truncate(len - 1);
                r.map_err(|e| store_error(c, e))?;
                v
            }
        };
        c.push(v);
        return Ok(());
    }
    let len = c.len();
    let ups = c.window()[len - n..].to_vec();
    let v = crate::c_function(c, f, &ups);
    c.truncate(len - n);
    c.push(v);
    Ok(())
}

/// Push the running thread (`lua_pushthread`); true if it is the main one.
pub(crate) fn pushthread(c: &mut NativeCtx<'_>) -> bool {
    let (t, main) = c.running();
    c.push(Value::Thread(t));
    main
}

// ---- getting and setting ----------------------------------------------------------

pub(crate) fn gettable_with(c: &mut NativeCtx<'_>, obj: Value, key: Value) -> Res<i32> {
    // Keep the key rooted while metamethods run.
    c.push(key);
    let v = index(c, obj, key);
    let len = c.len();
    c.truncate(len - 1);
    let v = v?;
    c.push(v);
    Ok(type_of(v))
}

pub(crate) fn gettable(c: &mut NativeCtx<'_>, idx: i32) -> Res<i32> {
    let obj = get(c, idx);
    let key = pop(c)?;
    gettable_with(c, obj, key)
}

pub(crate) fn getglobal(c: &mut NativeCtx<'_>, name: &[u8]) -> Res<i32> {
    let g = Value::Table(c.vm.rt.globals);
    let k = c.new_str(name);
    gettable_with(c, g, k)
}

pub(crate) fn rawget_with(c: &mut NativeCtx<'_>, idx: i32, key: Value) -> Res<i32> {
    let t = table_at(c, idx)?;
    let v = tbl::get(&c.vm.heap, t, key);
    c.push(v);
    Ok(type_of(v))
}

pub(crate) fn rawget(c: &mut NativeCtx<'_>, idx: i32) -> Res<i32> {
    let t = table_at(c, idx)?;
    let key = pop(c)?;
    let v = tbl::get(&c.vm.heap, t, key);
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

pub(crate) fn newuserdata(
    c: &mut NativeCtx<'_>,
    size: usize,
    nuv: i32,
) -> Res<*mut std::ffi::c_void> {
    if nuv < 0 || nuv > u16::MAX as i32 {
        return Err(c.error("C API: invalid number of user values"));
    }
    c.reserve(size)?;
    let block = CBlock::new(size, nuv as u16);
    let p = block.as_ptr();
    let u = c.new_userdata(block);
    c.push(Value::UserData(u));
    if nuv >= 2 {
        // More than one user value: they live in a table in the userdata's
        // one traced slot.
        let t = c.new_table(Table::with_capacity(nuv as usize, 0));
        c.vm.heap.update(u, |u| u.user_value = Value::Table(t));
    }
    Ok(p)
}

/// Where user value `n` (1-based) of the userdata at `idx` is kept, or
/// `None` if it has no such value.
enum UserValue {
    /// The userdata's own slot (one user value, or a Rust userdata's).
    Slot(Gc<orbit_vm::orbit_core::UserData>),
    /// Entry `n` of the table in that slot.
    InTable(Gc<Table>, i64),
}

fn user_value(c: &NativeCtx<'_>, idx: i32, n: i32) -> Option<UserValue> {
    let Value::UserData(u) = get(c, idx) else {
        return None;
    };
    let ud = c.vm.heap.get(u);
    let count = ud.downcast_ref::<CBlock>().map_or(1, |b| b.nuv as i32);
    if n < 1 || n > count {
        return None;
    }
    if count == 1 {
        return Some(UserValue::Slot(u));
    }
    match ud.user_value {
        Value::Table(t) => Some(UserValue::InTable(t, n as i64)),
        _ => None,
    }
}

pub(crate) fn getiuservalue(c: &mut NativeCtx<'_>, idx: i32, n: i32) -> i32 {
    let v = match user_value(c, idx, n) {
        Some(UserValue::Slot(u)) => c.vm.heap.get(u).user_value,
        Some(UserValue::InTable(t, i)) => c.vm.heap.get(t).get_int(i),
        None => {
            c.push(Value::Nil);
            return abi::TNONE;
        }
    };
    c.push(v);
    type_of(v)
}

pub(crate) fn setiuservalue(c: &mut NativeCtx<'_>, idx: i32, n: i32) -> Res<bool> {
    need(c, 1)?;
    let len = c.len();
    let v = c.arg(len - 1);
    let r = match user_value(c, idx, n) {
        Some(UserValue::Slot(u)) => {
            c.vm.heap.update(u, |u| u.user_value = v);
            true
        }
        Some(UserValue::InTable(t, i)) => {
            tbl::set_int(&mut c.vm.heap, t, i, v).map_err(|_| c.no_memory())?;
            true
        }
        None => false,
    };
    c.truncate(len - 1);
    Ok(r)
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

pub(crate) fn settable(c: &mut NativeCtx<'_>, idx: i32) -> Res<()> {
    need(c, 2)?;
    let obj = get(c, idx);
    let len = c.len();
    let (key, v) = (c.arg(len - 2), c.arg(len - 1));
    // Both stay rooted until the store is done.
    set_index(c, obj, key, v)?;
    c.truncate(len - 2);
    Ok(())
}

/// `obj[key] = top` and pop it, for setfield, seti and setglobal.
pub(crate) fn set_popping(c: &mut NativeCtx<'_>, obj: Value, key: Value) -> Res<()> {
    need(c, 1)?;
    let len = c.len();
    let v = c.arg(len - 1);
    c.push(key);
    set_index(c, obj, key, v)?;
    c.truncate(len - 1);
    Ok(())
}

pub(crate) fn setfield(c: &mut NativeCtx<'_>, idx: i32, k: &[u8]) -> Res<()> {
    let obj = get(c, idx);
    let key = c.new_str(k);
    set_popping(c, obj, key)
}

pub(crate) fn setglobal(c: &mut NativeCtx<'_>, name: &[u8]) -> Res<()> {
    let g = Value::Table(c.vm.rt.globals);
    let key = c.new_str(name);
    set_popping(c, g, key)
}

pub(crate) fn rawset_with(c: &mut NativeCtx<'_>, idx: i32, key: Value) -> Res<()> {
    let t = table_at(c, idx)?;
    need(c, 1)?;
    let len = c.len();
    let v = c.arg(len - 1);
    c.push(key);
    let r = tbl::set(&mut c.vm.heap, t, key, v);
    c.truncate(len - 1);
    r.map_err(|e| store_error(c, e))
}

pub(crate) fn rawset(c: &mut NativeCtx<'_>, idx: i32) -> Res<()> {
    let t = table_at(c, idx)?;
    need(c, 2)?;
    let len = c.len();
    let (key, v) = (c.arg(len - 2), c.arg(len - 1));
    let r = tbl::set(&mut c.vm.heap, t, key, v);
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
    if !gc::set_metatable(&mut c.vm.heap, target, mt) {
        // Types without their own metatable share one per type, as in Lua.
        let tag = target.tag();
        c.vm.rt.set_type_meta(tag, mt);
    }
    Ok(())
}

// ---- calls ----------------------------------------------------------------

fn check_call(c: &mut NativeCtx<'_>, nargs: i32) -> Res<usize> {
    let nargs = nargs.max(0) as usize;
    if nargs + 1 > c.len() {
        return Err(c.error("C API: not enough elements for the call"));
    }
    Ok(nargs)
}

/// Keep `nresults` of the `n` results at the top (all for `LUA_MULTRET`).
fn adjust(c: &mut NativeCtx<'_>, n: usize, nresults: i32) {
    if nresults == abi::MULTRET {
        return;
    }
    let want = nresults.max(0) as usize;
    let len = c.len();
    if n > want {
        c.truncate(len - (n - want));
    } else {
        for _ in n..want {
            c.push(Value::Nil);
        }
    }
}

pub(crate) fn call(c: &mut NativeCtx<'_>, nargs: i32, nresults: i32) -> Res<()> {
    let nargs = check_call(c, nargs)?;
    let n = c.call(nargs, None)?;
    adjust(c, n, nresults);
    Ok(())
}

pub(crate) fn pcall(c: &mut NativeCtx<'_>, nargs: i32, nresults: i32, errfunc: i32) -> Res<i32> {
    let nargs = check_call(c, nargs)?;
    let handler = if errfunc == 0 {
        None
    } else {
        match slot(c, errfunc) {
            Slot::Stack(i) => Some(i),
            _ => return Err(c.error("C API: invalid message handler index")),
        }
    };
    match c.call(nargs, handler) {
        Ok(n) => {
            adjust(c, n, nresults);
            Ok(abi::OK)
        }
        Err(e) => {
            let status = error_status(c, e.value);
            c.push(e.value);
            Ok(status)
        }
    }
}

/// The status code for an error that left a protected call.
fn error_status(c: &NativeCtx<'_>, v: Value) -> i32 {
    match v {
        Value::Str(s) if c.bytes(s) == orbit_vm::orbit_core::NoMemory::MESSAGE.as_bytes() => {
            abi::ERRMEM
        }
        Value::Str(s) if c.bytes(s) == b"error in error handling" => abi::ERRERR,
        _ => abi::ERRRUN,
    }
}

/// `lua_load`: compile or decode `bytes` as a chunk named `name`, whose
/// first upvalue is the global table. Pushes the function or the message.
pub(crate) fn load(c: &mut NativeCtx<'_>, bytes: &[u8], name: &[u8], mode: &[u8]) -> i32 {
    let env = Value::Table(c.vm.rt.globals);
    match orbit_stdlib::load_chunk_in(c, bytes, name, mode, env) {
        Ok(f) => {
            c.push(f);
            abi::OK
        }
        Err(msg) => {
            let status = if msg.contains(orbit_vm::orbit_core::NoMemory::MESSAGE) {
                abi::ERRMEM
            } else {
                abi::ERRSYNTAX
            };
            pushstr(c, msg.as_bytes());
            status
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
    match c.vm.heap.get(t).next(&c.vm.heap, k) {
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

/// `lua_gc`. `a` and `b` are the option's arguments: the step size for
/// `LUA_GCSTEP`, the parameter and its new value (negative: unchanged) for
/// `LUA_GCPARAM`. Inside a finalizer every option does nothing and returns
/// -1, as in Lua.
pub(crate) fn gc(c: &mut NativeCtx<'_>, what: i32, a: i32, b: i32) -> i32 {
    use orbit_vm::{GcMode, GcParam};
    if c.vm.rt.gc.finalizing() {
        return -1;
    }
    let mode_code = |m: GcMode| match m {
        GcMode::Generational => abi::GCGEN,
        GcMode::Incremental => abi::GCINC,
    };
    match what {
        abi::GCCOLLECT => {
            c.full_collect();
            0
        }
        abi::GCSTEP => match c.vm.rt.gc.mode {
            GcMode::Generational => {
                c.full_collect();
                0
            }
            GcMode::Incremental => {
                let bytes = usize::try_from(a).unwrap_or(0).saturating_mul(1024);
                c.vm.heap.step_with(&mut c.vm.rt, bytes) as i32
            }
        },
        abi::GCCOUNT => (c.vm.heap.bytes() / 1024) as i32,
        abi::GCCOUNTB => (c.vm.heap.bytes() % 1024) as i32,
        abi::GCSTOP => {
            c.vm.heap.stop();
            0
        }
        abi::GCRESTART => {
            c.vm.heap.restart();
            0
        }
        abi::GCISRUNNING => !c.vm.heap.is_stopped() as i32,
        abi::GCGEN | abi::GCINC => {
            let mode = if what == abi::GCGEN {
                GcMode::Generational
            } else {
                GcMode::Incremental
            };
            mode_code(std::mem::replace(&mut c.vm.rt.gc.mode, mode))
        }
        abi::GCPARAM => {
            // lua.h's LUA_GCP* order.
            let p = match a {
                0 => GcParam::MinorMul,
                1 => GcParam::MajorMinor,
                2 => GcParam::MinorMajor,
                3 => GcParam::Pause,
                4 => GcParam::StepMul,
                5 => GcParam::StepSize,
                _ => return -1,
            };
            let old = if b >= 0 {
                c.vm.rt.gc.set_param(&mut c.vm.heap, p, i64::from(b))
            } else {
                c.vm.rt.gc.param(p)
            };
            i32::try_from(old).unwrap_or(i32::MAX)
        }
        _ => -1,
    }
}

pub(crate) fn warning(c: &mut NativeCtx<'_>, msg: &[u8], tocont: bool) {
    if let Some(hook) = &mut c.vm.rt.warn_hook {
        (hook.0)(msg, tocont);
        return;
    }
    // Pieces are joined until the last one (`tocont` false) arrives.
    let pending = c.vm.rt.ext.get_or_insert_with(WarnBuffer::default);
    let first = pending.0.is_empty();
    pending.0.extend_from_slice(msg);
    if tocont {
        return;
    }
    let text = std::mem::take(&mut pending.0);
    // A one-piece message starting with '@' is a control message.
    match (first, text.as_slice()) {
        (true, b"@on") => c.vm.rt.warnings = true,
        (true, b"@off") => c.vm.rt.warnings = false,
        (true, [b'@', ..]) => {}
        _ => c.vm.rt.warn(&text),
    }
}

/// A warning whose pieces are still arriving (`lua_warning` with `tocont`).
#[derive(Debug, Default)]
struct WarnBuffer(Vec<u8>);

/// `luaopen_*`: open standard library `lib` (its index in
/// [`orbit_stdlib::Lib::ALL`]) and push its table, as `luaL_requiref`
/// expects. Like Lua's, it leaves the global to `luaL_requiref`: a library
/// opened through `package.preload` gets no global. Returns 0, pushing
/// nothing, for a library this build leaves out.
pub(crate) fn openlib(c: &mut NativeCtx<'_>, lib: i32) -> i32 {
    let Some(&lib) = usize::try_from(lib)
        .ok()
        .and_then(|i| orbit_stdlib::Lib::ALL.get(i))
    else {
        return 0;
    };
    // `registry.LUA_NOENV` (set by `lua -E`) makes the libraries ignore
    // environment variables.
    let k = c.vm.new_str(b"LUA_NOENV");
    let noenv = tbl::get(&c.vm.heap, c.vm.rt.registry, k);
    let options = orbit_stdlib::Options {
        ignore_env: !noenv.is_falsy(),
        ..Default::default()
    };
    let before = (lib != orbit_stdlib::Lib::Base).then(|| c.vm.global(lib.name()));
    let Some(t) = orbit_stdlib::open_one(c.vm, lib, options) else {
        return 0;
    };
    if let Some(v) = before {
        c.vm.set_global(lib.name(), v);
    }
    c.push(t);
    1
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

/// `luaL_traceback` for the running thread: `msg`, then the traceback from
/// level `level`, pushed as one string.
pub(crate) fn traceback(c: &mut NativeCtx<'_>, msg: Option<&[u8]>, level: i32) {
    let mut out = Vec::new();
    if let Some(m) = msg {
        out.extend_from_slice(m);
        out.push(b'\n');
    }
    out.extend_from_slice(c.traceback(level.max(0) as usize).as_bytes());
    pushstr(c, &out);
}

/// `luaL_tolstring`: `__tostring`, else the plain conversion (with
/// `__name`). Pushes the result.
pub(crate) fn tolstring_meta(c: &mut NativeCtx<'_>, idx: i32) -> Res<Gc<LuaStr>> {
    let v = get(c, idx);
    let mm = c.metamethod(v, MetaField::ToString);
    if !mm.is_nil() {
        c.push(mm);
        c.push(v);
        let n = c.call(1, None)?;
        adjust(c, n, 1);
        let len = c.len();
        return match c.arg(len - 1) {
            Value::Str(st) => Ok(st),
            _ => Err(c.error("'__tostring' must return a string")),
        };
    }
    let mut out = Vec::new();
    c.display(v, &mut out);
    Ok(pushstr(c, &out))
}

// ---- debug interface ---------------------------------------------------------

/// What `lua_getinfo` fills in, before it goes into the C struct.
#[derive(Debug, Default)]
pub(crate) struct Info {
    pub name: Option<Vec<u8>>,
    pub namewhat: &'static str,
    pub what: &'static str,
    pub source: Vec<u8>,
    pub short_src: String,
    pub currentline: i32,
    pub linedefined: i32,
    pub lastlinedefined: i32,
    pub nups: u8,
    pub nparams: u8,
    pub isvararg: bool,
    pub extraargs: u8,
    pub istailcall: bool,
}

/// Whether there is an activation at `level` (`lua_getstack`).
pub(crate) fn has_level(c: &NativeCtx<'_>, level: i32) -> bool {
    level >= 0 && c.level(level as usize).is_some()
}

/// `lua_getinfo` for activation `level`, or for function `f` when `level`
/// is `None`. `None` if there is no such level.
pub(crate) fn info(c: &NativeCtx<'_>, level: Option<usize>, f: Value) -> Option<Info> {
    let (func, at) = match level {
        Some(n) => {
            let l = c.level(n)?;
            (c.level_func(l), Some(l))
        }
        None => (f, None),
    };
    let fi = c.func_info(func);
    let mut out = Info {
        what: fi.what,
        source: fi.source,
        short_src: fi.short_src,
        linedefined: fi.line_defined as i32,
        lastlinedefined: fi.last_line as i32,
        nups: fi.nups.min(255) as u8,
        nparams: fi.nparams,
        isvararg: fi.is_vararg,
        currentline: -1,
        namewhat: "",
        ..Info::default()
    };
    if let Some(l) = at {
        out.currentline = c.current_line(l) as i32;
        out.istailcall = c.is_tail_call(l);
        out.extraargs = c.extra_args(l);
        if let Some(n) = c.level_name(l) {
            out.namewhat = n.namewhat;
            out.name = Some(n.name);
        }
    }
    Some(out)
}

/// `lua_getlocal` at activation `level`: local `n`'s name, its value
/// pushed; `None`, pushing nothing, if there is no such local.
pub(crate) fn getlocal(c: &mut NativeCtx<'_>, level: usize, n: i32) -> Option<Vec<u8>> {
    let l = c.level(level)?;
    let (name, v) = c.get_local(l, n.into())?;
    c.push(v);
    Some(name)
}

/// `lua_getlocal` with no activation: the name of parameter `n` of the
/// Lua function on top of the stack, pushing nothing.
pub(crate) fn param_name(c: &mut NativeCtx<'_>, n: i32) -> Option<Vec<u8>> {
    let f = c.arg(c.len().checked_sub(1)?);
    c.param_name(f, n.into())
}

/// `lua_setlocal`: pop the top value into local `n` of activation `level`
/// and return its name; with no such local, pop nothing.
pub(crate) fn setlocal(c: &mut NativeCtx<'_>, level: usize, n: i32) -> Res<Option<Vec<u8>>> {
    need(c, 1)?;
    let len = c.len();
    let v = c.arg(len - 1);
    let Some(l) = c.level(level) else {
        return Ok(None);
    };
    let name = c.set_local(l, n.into(), v);
    if name.is_some() {
        c.truncate(len - 1);
    }
    Ok(name)
}

/// The function at activation `level`, pushed (`lua_getinfo`'s `f`).
pub(crate) fn push_level_func(c: &mut NativeCtx<'_>, level: usize) {
    let f = c.level(level).map_or(Value::Nil, |l| c.level_func(l));
    c.push(f);
}

/// `lua_getupvalue`/`lua_setupvalue` on the function at `funcindex`.
pub(crate) fn getupvalue(c: &mut NativeCtx<'_>, funcindex: i32, n: i32) -> Option<Vec<u8>> {
    let f = get(c, funcindex);
    let (name, v) = c.upvalue_of(f, n as i64)?;
    c.push(v);
    Some(name)
}

pub(crate) fn setupvalue(c: &mut NativeCtx<'_>, funcindex: i32, n: i32) -> Res<Option<Vec<u8>>> {
    need(c, 1)?;
    let f = get(c, funcindex);
    let len = c.len();
    let v = c.arg(len - 1);
    let r = c.set_upvalue_of(f, n as i64, v);
    c.truncate(len - 1);
    Ok(r)
}

/// `lua_upvalueid`: an identity for upvalue `n` of the function at
/// `funcindex`, or 0.
pub(crate) fn upvalueid(c: &NativeCtx<'_>, funcindex: i32, n: i32) -> usize {
    match c.upvalue_cell(get(c, funcindex), n as i64) {
        Some((_, cell)) => cell.to_bits() as usize | 1 << 63,
        None => 0,
    }
}

pub(crate) fn upvaluejoin(c: &mut NativeCtx<'_>, f1: i32, n1: i32, f2: i32, n2: i32) -> Res<()> {
    let (a, b) = (get(c, f1), get(c, f2));
    let (Value::Closure(x), Value::Closure(y)) = (a, b) else {
        return Err(c.error("C API: Lua function expected"));
    };
    if c.upvalue_cell(a, n1 as i64).is_none() || c.upvalue_cell(b, n2 as i64).is_none() {
        return Err(c.error("C API: invalid upvalue index"));
    }
    c.join_upvalues(x, n1 as usize, y, n2 as usize);
    Ok(())
}
