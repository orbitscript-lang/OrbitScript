//! The C library's buffered streams (`FILE*`), which `io` handles are, so
//! that C modules and scripts share one stream, one buffer and one file
//! position, as the Lua C API promises (`luaL_Stream`, ADR-15).
//!
//! [`CFile`] owns or borrows a `FILE*` and offers safe calls over it that
//! take Rust slices with their lengths. No script value ever becomes a
//! pointer here: a `FILE*` comes from `fdopen` on a descriptor Rust opened,
//! from the C library's standard streams, or, for a stream C code made,
//! from the `luaL_Stream` block that C code filled in (which only exists
//! once native code is loaded).
//!
//! These are orbit-std's only unsafe calls apart from `crt.rs` and
//! `ctime.rs` (ADR-6).

#![allow(unsafe_code)]

use std::ffi::{CStr, c_int, c_void};
use std::io;
use std::ptr::NonNull;

use libc::FILE;

// ---- the C library's names that the `libc` crate doesn't bind everywhere --

#[cfg(all(unix, not(target_vendor = "apple")))]
extern "C" {
    static stdin: *mut FILE;
    static stdout: *mut FILE;
    static stderr: *mut FILE;
}

#[cfg(target_vendor = "apple")]
extern "C" {
    #[link_name = "__stdinp"]
    static stdin: *mut FILE;
    #[link_name = "__stdoutp"]
    static stdout: *mut FILE;
    #[link_name = "__stderrp"]
    static stderr: *mut FILE;
}

#[cfg(unix)]
extern "C" {
    fn getdelim(
        line: *mut *mut libc::c_char,
        cap: *mut libc::size_t,
        delim: c_int,
        f: *mut FILE,
    ) -> libc::ssize_t;
    fn getc_unlocked(f: *mut FILE) -> c_int;
    fn flockfile(f: *mut FILE);
    fn funlockfile(f: *mut FILE);
}

#[cfg(windows)]
extern "C" {
    fn __acrt_iob_func(index: std::ffi::c_uint) -> *mut FILE;
    fn _errno() -> *mut c_int;
    fn _fseeki64(f: *mut FILE, off: i64, whence: c_int) -> c_int;
    fn _ftelli64(f: *mut FILE) -> i64;
    fn _lock_file(f: *mut FILE);
    fn _unlock_file(f: *mut FILE);
    fn _fgetc_nolock(f: *mut FILE) -> c_int;
    fn _setmode(fd: c_int, mode: c_int) -> c_int;
    fn clearerr(f: *mut FILE);
}

/// One of the C library's standard streams.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Std {
    In,
    Out,
    Err,
}

fn std_stream(which: Std) -> *mut FILE {
    #[cfg(unix)]
    // SAFETY: the C library's standard stream pointers, set before `main`
    // and never changed by us.
    unsafe {
        match which {
            Std::In => stdin,
            Std::Out => stdout,
            Std::Err => stderr,
        }
    }
    #[cfg(windows)]
    // SAFETY: the UCRT's accessor for its standard streams, 0 to 2.
    unsafe {
        __acrt_iob_func(match which {
            Std::In => 0,
            Std::Out => 1,
            Std::Err => 2,
        })
    }
}

/// The C library's `errno` now, as an error.
fn last_error() -> io::Error {
    #[cfg(unix)]
    {
        let e = io::Error::last_os_error();
        match e.raw_os_error() {
            Some(0) | None => crate::crt::system_error(libc::EIO),
            Some(_) => e,
        }
    }
    #[cfg(windows)]
    {
        // SAFETY: `_errno` gives this thread's `errno`.
        let n = unsafe { *_errno() };
        crate::crt::system_error(if n == 0 { libc::EIO } else { n })
    }
}

/// Clear `errno`, so an error read after a call is that call's.
fn clear_errno() {
    #[cfg(target_os = "linux")]
    // SAFETY: this thread's `errno`.
    unsafe {
        *libc::__errno_location() = 0
    };
    #[cfg(target_vendor = "apple")]
    // SAFETY: this thread's `errno`.
    unsafe {
        *libc::__error() = 0
    };
    #[cfg(windows)]
    // SAFETY: this thread's `errno`.
    unsafe {
        *_errno() = 0
    };
}

