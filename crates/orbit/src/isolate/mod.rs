//! [`Runtime`], [`Config`] and [`Isolate`]: where a host starts.

#[cfg(runtime_backend)]
#[path = "rt.rs"]
pub(crate) mod backend;
#[cfg(shared_backend)]
#[path = "shared.rs"]
pub(crate) mod backend;

pub use backend::{Gc, Interrupt, Isolate};

use crate::chunk::Chunk;
use crate::compiler::Compiler;
use crate::error::Error;
use crate::pin::Pinned;
use crate::value::{Function, Table};

/// The runtime library. Creates isolates and gives the compiler; in a
/// shared build, this is where `liborbit` gets loaded and checked.
#[derive(Debug, Clone, Copy)]
pub struct Runtime(());

impl Default for Runtime {
    fn default() -> Runtime {
        Runtime::new()
    }
}

impl Runtime {
    /// The runtime. In a shared build this checks that `liborbit` was
    /// built from the same boundary as this program, and panics if not.
    pub fn new() -> Runtime {
        backend::check_library();
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
    /// doesn't want inspected says so here (ADR-12, D5).
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

// Built on `Isolate::scope`, the same for both backends.
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
