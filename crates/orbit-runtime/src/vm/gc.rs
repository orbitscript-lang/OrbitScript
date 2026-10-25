//! Running `__gc` finalizers, closing a VM, and the collector state
//! `collectgarbage` reports ([GC semantics design]).
//!
//! `orbit-gc` keeps unreachable objects registered for finalization alive
//! and queues them, most recently registered first; this module calls their
//! `__gc` metamethods. A finalizer runs as a protected call that can't
//! yield. While one runs, automatic collection is suspended and
//! `collectgarbage` does nothing (it returns `nil`), so the collector is
//! never re-entered. An error in a finalizer becomes the warning
//! `error in __gc (msg)` and the next finalizer runs.
//!
//! Finalizers run at three points: after an automatic collection step in
//! the interpreter (a nested call), at the end of `collectgarbage("collect")`
//! and `collectgarbage("step")` (through [`NativeReturn::CallThen`], see
//! [`finalize_then_return`]), and when the VM closes ([`Vm::close`], or
//! [`close_then`] from a native, for `os.exit(code, true)`).
//!
//! [GC semantics design]: https://github.com/orbitscript-lang/OrbitScript/blob/main/docs/design/gc-semantics.md

use crate::object::gc::finalizer;
use crate::object::{LuaHeap, Value};

use crate::vm::{CallOutcome, Continuation, LuaError, NativeCtx, NativeReturn, Runtime, Vm};

/// The collector mode (`collectgarbage("incremental")` and
/// `collectgarbage("generational")`), applied to the heap by
/// [`Vm::set_gc_mode`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GcMode {
    Incremental,
    Generational,
}

impl GcMode {
    pub fn name(self) -> &'static str {
        match self {
            GcMode::Incremental => "incremental",
            GcMode::Generational => "generational",
        }
    }
}

/// A `collectgarbage("param", name)` parameter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GcParam {
    /// Wait between cycles, as a percentage of the heap the last cycle
    /// found reachable (`Params::growth` + 100).
    Pause,
    /// Collection work per byte allocated (`Params::work_ratio` / 4).
    StepMul,
    /// Bytes allocated between steps (`Params::step_bytes`); 0 makes every
    /// step a whole cycle.
    StepSize,
    /// Generational: allocation between minor collections, as a percentage
    /// of the heap the last major collection found reachable
    /// (`Params::minor_percent`).
    MinorMul,
    /// Generational: how much a major collection must free to go back to
    /// minor ones (`Params::minor_return_percent`).
    MajorMinor,
    /// Generational: heap growth past the last major collection's live
    /// size that makes the next collection major (`Params::major_percent`).
    MinorMajor,
}

impl GcParam {
    pub fn from_name(name: &[u8]) -> Option<GcParam> {
        Some(match name {
            b"pause" => GcParam::Pause,
            b"stepmul" => GcParam::StepMul,
            b"stepsize" => GcParam::StepSize,
            b"minormul" => GcParam::MinorMul,
            b"majorminor" => GcParam::MajorMinor,
            b"minormajor" => GcParam::MinorMajor,
            _ => return None,
        })
    }
}

/// The default floor under the start of a cycle in generational mode.
const GC_FLOOR: usize = 1 << 20;

/// [`GC_FLOOR`], or the size `ORBIT_GC_MIN_THRESHOLD` gives (bytes, or with
/// `K`, `M` or `G`; `0` turns the floor off). An unreadable value keeps the
/// default.
fn gc_floor() -> usize {
    let Some(s) = std::env::var_os("ORBIT_GC_MIN_THRESHOLD") else {
        return GC_FLOOR;
    };
    let s = s.to_str().unwrap_or("");
    let (digits, shift) = match s.as_bytes().last() {
        Some(b'K' | b'k') => (&s[..s.len() - 1], 10),
        Some(b'M' | b'm') => (&s[..s.len() - 1], 20),
        Some(b'G' | b'g') => (&s[..s.len() - 1], 30),
        _ => (s, 0),
    };
    digits
        .parse::<usize>()
        .ok()
        .and_then(|n| n.checked_mul(1 << shift))
        .unwrap_or(GC_FLOOR)
}

/// How `stepmul` scales to `Params::work_ratio`: our default work ratio of
/// 800 reads as Lua's default `stepmul` of 200.
const STEPMUL_SCALE: u32 = 4;