/// How a stream buffers (`setvbuf`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Buffering {
    No,
    Line,
    Full,
}

/// Where `seek` counts from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Whence {
    Set,
    Cur,
    End,
}

/// A C stream. An owned one is closed when dropped unless [`CFile::close`]
/// closed it; a borrowed one (a standard stream, or one C code made) is
/// never closed by us except through [`CFile::close`].
#[derive(Debug)]
pub(crate) struct CFile {
    ptr: NonNull<FILE>,
    owned: bool,
    /// `getdelim`'s line buffer, kept between reads.
    #[cfg(unix)]
    line: LineBuf,
}

/// The buffer `getdelim` reads lines into, kept between reads; the C
/// library allocates and grows it.
#[cfg(unix)]
#[derive(Debug)]
struct LineBuf {
    ptr: *mut libc::c_char,
    cap: libc::size_t,
}

#[cfg(unix)]
impl LineBuf {
    const EMPTY: LineBuf = LineBuf {
        ptr: std::ptr::null_mut(),
        cap: 0,
    };
}

#[cfg(unix)]
impl Drop for LineBuf {
    fn drop(&mut self) {
        // SAFETY: null, or a buffer `getdelim` allocated with `malloc`.
        unsafe { libc::free(self.ptr.cast()) };
    }
}

impl CFile {
    fn new(ptr: NonNull<FILE>, owned: bool) -> CFile {
        CFile {
            ptr,
            owned,
            #[cfg(unix)]
            line: LineBuf::EMPTY,
        }
    }

    /// One of the C library's standard streams.
    pub(crate) fn standard(which: Std) -> Option<CFile> {
        NonNull::new(std_stream(which)).map(|ptr| CFile::new(ptr, false))
    }

    /// A binary stream of its own over one of the standard streams'
    /// descriptors, for an isolate whose standard streams carry bytes on a
    /// platform whose C library makes them text streams (Windows).
    #[cfg(windows)]
    pub(crate) fn standard_binary(which: Std) -> Option<CFile> {
        let (fd, m): (c_int, &[u8]) = match which {
            Std::In => (0, b"rb\0"),
            Std::Out => (1, b"wb\0"),
            Std::Err => (2, b"wb\0"),
        };
        // SAFETY: duplicates a standard descriptor; we own the copy.
        let dup = unsafe { libc::dup(fd) };
        if dup < 0 {
            return None;
        }
        // SAFETY: sets our copy's translation mode.
        unsafe { _setmode(dup, 0x8000) };
        // SAFETY: `dup` is ours; on success the stream owns it.
        let f = unsafe { libc::fdopen(dup, m.as_ptr().cast()) };
        match NonNull::new(f) {
            Some(ptr) => Some(CFile::new(ptr, true)),
            None => {
                // SAFETY: still ours, as fdopen failed.
                unsafe { libc::close(dup) };
                None
            }
        }
    }

    /// The stream in a `luaL_Stream` block that C code made: its `f`.
    /// `None` if `f` is null. `block` is the userdata's memory, at least
    /// [`STREAM_BLOCK_SIZE`] bytes long, which the caller checked.
    pub(crate) fn foreign(block: *mut c_void) -> Option<CFile> {
        let (f, _) = read_stream_block(block);
        NonNull::new(f as *mut FILE).map(|ptr| CFile::new(ptr, false))
    }

    /// The `FILE*`, as C sees it.
    pub(crate) fn as_raw(&self) -> usize {
        self.ptr.as_ptr() as usize
    }

    /// Wrap a descriptor Rust opened in a C stream with `fopen`-style
    /// `mode` (`r`, `w+`, `a`...). `text` makes it a text stream where the
    /// C library has them (Windows); elsewhere it changes nothing. The
    /// descriptor is closed if this fails.
    #[cfg(unix)]
    pub(crate) fn from_fd(fd: std::os::fd::OwnedFd, mode: &CStr, _text: bool) -> io::Result<CFile> {
        use std::os::fd::IntoRawFd;
        let raw = fd.into_raw_fd();
        // SAFETY: `raw` is an open descriptor we own; on success the stream
        // owns it.
        let f = unsafe { libc::fdopen(raw, mode.as_ptr()) };
        match NonNull::new(f) {
            Some(ptr) => Ok(CFile::new(ptr, true)),
            None => {
                let e = last_error();
                // SAFETY: still ours, as fdopen failed.
                unsafe { libc::close(raw) };
                Err(e)
            }
        }
    }

