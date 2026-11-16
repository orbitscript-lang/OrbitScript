//! What the host configures: where output and warnings go, the compilers
//! `load` uses, the limits, the interrupt flag, the locale provider and the
//! libraries' own per-VM state. The runtime reads these; a host (the
//! `orbit` crate) sets them.

use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use super::{CompileFn, Extensions, Interrupt, Limits, Vm, WarnHook, WriteDebug};
use crate::locale::{CLocaleProvider, LocaleProvider};
use crate::text::{LineEndings, TextWriter};
use crate::vm::op;

/// The host's settings, one per VM ([`VmState::host`](super::VmState::host)).
#[derive(Debug)]
pub struct Host {
    limits: Limits,
    /// Where `print` writes. Buffered; call [`Vm::flush`] before exiting.
    out: Box<dyn WriteDebug>,
    /// Whether `warn` messages are shown (on stderr). Off at start, as in
    /// the reference interpreter; `warn("@on")` and `warn("@off")` switch it.
    warnings: bool,
    /// Where warnings go when the host takes them over (`lua_setwarnf`).
    warn_hook: Option<WarnHook>,
    /// How text streams treat line endings: `out` when it is the process's
    /// standard output, warnings, and `orbit-std`'s streams.
    line_endings: LineEndings,
    /// Set by an [`Interrupt`] handle, from any thread.
    interrupt: Arc<AtomicBool>,
    /// Asked first when an interrupt is seen ([`Host::set_interrupt_handler`]).
    interrupt_handler: Option<fn(&mut Vm) -> bool>,
    ext: Extensions,
    /// The Lua compiler. A hook rather than a dependency, so the VM stays
    /// free of the front ends.
    compile: Option<CompileFn>,
    /// The OrbitScript compiler, for source files whose name ends in `.orb`.
    compile_orbitscript: Option<CompileFn>,
    /// How much `load` checks binary chunks (`stdlib::checks`). Kept here,
    /// not in the registry, so scripts can't change it.
    binary_checks: Option<op::verify::Level>,
    /// Where `os.setlocale` gets locales ([`Vm::set_locale_provider`]).
    locale_provider: Box<dyn LocaleProvider>,
}

impl Host {
    pub(super) fn new() -> Self {
        Host {
            limits: Limits::default(),
            out: process_stdout(LineEndings::default()),
            line_endings: LineEndings::default(),
            warnings: false,
            warn_hook: None,
            interrupt: Arc::default(),
            interrupt_handler: None,
            ext: Extensions::default(),
            compile: None,
            compile_orbitscript: None,
            binary_checks: Some(op::verify::Level::Full),
            locale_provider: Box::new(CLocaleProvider::default()),
        }
    }

    /// Register slots on a stack before `stack overflow`. Read on every
    /// Lua call.
    #[inline(always)]
    pub(crate) fn max_stack(&self) -> usize {
        self.limits.max_stack
    }

    /// The stream `print` and `io.stdout` share.
    pub(crate) fn out(&mut self) -> &mut dyn WriteDebug {
        &mut *self.out
    }

    /// Send `print` output to `out`.
    pub fn set_out(&mut self, out: Box<dyn WriteDebug>) {
        self.out = out;
    }

    /// How text streams treat line endings ([`LineEndings`]).
    pub(crate) fn line_endings(&self) -> LineEndings {
        self.line_endings
    }

    /// Change how text streams treat line endings. Standard output starts
    /// over as the process's, flushed first.
    pub(crate) fn set_line_endings(&mut self, line_endings: LineEndings) {
        let _ = self.out.flush();
        self.line_endings = line_endings;
        self.out = process_stdout(line_endings);
    }

    /// Show `warn` messages or not, as `warn("@on")` and `warn("@off")` do.
    pub fn set_warnings(&mut self, on: bool) {
        self.warnings = on;
    }

    /// The host's warning function, if it took warnings over.
    pub(crate) fn warn_hook(&mut self) -> Option<&mut WarnHook> {
        self.warn_hook.as_mut()
    }

