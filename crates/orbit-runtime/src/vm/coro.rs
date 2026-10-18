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

use crate::object::table::mark;
use crate::object::{
    Cell, Closure, Gc, LuaHeap, LuaSpace, NativeId, Proto, ProtoCode, Thread, ThreadState, Value,
};
use orbit_bytecode::{FunctionName, MULTI, Op};
use orbit_gc::Marker;

use crate::vm::interp::Flow;
use crate::vm::interp::extend_nil;
use crate::vm::native::{CallOutcome, Continuation, NativeCtx, NativeReturn, ResumeOk};
use crate::vm::ops::Exc;
use crate::vm::{Catch, Frame, FrameKind, LuaError, Stack, Vm};

/// Nested resumes allowed before `C stack overflow` (where the reference
/// stops).
pub(crate) const MAX_RESUMES: usize = 196;

/// How many suspended stacks stay at full size (ADR-3): a stack parked
/// longer ago than the last `HOT_STACKS` parks, and not resumed since, is
/// shrunk to its live values.
pub(crate) const HOT_STACKS: usize = 16;

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

/// Who is closing a coroutine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum Closing {
    #[default]
    No,
    /// `coroutine.close(co)` from another thread, which gets the outcome.
    Closer,
    /// `coroutine.close()` in the coroutine itself, which then ends as if
    /// with an error (or returning nothing, if no handler fails).
    Itself,
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
    /// `rt.gc_cycles` when it was parked.
    cycle: u32,
}

impl Coroutine {
    /// Where a parked stack's live registers end: its top, or past its
    /// last variable still to close (a dead coroutine kept for
    /// `coroutine.close`).
    fn live(&self) -> usize {
        live_end(&self.stack, self.top)
    }

