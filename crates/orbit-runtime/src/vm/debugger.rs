//! The debugger engine (ADR-12): breakpoints, stepping, pause and stops on
//! errors, reported to the host's [`DebugEvents`].
//!
//! The engine runs on the hooked dispatch loop, but only where it needs to.
//! A function runs hooked while it has a breakpoint or a step could stop in
//! it; the run loop asks [`Vm::hooked_here`] at `'reload`, after a native
//! returns, and on every Lua call and return while [`Debugger::watch`] is
//! set, and switches loops when the answer changes. Everything else runs in
//! the plain loop at full speed, and with nothing set the only cost is the
//! test of `watch` on calls and returns.
//!
//! Which functions have breakpoints is worked out lazily: each
//! [`ProtoCode`] keeps a mark of the breakpoint generation it was last
//! checked against, so setting breakpoints is one counter bump plus a scan
//! of the source's prototypes to resolve lines, and a function is checked
//! again the next time it is entered. No code is patched (ADR-12, D6).
//!
//! A stop calls the host's [`DebugEvents::stopped`] from a native run above
//! a hook marker frame, as a Lua hook is called, so the host sees the
//! stopped program through the usual debug interface ([`Paused`]) and can
//! run code in it. Lua hooks and further stops are off meanwhile.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::object::{Closure, Gc, LuaHeap, NativeId, Proto, ProtoCode, Thread, Value};
use crate::vm::debug::Level;
use crate::vm::{FrameKind, LuaError, NativeCtx, NativeReturn, Vm};

/// Why the program stopped.
#[derive(Debug, Clone, Copy)]
pub enum StopReason {
    /// A breakpoint on this line.
    Breakpoint { line: u32 },
    /// A step finished.
    Step,
    /// [`PauseHandle::request`].
    Pause,
    /// An error is about to unwind the stack, which is still intact.
    /// `caught` says whether a `pcall`, `xpcall` or `coroutine.resume`
    /// will catch it; otherwise it leaves the VM.
    Error { value: Value, caught: bool },
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

/// Which errors stop the program.
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

/// What the host learns from the engine. Called on the VM's own thread.
pub trait DebugEvents {
    /// The program stopped. `paused` shows its stack; the answer says how
    /// to go on.
    fn stopped(&mut self, paused: &mut Paused<'_, '_>, reason: StopReason) -> Resume;

    /// A chunk was loaded that resolves some of `source`'s breakpoints:
    /// all of them, as [`Vm::set_breakpoints`] would now answer.
    fn breakpoints_resolved(&mut self, source: &[u8], breakpoints: &[Breakpoint]) {
        let _ = (source, breakpoints);
    }
}

/// The stopped program, as [`DebugEvents::stopped`] sees it.
pub struct Paused<'a, 'v> {
    ctx: &'a mut NativeCtx<'v>,
}

impl std::fmt::Debug for Paused<'_, '_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Paused")
    }
}

impl<'v> Paused<'_, 'v> {
    /// Activation `n` of the stopped stack: 0 is the function that stopped,
    /// 1 its caller, and so on.
    pub fn level(&self, n: usize) -> Option<Level> {
        // Level 0 of the context is the native reporting the stop; the
        // hook marker below it is no activation.
        self.ctx.level(n + 1)
    }

    /// [`Paused::ctx`], read only.
    pub fn ctx_ref(&self) -> &NativeCtx<'v> {
        self.ctx
    }

    /// The context the stop is reported from, for the rest of the debug
    /// interface (locals, upvalues, calling functions). Its own level 0 is
    /// the reporting native: add one to [`Paused::level`]'s numbers.
    pub fn ctx(&mut self) -> &mut NativeCtx<'v> {
        self.ctx
    }
}

/// Asks a VM to stop at its next line, from any thread. Like an
/// [`crate::vm::Interrupt`], it takes effect at the running code's next
/// loop back edge, call into the hooked loop or return from a native.
#[derive(Debug, Clone)]
pub struct PauseHandle {
    pause: Arc<AtomicBool>,
    interrupt: Arc<AtomicBool>,
}

