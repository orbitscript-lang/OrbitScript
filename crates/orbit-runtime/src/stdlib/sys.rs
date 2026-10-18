//! Helpers the `os` and `io` libraries share: OS errors as Lua's file
//! functions return them, and child processes.

use crate::object::Value;
use crate::vm::{LuaError, NativeCtx, NativeReturn};

type R = Result<NativeReturn, LuaError>;

/// `nil, message, code` for a failed file operation, as Lua's file
/// functions return it. `name`, when given, prefixes the message.
pub(crate) fn file_error(ctx: &mut NativeCtx<'_>, e: &std::io::Error, name: Option<&[u8]>) -> R {
    let mut msg = Vec::new();
    if let Some(n) = name {
        msg.extend_from_slice(n);
        msg.extend_from_slice(b": ");
    }
    msg.extend_from_slice(crate::stdlib::os_error_text(e).as_bytes());
    let m = ctx.new_str_owned(msg);
    ctx.push(Value::Nil);
    ctx.push(m);
    ctx.push(Value::Int(i64::from(e.raw_os_error().unwrap_or(0))));
    Ok(NativeReturn::Return(3))
}

/// The shell command for `cmd`.
pub(crate) fn shell(cmd: &std::ffi::OsStr) -> std::process::Command {
    #[cfg(windows)]
    {
        let mut c = std::process::Command::new("cmd");
        c.arg("/C").arg(cmd);
        c
    }
    #[cfg(not(windows))]
    {
        let mut c = std::process::Command::new("/bin/sh");
        c.arg("-c").arg(cmd);
        c
    }
}

/// `true|nil, "exit"|"signal", code` for a finished child, as
/// `os.execute` and a popen file's `close` return it.
pub(crate) fn exit_status(ctx: &mut NativeCtx<'_>, status: std::process::ExitStatus) -> R {
    let (what, code): (&[u8], i64) = match status.code() {
        Some(c) => (b"exit", i64::from(c)),
        None => {
            #[cfg(unix)]
            let sig = std::os::unix::process::ExitStatusExt::signal(&status).unwrap_or(0);
            #[cfg(not(unix))]
            let sig = 0;
            (b"signal", i64::from(sig))
        }
    };
    let ok = what == b"exit" && code == 0;
    ctx.push(if ok { Value::Bool(true) } else { Value::Nil });
    let w = ctx.new_str(what);
    ctx.push(w);
    ctx.push(Value::Int(code));
    Ok(NativeReturn::Return(3))
}
