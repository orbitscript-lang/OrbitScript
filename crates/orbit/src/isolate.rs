//! [`Runtime`], [`Config`] and [`Isolate`]: where a host starts.

use std::rc::Rc;

use orbit_runtime::stdlib::{self, Lib};
use orbit_runtime::vm::{self, Vm};

use crate::chunk::Chunk;
use crate::compiler::{self, Compiler};
use crate::error::Error;
use crate::pin::Pinned;
use crate::scope::{Scope, Shared};
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

/// A standard library an isolate can open ([`Config::libraries`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Library {
    /// The base functions (`print`, `pairs`, `pcall`, ...), as globals.
    Base,
    Package,
    Coroutine,
    Debug,
    Io,
    Math,
    Os,
    String,
    Table,
    Utf8,
}

impl Library {
    pub const ALL: [Library; 10] = [
        Library::Base,
        Library::Package,
        Library::Coroutine,
        Library::Debug,
        Library::Io,
        Library::Math,
        Library::Os,
        Library::String,
        Library::Table,
        Library::Utf8,
    ];

    fn lib(self) -> Lib {
        match self {
            Library::Base => Lib::Base,
            Library::Package => Lib::Package,
            Library::Coroutine => Lib::Coroutine,
            Library::Debug => Lib::Debug,
            Library::Io => Lib::Io,
            Library::Math => Lib::Math,
            Library::Os => Lib::Os,
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
    fn raw(self) -> vm::GcMode {
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
    lock_cpath: bool,
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
            lock_cpath: false,
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

    /// Ignore environment variables (`LUA_PATH`, `LUA_CPATH`,
    /// `ORBIT_VERIFY`), as `lua -E` does.
    pub fn ignore_env(mut self, ignore: bool) -> Config {
        self.ignore_env = ignore;
        self
    }

    /// Lock the C module search path to the one `package.cpath` starts
    /// with.
    pub fn lock_cpath(mut self, lock: bool) -> Config {
        self.lock_cpath = lock;
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
        vm.rt.ext.get_or_insert_with(|| Rc::new(Shared::default()));
        vm.rt.compile = Some(compiler::compile_lua);
        vm.rt.compile_orbitscript = Some(compiler::compile_orb);
        vm.rt.warnings = config.warnings;
        vm.rt.gc.mode = config.gc_mode.raw();
        let options = stdlib::Options {
            ignore_env: config.ignore_env,
            lock_cpath: config.lock_cpath,
        };
        for lib in Lib::OPEN_ORDER {
            if config.libraries.iter().any(|l| l.lib() == lib) {
                stdlib::open_one(&mut vm, lib, options);
            }
        }
        #[cfg(feature = "c-modules")]
        if config.libraries.contains(&Library::Package) {
            orbit_capi::install(&mut vm);
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
    /// Make `module` what `require(name)` returns, as if a searcher had
    /// loaded it (`package.loaded[name] = module`).
    pub fn register_module(&'s self, name: &str, module: Table<'s>) {
        let mut ctx = self.ctx();
        stdlib::mark_loaded(ctx.vm, name, module.raw());
    }
}