    #[cfg(windows)]
    pub(crate) fn from_handle(
        h: std::os::windows::io::OwnedHandle,
        mode: &CStr,
        text: bool,
    ) -> io::Result<CFile> {
        use std::os::windows::io::IntoRawHandle;
        const O_RDONLY: c_int = 0x0000;
        const O_APPEND: c_int = 0x0008;
        const O_TEXT: c_int = 0x4000;
        const O_BINARY: c_int = 0x8000;
        let m = mode.to_bytes();
        let mut flags = if text { O_TEXT } else { O_BINARY };
        if m.first() == Some(&b'r') && !m.contains(&b'+') {
            flags |= O_RDONLY;
        }
        if m.first() == Some(&b'a') {
            flags |= O_APPEND;
        }
        let raw = h.into_raw_handle();
        // SAFETY: `raw` is an open handle we own; on success the descriptor
        // owns it.
        let fd = unsafe { libc::open_osfhandle(raw as libc::intptr_t, flags) };
        if fd < 0 {
            let e = last_error();
            // SAFETY: still ours, as `_open_osfhandle` failed.
            drop(unsafe {
                <std::os::windows::io::OwnedHandle as std::os::windows::io::FromRawHandle>::from_raw_handle(raw)
            });
            return Err(e);
        }
        // SAFETY: `fd` is a descriptor we own; on success the stream owns it.
        let f = unsafe { libc::fdopen(fd, mode.as_ptr()) };
        match NonNull::new(f) {
            Some(ptr) => Ok(CFile::new(ptr, true)),
            None => {
                let e = last_error();
                // SAFETY: still ours, as fdopen failed.
                unsafe { libc::close(fd) };
                Err(e)
            }
        }
    }

    /// Lock the stream for a run of calls (`flockfile`), so that they
    /// take the C library's lock once between them, not once each.
    pub(crate) fn lock(&mut self) -> Locked<'_> {
        // SAFETY: the stream is open; `Locked` unlocks it when dropped.
        unsafe { lock(self.ptr.as_ptr()) };
        Locked { file: self }
    }

    /// Write all of `bytes` (`fwrite`).
    pub(crate) fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.lock().write(bytes)
    }

    /// Move the position and give the new one (`fseek` then `ftell`).
    pub(crate) fn seek(&mut self, whence: Whence, off: i64) -> io::Result<u64> {
        let w = match whence {
            Whence::Set => libc::SEEK_SET,
            Whence::Cur => libc::SEEK_CUR,
            Whence::End => libc::SEEK_END,
        };
        let f = self.ptr.as_ptr();
        clear_errno();
        #[cfg(unix)]
        // SAFETY: the stream is open.
        let (r, pos) = unsafe {
            let r = libc::fseeko(f, off as libc::off_t, w);
            (r, if r == 0 { libc::ftello(f) as i64 } else { -1 })
        };
        #[cfg(windows)]
        // SAFETY: the stream is open.
        let (r, pos) = unsafe {
            let r = _fseeki64(f, off, w);
            (r, if r == 0 { _ftelli64(f) } else { -1 })
        };
        if r != 0 || pos < 0 {
            return Err(last_error());
        }
        Ok(pos as u64)
    }

    /// Change the buffering (`setvbuf`), with a buffer of `size` bytes the
    /// C library allocates.
    pub(crate) fn set_buffering(&mut self, mode: Buffering, size: usize) -> io::Result<()> {
        let m = match mode {
            Buffering::No => libc::_IONBF,
            Buffering::Line => libc::_IOLBF,
            Buffering::Full => libc::_IOFBF,
        };
        clear_errno();
        // SAFETY: the stream is open; a null buffer asks the C library for
        // its own.
        let r = unsafe { libc::setvbuf(self.ptr.as_ptr(), std::ptr::null_mut(), m, size) };
        if r == 0 { Ok(()) } else { Err(last_error()) }
    }

    /// Write out the buffer (`fflush`).
    pub(crate) fn flush(&mut self) -> io::Result<()> {
        clear_errno();
        // SAFETY: the stream is open.
        if unsafe { libc::fflush(self.ptr.as_ptr()) } == 0 {
            Ok(())
        } else {
            Err(last_error())
        }
    }

    /// Close the stream (`fclose`), owned or not.
    pub(crate) fn close(mut self) -> io::Result<()> {
        let f = self.ptr.as_ptr();
        // Dropped without closing; the line buffer goes with it.
        self.owned = false;
        drop(self);
        clear_errno();
        // SAFETY: the stream is open, and nothing uses it after this.
        if unsafe { libc::fclose(f) } == 0 {
            Ok(())
        } else {
            Err(last_error())
        }
    }
}

