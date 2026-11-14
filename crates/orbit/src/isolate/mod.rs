//! [`Runtime`], [`Config`] and [`Isolate`]: where a host starts.

use crate::LocaleMode;
use crate::chunk::Chunk;
use crate::compiler;
use crate::compiler::Compiler;
use crate::error::Error;
use crate::pin::Pinned;
use crate::scope::{Scope, Shared};
use crate::stack::{Control, Stack};
use crate::value::{Function, Table};
use orbit_runtime::object as raw;
use orbit_runtime::stdlib::{self, Lib};
use orbit_runtime::vm::{self, CallOutcome, LuaError, MetaField, NativeCtx, NativeReturn, Vm};
use std::rc::Rc;

/// The runtime library. Creates isolates and gives the compiler.
#[derive(Debug, Clone, Copy)]
pub struct Runtime(());

impl Default for Runtime {
    fn default() -> Runtime {
        Runtime::new()
    }
}

impl Runtime {
    /// The runtime.
    pub fn new() -> Runtime {
        Runtime(())
    }

    /// The runtime's version.
    pub fn version(&self) -> &'static str {
        crate::VERSION
    }

    /// A new isolate set up as `config` says.
    pub fn new_isolate(&self, config: Config) -> Isolate {
        Isolate::new(config)
    }

    /// The compiler, which needs no isolate.
    pub fn compiler(&self) -> Compiler {
        Compiler
    }
}
/// A standard library an isolate can open ([`Config::libraries`]). The
/// libraries that touch the system (`io`, `os`, `print`, `loadfile`,
/// `dofile`) are `orbit-std`'s, which hosts install on top.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Library {
    /// The base functions (`print`, `pairs`, `pcall`, ...), as globals.
    Base,
    Package,
    Coroutine,
    Debug,
    Math,
    String,
    Table,
    Utf8,
}

impl Library {
    pub const ALL: [Library; 8] = [
        Library::Base,
        Library::Package,
        Library::Coroutine,
        Library::Debug,
        Library::Math,
        Library::String,
        Library::Table,
        Library::Utf8,
    ];
}

/// How much binary chunks are checked ([`Isolate::set_binary_checks`]).
#[cfg(feature = "verify")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinaryChecks {
    /// Their structure: operands in range, jumps inside the function.
    Structure,
    /// Their structure and data flow, after which no binary chunk can make
    /// the VM panic (feature `verify-flow`; without it, as `Structure`).
    Full,
}

/// The collector's mode ([`Config::gc_mode`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum GcMode {
    /// Every cycle marks the whole heap, in small steps paced by
    /// allocation: short pauses, more total work.
    Incremental,
    /// Frequent minor collections of recently made objects, and major ones
    /// of everything when the heap has grown: less total work for programs
    /// that make many short-lived objects. Each collection runs to the end
    /// once started, so a major one is a longer pause.
    #[default]
    Generational,
}

/// Whether a debugger may attach to an isolate ([`Config::debugger`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Debugger {
    /// One may, if the build has the engine (feature `debugger`).
    #[default]
    Allowed,
    /// None can, for the isolate's whole life: a host that runs code it
    /// doesn't want inspected says so here (ADR-12).
    Disabled,
}

/// How an isolate is set up.
#[derive(Debug, Clone)]
pub struct Config {
    libraries: Vec<Library>,
    memory_limit: Option<usize>,
    gc_mode: GcMode,
    warnings: bool,
    ignore_env: bool,
    debugger: Debugger,
}

impl Default for Config {
    /// Every library this build has, no memory limit, the generational
    /// collector, and warnings off (as in `lua` without `-W`).
    fn default() -> Config {
        Config {
            libraries: Library::ALL.to_vec(),
            memory_limit: None,
            gc_mode: GcMode::Generational,
            warnings: false,
            ignore_env: false,
            debugger: Debugger::Allowed,
        }
    }
}

impl Config {
    /// The standard libraries to open, of those this build has.
    pub fn libraries(mut self, libraries: &[Library]) -> Config {
        self.libraries = libraries.to_vec();
        self
    }

