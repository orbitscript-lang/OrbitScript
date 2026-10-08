//! The `os` library ([design](https://github.com/tymlipari/OrbitScript/blob/main/docs/design/chunks-io-os-cli.md#os)).

use std::io::Write;

use orbit_vm::orbit_core::table as tbl;
use orbit_vm::orbit_core::{Gc, Table, Value};
use orbit_vm::{CallOutcome, Event, LuaError, NativeCtx, NativeFn, NativeReturn};

use crate::sys::{exit_status, file_error, shell};
use crate::time::{self, Tm};
use crate::tz;

type R = Result<NativeReturn, LuaError>;

pub(crate) const FUNCTIONS: &[(&str, NativeFn)] = &[
    ("os.clock", clock),
    ("os.date", date),
    ("os.difftime", difftime),
    ("os.execute", execute),
    ("os.exit", exit),
    ("os.getenv", getenv),
    ("os.remove", remove),
    ("os.rename", rename),
    ("os.setlocale", setlocale),
    ("os.time", time_),
    ("os.tmpname", tmpname),
];

fn clock(ctx: &mut NativeCtx<'_>) -> R {
    // Wall time since the VM started, not CPU time: std has no CPU clock and
    // ADR-7 adds no dependency for one (tests/lua-5.5/DEVIATIONS.md).
    let t = ctx.rt.start.elapsed().as_secs_f64();
    ctx.ret1(Value::Float(t))
}

fn getenv(ctx: &mut NativeCtx<'_>) -> R {
    let s = ctx.check_str(0)?;
    let name = crate::os_path(ctx.bytes(s));
    match std::env::var_os(name) {
        Some(v) => {
            let v = ctx.new_str(&crate::os_bytes(&v));
            ctx.ret1(v)
        }
        None => ctx.ret1(Value::Nil),
    }
}

fn remove(ctx: &mut NativeCtx<'_>) -> R {
    let s = ctx.check_str(0)?;
    let name = ctx.bytes(s).to_vec();
    let path = crate::os_path(&name);
    // C's `remove` deletes files and empty directories.
    let r = match std::fs::symlink_metadata(&path) {
        Ok(m) if m.is_dir() => std::fs::remove_dir(&path),
        _ => std::fs::remove_file(&path),
    };
    match r {
        Ok(()) => ctx.ret1(Value::Bool(true)),
        Err(e) => file_error(ctx, &e, Some(&name)),
    }
}

fn rename(ctx: &mut NativeCtx<'_>) -> R {
    let from = ctx.check_str(0)?;
    let to = ctx.check_str(1)?;
    let (from, to) = (
        crate::os_path(ctx.bytes(from)),
        crate::os_path(ctx.bytes(to)),
    );
    match std::fs::rename(from, to) {
        Ok(()) => ctx.ret1(Value::Bool(true)),
        // lua5.5 names no file in this message.
        Err(e) => file_error(ctx, &e, None),
    }
}

fn tmpname(ctx: &mut NativeCtx<'_>) -> R {
    match crate::temp_file() {
        Ok((path, _file)) => {
            let v = ctx.new_str(&crate::os_bytes(path.as_os_str()));
            ctx.ret1(v)
        }
        Err(_) => Err(ctx.error("unable to generate a unique filename")),
    }
}

/// Flush the output `print` and `io.write` share, so a child process or
/// the exit doesn't overtake it.
fn flush_output(ctx: &mut NativeCtx<'_>) {
    let _ = ctx.rt.out.flush();
}

fn execute(ctx: &mut NativeCtx<'_>) -> R {
    if ctx.arg(0).is_nil() {
        // Is a shell available?
        let has_shell = cfg!(windows) || std::path::Path::new("/bin/sh").exists();
        return ctx.ret1(Value::Bool(has_shell));
    }
    let s = ctx.check_str(0)?;
    let cmd = crate::os_path(ctx.bytes(s));
    flush_output(ctx);
    match shell(cmd.as_os_str()).status() {
        Ok(status) => exit_status(ctx, status),
        Err(e) => file_error(ctx, &e, None),
    }
}