impl PauseHandle {
    pub fn request(&self) {
        self.pause.store(true, Ordering::Relaxed);
        self.interrupt.store(true, Ordering::Relaxed);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StepKind {
    In,
    Over,
    Out,
    /// A pause: stop at the next instruction of a script function.
    Pause,
}

#[derive(Debug, Clone, Copy)]
struct Step {
    kind: StepKind,
    /// The stack depth, thread and function the step started in. The
    /// function runs hooked at every depth while the step lasts, so a
    /// recursive call or return, which keeps the loop it is in, can't
    /// leave it plain where the step must stop.
    depth: usize,
    thread: Gc<Thread>,
    proto: Option<Gc<Proto>>,
}

/// `old_pc` when the next instruction starts a line whatever it is.
const FRESH: u32 = u32::MAX;

/// A VM's debugger state.
#[derive(Default)]
pub(crate) struct Debugger {
    /// Something may make code run hooked: a breakpoint or a step. The run
    /// loop tests this on calls and returns.
    pub(crate) watch: bool,
    events: Option<Box<dyn DebugEvents>>,
    /// Breakpoints by chunk source (`@file.lua`).
    breakpoints: HashMap<Box<[u8]>, Vec<Breakpoint>>,
    /// Bumped whenever breakpoints change, so functions are checked again
    /// ([`ProtoCode::debug_mark`]). Never 0, the mark of an unchecked
    /// function.
    generation: u32,
    step: Option<Step>,
    on_error: ErrorBreak,
    pause: Arc<AtomicBool>,
    /// A stop is being reported: no more stops until it returns.
    active: bool,
    /// The stop being reported, and the answer, passed through the
    /// reporting native.
    pending: Option<StopReason>,
    resume: Resume,
    native: Option<NativeId>,
    /// The instruction the line check last saw, and its frame depth.
    old_pc: u32,
    old_depth: u32,
}

impl std::fmt::Debug for Debugger {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Debugger")
            .field("watch", &self.watch)
            .field("attached", &self.events.is_some())
            .field("breakpoints", &self.breakpoints.len())
            .field("step", &self.step)
            .finish_non_exhaustive()
    }
}

impl Debugger {
    fn update_watch(&mut self) {
        self.watch = self.events.is_some()
            && (self.step.is_some() || self.breakpoints.values().any(|b| !b.is_empty()));
    }

    /// Whether `code` (of `closure`) has a breakpoint, checking it again if
    /// breakpoints changed since it was last checked.
    #[inline]
    fn marked(&self, heap: &LuaHeap, code: &ProtoCode, closure: Gc<Closure>) -> bool {
        let mark = code.debug_mark.get();
        if mark >> 1 == self.generation {
            return mark & 1 != 0;
        }
        let has = !code.builtin && self.proto_has_breakpoint(heap.get(heap.get(closure).proto));
        code.debug_mark.set(self.generation << 1 | u32::from(has));
        has
    }

    fn proto_has_breakpoint(&self, proto: &Proto) -> bool {
        let Some(bps) = self.breakpoints.get(&*proto.source) else {
            return false;
        };
        proto
            .debug
            .lines
            .iter()
            .any(|&(_, line)| bps.iter().any(|b| b.line == Some(line)))
    }

    /// Whether a resolved breakpoint of `proto`'s source is on `line`.
    fn breakpoint_on(&self, proto: &Proto, line: u32) -> bool {
        self.breakpoints
            .get(&*proto.source)
            .is_some_and(|bps| bps.iter().any(|b| b.line == Some(line)))
    }

    fn bump(&mut self) {
        // Marks hold the generation in 31 bits; 0 is no generation.
        self.generation = (self.generation + 1) & (u32::MAX >> 1);
        if self.generation == 0 {
            self.generation = 1;
        }
    }

