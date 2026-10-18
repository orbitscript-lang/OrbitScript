//! The local time zone for `os.date` and `os.time`.
//!
//! Stable `std` can't read the local zone, so this reads it the way the C
//! library on Unix does, from public formats: the `TZ` variable (a zone
//! name under the zoneinfo directory, a file path, or a POSIX rule string
//! such as `EST5EDT,M3.2.0,M11.1.0`), else `/etc/localtime`. Zone files are
//! TZif (RFC 8536); times past a file's last transition follow the POSIX
//! rule in its footer. Anything unreadable means UTC. Elsewhere than Unix
//! the local zone is UTC.
//!
//! The zone is re-read when `TZ` changes, as the C library does.

use std::cell::RefCell;
use std::ffi::OsString;

/// The local zone's rules at one instant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Local {
    /// Seconds east of UTC.
    pub offset: i64,
    pub isdst: bool,
    /// Abbreviation for `%Z`.
    pub zone: String,
}

/// One local time type: an offset, whether it's daylight saving time, and
/// its abbreviation.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Kind {
    offset: i64,
    isdst: bool,
    abbr: String,
}

/// Where in the year a POSIX rule switches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Day {
    /// `Jn`: day 1 to 365, never counting February 29.
    Julian1(i64),
    /// `n`: day 0 to 365, counting February 29.
    Julian0(i64),
    /// `Mm.w.d`: day `d` (0 is Sunday) of week `w` (5 is the last) of month `m`.
    Month { month: i64, week: i64, wday: i64 },
}

/// A POSIX `TZ` rule: a standard type, and optionally a daylight type with
/// the local times it starts and ends.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Rule {
    std: Kind,
    dst: Option<(Kind, Day, i64, Day, i64)>,
}

/// A loaded zone.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Zone {
    /// Transition times, ascending, and the type each one switches to.
    transitions: Vec<(i64, usize)>,
    kinds: Vec<Kind>,
    /// For times after the last transition (or all times, without a file).
    rule: Option<Rule>,
}

fn utc_kind() -> Kind {
    Kind {
        offset: 0,
        isdst: false,
        abbr: "UTC".into(),
    }
}

impl Zone {
    fn utc() -> Zone {
        Zone {
            transitions: Vec::new(),
            kinds: Vec::new(),
            rule: Some(Rule {
                std: utc_kind(),
                dst: None,
            }),
        }
    }

    fn at(&self, t: i64) -> Kind {
        let i = self.transitions.partition_point(|&(at, _)| at <= t);
        if i == 0 {
            if self.transitions.is_empty()
                && let Some(r) = &self.rule
            {
                return r.at(t);
            }
            // Before the first transition: the first standard-time type.
            return self
                .kinds
                .iter()
                .find(|k| !k.isdst)
                .or(self.kinds.first())
                .cloned()
                .unwrap_or_else(utc_kind);
        }
        if i == self.transitions.len()
            && let Some(r) = &self.rule
        {
            return r.at(t);
        }
        self.kinds[self.transitions[i - 1].1].clone()
    }
}

impl Rule {
    fn at(&self, t: i64) -> Kind {
        let Some((dst, start, start_time, end, end_time)) = &self.dst else {
            return self.std.clone();
        };
        // The year of `t` in standard time is the one whose rules apply
        // (the switches happen near mid-year or at year ends).
        let (year, _, _) = crate::time::civil_from_days((t + self.std.offset).div_euclid(86_400));
        // glibc uses 1970's switches for every earlier time.
        let year = year.max(1970);
        let in_dst = |year: i64| {
            // Switch instants in UTC: the start is in standard local
            // time, the end in daylight local time.
            let s = day_of(*start, year) * 86_400 + start_time - self.std.offset;
            let e = day_of(*end, year) * 86_400 + end_time - dst.offset;
            if s < e {
                s <= t && t < e
            } else {
                // Southern hemisphere: daylight time spans the new year.
                !(e <= t && t < s)
            }
        };
        if in_dst(year) {
            dst.clone()
        } else {
            self.std.clone()
        }
    }
}

/// Days since the epoch of a rule's switch day in `year`.
fn day_of(day: Day, year: i64) -> i64 {
    use crate::time::days_from_civil;
    let jan1 = days_from_civil(year, 1, 1);
    let leap = days_from_civil(year, 3, 1) - days_from_civil(year, 2, 1) == 29;
    match day {
        Day::Julian1(n) => jan1 + n - 1 + i64::from(leap && n >= 60),
        Day::Julian0(n) => jan1 + n,
        Day::Month { month, week, wday } => {
            let first = days_from_civil(year, month, 1);
            let next = if month == 12 {
                days_from_civil(year + 1, 1, 1)
            } else {
                days_from_civil(year, month + 1, 1)
            };
            // 1970-01-01 was a Thursday (4).
            let first_wday = (first + 4).rem_euclid(7);
            let mut d = first + (wday - first_wday).rem_euclid(7) + (week - 1) * 7;
            while d >= next {
                d -= 7;
            }
            d
        }
    }
}

