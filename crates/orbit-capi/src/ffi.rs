//! The crate's only `unsafe` (ADR-6 exception, Tym 2026-10-10): the entry
//! points `csrc/capi.c` calls, the call into C, and dynamic loading.
//!
//! Each entry point gets back the running native's context from the state
//! pointer, does its work through the safe functions in [`crate::api`],
//! and reports an error by leaving it pending and setting the state's
//! status, never by unwinding: the C side turns that into a `longjmp` once
//! this function has returned.
#![allow(unsafe_code)]

use std::cell::Cell;
use std::ffi::{CStr, c_char, c_int, c_void};

use orbit_vm::orbit_core::Value;
use orbit_vm::{LuaError, NativeCtx, Vm};

use crate::api::{self, Res, Shared, abi};

/// `struct lua_State` in `csrc/capi.c`.
#[repr(C)]
pub(crate) struct LState {
    rs: *const c_void,
    jb: *mut c_void,
    status: c_int,
}

type CFn = unsafe extern "C" fn(*mut LState) -> c_int;

unsafe extern "C" {
    fn orbit_capi_newstate(rs: *const c_void) -> *mut LState;
    fn orbit_capi_freestate(l: *mut LState);
    fn orbit_capi_trampoline(l: *mut LState, f: CFn) -> c_int;
}

/// What the C side reaches through a state: the running native and the
/// API's state.
#[derive(Debug)]
pub(crate) struct Bridge {
    l: Cell<*mut LState>,
    /// The native running a C function, null between calls.
    ctx: Cell<*mut NativeCtx<'static>>,
    st: Shared,
}

/// Owns a VM's [`Bridge`] and its C state, from [`orbit_vm::Runtime::ext`].
#[derive(Debug)]
struct BridgeHandle(*const Bridge);

impl Drop for BridgeHandle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: made by `install` with `Box::into_raw`; no C function
            // can be running while the runtime is being dropped.
            unsafe {
                let b = Box::from_raw(self.0 as *mut Bridge);
                orbit_capi_freestate(b.l.get());
            }
        }
    }
}

pub(crate) fn install(vm: &mut Vm) {
    let h = vm
        .rt
        .ext
        .get_or_insert_with(|| BridgeHandle(std::ptr::null()));
    if !h.0.is_null() {
        return;
    }
    let b = Box::into_raw(Box::new(Bridge {
        l: Cell::new(std::ptr::null_mut()),
        ctx: Cell::new(std::ptr::null_mut()),
        st: Shared::default(),
    }));
    // SAFETY: plain allocation in C; `b` outlives the state (BridgeHandle).
    let l = unsafe { orbit_capi_newstate(b as *const c_void) };
    assert!(!l.is_null(), "out of memory creating the C API state");
    // SAFETY: `b` is live and not yet shared.
    unsafe { (*b).l.set(l) };
    h.0 = b;
}