    /// Cap the heap at `bytes` (`None`: no cap). A script that needs more,
    /// even after a full collection, gets the catchable error `not enough
    /// memory`. The count is `collectgarbage("count")`'s.
    pub fn memory_limit(mut self, bytes: Option<usize>) -> Config {
        self.memory_limit = bytes;
        self
    }

    /// The collector's mode at the start (generational by default).
    /// Scripts can switch with `collectgarbage("incremental")` and
    /// `collectgarbage("generational")`.
    pub fn gc_mode(mut self, mode: GcMode) -> Config {
        self.gc_mode = mode;
        self
    }

    /// Whether `warn` messages are shown at the start (scripts can switch
    /// them with `warn("@on")`).
    pub fn warnings(mut self, on: bool) -> Config {
        self.warnings = on;
        self
    }

    /// Ignore the environment variable `ORBIT_VERIFY`, as `lua -E` does.
    pub fn ignore_env(mut self, ignore: bool) -> Config {
        self.ignore_env = ignore;
        self
    }

    /// Whether a debugger may ever attach. Only set when the isolate is
    /// made: there is no turning it off later.
    pub fn debugger(mut self, debugger: Debugger) -> Config {
        self.debugger = debugger;
        self
    }
}

// Built on `Isolate::scope`.
impl Isolate {
    /// Load and run a chunk, dropping its results.
    pub fn exec(&mut self, chunk: Chunk<'_>) -> Result<(), Error> {
        self.scope(|s| s.load(chunk)?.call(()).map(drop))
    }

    /// Load a chunk and keep its main function.
    pub fn load(&mut self, chunk: Chunk<'_>) -> Result<Pinned<Function<'static>>, Error> {
        self.scope(|s| Ok(s.load(chunk)?.pin()))
    }

    /// The global table, pinned.
    pub fn globals(&mut self) -> Pinned<Table<'static>> {
        self.scope(|s| s.globals().pin())
    }
}

// ---- the isolate over the runtime --------------------------------------------

impl Library {
    pub(crate) fn lib(self) -> Lib {
        match self {
            Library::Base => Lib::Base,
            Library::Package => Lib::Package,
            Library::Coroutine => Lib::Coroutine,
            Library::Debug => Lib::Debug,
            Library::Math => Lib::Math,
            Library::String => Lib::String,
            Library::Table => Lib::Table,
            Library::Utf8 => Lib::Utf8,
        }
    }
}

impl GcMode {
    pub(crate) fn raw(self) -> vm::GcMode {
        match self {
            GcMode::Incremental => vm::GcMode::Incremental,
            GcMode::Generational => vm::GcMode::Generational,
        }
    }
}

/// One VM: its own heap, globals and collector, run on one thread.
/// Closing it (dropping it) runs pending finalizers and flushes output.
#[derive(Debug)]
pub struct Isolate {
    vm: Vm,
}

impl Isolate {
    pub(crate) fn new(config: Config) -> Isolate {
        let mut vm = Vm::new();
        adopt(&mut vm);
        #[cfg(feature = "locale")]
        vm.set_locale_provider(orbit_locale::provider());
        vm.rt.warnings = config.warnings;
        vm.set_gc_mode(config.gc_mode.raw());
        let options = stdlib::Options {
            ignore_env: config.ignore_env,
        };
        for lib in Lib::OPEN_ORDER {
            if config.libraries.iter().any(|l| l.lib() == lib) {
                stdlib::open_one(&mut vm, lib, options);
            }
        }
        vm.set_memory_limit(config.memory_limit);
        #[cfg(feature = "debugger")]
        if config.debugger == super::Debugger::Disabled {
            vm.disable_debugger();
        }
        Isolate { vm }
    }

    /// Run `f` with a scope for making and using handles. Everything the
    /// scope made is released when `f` returns.
    pub fn scope<R>(&mut self, f: impl for<'s> FnOnce(&'s Scope<'s>) -> R) -> R {
        let stack = Stack::new(self.vm.host_ctx());
        let start = stack.top();
        let scope = Scope::new(stack);
        let r = f(&scope);
        let _ = scope.stack().set_top(start);
        r
    }