impl Drop for CFile {
    fn drop(&mut self) {
        if self.owned {
            // SAFETY: we own the stream, and nothing uses it after this.
            unsafe { libc::fclose(self.ptr.as_ptr()) };
        }
    }
}

/// A stream this thread holds the C library's lock of, for a run of
/// calls; dropping it unlocks. Every call here takes the stream without
/// locking it again where the C library has a way to (glibc and musl,
/// the UCRT), and through its recursive lock elsewhere.
pub(crate) struct Locked<'a> {
    file: &'a mut CFile,
}

impl Drop for Locked<'_> {
    fn drop(&mut self) {
        // SAFETY: an open stream this thread locked in `CFile::lock`.
        unsafe { unlock(self.file.ptr.as_ptr()) };
    }
}

impl Locked<'_> {
    fn raw(&self) -> *mut FILE {
        self.file.ptr.as_ptr()
    }

    /// Write all of `bytes` (`fwrite`).
    pub(crate) fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
        if bytes.is_empty() {
            return Ok(());
        }
        clear_errno();
        // SAFETY: `bytes` is readable for its length; the stream is open
        // and locked.
        let n = unsafe { fwrite_nolock(bytes, self.raw()) };
        if n == bytes.len() {
            Ok(())
        } else {
            Err(last_error())
        }
    }

    /// Append up to `n` bytes to `into` (`fread`). Gives whether it read
    /// any; fewer than `n` means the end of the stream or an error, which
    /// [`Locked::error`] then reports.
    pub(crate) fn read_into(&mut self, n: usize, into: &mut Vec<u8>) -> io::Result<bool> {
        let mut left = n;
        let mut any = false;
        while left > 0 {
            // A small count reads in one go; a large one (`"a"`) in steps,
            // so memory grows only as the bytes arrive.
            let want = left.min(64 * 1024);
            let at = into.len();
            into.try_reserve(want)
                .map_err(|_| io::Error::from(io::ErrorKind::OutOfMemory))?;
            let spare = &mut into.spare_capacity_mut()[..want];
            // SAFETY: `spare` is writable for `want` bytes; the stream is
            // open and locked. `fread` initialised the `got` bytes it
            // reports, which the length then takes in.
            let got = unsafe {
                let got = fread_nolock(spare.as_mut_ptr().cast(), want, self.raw());
                into.set_len(at + got);
                got
            };
            any |= got > 0;
            left -= got;
            if got < want {
                break;
            }
        }
        Ok(any)
    }

    /// Read through the next `\n` (kept if `keep`) into `into`, or to the
    /// end. Gives whether it read anything, newline included.
    #[cfg(unix)]
    pub(crate) fn read_line(&mut self, keep: bool, into: &mut Vec<u8>) -> io::Result<bool> {
        let f = self.raw();
        let line = &mut self.file.line;
        // SAFETY: `line` is null or a buffer `getdelim` made, with its
        // capacity; the stream is open (and locked by this thread, which
        // `getdelim`'s own lock allows).
        let n = unsafe { getdelim(&mut line.ptr, &mut line.cap, c_int::from(b'\n'), f) };
        if n <= 0 {
            return Ok(false);
        }
        // SAFETY: `getdelim` wrote `n` bytes there.
        let got = unsafe { std::slice::from_raw_parts(line.ptr.cast::<u8>(), n as usize) };
        let got = match got.split_last() {
            Some((b'\n', rest)) if !keep => rest,
            _ => got,
        };
        into.try_reserve(got.len())
            .map_err(|_| io::Error::from(io::ErrorKind::OutOfMemory))?;
        into.extend_from_slice(got);
        Ok(true)
    }

    #[cfg(windows)]
    pub(crate) fn read_line(&mut self, keep: bool, into: &mut Vec<u8>) -> io::Result<bool> {
        let mut any = false;
        while let Some(c) = self.getc() {
            any = true;
            if c == b'\n' && !keep {
                break;
            }
            if into.len() == into.capacity() && into.try_reserve(into.len().max(64)).is_err() {
                return Err(io::Error::from(io::ErrorKind::OutOfMemory));
            }
            into.push(c);
            if c == b'\n' {
                break;
            }
        }
        Ok(any)
    }

    /// The next byte, or `None` at the end (or on an error).
    pub(crate) fn getc(&mut self) -> Option<u8> {
        // SAFETY: the stream is open and locked.
        let c = unsafe { getc_nolock(self.raw()) };
        u8::try_from(c).ok()
    }

    /// Push `c` back (`ungetc`); one byte of push-back always works.
    pub(crate) fn ungetc(&mut self, c: u8) {
        // SAFETY: the stream is open and locked.
        unsafe { ungetc_nolock(c, self.raw()) };
    }

    /// Whether an error happened (`ferror`), as the error `errno` holds.
    pub(crate) fn error(&mut self) -> Option<io::Error> {
        // SAFETY: the stream is open and locked.
        if unsafe { ferror_nolock(self.raw()) } {
            Some(last_error())
        } else {
            None
        }
    }

    /// Forget an earlier error or end of file (`clearerr`).
    pub(crate) fn clear_error(&mut self) {
        clear_errno();
        // SAFETY: the stream is open and locked.
        unsafe { clearerr_nolock(self.raw()) };
    }
}