/// Call C function `f` with the native's window as its stack.
pub(crate) fn invoke(ctx: &mut NativeCtx<'_>, f: u64) -> Result<usize, LuaError> {
    let p = ctx
        .rt
        .ext
        .get_or_insert_with(|| BridgeHandle(std::ptr::null()))
        .0;
    if p.is_null() {
        return Err(ctx.error("the C API is not installed in this VM"));
    }
    // SAFETY: the handle keeps the bridge alive as long as the runtime.
    let b = unsafe { &*p };
    // The native's context, for the entry points C calls; restored after,
    // so nested C calls each see their own.
    let raw = (ctx as *mut NativeCtx<'_>).cast::<NativeCtx<'static>>();
    let prev = b.ctx.replace(raw);
    let mark = b.st.borrow().strs.len();
    // SAFETY: `f` came from a C module as a `lua_CFunction` (lua_pushcclosure
    // or dlsym); calling it is what loading the module asked for.
    let n = unsafe {
        let cf = std::mem::transmute::<usize, CFn>(f as usize);
        orbit_capi_trampoline(b.l.get(), cf)
    };
    b.ctx.set(prev);
    b.st.borrow_mut().strs.truncate(mark);
    if n < 0 {
        let v = b.st.borrow_mut().pending.take().unwrap_or(Value::Nil);
        return Err(ctx.error_value(v));
    }
    let n = n as usize;
    if n > ctx.len() {
        return Err(ctx.error("C function returned more results than its stack holds"));
    }
    Ok(n)
}

/// The context and bridge of the C function running on state `l`. Aborts
/// if none is (C code kept `L` and used it after its call returned).
///
/// # Safety
/// `l` is a state made by [`install`].
unsafe fn enter<'a>(l: *mut LState) -> (&'a Bridge, &'a mut NativeCtx<'static>) {
    // SAFETY: per the contract; the bridge lives as long as the state.
    let b = unsafe { &*((*l).rs as *const Bridge) };
    let c = b.ctx.get();
    if c.is_null() {
        eprintln!("OrbitScript: Lua C API called outside a C function call");
        std::process::abort();
    }
    // SAFETY: set by `invoke` from a live `&mut NativeCtx` that is not used
    // again until the C function returns.
    (b, unsafe { &mut *c })
}

fn fail(l: *mut LState, b: &Bridge, e: LuaError) {
    b.st.borrow_mut().pending = Some(e.value);
    // SAFETY: `l` is the live state the entry point was called with.
    unsafe { (*l).status = 1 };
}

/// Run `body` as an entry point; on error, leave it pending and return
/// `default`.
macro_rules! entry {
    ($l:expr, $default:expr, |$b:ident, $c:ident| $body:expr) => {{
        // SAFETY: `$l` came from C as the state of a running call.
        let ($b, $c) = unsafe { enter($l) };
        #[allow(clippy::redundant_closure_call)]
        let r: Res<_> = (|| $body)();
        match r {
            Ok(v) => v,
            Err(e) => {
                fail($l, $b, e);
                $default
            }
        }
    }};
}

/// A NUL-terminated copy of `bytes` that stays valid until the running C
/// function returns.
fn hand_out(st: &Shared, bytes: &[u8], len: *mut usize) -> *const c_char {
    let mut copy = Vec::with_capacity(bytes.len() + 1);
    copy.extend_from_slice(bytes);
    copy.push(0);
    let copy = copy.into_boxed_slice();
    let p = copy.as_ptr() as *const c_char;
    st.borrow_mut().strs.push(copy);
    if !len.is_null() {
        // SAFETY: C passed a pointer to a size_t, or NULL.
        unsafe { *len = bytes.len() };
    }
    p
}

/// # Safety
/// `p` is null or a NUL-terminated string.
unsafe fn cbytes<'a>(p: *const c_char) -> &'a [u8] {
    if p.is_null() {
        b""
    } else {
        // SAFETY: per the contract.
        unsafe { CStr::from_ptr(p) }.to_bytes()
    }
}

fn set_flag(p: *mut c_int, ok: bool) {
    if !p.is_null() {
        // SAFETY: C passed a pointer to an int, or NULL.
        unsafe { *p = ok as c_int };
    }
}

// ---- entry points (names match csrc/capi.c) ---------------------------------

#[unsafe(no_mangle)]
extern "C" fn orbit_r_version(l: *mut LState) -> f64 {
    entry!(l, 0.0, |_b, _c| Ok(505.0))
}

#[unsafe(no_mangle)]
extern "C" fn orbit_r_absindex(l: *mut LState, idx: c_int) -> c_int {
    entry!(l, 0, |_b, c| Ok(api::absindex(c, idx)))
}

#[unsafe(no_mangle)]
extern "C" fn orbit_r_gettop(l: *mut LState) -> c_int {
    entry!(l, 0, |_b, c| Ok(api::gettop(c)))
}

#[unsafe(no_mangle)]
extern "C" fn orbit_r_settop(l: *mut LState, idx: c_int) {
    entry!(l, (), |_b, c| api::settop(c, idx))
}

