//! The OS library (`loslib.c`).
//!
//! Dates are computed without the C library. Local time is taken to be UTC (the
//! runtime reads no time zone database yet), `os.date` formats like `strftime` in the
//! "C" locale, and `os.setlocale` knows only the "C" locale.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use orbit_core::Value;
use orbit_vm::number::{self, F2I};
use orbit_vm::{Action, LuaResult, Vm};

/// Opens the os library.
pub fn open_os(vm: &mut Vm) {
    vm.new_lib(
        "os",
        &[
            ("clock", os_clock),
            ("date", os_date),
            ("difftime", os_difftime),
            ("execute", os_execute),
            ("exit", os_exit),
            ("getenv", os_getenv),
            ("remove", os_remove),
            ("rename", os_rename),
            ("setlocale", os_setlocale),
            ("time", os_time),
            ("tmpname", os_tmpname),
        ],
    );
}

// ---- shared results (lauxlib) -----------------------------------------------

/// Pushes `fail, message, errno` for a failed operation (`luaL_fileresult`).
pub(crate) fn file_error(
    vm: &mut Vm,
    e: &std::io::Error,
    fname: Option<&[u8]>,
) -> LuaResult<Action> {
    let text = orbit_vm::baselib::io_error_text(e);
    let mut msg = Vec::new();
    if let Some(f) = fname {
        msg.extend_from_slice(f);
        msg.extend_from_slice(b": ");
    }
    msg.extend_from_slice(text.as_bytes());
    let m = vm.heap.str_value(msg);
    vm.check_stack(3)?;
    vm.push(Value::Nil);
    vm.push(m);
    vm.push(Value::Int(e.raw_os_error().unwrap_or(0) as i64));
    Ok(Action::Return(3))
}

/// Pushes `true` or the error triple (`luaL_fileresult`).
pub(crate) fn file_result(
    vm: &mut Vm,
    r: std::io::Result<()>,
    fname: Option<&[u8]>,
) -> LuaResult<Action> {
    match r {
        Ok(()) => {
            vm.push(Value::Bool(true));
            Ok(Action::Return(1))
        }
        Err(e) => file_error(vm, &e, fname),
    }
}

/// Pushes the result of running a command (`luaL_execresult`).
pub(crate) fn exec_result(
    vm: &mut Vm,
    r: std::io::Result<std::process::ExitStatus>,
) -> LuaResult<Action> {
    let status = match r {
        Ok(s) => s,
        Err(e) => return file_error(vm, &e, None),
    };
    let (what, code) = match status.code() {
        Some(c) => ("exit", c as i64),
        None => ("signal", signal_of(&status)),
    };
    let what_v = vm.heap.str_value(what);
    vm.check_stack(3)?;
    vm.push(if what == "exit" && code == 0 {
        Value::Bool(true)
    } else {
        Value::Nil
    });
    vm.push(what_v);
    vm.push(Value::Int(code));
    Ok(Action::Return(3))
}

#[cfg(unix)]
fn signal_of(s: &std::process::ExitStatus) -> i64 {
    use std::os::unix::process::ExitStatusExt;
    s.signal().unwrap_or(0) as i64
}

#[cfg(not(unix))]
fn signal_of(_: &std::process::ExitStatus) -> i64 {
    0
}

/// A file name from Lua bytes.
pub(crate) fn os_path(name: &[u8]) -> std::path::PathBuf {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        std::path::PathBuf::from(std::ffi::OsStr::from_bytes(name))
    }
    #[cfg(not(unix))]
    {
        std::path::PathBuf::from(String::from_utf8_lossy(name).into_owned())
    }
}

/// Runs `cmd` with the system shell.
pub(crate) fn shell(cmd: &[u8]) -> std::process::Command {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        let mut c = std::process::Command::new("/bin/sh");
        c.arg("-c").arg(std::ffi::OsStr::from_bytes(cmd));
        c
    }
    #[cfg(not(unix))]
    {
        let mut c = std::process::Command::new("cmd");
        c.arg("/C").arg(String::from_utf8_lossy(cmd).into_owned());
        c
    }
}

fn string_arg(vm: &mut Vm, arg: usize) -> LuaResult<Vec<u8>> {
    let s = vm.check_string(arg)?;
    Ok(vm.heap.string(s).as_bytes().to_vec())
}

// ---- processes and files ----------------------------------------------------

