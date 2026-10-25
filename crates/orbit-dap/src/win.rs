//! Everything unsafe in the debugger (ADR-12): AF_UNIX stream sockets
//! and the debug-request event on Windows, through Winsock and kernel32.
//! std has Windows Unix sockets only on nightly
//! (`windows_unix_domain_sockets`); this file goes when they are stable.
//!
//! Every Windows that is supported (10 build 17063 and later) has AF_UNIX
//! stream sockets. A socket is a file in the per-user directory, whose
//! default ACL already keeps other users out.

#![allow(unsafe_code)]

use std::io::{self, Read, Write};
use std::path::Path;
use std::sync::Once;
use std::time::Duration;

use windows_sys::Win32::Foundation::{CloseHandle, FALSE, HANDLE, WAIT_OBJECT_0};
use windows_sys::Win32::Networking::WinSock::{
    AF_UNIX, FIONBIO, FROM_PROTOCOL_INFO, INVALID_SOCKET, SD_BOTH, SD_RECEIVE, SD_SEND,
    SO_RCVTIMEO, SOCK_STREAM, SOCKADDR, SOCKADDR_UN, SOCKET, SOCKET_ERROR, SOL_SOCKET,
    WSA_FLAG_NO_HANDLE_INHERIT, WSA_FLAG_OVERLAPPED, WSADATA, WSADuplicateSocketW, WSAGetLastError,
    WSAPROTOCOL_INFOW, WSASocketW, WSAStartup, accept, bind, closesocket, connect, ioctlsocket,
    listen, recv, send, setsockopt, shutdown,
};
use windows_sys::Win32::System::Threading::{
    CreateEventW, EVENT_MODIFY_STATE, INFINITE, OpenEventW, SetEvent, WaitForSingleObject,
};

/// Start Winsock once per process.
fn startup() {
    static START: Once = Once::new();
    START.call_once(|| {
        // SAFETY: `data` is a valid out-pointer for WSAStartup.
        let mut data: WSADATA = unsafe { std::mem::zeroed() };
        // SAFETY: as above. A failure shows in the first socket call.
        unsafe { WSAStartup(0x0202, &mut data) };
    });
}

fn last_error() -> io::Error {
    // SAFETY: no preconditions.
    io::Error::from_raw_os_error(unsafe { WSAGetLastError() })
}

/// `path` as a socket address.
fn address(path: &Path) -> io::Result<(SOCKADDR_UN, i32)> {
    let bytes = path
        .to_str()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "socket path is not UTF-8"))?
        .as_bytes();
    // SAFETY: SOCKADDR_UN is plain data; all zeroes is valid.
    let mut addr: SOCKADDR_UN = unsafe { std::mem::zeroed() };
    if bytes.len() >= addr.sun_path.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "socket path is too long",
        ));
    }
    addr.sun_family = AF_UNIX;
    for (to, &from) in addr.sun_path.iter_mut().zip(bytes) {
        *to = from as i8;
    }
    let len = std::mem::size_of::<u16>() + bytes.len() + 1;
    Ok((addr, len as i32))
}

/// How every socket here is made. Overlapped, as std makes its own: a
/// socket without it is synchronous, so Windows runs one call at a time on
/// it and its duplicates (a `send` on a clone waits for a blocked `recv`),
/// and `SO_RCVTIMEO` is ignored. Calls still block. Not inherited by child
/// processes.
const SOCKET_FLAGS: u32 = WSA_FLAG_OVERLAPPED | WSA_FLAG_NO_HANDLE_INHERIT;

/// A new AF_UNIX stream socket.
fn new_socket() -> io::Result<Socket> {
    startup();
    // SAFETY: plain arguments; no protocol info.
    let s = unsafe {
        WSASocketW(
            AF_UNIX as i32,
            SOCK_STREAM,
            0,
            std::ptr::null(),
            0,
            SOCKET_FLAGS,
        )
    };
    if s == INVALID_SOCKET {
        return Err(last_error());
    }
    Ok(Socket(s))
}

