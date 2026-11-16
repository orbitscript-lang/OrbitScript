//! [`Runtime`], [`Config`] and [`Isolate`]: where a host starts.

use crate::LineEndings;
use crate::LocaleMode;
use crate::chunk::Chunk;
use crate::compiler;
use crate::compiler::Compiler;
use crate::error::Error;
use crate::pin::Pinned;
use crate::scope::{Scope, Shared};
use crate::stack::{Control, Stack};
use crate::value::{Function, Table};
use orbit_runtime::stack::exit_hooks;
use orbit_runtime::stdlib::{self, Lib};
use orbit_runtime::vm::Vm;
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
pub use orbit_runtime::stack::Library;

/// How much binary chunks are checked ([`Isolate::set_binary_checks`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinaryChecks {
    /// Their structure: operands in range, jumps inside the function.
    Structure,
    /// Their structure and data flow, after which no binary chunk can make
    /// the VM panic.
    Full,
}

pub use orbit_runtime::stack::GcMode;

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
    debugger: Debugger,
    line_endings: LineEndings,
    native_stack_limit: usize,
    hash_seed: Option<u64>,
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
            debugger: Debugger::Allowed,
            line_endings: LineEndings::Native,
            native_stack_limit: orbit_runtime::vm::DEFAULT_NATIVE_STACK,
            hash_seed: None,
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

    /// How text streams treat line endings: standard input, output and
    /// error, `io.open` without `b`, `io.popen` and script files. With
    /// [`LineEndings::Native`] (the default) they are the platform's text
    /// streams, as the C library's: on Windows `\n` is written as `\r\n`,
    /// `\r\n` reads as `\n` and a Ctrl-Z ends input. A host whose standard
    /// streams carry a byte protocol chooses [`LineEndings::Binary`]. A
    /// writer given to [`Isolate::set_stdout`] gets `print`'s bytes as they
    /// are, whatever this says.
    pub fn line_endings(mut self, line_endings: LineEndings) -> Config {
        self.line_endings = line_endings;
        self
    }

    /// How many bytes of native stack calls back into scripts may use
    /// below the host's outermost call (1 MiB by default). Each time a
    /// native function, a library function such as `string.gsub`, or a C
    /// module calls a script function, the call nests on the native stack.
    /// Past this budget, or past 200 nested levels, the call that would go
    /// deeper raises the script error `C stack overflow`.
    ///
    /// The thread that runs scripts needs this budget plus room for the
    /// host's own frames above its call into the isolate, and some 16 KiB
    /// for the last level and the error. The default suits threads Rust
    /// spawns (2 MiB) and the main thread on Linux and macOS (8 MiB). On
    /// a smaller stack, such as the main thread on Windows (1 MiB) or a
    /// thread spawned with a set size, set about half the thread's size.
    /// A nested level takes about 3.5 KiB in an optimised x86_64 build, so
    /// 768 KiB reaches the 200-level limit and 512 KiB about 150 levels;
    /// an unoptimised build takes several times as much per level and
    /// stops sooner.
    pub fn native_stack_limit(mut self, bytes: usize) -> Config {
        self.native_stack_limit = bytes;
        self
    }

    /// Hash strings and table keys with this seed instead of a random one,
    /// so that a run repeats exactly: the same collisions, the same probes,
    /// the same instruction count. Only for benchmarks and other
    /// measurements. Whoever knows the seed can choose keys that all
    /// collide and make every lookup in a table slow, so an isolate that
    /// sees untrusted input keeps the random seed (the default).
    pub fn fixed_hash_seed(mut self, seed: u64) -> Config {
        self.hash_seed = Some(seed);
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

/// One VM: its own heap, globals and collector, run on one thread.
/// Closing it (dropping it) runs pending finalizers and flushes output.
#[derive(Debug)]
pub struct Isolate {
    vm: Vm,
}

impl Isolate {
    pub(crate) fn new(config: Config) -> Isolate {
        let mut vm = Vm::with_hash_seed(config.hash_seed);
        adopt(&mut vm);
        #[cfg(feature = "locale")]
        vm.set_locale_provider(orbit_locale::provider());
        vm.host_mut().set_warnings(config.warnings);
        vm.set_line_endings(config.line_endings);
        vm.set_gc_mode(config.gc_mode.raw());
        for lib in Lib::OPEN_ORDER {
            if config.libraries.iter().any(|l| l.lib() == lib) {
                stdlib::open_one(&mut vm, lib);
            }
        }
        vm.set_memory_limit(config.memory_limit);
        vm.set_native_stack_limit(config.native_stack_limit);
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

    /// Send standard output (`print`, `io.stdout`) to `out`, which gets the
    /// bytes as scripts write them ([`Config::line_endings`] doesn't
    /// apply).
    pub fn set_stdout(&mut self, out: impl std::io::Write + std::fmt::Debug + 'static) {
        self.vm.host_mut().set_out(Box::new(out));
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
    /// not at all: a malformed chunk can then make the VM panic or reach
    /// values the script was never given, so lower it only for chunks you
    /// trust). Starts at [`BinaryChecks::Full`], whatever the environment.
    pub fn set_binary_checks(&mut self, level: Option<BinaryChecks>) {
        use orbit_runtime::vm::op::verify::Level;
        let level = level.map(|l| match l {
            BinaryChecks::Structure => Level::Structure,
            BinaryChecks::Full => Level::Full,
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
                    s.native_ctx().push(value);
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
        self.vm.heap_bytes()
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
        self.vm.set_gc_stress(on);
    }

    /// Check the heap at the end of markings, and panic, naming the chain
    /// of objects from a root, if the collector missed an object it can
    /// reach (a missed write barrier) or the byte count is off. Debug
    /// builds only: slow, for tests, best with [`Gc::set_stress`].
    ///
    /// `every`: check every minor collection too, which makes each cost a
    /// walk of the whole heap. Otherwise a minor collection is checked once
    /// as many bytes as the heap holds have been allocated since the last
    /// check, and every full marking is.
    #[cfg(debug_assertions)]
    pub fn set_verify(&mut self, on: bool, every: bool) {
        use orbit_runtime::object::Verify;
        self.vm.set_gc_verify(match (on, every) {
            (false, _) => Verify::Off,
            (true, false) => Verify::Paced,
            (true, true) => Verify::Every,
        });
    }
}

/// Make `vm` one the API can reach (scopes, handles, host functions), with
/// both front ends as `load`'s compilers.
pub(crate) fn adopt(vm: &mut Vm) {
    vm.host_mut()
        .ext_mut()
        .get_or_insert_with(|| Rc::new(Shared::default()));
    vm.host_mut()
        .set_compilers(compiler::compile_lua, compiler::compile_orb);
}
