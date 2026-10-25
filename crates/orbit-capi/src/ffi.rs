//! The crate's only `unsafe` (ADR-6 exception): the entry
//! points `csrc/capi.c` calls, the call into C, and dynamic loading.
//!
//! Each entry point gets back the running native's [`Stack`] from the
//! state pointer, does its work through the stack API (and the C-specific
//! glue in [`crate::api`]), and reports an error by leaving it pending and
//! setting the state's status, never by unwinding: the C side turns that
//! into a `longjmp` once this function has returned.
#![allow(unsafe_code)]

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::ffi::{CStr, c_char, c_int, c_void};
use std::mem::size_of;

use orbit::stack::{
    Continuation, Control, InterruptHandler, Native, NativeRef, Outcome, Raised, Return, Stack,
    Status, ThreadRef, ThreadWindow, Type,
};
use orbit::{OrbitStr, OrbitString, ThreadStatus};

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

/// `lua_KFunction`.
type KFn = unsafe extern "C" fn(*mut LState, c_int, isize) -> c_int;

extern "C" {
    fn orbit_capi_newstate(rs: *const c_void, table: *const RTable) -> *mut LState;
    fn orbit_capi_table_size() -> usize;
    fn orbit_capi_freestate(l: *mut LState);
    fn orbit_capi_newthread(main: *mut LState) -> *mut LState;
    fn orbit_capi_trampoline(l: *mut LState, f: CFn) -> c_int;
    fn orbit_capi_trampoline_k(l: *mut LState, k: KFn, status: c_int, ctx: isize) -> c_int;
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

/// The stack of a running C function, as the bridge keeps it between the
/// call into C and the entry points C calls back.
type StackPtr = *mut Stack<'static>;

/// What the C side reaches through a state: the running native and the
/// API's state.
#[derive(Debug)]
pub(crate) struct Bridge {
    l: Cell<*mut LState>,
    /// The stack of the native running a C function, null between calls.
    ctx: Cell<StackPtr>,
    st: Shared,
    /// In a state C created (ADR-10), the host's own stack, which
    /// [`Bridge::ctx`] holds between calls; null otherwise.
    host: Cell<StackPtr>,
    /// In a state C created, its isolate, which the state owns.
    isolate: Cell<*mut orbit::Isolate>,
    /// A state C created that debuggers can attach to ([`crate::debug`]).
    debuggable: Cell<bool>,
    /// The native standing for each C hook function installed so far.
    hooks: RefCell<Vec<(usize, NativeRef)>>,
    /// While a C hook runs, its native's stack: there, as in Lua, level 0
    /// is the hooked function, one below the native's own.
    hook_ctx: Cell<StackPtr>,
    /// The main thread, which [`Bridge::l`] stands for.
    main: ThreadRef,
    /// The state standing for each other thread C has seen, by the
    /// thread's slot. A collected thread's state is reused by whichever
    /// thread takes its slot; all are freed with the bridge.
    threads: RefCell<HashMap<u32, ThreadState>>,
    /// Each state's thread, by the state's address.
    by_l: RefCell<HashMap<usize, ThreadRef>>,
    /// What the running C function asked for by jumping out of it
    /// (`lua_yieldk`, or `lua_callk`/`lua_pcallk` with a continuation).
    request: Cell<Option<Request>>,
    /// The continuations of C functions waiting for a yield or a call,
    /// newest last ([`push_kont`]).
    konts: RefCell<Vec<Kont>>,
}

/// A thread other than the main one, as C sees it.
#[derive(Debug)]
struct ThreadState {
    thread: ThreadRef,
    l: *mut LState,
    /// Where its stack starts for C, when the VM can't tell: after it died
    /// with an error, just below the error C was handed.
    base: Option<usize>,
    /// The status it died with.
    status: c_int,
}

/// What a C function asked for in place of returning.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Request {
    /// Yield its top `n` values; when resumed, continue in `k`, or return
    /// the values the resume passed.
    Yield {
        n: usize,
        k: Option<KFn>,
        kctx: isize,
    },
    /// Call the function below its top `nargs` values, which code may
    /// yield across, then continue in `k`.
    Call {
        nargs: usize,
        nresults: i32,
        protected: bool,
        k: KFn,
        kctx: isize,
    },
}

/// A C continuation waiting on a thread, for the C function whose window
/// starts at `at`.
#[derive(Debug, Clone, Copy)]
struct Kont {
    thread: ThreadRef,
    at: usize,
    k: KFn,
    kctx: isize,
    nresults: i32,
}

/// How a call into C ended without an error.
pub(crate) enum COut {
    /// It returned this many results.
    Ret(usize),
    /// It jumped out with a request.
    Suspend(Request),
}

/// Owns an isolate's [`Bridge`] and its C state, as an isolate extension.
#[derive(Debug)]
struct BridgeHandle(*const Bridge);

impl Default for BridgeHandle {
    fn default() -> BridgeHandle {
        BridgeHandle(std::ptr::null())
    }
}

impl Drop for BridgeHandle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: made by `install` with `Box::into_raw`; no C function
            // can be running while the runtime is being dropped.
            unsafe {
                let b = Box::from_raw(self.0 as *mut Bridge);
                for t in b.threads.borrow().values() {
                    orbit_capi_freestate(t.l);
                }
                orbit_capi_freestate(b.l.get());
            }
        }
    }
}

