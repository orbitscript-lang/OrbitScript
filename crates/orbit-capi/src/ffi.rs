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

use crate::api::{self, CStrCopy, Res, Shared, abi};

/// `struct lua_State` in `csrc/capi.c`.
#[repr(C)]
pub(crate) struct LState {
    rs: *const c_void,
    jb: *mut c_void,
    status: c_int,
    /// Set by `lua_sethook` (atomically: it may run in a signal handler).
    hookchanged: c_int,
}

type CFn = unsafe extern "C" fn(*mut LState) -> c_int;

unsafe extern "C" {
    fn orbit_capi_newstate(rs: *const c_void, table: *const RTable) -> *mut LState;
    fn orbit_capi_table_size() -> usize;
    fn orbit_capi_freestate(l: *mut LState);
    fn orbit_capi_trampoline(l: *mut LState, f: CFn) -> c_int;
    fn orbit_capi_set_wake(l: *mut LState, wake: *mut u8);
    fn orbit_capi_take_hook(
        l: *mut LState,
        f: *mut Option<HookFn>,
        mask: *mut c_int,
        count: *mut c_int,
    ) -> c_int;
    fn orbit_capi_call_hook(
        l: *mut LState,
        f: HookFn,
        event: c_int,
        line: c_int,
        level: usize,
    ) -> c_int;
}

/// `lua_Hook`.
type HookFn = unsafe extern "C" fn(*mut LState, *mut c_void);

/// What the C side reaches through a state: the running native and the
/// API's state.
#[derive(Debug)]
pub(crate) struct Bridge {
    l: Cell<*mut LState>,
    /// The native running a C function, null between calls.
    ctx: Cell<*mut NativeCtx<'static>>,
    st: Shared,
    /// In a state C created (ADR-10), the host's own context, which
    /// [`Bridge::ctx`] holds between calls; null otherwise.
    host: Cell<*mut NativeCtx<'static>>,
    /// The native standing for each C hook function installed so far.
    hooks: std::cell::RefCell<Vec<(usize, Value)>>,
    /// While a C hook runs, its native's context: there, as in Lua, level 0
    /// is the hooked function, one below the native's own.
    hook_ctx: Cell<*mut NativeCtx<'static>>,
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
    let wake = vm.interrupt_flag().cast::<u8>();
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
        host: Cell::new(std::ptr::null_mut()),
        hooks: Default::default(),
        hook_ctx: Cell::new(std::ptr::null_mut()),
    }));
    // SAFETY: no arguments; the size of C's `struct orbit_r_table`.
    let c_size = unsafe { orbit_capi_table_size() };
    assert_eq!(
        c_size,
        size_of::<RTable>(),
        "csrc/capi.c and RTable disagree"
    );
    // SAFETY: plain allocation in C; `b` outlives the state (BridgeHandle),
    // and TABLE is static.
    let l = unsafe { orbit_capi_newstate(b as *const c_void, &TABLE) };
    assert!(!l.is_null(), "out of memory creating the C API state");
    // SAFETY: `b` is live and not yet shared. The interrupt flag lives as
    // long as the VM, which outlives the state.
    unsafe {
        (*b).l.set(l);
        orbit_capi_set_wake(l, wake);
    }
    h.0 = b;
    vm.rt.interrupt_handler = Some(on_interrupt);
    // Modules read the main thread from the registry, as in Lua.
    let (reg, main) = (vm.rt.registry, vm.main_thread());
    vm.table_set(reg, Value::Int(abi::RIDX_MAINTHREAD), Value::Thread(main));
}

/// The VM's bridge, if the C API is installed.
fn bridge<'a>(vm: &Vm) -> Option<&'a Bridge> {
    match vm.rt.ext.get::<BridgeHandle>() {
        // SAFETY: the handle keeps the bridge alive as long as the runtime.
        Some(h) if !h.0.is_null() => Some(unsafe { &*h.0 }),
        _ => None,
    }
}

