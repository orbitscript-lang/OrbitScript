//! Coroutines (ADR-3, [VM semantics]) and native closures.
//!
//! Every thread owns a [`Stack`]. The running thread's stack lives in
//! `rt.stack`, where the dispatch loop, natives and the register window use
//! it as before; a switch parks it in its [`Thread`] object and moves the
//! target's in. A coroutine's stack starts with a [`FrameKind::CoBase`]
//! frame; its resumer waits in a native continuation frame, so resume,
//! yield, return and errors all come back through the continuation
//! mechanism `pcall` uses.
//!
//! [VM semantics]: https://github.com/tymlipari/OrbitScript/blob/main/docs/design/vm-semantics.md#coroutines

use std::any::Any;
use std::rc::Rc;

use orbit_bytecode::{FunctionName, MULTI, Op};
use orbit_core::table::mark;
use orbit_core::{
    Cell, Closure, Gc, LuaHeap, LuaSpace, NativeId, Proto, ProtoCode, Thread, ThreadState, Value,
};
use orbit_gc::Marker;

use crate::interp::Flow;
use crate::native::{CallOutcome, Continuation, NativeCtx, NativeReturn};
use crate::ops::Exc;
use crate::{Catch, Frame, FrameKind, LuaError, Stack, Vm};

/// Nested resumes allowed before `C stack overflow` (where the reference
/// stops).
pub(crate) const MAX_RESUMES: usize = 196;

/// How many suspended stacks stay at full size (ADR-3): a stack parked
/// longer ago than the last `HOT_STACKS` parks, and not resumed since, is
/// shrunk to its live values.
const HOT_STACKS: usize = 16;

/// A thread's status, as `coroutine.status` reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoStatus {
    Suspended,
    Running,
    /// It resumed another coroutine and waits for it.
    Normal,
    Dead,
}

impl CoStatus {
    pub fn name(self) -> &'static str {
        match self {
            CoStatus::Suspended => "suspended",
            CoStatus::Running => "running",
            CoStatus::Normal => "normal",
            CoStatus::Dead => "dead",
        }
    }
}

/// What the VM keeps in a [`Thread`].
#[derive(Debug)]
pub(crate) struct Coroutine {
    /// The thread's stack while it isn't running (empty while it is).
    stack: Stack,
    /// Its `rt.top` while parked.
    top: usize,
    status: CoStatus,
    /// The error it died with, if any (for `coroutine.close`).
    error: Option<Value>,
    /// What [`ThreadState::extra_bytes`] reports: the parked stack's size.
    bytes: usize,
    /// How many times it has been parked suspended, to tell whether an
    /// entry in `rt.cooling` is still its latest park.
    parks: u32,
    /// `coroutine.close` is running its `__close` handlers: errors unwind
    /// straight to the base frame.
    pub(crate) closing: bool,
    /// `rt.gc_cycles` when it was parked.
    cycle: u32,
}

impl Coroutine {
    /// Where a parked stack's live registers end: its top, or past its
    /// last variable still to close (a dead coroutine kept for
    /// `coroutine.close`).
    fn live(&self) -> usize {
        self.stack
            .tbc
            .last()
            .map_or(self.top, |&r| self.top.max(r as usize + 1))
    }

    /// A suspended coroutine that will call `f`: `f` sits above the base
    /// frame, where the first resume calls it.
    fn fresh(f: Value) -> Coroutine {
        let mut stack = Stack::default();
        stack.regs.push(f);
        stack.frames.push(Frame {
            kind: FrameKind::CoBase,
            func: 0,
            base: 0,
            pc: 0,
            cell_base: 0,
            nvarargs: 0,
            want: MULTI,
            tail: false,
            extra: 0,
        });
        Coroutine {
            bytes: stack.bytes(),
            stack,
            top: 1,
            status: CoStatus::Suspended,
            error: None,
            parks: 0,
            closing: false,
            cycle: 0,
        }
    }

