//! Calendar arithmetic and `strftime`-style formatting for `os.date` and
//! `os.time`, in the C locale. Pure functions on plain numbers: the time
//! zone comes in as an offset from [`crate::tz`].
//!
//! The day-count conversions are the well-known proleptic Gregorian
//! formulas (days since 1970-01-01, with 400-year eras), valid for every
//! year an `i64` time can reach.

/// A broken-down time, like C's `struct tm` but with a full year and
/// 1-based month and year day.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tm {
    pub year: i64,
    /// 1 to 12.
    pub month: i64,
    /// 1 to 31.
    pub day: i64,
    pub hour: i64,
    pub min: i64,
    pub sec: i64,
    /// 0 (Sunday) to 6.
    pub wday: i64,
    /// 1 to 366.
    pub yday: i64,
    pub isdst: bool,
    /// Seconds east of UTC.
    pub offset: i64,
    /// Zone abbreviation for `%Z`.
    pub zone: String,
}

/// Days from 1970-01-01 to `year-month-day` (month 1 to 12).
pub fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The date `days` after 1970-01-01, as (year, month, day).
pub fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

/// Break down `local`, seconds since the epoch already shifted into the
/// wanted zone.
pub fn breakdown(local: i64, offset: i64, isdst: bool, zone: &str) -> Tm {
    let days = local.div_euclid(86_400);
    let secs = local.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    Tm {
        year,
        month,
        day,
        hour: secs / 3600,
        min: secs / 60 % 60,
        sec: secs % 60,
        // 1970-01-01 was a Thursday.
        wday: (days + 4).rem_euclid(7),
        yday: days - days_from_civil(year, 1, 1) + 1,
        isdst,
        offset,
        zone: zone.to_string(),
    }
}

/// Seconds since the epoch of a local time given as possibly
/// out-of-range fields (`month` 13 is January of the next year, `sec` -1
/// the last second of the previous minute), before any zone offset.
pub fn local_seconds(year: i64, month: i64, day: i64, hour: i64, min: i64, sec: i64) -> i64 {
    let m0 = month - 1;
    let y = year + m0.div_euclid(12);
    let m = m0.rem_euclid(12) + 1;
    let days = days_from_civil(y, m, 1) + day - 1;
    days * 86_400 + hour * 3600 + min * 60 + sec
}

/// Is `spec` (what follows a `%`) a conversion `os.date` accepts, and how
/// many bytes does it take? The C99 conversions plus the `E` and `O`
/// modifiers POSIX allows.
pub fn conversion_len(spec: &[u8]) -> Option<usize> {
    match spec {
        [b'E', c, ..] if b"cCxXyY".contains(c) => Some(2),
        [b'O', c, ..] if b"deHImMSuUVwWy".contains(c) => Some(2),
        [c, ..] if b"aAbBcCdDeFgGhHIjmMnprRStTuUVwWxXyYzZ%".contains(c) => Some(1),
        _ => None,
    }
}

const DAYS: [&str; 7] = [
    "Sunday",
    "Monday",
    "Tuesday",
    "Wednesday",
    "Thursday",
    "Friday",
    "Saturday",
];
const MONTHS: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];

/// ISO 8601 week-numbering year and week of `tm`.
fn iso_week(tm: &Tm) -> (i64, i64) {
    // Weeks start on Monday; week 1 holds the year's first Thursday.
    let weeks_in = |y: i64| {
        let p =
            |y: i64| (y + y.div_euclid(4) - y.div_euclid(100) + y.div_euclid(400)).rem_euclid(7);
        if p(y) == 4 || p(y - 1) == 3 { 53 } else { 52 }
    };
    let iso_wday = if tm.wday == 0 { 7 } else { tm.wday };
    let week = (tm.yday - iso_wday + 10) / 7;
    if week < 1 {
        (tm.year - 1, weeks_in(tm.year - 1))
    } else if week > weeks_in(tm.year) {
        (tm.year + 1, 1)
    } else {
        (tm.year, week)
    }
}