    /// Run `f` on the host's window ([`Stack`]); what `f` leaves there is
    /// dropped after.
    pub fn stack<R>(&mut self, f: impl FnOnce(&mut Stack<'_>) -> R) -> R {
        let ctx = self.vm.host_ctx();
        let start = ctx.len();
        let mut s = Stack::new(ctx);
        let r = f(&mut s);
        let _ = s.set_top(start as i32);
        r
    }

    /// The host's window, kept as it is between uses: for a host that
    /// drives the isolate through the stack API alone, as a C program does
    /// through the Lua C API.
    pub fn host_stack(&mut self) -> Stack<'_> {
        Stack::new(self.vm.host_ctx())
    }

    /// What reaches the isolate itself ([`Control`]).
    pub fn control(&mut self) -> Control<'_> {
        Control::new(&mut self.vm)
    }

    /// Send standard output (`print`, `io.stdout`) to `out`.
    pub fn set_stdout(&mut self, out: impl std::io::Write + std::fmt::Debug + 'static) {
        self.vm.rt.out = Box::new(out);
    }

    /// Whether `os.setlocale` changes this isolate's own locale (the
    /// default) or the process's, as PUC Lua does (ADR-13). The locale
    /// scripts see doesn't change: a switch to the process's takes it on,
    /// one to the isolate's own keeps it.
    pub fn set_locale_mode(&mut self, mode: LocaleMode) {
        self.vm.set_locale_mode(mode)
    }

    pub fn locale_mode(&self) -> LocaleMode {
        self.vm.locale_mode()
    }

    /// The collector.
    pub fn gc(&mut self) -> Gc<'_> {
        Gc { vm: &mut self.vm }
    }

    /// A handle that stops running code from any thread.
    pub fn interrupt(&self) -> Interrupt {
        self.vm.interrupt_handle()
    }

    /// Flush open files and `print` output.
    pub fn flush(&mut self) {
        stdlib::flush_files(&mut self.vm);
    }

    /// How much `load` checks binary chunks before running them (`None`:
    /// not at all, so a malformed chunk can make the VM panic). Starts at
    /// what `ORBIT_VERIFY` asks for, or the most this build can check;
    /// levels past that are capped.
    #[cfg(feature = "verify")]
    pub fn set_binary_checks(&mut self, level: Option<BinaryChecks>) {
        use orbit_runtime::vm::op::verify::Level;
        let level = level.map(|l| match l {
            BinaryChecks::Structure => Level::Structure,
            #[cfg(feature = "verify-flow")]
            BinaryChecks::Full => Level::Full,
            #[cfg(not(feature = "verify-flow"))]
            BinaryChecks::Full => Level::Structure,
        });
        stdlib::checks::set_level(&mut self.vm, level);
    }

    /// Run `hook` when a script ends the process ([`Call::exit`], `os.exit`),
    /// after the isolate closes (if it does) and before standard output is
    /// flushed: for buffers the process exit would otherwise lose.
    ///
    /// [`Call::exit`]: crate::Call::exit
    pub fn before_exit(&mut self, hook: impl Fn() + 'static) {
        exit_hooks(&mut self.vm).0.push(Box::new(hook));
    }
}

#[cfg(feature = "debugger")]
mod debug {
    use orbit_runtime::vm::debugger as engine;

    use super::Isolate;
    use crate::debugger::{
        Breakpoint, DebugEvents, ErrorBreak, PauseHandle, Paused, Resume, StopReason,
    };
    use crate::error::Error;
    use crate::stack::Stack;

    /// [`PauseHandle`]'s state: none in an isolate no debugger may attach
    /// to.
    #[derive(Debug, Clone)]
    pub(crate) struct PauseInner(Option<engine::PauseHandle>);

    impl PauseInner {
        pub(crate) fn request(&self) {
            if let Some(p) = &self.0 {
                p.request()
            }
        }
    }

    /// A host's events, as the engine calls them.
    struct Events(Box<dyn DebugEvents>);

