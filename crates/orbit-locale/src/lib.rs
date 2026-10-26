//! Locales from the C library for OrbitScript (ADR-13): the
//! [`LocaleProvider`] behind `os.setlocale`.
//!
//! In [`LocaleMode::Isolate`] each isolate has a locale of its own, a C
//! library locale object that nothing else sees: the process's locale, the
//! host and other isolates are never changed. While native code (a C
//! module, `ffi`) runs, the object is made the thread's locale, so C code
//! formats and compares as the script asked. In [`LocaleMode::Process`]
//! `os.setlocale` is the C library's `setlocale`, as in PUC Lua, and the
//! isolate follows what the host does to the process's locale.
//!
//! The names `os.setlocale` returns are the C library's: the name asked
//! for (cut at a NUL), `"C"` for `"POSIX"`, the environment's for `""`
//! (`LC_ALL`, then `LC_<CATEGORY>`, then `LANG`), and the C library's own
//! form for a whole locale whose categories differ. On Windows the C
//! runtime names them (`"Portuguese_Brazil.1252"`).
//!
//! The only C calls are in `sys.rs`. Elsewhere (platforms other than
//! Linux, macOS and Windows) [`provider`] gives the C-only provider.

#[cfg(any(target_os = "linux", target_os = "macos", windows))]
mod sys;

use orbit_runtime::locale::LocaleProvider;

/// The provider for a new isolate: the C library's locales in
/// [`LocaleMode::Isolate`], starting in the C locale.
pub fn provider() -> Box<dyn LocaleProvider> {
    #[cfg(any(target_os = "linux", target_os = "macos", windows))]
    return Box::new(libc_locale::LibcProvider::default());
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    return Box::new(orbit_runtime::locale::CLocaleProvider::default());
}

pub use orbit_runtime::locale::LocaleMode;

#[cfg(any(target_os = "linux", target_os = "macos", windows))]
mod libc_locale {
    use std::cell::RefCell;
    use std::cmp::Ordering;
    use std::ffi::CStr;
    use std::rc::Rc;

    use orbit_runtime::locale::{
        Category, CharTables, Collate, Locale, LocaleMode, LocaleProvider, TimeParts,
    };

    use crate::sys::{self, LocaleObj, Names, ThreadPin, TimeNames};

    const COLLATE: usize = 0;
    const CTYPE: usize = 1;
    const NUMERIC: usize = 3;
    const TIME: usize = 4;

    /// The index of `cat` in [`Names`]; `None` for `"all"`.
    fn index(cat: Category) -> Option<usize> {
        match cat {
            Category::All => None,
            Category::Collate => Some(COLLATE),
            Category::Ctype => Some(CTYPE),
            Category::Monetary => Some(2),
            Category::Numeric => Some(NUMERIC),
            Category::Time => Some(TIME),
        }
    }

    fn is_c(name: &[u8]) -> bool {
        name == b"C" || name == b"POSIX"
    }

    /// The name `""` means for category `cat` (Unix): from the environment,
    /// as the C library reads it.
    #[cfg(unix)]
    fn environment_name(cat: usize) -> Vec<u8> {
        use std::os::unix::ffi::OsStrExt;
        let var = [
            "LC_COLLATE",
            "LC_CTYPE",
            "LC_MONETARY",
            "LC_NUMERIC",
            "LC_TIME",
            "LC_MESSAGES",
        ][cat];
        ["LC_ALL", var, "LANG"]
            .iter()
            .filter_map(std::env::var_os)
            .find(|v| !v.is_empty())
            .map_or_else(|| b"C".to_vec(), |v| v.as_bytes().to_vec())
    }

    /// `name` up to its first NUL, as C sees it.
    fn cut(name: &[u8]) -> &[u8] {
        &name[..name.iter().position(|&b| b == 0).unwrap_or(name.len())]
    }

    /// What the C library takes `name` to be for category `cat`: `""`
    /// resolved from the environment, and `"POSIX"` named `"C"`.
    #[cfg(unix)]
    fn requested(name: &[u8], cat: usize) -> Vec<u8> {
        let name = match cut(name) {
            b"" => environment_name(cat),
            n => n.to_vec(),
        };
        if name == b"POSIX" {
            b"C".to_vec()
        } else {
            name
        }
    }

    /// The Windows C runtime resolves and names locales itself.
    #[cfg(windows)]
    fn requested(name: &[u8], _cat: usize) -> Vec<u8> {
        cut(name).to_vec()
    }

