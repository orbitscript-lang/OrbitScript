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
//! [`finalize_then_return`]), and when the VM closes ([`Vm::close`]).
//!
//! [GC semantics design]: https://github.com/tymlipari/OrbitScript/blob/main/docs/design/gc-semantics.md

use orbit_core::gc::finalizer;
use orbit_core::{LuaHeap, Value};

use crate::{CallOutcome, LuaError, NativeCtx, NativeReturn, Runtime, Vm};

/// The collector mode `collectgarbage` reports. The incremental collector
/// runs underneath in both; generational changes only what `"step"` does.
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
    /// Generational parameters: stored and reported, unused until there is
    /// a generational collector.
    MinorMul,
    MajorMinor,
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

/// How `stepmul` scales to `Params::work_ratio`: our default work ratio of
/// 800 reads as Lua's default `stepmul` of 200.
const STEPMUL_SCALE: u32 = 4;

/// What `collectgarbage` keeps besides the collector's own state.
#[derive(Debug)]
pub struct GcState {
    pub mode: GcMode,
    /// The parameters as last set, so they read back exactly. `pause`,
    /// `stepmul` and `stepsize` are also applied to the heap's `Params`.
    params: [i64; 6],
    /// A finalizer is running.
    finalizing: bool,
    /// Whether the collector was stopped when the running finalizer
    /// started.
    was_stopped: bool,
    closed: bool,
}

impl GcState {
    pub(crate) fn new(heap: &LuaHeap) -> Self {
        let p = heap.params();
        GcState {
            mode: GcMode::Incremental,
            params: [
                i64::from(p.growth) + 100,
                i64::from(p.work_ratio / STEPMUL_SCALE),
                p.step_bytes as i64,
                // lua5.5's defaults.
                20,
                50,
                68,
            ],
            finalizing: false,
            was_stopped: false,
            closed: false,
        }
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
            GcParam::MinorMul | GcParam::MajorMinor | GcParam::MinorMajor => {}
        }
        old
    }

    /// Suspend automatic collection while a finalizer runs.
    fn enter(&mut self, heap: &mut LuaHeap) {
        debug_assert!(!self.finalizing);
        self.finalizing = true;
        self.was_stopped = heap.is_stopped();
        heap.stop();
    }

    fn exit(&mut self, heap: &mut LuaHeap) {
        self.finalizing = false;
        if !self.was_stopped {
            heap.restart();
        }
    }
}

impl Runtime {
    /// Emit a warning (Lua's `warn`). Warnings are off by default, and
    /// `warn("@on")` arrives with the `warn` function (C3); until then they
    /// are dropped.
    pub fn warn(&mut self, msg: &[u8]) {
        let _ = msg;
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
            self.rt.gc.enter(&mut self.heap);
            let r = self.call(f, &[o]);
            self.rt.gc.exit(&mut self.heap);
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
    if ctx.rt.gc.finalizing {
        return Ok(NativeReturn::Return(1));
    }
    while let Some(obj) = ctx.heap.take_finalizable() {
        let (o, f) = finalizer(ctx.heap, obj);
        if f.is_nil() {
            continue;
        }
        if !callable(f) {
            let msg = not_callable_message(f);
            ctx.rt.warn_gc_error_text(&msg);
            continue;
        }
        ctx.truncate(1);
        ctx.push(o);
        ctx.rt.gc.enter(ctx.heap);
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
    ctx.rt.gc.exit(ctx.heap);
    if let CallOutcome::Err(e) = out {
        ctx.rt.warn_gc_error(ctx.heap, e);
    }
    ctx.truncate(1);
    next_finalizer(ctx)
}