fn os_execute(vm: &mut Vm) -> LuaResult<Action> {
    let Some(cmd) = vm.opt_string(1)? else {
        let has_shell = if cfg!(unix) {
            std::path::Path::new("/bin/sh").exists()
        } else {
            true
        };
        vm.push(Value::Bool(has_shell));
        return Ok(Action::Return(1));
    };
    let cmd = vm.heap.string(cmd).as_bytes().to_vec();
    use std::io::Write;
    let _ = std::io::stdout().flush();
    let r = shell(&cmd).status();
    exec_result(vm, r)
}

fn os_remove(vm: &mut Vm) -> LuaResult<Action> {
    let name = string_arg(vm, 1)?;
    let path = os_path(&name);
    // Like C's remove(), this also removes empty directories.
    let r = match std::fs::symlink_metadata(&path) {
        Ok(m) if m.is_dir() => std::fs::remove_dir(&path),
        _ => std::fs::remove_file(&path),
    };
    file_result(vm, r, Some(&name))
}

fn os_rename(vm: &mut Vm) -> LuaResult<Action> {
    let from = string_arg(vm, 1)?;
    let to = string_arg(vm, 2)?;
    let r = std::fs::rename(os_path(&from), os_path(&to));
    file_result(vm, r, None)
}

/// A few pseudo-random bits for temporary file names.
fn unique_bits() -> u64 {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let t = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos() as u64);
    let mut x =
        t ^ (std::process::id() as u64).rotate_left(32) ^ n.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    // splitmix64 finalizer
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 31)
}

/// Creates a new empty file with a unique name like `mkstemp`, returning its name
/// and the open file.
pub(crate) fn make_temp() -> std::io::Result<(std::path::PathBuf, std::fs::File)> {
    const CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    let dir = if cfg!(unix) {
        std::path::PathBuf::from("/tmp")
    } else {
        std::env::temp_dir()
    };
    let mut last = None;
    for _ in 0..100 {
        let mut bits = unique_bits();
        let mut name = String::from("lua_");
        for _ in 0..6 {
            name.push(CHARS[(bits % CHARS.len() as u64) as usize] as char);
            bits /= CHARS.len() as u64;
        }
        let path = dir.join(name);
        match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(f) => return Ok((path, f)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => last = Some(e),
            Err(e) => return Err(e),
        }
    }
    Err(last.unwrap_or_else(|| std::io::Error::other("no unique name")))
}

fn os_tmpname(vm: &mut Vm) -> LuaResult<Action> {
    match make_temp() {
        Ok((path, _)) => {
            let v = vm.heap.str_value(path.to_string_lossy().as_bytes());
            vm.push(v);
            Ok(Action::Return(1))
        }
        Err(_) => Err(vm.error("unable to generate a unique filename")),
    }
}

fn os_getenv(vm: &mut Vm) -> LuaResult<Action> {
    let name = string_arg(vm, 1)?;
    let v = if name.is_empty() || name.contains(&b'=') || name.contains(&0) {
        None
    } else {
        std::env::var_os(os_path(&name).as_os_str())
    };
    let v = match v {
        Some(s) => {
            #[cfg(unix)]
            let bytes = {
                use std::os::unix::ffi::OsStrExt;
                s.as_bytes().to_vec()
            };
            #[cfg(not(unix))]
            let bytes = s.to_string_lossy().into_owned().into_bytes();
            vm.heap.str_value(bytes)
        }
        None => Value::Nil,
    };
    vm.push(v);
    Ok(Action::Return(1))
}

/// CPU time used by the process, in seconds (`clock()`).
fn cpu_seconds() -> f64 {
    // The first field of schedstat is the time on the CPU, in nanoseconds.
    if let Ok(s) = std::fs::read_to_string("/proc/self/schedstat")
        && let Some(ns) = s
            .split_whitespace()
            .next()
            .and_then(|f| f.parse::<u64>().ok())
    {
        return ns as f64 / 1e9;
    }
    static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    START
        .get_or_init(std::time::Instant::now)
        .elapsed()
        .as_secs_f64()
}

fn os_clock(vm: &mut Vm) -> LuaResult<Action> {
    vm.push(Value::Float(cpu_seconds()));
    Ok(Action::Return(1))
}

fn os_exit(vm: &mut Vm) -> LuaResult<Action> {
    let status = match vm.arg(1) {
        Value::Bool(b) => i32::from(!b),
        _ => vm.opt_integer(1, 0)? as i32,
    };
    if vm.arg(2).is_truthy() {
        vm.close_state();
    }
    use std::io::Write;
    let _ = std::io::stdout().flush();
    let _ = std::io::stderr().flush();
    std::process::exit(status)
}