#[unsafe(no_mangle)]
extern "C" fn orbit_r_pushvalue(l: *mut LState, idx: c_int) {
    entry!(l, (), |_b, c| {
        api::pushvalue(c, idx);
        Ok(())
    })
}

#[unsafe(no_mangle)]
extern "C" fn orbit_r_rotate(l: *mut LState, idx: c_int, n: c_int) {
    entry!(l, (), |_b, c| api::rotate(c, idx, n))
}

#[unsafe(no_mangle)]
extern "C" fn orbit_r_copy(l: *mut LState, from: c_int, to: c_int) {
    entry!(l, (), |_b, c| api::copy(c, from, to))
}

#[unsafe(no_mangle)]
extern "C" fn orbit_r_checkstack(l: *mut LState, _n: c_int) -> c_int {
    entry!(l, 0, |_b, _c| Ok(1))
}

#[unsafe(no_mangle)]
extern "C" fn orbit_r_isnumber(l: *mut LState, idx: c_int) -> c_int {
    entry!(l, 0, |_b, c| Ok(api::tonumber(c, idx).is_some() as c_int))
}

#[unsafe(no_mangle)]
extern "C" fn orbit_r_isstring(l: *mut LState, idx: c_int) -> c_int {
    entry!(l, 0, |_b, c| Ok(api::isstring(c, idx) as c_int))
}

#[unsafe(no_mangle)]
extern "C" fn orbit_r_iscfunction(l: *mut LState, idx: c_int) -> c_int {
    entry!(l, 0, |_b, c| Ok(api::iscfunction(c, idx) as c_int))
}

#[unsafe(no_mangle)]
extern "C" fn orbit_r_isinteger(l: *mut LState, idx: c_int) -> c_int {
    entry!(l, 0, |_b, c| Ok(api::isinteger(c, idx) as c_int))
}

#[unsafe(no_mangle)]
extern "C" fn orbit_r_isuserdata(l: *mut LState, idx: c_int) -> c_int {
    entry!(l, 0, |_b, c| Ok(api::isuserdata(c, idx) as c_int))
}

#[unsafe(no_mangle)]
extern "C" fn orbit_r_type(l: *mut LState, idx: c_int) -> c_int {
    entry!(l, 0, |_b, c| Ok(api::type_(c, idx)))
}

#[unsafe(no_mangle)]
extern "C" fn orbit_r_tonumberx(l: *mut LState, idx: c_int, isnum: *mut c_int) -> f64 {
    entry!(l, 0.0, |_b, c| {
        let n = api::tonumber(c, idx);
        set_flag(isnum, n.is_some());
        Ok(n.unwrap_or(0.0))
    })
}

#[unsafe(no_mangle)]
extern "C" fn orbit_r_tointegerx(l: *mut LState, idx: c_int, isnum: *mut c_int) -> i64 {
    entry!(l, 0, |_b, c| {
        let n = api::tointeger(c, idx);
        set_flag(isnum, n.is_some());
        Ok(n.unwrap_or(0))
    })
}

#[unsafe(no_mangle)]
extern "C" fn orbit_r_toboolean(l: *mut LState, idx: c_int) -> c_int {
    entry!(l, 0, |_b, c| Ok(api::toboolean(c, idx) as c_int))
}

#[unsafe(no_mangle)]
extern "C" fn orbit_r_tolstring(l: *mut LState, idx: c_int, len: *mut usize) -> *const c_char {
    entry!(l, std::ptr::null(), |b, c| {
        Ok(match api::tolstring(c, idx)? {
            Some(s) => hand_out(&b.st, c.bytes(s), len),
            None => {
                if !len.is_null() {
                    // SAFETY: C passed a pointer to a size_t.
                    unsafe { *len = 0 };
                }
                std::ptr::null()
            }
        })
    })
}

#[unsafe(no_mangle)]
extern "C" fn orbit_r_rawlen(l: *mut LState, idx: c_int) -> u64 {
    entry!(l, 0, |_b, c| Ok(api::rawlen(c, idx)))
}