/// What `collectgarbage` keeps besides the collector's own state.
#[derive(Debug)]
pub struct GcState {
    /// Set with [`Vm::set_gc_mode`], which also moves the heap's floor.
    pub mode: GcMode,
    /// The floor under the start of a cycle in generational mode
    /// ([`gc_floor`]).
    floor: usize,
    /// The parameters as last set, so they read back exactly. `pause`,
    /// `stepmul` and `stepsize` are also applied to the heap's `Params`.
    params: [i64; 6],
    /// A finalizer is running.
    finalizing: bool,
    /// Whether the collector was stopped when the running finalizer
    /// started.
    was_stopped: bool,
    /// Whether the running stack's hooks were already off when the running
    /// finalizer started. Finalizers run without hook events, as in lua5.5.
    hooks_were_off: bool,
    closed: bool,
    /// What [`close_then`] runs once the last finalizer has.
    close_k: Option<Continuation>,
}

impl GcState {
    pub(crate) fn new(heap: &LuaHeap) -> Self {
        let p = heap.params();
        GcState {
            mode: GcMode::Incremental,
            floor: gc_floor(),
            params: [
                i64::from(p.growth) + 100,
                i64::from(p.work_ratio / STEPMUL_SCALE),
                p.step_bytes as i64,
                i64::from(p.minor_percent),
                i64::from(p.minor_return_percent),
                i64::from(p.major_percent),
            ],
            finalizing: false,
            was_stopped: false,
            hooks_were_off: false,
            closed: false,
            close_k: None,
        }
    }

    /// Record the mode and move the heap's floor; [`Vm::set_gc_mode`]
    /// switches the heap too. Generational mode puts a floor under the heap
    /// size that starts a major collection, so small programs don't run a
    /// full collection every few kilobytes (minor collections keep their
    /// own schedule). Incremental mode keeps Lua's pacing exactly (the next
    /// cycle starts at `pause` percent of the live heap, however small).
    pub(crate) fn set_mode(&mut self, heap: &mut LuaHeap, mode: GcMode) -> GcMode {
        heap.set_min_threshold(match mode {
            GcMode::Incremental => 0,
            GcMode::Generational => self.floor,
        });
        std::mem::replace(&mut self.mode, mode)
    }

    /// Whether a finalizer is running: `collectgarbage` must do nothing.
    pub fn finalizing(&self) -> bool {
        self.finalizing
    }

    pub fn param(&self, p: GcParam) -> i64 {
        self.params[p as usize]
    }

    /// Set a parameter (a non-negative value) and apply it to the heap.
    /// Returns the old value.
    pub fn set_param(&mut self, heap: &mut LuaHeap, p: GcParam, v: i64) -> i64 {
        let old = std::mem::replace(&mut self.params[p as usize], v);
        let u32_of = |v: i64| u32::try_from(v).unwrap_or(u32::MAX);
        let params = heap.params_mut();
        match p {
            GcParam::Pause => params.growth = u32_of(v.saturating_sub(100).max(0)),
            GcParam::StepMul => params.work_ratio = u32_of(v).saturating_mul(STEPMUL_SCALE),
            GcParam::StepSize => params.step_bytes = usize::try_from(v).unwrap_or(usize::MAX),
            GcParam::MinorMul => params.minor_percent = u32_of(v),
            GcParam::MinorMajor => params.major_percent = u32_of(v),
            GcParam::MajorMinor => params.minor_return_percent = u32_of(v),
        }
        old
    }

    /// Suspend automatic collection, and the hook of the stack it runs
    /// on, while a finalizer runs.
    fn enter(&mut self, heap: &mut LuaHeap, hook: &mut crate::vm::hook::Hook) {
        debug_assert!(!self.finalizing);
        self.finalizing = true;
        self.was_stopped = heap.is_stopped();
        heap.stop();
        self.hooks_off(hook);
    }

    fn exit(&mut self, heap: &mut LuaHeap, hook: &mut crate::vm::hook::Hook) {
        self.finalizing = false;
        if !self.was_stopped {
            heap.restart();
        }
        self.hooks_back(hook);
    }

    /// No hook events fire while a finalizer runs (a running hook turns
    /// them off the same way).
    fn hooks_off(&mut self, hook: &mut crate::vm::hook::Hook) {
        self.hooks_were_off = std::mem::replace(&mut hook.active, true);
    }

    fn hooks_back(&mut self, hook: &mut crate::vm::hook::Hook) {
        hook.active = self.hooks_were_off;
    }
}