    /// Resolve the unresolved breakpoints of a chunk just linked, whose
    /// functions are `protos`.
    pub(crate) fn linked(&mut self, heap: &LuaHeap, protos: &[Gc<Proto>]) {
        let Some(&first) = protos.first() else {
            return;
        };
        let source = &heap.get(first).source;
        let Some(bps) = self.breakpoints.get_mut(&**source) else {
            return;
        };
        if bps.iter().all(|b| b.line.is_some()) {
            return;
        }
        let lines = code_lines(protos.iter().map(|&p| heap.get(p)));
        let mut changed = false;
        for b in bps.iter_mut().filter(|b| b.line.is_none()) {
            b.line = resolve(&lines, b.requested);
            changed |= b.line.is_some();
        }
        if changed {
            let source = source.clone();
            let bps = bps.clone();
            self.bump();
            if let Some(events) = &mut self.events {
                events.breakpoints_resolved(&source, &bps);
            }
        }
    }
}

/// The sorted lines that have code in `protos`.
fn code_lines<'p>(protos: impl Iterator<Item = &'p Proto>) -> Vec<u32> {
    let mut lines: Vec<u32> = protos
        .flat_map(|p| p.debug.lines.iter().map(|&(_, line)| line))
        .collect();
    lines.sort_unstable();
    lines.dedup();
    lines
}

/// The first line at or after `requested` in `lines`.
fn resolve(lines: &[u32], requested: u32) -> Option<u32> {
    lines
        .get(lines.partition_point(|&l| l < requested))
        .copied()
}

impl Vm {
    /// Report stops to `events` from now on (`None` detaches it, which also
    /// ends any step).
    pub fn set_debug_events(&mut self, events: Option<Box<dyn DebugEvents>>) {
        let d = &mut self.rt.debug;
        d.events = events;
        if d.events.is_none() {
            d.step = None;
        }
        d.update_watch();
    }

    /// Replace the breakpoints of `source` (a chunk's source as given to
    /// the loader, `@file.lua` for a file) with breakpoints on `lines`,
    /// and say where each one is. Breakpoints of a source not loaded yet
    /// resolve when it is ([`DebugEvents::breakpoints_resolved`]).
    pub fn set_breakpoints(&mut self, source: &[u8], lines: &[u32]) -> Vec<Breakpoint> {
        let mut code = Vec::new();
        self.heap.for_each(|p: &Proto| {
            if *p.source == *source && !p.code.builtin {
                code.extend(p.debug.lines.iter().map(|&(_, line)| line));
            }
        });
        code.sort_unstable();
        code.dedup();
        let bps: Vec<Breakpoint> = lines
            .iter()
            .map(|&requested| Breakpoint {
                requested,
                line: resolve(&code, requested),
            })
            .collect();
        let d = &mut self.rt.debug;
        if bps.is_empty() {
            d.breakpoints.remove(source);
        } else {
            d.breakpoints.insert(source.into(), bps.clone());
        }
        d.bump();
        d.update_watch();
        bps
    }

    /// Which errors stop the program ([`StopReason::Error`]).
    pub fn set_error_break(&mut self, on: ErrorBreak) {
        self.rt.debug.on_error = on;
    }

    /// A handle that pauses this VM from any thread.
    pub fn pause_handle(&self) -> PauseHandle {
        PauseHandle {
            pause: self.rt.debug.pause.clone(),
            interrupt: self.rt.interrupt.clone(),
        }
    }

    /// Whether the code of `closure` at stack depth `depth` must run in the
    /// hooked loop: a hook is set, or the debugger may stop there.
    #[inline(always)]
    pub(crate) fn hooked_here(&self, code: &ProtoCode, closure: Gc<Closure>, depth: usize) -> bool {
        self.rt.stack.hook.armed()
            || (self.rt.debug.watch && self.debug_wants_fast(code, closure, depth))
    }

    /// [`Vm::debug_wants`] answered from the code's mark when no step is
    /// set and the mark is current, the common case on calls and returns
    /// while breakpoints are set elsewhere.
    #[inline(always)]
    fn debug_wants_fast(&self, code: &ProtoCode, closure: Gc<Closure>, depth: usize) -> bool {
        let d = &self.rt.debug;
        let mark = code.debug_mark.get();
        if d.step.is_none() && mark >> 1 == d.generation {
            return mark & 1 != 0;
        }
        self.debug_wants(code, closure, depth)
    }