#[unsafe(no_mangle)]
extern "C" fn orbit_r_tocfunction(l: *mut LState, _idx: c_int) -> Option<CFn> {
    // Spike: C functions are closures; not mapped back to pointers yet.
    entry!(l, None, |_b, _c| Ok(None))
}

#[unsafe(no_mangle)]
extern "C" fn orbit_r_touserdata(l: *mut LState, idx: c_int) -> *mut c_void {
    entry!(l, std::ptr::null_mut(), |_b, c| Ok(api::touserdata(c, idx)))
}

#[unsafe(no_mangle)]
extern "C" fn orbit_r_topointer(l: *mut LState, idx: c_int) -> *const c_void {
    entry!(l, std::ptr::null(), |_b, c| Ok(
        api::topointer(c, idx) as *const c_void
    ))
}

#[unsafe(no_mangle)]
extern "C" fn orbit_r_rawequal(l: *mut LState, a: c_int, b_: c_int) -> c_int {
    entry!(l, 0, |_b, c| Ok(api::rawequal(c, a, b_) as c_int))
}

#[unsafe(no_mangle)]
extern "C" fn orbit_r_compare(l: *mut LState, a: c_int, b_: c_int, op: c_int) -> c_int {
    entry!(l, 0, |b, c| api::compare(c, &b.st, a, b_, op)
        .map(|r| r as c_int))
}

#[unsafe(no_mangle)]
extern "C" fn orbit_r_pushnil(l: *mut LState) {
    entry!(l, (), |_b, c| {
        c.push(Value::Nil);
        Ok(())
    })
}

#[unsafe(no_mangle)]
extern "C" fn orbit_r_pushnumber(l: *mut LState, n: f64) {
    entry!(l, (), |_b, c| {
        c.push(Value::Float(n));
        Ok(())
    })
}

#[unsafe(no_mangle)]
extern "C" fn orbit_r_pushinteger(l: *mut LState, n: i64) {
    entry!(l, (), |_b, c| {
        c.push(Value::Int(n));
        Ok(())
    })
}

#[unsafe(no_mangle)]
extern "C" fn orbit_r_pushlstring(l: *mut LState, s: *const c_char, len: usize) -> *const c_char {
    entry!(l, std::ptr::null(), |b, c| {
        let bytes: &[u8] = if len == 0 {
            b""
        } else {
            // SAFETY: C passed `len` readable bytes at `s`.
            unsafe { std::slice::from_raw_parts(s as *const u8, len) }
        };
        let st = api::pushstr(c, bytes);
        Ok(hand_out(&b.st, c.bytes(st), std::ptr::null_mut()))
    })
}

#[unsafe(no_mangle)]
extern "C" fn orbit_r_pushcclosure(l: *mut LState, f: Option<CFn>, n: c_int) {
    entry!(l, (), |_b, c| match f {
        Some(f) => api::pushcclosure(c, f as usize as u64, n),
        None => Err(c.error("C API: null C function")),
    })
}

#[unsafe(no_mangle)]
extern "C" fn orbit_r_pushboolean(l: *mut LState, v: c_int) {
    entry!(l, (), |_b, c| {
        c.push(Value::Bool(v != 0));
        Ok(())
    })
}

#[unsafe(no_mangle)]
extern "C" fn orbit_r_pushlightuserdata(l: *mut LState, p: *mut c_void) {
    entry!(l, (), |_b, c| {
        c.push(api::light(p as usize));
        Ok(())
    })
}

#[unsafe(no_mangle)]
extern "C" fn orbit_r_getglobal(l: *mut LState, name: *const c_char) -> c_int {
    // SAFETY: C passes a NUL-terminated name.
    let name = unsafe { cbytes(name) };
    entry!(l, 0, |b, c| api::getglobal(c, &b.st, name))
}

#[unsafe(no_mangle)]
extern "C" fn orbit_r_gettable(l: *mut LState, idx: c_int) -> c_int {
    entry!(l, 0, |b, c| api::gettable(c, &b.st, idx))
}