    impl engine::DebugEvents for Events {
        fn stopped(
            &mut self,
            p: &mut engine::Paused<'_, '_>,
            reason: engine::StopReason,
        ) -> engine::Resume {
            let mut s = Stack::new(p.ctx().reborrow());
            let reason = match reason {
                engine::StopReason::Breakpoint { line } => StopReason::Breakpoint { line },
                engine::StopReason::Step => StopReason::Step,
                engine::StopReason::Pause => StopReason::Pause,
                engine::StopReason::Error { value, caught } => {
                    s.ctx.push(value);
                    StopReason::Error { caught }
                }
            };
            let error = matches!(reason, StopReason::Error { .. });
            let resume = self.0.stopped(&mut Paused::new(&mut s, error), reason);
            match resume {
                Resume::Continue => engine::Resume::Continue,
                Resume::StepIn => engine::Resume::StepIn,
                Resume::StepOver => engine::Resume::StepOver,
                Resume::StepOut => engine::Resume::StepOut,
            }
        }

        fn breakpoints_resolved(&mut self, source: &[u8], breakpoints: &[engine::Breakpoint]) {
            let bps: Vec<Breakpoint> = breakpoints.iter().map(|&b| breakpoint(b)).collect();
            self.0.breakpoints_resolved(source, &bps)
        }
    }

    fn breakpoint(b: engine::Breakpoint) -> Breakpoint {
        Breakpoint {
            requested: b.requested,
            line: b.line,
        }
    }

    /// The debugger (ADR-12; feature `debugger`). See [`crate::debugger`].
    impl Isolate {
        /// Report stops to `events` from now on; `None` detaches the
        /// debugger, which also ends any step. Fails in an isolate made
        /// with [`crate::Debugger::Disabled`].
        pub fn set_debug_events(
            &mut self,
            events: Option<Box<dyn DebugEvents>>,
        ) -> Result<(), Error> {
            let events = events.map(|e| Box::new(Events(e)) as Box<dyn engine::DebugEvents>);
            if self.vm.set_debug_events(events) {
                Ok(())
            } else {
                Err(Error::runtime("this isolate allows no debugger"))
            }
        }

        /// Replace the breakpoints of `source` (a chunk's name as given to
        /// the loader: `@file.lua` for a file) with breakpoints on `lines`,
        /// and say where each one is. Those of a source not loaded yet
        /// resolve when it is ([`DebugEvents::breakpoints_resolved`]).
        pub fn set_breakpoints(&mut self, source: &[u8], lines: &[u32]) -> Vec<Breakpoint> {
            self.control().set_breakpoints(source, lines)
        }

        /// Stop at the next line that runs, as a step in does
        /// ([`StopReason::Step`]): set before running a program to stop on
        /// its entry. Does nothing while no events are attached.
        pub fn stop_on_next_line(&mut self) {
            self.control().stop_on_next_line()
        }

        /// Which errors stop the program ([`StopReason::Error`]).
        pub fn set_error_break(&mut self, on: ErrorBreak) {
            self.control().set_error_break(on)
        }

        /// A handle that pauses this isolate from any thread.
        pub fn pause_handle(&self) -> PauseHandle {
            PauseHandle(PauseInner(
                self.vm.debugger_allowed().then(|| self.vm.pause_handle()),
            ))
        }
    }
}

#[cfg(feature = "debugger")]
pub(crate) use debug::PauseInner;

impl Drop for Isolate {
    fn drop(&mut self) {
        self.vm.close();
        stdlib::flush_files(&mut self.vm);
    }
}

pub use orbit_runtime::vm::Interrupt;

/// The collector of one isolate ([`Isolate::gc`]).
#[derive(Debug)]
pub struct Gc<'a> {
    vm: &'a mut Vm,
}

impl Gc<'_> {
    /// Run a full collection cycle.
    pub fn collect(&mut self) {
        // Through the host window, which drops dead values above it first.
        self.vm.host_ctx().full_collect();
    }

    /// Bytes in use, as `collectgarbage("count")` counts them.
    pub fn used_bytes(&self) -> usize {
        self.vm.heap.bytes()
    }

    pub fn set_mode(&mut self, mode: GcMode) {
        self.vm.set_gc_mode(mode.raw());
    }

    /// See [`Config::memory_limit`].
    pub fn set_limit(&mut self, bytes: Option<usize>) {
        self.vm.set_memory_limit(bytes);
    }

    pub fn limit(&self) -> Option<usize> {
        self.vm.memory_limit()
    }

    /// Collect at nearly every safe point after an allocation: slow, for
    /// shaking out code that keeps values the collector can't see.
    pub fn set_stress(&mut self, on: bool) {
        self.vm.heap.set_stress(on);
    }
}