impl Runtime {
    /// Emit a warning (Lua's `warn`) on stderr, as `Lua warning: msg`, if
    /// warnings are on ([`Runtime::warnings`]; off by default).
    pub fn warn(&mut self, msg: &[u8]) {
        use std::io::Write;
        if let Some(hook) = &mut self.warn_hook {
            (hook.0)(msg, false);
            return;
        }
        if !self.warnings {
            return;
        }
        let mut text = b"Lua warning: ".to_vec();
        text.extend_from_slice(msg);
        text.push(b'\n');
        // Keep the order of output already printed.
        let _ = self.out.flush();
        let _ = std::io::stderr().write_all(&text);
    }

    /// Warn about an error raised by a finalizer.
    fn warn_gc_error(&mut self, heap: &LuaHeap, err: Value) {
        let mut msg = b"error in __gc (".to_vec();
        match err {
            Value::Str(s) => msg.extend_from_slice(heap.get(s).as_bytes()),
            Value::Nil => msg.extend_from_slice(b"<no error object>"),
            _ => msg.extend_from_slice(b"error object is not a string"),
        }
        msg.push(b')');
        self.warn(&msg);
    }
}

/// Whether `f` can be called as a finalizer without the call failing for
/// its type. Tables may have `__call`; the call decides.
fn callable(f: Value) -> bool {
    matches!(f, Value::Closure(_) | Value::Native(_) | Value::Table(_))
}

fn not_callable_message(f: Value) -> Vec<u8> {
    format!(
        "attempt to call a {} value (metamethod '__gc')",
        f.type_name()
    )
    .into_bytes()
}

impl Vm {
    /// Switch the collector mode; returns the previous one. Entering
    /// generational mode collects in full.
    pub fn set_gc_mode(&mut self, mode: GcMode) -> GcMode {
        let heap_mode = match mode {
            GcMode::Incremental => orbit_gc::Mode::Incremental,
            GcMode::Generational => orbit_gc::Mode::Generational,
        };
        // The floor first, so a switch to incremental paces without it.
        let old = self.rt.gc.set_mode(&mut self.heap, mode);
        // Values above the window are dead.
        let top = self.rt.top;
        self.rt.stack.regs.truncate(top);
        self.heap.set_mode(heap_mode, &mut self.rt);
        old
    }

    /// Call the `__gc` of every object waiting for finalization. Does
    /// nothing inside a finalizer. Called by the interpreter after an
    /// automatic step that left finalizers pending, and by [`Vm::close`].
    #[cold]
    #[inline(never)]
    pub fn run_finalizers(&mut self) {
        if self.rt.gc.finalizing {
            return;
        }
        let top = self.rt.top;
        while let Some(obj) = self.heap.take_finalizable() {
            // `obj` is unrooted from here until `call` pushes it, and
            // nothing in between allocates.
            let (o, f) = finalizer(&self.heap, obj);
            if f.is_nil() {
                continue;
            }
            if !callable(f) {
                let msg = not_callable_message(f);
                self.rt.warn_gc_error_text(&msg);
                continue;
            }
            self.rt.hooks_touched = true;
            self.rt.gc.enter(&mut self.heap, &mut self.rt.stack.hook);
            let at = self.rt.stack.frames.len() as u32;
            self.rt.stack.aux.finalizer_at = Some(at);
            let r = self.call(f, &[o]);
            self.rt.stack.aux.finalizer_at = None;
            self.rt.gc.exit(&mut self.heap, &mut self.rt.stack.hook);
            if let Err(LuaError { value, .. }) = r {
                self.rt.warn_gc_error(&self.heap, value);
            }
        }
        self.rt.top = top;
    }

    /// Close the VM as Lua closes a state: call the finalizer of every
    /// object still registered for one, reachable or not, most recently
    /// registered first. Objects registered while this runs are not
    /// finalized. Runs once; later calls do nothing.
    pub fn close(&mut self) {
        if self.rt.gc.closed {
            return;
        }
        self.rt.gc.closed = true;
        self.heap.finalize_all();
        self.run_finalizers();
    }
}

impl Runtime {
    fn warn_gc_error_text(&mut self, text: &[u8]) {
        let mut msg = b"error in __gc (".to_vec();
        msg.extend_from_slice(text);
        msg.push(b')');
        self.warn(&msg);
    }
}

/// From a native: call every pending finalizer, then return `result`. The
/// finalizers run through [`NativeReturn::CallThen`], so this is the
/// native's whole return value.
pub fn finalize_then_return(
    ctx: &mut NativeCtx<'_>,
    result: Value,
) -> Result<NativeReturn, LuaError> {
    ctx.truncate(0);
    ctx.push(result);
    next_finalizer(ctx)
}