#[unsafe(no_mangle)]
extern "C" fn orbit_r_getfield(l: *mut LState, idx: c_int, k: *const c_char) -> c_int {
    // SAFETY: C passes a NUL-terminated key.
    let k = unsafe { cbytes(k) };
    entry!(l, 0, |b, c| {
        let obj = api_get(c, idx);
        let key = c.new_str(k);
        api::gettable_with(c, &b.st, obj, key)
    })
}

#[unsafe(no_mangle)]
extern "C" fn orbit_r_geti(l: *mut LState, idx: c_int, n: i64) -> c_int {
    entry!(l, 0, |b, c| {
        let obj = api_get(c, idx);
        api::gettable_with(c, &b.st, obj, Value::Int(n))
    })
}

#[unsafe(no_mangle)]
extern "C" fn orbit_r_rawget(l: *mut LState, idx: c_int) -> c_int {
    entry!(l, 0, |_b, c| api::rawget(c, idx))
}

#[unsafe(no_mangle)]
extern "C" fn orbit_r_rawgeti(l: *mut LState, idx: c_int, n: i64) -> c_int {
    entry!(l, 0, |_b, c| api::rawget_with(c, idx, Value::Int(n)))
}

#[unsafe(no_mangle)]
extern "C" fn orbit_r_rawgetp(l: *mut LState, idx: c_int, p: *const c_void) -> c_int {
    entry!(l, 0, |_b, c| api::rawget_with(
        c,
        idx,
        api::light(p as usize)
    ))
}

#[unsafe(no_mangle)]
extern "C" fn orbit_r_createtable(l: *mut LState, narr: c_int, nrec: c_int) {
    entry!(l, (), |_b, c| {
        api::createtable(c, narr, nrec);
        Ok(())
    })
}

#[unsafe(no_mangle)]
extern "C" fn orbit_r_newuserdatauv(l: *mut LState, size: usize, _nuv: c_int) -> *mut c_void {
    entry!(l, std::ptr::null_mut(), |_b, c| api::newuserdata(c, size))
}

#[unsafe(no_mangle)]
extern "C" fn orbit_r_getmetatable(l: *mut LState, idx: c_int) -> c_int {
    entry!(l, 0, |_b, c| Ok(api::getmetatable(c, idx) as c_int))
}

#[unsafe(no_mangle)]
extern "C" fn orbit_r_getiuservalue(l: *mut LState, _idx: c_int, _n: c_int) -> c_int {
    // Spike: user values aren't mapped yet.
    entry!(l, abi::TNONE, |_b, c| {
        c.push(Value::Nil);
        Ok(abi::TNONE)
    })
}

#[unsafe(no_mangle)]
extern "C" fn orbit_r_setglobal(l: *mut LState, name: *const c_char) {
    // SAFETY: C passes a NUL-terminated name.
    let name = unsafe { cbytes(name) };
    entry!(l, (), |b, c| api::setglobal(c, &b.st, name))
}

#[unsafe(no_mangle)]
extern "C" fn orbit_r_settable(l: *mut LState, idx: c_int) {
    entry!(l, (), |b, c| api::settable(c, &b.st, idx))
}

#[unsafe(no_mangle)]
extern "C" fn orbit_r_setfield(l: *mut LState, idx: c_int, k: *const c_char) {
    // SAFETY: C passes a NUL-terminated key.
    let k = unsafe { cbytes(k) };
    entry!(l, (), |b, c| api::setfield(c, &b.st, idx, k))
}

#[unsafe(no_mangle)]
extern "C" fn orbit_r_seti(l: *mut LState, idx: c_int, n: i64) {
    entry!(l, (), |b, c| {
        let obj = api_get(c, idx);
        api::set_popping(c, &b.st, obj, Value::Int(n))
    })
}

#[unsafe(no_mangle)]
extern "C" fn orbit_r_rawset(l: *mut LState, idx: c_int) {
    entry!(l, (), |_b, c| api::rawset(c, idx))
}