/// A connected AF_UNIX stream socket, closed when dropped.
#[derive(Debug)]
pub(crate) struct Socket(SOCKET);

impl Drop for Socket {
    fn drop(&mut self) {
        // SAFETY: the socket is ours and closed only here.
        unsafe { closesocket(self.0) };
    }
}

impl Socket {
    pub(crate) fn connect(path: &Path) -> io::Result<Socket> {
        let (addr, len) = address(path)?;
        let s = new_socket()?;
        // SAFETY: `addr` is a valid SOCKADDR_UN of `len` bytes.
        let r = unsafe { connect(s.0, (std::ptr::addr_of!(addr)).cast::<SOCKADDR>(), len) };
        if r == SOCKET_ERROR {
            return Err(last_error());
        }
        Ok(s)
    }

    /// Another handle to the same connection.
    pub(crate) fn try_clone(&self) -> io::Result<Socket> {
        // SAFETY: `info` is a valid out-pointer.
        let mut info: WSAPROTOCOL_INFOW = unsafe { std::mem::zeroed() };
        // SAFETY: duplicating into this process with a valid out-pointer.
        let r = unsafe { WSADuplicateSocketW(self.0, std::process::id(), &mut info) };
        if r == SOCKET_ERROR {
            return Err(last_error());
        }
        // SAFETY: `info` describes the socket just duplicated.
        let s = unsafe {
            WSASocketW(
                FROM_PROTOCOL_INFO,
                FROM_PROTOCOL_INFO,
                FROM_PROTOCOL_INFO,
                &info,
                0,
                SOCKET_FLAGS,
            )
        };
        if s == INVALID_SOCKET {
            return Err(last_error());
        }
        Ok(Socket(s))
    }

    pub(crate) fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        // Milliseconds as a DWORD; 0 waits forever.
        let ms: u32 = timeout.map_or(0, |d| d.as_millis().clamp(1, u32::MAX as u128) as u32);
        // SAFETY: `ms` is a DWORD of the size given.
        let r = unsafe {
            setsockopt(
                self.0,
                SOL_SOCKET,
                SO_RCVTIMEO,
                (std::ptr::addr_of!(ms)).cast(),
                std::mem::size_of::<u32>() as i32,
            )
        };
        if r == SOCKET_ERROR {
            return Err(last_error());
        }
        Ok(())
    }

    pub(crate) fn set_nonblocking(&self, on: bool) -> io::Result<()> {
        let mut arg = u32::from(on);
        // SAFETY: FIONBIO takes a pointer to a u32.
        let r = unsafe { ioctlsocket(self.0, FIONBIO, &mut arg) };
        if r == SOCKET_ERROR {
            return Err(last_error());
        }
        Ok(())
    }

    pub(crate) fn shutdown(&self, how: std::net::Shutdown) -> io::Result<()> {
        let how = match how {
            std::net::Shutdown::Read => SD_RECEIVE,
            std::net::Shutdown::Write => SD_SEND,
            std::net::Shutdown::Both => SD_BOTH,
        };
        // SAFETY: the socket is ours.
        if unsafe { shutdown(self.0, how) } == SOCKET_ERROR {
            return Err(last_error());
        }
        Ok(())
    }

    fn recv(&self, buf: &mut [u8]) -> io::Result<usize> {
        let len = buf.len().min(i32::MAX as usize) as i32;
        // SAFETY: `buf` is valid for `len` bytes.
        let n = unsafe { recv(self.0, buf.as_mut_ptr(), len, 0) };
        if n == SOCKET_ERROR {
            return Err(last_error());
        }
        Ok(n as usize)
    }

    fn send(&self, buf: &[u8]) -> io::Result<usize> {
        let len = buf.len().min(i32::MAX as usize) as i32;
        // SAFETY: `buf` is valid for `len` bytes.
        let n = unsafe { send(self.0, buf.as_ptr(), len, 0) };
        if n == SOCKET_ERROR {
            return Err(last_error());
        }
        Ok(n as usize)
    }
}

