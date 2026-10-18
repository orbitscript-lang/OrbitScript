//! The `os` library ([design](https://github.com/tymlipari/OrbitScript/blob/main/docs/design/chunks-io-os-cli.md#os)).

use std::time::Instant;

use orbit::{Call, Error, Ret, Scope, Table, Value};

use crate::sys::{exit_status, file_error, flush_stdout, os_bytes, os_path, shell};
use crate::time::{self, Tm};
use crate::tz;

fn clock<'s>(c: &'s Call<'s>, start: Instant) -> Result<Ret, Error> {
    // Wall time since the library opened, not CPU time: std has no CPU
    // clock and ADR-7 adds no dependency for one
    // (tests/lua-5.5/DEVIATIONS.md).
    c.ret(start.elapsed().as_secs_f64())
}

fn getenv<'s>(c: &'s Call<'s>) -> Result<Ret, Error> {
    let name: Vec<u8> = c.check(0)?;
    match std::env::var_os(os_path(&name)) {
        Some(v) => c.ret(&os_bytes(&v)[..]),
        None => c.ret(Value::Nil),
    }
}

fn remove<'s>(c: &'s Call<'s>) -> Result<Ret, Error> {
    let name: Vec<u8> = c.check(0)?;
    let path = os_path(&name);
    // C's `remove` deletes files and empty directories.
    let r = match std::fs::symlink_metadata(&path) {
        Ok(m) if m.is_dir() => std::fs::remove_dir(&path),
        _ => std::fs::remove_file(&path),
    };
    match r {
        Ok(()) => c.ret(true),
        Err(e) => file_error(c, &e, Some(&name)),
    }
}

fn rename<'s>(c: &'s Call<'s>) -> Result<Ret, Error> {
    let from: Vec<u8> = c.check(0)?;
    let to: Vec<u8> = c.check(1)?;
    match std::fs::rename(os_path(&from), os_path(&to)) {
        Ok(()) => c.ret(true),
        // lua5.5 names no file in this message.
        Err(e) => file_error(c, &e, None),
    }
}

fn tmpname<'s>(c: &'s Call<'s>) -> Result<Ret, Error> {
    match crate::sys::temp_file() {
        Ok((path, _)) => c.ret(&os_bytes(path.as_os_str())[..]),
        Err(_) => Err(c.error("unable to generate a unique filename")),
    }
}

fn execute<'s>(c: &'s Call<'s>) -> Result<Ret, Error> {
    if c.arg(0).is_nil() {
        // Is a shell available?
        let has_shell = cfg!(windows) || std::path::Path::new("/bin/sh").exists();
        return c.ret(has_shell);
    }
    let cmd: Vec<u8> = c.check(0)?;
    flush_stdout(c);
    match shell(os_path(&cmd).as_os_str()).status() {
        Ok(status) => exit_status(c, status),
        Err(e) => file_error(c, &e, None),
    }
}

fn exit<'s>(c: &'s Call<'s>) -> Result<Ret, Error> {
    let code = match c.arg(0) {
        Value::Nil | Value::Bool(true) => 0,
        Value::Bool(false) => 1,
        _ => c.check::<i64>(0)? as i32,
    };
    c.exit(code, c.arg(1).truthy())
}

fn difftime<'s>(c: &'s Call<'s>) -> Result<Ret, Error> {
    let t2: i64 = c.check(0)?;
    let t1: i64 = c.check(1)?;
    c.ret(t2 as f64 - t1 as f64)
}

fn setlocale<'s>(c: &'s Call<'s>) -> Result<Ret, Error> {
    const CATEGORIES: [&[u8]; 6] = [
        b"all",
        b"collate",
        b"ctype",
        b"monetary",
        b"numeric",
        b"time",
    ];
    if !c.arg(1).is_nil() {
        let cat: Vec<u8> = c.check(1)?;
        if !CATEGORIES.contains(&&cat[..]) {
            let msg = format!("invalid option '{}'", String::from_utf8_lossy(&cat));
            return Err(c.arg_error(1, msg));
        }
    }
    // Only the C locale exists: querying or asking for it (by any of its
    // names) gives "C", anything else fails.
    let ok = match c.arg(0) {
        Value::Nil => true,
        _ => matches!(&c.check::<Vec<u8>>(0)?[..], b"" | b"C" | b"POSIX"),
    };
    if ok { c.ret("C") } else { c.ret(Value::Nil) }
}

/// The current time in seconds since the epoch.
fn now() -> i64 {
    match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        Ok(d) => d.as_secs() as i64,
        Err(e) => -(e.duration().as_secs() as i64),
    }
}

/// Does a broken-down year fit C's `int tm_year` (years since 1900)?
fn year_fits(year: i64) -> bool {
    i32::try_from(year - 1900).is_ok()
}