    /// A suspended coroutine that will call `f`: `f` sits above the base
    /// frame, where the first resume calls it.
    fn fresh(f: Value) -> Coroutine {
        let mut stack = Stack::default();
        stack.regs.push(f);
        stack.frames.push(base_frame());
        Coroutine {
            bytes: stack.bytes(),
            stack,
            top: 1,
            status: CoStatus::Suspended,
            error: None,
            parks: 0,
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

/// Where the live registers of parked stack `s`, whose top is `top`, end:
/// [`Coroutine::live`].
fn live_end(s: &Stack, top: usize) -> usize {
    s.aux.tbc.last().map_or(top, |&r| top.max(r as usize + 1))
}

// Upcasting to `dyn Any` reads its vtable from the thread's, which saves
// the indirect call `as_any` would make on every switch.
fn co(heap: &LuaHeap, t: Gc<Thread>) -> &Coroutine {
    let state: &dyn Any = &*heap.get(t).state;
    state.downcast_ref().expect("threads hold VM coroutines")
}

fn co_mut(heap: &mut LuaHeap, t: Gc<Thread>) -> &mut Coroutine {
    let state: &mut dyn Any = &mut *heap.get_mut(t).state;
    state.downcast_mut().expect("threads hold VM coroutines")
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
    ///
    /// The running thread's own `stack` slot holds an empty stack (and a
    /// stale copy of its hook), so a switch swaps the running stack with
    /// that empty one, then the empty one with `to`'s: nothing is built or
    /// dropped on the way ([`Stack::park`], [`Stack::unpark`]).
    fn switch_to(&mut self, to: Gc<Thread>, from_status: CoStatus) {
        let from = self.rt.current;
        let top = self.rt.top;
        let gc_cycles = self.rt.gc_cycles;
        let hooks = self.rt.hooks_touched;
        // A parked stack is traced through its thread, which the collector
        // may already have scanned.
        self.heap.barrier_back(from);
        let c = co_mut(&mut self.heap, from);
        c.status = from_status;
        // A thread's bytes are its stack's, parked or running: they change
        // only when the stack's capacity does.
        let old = c.bytes;
        // A coroutine that died with variables still to close keeps its
        // stack for `coroutine.close`, and one that died with an error keeps
        // it for the debug library (its traceback, locals) until closed.
        let finished =
            from_status == CoStatus::Dead && self.rt.stack.aux.tbc.is_empty() && c.error.is_none();
        if finished {
            c.bytes = 0;
            c.top = 0;
            // `debug.gethook` still reports a dead coroutine's hook.
            c.stack.hook = self.rt.stack.hook;
        } else {
            c.bytes = self.rt.stack.bytes();
            // An errored stack's frames stop mid-body, so their locals can
            // sit above `top`: keep every register (all traced while running).
            c.top = if from_status == CoStatus::Dead && c.error.is_some() {
                self.rt.stack.regs.len()
            } else {
                top
            };
            self.rt.stack.park(&mut c.stack, hooks);
            c.cycle = gc_cycles;
        }
        let delta = c.bytes as isize - old as isize;
        let suspended = from_status == CoStatus::Suspended;
        c.parks = c.parks.wrapping_add(suspended as u32);
        let stamp = c.parks;
        if delta != 0 {
            self.heap.adjust_bytes(delta);
        }
        if suspended {
            let at = self.rt.cooling_at;
            self.rt.cooling_at = (at + 1) % HOT_STACKS;
            let out = self.rt.cooling[at].replace((from, stamp));
            // An older entry of the thread parking now is stale.
            if let Some((cold, stamp)) = out
                && cold != from
            {
                self.shrink_parked(cold, stamp);
            }
        }
        let c = co_mut(&mut self.heap, to);
        c.status = CoStatus::Running;
        self.rt.stack.unpark(&mut c.stack, hooks);
        if finished {
            // The finished stack goes, leaving `to`'s slot empty.
            c.stack.clear(Default::default());
        }
        self.rt.top = c.top;
        // If a collection swept while it was parked, the dead slots above
        // its top (which aren't traced) may refer to freed objects.
        if c.cycle != gc_cycles {
            let live = live_end(&self.rt.stack, c.top);
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
    #[inline]
    fn shrink_parked(&mut self, t: Gc<Thread>, stamp: u32) {
        // Usually `t` has been resumed since: nothing to do.
        let c = co(&self.heap, t);
        if c.status == CoStatus::Suspended && c.parks == stamp {
            self.shrink_parked_stack(t);
        }
    }

    /// [`Vm::shrink_parked`]'s copy, for a thread that has gone cold.
    #[cold]
    #[inline(never)]
    fn shrink_parked_stack(&mut self, t: Gc<Thread>) {
        let c = co_mut(&mut self.heap, t);
        let before = c.bytes;
        let s = &mut c.stack;
        s.regs = s.regs[..c.top].to_vec();
        s.cells = s.cells.to_vec();
        s.frames = s.frames.to_vec();
        s.aux.tbc = s.aux.tbc.to_vec();
        c.bytes = s.bytes();
        let delta = c.bytes as isize - before as isize;
        self.heap.adjust_bytes(delta);
    }

    /// Push (or reuse) the continuation frame of native `id`, which waits
    /// for something that isn't a call on this stack (a resume or a
    /// yield); `at` is where the values it gets back will go.
    #[allow(clippy::too_many_arguments)]
    #[inline(always)]
    fn wait_frame(
        &mut self,
        frame: Option<Frame>,
        func: usize,
        want: u8,
        id: NativeId,
        k: Continuation,
        ok: ResumeOk,
        at: usize,
    ) {
        let kind = FrameKind::Native {
            id,
            k,
            catch: Catch::No,
            yieldable: true,
            call_at: at as u32,
            ok,
        };
        match frame {
            Some(_) => self.rt.stack.frames.last_mut().expect("frame").kind = kind,
            None => {
                let nargs = at - (func + 1);
                // Not recorded in `depth_at`: until it is popped, the frame
                // recorded before it gives the same depth.
                let cdepth = self.rt.stack.cdepth();
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
                    cdepth,
                });
            }
        }
    }

    /// Move the last `n` values of the running stack to `xfer`.
    #[inline(always)]
    fn take_values(&mut self, n: usize) {
        let at = self.rt.top - n;
        self.rt.xfer.clear();
        if n == 1 {
            self.rt.xfer.push(self.rt.stack.regs[at]);
        } else {
            self.rt
                .xfer
                .extend_from_slice(&self.rt.stack.regs[at..self.rt.top]);
        }
        self.rt.top = at;
    }

    /// Run the continuation on top of the running stack with an outcome:
    /// `Ok` delivers the values in `xfer`.
    #[inline(always)]
    fn deliver(&mut self, outcome: Result<(), Value>) -> Result<Flow, Exc> {
        // The frame's fields are read one by one: a copy of the whole frame
        // costs more than the fast path uses.
        let f = self.rt.stack.frames.last().expect("a waiting frame");
        // The usual case: the values are the waiting native's results,
        // which a return hook (when one is set) must see in its frame.
        if outcome.is_ok()
            && let FrameKind::Native { call_at, ok, .. } = f.kind
            && ok != ResumeOk::Continue
            && !self.rt.stack.hook.wants(crate::vm::hook::RETURN)
        {
            let (func, want) = (f.func as usize, f.want);
            return self.deliver_values(func, want, call_at as usize, ok == ResumeOk::ReturnTrue);
        }
        let f = *f;
        self.deliver_k(f, outcome)
    }

    /// [`Vm::deliver`] through the continuation.
    #[inline(never)]
    fn deliver_k(&mut self, f: Frame, outcome: Result<(), Value>) -> Result<Flow, Exc> {
        if let FrameKind::Boundary { .. } = f.kind {
            // A native waiting in [`NativeCtx::resume_nested`]: the values
            // stay in `xfer`, and an error unwinds to the boundary.
            return match outcome {
                Ok(()) => {
                    self.rt.stack.frames.pop();
                    Ok(Flow::Done)
                }
                Err(v) => Err(Exc::Lua(LuaError::new(v))),
            };
        }
        let FrameKind::Native { id, k, call_at, .. } = f.kind else {
            unreachable!("a native waits for every switch")
        };
        let o = match outcome {
            Ok(()) => {
                let n = self.rt.xfer.len();
                self.rt.top = if let [v] = self.rt.xfer[..] {
                    self.rt.stack.write1(call_at as usize, v)
                } else {
                    self.rt.stack.write(call_at as usize, &self.rt.xfer)
                };
                CallOutcome::Ok(n)
            }
            Err(v) => {
                self.rt.top = call_at as usize;
                CallOutcome::Err(v)
            }
        };
        self.rt.xfer.clear();
        let upvals = self.rt.stack.closure_at(f.func as usize);
        let mut ctx = NativeCtx {
            vm: self,
            base: f.base as usize,
            nargs: f.nvarargs as usize,
            id,
            upvals,
        };
        let r = k(&mut ctx, o);
        // The usual result is rebuilt from its count rather than moved
        // whole: copying what the continuation just stored field by field
        // as one wide value stalls on store forwarding, on every switch.
        if let Ok(NativeReturn::Return(n)) = r {
            return self.native_returned(
                NativeReturn::Return(n),
                Some(f),
                f.func as usize,
                f.want,
                id,
            );
        }
        self.native_returned(r?, Some(f), f.func as usize, f.want, id)
    }

    /// [`Vm::deliver`]'s success path for the waiting frame on top, at
    /// `func` and wanting `want` results, whose native returns the values in
    /// `xfer` (after `true`, with `with_true`), as [`ResumeOk`] promises: pop
    /// the frame and place them as its results straight from `xfer`, as its
    /// continuation and [`Vm::native_returned`] would.
    #[inline(always)]
    fn deliver_values(
        &mut self,
        func: usize,
        want: u8,
        call_at: usize,
        with_true: bool,
    ) -> Result<Flow, Exc> {
        let s = &mut self.rt.stack;
        let xfer = &self.rt.xfer;
        let depth = s.frames.len() - 1;
        s.frames.truncate(depth);
        let pre = with_true as usize;
        let n = xfer.len() + pre;
        let end = func + if want == MULTI { n } else { want as usize };
        // What the native left in its window: its arguments and pushes,
        // and where the continuation would have got the values.
        let stop = s.dirty_to.max(call_at + xfer.len());
        extend_nil(&mut s.regs, end);
        let out = &mut s.regs[func..end];
        let m = n.min(out.len());
        if m > 0 {
            if with_true {
                out[0] = Value::Bool(true);
            }
            if m - pre == 1 {
                out[pre] = xfer[0];
            } else {
                out[pre..m].copy_from_slice(&xfer[..m - pre]);
            }
        }
        out[m..].fill(Value::Nil);
        // As `native_returned` does: stale copies mustn't keep objects
        // alive.
        let stop = stop.min(s.regs.len());
        if end < stop {
            s.regs[end..stop].fill(Value::Nil);
        }
        s.dirty_to = end;
        self.rt.top = end;
        self.rt.xfer.clear();
        if matches!(
            self.rt.stack.frames.last().map(|f| &f.kind),
            Some(FrameKind::Lua { .. })
        ) {
            Ok(Flow::Reload)
        } else {
            self.after_return()
        }
    }

    /// `NativeReturn::Resume`: native `id` resumes `co` with the last `nargs`
    /// values of its window and waits in `k`.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn resume(
        &mut self,
        co: Gc<Thread>,
        nargs: usize,
        k: Continuation,
        ok: ResumeOk,
        frame: Option<Frame>,
        func: usize,
        want: u8,
        id: NativeId,
    ) -> Result<Flow, Exc> {
        let at = self.rt.top - nargs;
        self.wait_frame(frame, func, want, id, k, ok, at);
        self.take_values(nargs);
        let status = self.co_status(co);
        let refuse = if status != CoStatus::Suspended {
            Some(if status == CoStatus::Dead {
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
            self.rt.stack.frames.last().map(|f| &f.kind),
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
        self.yield_then(n, yield_k, ResumeOk::Return, frame, func, want, id)
    }

    /// [`Vm::yield_`], waiting in continuation `k`.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn yield_then(
        &mut self,
        n: usize,
        k: Continuation,
        ok: ResumeOk,
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
        if !yieldable(&self.heap, &self.rt.stack) || self.rt.stack.aux.closing != Closing::No {
            return Err(Exc::Plain(
                "attempt to yield across a C-call boundary".into(),
            ));
        }
        let at = self.rt.top - n;
        self.wait_frame(frame, func, want, id, k, ok, at);
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
        let closing = std::mem::replace(&mut self.rt.stack.aux.closing, Closing::No);
        if let Err(e) = outcome
            && closing != Closing::Closer
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
        self.wait_frame(frame, func, want, id, k, ResumeOk::Continue, at);
        self.rt.xfer.clear();
        if self.rt.resumers.len() >= MAX_RESUMES {
            // Closing runs on the coroutine's stack, so it nests like a
            // resume.
            let e = self.new_str(b"C stack overflow");
            return self.deliver(Err(e));
        }
        let c = co_mut(&mut self.heap, co);
        let error = c.error.take();
        if c.stack.aux.tbc.is_empty() {
            c.status = CoStatus::Dead;
            let hook = c.stack.hook;
            c.stack.clear(hook);
            c.top = 0;
            let freed = std::mem::take(&mut c.bytes);
            self.heap.adjust_bytes(-(freed as isize));
            return self.deliver(error.map_or(Ok(()), Err));
        }
        // Run the handlers on the coroutine's own stack, as an error
        // unwinding to its base frame would.
        c.stack.aux.closing = Closing::Closer;
        self.rt.resumers.push(self.rt.current);
        self.switch_to(co, CoStatus::Normal);
        self.rt.stack.pop_to_base();
        let e = LuaError::new(error.unwrap_or(Value::Nil));
        switched(self.unwind(e).map_err(Exc::Lua))
    }

    /// `NativeReturn::Exit`: the running coroutine closes itself. Its
    /// variables still to close are closed as an error unwinding to its
    /// base would close them, then it ends.
    pub(crate) fn exit_co(&mut self) -> Result<Flow, Exc> {
        if self.rt.stack.aux.tbc.is_empty() {
            self.rt.xfer.clear();
            return self.co_finished(Ok(()));
        }
        self.rt.stack.aux.closing = Closing::Itself;
        self.rt.stack.pop_to_base();
        Err(Exc::Lua(LuaError::new(Value::Nil)))
    }

    /// The body returned into the coroutine's base frame: its results are
    /// at the bottom of the stack.
    pub(crate) fn co_returned(&mut self) -> Result<Flow, Exc> {
        let top = self.rt.top;
        // The body sat at the base frame's `func` (0, unless C put values
        // below it: [`NativeCtx::resume_nested`]).
        let from = self.rt.stack.frames.last().map_or(0, |f| f.func as usize);
        self.rt.xfer.clear();
        self.rt
            .xfer
            .extend_from_slice(&self.rt.stack.regs[from..top]);
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

impl Vm {
    /// Push the boundary a native waits on in [`NativeCtx::resume_nested`]
    /// or [`NativeCtx::close_nested`]; returns the top to restore after.
    fn push_wait_boundary(&mut self) -> Result<usize, LuaError> {
        let at = self.rt.top;
        let cdepth = self.rt.stack.deeper();
        if self.c_overflow(cdepth) {
            return Err(LuaError::new(self.new_str(b"C stack overflow")));
        }
        let s = &mut self.rt.stack;
        s.frames.push(Frame {
            kind: FrameKind::Boundary {
                native: true,
                handler: false,
            },
            func: at as u32,
            base: at as u32,
            pc: 0,
            cell_base: s.cells.len() as u32,
            nvarargs: 0,
            want: MULTI,
            tail: false,
            extra: 0,
            cdepth,
        });
        Ok(at)
    }

    /// Run a nested resume or close until it comes back to its boundary.
    fn finish_nested(&mut self, flow: Result<Flow, Exc>) -> Result<(), LuaError> {
        let flow = match flow {
            Ok(f) => f,
            Err(x) => {
                let e = self.make_error(x);
                self.unwind(e)?
            }
        };
        match flow {
            Flow::Done => Ok(()),
            Flow::Reload | Flow::Switched => {
                self.rt.switched = false;
                self.run()
            }
        }
    }
}

/// What [`NativeCtx::enter_thread_window`] changed, for
/// [`NativeCtx::leave_thread_window`] to restore.
#[derive(Debug)]
#[must_use]
pub struct WindowSave(Option<(usize, usize, usize)>);

/// A coroutine's base frame, with its body at register 0.
fn base_frame() -> Frame {
    Frame {
        kind: FrameKind::CoBase,
        func: 0,
        base: 0,
        pc: 0,
        cell_base: 0,
        nvarargs: 0,
        want: MULTI,
        tail: false,
        extra: 0,
        cdepth: 0,
    }
}

/// Where the C API's view of a parked stack starts: the window of the
/// native waiting in a yield, everything for a coroutine that hasn't
/// started (its function) or a finished one, and nothing (`top`) for one
/// stopped by an error.
fn window_base(s: &Stack, top: usize) -> usize {
    match s.frames.last().map(|f| (f.kind, f.base)) {
        Some((FrameKind::Native { .. }, base)) => base as usize,
        Some((FrameKind::CoBase, _)) | None => 0,
        Some(_) => top,
    }
}

/// Whether code running on top of `stack` (a coroutine's) may yield:
/// nothing between it and the coroutine's base refuses.
pub(crate) fn yieldable(heap: &LuaHeap, stack: &Stack) -> bool {
    !stack.frames.iter().any(|f| match f.kind {
        FrameKind::Native {
            yieldable: false, ..
        }
        | FrameKind::Handler
        | FrameKind::Boundary { .. }
        // A hook is a nested call, as a Lua hook is in the reference.
        | FrameKind::Hook => true,
        // Most built-ins refuse, as the C functions they stand for do.
        FrameKind::Lua { closure, .. } => heap.get(closure).code.refuses_yield,
        _ => false,
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
pub(crate) fn closing(rt: &crate::vm::Runtime) -> bool {
    rt.stack.aux.closing != Closing::No
}

impl NativeCtx<'_> {
    /// Whether thread `t` could yield where it is: it isn't the main thread
    /// and no frame on its stack refuses (`coroutine.isyieldable`).
    pub fn thread_yieldable(&self, t: Gc<Thread>) -> bool {
        if t == self.vm.rt.main_thread {
            return false;
        }
        let s = if t == self.vm.rt.current {
            &self.vm.rt.stack
        } else {
            &co(&self.vm.heap, t).stack
        };
        s.aux.closing == Closing::No && yieldable(&self.vm.heap, s)
    }

    /// A new suspended coroutine running `f`.
    pub fn new_thread(&mut self, f: Value) -> Gc<Thread> {
        let mut c = Coroutine::fresh(f);
        // A new thread gets its creator's hook mask and count, but not its
        // function (observed from lua5.5: `gethook(co)` is `nil, mask,
        // count` and no events reach a hook).
        c.stack.hook = self.vm.rt.stack.hook.inherited();
        self.vm.heap.alloc(Thread::new(c))
    }

    /// Run `f` with thread `t`'s stack in place of the running one, for
    /// the debug library's view of another thread. `f` may read and write
    /// that stack's frames and registers, but must not touch this native's
    /// window or run Lua code. The running thread runs `f` directly.
    pub fn on_thread<R>(&mut self, t: Gc<Thread>, f: impl FnOnce(&mut Self, bool) -> R) -> R {
        if t == self.vm.rt.current {
            return f(self, false);
        }
        let gc_cycles = self.vm.rt.gc_cycles;
        let c = co_mut(&mut self.vm.heap, t);
        // As when resuming it: after a sweep, the untraced slots above the
        // parked top may refer to freed objects.
        if c.cycle != gc_cycles {
            c.stack.regs.truncate(c.top);
        }
        // The running stack waits in `t`'s slot.
        c.stack.swap(&mut self.vm.rt.stack);
        let saved_top = std::mem::replace(&mut self.vm.rt.top, c.top);
        let r = f(self, true);
        // `f` may have stored values into the parked stack.
        self.vm.heap.barrier_back(t);
        let c = co_mut(&mut self.vm.heap, t);
        c.stack.swap(&mut self.vm.rt.stack);
        c.top = std::mem::replace(&mut self.vm.rt.top, saved_top);
        r
    }

    /// [`NativeCtx::on_thread`] with `t`'s stack as this native's window,
    /// from register `base` (by default where the C API's view of that
    /// thread starts) to its top: for the C API's calls on a thread that
    /// isn't running. `f` may push and pop, and should not run Lua code.
    pub fn on_thread_window<R>(
        &mut self,
        t: Gc<Thread>,
        base: Option<usize>,
        f: impl FnOnce(&mut Self) -> R,
    ) -> R {
        let saved = self.enter_thread_window(t, base);
        let r = f(self);
        self.leave_thread_window(t, saved);
        r
    }

    /// The first half of [`NativeCtx::on_thread_window`]: `t`'s stack is
    /// this native's window until [`NativeCtx::leave_thread_window`] is
    /// given what this returns.
    pub fn enter_thread_window(&mut self, t: Gc<Thread>, base: Option<usize>) -> WindowSave {
        if t == self.vm.rt.current {
            return WindowSave(None);
        }
        let gc_cycles = self.vm.rt.gc_cycles;
        let c = co_mut(&mut self.vm.heap, t);
        // As when resuming it: after a sweep, the untraced slots above the
        // parked top may refer to freed objects.
        if c.cycle != gc_cycles {
            c.stack.regs.truncate(c.top);
        }
        let base = base
            .unwrap_or_else(|| window_base(&c.stack, c.top))
            .min(c.top);
        // The running stack waits in `t`'s slot, all of it traced.
        c.stack.swap(&mut self.vm.rt.stack);
        let saved_top = std::mem::replace(&mut self.vm.rt.top, c.top);
        c.top = saved_top;
        let saved = WindowSave(Some((saved_top, self.base, self.nargs)));
        self.base = base;
        self.nargs = self.vm.rt.top - base;
        saved
    }

    /// Back to the running stack after [`NativeCtx::enter_thread_window`].
    pub fn leave_thread_window(&mut self, t: Gc<Thread>, saved: WindowSave) {
        let WindowSave(Some((saved_top, base, nargs))) = saved else {
            return;
        };
        (self.base, self.nargs) = (base, nargs);
        // Values may have been stored into the parked stack.
        self.vm.heap.barrier_back(t);
        let c = co_mut(&mut self.vm.heap, t);
        c.stack.swap(&mut self.vm.rt.stack);
        c.top = std::mem::replace(&mut self.vm.rt.top, saved_top);
    }

    /// A thread with nothing on its stack, as the C API's `lua_newthread`
    /// makes: it reads as dead until a function is put on its stack and it
    /// is resumed ([`NativeCtx::resume_nested`]).
    pub fn new_empty_thread(&mut self) -> Gc<Thread> {
        let mut c = Coroutine::main();
        c.status = CoStatus::Dead;
        c.stack.hook = self.vm.rt.stack.hook.inherited();
        self.vm.heap.alloc(Thread::new(c))
    }

    /// Whether thread `t` is suspended in a yield (not one that hasn't
    /// started).
    pub fn thread_yielded(&self, t: Gc<Thread>) -> bool {
        if t == self.vm.rt.current {
            return false;
        }
        let c = co(&self.vm.heap, t);
        c.status == CoStatus::Suspended
            && !matches!(
                c.stack.frames.last().map(|f| f.kind),
                Some(FrameKind::CoBase)
            )
    }

    /// Where thread `t`'s values end (`t` not running), for a window
    /// ([`NativeCtx::on_thread_window`]) starting there.
    pub fn thread_top(&self, t: Gc<Thread>) -> usize {
        co(&self.vm.heap, t).top
    }

    /// Whether thread `t` died with an error that no close has cleared.
    pub fn thread_failed(&self, t: Gc<Thread>) -> bool {
        t != self.vm.rt.current && co(&self.vm.heap, t).error.is_some()
    }

    /// Resume `t` with `args` and wait until it yields, returns or fails,
    /// running it in a nested interpreter loop, as the C API's
    /// `lua_resume` does: `Ok` with what it yielded or returned (and
    /// whether it returned), `Err` with its error or why it can't be
    /// resumed. A coroutine that hasn't started calls the function on top
    /// of its stack; so does a dead one that ended without an error, once
    /// something has put a function there (C reuses threads that way).
    /// Nothing can yield across the native while it waits.
    pub fn resume_nested(
        &mut self,
        t: Gc<Thread>,
        args: &[Value],
    ) -> Result<(bool, Vec<Value>), Value> {
        let vm = &mut *self.vm;
        if t != vm.rt.current {
            let c = co_mut(&mut vm.heap, t);
            if c.status == CoStatus::Dead
                && c.error.is_none()
                && c.stack.frames.is_empty()
                && c.top > 0
            {
                c.stack.frames.push(base_frame());
                c.status = CoStatus::Suspended;
            }
        }
        let fresh = t != vm.rt.current
            && matches!(
                co(&vm.heap, t).stack.frames.last().map(|f| f.kind),
                Some(FrameKind::CoBase)
            );
        let refuse = match vm.co_status(t) {
            CoStatus::Suspended if fresh && co(&vm.heap, t).top == 0 => {
                Some("cannot resume dead coroutine")
            }
            CoStatus::Suspended if vm.rt.resumers.len() >= MAX_RESUMES => Some("C stack overflow"),
            CoStatus::Suspended => None,
            CoStatus::Dead => Some("cannot resume dead coroutine"),
            _ => Some("cannot resume non-suspended coroutine"),
        };
        if let Some(msg) = refuse {
            return Err(vm.new_str(msg.as_bytes()));
        }
        let at = vm.push_wait_boundary().map_err(|e| e.value)?;
        vm.rt.resumers.push(vm.rt.current);
        vm.switch_to(t, CoStatus::Normal);
        let flow = if fresh {
            let func = vm.rt.top - 1;
            vm.rt.stack.frames[0].func = func as u32;
            vm.rt.top = vm.rt.stack.write(func + 1, args);
            vm.call_value(func, args.len(), MULTI)
        } else {
            vm.rt.xfer.clear();
            vm.rt.xfer.extend_from_slice(args);
            vm.deliver(Ok(()))
        };
        let r = vm.finish_nested(flow);
        vm.rt.top = at;
        match r {
            Ok(()) => {
                let vals = std::mem::take(&mut vm.rt.xfer);
                Ok((vm.co_status(t) == CoStatus::Dead, vals))
            }
            Err(e) => Err(e.value),
        }
    }

    /// Close thread `co` (suspended or dead) and wait, as the C API's
    /// `lua_closethread` does: run its pending `__close` handlers on its
    /// stack, newest first, with its error (nil if it has none). `Err` has
    /// its error or the last error a handler raised. The thread is left
    /// dead with an empty stack, which C may reuse.
    pub fn close_nested(&mut self, co: Gc<Thread>) -> Result<(), Value> {
        let vm = &mut *self.vm;
        match vm.co_status(co) {
            CoStatus::Suspended | CoStatus::Dead => {}
            _ => return Err(vm.new_str(b"cannot close a running coroutine")),
        }
        if vm.rt.resumers.len() >= MAX_RESUMES {
            return Err(vm.new_str(b"C stack overflow"));
        }
        let c = co_mut(&mut vm.heap, co);
        let error = c.error.take();
        if c.stack.aux.tbc.is_empty() {
            c.status = CoStatus::Dead;
            let hook = c.stack.hook;
            c.stack.clear(hook);
            c.top = 0;
            let freed = std::mem::take(&mut c.bytes);
            vm.heap.adjust_bytes(-(freed as isize));
            return error.map_or(Ok(()), Err);
        }
        c.stack.aux.closing = Closing::Closer;
        let at = vm.push_wait_boundary().map_err(|e| e.value)?;
        vm.rt.resumers.push(vm.rt.current);
        vm.switch_to(co, CoStatus::Normal);
        vm.rt.stack.pop_to_base();
        let flow = vm
            .unwind(LuaError::new(error.unwrap_or(Value::Nil)))
            .map_err(Exc::Lua);
        let r = vm.finish_nested(flow);
        vm.rt.top = at;
        vm.rt.xfer.clear();
        r.map_err(|e| e.value)
    }

    /// The status of thread `t`.
    pub fn thread_status(&self, t: Gc<Thread>) -> CoStatus {
        if t == self.vm.rt.current {
            CoStatus::Running
        } else {
            co(&self.vm.heap, t).status
        }
    }

    /// The main thread.
    pub fn running_main_thread(&self) -> Gc<Thread> {
        self.vm.rt.main_thread
    }

    /// The running thread, and whether it is the main thread.
    pub fn running(&self) -> (Gc<Thread>, bool) {
        (
            self.vm.rt.current,
            self.vm.rt.current == self.vm.rt.main_thread,
        )
    }

    /// The values of the main thread's variables still to close, oldest
    /// first, for `os.exit(code, true)`, which closes them as lua5.5 does
    /// when it closes the state.
    pub fn main_pending_closes(&self) -> Vec<Value> {
        let s = if self.vm.rt.current == self.vm.rt.main_thread {
            &self.vm.rt.stack
        } else {
            &co(&self.vm.heap, self.vm.rt.main_thread).stack
        };
        s.aux.tbc.iter().map(|&r| s.regs[r as usize]).collect()
    }

    // ---- native closures -------------------------------------------------

    /// A function value that runs native `f` (registered as `name`) with
    /// `upvals` as its upvalues ([`NativeCtx::upvalue`]). It is a closure
    /// whose prototype is a one-instruction stub, so calling it is an
    /// ordinary Lua call.
    pub fn new_closure(
        &mut self,
        name: &'static str,
        f: crate::vm::NativeFn,
        upvals: &[Value],
    ) -> Value {
        self.new_closure_hidden(name, f, upvals, &[])
    }

    /// [`NativeCtx::new_closure`], plus `hidden` upvalues that scripts can't
    /// see or change through the debug library, for state a native must be
    /// able to trust ([`NativeCtx::hidden_upvalue`]). Every closure of `f`
    /// has the same number of hidden upvalues.
    pub fn new_closure_hidden(
        &mut self,
        name: &'static str,
        f: crate::vm::NativeFn,
        upvals: &[Value],
        hidden: &[Value],
    ) -> Value {
        let Value::Native(id) = self.native(name, f) else {
            unreachable!("natives are registered as natives")
        };
        let n = u8::try_from(hidden.len()).expect("few hidden upvalues");
        let native = &mut self.vm.rt.natives[id.0 as usize];
        debug_assert!(
            native.hidden == n || !self.vm.rt.native_protos.contains_key(&id),
            "every closure of a native has the same hidden upvalues"
        );
        native.hidden = n;
        let proto = match self.vm.rt.native_protos.get(&id) {
            Some(&p) => p,
            None => {
                let p = self.vm.heap.alloc(stub_proto(id));
                self.vm.rt.native_protos.insert(id, p);
                p
            }
        };
        // Hidden upvalues go last, so visible ones keep their indices.
        let captures: Box<[Gc<Cell>]> = upvals
            .iter()
            .chain(hidden)
            .map(|&value| self.vm.heap.alloc(Cell { value }))
            .collect();
        let code = self.vm.heap.get(proto).code.clone();
        Value::Closure(self.vm.heap.alloc(Closure {
            proto,
            code,
            captures,
        }))
    }

    /// Hidden upvalue `i` of the native closure being called (nil outside
    /// one); see [`NativeCtx::new_closure_hidden`].
    #[inline]
    pub fn hidden_upvalue(&self, i: usize) -> Value {
        let Some(c) = self.upvals else {
            return Value::Nil;
        };
        let caps = &self.vm.heap.get(c).captures;
        let hidden = self.vm.rt.natives[self.id.0 as usize].hidden as usize;
        match (i < hidden).then(|| caps[caps.len() - hidden + i]) {
            Some(cell) => self.vm.heap.get(cell).value,
            None => Value::Nil,
        }
    }

    /// The native behind function `f`: `f` itself, or the native a native
    /// closure runs. `None` for Lua functions and other values.
    pub fn native_of(&self, f: Value) -> Option<NativeId> {
        match f {
            Value::Native(id) => Some(id),
            Value::Closure(c) => {
                let p = self.vm.heap.get(self.vm.heap.get(c).proto);
                match p.code.code.first() {
                    Some(Op::NativeEntry { id }) => Some(NativeId(*id)),
                    _ => None,
                }
            }
            _ => None,
        }
    }

    /// Hidden upvalue `i` of native closure `f` (nil if it has none).
    pub fn hidden_upvalue_of(&self, f: Value, i: usize) -> Value {
        let Value::Closure(c) = f else {
            return Value::Nil;
        };
        let closure = self.vm.heap.get(c);
        let hidden = self.vm.rt.hidden_upvalues(&self.vm.heap, closure);
        let caps = &closure.captures;
        match (i < hidden).then(|| caps[caps.len() - hidden + i]) {
            Some(cell) => self.vm.heap.get(cell).value,
            None => Value::Nil,
        }
    }

    /// How many visible upvalues the native closure being called has.
    pub fn upvalue_count(&self) -> usize {
        match self.upvals {
            Some(c) => {
                self.vm.heap.get(c).captures.len()
                    - self.vm.rt.natives[self.id.0 as usize].hidden as usize
            }
            None => 0,
        }
    }

    /// Upvalue `i` of the native closure being called, or whose
    /// continuation is running (nil outside one).
    pub fn upvalue(&self, i: usize) -> Value {
        match self.upvals {
            Some(c) => match self.vm.heap.get(c).captures.get(i) {
                Some(&cell) => self.vm.heap.get(cell).value,
                None => Value::Nil,
            },
            None => Value::Nil,
        }
    }

    /// Set upvalue `i` of the native closure being called.
    pub fn set_upvalue(&mut self, i: usize, v: Value) {
        if let Some(c) = self.upvals
            && let Some(&cell) = self.vm.heap.get(c).captures.get(i)
        {
            self.vm.heap.update(cell, |cell| cell.value = v);
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
            builtin: false,
            refuses_yield: false,
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
        // Its fields one by one, as in `deliver`.
        let f = self.rt.stack.frames.last().expect("the stub's frame");
        let (func, nargs, want) = (f.func as usize, f.nvarargs as usize, f.want);
        let depth = self.rt.stack.frames.len() - 1;
        self.rt.stack.frames.truncate(depth);
        self.rt.top = func + 1 + nargs;
        let nf = self.rt.natives[id.0 as usize].f;
        let mut ctx = NativeCtx {
            vm: self,
            base: func + 1,
            nargs,
            id,
            upvals: Some(c),
        };
        let r = nf(&mut ctx);
        // A wrapped coroutine's function resumes it: switch at once, from
        // the fields alone (see `deliver`).
        if let Ok(NativeReturn::Resume { co, nargs, k, ok }) = r {
            return self.resume(co, nargs, k, ok, None, func, want, id);
        }
        let r = r?;
        if let NativeReturn::Return(n) = r
            && self.rt.stack.hook.wants(crate::vm::hook::RETURN)
        {
            self.native_return_hook(func, id, n).map_err(Exc::Lua)?;
        }
        self.native_returned(r, None, func, want, id)
    }
}
