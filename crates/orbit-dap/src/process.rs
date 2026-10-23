//! Everything unsafe in the debugger on Unix (ADR-12): sending `SIGUSR1` to
//! a program so it starts its debug server, and on macOS, which has no
//! `/proc`, finding the programs that would (libproc and `sysctl`). All of
//! it is calls through `libc`; what they write is decoded by safe code.

#![allow(unsafe_code)]

use std::io;

/// Send `SIGUSR1` to `pid`.
pub(crate) fn send_debug_signal(pid: u32) -> io::Result<()> {
    let pid =
        libc::pid_t::try_from(pid).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
    // SAFETY: `kill` takes no pointers.
    if unsafe { libc::kill(pid, libc::SIGUSR1) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(target_os = "macos")]
pub(crate) use macos::{args, caught_signals, executable, pids};

#[cfg(target_os = "macos")]
mod macos {
    use std::ffi::{OsString, c_int, c_void};
    use std::os::unix::ffi::OsStringExt;
    use std::path::PathBuf;
    use std::ptr::null_mut;

    /// The pids of every running process.
    pub(crate) fn pids() -> Vec<u32> {
        // SAFETY: a null buffer asks only for the count.
        let n = unsafe { libc::proc_listallpids(null_mut(), 0) };
        let Ok(n) = usize::try_from(n) else {
            return Vec::new();
        };
        // Room for processes started since.
        let mut pids: Vec<libc::pid_t> = vec![0; n + 64];
        let bytes = c_int::try_from(std::mem::size_of_val(pids.as_slice())).unwrap_or(c_int::MAX);
        // SAFETY: `pids` holds `bytes` bytes, and the call writes at most
        // that many.
        let n = unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast::<c_void>(), bytes) };
        pids.truncate(usize::try_from(n).unwrap_or(0));
        pids.into_iter()
            .filter_map(|p| u32::try_from(p).ok())
            .filter(|&p| p != 0)
            .collect()
    }

    /// The executable `pid` runs.
    pub(crate) fn executable(pid: u32) -> Option<PathBuf> {
        let pid = c_int::try_from(pid).ok()?;
        let mut buf = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
        let len = u32::try_from(buf.len()).ok()?;
        // SAFETY: `buf` holds `len` bytes, and the call writes at most that
        // many.
        let n = unsafe { libc::proc_pidpath(pid, buf.as_mut_ptr().cast::<c_void>(), len) };
        buf.truncate(usize::try_from(n).ok().filter(|&n| n > 0)?);
        Some(PathBuf::from(OsString::from_vec(buf)))
    }

    /// The signals `pid` has handlers for, bit `n - 1` for signal `n`.
    pub(crate) fn caught_signals(pid: u32) -> Option<u32> {
        let pid = c_int::try_from(pid).ok()?;
        let info = sysctl(&mut [libc::CTL_KERN, libc::KERN_PROC, libc::KERN_PROC_PID, pid])?;
        let at = std::mem::offset_of!(ExternProc, p_sigcatch);
        let mask = info.get(at..at + size_of::<libc::sigset_t>())?;
        Some(libc::sigset_t::from_ne_bytes(mask.try_into().ok()?))
    }

    /// The command line `pid` was started with.
    pub(crate) fn args(pid: u32) -> Option<Vec<String>> {
        let pid = c_int::try_from(pid).ok()?;
        super::parse_procargs(&sysctl(&mut [libc::CTL_KERN, libc::KERN_PROCARGS2, pid])?)
    }

    /// What the `sysctl` `mib` names.
    fn sysctl(mib: &mut [c_int]) -> Option<Vec<u8>> {
        let names = u32::try_from(mib.len()).ok()?;
        let mut len = 0;
        // SAFETY: `mib` holds `names` integers; a null buffer asks only for
        // the size, which is written to `len`.
        let asked =
            unsafe { libc::sysctl(mib.as_mut_ptr(), names, null_mut(), &mut len, null_mut(), 0) };
        if asked != 0 {
            return None;
        }
        let mut buf = vec![0u8; len];
        // SAFETY: as above, and `buf` holds `len` bytes; the call writes at
        // most that many and sets `len` to the number written.
        let read = unsafe {
            libc::sysctl(
                mib.as_mut_ptr(),
                names,
                buf.as_mut_ptr().cast::<c_void>(),
                &mut len,
                null_mut(),
                0,
            )
        };
        if read != 0 {
            return None;
        }
        buf.truncate(len);
        Some(buf)
    }

    /// The start of `struct extern_proc` (`<sys/proc.h>`), up to the
    /// caught-signals mask: the first member of the `struct kinfo_proc`
    /// that `KERN_PROC_PID` answers with. `libc` doesn't declare it for
    /// macOS. Only its layout is used, for the mask's offset.
    #[allow(dead_code)]
    #[repr(C)]
    struct ExternProc {
        /// A union of two pointers and a `timeval`.
        p_un: [*mut c_void; 2],
        p_vmspace: *mut c_void,
        p_sigacts: *mut c_void,
        p_flag: c_int,
        p_stat: libc::c_char,
        p_pid: libc::pid_t,
        p_oppid: libc::pid_t,
        p_dupfd: c_int,
        user_stack: *mut libc::c_char,
        exit_thread: *mut c_void,
        p_debugger: c_int,
        sigwait: c_int,
        p_estcpu: u32,
        p_cpticks: c_int,
        p_pctcpu: u32,
        p_wchan: *mut c_void,
        p_wmesg: *mut libc::c_char,
        p_swtime: u32,
        p_slptime: u32,
        p_realtimer: [libc::timeval; 2],
        p_rtime: libc::timeval,
        p_uticks: u64,
        p_sticks: u64,
        p_iticks: u64,
        p_traceflag: c_int,
        p_tracep: *mut c_void,
        p_siglist: c_int,
        p_textvp: *mut c_void,
        p_holdcnt: c_int,
        p_sigmask: libc::sigset_t,
        p_sigignore: libc::sigset_t,
        p_sigcatch: libc::sigset_t,
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn this_process() {
            let me = std::process::id();
            assert!(pids().contains(&me));
            assert_eq!(
                executable(me).unwrap().canonicalize().unwrap(),
                std::env::current_exe().unwrap().canonicalize().unwrap()
            );
            // std handles SIGSEGV and SIGBUS to report stack overflows;
            // nothing here handles SIGUSR1.
            let caught = caught_signals(me).unwrap();
            assert_ne!(caught & 1 << (libc::SIGSEGV - 1), 0, "{caught:#x}");
            assert_ne!(caught & 1 << (libc::SIGBUS - 1), 0, "{caught:#x}");
            assert_eq!(caught & 1 << (libc::SIGUSR1 - 1), 0, "{caught:#x}");
            let args = args(me).unwrap();
            assert_eq!(args.len(), std::env::args().count(), "{args:?}");
        }
    }
}