    #[cold]
    #[inline(never)]
    fn debug_wants(&self, code: &ProtoCode, closure: Gc<Closure>, depth: usize) -> bool {
        let d = &self.rt.debug;
        if let Some(s) = d.step {
            // A coroutine the stepping thread resumes runs plain unless
            // stepping in: its yield comes back through a reload, which asks
            // again.
            let here = s.thread == self.rt.current;
            let wanted = match s.kind {
                StepKind::In | StepKind::Pause => true,
                StepKind::Over => here && depth <= s.depth,
                StepKind::Out => here && depth < s.depth,
            };
            if wanted || s.proto == Some(self.heap.get(closure).proto) {
                return true;
            }
        }
        d.marked(&self.heap, code, closure)
    }

    /// The run loop entered other code while [`Debugger::watch`] is set:
    /// whether it must switch loops.
    #[inline(always)]
    pub(crate) fn debug_switches(
        &self,
        code: &ProtoCode,
        closure: Gc<Closure>,
        depth: usize,
        hooked: bool,
    ) -> bool {
        self.hooked_here(code, closure, depth) != hooked
    }

    /// Whether the top frame must run in the hooked loop.
    pub(crate) fn top_hooked(&self) -> bool {
        if self.rt.stack.hook.armed() {
            return true;
        }
        if !self.rt.debug.watch {
            return false;
        }
        let frames = &self.rt.stack.frames;
        match frames.last().map(|f| f.kind) {
            Some(FrameKind::Lua { closure, .. }) => {
                self.debug_wants(&self.heap.get(closure).code, closure, frames.len())
            }
            _ => false,
        }
    }

    /// The hooked loop reloaded its frame state at `pc`: what ran before
    /// was the instruction before it (a call, a yield), or nothing.
    #[inline]
    pub(crate) fn debug_reloaded(&mut self, pc: usize, depth: usize) {
        let d = &mut self.rt.debug;
        d.old_pc = pc.checked_sub(1).map_or(FRESH, |p| p as u32);
        d.old_depth = depth as u32;
    }

    /// The hooked loop's debugger check before the instruction at `pc` of
    /// the top frame runs (its saved pc is `pc + 1`).
    #[cold]
    #[inline(never)]
    pub(crate) fn debug_point(&mut self, pc: usize) -> Result<(), LuaError> {
        let d = &self.rt.debug;
        if d.active || d.events.is_none() {
            return Ok(());
        }
        let depth = self.rt.stack.frames.len();
        let FrameKind::Lua { closure, .. } = self.rt.stack.frames[depth - 1].kind else {
            unreachable!("debug points are in Lua frames")
        };
        let c = self.heap.get(closure);
        // Built-ins run like C functions; stripped code has no lines.
        let proto = self.heap.get(c.proto);
        if c.code.builtin || proto.debug.lines.is_empty() {
            return Ok(());
        }
        let lines = &proto.debug;
        let old = if d.old_depth != depth as u32 {
            pc.checked_sub(1)
        } else if d.old_pc == FRESH {
            None
        } else {
            Some(d.old_pc as usize)
        };
        let line = lines.line(pc as u32);
        let new_line = pc == 0
            || match old {
                None => true,
                Some(o) => pc <= o || line != lines.line(o as u32),
            };
        let here = d.step.is_some_and(|s| s.thread == self.rt.current);
        let reason = match d.step {
            Some(s) => match s.kind {
                StepKind::Pause => Some(StopReason::Pause),
                StepKind::In if new_line || (here && depth < s.depth) => Some(StopReason::Step),
                StepKind::Over if here && (depth < s.depth || depth == s.depth && new_line) => {
                    Some(StopReason::Step)
                }
                StepKind::Out if here && depth < s.depth => Some(StopReason::Step),
                _ => None,
            },
            None => None,
        };
        let reason = reason.or_else(|| {
            (new_line && d.marked(&self.heap, &c.code, closure) && d.breakpoint_on(proto, line))
                .then_some(StopReason::Breakpoint { line })
        });
        let d = &mut self.rt.debug;
        d.old_pc = pc as u32;
        d.old_depth = depth as u32;
        match reason {
            Some(r) => self.debug_stop(r),
            None => Ok(()),
        }
    }

