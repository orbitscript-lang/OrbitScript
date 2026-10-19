//! The isolate over `liborbit` (ADR-10). The library is linked at load
//! time; [`check_library`] makes sure it was built from the same boundary
//! as this program before anything else crosses.

use std::rc::Rc;

use stabby::boxed::Box as SBox;
use stabby::result::Result as SResult;
use stabby::string::String as SString;

use crate::abi::*;
use crate::chunk::Chunk;
use crate::error::Error;
use crate::pin::Pinned;
use crate::scope::{Scope, Shared};
use crate::stack::backend::{HostState, hook};
use crate::stack::{Control, Stack};
use crate::value::{Function, Table};

#[cfg(feature = "verify")]
use super::BinaryChecks;
use super::{Config, GcMode, Library};

#[stabby::import(name = "orbit_lib")]
unsafe extern "C" {
    pub fn orbit_check(hash: u64) -> stabby::result::Result<(), stabby::string::String>;
    pub fn orbit_new_isolate(config: crate::abi::ConfigAbi) -> crate::abi::IsolateBox;
}

/// Panic unless `liborbit` was built from this program's boundary.
pub(crate) fn check_library() {
    if let Err(m) = Result::<(), SString>::from(orbit_check(INTERFACE_HASH)) {
        panic!("{}", m.as_str());
    }
}

/// One VM: its own heap, globals and collector, run on one thread.
/// Closing it (dropping it) runs pending finalizers and flushes output.
pub struct Isolate {
    obj: IsolateBox,
    host: Rc<HostState>,
}

impl std::fmt::Debug for Isolate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Isolate").finish_non_exhaustive()
    }
}

/// Runs a program's closure on the host's window.
struct EntryObj<F, R> {
    f: Option<F>,
    r: Option<R>,
    host: Rc<HostState>,
}

impl<F: FnOnce(&mut Stack<'_>) -> R, R> Entry for EntryObj<F, R> {
    extern "C" fn run<'s>(&mut self, s: StackArg<'s>) {
        let mut s = s.into_inner();
        let f = self.f.take().expect("the library enters once");
        let mut stack = Stack::new(&mut s, Rc::clone(&self.host));
        let start = stack.top();
        let r = f(&mut stack);
        let _ = stack.set_top(start);
        self.r = Some(r);
    }
}

/// A program's stream as the isolate's standard output.
struct WriterObj<W>(W);

impl<W: std::io::Write> Writer for WriterObj<W> {
    extern "C" fn write<'b>(
        &mut self,
        bytes: stabby::slice::Slice<'b, u8>,
    ) -> SResult<usize, SString> {
        match self.0.write(&bytes) {
            Ok(n) => SResult::Ok(n),
            Err(e) => SResult::Err(e.to_string().as_str().into()),
        }
    }
    extern "C" fn flush(&mut self) -> SResult<(), SString> {
        match self.0.flush() {
            Ok(()) => SResult::Ok(()),
            Err(e) => SResult::Err(e.to_string().as_str().into()),
        }
    }
}

impl Isolate {
    pub(crate) fn new(config: Config) -> Isolate {
        let libraries = Library::ALL
            .iter()
            .enumerate()
            .filter(|(_, l)| config.libraries.contains(l))
            .fold(0u16, |bits, (i, _)| bits | 1 << i);
        let obj = orbit_new_isolate(ConfigAbi {
            memory_limit: config.memory_limit.into(),
            libraries,
            gc_mode: config.gc_mode as u8,
            warnings: config.warnings,
            ignore_env: config.ignore_env,
            debugger: config.debugger == super::Debugger::Allowed,
        });
        let host = Rc::new(HostState::default());
        host.extension(|_: &mut Rc<Shared>| ());
        Isolate { obj, host }
    }

    /// Run `f` with a scope for making and using handles. Everything the
    /// scope made is released when `f` returns.
    pub fn scope<R>(&mut self, f: impl for<'s> FnOnce(&'s Scope<'s>) -> R) -> R {
        self.stack(|s| {
            let scope = Scope::new(s.reborrow());
            f(&scope)
        })
    }

