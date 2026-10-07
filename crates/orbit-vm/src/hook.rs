//! Debug hooks (`debug.sethook`): the per-stack hook state and the cold
//! paths that call a hook ([debug design]).
//!
//! The dispatch loop has two instantiations ([`Vm::run`]): the plain one
//! checks only whether a hook was armed, at `'reload` and after a native
//! returns in place, and switches; the hooked one (compiled with the
//! `hooks` feature) calls [`Vm::hook_point`] before every instruction.
//!
//! A hook runs as an ordinary nested call above a [`FrameKind::Hook`]
//! marker frame, which names it (`hook '?'`) for `debug.getinfo` and
//! tracebacks and, like a boundary frame, ends the call. Lua hooks can't yield (the reference raises "attempt to yield
//! across a C-call boundary"), so the nested call costs no capability.
//!
//! [debug design]: https://github.com/tymlipari/OrbitScript/blob/main/docs/design/debug.md

use orbit_bytecode::{MULTI, Op};
use orbit_core::{NativeId, Value};

use crate::{Frame, FrameKind, LuaError, Vm};

/// Hook event bits ([`Hook::mask`]).
pub const CALL: u8 = 1;
pub const RETURN: u8 = 2;
pub const LINE: u8 = 4;
pub const COUNT: u8 = 8;
/// Not an event: an [`crate::Interrupt`] was seen, and the next hook point
/// raises `interrupted!`. Arms the hooked loop like an event does.
pub(crate) const INTERRUPT: u8 = 16;
/// The events `sethook` controls.
const EVENTS: u8 = CALL | RETURN | LINE | COUNT;

/// `old_pc` when the frame at `old_depth` was just entered: its first
/// instruction always starts a line.
const FRESH: u32 = u32::MAX;

/// A stack's hook.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Hook {
    /// The hook function (nil when off).
    pub func: Value,
    /// Which events fire ([`CALL`], [`RETURN`], [`LINE`], [`COUNT`]).
    pub mask: u8,
    /// The count interval as given to `sethook` (count events only when
    /// positive).
    pub base_count: i64,
    /// Instructions left until the next count event.
    pub count: u32,
    /// A hook is running: no events fire until it returns.
    pub active: bool,
    /// The instruction the line check last saw, and the frame depth it
    /// belongs to.
    pub old_pc: u32,
    pub old_depth: u32,
    /// A Lua frame was just entered (`Some(true)` for a tail call): the
    /// next hook point fires its call event.
    pub call_pending: Option<bool>,
    /// While a call or return hook runs: the frame index it is for and the
    /// values it transfers (`ftransfer`, `ntransfer`).
    pub transfer: Option<(usize, u32, u32)>,
}

impl Default for Hook {
    fn default() -> Self {
        Hook {
            func: Value::Nil,
            mask: 0,
            base_count: 0,
            count: 0,
            active: false,
            old_pc: 0,
            old_depth: 0,
            call_pending: None,
            transfer: None,
        }
    }
}

impl Hook {
    /// Whether the hooked dispatch loop should run.
    #[inline(always)]
    pub fn armed(&self) -> bool {
        cfg!(feature = "hooks") && self.mask != 0
    }

    /// Whether any of `events` would fire now.
    #[inline(always)]
    pub fn wants(&self, events: u8) -> bool {
        cfg!(feature = "hooks") && self.mask & events != 0 && !self.active
    }

    /// The hook a thread created now starts with: the mask and count, but
    /// no function.
    pub fn inherited(&self) -> Hook {
        Hook {
            func: Value::Nil,
            mask: self.mask & EVENTS,
            base_count: self.base_count,
            count: self.base_count.clamp(0, u32::MAX as i64) as u32,
            ..Hook::default()
        }
    }