    /// A pause was requested: the interrupt that carried it is handled.
    pub(crate) fn take_pause(&mut self) -> bool {
        if !self.rt.debug.pause.swap(false, Ordering::Relaxed) {
            return false;
        }
        self.clear_interrupt();
        let d = &mut self.rt.debug;
        if d.events.is_some() && !d.active {
            d.step = Some(Step {
                kind: StepKind::Pause,
                depth: self.rt.stack.frames.len(),
                thread: self.rt.current,
                proto: None,
            });
            d.update_watch();
        }
        true
    }

    /// An error is about to unwind: stop first if errors of its kind should.
    #[cold]
    pub(crate) fn debug_error(&mut self, value: Value) {
        let d = &self.rt.debug;
        if d.on_error == ErrorBreak::Never || d.active || d.events.is_none() {
            return;
        }
        let caught = self
            .rt
            .stack
            .frames
            .iter()
            .rev()
            .find_map(|f| match f.kind {
                FrameKind::Native {
                    catch: crate::vm::Catch::Yes | crate::vm::Catch::Handler,
                    ..
                }
                | FrameKind::CoBase => Some(true),
                FrameKind::Boundary { .. } | FrameKind::Hook => Some(false),
                _ => None,
            })
            == Some(true);
        if caught && d.on_error == ErrorBreak::Uncaught {
            return;
        }
        // The stop runs code above the failing frames; the error's own
        // state waits.
        let (errerr, handled) = (self.rt.errerr, self.rt.handled);
        let traceback = self.rt.unwind_traceback.take();
        // An error from the host's callback is the host's to report.
        let _ = self.debug_stop(StopReason::Error { value, caught });
        self.rt.errerr = errerr;
        self.rt.handled = handled;
        self.rt.unwind_traceback = traceback;
    }

    /// Report a stop and take the host's answer.
    fn debug_stop(&mut self, reason: StopReason) -> Result<(), LuaError> {
        let depth = self.rt.stack.frames.len();
        let thread = self.rt.current;
        let proto = match self.rt.stack.frames[depth - 1].kind {
            FrameKind::Lua { closure, .. } => Some(self.heap.get(closure).proto),
            _ => None,
        };
        let id = match self.rt.debug.native {
            Some(id) => id,
            None => {
                let Value::Native(id) = self.register_native("debugger", report_stop) else {
                    unreachable!("a native")
                };
                self.rt.debug.native = Some(id);
                id
            }
        };
        let d = &mut self.rt.debug;
        d.pending = Some(reason);
        d.active = true;
        d.step = None;
        let hook_active = std::mem::replace(&mut self.rt.stack.hook.active, true);
        let r = self.call_under_marker(&[Value::Native(id)]);
        self.rt.stack.hook.active = hook_active;
        let d = &mut self.rt.debug;
        d.active = false;
        d.pending = None;
        let kind = match std::mem::take(&mut d.resume) {
            Resume::Continue => None,
            Resume::StepIn => Some(StepKind::In),
            Resume::StepOver => Some(StepKind::Over),
            Resume::StepOut => Some(StepKind::Out),
        };
        d.step = kind.map(|kind| Step {
            kind,
            depth,
            thread,
            // Stepping in stops anywhere, so it runs hooked everywhere.
            proto: proto.filter(|_| kind != StepKind::In),
        });
        d.update_watch();
        r
    }
}

/// The native a stop is reported from.
fn report_stop(ctx: &mut NativeCtx<'_>) -> Result<NativeReturn, LuaError> {
    let Some(reason) = ctx.vm.rt.debug.pending.take() else {
        return Ok(NativeReturn::Return(0));
    };
    let mut events = ctx.vm.rt.debug.events.take();
    let resume = match &mut events {
        Some(e) => e.stopped(&mut Paused { ctx }, reason),
        None => Resume::Continue,
    };
    let d = &mut ctx.vm.rt.debug;
    // Unless the host attached other events meanwhile.
    if d.events.is_none() {
        d.events = events;
    }
    d.resume = resume;
    Ok(NativeReturn::Return(0))
}
