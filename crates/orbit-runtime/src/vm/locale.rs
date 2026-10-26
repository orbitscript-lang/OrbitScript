//! The isolate's locale (ADR-13): what `os.setlocale` changes, through the
//! installed [`LocaleProvider`].

use crate::locale::{Category, Locale, LocaleMode, LocaleProvider, LocaleScope, TimeParts};
use crate::vm::Vm;

impl Vm {
    /// Get locales from `provider` from now on, starting with its current
    /// one.
    pub fn set_locale_provider(&mut self, provider: Box<dyn LocaleProvider>) {
        self.rt.locale_provider = provider;
        self.refresh_locale();
    }

    /// The locale numbers, patterns and string comparisons follow.
    pub fn locale(&self) -> &Locale {
        &self.rt.metas.locale
    }

    /// `os.setlocale(name, category, scope)`: the locale's name, or `None`
    /// if it isn't available.
    pub fn set_locale(
        &mut self,
        category: Category,
        name: Option<&[u8]>,
        scope: LocaleScope,
    ) -> Option<Vec<u8>> {
        let r = self.rt.locale_provider.set(category, name, scope);
        if r.is_some() && name.is_some() {
            self.refresh_locale();
        }
        r
    }

    pub fn locale_mode(&self) -> LocaleMode {
        self.rt.locale_provider.mode()
    }

    /// Whether scripts change the isolate's own locale or the process's.
    pub fn set_locale_mode(&mut self, mode: LocaleMode) {
        self.rt.locale_provider.set_mode(mode);
        self.refresh_locale();
    }

    /// See [`LocaleProvider::time_pattern`].
    pub fn locale_time_pattern(&self, spec: &[u8]) -> Option<Vec<u8>> {
        self.rt.locale_provider.time_pattern(spec)
    }

    /// See [`LocaleProvider::time_text`].
    pub fn locale_time_text(&self, spec: &[u8], tm: &TimeParts) -> Option<Vec<u8>> {
        self.rt.locale_provider.time_text(spec, tm)
    }

    /// Run `f`, which runs code, with the isolate's locale on this
    /// thread ([`LocaleProvider::enter`]). Every way into running code
    /// (`call_at`, nested resumes and closes) goes through here, so native
    /// code needs nothing of its own.
    pub(crate) fn with_thread_locale<R>(&mut self, f: impl FnOnce(&mut Vm) -> R) -> R {
        if self.rt.locale_provider.enter() {
            self.refresh_locale();
        }
        let r = f(self);
        self.rt.locale_provider.leave();
        r
    }

    fn refresh_locale(&mut self) {
        self.rt.metas.locale = self.rt.locale_provider.locale();
    }
}
