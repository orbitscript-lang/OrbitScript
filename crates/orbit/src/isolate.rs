//! [`Runtime`], [`Config`] and [`Isolate`]: where a host starts.

use std::rc::Rc;

use orbit_runtime::object as raw;
use orbit_runtime::stdlib::{self, Lib};
use orbit_runtime::vm::{self, CallOutcome, LuaError, MetaField, NativeCtx, NativeReturn, Vm};

use crate::chunk::Chunk;
use crate::compiler::{self, Compiler};
use crate::error::Error;
use crate::pin::Pinned;
use crate::scope::{Scope, Shared};
use crate::stack::{Control, Stack};
use crate::value::{Function, Table};

/// The runtime library. Creates isolates and gives the compiler; in a
/// shared build, this is where `liborbit` gets loaded and checked.
#[derive(Debug, Clone, Copy, Default)]
pub struct Runtime(());

impl Runtime {
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

/// The collector's mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum GcMode {
    #[default]
    Incremental,
    Generational,
}

impl GcMode {
    pub(crate) fn raw(self) -> vm::GcMode {
        match self {
            GcMode::Incremental => vm::GcMode::Incremental,
            GcMode::Generational => vm::GcMode::Generational,
        }
    }
}

/// How an isolate is set up.
#[derive(Debug, Clone)]
pub struct Config {
    libraries: Vec<Library>,
    memory_limit: Option<usize>,
    gc_mode: GcMode,
    warnings: bool,
    ignore_env: bool,
}

impl Default for Config {
    /// Every library this build has, no memory limit, the incremental
    /// collector, and warnings off (as in `lua` without `-W`).
    fn default() -> Config {
        Config {
            libraries: Library::ALL.to_vec(),
            memory_limit: None,
            gc_mode: GcMode::Incremental,
            warnings: false,
            ignore_env: false,
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
}

/// One VM: its own heap, globals and collector, run on one thread.
/// Closing it (dropping it) runs pending finalizers and flushes output.
#[derive(Debug)]
pub struct Isolate {
    vm: Vm,
}

impl Isolate {
    fn new(config: Config) -> Isolate {
        let mut vm = Vm::new();
        adopt(&mut vm);
        vm.rt.warnings = config.warnings;
        vm.rt.gc.mode = config.gc_mode.raw();
        let options = stdlib::Options {
            ignore_env: config.ignore_env,
        };
        for lib in Lib::OPEN_ORDER {
            if config.libraries.iter().any(|l| l.lib() == lib) {
                stdlib::open_one(&mut vm, lib, options);
            }
        }
        vm.set_memory_limit(config.memory_limit);
        Isolate { vm }
    }

    /// Run `f` with a scope for making and using handles. Everything the
    /// scope made is released when `f` returns.
    pub fn scope<R>(&mut self, f: impl for<'s> FnOnce(&'s Scope<'s>) -> R) -> R {
        let ctx = self.vm.host_ctx();
        let start = ctx.len();
        let scope = Scope::new(ctx);
        scope.release_pins(&mut scope.ctx());
        let r = f(&scope);
        scope.ctx().truncate(start);
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

    /// Give scripts the `ffi` library (the `ffi` global and
    /// `package.loaded.ffi`): calling C functions in any shared library the
    /// process can load, and reading and writing memory through pointers.
    /// That is as much power as a C module has, so a host enables it only
    /// for scripts it trusts that far (ADR-9). Calling it again does nothing.
    #[cfg(feature = "ffi")]
    pub fn enable_ffi(&mut self) {
        orbit_ffi::install(&mut self.vm);
    }
}

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
        self.vm.rt.gc.mode = mode.raw();
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

impl<'s> Scope<'s> {
    /// [`Isolate::before_exit`], from inside the isolate (a library that
    /// keeps buffers registers its flush when it opens).
    pub fn before_exit(&self, hook: impl Fn() + 'static) {
        exit_hooks(self.ctx().vm).0.push(Box::new(hook));
    }

    /// Make `module` what `require(name)` returns, as if a searcher had
    /// loaded it (`package.loaded[name] = module`).
    pub fn register_module(&'s self, name: &str, module: Table<'s>) {
        let mut ctx = self.ctx();
        stdlib::mark_loaded(ctx.vm, name, module.raw());
    }
}

/// The [`Isolate::before_exit`] hooks.
#[derive(Default)]
struct ExitHooks(Vec<Box<dyn Fn()>>);

fn exit_hooks(vm: &mut Vm) -> &mut ExitHooks {
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