fn os_setlocale(vm: &mut Vm) -> LuaResult<Action> {
    let l = match vm.opt_string(1)? {
        Some(s) => Some(vm.heap.string(s).as_bytes().to_vec()),
        None => None,
    };
    vm.check_option(
        2,
        Some("all"),
        &["all", "collate", "ctype", "monetary", "numeric", "time"],
    )?;
    let r = match l.as_deref() {
        None | Some(b"") | Some(b"C") | Some(b"POSIX") => vm.heap.str_value("C"),
        Some(_) => Value::Nil,
    };
    vm.push(r);
    Ok(Action::Return(1))
}

// ---- time and date -----------------------------------------------------------

/// Broken-down time, as `struct tm` (with the full year).
#[derive(Clone, Copy, Debug, PartialEq)]
struct Tm {
    year: i64,
    mon: i64,
    mday: i64,
    hour: i64,
    min: i64,
    sec: i64,
    yday: i64,
    wday: i64,
    utc: bool,
}

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// `gmtime`: `None` when the year does not fit `tm_year`.
fn gmtime(t: i64, utc: bool) -> Option<Tm> {
    let days = t.div_euclid(86400);
    let secs = t.rem_euclid(86400);
    let (year, mon, mday) = civil_from_days(days);
    if i32::try_from(year - 1900).is_err() {
        return None;
    }
    Some(Tm {
        year,
        mon: mon - 1,
        mday,
        hour: secs / 3600,
        min: secs / 60 % 60,
        sec: secs % 60,
        yday: days - days_from_civil(year, 1, 1),
        wday: (days + 4).rem_euclid(7),
        utc,
    })
}

fn now() -> i64 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_secs() as i64,
        Err(e) => -(e.duration().as_secs() as i64),
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

/// The ISO 8601 week-based year and week number.
fn iso_week(tm: &Tm) -> (i64, i64) {
    fn weeks_in(y: i64) -> i64 {
        let p =
            |y: i64| (y + y.div_euclid(4) - y.div_euclid(100) + y.div_euclid(400)).rem_euclid(7);
        if p(y) == 4 || p(y - 1) == 3 { 53 } else { 52 }
    }
    let wday_mon = (tm.wday + 6) % 7 + 1; // Monday = 1
    let week = (tm.yday + 1 - wday_mon + 10) / 7;
    if week < 1 {
        (tm.year - 1, weeks_in(tm.year - 1))
    } else if week > weeks_in(tm.year) {
        (tm.year + 1, 1)
    } else {
        (tm.year, week)
    }
}

/// Formats one conversion (`strftime` in the "C" locale).
fn strftime_one(out: &mut Vec<u8>, conv: u8, tm: &Tm) {
    use std::io::Write;
    let hour12 = if tm.hour % 12 == 0 { 12 } else { tm.hour % 12 };
    let _ = match conv {
        b'a' => write!(out, "{}", &DAYS[tm.wday as usize][..3]),
        b'A' => write!(out, "{}", DAYS[tm.wday as usize]),
        b'b' | b'h' => write!(out, "{}", &MONTHS[tm.mon as usize][..3]),
        b'B' => write!(out, "{}", MONTHS[tm.mon as usize]),
        b'c' => {
            for &c in b"a b e H:M:S Y" {
                if c.is_ascii_alphabetic() {
                    strftime_one(out, c, tm);
                } else {
                    out.push(c);
                }
            }
            Ok(())
        }
        b'C' => write!(out, "{:02}", tm.year.div_euclid(100)),
        b'd' => write!(out, "{:02}", tm.mday),
        b'D' | b'x' => write!(
            out,
            "{:02}/{:02}/{:02}",
            tm.mon + 1,
            tm.mday,
            tm.year.rem_euclid(100)
        ),
        b'e' => write!(out, "{:2}", tm.mday),
        b'F' => write!(out, "{}-{:02}-{:02}", tm.year, tm.mon + 1, tm.mday),
        b'g' => write!(out, "{:02}", iso_week(tm).0.rem_euclid(100)),
        b'G' => write!(out, "{}", iso_week(tm).0),
        b'H' => write!(out, "{:02}", tm.hour),
        b'I' => write!(out, "{hour12:02}"),
        b'j' => write!(out, "{:03}", tm.yday + 1),
        b'm' => write!(out, "{:02}", tm.mon + 1),
        b'M' => write!(out, "{:02}", tm.min),
        b'n' => writeln!(out),
        b'p' => write!(out, "{}", if tm.hour < 12 { "AM" } else { "PM" }),
        b'r' => write!(
            out,
            "{hour12:02}:{:02}:{:02} {}",
            tm.min,
            tm.sec,
            if tm.hour < 12 { "AM" } else { "PM" }
        ),
        b'R' => write!(out, "{:02}:{:02}", tm.hour, tm.min),
        b'S' => write!(out, "{:02}", tm.sec),
        b't' => write!(out, "\t"),
        b'T' | b'X' => write!(out, "{:02}:{:02}:{:02}", tm.hour, tm.min, tm.sec),
        b'u' => write!(out, "{}", (tm.wday + 6) % 7 + 1),
        b'U' => write!(out, "{:02}", (tm.yday + 7 - tm.wday) / 7),
        b'V' => write!(out, "{:02}", iso_week(tm).1),
        b'w' => write!(out, "{}", tm.wday),
        b'W' => write!(out, "{:02}", (tm.yday + 7 - (tm.wday + 6) % 7) / 7),
        b'y' => write!(out, "{:02}", tm.year.rem_euclid(100)),
        b'Y' => write!(out, "{}", tm.year),
        b'z' => write!(out, "+0000"),
        b'Z' => write!(out, "{}", if tm.utc { "GMT" } else { "UTC" }),
        _ => write!(out, "%"),
    };
}