    /// `strcoll` order. Strings compare a piece at a time between their
    /// zero bytes, which C strings can't hold, as lua5.5 compares them.
    struct Collator {
        obj: Rc<LocaleObj>,
        /// Both pieces, NUL-terminated, so a comparison doesn't allocate.
        buf: RefCell<Vec<u8>>,
    }

    // Short `Debug` impls: derived ones would pull in printing for every
    // field (about 12 KiB of release `orbit`).
    impl std::fmt::Debug for Collator {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("Collator")
        }
    }

    impl Collate for Collator {
        fn compare(&self, mut a: &[u8], mut b: &[u8]) -> Ordering {
            let mut buf = self.buf.borrow_mut();
            loop {
                let la = a.iter().position(|&c| c == 0).unwrap_or(a.len());
                let lb = b.iter().position(|&c| c == 0).unwrap_or(b.len());
                buf.clear();
                buf.extend_from_slice(&a[..la]);
                buf.push(0);
                let mid = buf.len();
                buf.extend_from_slice(&b[..lb]);
                buf.push(0);
                let ca = CStr::from_bytes_with_nul(&buf[..mid]).expect("one NUL, at the end");
                let cb = CStr::from_bytes_with_nul(&buf[mid..]).expect("one NUL, at the end");
                match self.obj.strcoll(ca, cb).cmp(&0) {
                    Ordering::Equal => {}
                    o => return o,
                }
                match (la == a.len(), lb == b.len()) {
                    (true, true) => return Ordering::Equal,
                    (true, false) => return Ordering::Less,
                    (false, true) => return Ordering::Greater,
                    (false, false) => {
                        a = &a[la + 1..];
                        b = &b[lb + 1..];
                    }
                }
            }
        }
    }

    pub(crate) struct LibcProvider {
        mode: LocaleMode,
        /// Each category's name, as `os.setlocale` reports it.
        names: Names,
        /// The locale object; `None` while every category is C.
        obj: Option<Rc<LocaleObj>>,
        /// What the runtime reads.
        locale: Locale,
        /// The time category's names; `None` while it is C.
        time: Option<TimeNames>,
        /// In process mode, the process's locale as last seen.
        process_seen: Vec<u8>,
        /// Runs [`LocaleProvider::enter`] started and not yet left.
        depth: u32,
        /// While running in isolate mode with a locale other than C: the
        /// object installed on the thread (kept alive until then) and the
        /// thread's locale before.
        pin: Option<(Rc<LocaleObj>, ThreadPin)>,
    }

    impl std::fmt::Debug for LibcProvider {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("LibcProvider")
                .field("mode", &self.mode)
                .finish_non_exhaustive()
        }
    }

    impl Default for LibcProvider {
        fn default() -> LibcProvider {
            LibcProvider {
                mode: LocaleMode::Isolate,
                names: std::array::from_fn(|_| b"C".to_vec()),
                obj: None,
                locale: Locale::c(),
                time: None,
                process_seen: Vec::new(),
                depth: 0,
                pin: None,
            }
        }
    }

    impl LibcProvider {
        /// The name `os.setlocale` reports for category `cat` (`None` for
        /// the whole locale).
        fn name_of(&self, cat: Option<usize>) -> Vec<u8> {
            match cat {
                Some(i) => self.names[i].clone(),
                None if self.names.iter().all(|n| *n == self.names[0]) => self.names[0].clone(),
                None => sys::composite(&self.names),
            }
        }

        /// Make what the runtime reads from the object and names.
        fn rebuild(&mut self) {
            let Some(obj) = &self.obj else {
                self.locale = Locale::c();
                self.time = None;
                return;
            };
            let chars = if is_c(&self.names[CTYPE]) {
                CharTables::c()
            } else {
                obj.chars()
            };
            let decimal_point = if is_c(&self.names[NUMERIC]) {
                b".".to_vec()
            } else {
                obj.decimal_point()
            };
            let collate: Option<Rc<dyn Collate>> = if is_c(&self.names[COLLATE]) {
                None
            } else {
                Some(Rc::new(Collator {
                    obj: Rc::clone(obj),
                    buf: RefCell::default(),
                }))
            };
            self.locale = Locale::new(&decimal_point, Rc::new(chars), collate);
            self.time = (!is_c(&self.names[TIME])).then(|| obj.time_names());
        }

        /// Take on the process's locale.
        fn adopt_process(&mut self) {
            self.process_seen = sys::process_setlocale(None, None).unwrap_or_default();
            for (i, n) in self.names.iter_mut().enumerate() {
                *n = sys::process_setlocale(Some(i), None).unwrap_or_else(|| b"C".to_vec());
            }
            self.obj = if self.names.iter().all(|n| is_c(n)) {
                None
            } else {
                LocaleObj::of_process().map(Rc::new)
            };
            self.rebuild();
        }

        fn set_own(&mut self, cat: Option<usize>, name: &[u8]) -> Option<Vec<u8>> {
            let mut names = self.names.clone();
            match cat {
                Some(i) => names[i] = requested(name, i),
                None => {
                    for (i, n) in names.iter_mut().enumerate() {
                        *n = requested(name, i);
                    }
                }
            }
            if names.iter().all(|n| is_c(n)) {
                self.names = std::array::from_fn(|_| b"C".to_vec());
                self.obj = None;
            } else {
                let (obj, canonical) = LocaleObj::new(&names)?;
                self.names = canonical;
                self.obj = Some(Rc::new(obj));
            }
            self.rebuild();
            self.repin();
            Some(self.name_of(cat))
        }

        /// While running, make the thread's locale match the object (or
        /// give the thread its own back).
        fn repin(&mut self) {
            if self.depth == 0 {
                return;
            }
            if let Some((_obj, pin)) = self.pin.take() {
                pin.restore();
            }
            if let (Some(obj), LocaleMode::Isolate) = (&self.obj, self.mode) {
                self.pin = Some((Rc::clone(obj), ThreadPin::new(obj)));
            }
        }

        /// In process mode, take on the host's changes to the process's
        /// locale since last seen; true if there were any.
        fn sync(&mut self) -> bool {
            if self.mode != LocaleMode::Process {
                return false;
            }
            let now = sys::process_setlocale(None, None).unwrap_or_default();
            if now == self.process_seen {
                return false;
            }
            self.adopt_process();
            true
        }
    }

    impl LocaleProvider for LibcProvider {
        fn set(&mut self, category: Category, name: Option<&[u8]>) -> Option<Vec<u8>> {
            let cat = index(category);
            match (self.mode, name) {
                (LocaleMode::Isolate, None) => Some(self.name_of(cat)),
                (LocaleMode::Isolate, Some(name)) => self.set_own(cat, name),
                (LocaleMode::Process, _) => {
                    let r = sys::process_setlocale(cat, name);
                    if r.is_some() && name.is_some() {
                        self.adopt_process();
                    }
                    r
                }
            }
        }

        fn locale(&self) -> Locale {
            self.locale.clone()
        }

        fn mode(&self) -> LocaleMode {
            self.mode
        }

        fn set_mode(&mut self, mode: LocaleMode) {
            if mode == self.mode {
                return;
            }
            self.mode = mode;
            if mode == LocaleMode::Process {
                self.adopt_process();
            }
            // To the isolate's own: it keeps the process's locale as it was,
            // in the object `adopt_process` copied.
            self.repin();
        }

        fn time_pattern(&self, spec: &[u8]) -> Option<Vec<u8>> {
            let patterns = self.time.as_ref()?.patterns.as_ref()?;
            let p = match spec {
                b"c" => &patterns[0],
                b"x" => &patterns[1],
                b"X" => &patterns[2],
                b"r" => &patterns[3],
                b"Ec" => &patterns[4],
                b"Ex" => &patterns[5],
                b"EX" => &patterns[6],
                _ => return None,
            };
            // An empty `%r` format means the C one, as in glibc.
            (!p.is_empty()).then(|| p.clone())
        }

        fn time_text(&self, spec: &[u8], tm: &TimeParts) -> Option<Vec<u8>> {
            let names = self.time.as_ref()?;
            let day = tm.wday.clamp(0, 6) as usize;
            let month = (tm.month - 1).clamp(0, 11) as usize;
            let text = match spec {
                b"a" => names.abday[day].clone(),
                b"A" => names.day[day].clone(),
                b"b" | b"h" => names.abmon[month].clone(),
                b"B" => names.mon[month].clone(),
                b"p" if tm.hour < 12 => names.am.clone(),
                b"p" => names.pm.clone(),
                _ => {
                    let mut fmt = vec![b'%'];
                    fmt.extend_from_slice(spec);
                    let fmt = sys::c_string(&fmt);
                    self.obj.as_ref()?.strftime(&fmt, tm)
                }
            };
            Some(text)
        }

        fn enter(&mut self) -> bool {
            self.depth += 1;
            if self.mode == LocaleMode::Process {
                // Every run: C code may change the process's locale and
                // call back in.
                return self.sync();
            }
            if self.depth == 1 {
                self.repin();
            }
            false
        }

        fn leave(&mut self) {
            self.depth -= 1;
            if self.depth == 0 {
                if let Some((_obj, pin)) = self.pin.take() {
                    pin.restore();
                }
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[cfg(unix)]
        fn has(name: &str) -> bool {
            let mut p = LibcProvider::default();
            p.set(Category::All, Some(name.as_bytes())).is_some()
        }

        #[test]
        fn starts_in_c() {
            let mut p = LibcProvider::default();
            assert_eq!(p.set(Category::All, None), Some(b"C".to_vec()));
            assert_eq!(p.set(Category::All, Some(b"POSIX")), Some(b"C".to_vec()));
            assert_eq!(p.set(Category::Numeric, Some(b"no_SUCH.locale")), None);
            assert!(p.locale().c_numbers());
        }

        #[cfg(target_os = "linux")]
        #[test]
        fn per_category() {
            if !has("pt_BR.utf8") {
                return;
            }
            let mut p = LibcProvider::default();
            assert_eq!(
                p.set(Category::Numeric, Some(b"pt_BR.utf8")),
                Some(b"pt_BR.utf8".to_vec())
            );
            assert_eq!(p.locale().decimal_point(), b",");
            assert!(p.locale().byte_order());
            let all = p.set(Category::All, None).unwrap();
            if cfg!(target_env = "gnu") {
                assert!(all.starts_with(b"LC_CTYPE=C;LC_NUMERIC=pt_BR.utf8;LC_TIME=C;"));
            }
            // The process's locale is untouched.
            assert_eq!(
                sys::process_setlocale(Some(NUMERIC), None),
                Some(b"C".to_vec())
            );
        }

        #[cfg(target_os = "linux")]
        #[test]
        fn latin1_classes_and_collation() {
            if !has("pt_BR.iso88591") {
                return;
            }
            let mut p = LibcProvider::default();
            p.set(Category::All, Some(b"pt_BR.iso88591")).unwrap();
            let l = p.locale();
            assert!(l.chars().is(0xe1, orbit_runtime::locale::class::ALPHA));
            assert_eq!(l.chars().upper[0xe1], 0xc1);
            assert_eq!(l.compare(b"alo", b"\xe1lo"), Ordering::Less);
            assert_eq!(l.compare(b"\xe1lo", b"amo"), Ordering::Less);
            assert_eq!(l.compare(b"a\0b", b"a\0c"), Ordering::Less);
            assert_eq!(l.compare(b"a", b"a\0"), Ordering::Less);
            let tm = TimeParts {
                year: 1970,
                month: 2,
                day: 10,
                wday: 2,
                yday: 41,
                ..TimeParts::default()
            };
            assert_eq!(p.time_text(b"B", &tm), Some(b"fevereiro".to_vec()));
        }

        #[cfg(unix)]
        #[test]
        fn pinned_on_the_thread() {
            if !has("pt_BR.utf8") {
                return;
            }
            let point = sys::thread_decimal_point;
            let mut p = LibcProvider::default();
            p.set(Category::Numeric, Some(b"pt_BR.utf8")).unwrap();
            // Outside a run the thread keeps its own.
            assert_eq!(point(), b".");
            p.enter();
            assert_eq!(point(), b",");
            // Runs nest; only the outermost gives the thread back.
            p.enter();
            p.leave();
            assert_eq!(point(), b",");
            // `os.setlocale` while running takes effect at once.
            p.set(Category::Numeric, Some(b"C")).unwrap();
            assert_eq!(point(), b".");
            p.set(Category::All, Some(b"pt_BR.utf8")).unwrap();
            assert_eq!(point(), b",");
            p.leave();
            assert_eq!(point(), b".");

            // A second isolate running inside the first one's native code
            // puts the first one's back when it returns.
            let mut q = LibcProvider::default();
            q.set(Category::Numeric, Some(b"C")).unwrap();
            p.enter();
            q.enter();
            assert_eq!(point(), b",", "a C isolate leaves the thread alone");
            q.set(Category::Numeric, Some(b"pt_BR.utf8")).unwrap();
            q.leave();
            assert_eq!(point(), b",");
            p.set(Category::Numeric, Some(b"C")).unwrap();
            q.enter();
            assert_eq!(point(), b",");
            q.leave();
            assert_eq!(point(), b".");
            p.leave();
            assert_eq!(point(), b".");
        }
    }
}