/// A POSIX rule parser over bytes.
struct P<'a> {
    s: &'a [u8],
    i: usize,
}

impl P<'_> {
    fn peek(&self) -> Option<u8> {
        self.s.get(self.i).copied()
    }

    fn eat(&mut self, c: u8) -> bool {
        if self.peek() == Some(c) {
            self.i += 1;
            true
        } else {
            false
        }
    }

    fn name(&mut self) -> Option<String> {
        let start;
        let end;
        if self.eat(b'<') {
            start = self.i;
            while self.peek()? != b'>' {
                self.i += 1;
            }
            end = self.i;
            self.i += 1;
        } else {
            start = self.i;
            while self.peek().is_some_and(|c| c.is_ascii_alphabetic()) {
                self.i += 1;
            }
            end = self.i;
        }
        (end - start >= 3).then(|| String::from_utf8_lossy(&self.s[start..end]).into_owned())
    }

    fn num(&mut self) -> Option<i64> {
        let start = self.i;
        while self.peek().is_some_and(|c| c.is_ascii_digit()) {
            self.i += 1;
        }
        std::str::from_utf8(&self.s[start..self.i])
            .ok()?
            .parse()
            .ok()
    }

    /// `[+-]hh[:mm[:ss]]` as seconds.
    fn hms(&mut self) -> Option<i64> {
        let neg = if self.eat(b'-') {
            true
        } else {
            self.eat(b'+');
            false
        };
        let mut secs = self.num()? * 3600;
        if self.eat(b':') {
            secs += self.num()? * 60;
            if self.eat(b':') {
                secs += self.num()?;
            }
        }
        Some(if neg { -secs } else { secs })
    }

    fn day(&mut self) -> Option<Day> {
        if self.eat(b'J') {
            Some(Day::Julian1(self.num()?))
        } else if self.eat(b'M') {
            let month = self.num()?;
            self.eat(b'.').then_some(())?;
            let week = self.num()?;
            self.eat(b'.').then_some(())?;
            let wday = self.num()?;
            Some(Day::Month { month, week, wday })
        } else {
            Some(Day::Julian0(self.num()?))
        }
    }

    /// A switch: a day and an optional local time (default 02:00).
    fn switch(&mut self) -> Option<(Day, i64)> {
        let d = self.day()?;
        let t = if self.eat(b'/') { self.hms()? } else { 7200 };
        Some((d, t))
    }
}

/// Parse a POSIX `TZ` rule. Offsets in the string count west of UTC.
fn parse_rule(s: &[u8]) -> Option<Rule> {
    let mut p = P { s, i: 0 };
    let std_abbr = p.name()?;
    let std_off = -p.hms()?;
    let std = Kind {
        offset: std_off,
        isdst: false,
        abbr: std_abbr,
    };
    if p.peek().is_none() {
        return Some(Rule { std, dst: None });
    }
    let dst_abbr = p.name()?;
    let dst_off = match p.peek() {
        Some(b',') | None => std_off + 3600,
        _ => -p.hms()?,
    };
    let dst = Kind {
        offset: dst_off,
        isdst: true,
        abbr: dst_abbr,
    };
    // Without switch rules, the C library uses the United States' rules.
    let (start, end) = if p.eat(b',') {
        let start = p.switch()?;
        p.eat(b',').then_some(())?;
        (start, p.switch()?)
    } else {
        let m = |month, week| Day::Month {
            month,
            week,
            wday: 0,
        };
        ((m(3, 2), 7200), (m(11, 1), 7200))
    };
    (p.peek().is_none()).then_some(Rule {
        std,
        dst: Some((dst, start.0, start.1, end.0, end.1)),
    })
}