/// Run `call` on the C state with the native's context current, as a C
/// function's call does; an error it raised comes back as `Err`.
fn run_c(
    ctx: &mut NativeCtx<'_>,
    call: impl FnOnce(*mut LState) -> c_int,
) -> Result<c_int, LuaError> {
    let Some(b) = bridge(ctx.vm) else {
        return Err(ctx.error("the C API is not installed in this VM"));
    };
    // The native's context, for the entry points C calls; restored after,
    // so nested C calls each see their own.
    let raw = (ctx as *mut NativeCtx<'_>).cast::<NativeCtx<'static>>();
    let prev = b.ctx.replace(raw);
    let mark = b.st.borrow().strs.len();
    let n = call(b.l.get());
    b.ctx.set(prev);
    b.st.borrow_mut().strs.truncate(mark);
    take_hook(b, ctx.vm, true);
    if n < 0 {
        let v = b.st.borrow_mut().pending.take().unwrap_or(Value::Nil);
        return Err(ctx.error_value(v));
    }
    Ok(n)
}

/// Call C function `f` with the native's window as its stack.
pub(crate) fn invoke(ctx: &mut NativeCtx<'_>, f: u64) -> Result<usize, LuaError> {
    let n = run_c(ctx, |l| {
        // SAFETY: `f` came from a C module as a `lua_CFunction`
        // (lua_pushcclosure or dlsym); calling it is what loading the
        // module asked for.
        unsafe {
            let cf = std::mem::transmute::<usize, CFn>(f as usize);
            orbit_capi_trampoline(l, cf)
        }
    })?;
    let n = n as usize;
    if n > ctx.len() {
        return Err(ctx.error("C function returned more results than its stack holds"));
    }
    Ok(n)
}

/// Install the hook `lua_sethook` asked for, if it asked since the last
/// look. `clear` lowers the interrupt flag `lua_sethook` raised, which the
/// interrupt handler has already lowered. Returns whether it asked.
fn take_hook(b: &Bridge, vm: &mut Vm, clear: bool) -> bool {
    let l = b.l.get();
    // SAFETY: `l` is the bridge's live state; the field is only ever
    // stored to atomically, and a stale read just delays the hook.
    if unsafe { std::ptr::read_volatile(&(*l).hookchanged) } == 0 {
        return false;
    }
    let (mut f, mut mask, mut count) = (None, 0, 0);
    // SAFETY: the out-pointers are live locals.
    if unsafe { orbit_capi_take_hook(l, &mut f, &mut mask, &mut count) } == 0 {
        return false;
    }
    if clear {
        vm.clear_interrupt();
    }
    let func = match f {
        None => Value::Nil,
        Some(f) => {
            let key = f as usize;
            let mut hooks = b.hooks.borrow_mut();
            match hooks.iter().find(|(k, _)| *k == key) {
                Some(&(_, v)) => v,
                None => {
                    let v = vm.register_native_with_data("hook", c_hook, [key as u64, 0]);
                    hooks.push((key, v));
                    v
                }
            }
        }
    };
    // Lua's mask bits are the VM's (call 1, return 2, line 4, count 8).
    vm.set_hook(func, (mask & 0xf) as u8, count.into());
    true
}

/// The VM's interrupt handler: a `lua_sethook` from a signal handler
/// raised the flag.
fn on_interrupt(vm: &mut Vm) -> bool {
    match bridge(vm) {
        Some(b) => take_hook(b, vm, false),
        None => false,
    }
}

/// A level as C gives it, as the VM counts it: in a C hook, as in Lua,
/// level 0 is the hooked function, one below the hook's own native.
fn lua_level(b: &Bridge, level: c_int) -> c_int {
    let in_hook = !b.hook_ctx.get().is_null() && std::ptr::eq(b.ctx.get(), b.hook_ctx.get());
    if in_hook && level >= 0 {
        level + 1
    } else {
        level
    }
}

/// The native standing for a C hook (its function pointer in the data
/// word), called as a Lua hook is: `(event, line)`.
fn c_hook(ctx: &mut NativeCtx<'_>) -> Result<orbit_vm::NativeReturn, LuaError> {
    let f = ctx.native_data()[0] as usize;
    let event = match ctx.arg(0) {
        Value::Str(s) => match ctx.bytes(s) {
            b"call" => abi::HOOKCALL,
            b"return" => abi::HOOKRET,
            b"line" => abi::HOOKLINE,
            b"count" => abi::HOOKCOUNT,
            b"tail call" => abi::HOOKTAILCALL,
            _ => return Ok(orbit_vm::NativeReturn::Return(0)),
        },
        _ => return Ok(orbit_vm::NativeReturn::Return(0)),
    };
    let line = match ctx.arg(1) {
        Value::Int(i) => i as c_int,
        _ => -1,
    };
    let raw = (ctx as *mut NativeCtx<'_>).cast::<NativeCtx<'static>>();
    let prev = bridge(ctx.vm).map(|b| b.hook_ctx.replace(raw));
    let r = run_c(ctx, |l| {
        // SAFETY: `f` came from C as a `lua_Hook` (lua_sethook). From the
        // hook's native, level 1 is the hooked function; lua_getinfo takes
        // the level plus one.
        unsafe {
            let hf = std::mem::transmute::<usize, HookFn>(f);
            orbit_capi_call_hook(l, hf, event, line, 2)
        }
    });
    if let (Some(b), Some(p)) = (bridge(ctx.vm), prev) {
        b.hook_ctx.set(p);
    }
    r?;
    Ok(orbit_vm::NativeReturn::Return(0))
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
        take_hook($b, $c.vm, true);
        #[allow(clippy::redundant_closure_call)]
        let r: Res<_> = (|| $body)();
        host_prune($b, $c);
        match r {
            Ok(v) => v,
            Err(e) => {
                fail($l, $b, e);
                $default
            }
        }
    }};
}

/// Between calls in a state C created: let the collector run, and drop
/// string copies no string on the host's stack needs any more
/// ([`api::State::prune`]).
fn host_prune(b: &Bridge, c: &mut NativeCtx<'static>) {
    let host = b.host.get();
    if host.is_null() || !std::ptr::eq(b.ctx.get(), host) {
        return;
    }
    c.vm.host_safepoint();
    {
        let st = b.st.borrow();
        if st.strs.len() < 2 * st.kept + 16 {
            return;
        }
    }
    let texts: Vec<&[u8]> = (0..c.len())
        .filter_map(|i| match c.arg(i) {
            Value::Str(s) => Some(c.bytes(s)),
            _ => None,
        })
        .collect();
    b.st.borrow_mut().prune(&texts);
}

/// A NUL-terminated copy of `bytes` that stays valid until the running C
/// function returns.
fn hand_out(st: &Shared, bytes: &[u8], len: *mut usize) -> *const c_char {
    hand_out_copy(st, bytes, len, false)
}

/// [`hand_out`] for a string value C asked for (`lua_tolstring`): between
/// calls in a state C created, the copy lives while an equal string is on
/// the host's stack, and asking again for an equal string reuses a recent
/// copy.
fn hand_out_value(st: &Shared, bytes: &[u8], len: *mut usize) -> *const c_char {
    hand_out_copy(st, bytes, len, true)
}