fn exit(ctx: &mut NativeCtx<'_>) -> R {
    let code = match ctx.arg(0) {
        Value::Nil | Value::Bool(true) => 0,
        Value::Bool(false) => 1,
        _ => ctx.check_int(0)?,
    };
    if !ctx.arg(1).is_falsy() {
        // Close the VM first, as Lua closes the state: the main thread's
        // variables still to close, newest first, then every pending
        // finalizer, even when this is called from one. The window is
        // [code, failed, error, count, values to close...].
        let pending = ctx.main_pending_closes();
        ctx.truncate(0);
        ctx.push(Value::Int(code));
        ctx.push(Value::Bool(false));
        ctx.push(Value::Nil);
        ctx.push(Value::Int(pending.len() as i64));
        for v in pending {
            ctx.push(v);
        }
        return close_next(ctx);
    }
    exit_now(ctx, code)
}

/// Window slots of `os.exit`'s closing state.
const FAILED: usize = 1;
const ERROR: usize = 2;
const COUNT: usize = 3;

/// Call the newest variable's `__close` with no error, or, once a handler
/// has failed, with that error (the last error is dropped, as in lua5.5),
/// then the next; with none left, close the VM.
fn close_next(ctx: &mut NativeCtx<'_>) -> R {
    let Value::Int(n) = ctx.arg(COUNT) else {
        unreachable!("exit pushed the count")
    };
    if n == 0 {
        return orbit_vm::close_then(ctx, exit_closed);
    }
    let v = ctx.arg(COUNT + n as usize);
    ctx.set(COUNT, Value::Int(n - 1));
    ctx.truncate(COUNT + n as usize);
    let h = ctx.metamethod(v, Event::Close);
    ctx.push(v);
    let failed = matches!(ctx.arg(FAILED), Value::Bool(true));
    if failed {
        let e = ctx.arg(ERROR);
        ctx.push(e);
    }
    Ok(NativeReturn::CallThen {
        f: h,
        nargs: 1 + failed as usize,
        k: closed_one,
        protected: true,
    })
}

fn closed_one(ctx: &mut NativeCtx<'_>, out: CallOutcome) -> R {
    if let CallOutcome::Err(e) = out {
        ctx.set(FAILED, Value::Bool(true));
        ctx.set(ERROR, e);
    }
    let Value::Int(n) = ctx.arg(COUNT) else {
        unreachable!("exit pushed the count")
    };
    ctx.truncate(COUNT + 1 + n as usize);
    close_next(ctx)
}

fn exit_closed(ctx: &mut NativeCtx<'_>, _: CallOutcome) -> R {
    let Value::Int(code) = ctx.arg(0) else {
        unreachable!("exit pushed the code")
    };
    exit_now(ctx, code)
}

/// Everything that matters on the way out (buffered output, open files) is
/// flushed, closed or not, as the C library's `exit` does.
fn exit_now(ctx: &mut NativeCtx<'_>, code: i64) -> R {
    #[cfg(feature = "io")]
    crate::io::flush_all(&mut *ctx.rt.out);
    flush_output(ctx);
    std::process::exit(code as i32)
}

fn difftime(ctx: &mut NativeCtx<'_>) -> R {
    let t2 = ctx.check_int(0)?;
    let t1 = ctx.check_int(1)?;
    ctx.ret1(Value::Float(t2 as f64 - t1 as f64))
}