fn date<'s>(c: &'s Call<'s>) -> Result<Ret, Error> {
    let fmt = c
        .opt::<Option<Vec<u8>>>(0, None)?
        .unwrap_or_else(|| b"%c".to_vec());
    let t = if c.arg(1).is_nil() {
        now()
    } else {
        c.check::<i64>(1)?
    };
    let (utc, fmt) = match fmt.strip_prefix(b"!") {
        Some(rest) => (true, rest),
        None => (false, &fmt[..]),
    };
    let tm = if utc {
        time::breakdown(t, 0, false, "GMT")
    } else {
        let l = tz::at(t);
        time::breakdown(t + l.offset, l.offset, l.isdst, &l.zone)
    };
    if !year_fits(tm.year) {
        return Err(c.error("date result cannot be represented in this installation"));
    }
    if fmt == b"*t" {
        let t = c.create_table();
        set_fields(t, &tm)?;
        return c.ret(t);
    }
    match time::strftime(fmt, &tm) {
        Ok(out) => c.ret(&out[..]),
        Err(bad) => {
            let msg = format!(
                "invalid conversion specifier '{}'",
                String::from_utf8_lossy(&bad)
            );
            Err(c.arg_error(0, msg))
        }
    }
}

/// Fill a date table (`os.date("*t")`, and `os.time` normalizing its
/// argument).
fn set_fields(t: Table<'_>, tm: &Tm) -> Result<(), Error> {
    t.raw_set("year", tm.year)?;
    t.raw_set("month", tm.month)?;
    t.raw_set("day", tm.day)?;
    t.raw_set("hour", tm.hour)?;
    t.raw_set("min", tm.min)?;
    t.raw_set("sec", tm.sec)?;
    t.raw_set("yday", tm.yday)?;
    t.raw_set("wday", tm.wday + 1)?;
    t.raw_set("isdst", tm.isdst)
}

/// A float with an exact integer value, as an integer.
fn float_to_int(f: f64) -> Option<i64> {
    // 2^63 as a float: the first value past i64's range.
    const LIMIT: f64 = 9_223_372_036_854_775_808.0;
    (f.fract() == 0.0 && (-LIMIT..LIMIT).contains(&f)).then_some(f as i64)
}

/// A date-table field as an integer: `default` when absent (an error when
/// there is none), and it must fit a C `int` after subtracting `delta`,
/// as `struct tm` stores it.
fn get_field<'s>(
    c: &'s Call<'s>,
    t: Table<'s>,
    name: &str,
    default: Option<i64>,
    delta: i64,
) -> Result<i64, Error> {
    let n = match t.raw_get::<Value>(name)? {
        Value::Nil => match default {
            Some(d) => return Ok(d),
            None => return Err(c.error(format!("field '{name}' missing in date table"))),
        },
        Value::Int(n) => Some(n),
        Value::Float(f) => float_to_int(f),
        Value::Str(s) => match c.parse_number(&s.to_bytes()) {
            Some(Value::Int(n)) => Some(n),
            Some(Value::Float(f)) => float_to_int(f),
            _ => None,
        },
        _ => None,
    };
    let Some(n) = n else {
        return Err(c.error(format!("field '{name}' is not an integer")));
    };
    if n.checked_sub(delta)
        .and_then(|d| i32::try_from(d).ok())
        .is_none()
    {
        return Err(c.error(format!("field '{name}' is out-of-bound")));
    }
    Ok(n)
}

fn time_<'s>(c: &'s Call<'s>) -> Result<Ret, Error> {
    if c.arg(0).is_nil() {
        return c.ret(now());
    }
    let t: Table = c.check(0)?;
    let year = get_field(c, t, "year", None, 1900)?;
    let month = get_field(c, t, "month", None, 1)?;
    let day = get_field(c, t, "day", None, 0)?;
    let hour = get_field(c, t, "hour", Some(12), 0)?;
    let min = get_field(c, t, "min", Some(0), 0)?;
    let sec = get_field(c, t, "sec", Some(0), 0)?;
    let isdst = match t.raw_get::<Value>("isdst")? {
        Value::Nil => None,
        v => Some(v.truthy()),
    };
    let local = time::local_seconds(year, month, day, hour, min, sec);
    let utc = tz::from_local(local, isdst);
    let l = tz::at(utc);
    let tm = time::breakdown(utc + l.offset, l.offset, l.isdst, &l.zone);
    if !year_fits(tm.year) {
        return Err(c.error("time result cannot be represented in this installation"));
    }
    set_fields(t, &tm)?;
    c.ret(utc)
}

/// Open the library. Returns the `os` table.
pub fn open<'s>(s: &'s Scope<'s>) -> Result<Table<'s>, Error> {
    let os = s.create_table();
    let start = Instant::now();
    os.raw_set("clock", s.create_function(move |c| clock(c, start)))?;
    os.raw_set("date", s.create_function(date))?;
    os.raw_set("difftime", s.create_function(difftime))?;
    os.raw_set("execute", s.create_function(execute))?;
    os.raw_set("exit", s.create_function(exit))?;
    os.raw_set("getenv", s.create_function(getenv))?;
    os.raw_set("remove", s.create_function(remove))?;
    os.raw_set("rename", s.create_function(rename))?;
    os.raw_set("setlocale", s.create_function(setlocale))?;
    os.raw_set("time", s.create_function(time_))?;
    os.raw_set("tmpname", s.create_function(tmpname))?;
    Ok(os)
}