fn hand_out_copy(st: &Shared, bytes: &[u8], len: *mut usize, value: bool) -> *const c_char {
    let mut state = st.borrow_mut();
    let recent = state.strs.iter().rev().take(8).find(|s| {
        s.value && value && s.bytes.len() == bytes.len() + 1 && &s.bytes[..bytes.len()] == bytes
    });
    let p = match recent {
        Some(s) => s.bytes.as_ptr() as *const c_char,
        None => {
            let mut copy = Vec::with_capacity(bytes.len() + 1);
            copy.extend_from_slice(bytes);
            copy.push(0);
            let copy = copy.into_boxed_slice();
            let p = copy.as_ptr() as *const c_char;
            state.strs.push(CStrCopy { bytes: copy, value });
            p
        }
    };
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

/// The Rust entry points `csrc/capi.c` calls, handed to it once
/// ([`install`]) so none of them is an exported symbol. Fields are in name
/// order and match `struct orbit_r_table` there.
#[repr(C)]
struct RTable {
    absindex: extern "C" fn(*mut LState, c_int) -> c_int,
    argerror: extern "C" fn(*mut LState, c_int, *const c_char),
    arith: extern "C" fn(*mut LState, c_int),
    callk: extern "C" fn(*mut LState, c_int, c_int, isize, *const c_void),
    checkstack: extern "C" fn(*mut LState, c_int) -> c_int,
    close: extern "C" fn(*mut LState),
    closeslot: extern "C" fn(*mut LState, c_int),
    compare: extern "C" fn(*mut LState, c_int, c_int, c_int) -> c_int,
    concat: extern "C" fn(*mut LState, c_int),
    copy: extern "C" fn(*mut LState, c_int, c_int),
    createtable: extern "C" fn(*mut LState, c_int, c_int),
    error: extern "C" fn(*mut LState) -> c_int,
    gc: extern "C" fn(*mut LState, c_int, c_int, c_int) -> c_int,
    getfield: extern "C" fn(*mut LState, c_int, *const c_char) -> c_int,
    getglobal: extern "C" fn(*mut LState, *const c_char) -> c_int,
    geti: extern "C" fn(*mut LState, c_int, i64) -> c_int,
    getinfo: extern "C" fn(*mut LState, *const c_char, *mut LuaDebug) -> c_int,
    getiuservalue: extern "C" fn(*mut LState, c_int, c_int) -> c_int,
    getlocal: extern "C" fn(*mut LState, *const LuaDebug, c_int) -> *const c_char,
    getmetatable: extern "C" fn(*mut LState, c_int) -> c_int,
    getstack: extern "C" fn(*mut LState, c_int, *mut LuaDebug) -> c_int,
    gettable: extern "C" fn(*mut LState, c_int) -> c_int,
    gettop: extern "C" fn(*mut LState) -> c_int,
    getupvalue: extern "C" fn(*mut LState, c_int, c_int) -> *const c_char,
    iscfunction: extern "C" fn(*mut LState, c_int) -> c_int,
    isinteger: extern "C" fn(*mut LState, c_int) -> c_int,
    isnumber: extern "C" fn(*mut LState, c_int) -> c_int,
    isrunning: extern "C" fn(*mut LState, c_int) -> c_int,
    isrustudata: extern "C" fn(*mut LState, c_int) -> c_int,
    isstring: extern "C" fn(*mut LState, c_int) -> c_int,
    isuserdata: extern "C" fn(*mut LState, c_int) -> c_int,
    len: extern "C" fn(*mut LState, c_int),
    load: extern "C" fn(*mut LState, *const c_char, usize, *const c_char, *const c_char) -> c_int,
    newuserdatauv: extern "C" fn(*mut LState, usize, c_int) -> *mut c_void,
    next: extern "C" fn(*mut LState, c_int) -> c_int,
    numbertocstring: extern "C" fn(*mut LState, c_int, *mut c_char) -> u32,
    openlib: extern "C" fn(*mut LState, c_int) -> c_int,
    pcallk: extern "C" fn(*mut LState, c_int, c_int, c_int, isize, *const c_void) -> c_int,
    pushboolean: extern "C" fn(*mut LState, c_int),
    pushcclosure: extern "C" fn(*mut LState, Option<CFn>, c_int),
    pushinteger: extern "C" fn(*mut LState, i64),
    pushlightuserdata: extern "C" fn(*mut LState, *mut c_void),
    pushlstring: extern "C" fn(*mut LState, *const c_char, usize) -> *const c_char,
    pushnil: extern "C" fn(*mut LState),
    pushnumber: extern "C" fn(*mut LState, f64),
    pushpending: extern "C" fn(*mut LState),
    pushthread: extern "C" fn(*mut LState) -> c_int,
    pushvalue: extern "C" fn(*mut LState, c_int),
    rawequal: extern "C" fn(*mut LState, c_int, c_int) -> c_int,
    rawget: extern "C" fn(*mut LState, c_int) -> c_int,
    rawgeti: extern "C" fn(*mut LState, c_int, i64) -> c_int,
    rawgetp: extern "C" fn(*mut LState, c_int, *const c_void) -> c_int,
    rawlen: extern "C" fn(*mut LState, c_int) -> u64,
    rawset: extern "C" fn(*mut LState, c_int),
    rawseti: extern "C" fn(*mut LState, c_int, i64),
    rawsetp: extern "C" fn(*mut LState, c_int, *const c_void),
    rotate: extern "C" fn(*mut LState, c_int, c_int),
    setfield: extern "C" fn(*mut LState, c_int, *const c_char),
    setglobal: extern "C" fn(*mut LState, *const c_char),
    seti: extern "C" fn(*mut LState, c_int, i64),
    setiuservalue: extern "C" fn(*mut LState, c_int, c_int) -> c_int,
    setlocal: extern "C" fn(*mut LState, *const LuaDebug, c_int) -> *const c_char,
    setmetatable: extern "C" fn(*mut LState, c_int) -> c_int,
    settable: extern "C" fn(*mut LState, c_int),
    settop: extern "C" fn(*mut LState, c_int),
    setupvalue: extern "C" fn(*mut LState, c_int, c_int) -> *const c_char,
    setwarnf: extern "C" fn(*mut LState, Option<WarnFn>, *mut c_void),
    stringtonumber: extern "C" fn(*mut LState, *const c_char) -> usize,
    toboolean: extern "C" fn(*mut LState, c_int) -> c_int,
    tocfunction: extern "C" fn(*mut LState, c_int) -> Option<CFn>,
    toclose: extern "C" fn(*mut LState, c_int),
    tointegerx: extern "C" fn(*mut LState, c_int, *mut c_int) -> i64,
    tolstring: extern "C" fn(*mut LState, c_int, *mut usize) -> *const c_char,
    tolstringmeta: extern "C" fn(*mut LState, c_int, *mut usize) -> *const c_char,
    tonumberx: extern "C" fn(*mut LState, c_int, *mut c_int) -> f64,
    topointer: extern "C" fn(*mut LState, c_int) -> *const c_void,
    touserdata: extern "C" fn(*mut LState, c_int) -> *mut c_void,
    traceback: extern "C" fn(*mut LState, *const c_char, c_int),
    r#type: extern "C" fn(*mut LState, c_int) -> c_int,
    typeerror: extern "C" fn(*mut LState, c_int, *const c_char),
    unsupported: extern "C" fn(*mut LState, *const c_char),
    upvalueid: extern "C" fn(*mut LState, c_int, c_int) -> *mut c_void,
    upvaluejoin: extern "C" fn(*mut LState, c_int, c_int, c_int, c_int),
    version: extern "C" fn(*mut LState) -> f64,
    warning: extern "C" fn(*mut LState, *const c_char, c_int),
    r#where: extern "C" fn(*mut LState, c_int),
}

static TABLE: RTable = RTable {
    absindex: orbit_r_absindex,
    argerror: orbit_r_argerror,
    arith: orbit_r_arith,
    callk: orbit_r_callk,
    checkstack: orbit_r_checkstack,
    close: orbit_r_close,
    closeslot: orbit_r_closeslot,
    compare: orbit_r_compare,
    concat: orbit_r_concat,
    copy: orbit_r_copy,
    createtable: orbit_r_createtable,
    error: orbit_r_error,
    gc: orbit_r_gc,
    getfield: orbit_r_getfield,
    getglobal: orbit_r_getglobal,
    geti: orbit_r_geti,
    getinfo: orbit_r_getinfo,
    getiuservalue: orbit_r_getiuservalue,
    getlocal: orbit_r_getlocal,
    getmetatable: orbit_r_getmetatable,
    getstack: orbit_r_getstack,
    gettable: orbit_r_gettable,
    gettop: orbit_r_gettop,
    getupvalue: orbit_r_getupvalue,
    iscfunction: orbit_r_iscfunction,
    isinteger: orbit_r_isinteger,
    isnumber: orbit_r_isnumber,
    isrunning: orbit_r_isrunning,
    isrustudata: orbit_r_isrustudata,
    isstring: orbit_r_isstring,
    isuserdata: orbit_r_isuserdata,
    len: orbit_r_len,
    load: orbit_r_load,
    newuserdatauv: orbit_r_newuserdatauv,
    next: orbit_r_next,
    numbertocstring: orbit_r_numbertocstring,
    openlib: orbit_r_openlib,
    pcallk: orbit_r_pcallk,
    pushboolean: orbit_r_pushboolean,
    pushcclosure: orbit_r_pushcclosure,
    pushinteger: orbit_r_pushinteger,
    pushlightuserdata: orbit_r_pushlightuserdata,
    pushlstring: orbit_r_pushlstring,
    pushnil: orbit_r_pushnil,
    pushnumber: orbit_r_pushnumber,
    pushpending: orbit_r_pushpending,
    pushthread: orbit_r_pushthread,
    pushvalue: orbit_r_pushvalue,
    rawequal: orbit_r_rawequal,
    rawget: orbit_r_rawget,
    rawgeti: orbit_r_rawgeti,
    rawgetp: orbit_r_rawgetp,
    rawlen: orbit_r_rawlen,
    rawset: orbit_r_rawset,
    rawseti: orbit_r_rawseti,
    rawsetp: orbit_r_rawsetp,
    rotate: orbit_r_rotate,
    setfield: orbit_r_setfield,
    setglobal: orbit_r_setglobal,
    seti: orbit_r_seti,
    setiuservalue: orbit_r_setiuservalue,
    setlocal: orbit_r_setlocal,
    setmetatable: orbit_r_setmetatable,
    settable: orbit_r_settable,
    settop: orbit_r_settop,
    setupvalue: orbit_r_setupvalue,
    setwarnf: orbit_r_setwarnf,
    stringtonumber: orbit_r_stringtonumber,
    toboolean: orbit_r_toboolean,
    tocfunction: orbit_r_tocfunction,
    toclose: orbit_r_toclose,
    tointegerx: orbit_r_tointegerx,
    tolstring: orbit_r_tolstring,
    tolstringmeta: orbit_r_tolstringmeta,
    tonumberx: orbit_r_tonumberx,
    topointer: orbit_r_topointer,
    touserdata: orbit_r_touserdata,
    traceback: orbit_r_traceback,
    r#type: orbit_r_type,
    typeerror: orbit_r_typeerror,
    unsupported: orbit_r_unsupported,
    upvalueid: orbit_r_upvalueid,
    upvaluejoin: orbit_r_upvaluejoin,
    version: orbit_r_version,
    warning: orbit_r_warning,
    r#where: orbit_r_where,
};

// ---- entry points (names match csrc/capi.c) ---------------------------------

extern "C" fn orbit_r_version(l: *mut LState) -> f64 {
    entry!(l, 0.0, |_b, _c| Ok(505.0))
}

extern "C" fn orbit_r_absindex(l: *mut LState, idx: c_int) -> c_int {
    entry!(l, 0, |_b, c| Ok(api::absindex(c, idx)))
}

extern "C" fn orbit_r_gettop(l: *mut LState) -> c_int {
    entry!(l, 0, |_b, c| Ok(api::gettop(c)))
}

extern "C" fn orbit_r_settop(l: *mut LState, idx: c_int) {
    entry!(l, (), |_b, c| api::settop(c, idx))
}

extern "C" fn orbit_r_pushvalue(l: *mut LState, idx: c_int) {
    entry!(l, (), |_b, c| {
        api::pushvalue(c, idx);
        Ok(())
    })
}

extern "C" fn orbit_r_rotate(l: *mut LState, idx: c_int, n: c_int) {
    entry!(l, (), |_b, c| api::rotate(c, idx, n))
}

extern "C" fn orbit_r_copy(l: *mut LState, from: c_int, to: c_int) {
    entry!(l, (), |_b, c| api::copy(c, from, to))
}

extern "C" fn orbit_r_checkstack(l: *mut LState, n: c_int) -> c_int {
    entry!(l, 0, |_b, c| Ok(api::checkstack(c, n) as c_int))
}

extern "C" fn orbit_r_isnumber(l: *mut LState, idx: c_int) -> c_int {
    entry!(l, 0, |_b, c| Ok(api::tonumber(c, idx).is_some() as c_int))
}

extern "C" fn orbit_r_isstring(l: *mut LState, idx: c_int) -> c_int {
    entry!(l, 0, |_b, c| Ok(api::isstring(c, idx) as c_int))
}

extern "C" fn orbit_r_iscfunction(l: *mut LState, idx: c_int) -> c_int {
    entry!(l, 0, |_b, c| Ok(api::iscfunction(c, idx) as c_int))
}

extern "C" fn orbit_r_isinteger(l: *mut LState, idx: c_int) -> c_int {
    entry!(l, 0, |_b, c| Ok(api::isinteger(c, idx) as c_int))
}

extern "C" fn orbit_r_isuserdata(l: *mut LState, idx: c_int) -> c_int {
    entry!(l, 0, |_b, c| Ok(api::isuserdata(c, idx) as c_int))
}

extern "C" fn orbit_r_type(l: *mut LState, idx: c_int) -> c_int {
    entry!(l, 0, |_b, c| Ok(api::type_(c, idx)))
}

extern "C" fn orbit_r_tonumberx(l: *mut LState, idx: c_int, isnum: *mut c_int) -> f64 {
    entry!(l, 0.0, |_b, c| {
        let n = api::tonumber(c, idx);
        set_flag(isnum, n.is_some());
        Ok(n.unwrap_or(0.0))
    })
}

extern "C" fn orbit_r_tointegerx(l: *mut LState, idx: c_int, isnum: *mut c_int) -> i64 {
    entry!(l, 0, |_b, c| {
        let n = api::tointeger(c, idx);
        set_flag(isnum, n.is_some());
        Ok(n.unwrap_or(0))
    })
}

extern "C" fn orbit_r_toboolean(l: *mut LState, idx: c_int) -> c_int {
    entry!(l, 0, |_b, c| Ok(api::toboolean(c, idx) as c_int))
}

extern "C" fn orbit_r_tolstring(l: *mut LState, idx: c_int, len: *mut usize) -> *const c_char {
    entry!(l, std::ptr::null(), |b, c| {
        Ok(match api::tolstring(c, idx)? {
            Some(s) => hand_out_value(&b.st, c.bytes(s), len),
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

extern "C" fn orbit_r_rawlen(l: *mut LState, idx: c_int) -> u64 {
    entry!(l, 0, |_b, c| Ok(api::rawlen(c, idx)))
}

extern "C" fn orbit_r_tocfunction(l: *mut LState, idx: c_int) -> Option<CFn> {
    entry!(l, None, |_b, c| Ok(api::tocfunction(c, idx).map(|f| {
        // SAFETY: the address came from C as a `lua_CFunction`
        // (lua_pushcclosure), kept where scripts can't change it.
        unsafe { std::mem::transmute::<usize, CFn>(f as usize) }
    })))
}

extern "C" fn orbit_r_touserdata(l: *mut LState, idx: c_int) -> *mut c_void {
    entry!(l, std::ptr::null_mut(), |_b, c| Ok(api::touserdata(c, idx)))
}

extern "C" fn orbit_r_topointer(l: *mut LState, idx: c_int) -> *const c_void {
    entry!(l, std::ptr::null(), |_b, c| Ok(
        api::topointer(c, idx) as *const c_void
    ))
}

extern "C" fn orbit_r_rawequal(l: *mut LState, a: c_int, b_: c_int) -> c_int {
    entry!(l, 0, |_b, c| Ok(api::rawequal(c, a, b_) as c_int))
}

extern "C" fn orbit_r_compare(l: *mut LState, a: c_int, b_: c_int, op: c_int) -> c_int {
    entry!(l, 0, |b, c| api::compare(c, a, b_, op).map(|r| r as c_int))
}

extern "C" fn orbit_r_pushnil(l: *mut LState) {
    entry!(l, (), |_b, c| {
        c.push(Value::Nil);
        Ok(())
    })
}

extern "C" fn orbit_r_pushnumber(l: *mut LState, n: f64) {
    entry!(l, (), |_b, c| {
        c.push(Value::Float(n));
        Ok(())
    })
}

extern "C" fn orbit_r_pushinteger(l: *mut LState, n: i64) {
    entry!(l, (), |_b, c| {
        c.push(Value::Int(n));
        Ok(())
    })
}

extern "C" fn orbit_r_pushlstring(l: *mut LState, s: *const c_char, len: usize) -> *const c_char {
    entry!(l, std::ptr::null(), |b, c| {
        let bytes: &[u8] = if len == 0 {
            b""
        } else {
            // SAFETY: C passed `len` readable bytes at `s`.
            unsafe { std::slice::from_raw_parts(s as *const u8, len) }
        };
        let st = api::pushstr(c, bytes);
        Ok(hand_out_value(&b.st, c.bytes(st), std::ptr::null_mut()))
    })
}

extern "C" fn orbit_r_pushcclosure(l: *mut LState, f: Option<CFn>, n: c_int) {
    entry!(l, (), |_b, c| match f {
        Some(f) => api::pushcclosure(c, f as usize as u64, n),
        None => Err(c.error("C API: null C function")),
    })
}

extern "C" fn orbit_r_pushboolean(l: *mut LState, v: c_int) {
    entry!(l, (), |_b, c| {
        c.push(Value::Bool(v != 0));
        Ok(())
    })
}

extern "C" fn orbit_r_pushlightuserdata(l: *mut LState, p: *mut c_void) {
    entry!(l, (), |_b, c| {
        c.push(api::light(p as usize));
        Ok(())
    })
}

extern "C" fn orbit_r_getglobal(l: *mut LState, name: *const c_char) -> c_int {
    // SAFETY: C passes a NUL-terminated name.
    let name = unsafe { cbytes(name) };
    entry!(l, 0, |b, c| api::getglobal(c, name))
}

extern "C" fn orbit_r_gettable(l: *mut LState, idx: c_int) -> c_int {
    entry!(l, 0, |b, c| api::gettable(c, idx))
}

extern "C" fn orbit_r_getfield(l: *mut LState, idx: c_int, k: *const c_char) -> c_int {
    // SAFETY: C passes a NUL-terminated key.
    let k = unsafe { cbytes(k) };
    entry!(l, 0, |b, c| {
        let obj = api_get(c, idx);
        let key = c.new_str(k);
        api::gettable_with(c, obj, key)
    })
}

extern "C" fn orbit_r_geti(l: *mut LState, idx: c_int, n: i64) -> c_int {
    entry!(l, 0, |b, c| {
        let obj = api_get(c, idx);
        api::gettable_with(c, obj, Value::Int(n))
    })
}

extern "C" fn orbit_r_rawget(l: *mut LState, idx: c_int) -> c_int {
    entry!(l, 0, |_b, c| api::rawget(c, idx))
}

extern "C" fn orbit_r_rawgeti(l: *mut LState, idx: c_int, n: i64) -> c_int {
    entry!(l, 0, |_b, c| api::rawget_with(c, idx, Value::Int(n)))
}

extern "C" fn orbit_r_rawgetp(l: *mut LState, idx: c_int, p: *const c_void) -> c_int {
    entry!(l, 0, |_b, c| api::rawget_with(
        c,
        idx,
        api::light(p as usize)
    ))
}

extern "C" fn orbit_r_createtable(l: *mut LState, narr: c_int, nrec: c_int) {
    entry!(l, (), |_b, c| {
        api::createtable(c, narr, nrec);
        Ok(())
    })
}

extern "C" fn orbit_r_newuserdatauv(l: *mut LState, size: usize, nuv: c_int) -> *mut c_void {
    entry!(l, std::ptr::null_mut(), |_b, c| api::newuserdata(
        c, size, nuv
    ))
}

extern "C" fn orbit_r_getmetatable(l: *mut LState, idx: c_int) -> c_int {
    entry!(l, 0, |_b, c| Ok(api::getmetatable(c, idx) as c_int))
}

extern "C" fn orbit_r_getiuservalue(l: *mut LState, idx: c_int, n: c_int) -> c_int {
    entry!(l, abi::TNONE, |_b, c| Ok(api::getiuservalue(c, idx, n)))
}

extern "C" fn orbit_r_setglobal(l: *mut LState, name: *const c_char) {
    // SAFETY: C passes a NUL-terminated name.
    let name = unsafe { cbytes(name) };
    entry!(l, (), |b, c| api::setglobal(c, name))
}

extern "C" fn orbit_r_settable(l: *mut LState, idx: c_int) {
    entry!(l, (), |b, c| api::settable(c, idx))
}

extern "C" fn orbit_r_setfield(l: *mut LState, idx: c_int, k: *const c_char) {
    // SAFETY: C passes a NUL-terminated key.
    let k = unsafe { cbytes(k) };
    entry!(l, (), |b, c| api::setfield(c, idx, k))
}

extern "C" fn orbit_r_seti(l: *mut LState, idx: c_int, n: i64) {
    entry!(l, (), |b, c| {
        let obj = api_get(c, idx);
        api::set_popping(c, obj, Value::Int(n))
    })
}

extern "C" fn orbit_r_rawset(l: *mut LState, idx: c_int) {
    entry!(l, (), |_b, c| api::rawset(c, idx))
}

extern "C" fn orbit_r_rawseti(l: *mut LState, idx: c_int, n: i64) {
    entry!(l, (), |_b, c| api::rawset_with(c, idx, Value::Int(n)))
}

extern "C" fn orbit_r_rawsetp(l: *mut LState, idx: c_int, p: *const c_void) {
    entry!(l, (), |_b, c| api::rawset_with(
        c,
        idx,
        api::light(p as usize)
    ))
}

extern "C" fn orbit_r_setmetatable(l: *mut LState, idx: c_int) -> c_int {
    entry!(l, 0, |_b, c| api::setmetatable(c, idx).map(|()| 1))
}

extern "C" fn orbit_r_setiuservalue(l: *mut LState, idx: c_int, n: c_int) -> c_int {
    entry!(l, 0, |_b, c| api::setiuservalue(c, idx, n).map(c_int::from))
}

extern "C" fn orbit_r_callk(
    l: *mut LState,
    nargs: c_int,
    nresults: c_int,
    _ctx: isize,
    _k: *const c_void,
) {
    entry!(l, (), |b, c| api::call(c, nargs, nresults))
}

extern "C" fn orbit_r_pcallk(
    l: *mut LState,
    nargs: c_int,
    nresults: c_int,
    errfunc: c_int,
    _ctx: isize,
    _k: *const c_void,
) -> c_int {
    entry!(l, abi::ERRRUN, |b, c| api::pcall(
        c, nargs, nresults, errfunc
    ))
}

extern "C" fn orbit_r_error(l: *mut LState) -> c_int {
    entry!(l, 0, |_b, c| Err::<c_int, _>(api::error(c)))
}

extern "C" fn orbit_r_next(l: *mut LState, idx: c_int) -> c_int {
    entry!(l, 0, |_b, c| api::next(c, idx).map(|r| r as c_int))
}

extern "C" fn orbit_r_concat(l: *mut LState, n: c_int) {
    entry!(l, (), |_b, c| api::concat(c, n))
}

extern "C" fn orbit_r_len(l: *mut LState, idx: c_int) {
    entry!(l, (), |b, c| api::len(c, idx))
}

extern "C" fn orbit_r_stringtonumber(l: *mut LState, s: *const c_char) -> usize {
    // SAFETY: C passes a NUL-terminated string.
    let s = unsafe { cbytes(s) };
    entry!(l, 0, |_b, c| Ok(api::stringtonumber(c, s)))
}

extern "C" fn orbit_r_gc(l: *mut LState, what: c_int, a: c_int, b: c_int) -> c_int {
    entry!(l, 0, |_b, c| Ok(api::gc(c, what, a, b)))
}

extern "C" fn orbit_r_argerror(l: *mut LState, arg: c_int, msg: *const c_char) {
    // SAFETY: C passes a NUL-terminated message, or NULL.
    let msg = String::from_utf8_lossy(unsafe { cbytes(msg) }).into_owned();
    entry!(l, (), |_b, c| Err(api::argerror(c, arg, &msg)))
}

extern "C" fn orbit_r_typeerror(l: *mut LState, arg: c_int, tname: *const c_char) {
    // SAFETY: C passes a NUL-terminated type name.
    let tname = String::from_utf8_lossy(unsafe { cbytes(tname) }).into_owned();
    entry!(l, (), |_b, c| Err(api::typeerror(c, arg, &tname)))
}

extern "C" fn orbit_r_where(l: *mut LState, lvl: c_int) {
    entry!(l, (), |b, c| {
        api::where_(c, lua_level(b, lvl));
        Ok(())
    })
}

extern "C" fn orbit_r_tolstringmeta(l: *mut LState, idx: c_int, len: *mut usize) -> *const c_char {
    entry!(l, std::ptr::null(), |b, c| {
        let s = api::tolstring_meta(c, idx)?;
        Ok(hand_out_value(&b.st, c.bytes(s), len))
    })
}

extern "C" fn orbit_r_arith(l: *mut LState, op: c_int) {
    entry!(l, (), |_b, c| api::arith(c, op))
}

extern "C" fn orbit_r_numbertocstring(l: *mut LState, idx: c_int, buff: *mut c_char) -> u32 {
    entry!(l, 0, |_b, c| Ok(match api::number_text(c, idx) {
        Some(t) => {
            // `LUA_N2SBUFFSZ` (64) bytes is room for any number Lua prints.
            let n = t.len().min(63);
            // SAFETY: C passed a buffer of at least LUA_N2SBUFFSZ bytes.
            unsafe {
                std::ptr::copy_nonoverlapping(t.as_ptr(), buff as *mut u8, n);
                *buff.add(n) = 0;
            }
            n as u32 + 1
        }
        None => 0,
    }))
}

extern "C" fn orbit_r_load(
    l: *mut LState,
    buf: *const c_char,
    len: usize,
    chunkname: *const c_char,
    mode: *const c_char,
) -> c_int {
    let bytes: &[u8] = if len == 0 {
        b""
    } else {
        // SAFETY: the C side passes the `len` bytes it read.
        unsafe { std::slice::from_raw_parts(buf as *const u8, len) }
    };
    // SAFETY: NUL-terminated or NULL.
    let name: &[u8] = if chunkname.is_null() {
        b"?"
    } else {
        unsafe { cbytes(chunkname) }
    };
    // SAFETY: as above.
    let mode: &[u8] = if mode.is_null() {
        b"bt"
    } else {
        unsafe { cbytes(mode) }
    };
    entry!(l, abi::ERRSYNTAX, |_b, c| Ok(api::load(
        c, bytes, name, mode
    )))
}

extern "C" fn orbit_r_isrustudata(l: *mut LState, idx: c_int) -> c_int {
    entry!(l, 0, |_b, c| Ok(api::is_rust_userdata(c, idx) as c_int))
}

extern "C" fn orbit_r_warning(l: *mut LState, msg: *const c_char, tocont: c_int) {
    // SAFETY: NUL-terminated.
    let msg = unsafe { cbytes(msg) };
    entry!(l, (), |_b, c| {
        api::warning(c, msg, tocont != 0);
        Ok(())
    })
}

extern "C" fn orbit_r_traceback(l: *mut LState, msg: *const c_char, level: c_int) {
    // SAFETY: NUL-terminated or NULL.
    let text = unsafe { cbytes(msg) };
    let msg = (!msg.is_null()).then_some(text);
    entry!(l, (), |b, c| {
        api::traceback(c, msg, lua_level(b, level));
        Ok(())
    })
}

extern "C" fn orbit_r_pushthread(l: *mut LState) -> c_int {
    entry!(l, 0, |_b, c| Ok(api::pushthread(c) as c_int))
}

/// Whether the value at `idx` is the running thread (the only one this
/// state stands for).
extern "C" fn orbit_r_isrunning(l: *mut LState, idx: c_int) -> c_int {
    entry!(l, 0, |_b, c| Ok(match api::get(c, idx) {
        Value::Thread(t) => (t == c.running().0) as c_int,
        _ => 0,
    }))
}

extern "C" fn orbit_r_unsupported(l: *mut LState, what: *const c_char) {
    // SAFETY: NUL-terminated.
    let what = String::from_utf8_lossy(unsafe { cbytes(what) }).into_owned();
    entry!(l, (), |_b, c| Err(
        c.error(format!("{what} is not supported by OrbitScript yet"))
    ))
}

// ---- debug interface ----------------------------------------------------------

/// `lua_Debug` in `lua.h`.
#[repr(C)]
pub(crate) struct LuaDebug {
    event: c_int,
    name: *const c_char,
    namewhat: *const c_char,
    what: *const c_char,
    source: *const c_char,
    srclen: usize,
    currentline: c_int,
    linedefined: c_int,
    lastlinedefined: c_int,
    nups: u8,
    nparams: u8,
    isvararg: c_char,
    extraargs: u8,
    istailcall: c_char,
    ftransfer: c_int,
    ntransfer: c_int,
    short_src: [c_char; 60],
    /// The level `lua_getstack` found, plus one (0: none).
    private: usize,
}

extern "C" fn orbit_r_getstack(l: *mut LState, level: c_int, ar: *mut LuaDebug) -> c_int {
    entry!(l, 0, |b, c| {
        let level = lua_level(b, level);
        if !api::has_level(c, level) {
            return Ok(0);
        }
        // SAFETY: C passed a lua_Debug to fill.
        unsafe { (*ar).private = level as usize + 1 };
        Ok(1)
    })
}

extern "C" fn orbit_r_toclose(l: *mut LState, idx: c_int) {
    entry!(l, (), |_b, c| api::toclose(c, idx))
}

extern "C" fn orbit_r_closeslot(l: *mut LState, idx: c_int) {
    entry!(l, (), |_b, c| api::closeslot(c, idx))
}

/// The level a `lua_Debug` from `lua_getstack` names.
fn debug_level(c: &mut NativeCtx<'_>, ar: *const LuaDebug) -> Res<usize> {
    // SAFETY: C passed a lua_Debug from lua_getstack.
    match unsafe { (*ar).private } {
        0 => Err(c.error("C API: lua_Debug without lua_getstack")),
        p => Ok(p - 1),
    }
}

extern "C" fn orbit_r_getlocal(l: *mut LState, ar: *const LuaDebug, n: c_int) -> *const c_char {
    entry!(l, std::ptr::null(), |b, c| {
        let name = if ar.is_null() {
            api::param_name(c, n)
        } else {
            let level = debug_level(c, ar)?;
            api::getlocal(c, level, n)
        };
        Ok(name.map_or(std::ptr::null(), |name| {
            hand_out(&b.st, &name, std::ptr::null_mut())
        }))
    })
}

extern "C" fn orbit_r_setlocal(l: *mut LState, ar: *const LuaDebug, n: c_int) -> *const c_char {
    entry!(l, std::ptr::null(), |b, c| {
        let level = debug_level(c, ar)?;
        let name = api::setlocal(c, level, n)?;
        Ok(name.map_or(std::ptr::null(), |name| {
            hand_out(&b.st, &name, std::ptr::null_mut())
        }))
    })
}

/// A static C string for one of `getinfo`'s fixed words.
fn word(w: &str) -> *const c_char {
    let s: &'static std::ffi::CStr = match w {
        "Lua" => c"Lua",
        "C" => c"C",
        "main" => c"main",
        "global" => c"global",
        "local" => c"local",
        "method" => c"method",
        "field" => c"field",
        "upvalue" => c"upvalue",
        "constant" => c"constant",
        "for iterator" => c"for iterator",
        "metamethod" => c"metamethod",
        "hook" => c"hook",
        _ => c"",
    };
    s.as_ptr()
}

extern "C" fn orbit_r_getinfo(l: *mut LState, what: *const c_char, ar: *mut LuaDebug) -> c_int {
    // SAFETY: NUL-terminated.
    let opts = unsafe { cbytes(what) };
    entry!(l, 0, |b, c| {
        let (opts, level, f) = if opts.first() == Some(&b'>') {
            let len = c.len();
            if len == 0 {
                return Err(c.error("C API: function expected for '>'"));
            }
            let f = c.arg(len - 1);
            c.truncate(len - 1);
            // Keep it rooted while the strings are made.
            c.push(f);
            (&opts[1..], None, f)
        } else {
            // SAFETY: C passed a lua_Debug from lua_getstack.
            let private = unsafe { (*ar).private };
            if private == 0 {
                return Err(c.error("C API: lua_getinfo without lua_getstack"));
            }
            (opts, Some(private - 1), Value::Nil)
        };
        let Some(info) = api::info(c, level, f) else {
            if level.is_none() {
                let len = c.len();
                c.truncate(len - 1);
            }
            return Ok(0);
        };
        // SAFETY: `ar` is the caller's lua_Debug; the strings handed out live
        // until the running C function returns.
        let d = unsafe { &mut *ar };
        for &o in opts {
            match o {
                b'S' => {
                    d.source = hand_out(&b.st, &info.source, &mut d.srclen);
                    d.linedefined = info.linedefined;
                    d.lastlinedefined = info.lastlinedefined;
                    d.what = word(info.what);
                    let src = info.short_src.as_bytes();
                    let n = src.len().min(59);
                    for (i, &byte) in src[..n].iter().enumerate() {
                        d.short_src[i] = byte as c_char;
                    }
                    d.short_src[n] = 0;
                }
                b'l' => d.currentline = info.currentline,
                b'u' => {
                    d.nups = info.nups;
                    d.nparams = info.nparams;
                    d.isvararg = info.isvararg as c_char;
                }
                b'n' => {
                    d.namewhat = word(info.namewhat);
                    d.name = match &info.name {
                        Some(n) => hand_out(&b.st, n, std::ptr::null_mut()),
                        None => std::ptr::null(),
                    };
                }
                b't' => {
                    d.istailcall = info.istailcall as c_char;
                    d.extraargs = info.extraargs;
                }
                b'r' => {
                    d.ftransfer = 0;
                    d.ntransfer = 0;
                }
                b'f' | b'L' => {}
                _ => return Ok(0),
            }
        }
        if level.is_none() {
            let len = c.len();
            c.truncate(len - 1);
        }
        if opts.contains(&b'f') {
            match level {
                Some(n) => api::push_level_func(c, n),
                None => c.push(f),
            }
        }
        if opts.contains(&b'L') {
            c.push(Value::Nil);
        }
        Ok(1)
    })
}

extern "C" fn orbit_r_getupvalue(l: *mut LState, funcindex: c_int, n: c_int) -> *const c_char {
    entry!(l, std::ptr::null(), |b, c| Ok(
        match api::getupvalue(c, funcindex, n) {
            Some(name) => hand_out(&b.st, &name, std::ptr::null_mut()),
            None => std::ptr::null(),
        }
    ))
}

extern "C" fn orbit_r_setupvalue(l: *mut LState, funcindex: c_int, n: c_int) -> *const c_char {
    entry!(l, std::ptr::null(), |b, c| Ok(
        match api::setupvalue(c, funcindex, n)? {
            Some(name) => hand_out(&b.st, &name, std::ptr::null_mut()),
            None => std::ptr::null(),
        }
    ))
}

extern "C" fn orbit_r_upvalueid(l: *mut LState, funcindex: c_int, n: c_int) -> *mut c_void {
    entry!(l, std::ptr::null_mut(), |_b, c| Ok(
        api::upvalueid(c, funcindex, n) as *mut c_void
    ))
}

extern "C" fn orbit_r_upvaluejoin(l: *mut LState, f1: c_int, n1: c_int, f2: c_int, n2: c_int) {
    entry!(l, (), |_b, c| api::upvaluejoin(c, f1, n1, f2, n2))
}

fn api_get(c: &NativeCtx<'_>, idx: c_int) -> Value {
    // `lua_getfield`/`geti`/`seti` read their object before pushing.
    api::get(c, idx)
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

// ---- states created from C (ADR-10) ---------------------------------------------

/// `luaopen_*`.
extern "C" fn orbit_r_openlib(l: *mut LState, lib: c_int) -> c_int {
    entry!(l, 0, |_b, c| Ok(api::openlib(c, lib)))
}

/// Push the pending error, for a panic function to read.
extern "C" fn orbit_r_pushpending(l: *mut LState) {
    entry!(l, (), |b, c| {
        let v = b.st.borrow_mut().pending.take().unwrap_or(Value::Nil);
        c.push(v);
        Ok(())
    })
}

type WarnFn = unsafe extern "C" fn(ud: *mut c_void, msg: *const c_char, tocont: c_int);

/// `lua_setwarnf`: warnings go to `f` from now on; a null `f` drops them.
extern "C" fn orbit_r_setwarnf(l: *mut LState, f: Option<WarnFn>, ud: *mut c_void) {
    entry!(l, (), |_b, c| {
        c.vm.rt.warn_hook = Some(orbit_vm::WarnHook(Box::new(move |msg, tocont| {
            let Some(f) = f else { return };
            let mut text = Vec::with_capacity(msg.len() + 1);
            text.extend(msg.iter().map(|&b| if b == 0 { b'?' } else { b }));
            text.push(0);
            // SAFETY: `f` is the program's warning function, called as
            // lua_setwarnf documents, with a NUL-terminated message that
            // lives through the call.
            unsafe { f(ud, text.as_ptr() as *const c_char, tocont as c_int) }
        })));
        Ok(())
    })
}

#[cfg(feature = "host")]
unsafe extern "C" {
    fn orbit_capi_write_stdout(p: *const c_char, n: usize) -> usize;
    fn orbit_capi_flush_stdout() -> c_int;
}

/// `print`'s output in a state C created: C's own `stdout`, so it
/// interleaves with the program's `printf`s as in Lua.
#[cfg(feature = "host")]
#[derive(Debug)]
struct CStdout;

#[cfg(feature = "host")]
impl std::io::Write for CStdout {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        // SAFETY: `buf` is readable for its length.
        let n = unsafe { orbit_capi_write_stdout(buf.as_ptr() as *const c_char, buf.len()) };
        if n == 0 && !buf.is_empty() {
            return Err(std::io::Error::last_os_error());
        }
        Ok(n)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        // SAFETY: no arguments; fflush(stdout).
        match unsafe { orbit_capi_flush_stdout() } {
            0 => Ok(()),
            _ => Err(std::io::Error::last_os_error()),
        }
    }
}

/// `lua_newstate`: a VM owned by the C program, whose stack between calls
/// is the host window ([`Vm::host_ctx`]). Null if memory runs out.
#[cfg(feature = "host")]
#[unsafe(no_mangle)]
extern "C" fn orbit_r_newstate() -> *mut LState {
    let vm: &'static mut Vm = Box::leak(Box::new(crate::host::new_vm()));
    vm.rt.out = Box::new(CStdout);
    crate::install(vm);
    let Some(p) = vm.rt.ext.get::<BridgeHandle>().map(|h| h.0) else {
        return std::ptr::null_mut();
    };
    let ctx = Box::into_raw(Box::new(vm.host_ctx()));
    // SAFETY: the handle keeps the bridge alive until `orbit_r_close`.
    let b = unsafe { &*p };
    b.ctx.set(ctx);
    b.host.set(ctx);
    b.l.get()
}

/// `lua_close`: close the state as Lua does (finalizers run), flush the
/// libraries' files, and free the VM, its bridge and `l`. Only for states
/// `orbit_r_newstate` made; it does nothing to others.
extern "C" fn orbit_r_close(l: *mut LState) {
    // SAFETY: `l` is a live state; its bridge lives as long as it does.
    let b = unsafe { &*((*l).rs as *const Bridge) };
    let host = b.host.get();
    if host.is_null() || !std::ptr::eq(b.ctx.get(), host) {
        // A state Rust made, or a C function closing its own caller.
        return;
    }
    // SAFETY: made by `orbit_r_newstate` with `Box::into_raw`; no C function
    // is running, so nothing else uses it.
    let ctx = unsafe { Box::from_raw(host) };
    b.ctx.set(std::ptr::null_mut());
    b.host.set(std::ptr::null_mut());
    let vm: *mut Vm = &mut *ctx.vm;
    drop(ctx);
    // SAFETY: leaked by `orbit_r_newstate` and reached only through this
    // state, which the program gives up by calling lua_close. Dropping the
    // VM drops the bridge handle, which frees the bridge and `l`.
    unsafe {
        (*vm).close();
        orbit_stdlib::flush_files(&mut *vm);
        let _ = std::io::Write::flush(&mut (*vm).rt.out);
        drop(Box::from_raw(vm));
    }
}