    /// Run `f` on the host's window ([`Stack`]); what `f` leaves there is
    /// dropped after.
    pub fn stack<R>(&mut self, f: impl FnOnce(&mut Stack<'_>) -> R) -> R {
        let mut entry = EntryObj {
            f: Some(f),
            r: None,
            host: Rc::clone(&self.host),
        };
        self.obj.enter((&mut entry).into());
        entry.r.expect("the library runs the entry")
    }

    /// What reaches the isolate itself ([`Control`]).
    pub fn control(&mut self) -> Control<'_> {
        Control::new(self.obj.control(), Rc::clone(&self.host))
    }

    /// Send standard output (`print`, `io.stdout`) to `out`.
    pub fn set_stdout(&mut self, out: impl std::io::Write + std::fmt::Debug + 'static) {
        self.obj.set_stdout(SBox::new(WriterObj(out)).into())
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
        Gc { obj: &mut self.obj }
    }

    /// A handle that stops running code from any thread.
    pub fn interrupt(&self) -> Interrupt {
        Interrupt(self.obj.interrupt())
    }

    /// Flush open files and `print` output.
    pub fn flush(&mut self) {
        self.obj.flush()
    }

    /// How much `load` checks binary chunks before running them (`None`:
    /// not at all, so a malformed chunk can make the VM panic). Starts at
    /// what `ORBIT_VERIFY` asks for, or the most the library can check;
    /// levels past that are capped.
    #[cfg(feature = "verify")]
    pub fn set_binary_checks(&mut self, level: Option<BinaryChecks>) {
        self.obj.set_binary_checks(match level {
            None => 0,
            Some(BinaryChecks::Structure) => 1,
            Some(BinaryChecks::Full) => 2,
        })
    }

    /// Run `hook` when a script ends the process ([`Call::exit`], `os.exit`),
    /// after the isolate closes (if it does) and before standard output is
    /// flushed: for buffers the process exit would otherwise lose.
    ///
    /// [`Call::exit`]: crate::Call::exit
    pub fn before_exit(&mut self, f: impl Fn() + 'static) {
        self.obj.before_exit(hook(f))
    }

    /// Install the copy of `orbit-std` that `liborbit` carries, as
    /// `orbit_std::install` does with these options: a shared host's
    /// libraries then run inside the library, without crossing the
    /// boundary on each call.
    #[doc(hidden)]
    pub fn install_bundled_std(
        &mut self,
        base: bool,
        io: bool,
        os: bool,
        ignore_env: bool,
    ) -> Result<(), Error> {
        Result::<(), SString>::from(self.obj.install_std(base, io, os, ignore_env))
            .map_err(|m| Error::runtime(m.to_string()))
    }

    /// Install `liborbit`'s C module loader, as `orbit_capi::install` does.
    #[doc(hidden)]
    pub fn install_bundled_c_modules(&mut self, lock_cpath: bool) {
        self.obj.install_c_modules(lock_cpath)
    }

    /// Install `liborbit`'s `ffi` library; false if it was built without.
    #[doc(hidden)]
    pub fn install_bundled_ffi(&mut self) -> bool {
        self.obj.install_ffi()
    }
}

#[cfg(feature = "debugger")]
mod debug {
    use std::rc::Rc;

    use stabby::option::Option as SOption;

    use super::Isolate;
    use crate::abi::*;
    use crate::debugger::abi::{breakpoint_from, resume_to, stop_from};
    use crate::debugger::{Breakpoint, DebugEvents, ErrorBreak, PauseHandle, Paused, StopReason};
    use crate::error::Error;
    use crate::stack::backend::{HostState, on_window};

    /// [`PauseHandle`]'s state: none when no debugger can attach.
    #[derive(Clone)]
    pub(crate) struct PauseInner(Option<PauseArc>);

