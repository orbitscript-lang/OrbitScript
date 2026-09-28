//! Coroutines (`lua_newthread`, `lua_resume`, `lua_closethread`).
//!
//! Only the running coroutine's state lives in the [`Vm`]. Switching coroutines moves
//! the current state into its [`Thread`] object (stack and open upvalues where the
//! collector can trace them, the rest as a [`SavedState`]) and moves the target's state
//! out. `resume` runs the target on the Rust stack, as the reference does, so nesting
//! is bounded by [`crate::MAX_C_CALLS`].

use crate::error::{Error, LuaResult};
use crate::exec::{Exit, Flow};
use crate::state::{CoStatus, ExecState, MAX_C_CALLS, SavedState, Vm};
use orbit_core::Value;
use orbit_core::object::Thread;
use orbit_core::value::ThreadRef;

/// The status of a coroutine, as `coroutine.status` reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CoroutineStatus {
    /// Running: the coroutine asking.
    Running,
    /// Suspended in a yield, or not started yet.
    Suspended,
    /// Active but not running: it resumed another coroutine.
    Normal,
    /// Finished or stopped by an error.
    Dead,
}

impl CoroutineStatus {
    /// The name `coroutine.status` returns.
    pub fn name(self) -> &'static str {
        match self {
            CoroutineStatus::Running => "running",
            CoroutineStatus::Suspended => "suspended",
            CoroutineStatus::Normal => "normal",
            CoroutineStatus::Dead => "dead",
        }
    }
}

/// How a resume ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Resumed {
    /// The coroutine yielded this many values.
    Yielded(usize),
    /// The coroutine's body returned this many values.
    Returned(usize),
}

impl Resumed {
    /// Number of values transferred.
    pub fn count(self) -> usize {
        match self {
            Resumed::Yielded(n) | Resumed::Returned(n) => n,
        }
    }
}

impl Vm {
    /// Creates a coroutine that will run `f` (`lua_newthread` plus pushing the body).
    pub fn new_thread(&mut self, f: Value) -> ThreadRef {
        let saved = SavedState {
            top: 1,
            frames: Vec::new(),
            upval_slots: Vec::new(),
            tbc: Vec::new(),
            nny: 0,
            status: CoStatus::Fresh,
            nyield: 0,
        };
        self.heap.alloc(Thread {
            stack: vec![f],
            open_upvals: Vec::new(),
            vm_state: Some(Box::new(saved)),
        })
    }

    /// Internal status of a coroutine other than the running one.
    pub(crate) fn saved_status(&self, co: ThreadRef) -> CoStatus {
        self.heap
            .get(co)
            .vm_state
            .as_ref()
            .and_then(|s| s.downcast_ref::<SavedState>())
            .map_or(CoStatus::Dead, |s| s.status)
    }

    /// The status of coroutine `co` as seen from the running coroutine.
    pub fn thread_status(&self, co: ThreadRef) -> CoroutineStatus {
        if co == self.cur {
            return CoroutineStatus::Running;
        }
        match self.saved_status(co) {
            CoStatus::Fresh | CoStatus::Suspended => CoroutineStatus::Suspended,
            CoStatus::Active => CoroutineStatus::Normal,
            CoStatus::Dead | CoStatus::Failed => CoroutineStatus::Dead,
        }
    }

    /// Whether the running coroutine can yield (`lua_isyieldable`).
    pub fn is_yieldable(&self) -> bool {
        self.st.nny == 0
    }

    /// Whether coroutine `co` could yield once running.
    pub fn thread_is_yieldable(&self, co: ThreadRef) -> bool {
        if co == self.cur {
            return self.is_yieldable();
        }
        self.heap
            .get(co)
            .vm_state
            .as_ref()
            .and_then(|s| s.downcast_ref::<SavedState>())
            .is_some_and(|s| s.nny == 0)
    }

    /// Saves the running coroutine's state into its thread object.
    fn save_current(&mut self) {
        let st = std::mem::replace(&mut self.st, ExecState::new());
        let (upval_slots, uvs): (Vec<usize>, Vec<_>) = st.open_upvals.into_iter().unzip();
        let saved = SavedState {
            top: st.top,
            frames: st.frames,
            upval_slots,
            tbc: st.tbc,
            nny: st.nny,
            status: st.status,
            nyield: st.nyield,
        };
        let mut stack = st.stack;
        // Slots above the top are dead; dropping them keeps suspended coroutines small.
        stack.truncate(st.top);
        self.heap.update(self.cur, |t| {
            t.stack = stack;
            t.open_upvals = uvs;
            t.vm_state = Some(Box::new(saved));
        });
    }

    /// Makes `co` the running coroutine, loading its state.
    fn load_thread(&mut self, co: ThreadRef) {
        let (stack, uvs, saved) = self.heap.update(co, |t| {
            (
                std::mem::take(&mut t.stack),
                std::mem::take(&mut t.open_upvals),
                t.vm_state.take(),
            )
        });
        let saved = saved
            .and_then(|s| s.downcast::<SavedState>().ok())
            .expect("coroutine has saved state");
        self.st = ExecState {
            stack,
            top: saved.top,
            frames: saved.frames,
            open_upvals: saved.upval_slots.into_iter().zip(uvs).collect(),
            tbc: saved.tbc,
            nny: saved.nny,
            status: saved.status,
            nyield: saved.nyield,
        };
        self.cur = co;
        // Saving dropped the dead slots above the top; frames still expect their space.
        let need = self
            .st
            .frames
            .iter()
            .map(|f| f.top)
            .max()
            .unwrap_or(0)
            .max(self.st.top);
        self.ensure_stack(need + 1);
    }

