//! The C library's locales: the one module of `orbit-locale` that calls C
//! (ADR-6 exception, ADR-13). Everything here is a thin, safe wrapper; the
//! decisions are in `lib.rs`.
//!
//! On Unix (glibc, musl, macOS) a locale is a `newlocale` object, made
//! current on this thread with `uselocale` around the plain C functions
//! (`isalpha`, `strcoll`, `strftime`, ...), which then use it and nothing
//! else: the process's locale and other threads are never touched. On
//! Windows a locale is a `_create_locale` object for `_strcoll_l` and
//! `_strftime_l`; what has no `_l` form runs with the locale set for this
//! thread only (`_configthreadlocale`).

#![allow(unsafe_code)]

use std::ffi::{CStr, CString};

use orbit_runtime::locale::{CharTables, TimeParts, class};

/// The categories a locale is made of, in this order everywhere here:
/// collate, ctype, monetary, numeric, time, and on Unix messages (which
/// only `"all"` sets).
pub(crate) const COUNT: usize = if cfg!(windows) { 5 } else { 6 };

/// One name per category, in the order of [`COUNT`].
pub(crate) type Names = [Vec<u8>; COUNT];

/// The day and month names and the formats of the time category.
#[derive(Clone, Default)]
pub(crate) struct TimeNames {
    pub abday: [Vec<u8>; 7],
    pub day: [Vec<u8>; 7],
    pub abmon: [Vec<u8>; 12],
    pub mon: [Vec<u8>; 12],
    pub am: Vec<u8>,
    pub pm: Vec<u8>,
    /// What `%c`, `%x`, `%X`, `%r`, `%Ec`, `%Ex` and `%EX` expand to, where
    /// the C library says (not on Windows, where they are formatted with
    /// `strftime`).
    pub patterns: Option<[Vec<u8>; 7]>,
}