    pub(crate) fn main() -> Coroutine {
        Coroutine {
            stack: Stack::default(),
            top: 0,
            status: CoStatus::Running,
            error: None,
            bytes: 0,
            parks: 0,
            closing: false,
            cycle: 0,
        }
    }
}

impl ThreadState for Coroutine {
    fn trace(&self, m: &mut Marker<'_, LuaSpace>) {
        // The values above a parked stack's top are dead; `switch_to` cuts
        // them off before they can be read again.
        self.stack.trace(m, self.live());
        if let Some(e) = self.error {
            mark(m, e);
        }
    }

    fn extra_bytes(&self) -> usize {
        self.bytes
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

fn co(heap: &LuaHeap, t: Gc<Thread>) -> &Coroutine {
    heap.get(t)
        .state
        .as_any()
        .downcast_ref()
        .expect("threads hold VM coroutines")
}

fn co_mut(heap: &mut LuaHeap, t: Gc<Thread>) -> &mut Coroutine {
    heap.get_mut(t)
        .state
        .as_any_mut()
        .downcast_mut()
        .expect("threads hold VM coroutines")
}

/// The continuation a yielding coroutine waits in: the values the next
/// resume passes become `yield`'s results.
fn yield_k(_ctx: &mut NativeCtx<'_>, o: CallOutcome) -> Result<NativeReturn, LuaError> {
    match o {
        CallOutcome::Ok(n) => Ok(NativeReturn::Return(n)),
        CallOutcome::Err(v) => Err(LuaError::new(v)),
    }
}

impl Vm {
    /// Park the running stack in the current thread, which becomes
    /// `from_status` (or drop the stack, for a coroutine that finished), and
    /// run thread `to`'s.
    fn switch_to(&mut self, to: Gc<Thread>, from_status: CoStatus) {
        let from = self.rt.current;
        let stack = std::mem::take(&mut self.rt.stack);
        let top = self.rt.top;
        // A parked stack is traced through its thread, which the collector
        // may already have scanned.
        self.heap.barrier_back(from);
        let c = co_mut(&mut self.heap, from);
        c.status = from_status;
        // A thread's bytes are its stack's, parked or running: they change
        // only when the stack's capacity does.
        let old = c.bytes;
        // A coroutine that died with variables still to close keeps its
        // stack for `coroutine.close`.
        if from_status == CoStatus::Dead && stack.tbc.is_empty() {
            c.bytes = 0;
            drop(stack);
        } else {
            c.bytes = stack.bytes();
            c.stack = stack;
            c.top = top;
            c.cycle = self.rt.gc_cycles;
        }
        let delta = c.bytes as isize - old as isize;
        c.parks = c
            .parks
            .wrapping_add((from_status == CoStatus::Suspended) as u32);
        let stamp = c.parks;
        if delta != 0 {
            self.heap.adjust_bytes(delta);
        }
        if from_status == CoStatus::Suspended {
            self.rt.cooling.push_back((from, stamp));
            if self.rt.cooling.len() > HOT_STACKS {
                let (cold, stamp) = self.rt.cooling.pop_front().expect("a parked thread");
                self.shrink_parked(cold, stamp);
            }
        }
        let c = co_mut(&mut self.heap, to);
        c.status = CoStatus::Running;
        self.rt.stack = std::mem::take(&mut c.stack);
        self.rt.top = c.top;
        // If a collection swept while it was parked, the dead slots above
        // its top (which aren't traced) may refer to freed objects.
        if c.cycle != self.rt.gc_cycles {
            let live = c.live();
            self.rt.stack.regs.truncate(live);
        }
        self.rt.current = to;
        self.rt.switched = true;
    }

    /// Shrink suspended thread `t`'s stack to its live values, if it hasn't
    /// been resumed since park number `stamp`. A suspended stack keeps at
    /// least one 256-register window while it may be resumed soon; one that
    /// has gone cold costs only what it holds. The copy frees the big
    /// buffer whole, which a shrink in place would leave fragmented.
    #[cold]
    fn shrink_parked(&mut self, t: Gc<Thread>, stamp: u32) {
        let c = co_mut(&mut self.heap, t);
        if c.status != CoStatus::Suspended || c.parks != stamp {
            return;
        }
        let before = c.bytes;
        let s = &mut c.stack;
        s.regs = s.regs[..c.top].to_vec();
        s.cells = s.cells.to_vec();
        s.frames = s.frames.to_vec();
        s.tbc = s.tbc.to_vec();
        c.bytes = s.bytes();
        let delta = c.bytes as isize - before as isize;
        self.heap.adjust_bytes(delta);
    }

    /// Push (or reuse) the continuation frame of native `id`, which waits
    /// for something that isn't a call on this stack (a resume or a
    /// yield); `at` is where the values it gets back will go.
    fn wait_frame(
        &mut self,
        frame: Option<Frame>,
        func: usize,
        want: u8,
        id: NativeId,
        k: Continuation,
        at: usize,
    ) {
        let kind = FrameKind::Native {
            id,
            k,
            catch: Catch::No,
            yieldable: true,
            call_at: at as u32,
        };
        match frame {
            Some(_) => self.rt.stack.frames.last_mut().expect("frame").kind = kind,
            None => {
                let nargs = at - (func + 1);
                self.rt.stack.frames.push(Frame {
                    kind,
                    func: func as u32,
                    base: func as u32 + 1,
                    pc: 0,
                    cell_base: self.rt.stack.cells.len() as u32,
                    nvarargs: nargs as u32,
                    want,
                    tail: false,
                    extra: 0,
                });
            }
        }
    }

    /// Move the last `n` values of the running stack to `xfer`.
    fn take_values(&mut self, n: usize) {
        let at = self.rt.top - n;
        self.rt.xfer.clear();
        self.rt
            .xfer
            .extend_from_slice(&self.rt.stack.regs[at..self.rt.top]);
        self.rt.top = at;
    }

    /// Run the continuation on top of the running stack with an outcome:
    /// `Ok` delivers the values in `xfer`.
    fn deliver(&mut self, outcome: Result<(), Value>) -> Result<Flow, Exc> {
        let f = *self.rt.stack.frames.last().expect("a waiting frame");
        let FrameKind::Native { id, k, call_at, .. } = f.kind else {
            unreachable!("a native waits for every switch")
        };
        let o = match outcome {
            Ok(()) => {
                self.rt.top = self.rt.stack.write(call_at as usize, &self.rt.xfer);
                CallOutcome::Ok(self.rt.xfer.len())
            }
            Err(v) => {
                self.rt.top = call_at as usize;
                CallOutcome::Err(v)
            }
        };
        self.rt.xfer.clear();
        let mut ctx = NativeCtx {
            heap: &mut self.heap,
            rt: &mut self.rt,
            base: f.base as usize,
            nargs: f.nvarargs as usize,
            id,
            upvals: None,
        };
        let r = k(&mut ctx, o)?;
        self.native_returned(r, Some(f), f.func as usize, f.want, id)
    }

    /// `NativeReturn::Resume`: native `id` resumes `co` with the last `nargs`
    /// values of its window and waits in `k`.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn resume(
        &mut self,
        co: Gc<Thread>,
        nargs: usize,
        k: Continuation,
        frame: Option<Frame>,
        func: usize,
        want: u8,
        id: NativeId,
    ) -> Result<Flow, Exc> {
        let at = self.rt.top - nargs;
        self.wait_frame(frame, func, want, id, k, at);
        self.take_values(nargs);
        let refuse = if self.co_status(co) != CoStatus::Suspended {
            Some(if self.co_status(co) == CoStatus::Dead {
                "cannot resume dead coroutine"
            } else {
                "cannot resume non-suspended coroutine"
            })
        } else if self.rt.resumers.len() >= MAX_RESUMES {
            Some("C stack overflow")
        } else {
            None
        };
        if let Some(msg) = refuse {
            let e = self.new_str(msg.as_bytes());
            return self.deliver(Err(e));
        }
        self.rt.resumers.push(self.rt.current);
        self.switch_to(co, CoStatus::Normal);
        let flow = if matches!(
            self.rt.stack.frames.last().map(|f| f.kind),
            Some(FrameKind::CoBase)
        ) {
            // A fresh coroutine: call its body above the base frame.
            self.rt.stack.write(1, &self.rt.xfer);
            let n = self.rt.xfer.len();
            self.rt.xfer.clear();
            self.call_value(0, n, MULTI)
        } else {
            self.deliver(Ok(()))
        };
        switched(flow)
    }

    /// `NativeReturn::Yield`: native `id` yields the last `n` values of its
    /// window to the resumer.
    pub(crate) fn yield_(
        &mut self,
        n: usize,
        frame: Option<Frame>,
        func: usize,
        want: u8,
        id: NativeId,
    ) -> Result<Flow, Exc> {
        let Some(&resumer) = self.rt.resumers.last() else {
            return Err(Exc::Plain(
                "attempt to yield from outside a coroutine".into(),
            ));
        };
        if !yieldable(&self.rt.stack) {
            return Err(Exc::Plain(
                "attempt to yield across a C-call boundary".into(),
            ));
        }
        let at = self.rt.top - n;
        self.wait_frame(frame, func, want, id, yield_k, at);
        self.take_values(n);
        self.rt.resumers.pop();
        self.switch_to(resumer, CoStatus::Suspended);
        switched(self.deliver(Ok(())))
    }

    /// The running coroutine finished: with its results (now in `xfer`), or
    /// with an error. Its stack is dropped and the resumer continues.
    pub(crate) fn co_finished(&mut self, outcome: Result<(), Value>) -> Result<Flow, Exc> {
        let resumer = self.rt.resumers.pop().expect("a coroutine has a resumer");
        let co = self.rt.current;
        // A coroutine being closed has reported its error to the closer.
        let closing = std::mem::take(&mut co_mut(&mut self.heap, co).closing);
        if let Err(e) = outcome
            && !closing
        {
            self.heap.barrier_back(co);
            co_mut(&mut self.heap, co).error = Some(e);
        }
        self.switch_to(resumer, CoStatus::Dead);
        switched(self.deliver(outcome))
    }

    /// `NativeReturn::Close`: native `id` closes coroutine `co`, which is
    /// suspended or dead, and waits for the outcome in `k`.
    pub(crate) fn close_co(
        &mut self,
        co: Gc<Thread>,
        k: Continuation,
        frame: Option<Frame>,
        func: usize,
        want: u8,
        id: NativeId,
    ) -> Result<Flow, Exc> {
        let at = self.rt.top;
        self.wait_frame(frame, func, want, id, k, at);
        self.rt.xfer.clear();
        if self.rt.resumers.len() >= MAX_RESUMES {
            // Closing runs on the coroutine's stack, so it nests like a
            // resume.
            let e = self.new_str(b"C stack overflow");
            return self.deliver(Err(e));
        }
        let c = co_mut(&mut self.heap, co);
        let error = c.error.take();
        if c.stack.tbc.is_empty() {
            c.status = CoStatus::Dead;
            c.stack = Stack::default();
            let freed = std::mem::take(&mut c.bytes);
            self.heap.adjust_bytes(-(freed as isize));
            return self.deliver(error.map_or(Ok(()), Err));
        }
        // Run the handlers on the coroutine's own stack, as an error
        // unwinding to its base frame would.
        c.closing = true;
        self.rt.resumers.push(self.rt.current);
        self.switch_to(co, CoStatus::Normal);
        let e = LuaError::new(error.unwrap_or(Value::Nil));
        switched(self.unwind(e).map_err(Exc::Lua))
    }

    /// The body returned into the coroutine's base frame: its results are
    /// at the bottom of the stack.
    pub(crate) fn co_returned(&mut self) -> Result<Flow, Exc> {
        let top = self.rt.top;
        self.rt.xfer.clear();
        self.rt.xfer.extend_from_slice(&self.rt.stack.regs[..top]);
        self.co_finished(Ok(()))
    }

    pub(crate) fn co_status(&self, t: Gc<Thread>) -> CoStatus {
        if t == self.rt.current {
            CoStatus::Running
        } else {
            co(&self.heap, t).status
        }
    }
}

/// Whether code running on top of `stack` (a coroutine's) may yield:
/// nothing between it and the coroutine's base refuses.
pub(crate) fn yieldable(stack: &Stack) -> bool {
    !stack.frames.iter().any(|f| {
        matches!(
            f.kind,
            FrameKind::Native {
                yieldable: false,
                ..
            } | FrameKind::Handler
                | FrameKind::Boundary
                // A hook is a nested call, as a Lua hook is in the reference.
                | FrameKind::Hook
        )
    })
}

/// After a switch the loop must reload: the frame it had is on another
/// stack.
fn switched(flow: Result<Flow, Exc>) -> Result<Flow, Exc> {
    flow.map(|f| match f {
        Flow::Reload => Flow::Switched,
        f => f,
    })
}

/// Whether the running coroutine is being closed by `coroutine.close`.
pub(crate) fn closing(heap: &LuaHeap, t: Gc<Thread>) -> bool {
    co(heap, t).closing
}

impl NativeCtx<'_> {
    /// Whether thread `t` could yield where it is: it isn't the main thread
    /// and no frame on its stack refuses (`coroutine.isyieldable`).
    pub fn thread_yieldable(&self, t: Gc<Thread>) -> bool {
        if t == self.rt.main_thread {
            return false;
        }
        if t == self.rt.current {
            yieldable(&self.rt.stack)
        } else {
            yieldable(&co(self.heap, t).stack)
        }
    }