#[unsafe(no_mangle)]
extern "C" fn orbit_r_rawseti(l: *mut LState, idx: c_int, n: i64) {
    entry!(l, (), |_b, c| api::rawset_with(c, idx, Value::Int(n)))
}

#[unsafe(no_mangle)]
extern "C" fn orbit_r_rawsetp(l: *mut LState, idx: c_int, p: *const c_void) {
    entry!(l, (), |_b, c| api::rawset_with(
        c,
        idx,
        api::light(p as usize)
    ))
}

#[unsafe(no_mangle)]
extern "C" fn orbit_r_setmetatable(l: *mut LState, idx: c_int) -> c_int {
    entry!(l, 0, |_b, c| api::setmetatable(c, idx).map(|()| 1))
}

#[unsafe(no_mangle)]
extern "C" fn orbit_r_setiuservalue(l: *mut LState, _idx: c_int, _n: c_int) -> c_int {
    entry!(l, 0, |_b, c| {
        api::settop(c, -2)?;
        Ok(0)
    })
}

#[unsafe(no_mangle)]
extern "C" fn orbit_r_callk(
    l: *mut LState,
    nargs: c_int,
    nresults: c_int,
    _ctx: isize,
    _k: *const c_void,
) {
    entry!(l, (), |b, c| api::call(c, &b.st, nargs, nresults))
}

#[unsafe(no_mangle)]
extern "C" fn orbit_r_pcallk(
    l: *mut LState,
    nargs: c_int,
    nresults: c_int,
    errfunc: c_int,
    _ctx: isize,
    _k: *const c_void,
) -> c_int {
    entry!(l, abi::ERRRUN, |b, c| api::pcall(
        c, &b.st, nargs, nresults, errfunc
    ))
}

#[unsafe(no_mangle)]
extern "C" fn orbit_r_error(l: *mut LState) -> c_int {
    entry!(l, 0, |_b, c| Err::<c_int, _>(api::error(c)))
}

#[unsafe(no_mangle)]
extern "C" fn orbit_r_next(l: *mut LState, idx: c_int) -> c_int {
    entry!(l, 0, |_b, c| api::next(c, idx).map(|r| r as c_int))
}

#[unsafe(no_mangle)]
extern "C" fn orbit_r_concat(l: *mut LState, n: c_int) {
    entry!(l, (), |_b, c| api::concat(c, n))
}

#[unsafe(no_mangle)]
extern "C" fn orbit_r_len(l: *mut LState, idx: c_int) {
    entry!(l, (), |b, c| api::len(c, &b.st, idx))
}

#[unsafe(no_mangle)]
extern "C" fn orbit_r_stringtonumber(l: *mut LState, s: *const c_char) -> usize {
    // SAFETY: C passes a NUL-terminated string.
    let s = unsafe { cbytes(s) };
    entry!(l, 0, |_b, c| Ok(api::stringtonumber(c, s)))
}

#[unsafe(no_mangle)]
extern "C" fn orbit_r_gc(l: *mut LState, what: c_int, _arg: c_int) -> c_int {
    entry!(l, 0, |_b, c| Ok(api::gc(c, what)))
}

#[unsafe(no_mangle)]
extern "C" fn orbit_r_argerror(l: *mut LState, arg: c_int, msg: *const c_char) {
    // SAFETY: C passes a NUL-terminated message, or NULL.
    let msg = String::from_utf8_lossy(unsafe { cbytes(msg) }).into_owned();
    entry!(l, (), |_b, c| Err(api::argerror(c, arg, &msg)))
}

#[unsafe(no_mangle)]
extern "C" fn orbit_r_typeerror(l: *mut LState, arg: c_int, tname: *const c_char) {
    // SAFETY: C passes a NUL-terminated type name.
    let tname = String::from_utf8_lossy(unsafe { cbytes(tname) }).into_owned();
    entry!(l, (), |_b, c| Err(api::typeerror(c, arg, &tname)))
}