/// The [`Isolate::before_exit`] hooks.
#[derive(Default)]
pub(crate) struct ExitHooks(pub(crate) Vec<Box<dyn Fn()>>);

pub(crate) fn exit_hooks(vm: &mut Vm) -> &mut ExitHooks {
    vm.rt.ext.get_or_insert_with(ExitHooks::default)
}

type NativeResult = Result<NativeReturn, LuaError>;

/// [`Call::exit`](crate::Call::exit), from the host function's window.
pub(crate) fn exit_process(ctx: &mut NativeCtx<'_>, code: i32, close: bool) -> NativeResult {
    if !close {
        exit_now(ctx, code);
    }
    // Close the isolate first, as Lua closes the state: the main thread's
    // variables still to close, newest first, then every pending
    // finalizer, even when this is called from one. The window is
    // [code, failed, error, count, values to close...].
    let pending = ctx.main_pending_closes();
    ctx.truncate(0);
    ctx.push(raw::Value::Int(i64::from(code)));
    ctx.push(raw::Value::Bool(false));
    ctx.push(raw::Value::Nil);
    ctx.push(raw::Value::Int(pending.len() as i64));
    for v in pending {
        ctx.push(v);
    }
    close_next(ctx)
}

/// Window slots of the closing state.
const FAILED: usize = 1;
const ERROR: usize = 2;
const COUNT: usize = 3;

/// Call the newest variable's `__close` with no error, or, once a handler
/// has failed, with that error (the last error is dropped, as in lua5.5),
/// then the next; with none left, close the isolate.
fn close_next(ctx: &mut NativeCtx<'_>) -> NativeResult {
    let raw::Value::Int(n) = ctx.arg(COUNT) else {
        unreachable!("exit pushed the count")
    };
    if n == 0 {
        return vm::close_then(ctx, exit_closed);
    }
    let v = ctx.arg(COUNT + n as usize);
    ctx.set(COUNT, raw::Value::Int(n - 1));
    ctx.truncate(COUNT + n as usize);
    let h = ctx.metamethod(v, MetaField::Close);
    ctx.push(v);
    let failed = matches!(ctx.arg(FAILED), raw::Value::Bool(true));
    if failed {
        let e = ctx.arg(ERROR);
        ctx.push(e);
    }
    Ok(NativeReturn::CallThen {
        f: h,
        nargs: 1 + failed as usize,
        k: closed_one,
        protected: true,
    })
}

fn closed_one(ctx: &mut NativeCtx<'_>, out: CallOutcome) -> NativeResult {
    if let CallOutcome::Err(e) = out {
        ctx.set(FAILED, raw::Value::Bool(true));
        ctx.set(ERROR, e);
    }
    let raw::Value::Int(n) = ctx.arg(COUNT) else {
        unreachable!("exit pushed the count")
    };
    ctx.truncate(COUNT + 1 + n as usize);
    close_next(ctx)
}

fn exit_closed(ctx: &mut NativeCtx<'_>, _: CallOutcome) -> NativeResult {
    let raw::Value::Int(code) = ctx.arg(0) else {
        unreachable!("exit pushed the code")
    };
    exit_now(ctx, code as i32)
}

/// Run the exit hooks, flush standard output, and end the process.
fn exit_now(ctx: &mut NativeCtx<'_>, code: i32) -> ! {
    let hooks = std::mem::take(&mut exit_hooks(ctx.vm).0);
    for hook in &hooks {
        hook();
    }
    let _ = std::io::Write::flush(&mut ctx.vm.rt.out);
    std::process::exit(code)
}

/// Make `vm` one the API can reach (scopes, handles, host functions), with
/// both front ends as `load`'s compilers.
pub(crate) fn adopt(vm: &mut Vm) {
    vm.rt.ext.get_or_insert_with(|| Rc::new(Shared::default()));
    vm.rt.compile = Some(compiler::compile_lua);
    vm.rt.compile_orbitscript = Some(compiler::compile_orb);
}
