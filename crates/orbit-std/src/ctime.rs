//! Local time through the C library, for `os.date` and `os.time`:
//! `localtime_r`, `gmtime_r`, `mktime` and `strftime_l` (on Windows the C
//! runtime's `_localtime64_s`, `_gmtime64_s`, `_mktime64` and
//! `_strftime_l`), so dates follow the system's time zone rules exactly as
//! the reference interpreter's do. An exception to the workspace's
//! `unsafe` rule (ADR-6).
//!
//! Script data reaches C only as numbers: the time, and the fields of a
//! date table for `os.time`, each checked to fit a C `int`. A format string
//! never does: `os.date` checks every conversion letter in safe Rust and
//! passes one conversion at a time, and the `struct tm` `strftime` reads
//! always comes from the C library itself.

#![allow(unsafe_code)]

use std::ffi::{CStr, CString, c_int};

use orbit::TimeParts;
/// A broken-down time from the C library (`os.date` and `os.time`): its
/// `struct tm`, and the zone abbreviation copied out of it, which the
/// `tm_zone` field points to from then on where `struct tm` has one.
pub(crate) struct Time {
    tm: libc::tm,
    zone: CString,
}

impl Time {
    /// The time in the C library's `struct tm`, its zone copied out.
    fn new(tm: libc::tm) -> Time {
        #[cfg(unix)]
        let zone = if tm.tm_zone.is_null() {
            CString::default()
        } else {
            // SAFETY: a non-null `tm_zone` from the C library points to a
            // NUL-terminated abbreviation, copied at once.
            unsafe { CStr::from_ptr(tm.tm_zone) }.to_owned()
        };
        #[cfg(windows)]
        let zone = windows::zone_name(tm.tm_isdst > 0);
        #[cfg_attr(windows, allow(unused_mut))]
        let mut time = Time { tm, zone };
        // From now on the zone is the copy, which lives as long as `tm`.
        #[cfg(unix)]
        {
            time.tm.tm_zone = time.zone.as_ptr();
        }
        time
    }

    /// Seconds east of UTC.
    fn offset(&self) -> i64 {
        #[cfg(unix)]
        {
            // A C `long`, which is 64 bits on every target (ADR-6).
            self.tm.tm_gmtoff
        }
        #[cfg(windows)]
        {
            windows::offset(self.tm.tm_isdst > 0)
        }
    }

    /// The fields, as `os.date("*t")` gives them and a locale formats them.
    pub(crate) fn parts(&self) -> TimeParts {
        let tm = &self.tm;
        TimeParts {
            year: i64::from(tm.tm_year) + 1900,
            month: i64::from(tm.tm_mon) + 1,
            day: i64::from(tm.tm_mday),
            hour: i64::from(tm.tm_hour),
            min: i64::from(tm.tm_min),
            sec: i64::from(tm.tm_sec),
            wday: i64::from(tm.tm_wday),
            yday: i64::from(tm.tm_yday) + 1,
            isdst: tm.tm_isdst > 0,
            offset: self.offset(),
            zone: self.zone.as_bytes().to_vec(),
        }
    }

    /// One `strftime` conversion (`spec`, what follows the `%`) of this
    /// time in the "C" locale, whatever the process's locale is.
    pub(crate) fn format(&self, spec: &[u8]) -> Vec<u8> {
        let mut fmt = Vec::with_capacity(spec.len() + 2);
        fmt.push(b'%');
        fmt.extend(spec.iter().take_while(|&&b| b != 0));
        let fmt = CString::new(fmt).expect("NULs left out");
        let mut cap = 64;
        // One conversion is short: past this, the output is taken to be
        // empty, as `strftime` reports both the same way.
        while cap <= 4096 {
            let mut buf = vec![0u8; cap];
            let n = c_locale_strftime(&mut buf, &fmt, &self.tm);
            if n > 0 {
                buf.truncate(n);
                return buf;
            }
            cap *= 4;
        }
        Vec::new()
    }
}