/// Parse a TZif file (RFC 8536), using its 64-bit data when present.
fn parse_tzif(data: &[u8]) -> Option<Zone> {
    struct Header {
        isutcnt: usize,
        isstdcnt: usize,
        leapcnt: usize,
        timecnt: usize,
        typecnt: usize,
        charcnt: usize,
    }
    fn header(d: &[u8]) -> Option<(u8, Header)> {
        if d.get(..4)? != b"TZif" {
            return None;
        }
        let n = |i: usize| -> Option<usize> {
            Some(u32::from_be_bytes(d.get(20 + 4 * i..24 + 4 * i)?.try_into().ok()?) as usize)
        };
        Some((
            d[4],
            Header {
                isutcnt: n(0)?,
                isstdcnt: n(1)?,
                leapcnt: n(2)?,
                timecnt: n(3)?,
                typecnt: n(4)?,
                charcnt: n(5)?,
            },
        ))
    }
    let block_len = |h: &Header, tsize: usize| {
        h.timecnt * tsize
            + h.timecnt
            + h.typecnt * 6
            + h.charcnt
            + h.leapcnt * (tsize + 4)
            + h.isstdcnt
            + h.isutcnt
    };
    let (version, h1) = header(data)?;
    let (body, h, tsize) = if version >= b'2' {
        let v2 = data.get(44 + block_len(&h1, 4)..)?;
        let (_, h2) = header(v2)?;
        (&v2[44..], h2, 8)
    } else {
        (&data[44..], h1, 4)
    };
    let mut at = 0;
    let mut take = |n: usize| -> Option<&[u8]> {
        let s = body.get(at..at + n)?;
        at += n;
        Some(s)
    };
    let times = take(h.timecnt * tsize)?;
    let idx = take(h.timecnt)?;
    let types = take(h.typecnt * 6)?;
    let chars = take(h.charcnt)?;
    take(h.leapcnt * (tsize + 4))?;
    take(h.isstdcnt)?;
    take(h.isutcnt)?;
    let footer = &body[at..];
    let mut kinds = Vec::with_capacity(h.typecnt);
    for t in types.chunks_exact(6) {
        let offset = i64::from(i32::from_be_bytes(t[..4].try_into().ok()?));
        let ai = t[5] as usize;
        let end = chars
            .get(ai..)?
            .iter()
            .position(|&c| c == 0)
            .map_or(chars.len(), |p| ai + p);
        kinds.push(Kind {
            offset,
            isdst: t[4] != 0,
            abbr: String::from_utf8_lossy(&chars[ai..end]).into_owned(),
        });
    }
    let mut transitions = Vec::with_capacity(h.timecnt);
    for (i, &k) in idx.iter().enumerate() {
        let b = &times[i * tsize..(i + 1) * tsize];
        let t = if tsize == 8 {
            i64::from_be_bytes(b.try_into().ok()?)
        } else {
            i64::from(i32::from_be_bytes(b.try_into().ok()?))
        };
        if k as usize >= kinds.len() {
            return None;
        }
        transitions.push((t, k as usize));
    }
    let rule = match footer {
        [b'\n', rest @ ..] => {
            let end = rest.iter().position(|&c| c == b'\n')?;
            if end == 0 {
                None
            } else {
                parse_rule(&rest[..end])
            }
        }
        _ => None,
    };
    if kinds.is_empty() {
        return None;
    }
    Some(Zone {
        transitions,
        kinds,
        rule,
    })
}

/// Load the zone `TZ` names (`None`: unset).
fn load(tz: Option<&OsString>) -> Zone {
    if !cfg!(unix) {
        return Zone::utc();
    }
    let read = |p: &std::path::Path| std::fs::read(p).ok().and_then(|d| parse_tzif(&d));
    let Some(tz) = tz else {
        return read("/etc/localtime".as_ref()).unwrap_or_else(Zone::utc);
    };
    let bytes = crate::sys::os_bytes(tz);
    let name = bytes.strip_prefix(b":").unwrap_or(&bytes);
    if name.is_empty() {
        // glibc's name for UTC when `TZ` is set but empty.
        let mut z = Zone::utc();
        if let Some(r) = &mut z.rule {
            r.std.abbr = "Universal".into();
        }
        return z;
    }
    let path = crate::sys::os_path(name);
    let file = if path.is_absolute() {
        read(&path)
    } else if name.windows(2).any(|w| w == b"..") {
        None
    } else {
        let dir = std::env::var_os("TZDIR").unwrap_or_else(|| "/usr/share/zoneinfo".into());
        read(&std::path::Path::new(&dir).join(&path))
    };
    file.or_else(|| {
        parse_rule(name).map(|rule| Zone {
            transitions: Vec::new(),
            kinds: Vec::new(),
            rule: Some(rule),
        })
    })
    .unwrap_or_else(Zone::utc)
}

thread_local! {
    static CACHE: RefCell<Option<(Option<OsString>, Zone)>> = const { RefCell::new(None) };
}

fn with_zone<T>(f: impl FnOnce(&Zone) -> T) -> T {
    let tz = std::env::var_os("TZ");
    CACHE.with(|c| {
        let mut c = c.borrow_mut();
        if c.as_ref().is_none_or(|(key, _)| *key != tz) {
            let zone = load(tz.as_ref());
            *c = Some((tz, zone));
        }
        f(&c.as_ref().expect("just filled").1)
    })
}