fn setlocale(ctx: &mut NativeCtx<'_>) -> R {
    const CATEGORIES: [&[u8]; 6] = [
        b"all",
        b"collate",
        b"ctype",
        b"monetary",
        b"numeric",
        b"time",
    ];
    if !ctx.arg(1).is_nil() {
        let c = ctx.check_str(1)?;
        let cat = ctx.bytes(c).to_vec();
        if !CATEGORIES.contains(&&cat[..]) {
            let msg = format!("invalid option '{}'", String::from_utf8_lossy(&cat));
            return Err(ctx.arg_error(1, msg));
        }
    }
    // Only the C locale exists: querying or asking for it (by any of its
    // names) gives "C", anything else fails.
    let ok = match ctx.arg(0) {
        Value::Nil => true,
        _ => {
            let s = ctx.check_str(0)?;
            matches!(ctx.bytes(s), b"" | b"C" | b"POSIX")
        }
    };
    if ok {
        let c = ctx.new_str(b"C");
        ctx.ret1(c)
    } else {
        ctx.ret1(Value::Nil)
    }
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

fn date(ctx: &mut NativeCtx<'_>) -> R {
    let fmt = if ctx.arg(0).is_nil() {
        b"%c".to_vec()
    } else {
        let s = ctx.check_str(0)?;
        ctx.bytes(s).to_vec()
    };
    let t = if ctx.arg(1).is_nil() {
        now()
    } else {
        ctx.check_int(1)?
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
        return Err(ctx.error("date result cannot be represented in this installation"));
    }
    if fmt == b"*t" {
        let t = ctx.new_table(Table::new());
        ctx.push(Value::Table(t));
        set_fields(ctx, t, &tm);
        return Ok(NativeReturn::Return(1));
    }
    match time::strftime(fmt, &tm) {
        Ok(out) => {
            let s = ctx.new_str_owned(out);
            ctx.ret1(s)
        }
        Err(bad) => {
            let msg = format!(
                "invalid conversion specifier '{}'",
                String::from_utf8_lossy(&bad)
            );
            Err(ctx.arg_error(0, msg))
        }
    }
}

fn set_field(ctx: &mut NativeCtx<'_>, t: Gc<Table>, name: &str, v: Value) {
    let k = ctx.new_str(name.as_bytes());
    tbl::set(ctx.heap, t, k, v).expect("string keys are valid");
}

/// Fill a date table (`os.date("*t")`, and `os.time` normalizing its
/// argument).
fn set_fields(ctx: &mut NativeCtx<'_>, t: Gc<Table>, tm: &Tm) {
    set_field(ctx, t, "year", Value::Int(tm.year));
    set_field(ctx, t, "month", Value::Int(tm.month));
    set_field(ctx, t, "day", Value::Int(tm.day));
    set_field(ctx, t, "hour", Value::Int(tm.hour));
    set_field(ctx, t, "min", Value::Int(tm.min));
    set_field(ctx, t, "sec", Value::Int(tm.sec));
    set_field(ctx, t, "yday", Value::Int(tm.yday));
    set_field(ctx, t, "wday", Value::Int(tm.wday + 1));
    set_field(ctx, t, "isdst", Value::Bool(tm.isdst));
}

/// A date-table field as an integer: `default` when absent (an error when
/// there is none), and it must fit a C `int` after subtracting `delta`,
/// as `struct tm` stores it.
fn get_field(
    ctx: &mut NativeCtx<'_>,
    t: Gc<Table>,
    name: &str,
    default: Option<i64>,
    delta: i64,
) -> Result<i64, LuaError> {
    let k = ctx.new_str(name.as_bytes());
    let v = tbl::get(ctx.heap, t, k);
    let n = match v {
        Value::Nil => match default {
            Some(d) => return Ok(d),
            None => return Err(ctx.error(format!("field '{name}' missing in date table"))),
        },
        Value::Int(n) => Some(n),
        Value::Float(f) => orbit_vm::orbit_core::number::float_to_int(f),
        Value::Str(s) => match orbit_vm::orbit_core::number::str_to_number(ctx.bytes(s)) {
            Some(Value::Int(n)) => Some(n),
            Some(Value::Float(f)) => orbit_vm::orbit_core::number::float_to_int(f),
            _ => None,
        },
        _ => None,
    };
    let Some(n) = n else {
        return Err(ctx.error(format!("field '{name}' is not an integer")));
    };
    if n.checked_sub(delta)
        .and_then(|d| i32::try_from(d).ok())
        .is_none()
    {
        return Err(ctx.error(format!("field '{name}' is out-of-bound")));
    }
    Ok(n)
}

fn time_(ctx: &mut NativeCtx<'_>) -> R {
    if ctx.arg(0).is_nil() {
        return ctx.ret1(Value::Int(now()));
    }
    let t = ctx.check_table(0)?;
    let year = get_field(ctx, t, "year", None, 1900)?;
    let month = get_field(ctx, t, "month", None, 1)?;
    let day = get_field(ctx, t, "day", None, 0)?;
    let hour = get_field(ctx, t, "hour", Some(12), 0)?;
    let min = get_field(ctx, t, "min", Some(0), 0)?;
    let sec = get_field(ctx, t, "sec", Some(0), 0)?;
    let k = ctx.new_str(b"isdst");
    let isdst = match tbl::get(ctx.heap, t, k) {
        Value::Nil => None,
        v => Some(!v.is_falsy()),
    };
    let local = time::local_seconds(year, month, day, hour, min, sec);
    let utc = tz::from_local(local, isdst);
    let l = tz::at(utc);
    let tm = time::breakdown(utc + l.offset, l.offset, l.isdst, &l.zone);
    if !year_fits(tm.year) {
        return Err(ctx.error("time result cannot be represented in this installation"));
    }
    set_fields(ctx, t, &tm);
    ctx.ret1(Value::Int(utc))
}