/// The time `t` (seconds since the epoch) in the local time zone, or
/// `None` where the C library can't represent it.
pub(crate) fn local_time(t: i64) -> Option<Time> {
    let t = libc::time_t::try_from(t).ok()?;
    // SAFETY: an all-zero `struct tm` is a valid value to be overwritten
    // (integers, and on Unix a zone pointer left null).
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: valid pointers to a `time_t` and a `struct tm` the call
    // fills; tzset reads `TZ`, so a changed one applies.
    let ok = unsafe {
        sys::tzset();
        sys::localtime(&t, &mut tm)
    };
    ok.then(|| Time::new(tm))
}

/// The time `t` in UTC (`os.date("!...")`), or `None` where the C library
/// can't represent it.
pub(crate) fn utc_time(t: i64) -> Option<Time> {
    let t = libc::time_t::try_from(t).ok()?;
    // SAFETY: as in `local_time`.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: valid pointers to a `time_t` and a `struct tm` the call fills.
    let ok = unsafe { sys::gmtime(&t, &mut tm) };
    ok.then(|| Time::new(tm))
}

/// A local time from fields that may be out of range (`os.time`), which
/// `mktime` normalizes: `year` is years since 1900, `month` 0 to 11, and
/// `isdst` says whether daylight saving time is in effect (`None`: let the
/// C library decide). The time in seconds since the epoch, `None` when
/// `mktime` fails (like lua5.5, a result of exactly -1 counts as failing,
/// since that is how `mktime` reports it), and the fields as `mktime` left
/// them.
pub(crate) fn make_local_time(
    year: c_int,
    month: c_int,
    day: c_int,
    hour: c_int,
    min: c_int,
    sec: c_int,
    isdst: Option<bool>,
) -> (Option<i64>, Time) {
    // SAFETY: as in `local_time`.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    tm.tm_year = year;
    tm.tm_mon = month;
    tm.tm_mday = day;
    tm.tm_hour = hour;
    tm.tm_min = min;
    tm.tm_sec = sec;
    tm.tm_isdst = isdst.map_or(-1, c_int::from);
    // SAFETY: a valid `struct tm`, which the call normalizes in place.
    let t = unsafe {
        sys::tzset();
        sys::mktime(&mut tm)
    };
    ((t != -1).then_some(t as i64), Time::new(tm))
}

/// `strftime(fmt, tm)` into `buf` in the "C" locale: the length written,
/// 0 if it didn't fit.
fn c_locale_strftime(buf: &mut [u8], fmt: &CStr, tm: &libc::tm) -> usize {
    let Some(c) = sys::c_locale() else {
        return 0;
    };
    // SAFETY: `buf` holds `buf.len()` bytes; `fmt` is NUL-terminated;
    // `tm` is a valid `struct tm` whose zone pointer (Unix) points into
    // the `Time` that owns it; `c` is a live locale object.
    unsafe { sys::strftime_l(buf.as_mut_ptr().cast(), buf.len(), fmt.as_ptr(), tm, c) }
}

#[cfg(unix)]
mod sys {
    use std::sync::OnceLock;

    use libc::{c_char, c_int, locale_t, size_t, time_t, tm};

    // Declared here rather than taken from `libc`, which leaves some of
    // them out for some targets.
    extern "C" {
        pub(super) fn tzset();
        fn localtime_r(t: *const time_t, out: *mut tm) -> *mut tm;
        fn gmtime_r(t: *const time_t, out: *mut tm) -> *mut tm;
        pub(super) fn mktime(t: *mut tm) -> time_t;
        fn newlocale(mask: c_int, name: *const c_char, base: locale_t) -> locale_t;
        pub(super) fn strftime_l(
            s: *mut c_char,
            max: size_t,
            format: *const c_char,
            tm: *const tm,
            l: locale_t,
        ) -> size_t;
    }

    /// # Safety
    /// `t` and `out` are valid.
    pub(super) unsafe fn localtime(t: &time_t, out: &mut tm) -> bool {
        !localtime_r(t, out).is_null()
    }

    /// # Safety
    /// `t` and `out` are valid.
    pub(super) unsafe fn gmtime(t: &time_t, out: &mut tm) -> bool {
        !gmtime_r(t, out).is_null()
    }