/// The command line in a `KERN_PROCARGS2` answer: the argument count, the
/// executable's path, padding, then the arguments, each ended by a NUL
/// (and the environment after them).
#[cfg(any(target_os = "macos", test))]
fn parse_procargs(buf: &[u8]) -> Option<Vec<String>> {
    let argc = usize::try_from(i32::from_ne_bytes(buf.get(..4)?.try_into().ok()?)).ok()?;
    let rest = &buf[4..];
    let rest = &rest[rest.iter().position(|&b| b == 0)?..];
    let rest = &rest[rest.iter().position(|&b| b != 0)?..];
    let args: Vec<String> = rest
        .split(|&b| b == 0)
        .take(argc)
        .map(|a| String::from_utf8_lossy(a).into_owned())
        .collect();
    (args.len() == argc).then_some(args)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn procargs() {
        let mut buf = 2i32.to_ne_bytes().to_vec();
        buf.extend_from_slice(b"/usr/local/bin/orbit\0\0\0\0orbit\0my script.orb\0HOME=/x\0");
        assert_eq!(
            parse_procargs(&buf),
            Some(vec!["orbit".to_owned(), "my script.orb".to_owned()])
        );
        assert_eq!(parse_procargs(&buf[..20]), None);
        assert_eq!(parse_procargs(b"\x01"), None);
    }
}