    /// Set the hook (`mask` 0 or a nil `func` turns it off).
    pub fn set(&mut self, func: Value, mut mask: u8, count: i64) {
        if count > 0 {
            mask |= COUNT;
        } else {
            mask &= !COUNT;
        }
        if func.is_nil() {
            mask = 0;
        }
        self.func = if mask == 0 { Value::Nil } else { func };
        self.mask = mask | self.mask & !EVENTS;
        self.base_count = if mask == 0 { 0 } else { count };
        self.count = count.clamp(0, u32::MAX as i64) as u32;
        // The next instruction counts as following the call that set the
        // hook, not as a new function.
        self.old_depth = 0;
        self.call_pending = None;
    }
}

impl Vm {
    /// The hooked loop's check before the instruction at `pc` (`op`) runs.
    /// The frame's saved pc is `pc + 1`, as while the instruction runs.
    #[cold]
    #[inline(never)]
    pub(crate) fn hook_point(&mut self, pc: usize, op: Op) -> Result<(), LuaError> {
        // Not inside a hook, which runs without events, as the
        // reference's interrupt is one.
        if !self.rt.stack.hook.active
            && (self.rt.stack.hook.mask & INTERRUPT != 0
                || self.rt.interrupt.load(std::sync::atomic::Ordering::Relaxed))
        {
            return Err(self.interrupted());
        }
        let h = &mut self.rt.stack.hook;
        if h.active || h.mask == 0 {
            h.call_pending = None;
            return Ok(());
        }
        let depth = self.rt.stack.frames.len();
        let fi = depth - 1;
        let frame = self.rt.stack.frames[fi];
        let FrameKind::Lua { closure, .. } = frame.kind else {
            unreachable!("hook points are in Lua frames")
        };
        if let Some(tail) = h.call_pending.take() {
            h.old_pc = FRESH;
            h.old_depth = depth as u32;
            if h.mask & CALL != 0 {
                let np = self.heap.get(closure).code.nparams as u32;
                let event: &[u8] = if tail { b"tail call" } else { b"call" };
                self.call_hook(event, None, Some((fi, 1, np)))?;
            }
        }
        let h = &mut self.rt.stack.hook;
        // A count event makes the same instruction report its line again
        // (observed from lua5.5).
        let mut counted = false;
        if h.mask & COUNT != 0 && !h.active {
            h.count = h.count.saturating_sub(1);
            if h.count == 0 {
                h.count = h.base_count.clamp(0, u32::MAX as i64) as u32;
                counted = true;
                self.call_hook(b"count", None, None)?;
            }
        }
        let h = &mut self.rt.stack.hook;
        if h.mask & LINE != 0 {
            let proto = self.heap.get(self.heap.get(closure).proto);
            let lines = &proto.debug;
            if !lines.lines.is_empty() {
                let old = if h.old_depth != depth as u32 {
                    // Back from a call (or the call that set the hook):
                    // the last instruction seen is that call.
                    pc.checked_sub(1)
                } else if h.old_pc == FRESH {
                    None
                } else {
                    Some(h.old_pc as usize)
                };
                let line = lines.line(pc as u32);
                let fire = counted
                    || match old {
                        None => true,
                        Some(o) => pc <= o || line != lines.line(o as u32),
                    };
                h.old_pc = pc as u32;
                h.old_depth = depth as u32;
                if fire {
                    self.call_hook(b"line", Some(line), None)?;
                }
            }
        }
        if self.rt.stack.hook.mask & RETURN != 0 {
            let base = frame.base as usize;
            let ret = match op {
                Op::Return { first, count } => {
                    let src = base + first as usize;
                    let n = if count == MULTI {
                        self.rt.top - src
                    } else {
                        count as usize
                    };
                    Some((first as usize, n))
                }
                Op::Return0 => Some((0, 0)),
                Op::Return1 { src } => Some((src as usize, 1)),
                _ => None,
            };
            if let Some((first, n)) = ret {
                let first = self.rt.register_local(&self.heap, fi, first);
                self.call_hook(b"return", None, Some((fi, first, n as u32)))?;
            }
        }
        Ok(())
    }

