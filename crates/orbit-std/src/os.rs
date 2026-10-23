//! The `os` library ([design](https://github.com/tymlipari/OrbitScript/blob/main/docs/design/chunks-io-os-cli.md#os)).

use std::time::Instant;

use orbit::stack::{Arg, Either, Fail, Return, Stack, StackResult, TableArg, Type};
use orbit::{Call, Error, OrbitStr, OrbitString, Ret, Scope, Table, native};

use crate::sys::{ExitResults, FileFailure, exit_results, file_failure, os_bytes, os_path, shell};
use crate::time::{self, Tm};
use crate::tz;

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

#[native(name = "os.setlocale")]
fn setlocale(
    locale: Option<OrbitStr<'_>>,
    category: Option<OrbitStr<'_>>,
) -> Result<Option<&'static str>, Fail> {
    const CATEGORIES: [&[u8]; 6] = [
        b"all",
        b"collate",
        b"ctype",
        b"monetary",
        b"numeric",
        b"time",
    ];
    if let Some(cat) = category {
        if !CATEGORIES.contains(&cat.as_bytes()) {
            return Err(Fail::arg(
                2,
                OrbitString::concat(&[b"invalid option '", &cat, b"'"]),
            ));
        }
    }
    // Only the C locale exists: querying or asking for it (by any of its
    // names) gives "C", anything else fails.
    let ok = matches!(locale.as_deref(), None | Some(b"" | b"C" | b"POSIX"));
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
    let tm = if utc {
        time::breakdown(t, 0, false, "GMT")
    } else {
        let l = tz::at(t);
        time::breakdown(t + l.offset, l.offset, l.isdst, &l.zone)
    };
    if !year_fits(tm.year) {
        return Err("date result cannot be represented in this installation".into());
    }
    if fmt == b"*t" {
        s.create_table(0, 9);
        set_fields(s, -1, &tm)?;
        return Ok(Either::Left(Return::values(1)));
    }
    match time::strftime(fmt, &tm) {
        Ok(out) => Ok(Either::Right(out.into())),
        Err(bad) => Err(Fail::arg(
            1,
            OrbitString::concat(&[b"invalid conversion specifier '", &bad, b"'"]),
        )),
    }
}

/// Fill the date table at `t` (`os.date("*t")`, and `os.time` normalizing
/// its argument).
fn set_fields(s: &mut Stack<'_>, t: i32, tm: &Tm) -> StackResult<()> {
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

/// A field of the date table at `t` as an integer: `default` when absent
/// (an error when there is none), and it must fit a C `int` after
/// subtracting `delta`, as `struct tm` stores it.
fn get_field(
    s: &mut Stack<'_>,
    t: i32,
    name: &str,
    default: Option<i64>,
    delta: i64,
) -> Result<i64, Fail> {
    let top = s.top();
    let n = match s.raw_get_field(t, name.as_bytes())? {
        Type::Nil => None,
        // Integral floats and numeric strings convert, as `tonumber` reads them.
        Type::Number | Type::String => Some(s.to_integer(-1)),
        _ => Some(None),
    };
    s.set_top(top)?;
    match n {
        None => default.ok_or_else(|| format!("field '{name}' missing in date table").into()),
        Some(None) => Err(format!("field '{name}' is not an integer").into()),
        Some(Some(n)) => match n.checked_sub(delta).map(i32::try_from) {
            Some(Ok(_)) => Ok(n),
            _ => Err(format!("field '{name}' is out-of-bound").into()),
        },
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
    let local = time::local_seconds(year, month, day, hour, min, sec);
    let utc = tz::from_local(local, isdst);
    let l = tz::at(utc);
    let tm = time::breakdown(utc + l.offset, l.offset, l.isdst, &l.zone);
    if !year_fits(tm.year) {
        return Err("time result cannot be represented in this installation".into());
    }
    set_fields(s, t, &tm)?;
    Ok(utc)
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