    /// The "C" locale as an object, made once and kept for the process.
    pub(super) fn c_locale() -> Option<locale_t> {
        // The object's address: a `locale_t` is a pointer, which a static
        // can't hold, and it is only ever read after it is made.
        static C: OnceLock<usize> = OnceLock::new();
        let p = *C.get_or_init(|| {
            // SAFETY: a NUL-terminated name and no base object.
            let l = unsafe { newlocale(libc::LC_ALL_MASK, b"C\0".as_ptr().cast(), 0 as locale_t) };
            l as usize
        });
        (p != 0).then_some(p as locale_t)
    }
}

#[cfg(windows)]
mod sys {
    use std::sync::OnceLock;

    use libc::{c_char, c_int, c_void, size_t, time_t, tm};

    /// The C runtime's `_locale_t`.
    pub(super) type Locale = *mut c_void;

    // The C runtime's names for the 64-bit `time_t` functions, which its
    // headers map `localtime_s`, `gmtime_s` and `mktime` to.
    extern "C" {
        #[link_name = "_tzset"]
        pub(super) fn tzset();
        fn _localtime64_s(out: *mut tm, t: *const time_t) -> c_int;
        fn _gmtime64_s(out: *mut tm, t: *const time_t) -> c_int;
        #[link_name = "_mktime64"]
        pub(super) fn mktime(t: *mut tm) -> time_t;
        fn _create_locale(category: c_int, name: *const c_char) -> Locale;
        #[link_name = "_strftime_l"]
        pub(super) fn strftime_l(
            s: *mut c_char,
            max: size_t,
            format: *const c_char,
            tm: *const tm,
            l: Locale,
        ) -> size_t;
        fn _get_tzname(len: *mut size_t, buf: *mut c_char, size: size_t, index: c_int) -> c_int;
        fn _get_timezone(seconds: *mut libc::c_long) -> c_int;
        fn _get_dstbias(seconds: *mut libc::c_long) -> c_int;
    }

    /// # Safety
    /// `t` and `out` are valid.
    pub(super) unsafe fn localtime(t: &time_t, out: &mut tm) -> bool {
        _localtime64_s(out, t) == 0
    }

    /// # Safety
    /// `t` and `out` are valid.
    pub(super) unsafe fn gmtime(t: &time_t, out: &mut tm) -> bool {
        _gmtime64_s(out, t) == 0
    }

    /// The "C" locale as an object, made once and kept for the process.
    pub(super) fn c_locale() -> Option<Locale> {
        static C: OnceLock<usize> = OnceLock::new();
        let p = *C.get_or_init(|| {
            // SAFETY: a NUL-terminated name.
            unsafe { _create_locale(libc::LC_ALL, b"C\0".as_ptr().cast()) as usize }
        });
        (p != 0).then_some(p as Locale)
    }

    /// The local zone's standard (`dst` false) or daylight saving name,
    /// as `strftime`'s `%Z` gives it.
    pub(super) fn zone_name(dst: bool) -> std::ffi::CString {
        let mut buf = [0u8; 64];
        let mut len: size_t = 0;
        // SAFETY: `buf` holds `buf.len()` bytes, `len` is written.
        let r = unsafe {
            _get_tzname(
                &mut len,
                buf.as_mut_ptr().cast(),
                buf.len(),
                c_int::from(dst),
            )
        };
        let end = buf.iter().position(|&b| b == 0).unwrap_or(0);
        if r != 0 {
            return std::ffi::CString::default();
        }
        std::ffi::CString::new(&buf[..end]).unwrap_or_default()
    }

    /// Seconds east of UTC for the local zone, standard or daylight
    /// saving time, from the C runtime's zone (`_timezone` is seconds
    /// west, `_dstbias` the daylight saving shift, negative).
    pub(super) fn offset(dst: bool) -> i64 {
        let (mut west, mut bias) = (0, 0);
        // SAFETY: each writes one integer.
        unsafe {
            _get_timezone(&mut west);
            _get_dstbias(&mut bias);
        }
        let west = i64::from(west) + if dst { i64::from(bias) } else { 0 };
        -west
    }
}

#[cfg(windows)]
use sys as windows;