    /// Send every warning piece to `hook` instead of showing it.
    pub(crate) fn set_warn_hook(&mut self, hook: WarnHook) {
        self.warn_hook = Some(hook);
    }

    /// Emit a warning (Lua's `warn`): to the warning hook if there is one,
    /// otherwise on stderr as `Lua warning: msg` if warnings are on
    /// ([`Host::warnings`]; off by default).
    pub(crate) fn warn(&mut self, msg: &[u8]) {
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
        if self.line_endings.translates() {
            let _ = TextWriter::new(std::io::stderr()).write_all(&text);
        } else {
            let _ = std::io::stderr().write_all(&text);
        }
    }

    /// Whether an [`Interrupt`] handle asked for an interrupt that has not
    /// been raised yet. Read on every backward jump.
    #[inline(always)]
    pub(crate) fn interrupt_requested(&self) -> bool {
        self.interrupt.load(Ordering::Relaxed)
    }

    /// The interrupt flag, shared with the handles that raise it.
    pub(crate) fn interrupt_flag(&self) -> &Arc<AtomicBool> {
        &self.interrupt
    }

    /// A handle that raises the interrupt flag from any thread.
    pub(crate) fn interrupt_handle(&self) -> Interrupt {
        Interrupt(self.interrupt.clone())
    }

    /// Lower the interrupt flag.
    pub(crate) fn clear_interrupt(&self) {
        self.interrupt.store(false, Ordering::Relaxed);
    }

    pub(crate) fn interrupt_handler(&self) -> Option<fn(&mut Vm) -> bool> {
        self.interrupt_handler
    }

    /// `handler` is asked first when an interrupt is seen: it returns true
    /// if the interrupt was the host's (the C API's signal-safe
    /// `lua_sethook` raises the flag to have its hook installed) and has
    /// been handled, false to raise `interrupted!`.
    pub(crate) fn set_interrupt_handler(&mut self, handler: fn(&mut Vm) -> bool) {
        self.interrupt_handler = Some(handler);
    }

    /// Per-VM state of host libraries, one value per type (the `math`
    /// library's random generator, for example).
    pub(crate) fn ext(&self) -> &Extensions {
        &self.ext
    }

    /// [`Host::ext`], to add or change a library's state.
    pub fn ext_mut(&mut self) -> &mut Extensions {
        &mut self.ext
    }

    /// The compiler for source chunks: OrbitScript's or Lua's.
    pub(crate) fn compiler(&self, orbitscript: bool) -> Option<CompileFn> {
        if orbitscript {
            self.compile_orbitscript
        } else {
            self.compile
        }
    }

    /// Install the compilers `load`, `loadfile`, `dofile` and `require`
    /// use: `lua` for Lua source, `orbitscript` for files whose name ends
    /// in `.orb`. A VM starts with none, so the runtime and its libraries
    /// don't depend on a front end and a host can leave the compilers out.
    pub fn set_compilers(&mut self, lua: CompileFn, orbitscript: CompileFn) {
        self.compile = Some(lua);
        self.compile_orbitscript = Some(orbitscript);
    }

    pub(crate) fn binary_checks(&self) -> Option<op::verify::Level> {
        self.binary_checks
    }

    pub(crate) fn set_binary_checks(&mut self, level: Option<op::verify::Level>) {
        self.binary_checks = level;
    }

    pub(crate) fn locale_provider(&self) -> &dyn LocaleProvider {
        &*self.locale_provider
    }

    pub(crate) fn locale_provider_mut(&mut self) -> &mut dyn LocaleProvider {
        &mut *self.locale_provider
    }

    pub(crate) fn set_locale_provider(&mut self, provider: Box<dyn LocaleProvider>) {
        self.locale_provider = provider;
    }
}

/// The process's standard output, buffered, as a text stream if
/// `line_endings` translates.
fn process_stdout(line_endings: LineEndings) -> Box<dyn WriteDebug> {
    let out = std::io::BufWriter::new(std::io::stdout());
    if line_endings.translates() {
        Box::new(TextWriter::new(out))
    } else {
        Box::new(out)
    }
}
