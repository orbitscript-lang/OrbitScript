//! The debugger interface (ADR-12): breakpoints, stepping, pausing and
//! stopping on errors, reported to a host's [`DebugEvents`].
//!
//! The engine lives in the runtime (feature `debugger`). An isolate runs at
//! full speed outside functions with breakpoints and while nothing steps;
//! a stop calls [`DebugEvents::stopped`] on the isolate's own thread with
//! the program's stack intact, and the answer ([`Resume`]) says how to go
//! on.
//!
//! ```
//! use orbit::debugger::{DebugEvents, Paused, Resume, StopReason};
//! use orbit::{Chunk, Config, Runtime};
//!
//! struct Lines(std::rc::Rc<std::cell::RefCell<Vec<i32>>>);
//!
//! impl DebugEvents for Lines {
//!     fn stopped(&mut self, paused: &mut Paused<'_, '_>, _why: StopReason) -> Resume {
//!         let line = paused.frame(0).map_or(-1, |f| f.currentline);
//!         self.0.borrow_mut().push(line);
//!         Resume::StepOver
//!     }
//! }
//!
//! let mut iso = Runtime::new().new_isolate(Config::default());
//! let seen = std::rc::Rc::default();
//! iso.set_debug_events(Some(Box::new(Lines(std::rc::Rc::clone(&seen))))).unwrap();
//! iso.set_breakpoints(b"=demo", &[2]);
//! iso.exec(Chunk::new("local a = 1\nlocal b = a + 1\nreturn a + b").name("=demo")).unwrap();
//! assert_eq!(*seen.borrow(), [2, 3]);
//! ```

use crate::error::Error;
use crate::pin::Pinned;
use crate::scope::Scope;
use crate::stack::{FrameInfo, Stack};
use crate::value::{Table, Value};

/// Why the program stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    /// A breakpoint on this line.
    Breakpoint { line: u32 },
    /// A step finished.
    Step,
    /// [`PauseHandle::request`].
    Pause,
    /// An error is about to unwind the stack, which is still intact
    /// ([`Paused::error`] gives it). `caught` says whether a `pcall`,
    /// `xpcall` or `coroutine.resume` will catch it; otherwise it leaves
    /// the isolate.
    Error { caught: bool },
}

/// How to go on after a stop.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Resume {
    /// Run until the next breakpoint.
    #[default]
    Continue,
    /// Stop at the next line, in any function.
    StepIn,
    /// Stop at the next line of this function, or in its caller once it
    /// returns.
    StepOver,
    /// Stop in the caller once this function returns.
    StepOut,
}

/// Which errors stop the program ([`crate::Isolate::set_error_break`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ErrorBreak {
    #[default]
    Never,
    /// Errors nothing in the script catches.
    Uncaught,
    /// Every error.
    All,
}

/// A breakpoint as set, and where it ended up: the first line at or after
/// the requested one that has code, or `None` while no loaded function of
/// its source has code there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Breakpoint {
    pub requested: u32,
    pub line: Option<u32>,
}

/// What the host learns from the debugger, on the isolate's own thread.
pub trait DebugEvents: 'static {
    /// The program stopped. `paused` shows its stack; the answer says how
    /// to go on.
    fn stopped(&mut self, paused: &mut Paused<'_, '_>, reason: StopReason) -> Resume;

    /// A chunk was loaded that resolves some of `source`'s breakpoints:
    /// all of them, as [`crate::Isolate::set_breakpoints`] would now
    /// answer.
    fn breakpoints_resolved(&mut self, source: &[u8], breakpoints: &[Breakpoint]) {
        let _ = (source, breakpoints);
    }
}

/// Asks an isolate to stop at its next line, from any thread
/// ([`crate::Isolate::pause_handle`]). Like an [`crate::Interrupt`], it
/// takes effect at the running code's next loop back edge, call or return
/// from a native. It does nothing while no debugger is attached.
#[derive(Debug, Clone)]
pub struct PauseHandle(pub(crate) crate::isolate::backend::PauseInner);

impl PauseHandle {
    pub fn request(&self) {
        self.0.request()
    }
}

/// A local variable or upvalue of a stopped function.
#[derive(Debug)]
pub struct Variable {
    pub name: Vec<u8>,
    /// Its number for [`Paused::set_local`] (locals) or `debug.getupvalue`
    /// (upvalues), counting from 1.
    pub index: i32,
    /// Kept alive until the host drops it, which a debugger does when the
    /// program goes on (DAP's variable references last only while
    /// stopped).
    pub value: Pinned<Value<'static>>,
}

/// The stopped program, as [`DebugEvents::stopped`] sees it. Levels count
/// activations from the function that stopped (0), through its callers.
pub struct Paused<'a, 'b> {
    s: &'a mut Stack<'b>,
    /// Whether slot 1 holds the error being raised.
    error: bool,
}

impl std::fmt::Debug for Paused<'_, '_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Paused").finish_non_exhaustive()
    }
}

impl<'a, 'b> Paused<'a, 'b> {
    /// `s` is the window of the native reporting the stop, with the error
    /// in slot 1 for an error stop.
    pub(crate) fn new(s: &'a mut Stack<'b>, error: bool) -> Paused<'a, 'b> {
        Paused { s, error }
    }

    /// Activation `level` of the stopped stack, as `debug.getinfo` sees
    /// it.
    pub fn frame(&self, level: usize) -> Option<FrameInfo> {
        // The window's level 0 is the native reporting the stop.
        self.s.level_info(level + 1)
    }

    /// Every activation, from the function that stopped outwards.
    pub fn frames(&self) -> Vec<FrameInfo> {
        (0..).map_while(|l| self.frame(l)).collect()
    }