/// Format one conversion (`spec` is one or two bytes, already checked by
/// [`conversion_len`]) of `tm` onto `out`.
fn convert(out: &mut Vec<u8>, spec: &[u8], tm: &Tm) {
    use std::io::Write;
    let c = *spec.last().unwrap_or(&b'%');
    let hour12 = if tm.hour % 12 == 0 { 12 } else { tm.hour % 12 };
    let w = &mut *out;
    let _ = match c {
        b'a' => write!(w, "{}", &DAYS[tm.wday as usize][..3]),
        b'A' => write!(w, "{}", DAYS[tm.wday as usize]),
        b'b' | b'h' => write!(w, "{}", &MONTHS[tm.month as usize - 1][..3]),
        b'B' => write!(w, "{}", MONTHS[tm.month as usize - 1]),
        b'c' => {
            convert(w, b"a", tm);
            w.push(b' ');
            convert(w, b"b", tm);
            w.push(b' ');
            convert(w, b"e", tm);
            w.push(b' ');
            convert(w, b"T", tm);
            w.push(b' ');
            convert(w, b"Y", tm);
            Ok(())
        }
        b'C' => write!(w, "{:02}", tm.year.div_euclid(100)),
        b'd' => write!(w, "{:02}", tm.day),
        b'D' | b'x' => write!(
            w,
            "{:02}/{:02}/{:02}",
            tm.month,
            tm.day,
            tm.year.rem_euclid(100)
        ),
        b'e' => write!(w, "{:2}", tm.day),
        b'F' => write!(w, "{}-{:02}-{:02}", tm.year, tm.month, tm.day),
        b'g' => write!(w, "{:02}", iso_week(tm).0.rem_euclid(100)),
        b'G' => write!(w, "{}", iso_week(tm).0),
        b'H' => write!(w, "{:02}", tm.hour),
        b'I' => write!(w, "{hour12:02}"),
        b'j' => write!(w, "{:03}", tm.yday),
        b'm' => write!(w, "{:02}", tm.month),
        b'M' => write!(w, "{:02}", tm.min),
        b'n' => writeln!(w),
        b'p' => write!(w, "{}", if tm.hour < 12 { "AM" } else { "PM" }),
        b'r' => write!(
            w,
            "{hour12:02}:{:02}:{:02} {}",
            tm.min,
            tm.sec,
            if tm.hour < 12 { "AM" } else { "PM" }
        ),
        b'R' => write!(w, "{:02}:{:02}", tm.hour, tm.min),
        b'S' => write!(w, "{:02}", tm.sec),
        b't' => write!(w, "\t"),
        b'T' | b'X' => write!(w, "{:02}:{:02}:{:02}", tm.hour, tm.min, tm.sec),
        b'u' => write!(w, "{}", if tm.wday == 0 { 7 } else { tm.wday }),
        b'U' => write!(w, "{:02}", (tm.yday - 1 + 7 - tm.wday) / 7),
        b'V' => write!(w, "{:02}", iso_week(tm).1),
        b'w' => write!(w, "{}", tm.wday),
        b'W' => write!(w, "{:02}", (tm.yday - 1 + 7 - (tm.wday + 6) % 7) / 7),
        b'y' => write!(w, "{:02}", tm.year.rem_euclid(100)),
        b'Y' => write!(w, "{}", tm.year),
        b'z' => {
            let sign = if tm.offset < 0 { '-' } else { '+' };
            let a = tm.offset.abs();
            write!(w, "{sign}{:02}{:02}", a / 3600, a / 60 % 60)
        }
        b'Z' => write!(w, "{}", tm.zone),
        _ => write!(w, "%"),
    };
}

