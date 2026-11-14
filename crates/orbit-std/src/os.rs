//! The `os` library ([design](https://github.com/orbitscript-lang/OrbitScript/blob/main/docs/design/chunks-io-os-cli.md#os)).

use std::ffi::c_int;
use std::time::Instant;

use orbit::stack::{Arg, Either, Fail, Return, Stack, StackResult, TableArg, Type};
use orbit::{
    Call, Error, LocaleCategory, LocaleScope, OrbitStr, OrbitString, Ret, Scope, Table, TimeParts,
    native,
};

use crate::ctime;
use crate::sys::{ExitResults, FileFailure, exit_results, file_failure, os_bytes, os_path, shell};
use crate::time;

fn clock<'s>(c: &'s Call<'s>, start: Instant) -> Result<Ret, Error> {
    // Wall time since the library opened, not CPU time: std has no CPU
    // clock and ADR-7 adds no dependency for one
    // (tests/lua-5.5/DEVIATIONS.md).
    c.ret(start.elapsed().as_secs_f64())
}

#[native(name = "os.getenv")]
fn getenv(name: OrbitStr<'_>) -> Option<OrbitString> {
    std::env::var_os(os_path(&name)).map(|v| os_bytes(&v).into())
}

#[native(name = "os.remove")]
fn remove(name: OrbitStr<'_>) -> Either<bool, FileFailure> {
    let path = os_path(&name);
    // C's `remove` deletes files and empty directories.
    let r = match std::fs::symlink_metadata(&path) {
        Ok(m) if m.is_dir() => std::fs::remove_dir(&path),
        _ => std::fs::remove_file(&path),
    };
    match r {
        Ok(()) => Either::Left(true),
        Err(e) => Either::Right(file_failure(&e, Some(&name))),
    }
}

#[native(name = "os.rename")]
fn rename(from: OrbitStr<'_>, to: OrbitStr<'_>) -> Either<bool, FileFailure> {
    match std::fs::rename(os_path(&from), os_path(&to)) {
        Ok(()) => Either::Left(true),
        // lua5.5 names no file in this message.
        Err(e) => Either::Right(file_failure(&e, None)),
    }
}

#[native(name = "os.tmpname")]
fn tmpname() -> Result<OrbitString, &'static str> {
    match crate::sys::temp_file() {
        Ok((path, _)) => Ok(os_bytes(path.as_os_str()).into()),
        Err(_) => Err("unable to generate a unique filename"),
    }
}

/// `os.execute(cmd)`: the shell's exit status, or `nil, message, code`
/// when it can't run; without `cmd`, whether there is a shell.
#[native(name = "os.execute")]
fn execute(
    s: &mut Stack<'_>,
    cmd: Option<OrbitString>,
) -> Either<bool, Either<ExitResults, FileFailure>> {
    let Some(cmd) = cmd else {
        return Either::Left(cfg!(windows) || std::path::Path::new("/bin/sh").exists());
    };
    // The child's output mustn't overtake ours.
    s.with_stdout(|out| {
        let _ = out.flush();
    });
    Either::Right(match shell(os_path(&cmd).as_os_str()).status() {
        Ok(status) => Either::Left(exit_results(status)),
        Err(e) => Either::Right(file_failure(&e, None)),
    })
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

/// `os.setlocale`: the isolate's locale, or the process's when the host
/// asked for that (ADR-13).
#[native(name = "os.setlocale")]
fn setlocale(
    s: &mut Stack<'_>,
    locale: Option<OrbitString>,
    category: Option<OrbitString>,
    scope: Option<OrbitString>,
) -> Result<Option<OrbitString>, Fail> {
    let invalid = |arg, name: &OrbitString| {
        Fail::arg(arg, OrbitString::concat(&[b"invalid option '", name, b"'"]))
    };
    let category = match category {
        None => LocaleCategory::All,
        Some(cat) => LocaleCategory::from_name(&cat).ok_or_else(|| invalid(2, &cat))?,
    };
    // OrbitScript's addition (ADR-13): lua5.5 ignores a third argument.
    let scope = match scope {
        None => LocaleScope::Default,
        Some(sc) => LocaleScope::from_name(&sc).ok_or_else(|| invalid(3, &sc))?,
    };
    Ok(s.set_locale(category, locale.as_ref().map(OrbitStr::from), scope))
}

/// The current time in seconds since the epoch.
fn now() -> i64 {
    match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        Ok(d) => d.as_secs() as i64,
        Err(e) => -(e.duration().as_secs() as i64),
    }
}