const OPTIONS1: &[u8] = b"aAbBcCdDeFgGhHIjmMnprRStTuUVwWxXyYzZ%";
const OPTIONS2: &[&[u8]] = &[
    b"Ec", b"EC", b"Ex", b"EX", b"Ey", b"EY", b"Od", b"Oe", b"OH", b"OI", b"Om", b"OM", b"OS",
    b"Ou", b"OU", b"OV", b"Ow", b"OW", b"Oy",
];

fn set_int(vm: &mut Vm, t: orbit_core::value::TableRef, k: &str, v: i64) -> LuaResult<()> {
    let key = vm.heap.str_value(k);
    vm.set_table(Value::Table(t), key, Value::Int(v))
}

fn set_all_fields(vm: &mut Vm, t: orbit_core::value::TableRef, tm: &Tm) -> LuaResult<()> {
    set_int(vm, t, "year", tm.year)?;
    set_int(vm, t, "month", tm.mon + 1)?;
    set_int(vm, t, "day", tm.mday)?;
    set_int(vm, t, "hour", tm.hour)?;
    set_int(vm, t, "min", tm.min)?;
    set_int(vm, t, "sec", tm.sec)?;
    set_int(vm, t, "yday", tm.yday + 1)?;
    set_int(vm, t, "wday", tm.wday + 1)?;
    let key = vm.heap.str_value("isdst");
    vm.set_table(Value::Table(t), key, Value::Bool(false))
}

fn os_date(vm: &mut Vm) -> LuaResult<Action> {
    let fmt = match vm.opt_string(1)? {
        Some(s) => vm.heap.string(s).as_bytes().to_vec(),
        None => b"%c".to_vec(),
    };
    let t = match vm.arg(2) {
        Value::Nil => now(),
        _ => vm.check_integer(2)?,
    };
    let (utc, mut s) = match fmt.strip_prefix(b"!") {
        Some(rest) => (true, rest),
        None => (false, &fmt[..]),
    };
    let Some(tm) = gmtime(t, utc) else {
        return Err(vm.error("date result cannot be represented in this installation"));
    };
    if s == b"*t" {
        let t = vm.heap.new_table_with(0, 9).expect("small table");
        set_all_fields(vm, t, &tm)?;
        vm.push(Value::Table(t));
        return Ok(Action::Return(1));
    }
    let mut out = Vec::new();
    while let Some((&c, rest)) = s.split_first() {
        if c != b'%' {
            out.push(c);
            s = rest;
            continue;
        }
        let conv = rest;
        if let Some(&c1) = conv.first()
            && OPTIONS1.contains(&c1)
        {
            strftime_one(&mut out, c1, &tm);
            s = &conv[1..];
        } else if conv.len() >= 2 && OPTIONS2.contains(&&conv[..2]) {
            strftime_one(&mut out, conv[1], &tm);
            s = &conv[2..];
        } else {
            let msg = format!(
                "invalid conversion specifier '%{}'",
                String::from_utf8_lossy(conv)
            );
            return Err(vm.arg_error(1, &msg));
        }
    }
    let v = vm.heap.str_value(out);
    vm.push(v);
    Ok(Action::Return(1))
}