/// Format `fmt` with `tm`, or return the bad conversion: the format from
/// that `%` to its end, which is what the error message shows.
pub fn strftime(fmt: &[u8], tm: &Tm) -> Result<Vec<u8>, Vec<u8>> {
    let mut out = Vec::with_capacity(fmt.len());
    let mut i = 0;
    while i < fmt.len() {
        if fmt[i] != b'%' {
            out.push(fmt[i]);
            i += 1;
            continue;
        }
        let spec = &fmt[i + 1..];
        let Some(n) = conversion_len(spec) else {
            return Err(fmt[i..].to_vec());
        };
        convert(&mut out, &spec[..n], tm);
        i += 1 + n;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn utc(t: i64) -> Tm {
        breakdown(t, 0, false, "GMT")
    }

    fn fmt(f: &str, t: i64) -> String {
        String::from_utf8(strftime(f.as_bytes(), &utc(t)).unwrap()).unwrap()
    }

    #[test]
    fn day_counts_round_trip() {
        for d in [
            -1_000_000_000,
            -719_468,
            -1,
            0,
            1,
            11_016,
            59,
            60,
            1_000_000_000,
        ] {
            let (y, m, dd) = civil_from_days(d);
            assert_eq!(days_from_civil(y, m, dd), d);
        }
        assert_eq!(days_from_civil(2000, 3, 1), 11_017);
    }

    // Expected strings printed by the reference lua5.5 (`os.date("!...")`).
    #[test]
    fn formats_match_the_reference() {
        assert_eq!(fmt("%c", 0), "Thu Jan  1 00:00:00 1970");
        let t = 86_400 * 45 + 3600 * 15 + 61;
        assert_eq!(
            fmt("%x %X %p %y %j %U %W %w %a %A %b %B %h %e %D %F %T %R", t),
            "02/15/70 15:01:01 PM 70 046 07 06 0 Sun Sunday Feb February Feb 15 02/15/70 1970-02-15 15:01:01 15:01"
        );
        assert_eq!(
            fmt("%n%t%% %G %g %V %u %C %I %M %S %z %Z", t),
            "\n\t% 1970 70 07 7 19 03 01 01 +0000 GMT"
        );
        assert_eq!(fmt("%c", -(1 << 40)), "Sat Nov 12 23:23:44 -32873");
        assert_eq!(fmt("%C %y %G %g", -(1 << 40)), "-329 27 -32873 27");
        assert_eq!(
            fmt("%C|%y|%Y|%G|%g|%F", -62_167_219_200 - 86_400),
            "-1|99|-1|-1|99|-1-12-31"
        );
        assert_eq!(fmt("%e|%I|%p", 3600 * 13), " 1|01|PM");
        assert_eq!(fmt("%I %p", 0), "12 AM");
        assert_eq!(fmt("%U %W %V %G", 1_104_537_600), "00 00 53 2004");
        assert_eq!(
            fmt("%Y-%m-%d %j %U %W %G %V", 67_767_976_233_316_800),
            "2147483647-12-29 363 52 51 2147483647 52"
        );
        assert_eq!(
            fmt(
                "%Ec|%EC|%Ex|%EX|%Ey|%EY|%Od|%Oe|%OH|%OI|%Om|%OM|%OS|%Ou|%OU|%OV|%Ow|%OW|%Oy",
                0
            ),
            "Thu Jan  1 00:00:00 1970|19|01/01/70|00:00:00|70|1970|01| 1|00|12|01|00|00|4|00|01|4|00|70"
        );
    }

    #[test]
    fn bad_conversions_report_the_rest() {
        let tm = utc(0);
        assert_eq!(strftime(b"ab%Qxy", &tm), Err(b"%Qxy".to_vec()));
        assert_eq!(strftime(b"%", &tm), Err(b"%".to_vec()));
        assert_eq!(strftime(b"%Ea", &tm), Err(b"%Ea".to_vec()));
        assert_eq!(strftime(b"%O", &tm), Err(b"%O".to_vec()));
        assert_eq!(strftime(b"%9", &tm), Err(b"%9".to_vec()));
    }

    #[test]
    fn normalizes_fields() {
        // {year=2005, month=1, day=1, hour=1, min=0, sec=-3602}
        let t = local_seconds(2005, 1, 1, 1, 0, -3602);
        assert_eq!(t, 1_104_537_598);
        let tm = utc(t);
        assert_eq!(
            (tm.year, tm.month, tm.day, tm.hour, tm.min, tm.sec, tm.yday),
            (2004, 12, 31, 23, 59, 58, 366)
        );
        // {year=2000, month=14, day=-3, hour=25, min=61, sec=-1}
        assert_eq!(local_seconds(2000, 14, -3, 25, 61, -1), 980_733_659);
    }
}