    impl std::fmt::Debug for PauseInner {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("PauseInner").finish_non_exhaustive()
        }
    }

    impl PauseInner {
        pub(crate) fn request(&self) {
            if let Some(p) = &self.0 {
                p.request()
            }
        }
    }

    /// A host's events, as the library reports to them.
    struct EventsObj(Box<dyn DebugEvents>, Rc<HostState>);

    impl DebugEventsAbi for EventsObj {
        extern "C" fn stopped<'a>(&mut self, s: StackArg<'a>, reason: StopAbi) -> u8 {
            let EventsObj(events, host) = self;
            let reason = stop_from(reason);
            on_window(host, s, |s| {
                let error = matches!(reason, StopReason::Error { .. });
                resume_to(events.stopped(&mut Paused::new(s, error), reason))
            })
        }

        extern "C" fn breakpoints_resolved<'b>(
            &mut self,
            source: stabby::slice::Slice<'b, u8>,
            bps: stabby::slice::Slice<'b, BreakpointAbi>,
        ) {
            let bps: Vec<Breakpoint> = bps.iter().map(|&b| breakpoint_from(b)).collect();
            self.0.breakpoints_resolved(&source, &bps)
        }
    }

    /// The debugger (ADR-12; feature `debugger`). See [`crate::debugger`].
    impl Isolate {
        /// Report stops to `events` from now on; `None` detaches the
        /// debugger, which also ends any step. Fails in an isolate made
        /// with [`crate::Debugger::Disabled`], or when `liborbit` was built
        /// without the engine.
        pub fn set_debug_events(
            &mut self,
            events: Option<Box<dyn DebugEvents>>,
        ) -> Result<(), Error> {
            let events: Option<DebugEventsBox> =
                events.map(|e| stabby::boxed::Box::new(EventsObj(e, Rc::clone(&self.host))).into());
            let events = match events {
                Some(e) => SOption::Some(e),
                None => SOption::None(),
            };
            if self.obj.set_debug_events(events) {
                Ok(())
            } else {
                Err(Error::runtime(
                    "this isolate allows no debugger, or liborbit was built without one",
                ))
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
            PauseHandle(PauseInner(self.obj.pause_handle().into()))
        }
    }
}

#[cfg(feature = "debugger")]
pub(crate) use debug::PauseInner;

/// A handle that stops running code in an isolate, from any thread.
#[derive(Clone)]
pub struct Interrupt(InterruptArc);

impl std::fmt::Debug for Interrupt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Interrupt").finish_non_exhaustive()
    }
}

impl Interrupt {
    /// Stop the running code at its next safe point with the error
    /// `interrupted!`.
    pub fn request(&self) {
        self.0.request()
    }

    /// Whether a request is still waiting to be seen.
    pub fn pending(&self) -> bool {
        self.0.pending()
    }
}

/// The collector of one isolate ([`Isolate::gc`]).
pub struct Gc<'a> {
    obj: &'a mut IsolateBox,
}

impl std::fmt::Debug for Gc<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Gc").finish_non_exhaustive()
    }
}

impl Gc<'_> {
    /// Run a full collection cycle.
    pub fn collect(&mut self) {
        self.obj.gc_collect()
    }

    /// Bytes in use, as `collectgarbage("count")` counts them.
    pub fn used_bytes(&self) -> usize {
        self.obj.gc_used_bytes()
    }

    pub fn set_mode(&mut self, mode: GcMode) {
        self.obj.gc_set_mode(mode as u8)
    }

    /// See [`Config::memory_limit`].
    pub fn set_limit(&mut self, bytes: Option<usize>) {
        self.obj.gc_set_limit(bytes.into())
    }

    pub fn limit(&self) -> Option<usize> {
        self.obj.gc_limit().into()
    }

    /// Collect at nearly every safe point after an allocation: slow, for
    /// shaking out code that keeps values the collector can't see.
    pub fn set_stress(&mut self, on: bool) {
        self.obj.gc_set_stress(on)
    }
}