// The calls `Locked` makes. Each is unsafe: `f` must be an open stream
// this thread holds the lock of.

#[cfg(target_os = "linux")]
extern "C" {
    fn fread_unlocked(p: *mut c_void, size: usize, n: usize, f: *mut FILE) -> usize;
    fn fwrite_unlocked(p: *const c_void, size: usize, n: usize, f: *mut FILE) -> usize;
    fn ferror_unlocked(f: *mut FILE) -> c_int;
    fn clearerr_unlocked(f: *mut FILE);
}

#[cfg(windows)]
extern "C" {
    fn _fread_nolock(p: *mut c_void, size: usize, n: usize, f: *mut FILE) -> usize;
    fn _fwrite_nolock(p: *const c_void, size: usize, n: usize, f: *mut FILE) -> usize;
    fn _ungetc_nolock(c: c_int, f: *mut FILE) -> c_int;
}

unsafe fn lock(f: *mut FILE) {
    #[cfg(unix)]
    // SAFETY: as for every call here.
    unsafe {
        flockfile(f)
    };
    #[cfg(windows)]
    // SAFETY: as for every call here.
    unsafe {
        _lock_file(f)
    };
}

unsafe fn unlock(f: *mut FILE) {
    #[cfg(unix)]
    // SAFETY: as for every call here.
    unsafe {
        funlockfile(f)
    };
    #[cfg(windows)]
    // SAFETY: as for every call here.
    unsafe {
        _unlock_file(f)
    };
}

unsafe fn fread_nolock(p: *mut c_void, n: usize, f: *mut FILE) -> usize {
    // SAFETY: `p` is writable for `n` bytes; as for every call here.
    unsafe {
        #[cfg(target_os = "linux")]
        return fread_unlocked(p, 1, n, f);
        #[cfg(windows)]
        return _fread_nolock(p, 1, n, f);
        #[cfg(not(any(target_os = "linux", windows)))]
        return libc::fread(p, 1, n, f);
    }
}

