//! The `os` library ([design](https://github.com/tymlipari/OrbitScript/blob/main/docs/design/chunks-io-os-cli.md#os)).

use std::time::Instant;

use orbit::stack::{Arg, Either, Fail, Nil, Return, Stack, StackResult, Type};
use orbit::{Call, Error, Ret, Scope, Table, Value, native};

use crate::sys::{exit_status, file_error, flush_stdout, os_bytes, os_error_text, os_path, shell};
use crate::time::{self, Tm};
use crate::tz;

fn clock<'s>(c: &'s Call<'s>, start: Instant) -> Result<Ret, Error> {
    // Wall time since the library opened, not CPU time: std has no CPU
    // clock and ADR-7 adds no dependency for one
    // (tests/lua-5.5/DEVIATIONS.md).
    c.ret(start.elapsed().as_secs_f64())
}

#[native(name = "os.getenv")]
fn getenv(name: &[u8]) -> Option<Vec<u8>> {
    std::env::var_os(os_path(name)).map(|v| os_bytes(&v))
}

/// What a failed file operation returns: `nil, message, code`
/// (`luaL_fileresult`).
type Failed = (Nil, Vec<u8>, i64);

fn failed(e: &std::io::Error, name: Option<&[u8]>) -> Failed {
    let mut msg = Vec::new();
    if let Some(n) = name {
        msg.extend_from_slice(n);
        msg.extend_from_slice(b": ");
    }
    msg.extend_from_slice(os_error_text(e).as_bytes());
    (Nil, msg, i64::from(e.raw_os_error().unwrap_or(0)))
}

#[native(name = "os.remove")]
fn remove(name: &[u8]) -> Either<bool, Failed> {
    let path = os_path(name);
    // C's `remove` deletes files and empty directories.
    let r = match std::fs::symlink_metadata(&path) {
        Ok(m) if m.is_dir() => std::fs::remove_dir(&path),
        _ => std::fs::remove_file(&path),
    };
    match r {
        Ok(()) => Either::Left(true),
        Err(e) => Either::Right(failed(&e, Some(name))),
    }
}

#[native(name = "os.rename")]
fn rename(from: &[u8], to: &[u8]) -> Either<bool, Failed> {
    match std::fs::rename(os_path(from), os_path(to)) {
        Ok(()) => Either::Left(true),
        // lua5.5 names no file in this message.
        Err(e) => Either::Right(failed(&e, None)),
    }
}

#[native(name = "os.tmpname")]
fn tmpname() -> Result<Vec<u8>, &'static str> {
    match crate::sys::temp_file() {
        Ok((path, _)) => Ok(os_bytes(path.as_os_str())),
        Err(_) => Err("unable to generate a unique filename"),
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

/// `os.exit`'s status: `true` or absent is success, `false` failure.
struct ExitCode(i32);

impl Arg<'_> for ExitCode {
    type Checked = ExitCode;

    fn check(s: &mut Stack<'_>, i: i32) -> StackResult<ExitCode> {
        Ok(ExitCode(match s.type_at(i) {
            Type::None | Type::Nil => 0,
            Type::Boolean => i32::from(!s.to_boolean(i)),
            _ => s.check_integer(i)? as i32,
        }))
    }

    fn get(_: &Stack<'_>, _: i32, code: ExitCode) -> ExitCode {
        code
    }
}

#[native(name = "os.exit")]
fn exit(code: ExitCode, close: bool) -> Return {
    Return::exit(code.0, close)
}

#[native(name = "os.difftime")]
fn difftime(t2: i64, t1: i64) -> f64 {
    t2 as f64 - t1 as f64
}

#[native(name = "os.setlocale")]
fn setlocale(locale: Option<&[u8]>, category: Option<&[u8]>) -> Result<Option<&'static str>, Fail> {
    const CATEGORIES: [&[u8]; 6] = [
        b"all",
        b"collate",
        b"ctype",
        b"monetary",
        b"numeric",
        b"time",
    ];
    if let Some(cat) = category
        && !CATEGORIES.contains(&cat)
    {
        let msg = format!("invalid option '{}'", String::from_utf8_lossy(cat));
        return Err(Fail::arg(2, msg));
    }
    // Only the C locale exists: querying or asking for it (by any of its
    // names) gives "C", anything else fails.
    let ok = matches!(locale, None | Some(b"" | b"C" | b"POSIX"));
    Ok(ok.then_some("C"))
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
    os.raw_set("difftime", s.create_native::<difftime>())?;
    os.raw_set("execute", s.create_function(execute))?;
    os.raw_set("exit", s.create_native::<exit>())?;
    os.raw_set("getenv", s.create_native::<getenv>())?;
    os.raw_set("remove", s.create_native::<remove>())?;
    os.raw_set("rename", s.create_native::<rename>())?;
    os.raw_set("setlocale", s.create_native::<setlocale>())?;
    os.raw_set("time", s.create_function(time_))?;
    os.raw_set("tmpname", s.create_native::<tmpname>())?;
    Ok(os)
}