#[unsafe(no_mangle)]
extern "C" fn orbit_r_where(l: *mut LState, lvl: c_int) {
    entry!(l, (), |_b, c| {
        api::where_(c, lvl);
        Ok(())
    })
}

#[unsafe(no_mangle)]
extern "C" fn orbit_r_tolstringmeta(l: *mut LState, idx: c_int, len: *mut usize) -> *const c_char {
    entry!(l, std::ptr::null(), |b, c| {
        let s = api::tolstring_meta(c, &b.st, idx)?;
        Ok(hand_out(&b.st, c.bytes(s), len))
    })
}

fn api_get(c: &NativeCtx<'_>, idx: c_int) -> Value {
    // `lua_getfield`/`geti`/`seti` read their object before pushing.
    let len = c.len() as i32;
    if idx == abi::REGISTRYINDEX {
        Value::Table(c.rt.registry)
    } else if idx < abi::REGISTRYINDEX {
        c.upvalue((abi::REGISTRYINDEX - idx) as usize)
    } else if idx > 0 {
        if idx <= len {
            c.arg(idx as usize - 1)
        } else {
            Value::Nil
        }
    } else if idx < 0 && len + idx >= 0 {
        c.arg((len + idx) as usize)
    } else {
        Value::Nil
    }
}

// ---- dynamic loading ----------------------------------------------------------

#[cfg(unix)]
mod dl {
    use std::ffi::{c_char, c_int, c_void};

    pub const RTLD_NOW: c_int = 2;
    #[cfg(target_os = "linux")]
    pub const RTLD_GLOBAL: c_int = 0x100;
    #[cfg(not(target_os = "linux"))]
    pub const RTLD_GLOBAL: c_int = 0x8;
    pub const RTLD_LOCAL: c_int = 0;

    unsafe extern "C" {
        pub fn dlopen(filename: *const c_char, flag: c_int) -> *mut c_void;
        pub fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
        pub fn dlerror() -> *const c_char;
    }
}

#[cfg(unix)]
fn dl_error() -> String {
    // SAFETY: dlerror returns null or a NUL-terminated message.
    let p = unsafe { dl::dlerror() };
    // SAFETY: as above.
    String::from_utf8_lossy(unsafe { cbytes(p) }).into_owned()
}

/// The [`orbit_stdlib::LoadFn`] for this crate. Libraries stay loaded
/// until the process exits, as their functions may be referenced anywhere.
#[cfg(unix)]
pub(crate) fn load(
    ctx: &mut NativeCtx<'_>,
    path: &[u8],
    sym: &[u8],
) -> Result<Value, (String, &'static str)> {
    let cpath =
        std::ffi::CString::new(path).map_err(|_| ("path contains a NUL byte".into(), "open"))?;
    let global = sym == b"*";
    let flags = dl::RTLD_NOW
        | if global {
            dl::RTLD_GLOBAL
        } else {
            dl::RTLD_LOCAL
        };
    // SAFETY: loading the library runs its initializers; the host enabled
    // C modules, which is consent to run native code from cpath.
    let h = unsafe { dl::dlopen(cpath.as_ptr(), flags) };
    if h.is_null() {
        return Err((dl_error(), "open"));
    }
    if global {
        return Ok(Value::Bool(true));
    }
    let csym =
        std::ffi::CString::new(sym).map_err(|_| ("symbol contains a NUL byte".into(), "init"))?;
    // SAFETY: `h` is a live handle; the name is NUL-terminated.
    let f = unsafe { dl::dlsym(h, csym.as_ptr()) };
    if f.is_null() {
        return Err((dl_error(), "init"));
    }
    Ok(crate::c_function(ctx, f as usize as u64, &[]))
}

#[cfg(not(unix))]
pub(crate) fn load(
    _ctx: &mut NativeCtx<'_>,
    _path: &[u8],
    _sym: &[u8],
) -> Result<Value, (String, &'static str)> {
    Err((
        "dynamic libraries not supported on this platform yet".into(),
        "absent",
    ))
}