fn next_finalizer(ctx: &mut NativeCtx<'_>) -> Result<NativeReturn, LuaError> {
    if ctx.vm.rt.gc.finalizing {
        return Ok(NativeReturn::Return(1));
    }
    while let Some(obj) = ctx.vm.heap.take_finalizable() {
        let (o, f) = finalizer(&ctx.vm.heap, obj);
        if f.is_nil() {
            continue;
        }
        if !callable(f) {
            let msg = not_callable_message(f);
            ctx.vm.rt.warn_gc_error_text(&msg);
            continue;
        }
        ctx.truncate(1);
        ctx.push(o);
        ctx.vm.rt.hooks_touched = true;
        ctx.vm
            .rt
            .gc
            .enter(&mut ctx.vm.heap, &mut ctx.vm.rt.stack.hook);
        // The native's frame, which `CallThen` pushes next.
        ctx.vm.rt.stack.aux.finalizer_at = Some(ctx.vm.rt.stack.frames.len() as u32);
        return Ok(NativeReturn::CallThen {
            f,
            nargs: 1,
            k: after_finalizer,
            protected: true,
        });
    }
    Ok(NativeReturn::Return(1))
}

fn after_finalizer(ctx: &mut NativeCtx<'_>, out: CallOutcome) -> Result<NativeReturn, LuaError> {
    ctx.vm
        .rt
        .gc
        .exit(&mut ctx.vm.heap, &mut ctx.vm.rt.stack.hook);
    ctx.vm.rt.stack.aux.finalizer_at = None;
    if let CallOutcome::Err(e) = out {
        ctx.vm.rt.warn_gc_error(&ctx.vm.heap, e);
    }
    ctx.truncate(1);
    next_finalizer(ctx)
}

/// From a native: close the VM as [`Vm::close`] does, then run `k` (with
/// `CallOutcome::Ok(0)`) on the window's first slot. This works inside a
/// finalizer too, as `os.exit(code, true)` needs: the other finalizers
/// still run. The VM stays closed: the collector stays stopped,
/// `collectgarbage` does nothing, and objects registered from here on are
/// never finalized, so the caller ends the process (or drops the VM) next.
/// Called while the VM is already closing (from a finalizer [`Vm::close`]
/// runs), it runs the finalizers still waiting.
pub fn close_then(ctx: &mut NativeCtx<'_>, k: Continuation) -> Result<NativeReturn, LuaError> {
    ctx.truncate(1);
    let gc = &mut ctx.vm.rt.gc;
    if !gc.closed {
        gc.closed = true;
        ctx.vm.heap.finalize_all();
    }
    gc.finalizing = true;
    gc.close_k = Some(k);
    ctx.vm.heap.stop();
    next_close_finalizer(ctx)
}

fn next_close_finalizer(ctx: &mut NativeCtx<'_>) -> Result<NativeReturn, LuaError> {
    while let Some(obj) = ctx.vm.heap.take_finalizable() {
        let (o, f) = finalizer(&ctx.vm.heap, obj);
        if f.is_nil() {
            continue;
        }
        if !callable(f) {
            let msg = not_callable_message(f);
            ctx.vm.rt.warn_gc_error_text(&msg);
            continue;
        }
        ctx.truncate(1);
        ctx.push(o);
        ctx.vm.rt.stack.aux.finalizer_at = Some(ctx.vm.rt.stack.frames.len() as u32);
        ctx.vm.rt.hooks_touched = true;
        ctx.vm.rt.gc.hooks_off(&mut ctx.vm.rt.stack.hook);
        return Ok(NativeReturn::CallThen {
            f,
            nargs: 1,
            k: after_close_finalizer,
            protected: true,
        });
    }
    ctx.vm.rt.stack.aux.finalizer_at = None;
    let k = ctx.vm.rt.gc.close_k.take().expect("close_then set it");
    k(ctx, CallOutcome::Ok(0))
}

fn after_close_finalizer(
    ctx: &mut NativeCtx<'_>,
    out: CallOutcome,
) -> Result<NativeReturn, LuaError> {
    ctx.vm.rt.gc.hooks_back(&mut ctx.vm.rt.stack.hook);
    if let CallOutcome::Err(e) = out {
        ctx.vm.rt.warn_gc_error(&ctx.vm.heap, e);
    }
    ctx.truncate(1);
    next_close_finalizer(ctx)
}