    /// A new suspended coroutine running `f`.
    pub fn new_thread(&mut self, f: Value) -> Gc<Thread> {
        let mut c = Coroutine::fresh(f);
        // A new thread gets its creator's hook mask and count, but not its
        // function (observed from lua5.5: `gethook(co)` is `nil, mask,
        // count` and no events reach a hook).
        c.stack.hook = self.rt.stack.hook.inherited();
        self.heap.alloc(Thread::new(c))
    }

    /// Run `f` with thread `t`'s stack in place of the running one, for
    /// the debug library's view of another thread. `f` may read and write
    /// that stack's frames and registers, but must not touch this native's
    /// window or run Lua code. The running thread runs `f` directly.
    pub fn on_thread<R>(&mut self, t: Gc<Thread>, f: impl FnOnce(&mut Self, bool) -> R) -> R {
        if t == self.rt.current {
            return f(self, false);
        }
        let gc_cycles = self.rt.gc_cycles;
        let c = co_mut(self.heap, t);
        // As when resuming it: after a sweep, the untraced slots above the
        // parked top may refer to freed objects.
        if c.cycle != gc_cycles {
            c.stack.regs.truncate(c.top);
        }
        let stack = std::mem::take(&mut c.stack);
        let top = c.top;
        let saved = std::mem::replace(&mut self.rt.stack, stack);
        let saved_top = std::mem::replace(&mut self.rt.top, top);
        let r = f(self, true);
        // `f` may have stored values into the parked stack.
        self.heap.barrier_back(t);
        let c = co_mut(self.heap, t);
        c.stack = std::mem::replace(&mut self.rt.stack, saved);
        c.top = std::mem::replace(&mut self.rt.top, saved_top);
        r
    }