/// A C string literal (`c"..."` literals need Rust 1.77).
fn lit(bytes: &'static [u8]) -> &'static CStr {
    CStr::from_bytes_with_nul(bytes).expect("one NUL, at the end")
}

/// `bytes` up to its first NUL, as C sees it.
pub(crate) fn c_string(bytes: &[u8]) -> CString {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    CString::new(&bytes[..end]).expect("cut at the first NUL")
}

/// The bytes of a C string, copied. Null reads as empty.
///
/// # Safety
/// `p` is null or points to a NUL-terminated string.
unsafe fn copy_c(p: *const libc::c_char) -> Vec<u8> {
    if p.is_null() {
        Vec::new()
    } else {
        CStr::from_ptr(p).to_bytes().to_vec()
    }
}

/// The broken-down time C's `strftime` takes.
fn c_tm(tm: &TimeParts) -> libc::tm {
    let int = |v: i64| v.clamp(libc::c_int::MIN as i64, libc::c_int::MAX as i64) as libc::c_int;
    // SAFETY: `tm` is plain integers (and on some systems a zone pointer,
    // left null): all-zero is a valid value.
    let mut t: libc::tm = unsafe { std::mem::zeroed() };
    t.tm_year = int(tm.year - 1900);
    t.tm_mon = int(tm.month - 1);
    t.tm_mday = int(tm.day);
    t.tm_hour = int(tm.hour);
    t.tm_min = int(tm.min);
    t.tm_sec = int(tm.sec);
    t.tm_wday = int(tm.wday);
    t.tm_yday = int(tm.yday - 1);
    t.tm_isdst = libc::c_int::from(tm.isdst);
    t
}

/// A time on day `wday` (0 is Sunday) of month `month` (1 to 12), at
/// `hour`: what the names are read from where the C library has no
/// `nl_langinfo`.
#[cfg(windows)]
fn sample_time(wday: i64, month: i64, hour: i64) -> TimeParts {
    TimeParts {
        // 2023-01-01 was a Sunday.
        year: 2023,
        month,
        day: 1 + wday,
        hour,
        min: 0,
        sec: 0,
        wday,
        yday: 1 + wday,
        isdst: false,
    }
}

/// Which of C's classification functions say yes for byte `c`, and its
/// case forms, under whatever locale is current.
///
/// # Safety
/// None beyond calling C: every function takes any `unsigned char` value.
unsafe fn classify_current(c: u8) -> (u16, u8, u8) {
    let i = libc::c_int::from(c);
    let mut m = 0;
    let checks: [(unsafe extern "C" fn(libc::c_int) -> libc::c_int, u16); 10] = [
        (libc::isalpha, class::ALPHA),
        (libc::iscntrl, class::CNTRL),
        (libc::isdigit, class::DIGIT),
        (libc::isgraph, class::GRAPH),
        (libc::islower, class::LOWER),
        (libc::ispunct, class::PUNCT),
        (libc::isspace, class::SPACE),
        (libc::isupper, class::UPPER),
        (libc::isalnum, class::ALNUM),
        (libc::isxdigit, class::XDIGIT),
    ];
    for (f, bit) in checks {
        if f(i) != 0 {
            m |= bit;
        }
    }
    let byte = |r: libc::c_int| u8::try_from(r).unwrap_or(c);
    (m, byte(libc::toupper(i)), byte(libc::tolower(i)))
}

/// `strftime` into a growing buffer. `call` formats into `(buf, len)` and
/// returns the length written, 0 if it didn't fit (or wrote nothing).
fn grow_strftime(mut call: impl FnMut(*mut libc::c_char, usize) -> usize) -> Vec<u8> {
    let mut cap = 128;
    // C's formats are short: past this, the output is taken to be empty.
    while cap <= 4096 {
        let mut buf = vec![0u8; cap];
        let n = call(buf.as_mut_ptr().cast(), cap);
        if n > 0 {
            buf.truncate(n);
            return buf;
        }
        cap *= 4;
    }
    Vec::new()
}

#[cfg(unix)]
mod imp {
    use super::*;
    use libc::{c_int, locale_t};

    // Declared here rather than taken from `libc`, which leaves some of them
    // out for some targets.
    extern "C" {
        fn newlocale(mask: c_int, name: *const libc::c_char, base: locale_t) -> locale_t;
        fn duplocale(l: locale_t) -> locale_t;
        fn freelocale(l: locale_t);
        fn uselocale(l: locale_t) -> locale_t;
        fn nl_langinfo(item: libc::nl_item) -> *const libc::c_char;
        fn strftime(
            s: *mut libc::c_char,
            max: libc::size_t,
            format: *const libc::c_char,
            tm: *const libc::tm,
        ) -> libc::size_t;
    }

    /// The decimal point of whatever locale is current.
    ///
    /// # Safety
    /// None beyond calling C: `localeconv` returns a valid structure.
    unsafe fn decimal_point_current() -> Vec<u8> {
        let lc = libc::localeconv();
        if lc.is_null() {
            b".".to_vec()
        } else {
            copy_c((*lc).decimal_point)
        }
    }

    /// The decimal point C code on this thread sees.
    #[cfg(test)]
    pub(crate) fn thread_decimal_point() -> Vec<u8> {
        // SAFETY: see `decimal_point_current`.
        unsafe { decimal_point_current() }
    }

    /// `LC_GLOBAL_LOCALE`: `(locale_t)-1` on glibc, musl and macOS.
    const GLOBAL: locale_t = -1isize as locale_t;

    /// The categories' `LC_*` numbers, in the order of [`COUNT`].
    const LC: [c_int; COUNT] = [
        libc::LC_COLLATE,
        libc::LC_CTYPE,
        libc::LC_MONETARY,
        libc::LC_NUMERIC,
        libc::LC_TIME,
        libc::LC_MESSAGES,
    ];

    #[cfg(target_os = "macos")]
    fn mask(cat: usize) -> c_int {
        [
            libc::LC_COLLATE_MASK,
            libc::LC_CTYPE_MASK,
            libc::LC_MONETARY_MASK,
            libc::LC_NUMERIC_MASK,
            libc::LC_TIME_MASK,
            libc::LC_MESSAGES_MASK,
        ][cat]
    }

    /// glibc and musl: each category's mask is `1 << LC_*`.
    #[cfg(not(target_os = "macos"))]
    fn mask(cat: usize) -> c_int {
        1 << LC[cat]
    }

    /// Every category's mask.
    #[cfg(target_os = "macos")]
    const ALL_MASK: c_int = libc::LC_ALL_MASK;
    /// Every category's mask: glibc's has more categories than we name
    /// (`LC_PAPER` and so on), musl's is all bits.
    #[cfg(all(not(target_os = "macos"), target_env = "gnu"))]
    const ALL_MASK: c_int = libc::LC_ALL_MASK;
    #[cfg(all(not(target_os = "macos"), not(target_env = "gnu")))]
    const ALL_MASK: c_int = 0x7fff_ffff;

    /// A locale object, owned. Never `LC_GLOBAL_LOCALE`.
    #[derive(Debug)]
    pub(crate) struct LocaleObj(locale_t);

    impl Drop for LocaleObj {
        fn drop(&mut self) {
            // SAFETY: made by `newlocale` or `duplocale`, owned, and not in
            // use on any thread: a `ThreadPin` keeps its object alive.
            unsafe { freelocale(self.0) }
        }
    }

    impl LocaleObj {
        /// A locale with each category named by `names`; the names it
        /// reports are the same. `None` if one isn't available.
        pub(crate) fn new(names: &Names) -> Option<(LocaleObj, Names)> {
            let c = lit(b"C\0").as_ptr();
            // SAFETY: a valid name, no base.
            let mut obj = LocaleObj(unsafe { newlocale(ALL_MASK, c, std::ptr::null_mut()) });
            if obj.0.is_null() {
                return None;
            }
            for (cat, name) in names.iter().enumerate() {
                if name == b"C" {
                    continue;
                }
                let name = c_string(name);
                // SAFETY: on success `newlocale` takes over `obj.0` (it may
                // free it and return another); on failure it leaves it
                // alone, still ours. `forget` hands it over only once the
                // result is known.
                let next = unsafe { newlocale(mask(cat), name.as_ptr(), obj.0) };
                if next.is_null() {
                    return None;
                }
                std::mem::forget(obj);
                obj = LocaleObj(next);
            }
            Some((obj, names.clone()))
        }

        /// A copy of the process's locale.
        pub(crate) fn of_process() -> Option<LocaleObj> {
            // SAFETY: duplicating the global locale is allowed.
            let l = unsafe { duplocale(GLOBAL) };
            (!l.is_null()).then_some(LocaleObj(l))
        }

        /// Run `f` with this locale current on this thread.
        fn on_thread<R>(&self, f: impl FnOnce() -> R) -> R {
            // SAFETY: `self.0` is a live object for the whole call.
            let prev = unsafe { uselocale(self.0) };
            let r = f();
            // SAFETY: `prev` was the thread's locale a moment ago.
            unsafe { uselocale(prev) };
            r
        }

        pub(crate) fn chars(&self) -> CharTables {
            // SAFETY: see `classify_current`.
            self.on_thread(|| CharTables::from_fn(|c| unsafe { classify_current(c) }))
        }

        pub(crate) fn decimal_point(&self) -> Vec<u8> {
            // SAFETY: see `decimal_point_current`.
            self.on_thread(|| unsafe { decimal_point_current() })
        }

        /// `strcoll(a, b)` in this locale.
        pub(crate) fn strcoll(&self, a: &CStr, b: &CStr) -> c_int {
            // SAFETY: two NUL-terminated strings.
            self.on_thread(|| unsafe { libc::strcoll(a.as_ptr(), b.as_ptr()) })
        }

        /// `strftime(fmt, tm)` in this locale.
        pub(crate) fn strftime(&self, fmt: &CStr, tm: &TimeParts) -> Vec<u8> {
            let t = c_tm(tm);
            self.on_thread(|| {
                grow_strftime(|buf, len| {
                    // SAFETY: `buf` holds `len` bytes; `fmt` and `t` are
                    // valid for the call.
                    unsafe { strftime(buf, len, fmt.as_ptr(), &t) }
                })
            })
        }

        pub(crate) fn time_names(&self) -> TimeNames {
            self.on_thread(|| {
                // SAFETY: `nl_langinfo` returns a valid string for every
                // item, which stays valid until the next call on this
                // thread; it is copied at once.
                let info = |item: libc::nl_item| unsafe { copy_c(nl_langinfo(item)) };
                let from = |first: libc::nl_item| move |i: usize| info(first + i as libc::nl_item);
                TimeNames {
                    abday: std::array::from_fn(from(libc::ABDAY_1)),
                    day: std::array::from_fn(from(libc::DAY_1)),
                    abmon: std::array::from_fn(from(libc::ABMON_1)),
                    mon: std::array::from_fn(from(libc::MON_1)),
                    am: info(libc::AM_STR),
                    pm: info(libc::PM_STR),
                    patterns: Some({
                        let plain = [libc::D_T_FMT, libc::D_FMT, libc::T_FMT];
                        let era = [libc::ERA_D_T_FMT, libc::ERA_D_FMT, libc::ERA_T_FMT];
                        // A locale without eras, or without an era form of
                        // one format, uses the plain one for `%E`.
                        let has_eras = !info(libc::ERA).is_empty();
                        let alt = |i: usize| match info(era[i]) {
                            p if has_eras && !p.is_empty() => p,
                            _ => info(plain[i]),
                        };
                        [
                            info(plain[0]),
                            info(plain[1]),
                            info(plain[2]),
                            info(libc::T_FMT_AMPM),
                            alt(0),
                            alt(1),
                            alt(2),
                        ]
                    }),
                }
            })
        }
    }

    /// A locale made current on this thread; [`ThreadPin::restore`] puts
    /// back the one before. The caller keeps the object alive until then.
    #[derive(Debug)]
    pub(crate) struct ThreadPin(locale_t);

    impl ThreadPin {
        pub(crate) fn new(obj: &LocaleObj) -> ThreadPin {
            // SAFETY: `obj` is live, and the caller keeps it so until
            // `restore`.
            ThreadPin(unsafe { uselocale(obj.0) })
        }

        pub(crate) fn restore(self) {
            // SAFETY: the thread's locale before `new`.
            unsafe { uselocale(self.0) };
        }
    }

    /// The process's `setlocale(category, name)` (`None` is `LC_ALL`): the
    /// name, or `None` if it isn't available. Without a name, only asks.
    pub(crate) fn process_setlocale(cat: Option<usize>, name: Option<&[u8]>) -> Option<Vec<u8>> {
        let lc = cat.map_or(libc::LC_ALL, |c| LC[c]);
        let name = name.map(c_string);
        let p = name.as_ref().map_or(std::ptr::null(), |n| n.as_ptr());
        // SAFETY: a valid category and a NUL-terminated name or null. The
        // result is copied before any other `setlocale` call.
        unsafe {
            let r = libc::setlocale(lc, p);
            (!r.is_null()).then(|| copy_c(r))
        }
    }

    /// The name of the whole locale whose categories are `names`, as this
    /// C library's `setlocale(LC_ALL, NULL)` writes it when they differ.
    pub(crate) fn composite(names: &Names) -> Vec<u8> {
        let [collate, ctype, monetary, numeric, time, messages] = names;
        let mut out = Vec::new();
        if cfg!(target_os = "macos") {
            for (i, n) in [collate, ctype, monetary, numeric, time, messages]
                .iter()
                .enumerate()
            {
                if i > 0 {
                    out.push(b'/');
                }
                out.extend_from_slice(n);
            }
        } else if cfg!(target_env = "gnu") {
            let rest = [
                "LC_PAPER",
                "LC_NAME",
                "LC_ADDRESS",
                "LC_TELEPHONE",
                "LC_MEASUREMENT",
                "LC_IDENTIFICATION",
            ];
            let named = [
                ("LC_CTYPE", ctype),
                ("LC_NUMERIC", numeric),
                ("LC_TIME", time),
                ("LC_COLLATE", collate),
                ("LC_MONETARY", monetary),
                ("LC_MESSAGES", messages),
            ];
            let all = named
                .iter()
                .copied()
                .chain(rest.iter().map(|r| (*r, messages)));
            for (i, (key, n)) in all.enumerate() {
                if i > 0 {
                    out.push(b';');
                }
                out.extend_from_slice(key.as_bytes());
                out.push(b'=');
                out.extend_from_slice(n);
            }
        } else {
            // musl: the names in category order.
            for (i, n) in [ctype, numeric, time, collate, monetary, messages]
                .iter()
                .enumerate()
            {
                if i > 0 {
                    out.push(b';');
                }
                out.extend_from_slice(n);
            }
        }
        out
    }
}

#[cfg(windows)]
mod imp {
    use super::*;
    use libc::c_int;

    #[allow(non_camel_case_types)]
    type _locale_t = *mut libc::c_void;

    extern "C" {
        fn _create_locale(category: c_int, locale: *const libc::c_char) -> _locale_t;
        fn _free_locale(locale: _locale_t);
        fn _configthreadlocale(per_thread: c_int) -> c_int;
        fn _strcoll_l(a: *const libc::c_char, b: *const libc::c_char, l: _locale_t) -> c_int;
        fn _strftime_l(
            s: *mut libc::c_char,
            max: libc::size_t,
            format: *const libc::c_char,
            tm: *const libc::tm,
            l: _locale_t,
        ) -> libc::size_t;
        fn localeconv() -> *mut Lconv;
    }

    /// The start of the C runtime's `struct lconv`.
    #[repr(C)]
    struct Lconv {
        decimal_point: *mut libc::c_char,
    }

    /// The decimal point of whatever locale is current.
    ///
    /// # Safety
    /// None beyond calling C: `localeconv` returns a valid structure.
    unsafe fn decimal_point_current() -> Vec<u8> {
        let lc = localeconv();
        if lc.is_null() {
            b".".to_vec()
        } else {
            copy_c((*lc).decimal_point)
        }
    }

    const ENABLE_PER_THREAD: c_int = 1;

    const LC: [c_int; COUNT] = [
        libc::LC_COLLATE,
        libc::LC_CTYPE,
        libc::LC_MONETARY,
        libc::LC_NUMERIC,
        libc::LC_TIME,
    ];

    /// A locale object and the name it was made from (for this thread's
    /// `setlocale`).
    #[derive(Debug)]
    pub(crate) struct LocaleObj {
        loc: _locale_t,
        name: CString,
    }

    impl Drop for LocaleObj {
        fn drop(&mut self) {
            // SAFETY: made by `_create_locale`, owned.
            unsafe { _free_locale(self.loc) }
        }
    }

    /// Run `f` with `name` as this thread's locale, then put the thread
    /// back as it was. `None` if the C runtime doesn't know `name`.
    fn with_thread_locale<R>(name: &CStr, f: impl FnOnce() -> R) -> Option<R> {
        let pin = ThreadPin::set(name)?;
        let r = f();
        pin.restore();
        Some(r)
    }

    impl LocaleObj {
        /// A locale with each category named by `names`, and the names the
        /// C runtime gives them (`"Portuguese_Brazil.1252"` for
        /// `"Portuguese"`). `None` if one isn't available.
        pub(crate) fn new(names: &Names) -> Option<(LocaleObj, Names)> {
            let name = c_string(&composite(names));
            let canonical = with_thread_locale(&name, || {
                std::array::from_fn(|cat| {
                    // SAFETY: a valid category; copied at once.
                    unsafe { copy_c(libc::setlocale(LC[cat], std::ptr::null())) }
                })
            })?;
            // SAFETY: a valid category and name.
            let loc = unsafe { _create_locale(libc::LC_ALL, name.as_ptr()) };
            if loc.is_null() {
                return None;
            }
            Some((LocaleObj { loc, name }, canonical))
        }

        /// A copy of the process's locale.
        pub(crate) fn of_process() -> Option<LocaleObj> {
            let name = process_setlocale(None, None)?;
            let name = c_string(&name);
            // SAFETY: a valid category and name.
            let loc = unsafe { _create_locale(libc::LC_ALL, name.as_ptr()) };
            (!loc.is_null()).then_some(LocaleObj { loc, name })
        }

        pub(crate) fn chars(&self) -> CharTables {
            with_thread_locale(&self.name, || {
                // SAFETY: see `classify_current`.
                CharTables::from_fn(|c| unsafe { classify_current(c) })
            })
            .unwrap_or_else(CharTables::c)
        }

        pub(crate) fn decimal_point(&self) -> Vec<u8> {
            // SAFETY: see `decimal_point_current`.
            with_thread_locale(&self.name, || unsafe { decimal_point_current() })
                .unwrap_or_else(|| b".".to_vec())
        }

        pub(crate) fn strcoll(&self, a: &CStr, b: &CStr) -> c_int {
            // SAFETY: two NUL-terminated strings and a live locale.
            unsafe { _strcoll_l(a.as_ptr(), b.as_ptr(), self.loc) }
        }

        pub(crate) fn strftime(&self, fmt: &CStr, tm: &TimeParts) -> Vec<u8> {
            let t = c_tm(tm);
            grow_strftime(|buf, len| {
                // SAFETY: `buf` holds `len` bytes; the rest is valid for the
                // call.
                unsafe { _strftime_l(buf, len, fmt.as_ptr(), &t, self.loc) }
            })
        }

        pub(crate) fn time_names(&self) -> TimeNames {
            let f = |spec: &CStr, tm: TimeParts| self.strftime(spec, &tm);
            TimeNames {
                abday: std::array::from_fn(|d| f(lit(b"%a\0"), sample_time(d as i64, 1, 0))),
                day: std::array::from_fn(|d| f(lit(b"%A\0"), sample_time(d as i64, 1, 0))),
                abmon: std::array::from_fn(|m| f(lit(b"%b\0"), sample_time(0, m as i64 + 1, 0))),
                mon: std::array::from_fn(|m| f(lit(b"%B\0"), sample_time(0, m as i64 + 1, 0))),
                am: f(lit(b"%p\0"), sample_time(0, 1, 1)),
                pm: f(lit(b"%p\0"), sample_time(0, 1, 13)),
                patterns: None,
            }
        }
    }

    /// `name` set as this thread's locale; [`ThreadPin::restore`] puts back
    /// the thread's locale and mode from before. Empty if `name` couldn't
    /// be set.
    #[derive(Debug)]
    pub(crate) struct ThreadPin(Option<(c_int, CString)>);

    impl ThreadPin {
        fn set(name: &CStr) -> Option<ThreadPin> {
            // SAFETY: switching this thread to its own locale, then asking
            // for it (copied at once) and setting it; undone below on
            // failure and by `restore` otherwise.
            unsafe {
                let mode = _configthreadlocale(ENABLE_PER_THREAD);
                let prev = CString::new(copy_c(libc::setlocale(libc::LC_ALL, std::ptr::null())))
                    .unwrap_or_default();
                if libc::setlocale(libc::LC_ALL, name.as_ptr()).is_null() {
                    _configthreadlocale(mode);
                    return None;
                }
                Some(ThreadPin(Some((mode, prev))))
            }
        }

        pub(crate) fn new(obj: &LocaleObj) -> ThreadPin {
            ThreadPin::set(&obj.name).unwrap_or(ThreadPin(None))
        }

        pub(crate) fn restore(self) {
            if let Some((mode, prev)) = self.0 {
                // SAFETY: the thread's own locale and mode from before
                // `set`.
                unsafe {
                    libc::setlocale(libc::LC_ALL, prev.as_ptr());
                    _configthreadlocale(mode);
                }
            }
        }
    }

    pub(crate) fn process_setlocale(cat: Option<usize>, name: Option<&[u8]>) -> Option<Vec<u8>> {
        let lc = cat.map_or(libc::LC_ALL, |c| LC[c]);
        let name = name.map(c_string);
        let p = name.as_ref().map_or(std::ptr::null(), |n| n.as_ptr());
        // SAFETY: a valid category and a NUL-terminated name or null; the
        // result is copied at once.
        unsafe {
            let r = libc::setlocale(lc, p);
            (!r.is_null()).then(|| copy_c(r))
        }
    }

    /// The C runtime's name for a locale whose categories differ.
    pub(crate) fn composite(names: &Names) -> Vec<u8> {
        if names.iter().all(|n| *n == names[0]) {
            return names[0].clone();
        }
        let keys = [
            "LC_COLLATE",
            "LC_CTYPE",
            "LC_MONETARY",
            "LC_NUMERIC",
            "LC_TIME",
        ];
        let mut out = Vec::new();
        for (i, (key, n)) in keys.iter().zip(names).enumerate() {
            if i > 0 {
                out.push(b';');
            }
            out.extend_from_slice(key.as_bytes());
            out.push(b'=');
            out.extend_from_slice(n);
        }
        out
    }
}

#[cfg(all(test, unix))]
pub(crate) use imp::thread_decimal_point;
pub(crate) use imp::{LocaleObj, ThreadPin, composite, process_setlocale};