impl Read for Socket {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.recv(buf)
    }
}

impl Read for &Socket {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.recv(buf)
    }
}

impl Write for Socket {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.send(buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// A listening AF_UNIX socket.
#[derive(Debug)]
pub(crate) struct Listener(Socket);

impl Listener {
    pub(crate) fn bind(path: &Path) -> io::Result<Listener> {
        let (addr, len) = address(path)?;
        let s = new_socket()?;
        // SAFETY: `addr` is a valid SOCKADDR_UN of `len` bytes.
        if unsafe { bind(s.0, (std::ptr::addr_of!(addr)).cast::<SOCKADDR>(), len) } == SOCKET_ERROR
        {
            return Err(last_error());
        }
        // SAFETY: a bound socket.
        if unsafe { listen(s.0, 16) } == SOCKET_ERROR {
            return Err(last_error());
        }
        Ok(Listener(s))
    }

    pub(crate) fn accept(&self) -> io::Result<Socket> {
        // SAFETY: no address is asked for.
        let s = unsafe { accept(self.0.0, std::ptr::null_mut(), std::ptr::null_mut()) };
        if s == INVALID_SOCKET {
            return Err(last_error());
        }
        let s = Socket(s);
        // An accepted socket inherits the listener's non-blocking mode.
        s.set_nonblocking(false)?;
        Ok(s)
    }

    pub(crate) fn try_clone(&self) -> io::Result<Listener> {
        self.0.try_clone().map(Listener)
    }

    pub(crate) fn set_nonblocking(&self, on: bool) -> io::Result<()> {
        self.0.set_nonblocking(on)
    }
}

/// The event `orbit --debug-attach` sets to ask `pid` to start its debug
/// server: Windows has no `SIGUSR1`.
fn event_name(pid: u32) -> Vec<u16> {
    format!("Local\\orbit-debug-{pid}")
        .encode_utf16()
        .chain([0])
        .collect()
}

/// This process's debug-request event.
#[derive(Debug)]
pub(crate) struct DebugRequests(HANDLE);

// SAFETY: an event handle may be waited on from any thread.
unsafe impl Send for DebugRequests {}

impl DebugRequests {
    pub(crate) fn create() -> io::Result<DebugRequests> {
        let name = event_name(std::process::id());
        // SAFETY: an auto-reset event with a valid NUL-terminated name.
        let h = unsafe { CreateEventW(std::ptr::null(), FALSE, FALSE, name.as_ptr()) };
        if h.is_null() {
            return Err(io::Error::last_os_error());
        }
        Ok(DebugRequests(h))
    }

    /// Wait for the next request.
    pub(crate) fn wait(&self) -> io::Result<()> {
        // SAFETY: the handle is ours and open.
        if unsafe { WaitForSingleObject(self.0, INFINITE) } != WAIT_OBJECT_0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

impl Drop for DebugRequests {
    fn drop(&mut self) {
        // SAFETY: the handle is ours and closed only here.
        unsafe { CloseHandle(self.0) };
    }
}

/// Ask `pid` to start its debug server; `false` if it takes no requests
/// (it has no event, so it isn't an `orbit` that serves debuggers).
pub(crate) fn request_debugging(pid: u32) -> io::Result<bool> {
    let name = event_name(pid);
    // SAFETY: a valid NUL-terminated name.
    let h = unsafe { OpenEventW(EVENT_MODIFY_STATE, FALSE, name.as_ptr()) };
    if h.is_null() {
        return Ok(false);
    }
    // SAFETY: `h` was just opened with EVENT_MODIFY_STATE.
    let ok = unsafe { SetEvent(h) } != 0;
    // SAFETY: as above, closed once.
    unsafe { CloseHandle(h) };
    if !ok {
        return Err(io::Error::last_os_error());
    }
    Ok(true)
}