#[native(name = "os.date")]
fn date(
    s: &mut Stack<'_>,
    fmt: Option<OrbitString>,
    t: Option<i64>,
) -> Result<Either<Return, OrbitString>, Fail> {
    let fmt = fmt.unwrap_or_else(|| "%c".into());
    let t = t.unwrap_or_else(now);
    let (utc, fmt) = match fmt.strip_prefix(b"!") {
        Some(rest) => (true, rest),
        None => (false, &fmt[..]),
    };
    let time = if utc {
        ctime::utc_time(t)
    } else {
        ctime::local_time(t)
    };
    let Some(time) = time else {
        return Err("date result cannot be represented in this installation".into());
    };
    let parts = time.parts();
    if fmt == b"*t" {
        s.create_table(0, 9);
        set_fields(s, -1, &parts)?;
        return Ok(Either::Left(Return::values(1)));
    }
    // Each conversion on its own: the isolate's time locale formats those
    // that depend on it (ADR-13), the C library in the "C" locale the rest.
    let out = time::format(fmt, |spec, out| {
        let localized = if time::is_localized(spec) {
            s.locale_time_text(spec, &parts)
        } else {
            None
        };
        match localized {
            Some(text) => out.extend_from_slice(text.as_bytes()),
            None => out.extend_from_slice(&time.format(spec)),
        }
    });
    match out {
        Ok(out) => Ok(Either::Right(out.into())),
        Err(bad) => Err(Fail::arg(
            1,
            OrbitString::concat(&[b"invalid conversion specifier '", &bad, b"'"]),
        )),
    }
}

/// Fill the date table at `t` (`os.date("*t")`, and `os.time` normalizing
/// its argument).
fn set_fields(s: &mut Stack<'_>, t: i32, tm: &TimeParts) -> StackResult<()> {
    s.raw_put(t, b"year", tm.year)?;
    s.raw_put(t, b"month", tm.month)?;
    s.raw_put(t, b"day", tm.day)?;
    s.raw_put(t, b"hour", tm.hour)?;
    s.raw_put(t, b"min", tm.min)?;
    s.raw_put(t, b"sec", tm.sec)?;
    s.raw_put(t, b"yday", tm.yday)?;
    s.raw_put(t, b"wday", tm.wday + 1)?;
    s.raw_put(t, b"isdst", tm.isdst)
}

/// A field of the date table at `t` as a C `int`, as `struct tm` stores
/// it: the integer less `delta`, which must fit; `default` when absent
/// (an error when there is none).
fn get_field(
    s: &mut Stack<'_>,
    t: i32,
    name: &str,
    default: Option<i64>,
    delta: i64,
) -> Result<c_int, Fail> {
    let top = s.top();
    let n = match s.raw_get_field(t, name.as_bytes())? {
        Type::Nil => None,
        // Integral floats and numeric strings convert, as `tonumber` reads them.
        Type::Number | Type::String => Some(s.to_integer(-1)),
        _ => Some(None),
    };
    s.set_top(top)?;
    let n = match n {
        None => default.ok_or_else(|| format!("field '{name}' missing in date table"))?,
        Some(None) => return Err(format!("field '{name}' is not an integer").into()),
        Some(Some(n)) => n,
    };
    match n.checked_sub(delta).map(c_int::try_from) {
        Some(Ok(n)) => Ok(n),
        _ => Err(format!("field '{name}' is out-of-bound").into()),
    }
}

#[native(name = "os.time")]
fn time_(s: &mut Stack<'_>, t: Option<TableArg>) -> Result<i64, Fail> {
    let Some(TableArg(t)) = t else {
        return Ok(now());
    };
    let year = get_field(s, t, "year", None, 1900)?;
    let month = get_field(s, t, "month", None, 1)?;
    let day = get_field(s, t, "day", None, 0)?;
    let hour = get_field(s, t, "hour", Some(12), 0)?;
    let min = get_field(s, t, "min", Some(0), 0)?;
    let sec = get_field(s, t, "sec", Some(0), 0)?;
    let isdst = match s.raw_get_field(t, b"isdst")? {
        Type::Nil => None,
        _ => Some(s.to_boolean(-1)),
    };
    s.set_top(t)?;
    let (secs, time) = ctime::make_local_time(year, month, day, hour, min, sec, isdst);
    // The table gets the fields as `mktime` left them, even when it failed.
    set_fields(s, t, &time.parts())?;
    secs.ok_or_else(|| "time result cannot be represented in this installation".into())
}

/// Open the library. Returns the `os` table.
pub fn open<'s>(s: &'s Scope<'s>) -> Result<Table<'s>, Error> {
    let os = s.create_table();
    let start = Instant::now();
    os.raw_set("clock", s.create_function(move |c| clock(c, start)))?;
    os.raw_set("date", s.create_native::<date>())?;
    os.raw_set("difftime", s.create_native::<difftime>())?;
    os.raw_set("execute", s.create_native::<execute>())?;
    os.raw_set("exit", s.create_native::<exit>())?;
    os.raw_set("getenv", s.create_native::<getenv>())?;
    os.raw_set("remove", s.create_native::<remove>())?;
    os.raw_set("rename", s.create_native::<rename>())?;
    os.raw_set("setlocale", s.create_native::<setlocale>())?;
    os.raw_set("time", s.create_native::<time_>())?;
    os.raw_set("tmpname", s.create_native::<tmpname>())?;
    Ok(os)
}