    /// The error being raised, for [`StopReason::Error`].
    pub fn error(&mut self) -> Option<Pinned<Value<'static>>> {
        self.error.then(|| self.pin(1))
    }

    /// The locals of activation `level` in scope at the stop, leaving out
    /// the hidden ones the compilers make (named in parentheses).
    pub fn locals(&mut self, level: usize) -> Vec<Variable> {
        let mut out = Vec::new();
        for index in 1.. {
            let Some(name) = self.s.get_local(level + 1, index) else {
                break;
            };
            let value = self.pin(-1);
            let _ = self.s.set_top(-2);
            if !name.starts_with(b"(") {
                out.push(Variable { name, index, value });
            }
        }
        out
    }

    /// The upvalues of activation `level`'s function.
    pub fn upvalues(&mut self, level: usize) -> Vec<Variable> {
        self.s.push_level_function(level + 1);
        let mut out = Vec::new();
        for index in 1.. {
            let Some(name) = self.s.get_upvalue(-1, index) else {
                break;
            };
            let value = self.pin(-1);
            let _ = self.s.set_top(-2);
            out.push(Variable { name, index, value });
        }
        let _ = self.s.set_top(-2);
        out
    }

    /// Set local `index` of activation `level` (a [`Variable::index`]) to
    /// the value `value` makes.
    pub fn set_local(
        &mut self,
        level: usize,
        index: i32,
        value: impl for<'s> FnOnce(&'s Scope<'s>) -> Result<Value<'s>, Error>,
    ) -> Result<(), Error> {
        self.s.with_scope(|s| {
            let v = value(s)?;
            let mut stack = s.stack();
            v.push(&mut stack);
            match stack.set_local(level + 1, index) {
                Ok(Some(_)) => Ok(()),
                Ok(None) => {
                    let _ = stack.set_top(-2);
                    Err(Error::runtime(format!("no local {index} at level {level}")))
                }
                Err(e) => Err(s.error_of(&mut stack, e)),
            }
        })
    }

    /// The global table.
    pub fn globals(&mut self) -> Pinned<Table<'static>> {
        self.s.with_scope(|s| s.globals().pin())
    }

    /// Run `f` with a scope, to read pinned values or call script code
    /// (a watch expression).
    pub fn scope<R>(&mut self, f: impl for<'s> FnOnce(&'s Scope<'s>) -> R) -> R {
        self.s.with_scope(f)
    }

    /// The stack API over the reporting native's window. Its levels are
    /// one more than this view's: its level 0 is the native.
    pub fn stack(&mut self) -> &mut Stack<'b> {
        self.s
    }

    /// Pin the value at `idx` of the window.
    fn pin(&mut self, idx: i32) -> Pinned<Value<'static>> {
        let idx = self.s.abs_index(idx);
        self.s.with_scope(|s| s.pin_slot(idx))
    }
}

/// The boundary's forms of these types (ADR-10).
#[cfg(any(feature = "abi", shared_backend))]
pub(crate) mod abi {
    use super::{Breakpoint, ErrorBreak, Resume, StopReason};
    use crate::abi::{BreakpointAbi, StopAbi};

    #[cfg(all(feature = "abi", runtime_backend))]
    pub(crate) fn breakpoint_to(b: Breakpoint) -> BreakpointAbi {
        BreakpointAbi {
            requested: b.requested,
            line: b.line.unwrap_or(0),
        }
    }

    #[cfg(shared_backend)]
    pub(crate) fn breakpoint_from(b: BreakpointAbi) -> Breakpoint {
        Breakpoint {
            requested: b.requested,
            line: (b.line != 0).then_some(b.line),
        }
    }

    #[cfg(all(feature = "abi", runtime_backend))]
    pub(crate) fn stop_to(r: StopReason) -> StopAbi {
        let (kind, caught, line) = match r {
            StopReason::Breakpoint { line } => (StopAbi::BREAKPOINT, false, line),
            StopReason::Step => (StopAbi::STEP, false, 0),
            StopReason::Pause => (StopAbi::PAUSE, false, 0),
            StopReason::Error { caught } => (StopAbi::ERROR, caught, 0),
        };
        StopAbi { kind, caught, line }
    }

    #[cfg(shared_backend)]
    pub(crate) fn stop_from(r: StopAbi) -> StopReason {
        match r.kind {
            StopAbi::BREAKPOINT => StopReason::Breakpoint { line: r.line },
            StopAbi::STEP => StopReason::Step,
            StopAbi::PAUSE => StopReason::Pause,
            _ => StopReason::Error { caught: r.caught },
        }
    }

    #[cfg(shared_backend)]
    pub(crate) fn resume_to(r: Resume) -> u8 {
        match r {
            Resume::Continue => 0,
            Resume::StepIn => 1,
            Resume::StepOver => 2,
            Resume::StepOut => 3,
        }
    }

    #[cfg(all(feature = "abi", runtime_backend))]
    pub(crate) fn resume_from(code: u8) -> Resume {
        match code {
            1 => Resume::StepIn,
            2 => Resume::StepOver,
            3 => Resume::StepOut,
            _ => Resume::Continue,
        }
    }

    #[cfg(shared_backend)]
    pub(crate) fn error_break_to(on: ErrorBreak) -> u8 {
        match on {
            ErrorBreak::Never => 0,
            ErrorBreak::Uncaught => 1,
            ErrorBreak::All => 2,
        }
    }

    #[cfg(all(feature = "abi", runtime_backend))]
    pub(crate) fn error_break_from(code: u8) -> ErrorBreak {
        match code {
            1 => ErrorBreak::Uncaught,
            2 => ErrorBreak::All,
            _ => ErrorBreak::Never,
        }
    }
}