    /// Call the hook with `(event, line)` above everything on the stack.
    /// `transfer` is what `getinfo`'s `r` option reports meanwhile.
    /// The `interrupted!` error, which clears the request. Like the
    /// reference's, it names the position of the interrupted function's
    /// caller.
    #[cold]
    fn interrupted(&mut self) -> LuaError {
        self.rt.stack.hook.mask &= !INTERRUPT;
        self.rt
            .interrupt
            .store(false, std::sync::atomic::Ordering::Relaxed);
        let msg = format!("{}interrupted!", self.rt.where_(&self.heap, None, 2));
        LuaError::new(self.new_str(msg.as_bytes()))
    }

    pub(crate) fn call_hook(
        &mut self,
        event: &[u8],
        line: Option<u32>,
        transfer: Option<(usize, u32, u32)>,
    ) -> Result<(), LuaError> {
        let h = self.rt.stack.hook;
        if h.active || h.func.is_nil() {
            return Ok(());
        }
        let ev = self.new_str(event);
        // Above everything live: the top frame's registers and the last
        // open results (or a native's window).
        let top = self.rt.stack.frames.last().expect("a frame");
        let end = match top.kind {
            FrameKind::Lua { closure, .. } => {
                top.base as usize + self.heap.get(closure).code.nregs as usize
            }
            _ => top.base as usize,
        };
        let old_top = self.rt.top;
        let at = end.max(old_top);
        self.rt.stack.frames.push(Frame {
            kind: FrameKind::Hook,
            func: at as u32,
            base: at as u32,
            pc: 0,
            cell_base: self.rt.stack.cells.len() as u32,
            nvarargs: 0,
            want: 0,
            tail: false,
            extra: 0,
        });
        let line = line.map_or(Value::Nil, |l| Value::Int(l as i64));
        self.rt.top = self.rt.stack.write(at, &[h.func, ev, line]);
        self.rt.stack.hook.active = true;
        self.rt.stack.hook.transfer = transfer;
        // The marker is a boundary: the call returns, or its error unwinds,
        // into it, and either pops it.
        let r = self.call_at(at, 2);
        let hook = &mut self.rt.stack.hook;
        hook.active = false;
        hook.transfer = None;
        self.rt.top = old_top;
        r
    }

    /// Fire a native's call event under a temporary frame, so the hook sees
    /// the native at level 2.
    #[cold]
    #[inline(never)]
    pub(crate) fn native_call_hook(
        &mut self,
        func: usize,
        nargs: usize,
        id: NativeId,
    ) -> Result<(), LuaError> {
        self.with_native_frame(func, id, |vm, fi| {
            vm.call_hook(b"call", None, Some((fi, 1, nargs as u32)))
        })
    }

    /// Fire a frameless native's return event for its `n` results (on top
    /// of the stack), under a temporary frame.
    #[cold]
    #[inline(never)]
    pub(crate) fn native_return_hook(
        &mut self,
        func: usize,
        id: NativeId,
        n: usize,
    ) -> Result<(), LuaError> {
        let first = self.rt.top - n - func;
        self.with_native_frame(func, id, |vm, fi| {
            vm.call_hook(b"return", None, Some((fi, first as u32, n as u32)))
        })
    }

    fn with_native_frame(
        &mut self,
        func: usize,
        id: NativeId,
        f: impl FnOnce(&mut Vm, usize) -> Result<(), LuaError>,
    ) -> Result<(), LuaError> {
        let fi = self.rt.stack.frames.len();
        self.rt.stack.frames.push(Frame {
            kind: FrameKind::NativeCall { id },
            func: func as u32,
            base: func as u32 + 1,
            pc: 0,
            cell_base: self.rt.stack.cells.len() as u32,
            nvarargs: 0,
            want: 0,
            tail: false,
            extra: 0,
        });
        let r = f(self, fi);
        self.rt.stack.frames.truncate(fi);
        r
    }
}