    /// The status of thread `t`.
    pub fn thread_status(&self, t: Gc<Thread>) -> CoStatus {
        if t == self.rt.current {
            CoStatus::Running
        } else {
            co(self.heap, t).status
        }
    }

    /// The main thread.
    pub fn running_main_thread(&self) -> Gc<Thread> {
        self.rt.main_thread
    }

    /// The running thread, and whether it is the main thread.
    pub fn running(&self) -> (Gc<Thread>, bool) {
        (self.rt.current, self.rt.current == self.rt.main_thread)
    }

    // ---- native closures -------------------------------------------------

    /// A function value that runs native `f` (registered as `name`) with
    /// `upvals` as its upvalues ([`NativeCtx::upvalue`]). It is a closure
    /// whose prototype is a one-instruction stub, so calling it is an
    /// ordinary Lua call.
    pub fn new_closure(
        &mut self,
        name: &'static str,
        f: crate::NativeFn,
        upvals: &[Value],
    ) -> Value {
        let Value::Native(id) = self.native(name, f) else {
            unreachable!("natives are registered as natives")
        };
        let proto = match self.rt.native_protos.get(&id) {
            Some(&p) => p,
            None => {
                let p = self.heap.alloc(stub_proto(id));
                self.rt.native_protos.insert(id, p);
                p
            }
        };
        let captures: Box<[Gc<Cell>]> = upvals
            .iter()
            .map(|&value| self.heap.alloc(Cell { value }))
            .collect();
        let code = self.heap.get(proto).code.clone();
        Value::Closure(self.heap.alloc(Closure {
            proto,
            code,
            captures,
        }))
    }

