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
use crate::{Frame, FrameKind, LuaError, Stack, Vm};

/// Nested resumes allowed before `C stack overflow` (where the reference
/// stops).
pub(crate) const MAX_RESUMES: usize = 196;

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
}

impl Coroutine {
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
        });
        Coroutine {
            bytes: stack.bytes(),
            stack,
            top: 0,
            status: CoStatus::Suspended,
            error: None,
        }
    }

    pub(crate) fn main() -> Coroutine {
        Coroutine {
            stack: Stack::default(),
            top: 0,
            status: CoStatus::Running,
            error: None,
            bytes: 0,
        }
    }
}

impl ThreadState for Coroutine {
    fn trace(&self, m: &mut Marker<'_, LuaSpace>) {
        self.stack.trace(m);
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

/// Change thread `t`'s coroutine state through the write barrier, keeping
/// the collector's byte count in step with its parked stack.
fn update_co<R>(heap: &mut LuaHeap, t: Gc<Thread>, f: impl FnOnce(&mut Coroutine) -> R) -> R {
    heap.update(t, |t| {
        let c = t
            .state
            .as_any_mut()
            .downcast_mut::<Coroutine>()
            .expect("threads hold VM coroutines");
        let r = f(c);
        c.bytes = c.stack.bytes();
        r
    })
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
    /// Park the running stack in the current thread (or drop it, for a
    /// coroutine that finished) and run thread `to`'s.
    fn switch_to(&mut self, to: Gc<Thread>, keep: bool) {
        let from = self.rt.current;
        let stack = std::mem::take(&mut self.rt.stack);
        let top = self.rt.top;
        update_co(&mut self.heap, from, |c| {
            if keep {
                c.stack = stack;
                c.top = top;
            }
        });
        let (stack, top) = update_co(&mut self.heap, to, |c| {
            (std::mem::take(&mut c.stack), c.top)
        });
        self.rt.stack = stack;
        self.rt.top = top;
        self.rt.current = to;
        self.rt.switched = true;
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
            protected: false,
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
                });
            }
        }
    }

    /// Move the last `n` values of the running stack to `xfer`.
    fn take_values(&mut self, n: usize) {
        let regs = &mut self.rt.stack.regs;
        let at = regs.len() - n;
        self.rt.xfer.clear();
        self.rt.xfer.extend_from_slice(&regs[at..]);
        regs.truncate(at);
    }

    /// Run the continuation on top of the running stack with an outcome:
    /// `Ok` delivers the values in `xfer`.
    fn deliver(&mut self, outcome: Result<(), Value>) -> Result<Flow, Exc> {
        let f = *self.rt.stack.frames.last().expect("a waiting frame");
        let FrameKind::Native { id, k, call_at, .. } = f.kind else {
            unreachable!("a native waits for every switch")
        };
        let regs = &mut self.rt.stack.regs;
        regs.truncate(call_at as usize);
        let o = match outcome {
            Ok(()) => {
                regs.extend_from_slice(&self.rt.xfer);
                CallOutcome::Ok(self.rt.xfer.len())
            }
            Err(v) => CallOutcome::Err(v),
        };
        self.rt.xfer.clear();
        self.rt.top = self.rt.stack.regs.len();
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
        let at = self.rt.stack.regs.len() - nargs;
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
        let from = self.rt.current;
        update_co(&mut self.heap, from, |c| c.status = CoStatus::Normal);
        update_co(&mut self.heap, co, |c| c.status = CoStatus::Running);
        self.rt.resumers.push(from);
        self.switch_to(co, true);
        let flow = if matches!(
            self.rt.stack.frames.last().map(|f| f.kind),
            Some(FrameKind::CoBase)
        ) {
            // A fresh coroutine: call its body above the base frame.
            let regs = &mut self.rt.stack.regs;
            regs.truncate(1);
            regs.extend_from_slice(&self.rt.xfer);
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
        let at = self.rt.stack.regs.len() - n;
        self.wait_frame(frame, func, want, id, yield_k, at);
        self.take_values(n);
        self.rt.resumers.pop();
        let co = self.rt.current;
        update_co(&mut self.heap, co, |c| c.status = CoStatus::Suspended);
        update_co(&mut self.heap, resumer, |c| c.status = CoStatus::Running);
        self.switch_to(resumer, true);
        switched(self.deliver(Ok(())))
    }

    /// The running coroutine finished: with its results (now in `xfer`), or
    /// with an error. Its stack is dropped and the resumer continues.
    pub(crate) fn co_finished(&mut self, outcome: Result<(), Value>) -> Result<Flow, Exc> {
        let resumer = self.rt.resumers.pop().expect("a coroutine has a resumer");
        let co = self.rt.current;
        update_co(&mut self.heap, co, |c| {
            c.status = CoStatus::Dead;
            c.error = outcome.err();
        });
        update_co(&mut self.heap, resumer, |c| c.status = CoStatus::Running);
        self.switch_to(resumer, false);
        switched(self.deliver(outcome))
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

/// After a switch the loop must reload: the frame it had is on another
/// stack.
fn switched(flow: Result<Flow, Exc>) -> Result<Flow, Exc> {
    flow.map(|f| match f {
        Flow::Reload => Flow::Switched,
        f => f,
    })
}

impl NativeCtx<'_> {
    /// A new suspended coroutine running `f`.
    pub fn new_thread(&mut self, f: Value) -> Gc<Thread> {
        self.heap.alloc(Thread::new(Coroutine::fresh(f)))
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

    /// Kill suspended or dead thread `t`, releasing its stack. Returns the
    /// error it died with, if any.
    pub fn close_thread(&mut self, t: Gc<Thread>) -> Option<Value> {
        update_co(self.heap, t, |c| {
            c.status = CoStatus::Dead;
            c.stack = Stack::default();
            c.error.take()
        })
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
        chunk: "[C]".into(),
        debug: Rc::default(),
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
        self.rt.stack.regs.truncate(func + 1 + nargs);
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