unsafe fn fwrite_nolock(buf: &[u8], f: *mut FILE) -> usize {
    let (p, n) = (buf.as_ptr().cast(), buf.len());
    // SAFETY: `buf` is readable for its length; as for every call here.
    unsafe {
        #[cfg(target_os = "linux")]
        return fwrite_unlocked(p, 1, n, f);
        #[cfg(windows)]
        return _fwrite_nolock(p, 1, n, f);
        #[cfg(not(any(target_os = "linux", windows)))]
        return libc::fwrite(p, 1, n, f);
    }
}

unsafe fn getc_nolock(f: *mut FILE) -> c_int {
    // SAFETY: as for every call here.
    unsafe {
        #[cfg(unix)]
        return getc_unlocked(f);
        #[cfg(windows)]
        return _fgetc_nolock(f);
    }
}

unsafe fn ungetc_nolock(c: u8, f: *mut FILE) {
    // SAFETY: as for every call here.
    unsafe {
        #[cfg(unix)]
        libc::ungetc(c_int::from(c), f);
        #[cfg(windows)]
        _ungetc_nolock(c_int::from(c), f);
    }
}

unsafe fn ferror_nolock(f: *mut FILE) -> bool {
    // SAFETY: as for every call here.
    unsafe {
        #[cfg(target_os = "linux")]
        return ferror_unlocked(f) != 0;
        #[cfg(not(target_os = "linux"))]
        return libc::ferror(f) != 0;
    }
}

unsafe fn clearerr_nolock(f: *mut FILE) {
    // SAFETY: as for every call here.
    unsafe {
        #[cfg(target_os = "linux")]
        clearerr_unlocked(f);
        #[cfg(windows)]
        clearerr(f);
        #[cfg(not(any(target_os = "linux", windows)))]
        libc::clearerr(f);
    }
}

// ---- luaL_Stream blocks ------------------------------------------------------

/// The size of a `luaL_Stream`: `FILE *f` then `lua_CFunction closef`.
pub(crate) const STREAM_BLOCK_SIZE: usize = 2 * std::mem::size_of::<usize>();

/// Fill in a `luaL_Stream` block: `f` and `closef` (0 for a closed
/// stream). `block` is the memory of a userdata at least
/// [`STREAM_BLOCK_SIZE`] bytes long, aligned to 16, that the caller holds.
pub(crate) fn write_stream_block(block: *mut c_void, f: usize, closef: usize) {
    let p = block.cast::<usize>();
    // SAFETY: as documented, `block` holds two aligned words.
    unsafe {
        p.write(f);
        p.add(1).write(closef);
    }
}

/// A `luaL_Stream` block's `f` and `closef`, under the same conditions as
/// [`write_stream_block`].
pub(crate) fn read_stream_block(block: *mut c_void) -> (usize, usize) {
    let p = block.cast::<usize>();
    // SAFETY: as documented, `block` holds two aligned words.
    unsafe { (p.read(), p.add(1).read()) }
}

// ---- standard output as the isolate's writer ---------------------------------

/// The C library's `stdout` as a writer: the isolate's standard output
/// unless the host redirects it, so `print`, `io.write` and C's `printf`
/// share one buffer.
#[derive(Debug)]
pub(crate) struct CStdout;

impl io::Write for CStdout {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let Some(mut f) = CFile::standard(Std::Out) else {
            return Err(crate::crt::system_error(libc::EBADF));
        };
        f.write(buf).map(|()| buf.len())
    }

    fn write_all(&mut self, buf: &[u8]) -> io::Result<()> {
        self.write(buf).map(|_| ())
    }

    fn flush(&mut self) -> io::Result<()> {
        match CFile::standard(Std::Out) {
            Some(mut f) => f.flush(),
            None => Ok(()),
        }
    }
}

/// What an open handle's `closef` holds while no C code can run: a C
/// function that closes nothing and returns no results, never called.
extern "C" fn no_closef(_: *mut c_void) -> c_int {
    0
}

/// The address of [`no_closef`].
pub(crate) fn no_closef_address() -> usize {
    no_closef as *const () as usize
}

/// A C mode string for `fdopen`, from bytes without a NUL.
pub(crate) fn mode(m: &'static [u8]) -> &'static CStr {
    CStr::from_bytes_with_nul(m).expect("mode ends in NUL")
}