    /// Upvalue `i` of the native closure being called (nil outside one, and
    /// in continuations).
    pub fn upvalue(&self, i: usize) -> Value {
        match self.upvals {
            Some(c) => match self.heap.get(c).captures.get(i) {
                Some(&cell) => self.heap.get(cell).value,
                None => Value::Nil,
            },
            None => Value::Nil,
        }
    }

    /// Set upvalue `i` of the native closure being called.
    pub fn set_upvalue(&mut self, i: usize, v: Value) {
        if let Some(c) = self.upvals
            && let Some(&cell) = self.heap.get(c).captures.get(i)
        {
            self.heap.update(cell, |cell| cell.value = v);
        }
    }
}

/// The prototype of native `id`'s closures: `NativeEntry` and nothing else,
/// taking any number of arguments.
fn stub_proto(id: NativeId) -> Proto {
    let code = [Op::NativeEntry { id: id.0 }];
    Proto {
        code: Rc::new(ProtoCode {
            code: code.as_slice().into(),
            consts: Box::new([]),
            caches: Box::new([]),
            #[cfg(feature = "type-feedback")]
            feedback: Box::new([std::cell::Cell::new(0)]),
            nparams: 0,
            is_vararg: true,
            nregs: 0,
            ncells: 0,
        }),
        children: Box::new([]),
        captures: Box::new([]),
        name: FunctionName::Anonymous,
        line_defined: 0,
        last_line: 0,
        chunk: "[C]".into(),
        source: Rc::from(b"=[C]".as_slice()),
        debug: Rc::default(),
        self_capture: None,
    }
}

impl Vm {
    /// `NativeEntry`: the running frame is native closure `c`'s stub. Pop it
    /// and run the native in its place, with the frame's arguments.
    #[inline(never)]
    pub(crate) fn native_entry(&mut self, c: Gc<Closure>, id: NativeId) -> Result<Flow, Exc> {
        let f = self.rt.stack.frames.pop().expect("the stub's frame");
        let func = f.func as usize;
        let nargs = f.nvarargs as usize;
        self.rt.top = func + 1 + nargs;
        let nf = self.rt.natives[id.0 as usize].f;
        let mut ctx = NativeCtx {
            heap: &mut self.heap,
            rt: &mut self.rt,
            base: func + 1,
            nargs,
            id,
            upvals: Some(c),
        };
        let r = nf(&mut ctx)?;
        self.native_returned(r, None, func, f.want, id)
    }
}