/// Reads an integer field of the date table (`getfield` in loslib.c).
fn get_field(vm: &mut Vm, key: &str, d: Option<i64>, delta: i64) -> LuaResult<i64> {
    let k = vm.heap.str_value(key);
    let t = vm.arg(1);
    let v = vm.index_value(t, k)?;
    match number::to_integer(&vm.heap, v, F2I::Exact) {
        Some(res) => {
            let ok = if res >= 0 {
                res - delta <= i32::MAX as i64
            } else {
                i32::MIN as i64 + delta <= res
            };
            if !ok {
                return Err(vm.error(format!("field '{key}' is out-of-bound")));
            }
            Ok(res - delta)
        }
        None if !v.is_nil() => Err(vm.error(format!("field '{key}' is not an integer"))),
        None => match d {
            Some(d) => Ok(d),
            None => Err(vm.error(format!("field '{key}' missing in date table"))),
        },
    }
}

fn os_time(vm: &mut Vm) -> LuaResult<Action> {
    let t = if vm.arg(1).is_nil() {
        now()
    } else {
        vm.check_table(1)?;
        vm.set_top(1);
        let year = get_field(vm, "year", None, 1900)?;
        let mon = get_field(vm, "month", None, 1)?;
        let mday = get_field(vm, "day", None, 0)?;
        let hour = get_field(vm, "hour", Some(12), 0)?;
        let min = get_field(vm, "min", Some(0), 0)?;
        let sec = get_field(vm, "sec", Some(0), 0)?;
        // (isdst is read but, with local time as UTC, has no effect.)
        let k = vm.heap.str_value("isdst");
        let t1 = vm.arg(1);
        vm.index_value(t1, k)?;
        let y = year + 1900 + mon.div_euclid(12);
        let days = days_from_civil(y, mon.rem_euclid(12) + 1, 1) + mday - 1;
        let t = days * 86400 + hour * 3600 + min * 60 + sec;
        let Some(tm) = gmtime(t, false) else {
            return Err(vm.error("time result cannot be represented in this installation"));
        };
        let Value::Table(tab) = vm.arg(1) else {
            unreachable!()
        };
        set_all_fields(vm, tab, &tm)?;
        t
    };
    if t == -1 {
        return Err(vm.error("time result cannot be represented in this installation"));
    }
    vm.push(Value::Int(t));
    Ok(Action::Return(1))
}

fn os_difftime(vm: &mut Vm) -> LuaResult<Action> {
    let t1 = vm.check_integer(1)?;
    let t2 = vm.check_integer(2)?;
    vm.push(Value::Float(t1 as f64 - t2 as f64));
    Ok(Action::Return(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fmt(t: i64, f: &[u8]) -> String {
        let tm = gmtime(t, true).unwrap();
        let mut out = Vec::new();
        for &c in f {
            strftime_one(&mut out, c, &tm);
        }
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn civil_round_trip() {
        for d in [-1_000_000, -719468, -1, 0, 1, 59, 60, 365, 10957, 2_000_000] {
            let (y, m, dd) = civil_from_days(d);
            assert_eq!(days_from_civil(y, m, dd), d);
        }
        assert_eq!(civil_from_days(0), (1970, 1, 1));
    }

    #[test]
    fn strftime_c_locale() {
        // 2009-02-13 23:31:30 UTC, a Friday.
        assert_eq!(fmt(1_234_567_890, b"c"), "Fri Feb 13 23:31:30 2009");
        assert_eq!(fmt(1_234_567_890, b"j"), "044");
        assert_eq!(fmt(1_234_567_890, b"U"), "06");
        assert_eq!(fmt(1_234_567_890, b"W"), "06");
        assert_eq!(fmt(1_234_567_890, b"V"), "07");
        assert_eq!(fmt(1_234_567_890, b"r"), "11:31:30 PM");
        // 2005-01-01 is in ISO week 53 of 2004.
        let t = days_from_civil(2005, 1, 1) * 86400;
        assert_eq!(fmt(t, b"G"), "2004");
        assert_eq!(fmt(t, b"V"), "53");
    }
}