    /// Switches from the running coroutine to `co`, remembering the current one.
    fn enter(&mut self, co: ThreadRef) {
        self.save_current();
        self.resumers.push(self.cur);
        self.load_thread(co);
    }

    /// Switches back to the coroutine that entered the running one.
    fn leave(&mut self) {
        self.save_current();
        let back = self.resumers.pop().expect("a coroutine to return to");
        self.load_thread(back);
    }

    /// Resumes coroutine `co` with the top `nargs` values of the running coroutine as
    /// arguments (`lua_resume`). On success the yielded or returned values replace the
    /// arguments. On failure the arguments are popped and the error returned; errors
    /// the coroutine raised leave it dead.
    pub fn resume(&mut self, co: ThreadRef, nargs: usize) -> LuaResult<Resumed> {
        let status = if co == self.cur {
            CoStatus::Active
        } else {
            self.saved_status(co)
        };
        let fail = |vm: &mut Vm, msg: &str| {
            vm.st.top -= nargs;
            Err(vm.error_msg(msg))
        };
        match status {
            CoStatus::Fresh | CoStatus::Suspended => {}
            CoStatus::Active => return fail(self, "cannot resume non-suspended coroutine"),
            CoStatus::Dead | CoStatus::Failed => return fail(self, "cannot resume dead coroutine"),
        }
        if self.c_calls >= MAX_C_CALLS {
            return fail(self, "C stack overflow");
        }
        let args_at = self.st.top - nargs;
        let args: Vec<Value> = self.st.stack[args_at..self.st.top].to_vec();
        self.st.top = args_at;
        self.enter(co);
        let top = self.st.top;
        self.ensure_stack(top + args.len() + crate::MIN_STACK);
        self.st.stack[top..top + args.len()].copy_from_slice(&args);
        self.st.top = top + args.len();
        self.c_calls += 1;
        let result = match status {
            CoStatus::Fresh => {
                self.st.status = CoStatus::Active;
                self.execute(0, |vm| vm.precall(0, -1))
            }
            _ => {
                self.st.status = CoStatus::Active;
                let n = args.len();
                self.execute(0, move |vm| vm.resume_native(n))
            }
        };
        self.c_calls -= 1;
        let (outcome, values) = match result {
            Ok(Exit::Returned) => {
                self.st.status = CoStatus::Dead;
                let vals = self.st.stack[..self.st.top].to_vec();
                self.st.top = 0;
                (Ok(Resumed::Returned(vals.len())), vals)
            }
            Ok(Exit::Yielded) => {
                self.st.status = CoStatus::Suspended;
                let n = self.st.nyield;
                let from = self.st.top - n;
                let vals = self.st.stack[from..self.st.top].to_vec();
                self.st.top = from;
                (Ok(Resumed::Yielded(n)), vals)
            }
            Err(e) => {
                self.st.status = CoStatus::Failed;
                // Keep the error object on the dead coroutine's stack for `close`.
                self.push(e.value);
                (Err(e), Vec::new())
            }
        };
        self.leave();
        let top = self.st.top;
        self.ensure_stack(top + values.len() + 1);
        self.st.stack[top..top + values.len()].copy_from_slice(&values);
        self.st.top = top + values.len();
        outcome
    }

    /// Continues a coroutine suspended in a native function's yield: the `n` resume
    /// arguments on top become its results, or go to its continuation.
    fn resume_native(&mut self, n: usize) -> LuaResult<Flow> {
        let fi = self.st.frames.len() - 1;
        let fr = self.st.frames[fi];
        if fr.k.is_some() {
            let action = self.continue_native(crate::KStatus::Yielded)?;
            self.native_action(action)
        } else {
            self.st.frames[fi].flags &= !crate::state::flag::YIELDED;
            let top = self.st.top;
            self.poscall(fi, top - n, n);
            Ok(Flow::Returned)
        }
    }

    /// Closes coroutine `co` (`lua_closethread`): runs its pending to-be-closed
    /// variables and kills it. Returns the error it died with, or one raised while
    /// closing, if any. `co` must be suspended or dead.
    pub fn close_thread(&mut self, co: ThreadRef) -> Option<Error> {
        let status = self.saved_status(co);
        self.enter(co);
        let err = if status == CoStatus::Failed {
            let v = self.st.stack[self.st.top - 1];
            Some(Error::new(v))
        } else {
            None
        };
        self.st.frames.clear();
        self.st.nny = 1;
        let err = self.close_protected(0, err);
        self.st.nny = 0;
        self.close_upvals(0);
        self.st.tbc.clear();
        self.st.top = 0;
        self.st.status = CoStatus::Dead;
        self.leave();
        err
    }

    /// Shuts the VM down the way `lua_close` does: closes the main coroutine's pending
    /// to-be-closed variables, then runs every pending `__gc` metamethod, reachable
    /// object or not. The VM stays usable, but objects may have been finalized.
    pub fn close_state(&mut self) {
        while !self.resumers.is_empty() {
            self.leave();
        }
        self.st.frames.clear();
        let _ = self.close_protected(0, None);
        self.st.top = 0;
        self.heap.gc_mut().finalize_all();
        let _ = self.run_finalizers();
    }
}