/// The local zone at `utc` seconds since the epoch.
pub fn at(utc: i64) -> Local {
    with_zone(|z| {
        let k = z.at(utc);
        Local {
            offset: k.offset,
            isdst: k.isdst,
            zone: k.abbr,
        }
    })
}

/// The instant whose local time is `local` seconds since the epoch, as
/// glibc's `mktime` finds it, given the date table's `isdst`:
///
/// - A local time that occurs once has one answer, unless `isdst` asks for
///   the other kind of time: then it's read with that kind's offset (the
///   zone's nearest one, or an hour's difference in a zone that has none).
/// - A local time that occurs twice (the hour clocks go back) is read as
///   `isdst` asks, or as standard time.
/// - A local time skipped when clocks go forward is read with the offset
///   `isdst` asks for, or the one in force before the switch.
pub fn from_local(local: i64, isdst: Option<bool>) -> i64 {
    with_zone(|z| {
        // The kinds of time in force around `local`.
        let mut near: Vec<Kind> = Vec::new();
        for probe in [local - 86_400, local, local + 86_400] {
            let k = z.at(probe - z.at(probe).offset);
            if !near.contains(&k) {
                near.push(k);
            }
        }
        let valid: Vec<&Kind> = near
            .iter()
            .filter(|k| z.at(local - k.offset).offset == k.offset)
            .collect();
        let want = |w: bool| {
            valid
                .iter()
                .find(|k| k.isdst == w)
                .map(|k| local - k.offset)
        };
        let other = |w: bool, from: &Kind| {
            let k = z.nearest(local - from.offset, w);
            match k {
                Some(k) => local - k.offset,
                None if w => local - from.offset - 3600,
                None => local - from.offset + 3600,
            }
        };
        match (valid.as_slice(), isdst) {
            ([], _) => {
                // A gap.
                let before = z.at(local - z.at(local).offset - 86_400);
                match isdst {
                    Some(w) if w != before.isdst => other(w, &before),
                    _ => local - before.offset,
                }
            }
            (_, Some(w)) => want(w).unwrap_or_else(|| other(w, valid[0])),
            (_, None) => want(false).unwrap_or(local - valid[0].offset),
        }
    })
}

impl Zone {
    /// The kind of time with `isdst` in force nearest to `t`, if the zone
    /// has one.
    fn nearest(&self, t: i64, isdst: bool) -> Option<Kind> {
        if let Some(r) = &self.rule
            && (self.transitions.is_empty() || t >= self.transitions.last()?.0)
        {
            match &r.dst {
                Some((d, ..)) if isdst => return Some(d.clone()),
                Some(_) => return Some(r.std.clone()),
                None if !isdst => return Some(r.std.clone()),
                None if self.transitions.is_empty() => return None,
                None => {}
            }
        }
        let i = self.transitions.partition_point(|&(at, _)| at <= t);
        let before = self.transitions[..i].iter().rev();
        let after = self.transitions[i..].iter();
        let pick = |it: &mut dyn Iterator<Item = &(i64, usize)>| {
            it.map(|&(at, k)| (at, &self.kinds[k]))
                .find(|(_, k)| k.isdst == isdst)
        };
        match (pick(&mut before.clone()), pick(&mut after.clone())) {
            (Some((a, ka)), Some((b, kb))) => Some(if t - a <= b - t { ka } else { kb }.clone()),
            (Some((_, k)), None) | (None, Some((_, k))) => Some(k.clone()),
            (None, None) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_posix_rules() {
        let r = parse_rule(b"EST5EDT,M3.2.0,M11.1.0").unwrap();
        assert_eq!(r.std.offset, -5 * 3600);
        let (dst, ..) = r.dst.clone().unwrap();
        assert_eq!((dst.offset, dst.abbr.as_str()), (-4 * 3600, "EDT"));
        // 2021-03-14 07:00 UTC is 03:00 EDT, a second before is 01:59:59 EST.
        assert!(r.at(1_615_705_200).isdst);
        assert!(!r.at(1_615_705_199).isdst);
        let r = parse_rule(b"<+0330>-3:30").unwrap();
        assert_eq!((r.std.offset, r.std.abbr.as_str()), (12_600, "+0330"));
        let r = parse_rule(b"AEST-10AEDT,M10.1.0,M4.1.0/3").unwrap();
        assert!(r.at(1_609_459_200).isdst); // 2021-01-01, southern summer
        assert!(!r.at(1_625_097_600).isdst); // 2021-07-01
        assert!(parse_rule(b"X5").is_none());
    }
}