pub(crate) fn install(s: &mut Stack<'_>) {
    let main = s.main_thread();
    let mut ctl = s.control();
    let wake = ctl.interrupt_flag().cast::<u8>();
    if !ctl.extension(|h: &mut BridgeHandle| h.0.is_null()) {
        return;
    }
    let b = Box::into_raw(Box::new(Bridge {
        l: Cell::new(std::ptr::null_mut()),
        ctx: Cell::new(std::ptr::null_mut()),
        st: Shared::default(),
        host: Cell::new(std::ptr::null_mut()),
        isolate: Cell::new(std::ptr::null_mut()),
        debuggable: Cell::new(false),
        hooks: Default::default(),
        hook_ctx: Cell::new(std::ptr::null_mut()),
        main,
        threads: Default::default(),
        by_l: Default::default(),
        request: Cell::new(None),
        konts: Default::default(),
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
    // long as the isolate, which outlives the state.
    unsafe {
        (*b).l.set(l);
        orbit_capi_set_wake(l, wake);
    }
    ctl.extension(|h: &mut BridgeHandle| h.0 = b);
    ctl.set_interrupt_handler::<OnInterrupt>();
    // Modules read the main thread from the registry, as in Lua.
    ctl.publish_main_thread();
}

/// The isolate's bridge, if the C API is installed.
fn bridge<'a>(ctl: &mut Control<'_>) -> Option<&'a Bridge> {
    let h = ctl.extension(|h: &mut BridgeHandle| h.0);
    // SAFETY: the handle keeps the bridge alive as long as the isolate.
    (!h.is_null()).then(|| unsafe { &*h })
}

/// Run `call` on the C state with the native's stack current, as a C
/// function's call does; an error it raised comes back as `Err`.
fn run_c(s: &mut Stack<'_>, call: impl FnOnce(*mut LState) -> c_int) -> Res<COut> {
    let Some(b) = bridge(&mut s.control()) else {
        return Err(s.error("the C API is not installed in this VM"));
    };
    let l = state_for(b, s.running_thread());
    // The native's stack, for the entry points C calls; restored after,
    // so nested C calls each see their own.
    let raw = (s as *mut Stack<'_>).cast::<Stack<'static>>();
    let prev = b.ctx.replace(raw);
    let mark = b.st.borrow().strs.len();
    let n = call(l);
    b.ctx.set(prev);
    b.st.borrow_mut().strs.truncate(mark);
    take_hook(b, &mut s.control(), true);
    if n < 0 {
        if let Some(r) = b.request.take() {
            return Ok(COut::Suspend(r));
        }
        return Err(match b.st.borrow_mut().pending.take() {
            Some(e) => e,
            None => {
                s.push_nil();
                s.error_from_top()
            }
        });
    }
    Ok(COut::Ret(n as usize))
}

/// Call C function `f` with the native's window as its stack.
pub(crate) fn invoke(s: &mut Stack<'_>, f: u64) -> Res<COut> {
    run_c(s, |l| {
        // SAFETY: `f` came from a C module as a `lua_CFunction`
        // (lua_pushcclosure or dlsym); calling it is what loading the
        // module asked for.
        unsafe {
            let cf = std::mem::transmute::<usize, CFn>(f as usize);
            orbit_capi_trampoline(l, cf)
        }
    })
}

/// What the native running a C function does once C is done: return its
/// results, or do what it asked for by jumping out.
pub(crate) fn finish(s: &mut Stack<'_>, out: COut) -> Res<Return> {
    match out {
        COut::Ret(n) => {
            if n > s.top() as usize {
                return Err(s.error("C function returned more results than its stack holds"));
            }
            // Variables the function marked with lua_toclose close as it
            // returns.
            s.close_all()?;
            Ok(Return::values(n))
        }
        COut::Suspend(Request::Yield { n, k: None, .. }) => Ok(Return::yield_values(n)),
        COut::Suspend(Request::Yield {
            n,
            k: Some(k),
            kctx,
        }) => {
            push_kont(s, k, kctx, 0);
            Ok(Return::yield_then::<YieldK>(n))
        }
        COut::Suspend(Request::Call {
            nargs,
            nresults,
            protected,
            k,
            kctx,
        }) => {
            push_kont(s, k, kctx, nresults);
            Ok(Return::call_then::<CallK>(nargs, protected))
        }
    }
}

/// Keep continuation `k` for the C function running on `s` until the VM
/// comes back to it. Entries of dead threads, and ones left by functions
/// an error unwound at or above this one, go.
fn push_kont(s: &mut Stack<'_>, k: KFn, kctx: isize, nresults: i32) {
    let Some(b) = bridge(&mut s.control()) else {
        return;
    };
    let (thread, at) = (s.running_thread(), s.window_start());
    let mut konts = b.konts.borrow_mut();
    konts.retain(|e| {
        !(e.thread == thread && e.at >= at)
            && s.thread_is_live(e.thread)
            && s.thread_status(e.thread) != ThreadStatus::Dead
    });
    konts.push(Kont {
        thread,
        at,
        k,
        kctx,
        nresults,
    });
}

/// The continuation [`push_kont`] kept for the C function on `s`.
fn pop_kont(s: &mut Stack<'_>) -> Res<Kont> {
    let (thread, at) = (s.running_thread(), s.window_start());
    let found = bridge(&mut s.control()).and_then(|b| {
        let mut konts = b.konts.borrow_mut();
        let i = konts
            .iter()
            .rposition(|e| e.thread == thread && e.at == at)?;
        Some(konts.remove(i))
    });
    found.ok_or_else(|| s.error("C API: a continuation went missing"))
}

/// Resumed after `lua_yieldk`: run the C continuation.
struct YieldK;

impl Continuation for YieldK {
    fn resume(s: &mut Stack<'_>, o: Outcome) -> Res<Return> {
        let kont = pop_kont(s)?;
        if let Outcome::Err(e) = o {
            return Err(e);
        }
        run_k(s, kont, abi::YIELD)
    }
}

/// Back from `lua_callk`'s or `lua_pcallk`'s call: run the C continuation
/// with the results or the error.
struct CallK;

impl Continuation for CallK {
    fn resume(s: &mut Stack<'_>, o: Outcome) -> Res<Return> {
        let kont = pop_kont(s)?;
        let status = match o {
            Outcome::Ok(n) => {
                s.adjust_results(n, kont.nresults);
                abi::YIELD
            }
            Outcome::Err(e) => {
                let status = s.status_of(&e) as c_int;
                s.push_raised(&e);
                status
            }
        };
        run_k(s, kont, status)
    }
}

fn run_k(s: &mut Stack<'_>, kont: Kont, status: c_int) -> Res<Return> {
    let out = run_c(s, |l| {
        // SAFETY: `k` came from C as a `lua_KFunction` (lua_yieldk,
        // lua_callk or lua_pcallk) for the function it continues.
        unsafe { orbit_capi_trampoline_k(l, kont.k, status, kont.kctx) }
    })?;
    finish(s, out)
}

/// The state standing for thread `t`, made the first time C sees it.
fn state_for(b: &Bridge, t: ThreadRef) -> *mut LState {
    if b.main == t {
        return b.l.get();
    }
    let mut threads = b.threads.borrow_mut();
    let e = threads.entry(t.slot()).or_insert_with(|| ThreadState {
        thread: t,
        // SAFETY: the main state is live; the new one is freed with the
        // bridge.
        l: unsafe { orbit_capi_newthread(b.l.get()) },
        base: None,
        status: abi::OK,
    });
    assert!(!e.l.is_null(), "out of memory creating a thread's C state");
    if e.thread != t {
        // A new thread in a collected one's slot.
        *e = ThreadState {
            thread: t,
            l: e.l,
            base: None,
            status: abi::OK,
        };
    }
    b.by_l.borrow_mut().insert(e.l as usize, t);
    e.l
}

/// The thread state `l` stands for.
fn thread_of(b: &Bridge, l: *mut LState) -> ThreadRef {
    if l == b.l.get() {
        return b.main;
    }
    match b.by_l.borrow().get(&(l as usize)) {
        Some(&t) => t,
        None => {
            eprintln!("OrbitScript: Lua C API called with an unknown lua_State");
            std::process::abort();
        }
    }
}

/// Where C's view of thread `t`'s stack starts, if the VM can't tell.
fn thread_base(b: &Bridge, t: ThreadRef) -> Option<usize> {
    b.threads
        .borrow()
        .get(&t.slot())
        .filter(|e| e.thread == t)
        .and_then(|e| e.base)
}

/// Set what [`ThreadState`] keeps for `t` after a resume or close.
fn set_thread(b: &Bridge, t: ThreadRef, base: Option<usize>, status: c_int) {
    if let Some(e) = b.threads.borrow_mut().get_mut(&t.slot()) {
        if e.thread == t {
            e.base = base;
            e.status = status;
        }
    }
}

/// Install the hook `lua_sethook` asked for, if it asked since the last
/// look. `clear` lowers the interrupt flag `lua_sethook` raised, which the
/// interrupt handler has already lowered. Returns whether it asked.
fn take_hook(b: &Bridge, ctl: &mut Control<'_>, clear: bool) -> bool {
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
        ctl.clear_interrupt();
    }
    let func = f.map(|f| {
        let key = f as usize;
        let mut hooks = b.hooks.borrow_mut();
        match hooks.iter().find(|(k, _)| *k == key) {
            Some(&(_, v)) => v,
            None => {
                let v = ctl.register_native::<CHook>([key as u64, 0]);
                hooks.push((key, v));
                v
            }
        }
    });
    // Lua's mask bits are the VM's (call 1, return 2, line 4, count 8).
    ctl.set_hook(func, (mask & 0xf) as u8, count.into());
    true
}

/// The isolate's interrupt handler: a `lua_sethook` from a signal handler
/// raised the flag.
struct OnInterrupt;

impl InterruptHandler for OnInterrupt {
    fn handle(ctl: &mut Control<'_>) -> bool {
        match bridge(ctl) {
            Some(b) => take_hook(b, ctl, false),
            None => false,
        }
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
struct CHook;

impl Native for CHook {
    const NAME: &'static str = "hook";

    fn call(s: &mut Stack<'_>) -> Res<Return> {
        let f = s.native_data()[0] as usize;
        let event = match s.string_at(1).as_deref() {
            Some(b"call") => abi::HOOKCALL,
            Some(b"return") => abi::HOOKRET,
            Some(b"line") => abi::HOOKLINE,
            Some(b"count") => abi::HOOKCOUNT,
            Some(b"tail call") => abi::HOOKTAILCALL,
            _ => return Ok(Return::values(0)),
        };
        let line = match s.type_at(2) {
            Type::Number if s.is_integer(2) => s.to_integer(2).unwrap_or(-1) as c_int,
            _ => -1,
        };
        let raw = (s as *mut Stack<'_>).cast::<Stack<'static>>();
        let prev = bridge(&mut s.control()).map(|b| b.hook_ctx.replace(raw));
        let r = run_c(s, |l| {
            // SAFETY: `f` came from C as a `lua_Hook` (lua_sethook). From
            // the hook's native, level 1 is the hooked function;
            // lua_getinfo takes the level plus one.
            unsafe {
                let hf = std::mem::transmute::<usize, HookFn>(f);
                orbit_capi_call_hook(l, hf, event, line, 2)
            }
        });
        if let (Some(b), Some(p)) = (bridge(&mut s.control()), prev) {
            b.hook_ctx.set(p);
        }
        if let COut::Suspend(_) = r? {
            return Err(s.error("attempt to yield from a C hook"));
        }
        Ok(Return::values(0))
    }
}

/// The stack and bridge of the C function running on state `l`. Aborts
/// if none is (C code kept `L` and used it after its call returned).
///
/// # Safety
/// `l` is a state made by [`install`].
unsafe fn enter<'a>(l: *mut LState, here: bool) -> (&'a Bridge, &'a mut Stack<'static>, Window) {
    // SAFETY: per the contract; the bridge lives as long as the state.
    let b = unsafe { &*((*l).rs as *const Bridge) };
    let c = b.ctx.get();
    if c.is_null() {
        eprintln!("OrbitScript: Lua C API called outside a C function call");
        std::process::abort();
    }
    // SAFETY: set by `run_c` from a live `&mut Stack` that is not used
    // again until the C function returns.
    let c = unsafe { &mut *c };
    take_hook(b, &mut c.control(), true);
    let other = if here { None } else { foreign(b, c, l) };
    let window = other.map(|t| (t, c.enter_thread(t, thread_base(b, t))));
    (b, c, window)
}

/// The other thread an entry point's body works on, if any.
type Window = Option<(ThreadRef, ThreadWindow)>;

/// The end of an entry point that [`enter`] began.
fn leave(b: &Bridge, c: &mut Stack<'static>, window: Window) {
    if let Some((t, saved)) = window {
        c.leave_thread(t, saved);
    }
    host_prune(b, c);
}

/// The thread `l` stands for, if it isn't the running one.
fn foreign(b: &Bridge, c: &Stack<'_>, l: *mut LState) -> Option<ThreadRef> {
    let running = c.running_thread();
    if l == b.l.get() && b.main == running {
        return None;
    }
    let t = thread_of(b, l);
    (t != running).then_some(t)
}

/// Leave a request for the C function running on `l`, which jumps out of
/// it once the entry point returns.
fn suspend(l: *mut LState, req: Request) {
    // SAFETY: `l` is the live state the entry point was called with.
    unsafe {
        let b = &*((*l).rs as *const Bridge);
        b.request.set(Some(req));
        (*l).status = 1;
    }
}

fn fail(l: *mut LState, b: &Bridge, e: Raised) {
    b.st.borrow_mut().pending = Some(e);
    // SAFETY: `l` is the live state the entry point was called with.
    unsafe { (*l).status = 1 };
}

/// Run `body` as an entry point; on error, leave it pending and return
/// `default`.
///
/// The body sees the stack of the thread `$l` stands for; with `here`, the
/// running native's, for entry points that reach other threads themselves.
macro_rules! entry {
    ($l:expr, $default:expr, |$b:ident, $c:ident| $body:expr) => {
        entry!(@ false, $l, $default, |$b, $c| $body)
    };
    (here $l:expr, $default:expr, |$b:ident, $c:ident| $body:expr) => {
        entry!(@ true, $l, $default, |$b, $c| $body)
    };
    (@ $here:expr, $l:expr, $default:expr, |$b:ident, $c:ident| $body:expr) => {{
        // SAFETY: `$l` came from C as the state of a running call.
        let ($b, $c, window) = unsafe { enter($l, $here) };
        #[allow(clippy::redundant_closure_call)]
        let r: Res<_> = (|| $body)();
        leave($b, $c, window);
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
fn host_prune(b: &Bridge, c: &mut Stack<'static>) {
    let host = b.host.get();
    if host.is_null() || !std::ptr::eq(b.ctx.get(), host) {
        return;
    }
    c.safepoint();
    {
        let st = b.st.borrow();
        if st.strs.len() < 2 * st.kept + 16 {
            return;
        }
    }
    // Strings on the stacks of the threads C has seen count too.
    let threads: Vec<(ThreadRef, Option<usize>)> = b
        .threads
        .borrow()
        .values()
        .filter(|e| {
            c.thread_is_live(e.thread) && c.thread_status(e.thread) != ThreadStatus::Running
        })
        .map(|e| (e.thread, e.base))
        .collect();
    let mut texts: Vec<Vec<u8>> = Vec::new();
    let mut collect = |c: &mut Stack<'_>| {
        for i in 1..=c.top() {
            if let Some(s) = c.string_at(i) {
                texts.push(s.to_vec());
            }
        }
    };
    collect(c);
    for (t, base) in threads {
        c.on_thread(t, base, &mut collect);
    }
    let texts: Vec<&[u8]> = texts.iter().map(Vec::as_slice).collect();
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
    callk: extern "C" fn(*mut LState, c_int, c_int, isize, Option<KFn>),
    checkstack: extern "C" fn(*mut LState, c_int) -> c_int,
    close: extern "C" fn(*mut LState),
    closeslot: extern "C" fn(*mut LState, c_int),
    closethread: extern "C" fn(*mut LState, *mut LState) -> c_int,
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
    isrustudata: extern "C" fn(*mut LState, c_int) -> c_int,
    isstring: extern "C" fn(*mut LState, c_int) -> c_int,
    isuserdata: extern "C" fn(*mut LState, c_int) -> c_int,
    isyieldable: extern "C" fn(*mut LState) -> c_int,
    len: extern "C" fn(*mut LState, c_int),
    load: extern "C" fn(*mut LState, *const c_char, usize, *const c_char, *const c_char) -> c_int,
    newthread: extern "C" fn(*mut LState) -> *mut LState,
    newuserdatauv: extern "C" fn(*mut LState, usize, c_int) -> *mut c_void,
    next: extern "C" fn(*mut LState, c_int) -> c_int,
    numbertocstring: extern "C" fn(*mut LState, c_int, *mut c_char) -> u32,
    openlib: extern "C" fn(*mut LState, c_int) -> c_int,
    pcallk: extern "C" fn(*mut LState, c_int, c_int, c_int, isize, Option<KFn>) -> c_int,
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
    resume: extern "C" fn(*mut LState, *mut LState, c_int, *mut c_int) -> c_int,
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
    status: extern "C" fn(*mut LState) -> c_int,
    stringtonumber: extern "C" fn(*mut LState, *const c_char) -> usize,
    toboolean: extern "C" fn(*mut LState, c_int) -> c_int,
    tocfunction: extern "C" fn(*mut LState, c_int) -> Option<CFn>,
    toclose: extern "C" fn(*mut LState, c_int),
    tointegerx: extern "C" fn(*mut LState, c_int, *mut c_int) -> i64,
    tolstring: extern "C" fn(*mut LState, c_int, *mut usize) -> *const c_char,
    tolstringmeta: extern "C" fn(*mut LState, c_int, *mut usize) -> *const c_char,
    tonumberx: extern "C" fn(*mut LState, c_int, *mut c_int) -> f64,
    topointer: extern "C" fn(*mut LState, c_int) -> *const c_void,
    tothread: extern "C" fn(*mut LState, c_int) -> *mut LState,
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
    xmove: extern "C" fn(*mut LState, *mut LState, c_int),
    yieldk: extern "C" fn(*mut LState, c_int, isize, Option<KFn>),
}

static TABLE: RTable = RTable {
    absindex: orbit_r_absindex,
    argerror: orbit_r_argerror,
    arith: orbit_r_arith,
    callk: orbit_r_callk,
    checkstack: orbit_r_checkstack,
    close: orbit_r_close,
    closeslot: orbit_r_closeslot,
    closethread: orbit_r_closethread,
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
    isrustudata: orbit_r_isrustudata,
    isstring: orbit_r_isstring,
    isuserdata: orbit_r_isuserdata,
    isyieldable: orbit_r_isyieldable,
    len: orbit_r_len,
    load: orbit_r_load,
    newthread: orbit_r_newthread,
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
    resume: orbit_r_resume,
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
    status: orbit_r_status,
    stringtonumber: orbit_r_stringtonumber,
    toboolean: orbit_r_toboolean,
    tocfunction: orbit_r_tocfunction,
    toclose: orbit_r_toclose,
    tointegerx: orbit_r_tointegerx,
    tolstring: orbit_r_tolstring,
    tolstringmeta: orbit_r_tolstringmeta,
    tonumberx: orbit_r_tonumberx,
    topointer: orbit_r_topointer,
    tothread: orbit_r_tothread,
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
    xmove: orbit_r_xmove,
    yieldk: orbit_r_yieldk,
};

// ---- entry points (names match csrc/capi.c) ---------------------------------

extern "C" fn orbit_r_version(l: *mut LState) -> f64 {
    entry!(l, 0.0, |_b, _c| Ok(505.0))
}

extern "C" fn orbit_r_absindex(l: *mut LState, idx: c_int) -> c_int {
    entry!(l, 0, |_b, c| Ok(c.abs_index(idx)))
}

extern "C" fn orbit_r_gettop(l: *mut LState) -> c_int {
    entry!(l, 0, |_b, c| Ok(c.top()))
}

extern "C" fn orbit_r_settop(l: *mut LState, idx: c_int) {
    entry!(l, (), |_b, c| c.set_top(idx))
}

extern "C" fn orbit_r_pushvalue(l: *mut LState, idx: c_int) {
    entry!(l, (), |_b, c| {
        c.push_copy(idx);
        Ok(())
    })
}

extern "C" fn orbit_r_rotate(l: *mut LState, idx: c_int, n: c_int) {
    entry!(l, (), |_b, c| c.rotate(idx, n))
}

extern "C" fn orbit_r_copy(l: *mut LState, from: c_int, to: c_int) {
    entry!(l, (), |_b, c| c.copy(from, to))
}

extern "C" fn orbit_r_checkstack(l: *mut LState, n: c_int) -> c_int {
    entry!(l, 0, |_b, c| Ok(c.check_room(n) as c_int))
}

extern "C" fn orbit_r_isnumber(l: *mut LState, idx: c_int) -> c_int {
    entry!(l, 0, |_b, c| Ok(c.to_number(idx).is_some() as c_int))
}

extern "C" fn orbit_r_isstring(l: *mut LState, idx: c_int) -> c_int {
    entry!(l, 0, |_b, c| Ok(c.is_string(idx) as c_int))
}

extern "C" fn orbit_r_iscfunction(l: *mut LState, idx: c_int) -> c_int {
    entry!(l, 0, |_b, c| Ok(c.is_native_function(idx) as c_int))
}

extern "C" fn orbit_r_isinteger(l: *mut LState, idx: c_int) -> c_int {
    entry!(l, 0, |_b, c| Ok(c.is_integer(idx) as c_int))
}

extern "C" fn orbit_r_isuserdata(l: *mut LState, idx: c_int) -> c_int {
    entry!(l, 0, |_b, c| Ok(c.is_userdata(idx) as c_int))
}

extern "C" fn orbit_r_type(l: *mut LState, idx: c_int) -> c_int {
    entry!(l, 0, |_b, c| Ok(api::type_code(c.type_at(idx))))
}

extern "C" fn orbit_r_tonumberx(l: *mut LState, idx: c_int, isnum: *mut c_int) -> f64 {
    entry!(l, 0.0, |_b, c| {
        let n = c.to_number(idx);
        set_flag(isnum, n.is_some());
        Ok(n.unwrap_or(0.0))
    })
}

extern "C" fn orbit_r_tointegerx(l: *mut LState, idx: c_int, isnum: *mut c_int) -> i64 {
    entry!(l, 0, |_b, c| {
        let n = c.to_integer(idx);
        set_flag(isnum, n.is_some());
        Ok(n.unwrap_or(0))
    })
}

extern "C" fn orbit_r_toboolean(l: *mut LState, idx: c_int) -> c_int {
    entry!(l, 0, |_b, c| Ok(c.to_boolean(idx) as c_int))
}

extern "C" fn orbit_r_tolstring(l: *mut LState, idx: c_int, len: *mut usize) -> *const c_char {
    entry!(l, std::ptr::null(), |b, c| {
        Ok(match c.to_str(idx)? {
            Some(s) => hand_out_value(&b.st, &s, len),
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
    entry!(l, 0, |_b, c| Ok(c.raw_len(idx)))
}

extern "C" fn orbit_r_tocfunction(l: *mut LState, idx: c_int) -> Option<CFn> {
    entry!(l, None, |_b, c| Ok(c
        .closure_data_of::<crate::CFunction>(idx)
        .map(|f| {
            // SAFETY: the address came from C as a `lua_CFunction`
            // (lua_pushcclosure), kept where scripts can't change it.
            unsafe { std::mem::transmute::<usize, CFn>(f as usize) }
        })))
}

extern "C" fn orbit_r_touserdata(l: *mut LState, idx: c_int) -> *mut c_void {
    entry!(l, std::ptr::null_mut(), |_b, c| Ok(c.to_userdata(idx)))
}

extern "C" fn orbit_r_topointer(l: *mut LState, idx: c_int) -> *const c_void {
    entry!(l, std::ptr::null(), |_b, c| Ok(
        c.to_pointer(idx) as *const c_void
    ))
}

extern "C" fn orbit_r_rawequal(l: *mut LState, a: c_int, b_: c_int) -> c_int {
    entry!(l, 0, |_b, c| Ok(c.raw_equal(a, b_) as c_int))
}

extern "C" fn orbit_r_compare(l: *mut LState, a: c_int, b_: c_int, op: c_int) -> c_int {
    entry!(l, 0, |_b, c| api::compare(c, a, b_, op).map(|r| r as c_int))
}

extern "C" fn orbit_r_pushnil(l: *mut LState) {
    entry!(l, (), |_b, c| {
        c.push_nil();
        Ok(())
    })
}

extern "C" fn orbit_r_pushnumber(l: *mut LState, n: f64) {
    entry!(l, (), |_b, c| {
        c.push_number(n);
        Ok(())
    })
}

extern "C" fn orbit_r_pushinteger(l: *mut LState, n: i64) {
    entry!(l, (), |_b, c| {
        c.push_integer(n);
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
        let st = c.push_bytes(bytes);
        Ok(hand_out_value(&b.st, &st, std::ptr::null_mut()))
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
        c.push_boolean(v != 0);
        Ok(())
    })
}

extern "C" fn orbit_r_pushlightuserdata(l: *mut LState, p: *mut c_void) {
    entry!(l, (), |_b, c| {
        c.push_light_userdata(p as usize);
        Ok(())
    })
}

extern "C" fn orbit_r_getglobal(l: *mut LState, name: *const c_char) -> c_int {
    // SAFETY: C passes a NUL-terminated name.
    let name = unsafe { cbytes(name) };
    entry!(l, 0, |_b, c| c.get_global(name).map(api::type_code))
}

extern "C" fn orbit_r_gettable(l: *mut LState, idx: c_int) -> c_int {
    entry!(l, 0, |_b, c| c.get_table(idx).map(api::type_code))
}

extern "C" fn orbit_r_getfield(l: *mut LState, idx: c_int, k: *const c_char) -> c_int {
    // SAFETY: C passes a NUL-terminated key.
    let k = unsafe { cbytes(k) };
    entry!(l, 0, |b, c| { c.get_field(idx, k).map(api::type_code) })
}

extern "C" fn orbit_r_geti(l: *mut LState, idx: c_int, n: i64) -> c_int {
    entry!(l, 0, |b, c| { c.get_index(idx, n).map(api::type_code) })
}

extern "C" fn orbit_r_rawget(l: *mut LState, idx: c_int) -> c_int {
    entry!(l, 0, |_b, c| c.raw_get(idx).map(api::type_code))
}

extern "C" fn orbit_r_rawgeti(l: *mut LState, idx: c_int, n: i64) -> c_int {
    entry!(l, 0, |_b, c| c.raw_get_index(idx, n).map(api::type_code))
}

extern "C" fn orbit_r_rawgetp(l: *mut LState, idx: c_int, p: *const c_void) -> c_int {
    entry!(l, 0, |_b, c| c
        .raw_get_light(idx, p as usize)
        .map(api::type_code))
}

extern "C" fn orbit_r_createtable(l: *mut LState, narr: c_int, nrec: c_int) {
    entry!(l, (), |_b, c| {
        c.create_table(narr, nrec);
        Ok(())
    })
}

extern "C" fn orbit_r_newuserdatauv(l: *mut LState, size: usize, nuv: c_int) -> *mut c_void {
    entry!(l, std::ptr::null_mut(), |_b, c| c.new_userdata(size, nuv))
}

extern "C" fn orbit_r_getmetatable(l: *mut LState, idx: c_int) -> c_int {
    entry!(l, 0, |_b, c| Ok(c.get_metatable(idx) as c_int))
}

extern "C" fn orbit_r_getiuservalue(l: *mut LState, idx: c_int, n: c_int) -> c_int {
    entry!(l, abi::TNONE, |_b, c| Ok(api::type_code(
        c.get_user_value(idx, n)
    )))
}

extern "C" fn orbit_r_setglobal(l: *mut LState, name: *const c_char) {
    // SAFETY: C passes a NUL-terminated name.
    let name = unsafe { cbytes(name) };
    entry!(l, (), |b, c| c.set_global(name))
}

extern "C" fn orbit_r_settable(l: *mut LState, idx: c_int) {
    entry!(l, (), |b, c| c.set_table(idx))
}

extern "C" fn orbit_r_setfield(l: *mut LState, idx: c_int, k: *const c_char) {
    // SAFETY: C passes a NUL-terminated key.
    let k = unsafe { cbytes(k) };
    entry!(l, (), |b, c| c.set_field(idx, k))
}

extern "C" fn orbit_r_seti(l: *mut LState, idx: c_int, n: i64) {
    entry!(l, (), |b, c| { c.set_index_int(idx, n) })
}

extern "C" fn orbit_r_rawset(l: *mut LState, idx: c_int) {
    entry!(l, (), |_b, c| c.raw_set(idx))
}

extern "C" fn orbit_r_rawseti(l: *mut LState, idx: c_int, n: i64) {
    entry!(l, (), |_b, c| c.raw_set_index(idx, n))
}

extern "C" fn orbit_r_rawsetp(l: *mut LState, idx: c_int, p: *const c_void) {
    entry!(l, (), |_b, c| c.raw_set_light(idx, p as usize))
}

extern "C" fn orbit_r_setmetatable(l: *mut LState, idx: c_int) -> c_int {
    entry!(l, 0, |_b, c| c.set_metatable(idx).map(|()| 1))
}

extern "C" fn orbit_r_setiuservalue(l: *mut LState, idx: c_int, n: c_int) -> c_int {
    entry!(l, 0, |_b, c| c.set_user_value(idx, n).map(c_int::from))
}

/// `lua_callk`: with a continuation, where the thread can yield, the C
/// function leaves and the continuation gets the results (with
/// `LUA_YIELD`, whether or not the call yielded).
extern "C" fn orbit_r_callk(
    l: *mut LState,
    nargs: c_int,
    nresults: c_int,
    kctx: isize,
    k: Option<KFn>,
) {
    let req = entry!(l, None, |b, c| {
        if let Some(k) = k {
            if can_continue(b, c, l) {
                let nargs = c.check_call(nargs)?;
                return Ok(Some(Request::Call {
                    nargs,
                    nresults,
                    protected: false,
                    k,
                    kctx,
                }));
            }
        }
        c.call(nargs, nresults).map(|()| None)
    });
    if let Some(r) = req {
        suspend(l, r);
    }
}

/// `lua_pcallk`: as [`orbit_r_callk`] for calls without a message
/// handler; the continuation also gets an error's status.
extern "C" fn orbit_r_pcallk(
    l: *mut LState,
    nargs: c_int,
    nresults: c_int,
    errfunc: c_int,
    kctx: isize,
    k: Option<KFn>,
) -> c_int {
    let req = entry!(l, Err(abi::ERRRUN), |b, c| {
        if let Some(k) = k {
            if errfunc == 0 && can_continue(b, c, l) {
                let nargs = c.check_call(nargs)?;
                return Ok(Ok(Request::Call {
                    nargs,
                    nresults,
                    protected: true,
                    k,
                    kctx,
                }));
            }
        }
        c.pcall(nargs, nresults, errfunc).map(|s| Err(s as c_int))
    });
    match req {
        Ok(r) => {
            suspend(l, r);
            abi::OK
        }
        Err(status) => status,
    }
}

extern "C" fn orbit_r_error(l: *mut LState) -> c_int {
    entry!(l, 0, |_b, c| Err::<c_int, _>(c.error_from_top()))
}

extern "C" fn orbit_r_next(l: *mut LState, idx: c_int) -> c_int {
    entry!(l, 0, |_b, c| c.next(idx).map(|r| r as c_int))
}

extern "C" fn orbit_r_concat(l: *mut LState, n: c_int) {
    entry!(l, (), |_b, c| c.concat(n))
}

extern "C" fn orbit_r_len(l: *mut LState, idx: c_int) {
    entry!(l, (), |b, c| c.len(idx))
}

extern "C" fn orbit_r_stringtonumber(l: *mut LState, s: *const c_char) -> usize {
    // SAFETY: C passes a NUL-terminated string.
    let s = unsafe { cbytes(s) };
    entry!(l, 0, |_b, c| Ok(c.push_numeral(s)))
}

extern "C" fn orbit_r_gc(l: *mut LState, what: c_int, a: c_int, b: c_int) -> c_int {
    entry!(l, 0, |_b, c| Ok(api::gc(c, what, a, b)))
}

extern "C" fn orbit_r_argerror(l: *mut LState, arg: c_int, msg: *const c_char) {
    // SAFETY: C passes a NUL-terminated message, or NULL.
    let msg = OrbitString::from(unsafe { cbytes(msg) });
    entry!(l, (), |_b, c| Err(c.arg_error(arg, &msg)))
}

extern "C" fn orbit_r_typeerror(l: *mut LState, arg: c_int, tname: *const c_char) {
    // SAFETY: C passes a NUL-terminated type name.
    let tname = String::from_utf8_lossy(unsafe { cbytes(tname) }).into_owned();
    entry!(l, (), |_b, c| Err(c.type_error(arg, &tname)))
}

extern "C" fn orbit_r_where(l: *mut LState, lvl: c_int) {
    entry!(l, (), |b, c| {
        c.push_where(lua_level(b, lvl));
        Ok(())
    })
}

extern "C" fn orbit_r_tolstringmeta(l: *mut LState, idx: c_int, len: *mut usize) -> *const c_char {
    entry!(l, std::ptr::null(), |b, c| {
        let s = c.to_display_string(idx)?;
        Ok(hand_out_value(&b.st, &s, len))
    })
}

extern "C" fn orbit_r_arith(l: *mut LState, op: c_int) {
    entry!(l, (), |_b, c| api::arith(c, op))
}

extern "C" fn orbit_r_numbertocstring(l: *mut LState, idx: c_int, buff: *mut c_char) -> u32 {
    entry!(l, 0, |_b, c| Ok(match c.number_text(idx) {
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
    entry!(l, abi::ERRSYNTAX, |_b, c| Ok(
        c.load(bytes, name, mode) as c_int
    ))
}

extern "C" fn orbit_r_isrustudata(l: *mut LState, idx: c_int) -> c_int {
    entry!(l, 0, |_b, c| Ok(c.is_host_userdata(idx) as c_int))
}

extern "C" fn orbit_r_warning(l: *mut LState, msg: *const c_char, tocont: c_int) {
    // SAFETY: NUL-terminated.
    let msg = unsafe { cbytes(msg) };
    entry!(l, (), |_b, c| {
        c.warn(msg, tocont != 0);
        Ok(())
    })
}

extern "C" fn orbit_r_traceback(l: *mut LState, msg: *const c_char, level: c_int) {
    // SAFETY: NUL-terminated or NULL.
    let text = unsafe { cbytes(msg) };
    let msg = (!msg.is_null()).then_some(OrbitStr::from(text));
    entry!(l, (), |b, c| {
        c.push_traceback(msg, lua_level(b, level));
        Ok(())
    })
}

extern "C" fn orbit_r_pushthread(l: *mut LState) -> c_int {
    entry!(l, 0, |b, c| {
        let t = thread_of(b, l);
        c.push_thread(t);
        Ok((b.main == t) as c_int)
    })
}

// ---- threads ----------------------------------------------------------------

extern "C" fn orbit_r_newthread(l: *mut LState) -> *mut LState {
    entry!(l, std::ptr::null_mut(), |b, c| {
        let t = c.push_new_thread();
        Ok(state_for(b, t))
    })
}

extern "C" fn orbit_r_tothread(l: *mut LState, idx: c_int) -> *mut LState {
    entry!(l, std::ptr::null_mut(), |b, c| Ok(match c.to_thread(idx) {
        Some(t) => state_for(b, t),
        None => std::ptr::null_mut(),
    }))
}

extern "C" fn orbit_r_status(l: *mut LState) -> c_int {
    entry!(here l, abi::OK, |b, c| {
        let t = thread_of(b, l);
        Ok(if c.thread_yielded(t) {
            abi::YIELD
        } else if c.thread_failed(t) {
            b.threads
                .borrow()
                .get(&t.slot())
                .filter(|e| e.thread == t)
                .map_or(abi::ERRRUN, |e| e.status)
        } else {
            abi::OK
        })
    })
}

extern "C" fn orbit_r_isyieldable(l: *mut LState) -> c_int {
    entry!(here l, 0, |b, c| Ok(c.thread_yieldable(thread_of(b, l)) as c_int))
}

/// Move the top `n` values of `from`'s stack to `to`'s.
extern "C" fn orbit_r_xmove(from: *mut LState, to: *mut LState, n: c_int) {
    entry!(here from, (), |b, c| {
        let n = usize::try_from(n).unwrap_or(0);
        if from == to || n == 0 {
            return Ok(());
        }
        let (tf, tt) = (thread_of(b, from), thread_of(b, to));
        c.move_values((tf, thread_base(b, tf)), (tt, thread_base(b, tt)), n);
        Ok(())
    })
}

/// Resume the thread `l` stands for with the top `narg` values of its
/// stack; what it yields or returns, or its error, goes on its stack.
extern "C" fn orbit_r_resume(
    l: *mut LState,
    _from: *mut LState,
    narg: c_int,
    nres: *mut c_int,
) -> c_int {
    entry!(here l, abi::ERRRUN, |b, c| {
        let t = thread_of(b, l);
        let narg = usize::try_from(narg).unwrap_or(0);
        let r = c.resume(t, thread_base(b, t), narg)?;
        let status = r.status as c_int;
        match r.status {
            Status::Ok | Status::Yield => set_thread(b, t, None, abi::OK),
            _ if r.failed_top.is_some() => set_thread(b, t, r.failed_top, status),
            _ => {}
        }
        if !nres.is_null() {
            // SAFETY: C passed a pointer to an int, or NULL.
            unsafe { *nres = r.nresults as c_int };
        }
        Ok(status)
    })
}

/// Close the thread `l` stands for: its pending variables close, and it
/// is left empty for reuse.
extern "C" fn orbit_r_closethread(l: *mut LState, _from: *mut LState) -> c_int {
    entry!(here l, abi::OK, |b, c| {
        let t = thread_of(b, l);
        set_thread(b, t, None, abi::OK);
        Ok(c.close_thread(t) as c_int)
    })
}

/// Whether the running C function can continue in a C continuation after
/// leaving: it runs on `l`'s thread, which can yield where it is.
fn can_continue(b: &Bridge, c: &Stack<'_>, l: *mut LState) -> bool {
    let running = c.running_thread();
    !std::ptr::eq(b.ctx.get(), b.host.get())
        && !std::ptr::eq(b.ctx.get(), b.hook_ctx.get())
        && thread_of(b, l) == running
        && c.thread_yieldable(running)
}

extern "C" fn orbit_r_yieldk(l: *mut LState, nresults: c_int, kctx: isize, k: Option<KFn>) {
    let req = entry!(l, None, |_b, c| {
        let n = usize::try_from(nresults).unwrap_or(0);
        if n > c.top() as usize {
            return Err(c.error("C API: not enough values to yield"));
        }
        Ok(Some(Request::Yield { n, k, kctx }))
    });
    if let Some(r) = req {
        suspend(l, r);
    }
}

extern "C" fn orbit_r_unsupported(l: *mut LState, what: *const c_char) {
    // SAFETY: NUL-terminated.
    let what = String::from_utf8_lossy(unsafe { cbytes(what) }).into_owned();
    entry!(l, (), |_b, c| Err(
        c.error(&format!("{what} is not supported by OrbitScript yet"))
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
        if !c.has_level(level) {
            return Ok(0);
        }
        // SAFETY: C passed a lua_Debug to fill.
        unsafe { (*ar).private = level as usize + 1 };
        Ok(1)
    })
}

extern "C" fn orbit_r_toclose(l: *mut LState, idx: c_int) {
    entry!(l, (), |_b, c| c.to_close(idx))
}

extern "C" fn orbit_r_closeslot(l: *mut LState, idx: c_int) {
    entry!(l, (), |_b, c| c.close_slot(idx))
}

/// The level a `lua_Debug` from `lua_getstack` names.
fn debug_level(c: &mut Stack<'_>, ar: *const LuaDebug) -> Res<usize> {
    // SAFETY: C passed a lua_Debug from lua_getstack.
    match unsafe { (*ar).private } {
        0 => Err(c.error("C API: lua_Debug without lua_getstack")),
        p => Ok(p - 1),
    }
}

extern "C" fn orbit_r_getlocal(l: *mut LState, ar: *const LuaDebug, n: c_int) -> *const c_char {
    entry!(l, std::ptr::null(), |b, c| {
        let name = if ar.is_null() {
            c.param_name(n)
        } else {
            let level = debug_level(c, ar)?;
            c.get_local(level, n)
        };
        Ok(name.map_or(std::ptr::null(), |name| {
            hand_out(&b.st, &name, std::ptr::null_mut())
        }))
    })
}

extern "C" fn orbit_r_setlocal(l: *mut LState, ar: *const LuaDebug, n: c_int) -> *const c_char {
    entry!(l, std::ptr::null(), |b, c| {
        let level = debug_level(c, ar)?;
        let name = c.set_local(level, n)?;
        Ok(name.map_or(std::ptr::null(), |name| {
            hand_out(&b.st, &name, std::ptr::null_mut())
        }))
    })
}

/// A static C string for one of `getinfo`'s fixed words.
fn word(w: &str) -> *const c_char {
    let s: &'static std::ffi::CStr = match w {
        "Lua" => cstr(b"Lua\0"),
        "C" => cstr(b"C\0"),
        "main" => cstr(b"main\0"),
        "global" => cstr(b"global\0"),
        "local" => cstr(b"local\0"),
        "method" => cstr(b"method\0"),
        "field" => cstr(b"field\0"),
        "upvalue" => cstr(b"upvalue\0"),
        "constant" => cstr(b"constant\0"),
        "for iterator" => cstr(b"for iterator\0"),
        "metamethod" => cstr(b"metamethod\0"),
        "hook" => cstr(b"hook\0"),
        _ => cstr(b"\0"),
    };
    s.as_ptr()
}

extern "C" fn orbit_r_getinfo(l: *mut LState, what: *const c_char, ar: *mut LuaDebug) -> c_int {
    // SAFETY: NUL-terminated.
    let opts = unsafe { cbytes(what) };
    entry!(l, 0, |b, c| {
        // With '>', the function stays on top until the strings are made.
        let (opts, level) = if opts.first() == Some(&b'>') {
            if c.top() == 0 {
                return Err(c.error("C API: function expected for '>'"));
            }
            (&opts[1..], None)
        } else {
            // SAFETY: C passed a lua_Debug from lua_getstack.
            let private = unsafe { (*ar).private };
            if private == 0 {
                return Err(c.error("C API: lua_getinfo without lua_getstack"));
            }
            (opts, Some(private - 1))
        };
        let Some(info) = (match level {
            Some(n) => c.level_info(n),
            None => Some(c.function_info(-1)),
        }) else {
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
        // '>' pops the function, which 'f' pushes back.
        match (level, opts.contains(&b'f')) {
            (None, false) => c.set_top(-2)?,
            (None, true) => {}
            (Some(n), true) => c.push_level_function(n),
            (Some(_), false) => {}
        }
        if opts.contains(&b'L') {
            c.push_nil();
        }
        Ok(1)
    })
}

extern "C" fn orbit_r_getupvalue(l: *mut LState, funcindex: c_int, n: c_int) -> *const c_char {
    entry!(l, std::ptr::null(), |b, c| Ok(
        match c.get_upvalue(funcindex, n) {
            Some(name) => hand_out(&b.st, &name, std::ptr::null_mut()),
            None => std::ptr::null(),
        }
    ))
}

extern "C" fn orbit_r_setupvalue(l: *mut LState, funcindex: c_int, n: c_int) -> *const c_char {
    entry!(l, std::ptr::null(), |b, c| Ok(
        match c.set_upvalue(funcindex, n)? {
            Some(name) => hand_out(&b.st, &name, std::ptr::null_mut()),
            None => std::ptr::null(),
        }
    ))
}

extern "C" fn orbit_r_upvalueid(l: *mut LState, funcindex: c_int, n: c_int) -> *mut c_void {
    entry!(l, std::ptr::null_mut(), |_b, c| Ok(
        c.upvalue_id(funcindex, n) as *mut c_void
    ))
}

extern "C" fn orbit_r_upvaluejoin(l: *mut LState, f1: c_int, n1: c_int, f2: c_int, n2: c_int) {
    entry!(l, (), |_b, c| c.join_upvalues(f1, n1, f2, n2))
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

    extern "C" {
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

/// The [`crate::LoadFn`] for this crate. Libraries stay loaded
/// until the process exits, as their functions may be referenced anywhere.
#[cfg(unix)]
pub(crate) fn load(
    s: &mut Stack<'_>,
    path: &[u8],
    sym: &[u8],
) -> Result<(), (String, &'static str)> {
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
        s.push_boolean(true);
        return Ok(());
    }
    let csym =
        std::ffi::CString::new(sym).map_err(|_| ("symbol contains a NUL byte".into(), "init"))?;
    // SAFETY: `h` is a live handle; the name is NUL-terminated.
    let f = unsafe { dl::dlsym(h, csym.as_ptr()) };
    if f.is_null() {
        return Err((dl_error(), "init"));
    }
    crate::push_c_function(s, f as usize as u64, 0)
        .map_err(|_| ("not enough memory".into(), "init"))
}

#[cfg(not(unix))]
pub(crate) fn load(
    _s: &mut Stack<'_>,
    _path: &[u8],
    _sym: &[u8],
) -> Result<(), (String, &'static str)> {
    Err((
        "dynamic libraries not supported on this platform yet".into(),
        "absent",
    ))
}

// ---- states created from C (ADR-10) ---------------------------------------------

/// `luaopen_*`.
extern "C" fn orbit_r_openlib(l: *mut LState, lib: c_int) -> c_int {
    entry!(l, 0, |_b, c| api::openlib(c, lib))
}

/// Push the pending error, for a panic function to read.
extern "C" fn orbit_r_pushpending(l: *mut LState) {
    entry!(l, (), |b, c| {
        match b.st.borrow_mut().pending.take() {
            Some(e) => c.push_raised(&e),
            None => c.push_nil(),
        }
        Ok(())
    })
}

type WarnFn = unsafe extern "C" fn(ud: *mut c_void, msg: *const c_char, tocont: c_int);

/// `lua_setwarnf`: warnings go to `f` from now on; a null `f` drops them.
extern "C" fn orbit_r_setwarnf(l: *mut LState, f: Option<WarnFn>, ud: *mut c_void) {
    entry!(l, (), |_b, c| {
        c.set_warn_hook(move |msg: OrbitStr<'_>, tocont: bool| {
            let Some(f) = f else { return };
            let mut text = Vec::with_capacity(msg.len() + 1);
            text.extend(msg.iter().map(|&b| if b == 0 { b'?' } else { b }));
            text.push(0);
            // SAFETY: `f` is the program's warning function, called as
            // lua_setwarnf documents, with a NUL-terminated message that
            // lives through the call.
            unsafe { f(ud, text.as_ptr() as *const c_char, tocont as c_int) }
        });
        Ok(())
    })
}

#[cfg(feature = "host")]
extern "C" {
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

/// `orbit_newstate`'s flag for a state no debugger may attach to.
#[cfg(feature = "host")]
const STATE_NO_DEBUGGER: std::ffi::c_uint = 1;

/// `orbit_newstate` (and `lua_newstate`, with no flags): an isolate owned
/// by the C program, with no libraries open, whose stack between calls is
/// the host's window ([`orbit::Isolate::host_stack`]). Null if memory runs
/// out.
#[cfg(feature = "host")]
#[no_mangle]
extern "C" fn orbit_r_newstate(flags: std::ffi::c_uint) -> *mut LState {
    let allowed = flags & STATE_NO_DEBUGGER == 0;
    let debugger = if allowed {
        orbit::Debugger::Allowed
    } else {
        orbit::Debugger::Disabled
    };
    let config = orbit::Config::default().libraries(&[]).debugger(debugger);
    let iso = Box::into_raw(Box::new(orbit::Runtime::new().new_isolate(config)));
    // SAFETY: just made; reached only through the state from now on, and
    // freed by `orbit_r_close`.
    let iso_ref: &'static mut orbit::Isolate = unsafe { &mut *iso };
    iso_ref.set_stdout(CStdout);
    let debuggable = allowed && crate::debug::serve(iso_ref);
    let host: StackPtr = Box::into_raw(Box::new(iso_ref.host_stack()));
    // SAFETY: just made, and not shared yet.
    let s = unsafe { &mut *host };
    crate::install_loader(s);
    let Some(b) = bridge(&mut s.control()) else {
        return std::ptr::null_mut();
    };
    b.ctx.set(host);
    b.host.set(host);
    b.isolate.set(iso);
    b.debuggable.set(debuggable);
    b.l.get()
}

/// `orbit_debug_listen`: start the process's debug server for debuggers
/// to attach to `l` and the program's other states, as
/// [`crate::debug::listen`] answers; -1 if `l` allows no debugger.
#[cfg(feature = "host")]
#[no_mangle]
extern "C" fn orbit_r_debug_listen(l: *mut LState, announce: c_int) -> c_int {
    // SAFETY: `l` is a live state; its bridge lives as long as it does.
    let b = unsafe { &*((*l).rs as *const Bridge) };
    if !b.debuggable.get() {
        return -1;
    }
    crate::debug::listen(announce != 0)
}

/// The program is exiting: remove the debug server's socket.
#[cfg(feature = "host")]
#[no_mangle]
extern "C" fn orbit_r_debug_stop() {
    crate::debug::stop();
}

/// `lua_close`: close the state as Lua does (finalizers run), flush the
/// libraries' files, and free the isolate, its bridge and `l`. Only for
/// states `orbit_r_newstate` made; it does nothing to others.
extern "C" fn orbit_r_close(l: *mut LState) {
    // SAFETY: `l` is a live state; its bridge lives as long as it does.
    let b = unsafe { &*((*l).rs as *const Bridge) };
    let host = b.host.get();
    if host.is_null() || !std::ptr::eq(b.ctx.get(), host) {
        // A state Rust made, or a C function closing its own caller.
        return;
    }
    let iso = b.isolate.replace(std::ptr::null_mut());
    b.ctx.set(std::ptr::null_mut());
    b.host.set(std::ptr::null_mut());
    // SAFETY: both made by `orbit_r_newstate` with `Box::into_raw` and
    // reached only through this state, which the program gives up by
    // calling lua_close; no C function is running. The host stack borrows
    // the isolate, so it goes first. Dropping the isolate closes it and
    // drops the bridge handle, which frees the bridge and `l`.
    unsafe {
        drop(Box::from_raw(host));
        drop(Box::from_raw(iso));
    }
}

/// A C string literal (`c"..."` literals are stable only from Rust 1.77).
fn cstr(bytes: &'static [u8]) -> &'static std::ffi::CStr {
    std::ffi::CStr::from_bytes_with_nul(bytes).expect("one NUL, at the end")
}
