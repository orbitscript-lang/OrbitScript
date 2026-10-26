//! The isolate's locale (ADR-13): what `os.setlocale` changes, through the
//! installed [`LocaleProvider`].

use crate::locale::{Category, Locale, LocaleMode, LocaleProvider, TimeParts};
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

    /// `os.setlocale(name, category)`: the locale's name, or `None` if it
    /// isn't available.
    pub fn set_locale(&mut self, category: Category, name: Option<&[u8]>) -> Option<Vec<u8>> {
        let r = self.rt.locale_provider.set(category, name);
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

    /// In [`LocaleMode::Process`], take on changes the host made to the
    /// process's locale.
    pub fn sync_locale(&mut self) {
        if self.rt.locale_provider.sync() {
            self.refresh_locale();
        }
    }

    /// See [`LocaleProvider::time_pattern`].
    pub fn locale_time_pattern(&self, spec: &[u8]) -> Option<Vec<u8>> {
        self.rt.locale_provider.time_pattern(spec)
    }

    /// See [`LocaleProvider::time_text`].
    pub fn locale_time_text(&self, spec: &[u8], tm: &TimeParts) -> Option<Vec<u8>> {
        self.rt.locale_provider.time_text(spec, tm)
    }

    /// See [`LocaleProvider::enter_native`].
    pub fn enter_native_locale(&mut self) {
        self.rt.locale_provider.enter_native();
    }

    /// See [`LocaleProvider::leave_native`].
    pub fn leave_native_locale(&mut self) {
        self.rt.locale_provider.leave_native();
    }

    fn refresh_locale(&mut self) {
        self.rt.metas.locale = self.rt.locale_provider.locale();
    }
}
