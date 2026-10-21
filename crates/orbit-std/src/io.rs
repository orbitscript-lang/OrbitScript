//! The `io` library: file handles as `FILE*` userdata.
//!
//! A handle owns a [`Stream`]: a file, a standard stream or a child process
//! from `io.popen`, with our own read and write buffers so reads and writes
//! can interleave, `setvbuf` means what it says, and `read("n")` can look one
//! byte ahead. Writes to standard output go through the isolate's standard
//! output, which `print` also writes to, so the two keep their order.

use std::cell::RefCell;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::process::{Child, Stdio};
use std::rc::{Rc, Weak};

use orbit::stack::{Any, Stack};
use orbit::{Call, Class, Error, Pinned, Ret, Scope, Table, Userdata, Value, native};

use crate::sys::{exit_status, file_error, flush_stdout, os_error_text, os_path, shell};

/// The C library's `BUFSIZ`, the default buffer size.
const BUFSIZ: usize = 8192;

/// The most formats `lines` takes, as in the reference.
const MAX_LINE_FORMATS: usize = 250;

/// The longest numeral `read("n")` reads.
const MAX_NUMERAL: usize = 200;

/// The handles' type name, also the registry key of their metatable.
const FILE_TYPE: &str = "FILE*";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Buffering {
    No,
    Line(usize),
    Full(usize),
}

#[derive(Debug)]
enum Kind {
    File(File),
    Stdin,
    Stdout,
    Stderr,
    /// `io.popen(cmd, "r")`: we read the child's output.
    PipeRead(Child),
    /// `io.popen(cmd, "w")`: we write the child's input.
    PipeWrite(Child),
    Closed,
}

/// An open stream and its buffers.
#[derive(Debug)]
struct Stream {
    kind: Kind,
    readable: bool,
    writable: bool,
    rbuf: Vec<u8>,
    rpos: usize,
    wbuf: Vec<u8>,
    buffering: Buffering,
}

type Shared = Rc<RefCell<Stream>>;

/// A `FILE*` userdata's payload. Shared, so `os.exit` can flush every open
/// stream ([`flush_all`]).
#[derive(Debug, Clone)]
struct LuaFile(Shared);

thread_local! {
    static OPEN: RefCell<Vec<Weak<RefCell<Stream>>>> = const { RefCell::new(Vec::new()) };
}

/// Flush every open file's buffer, as the C library does at exit. Standard
/// output's buffer is the isolate's, which flushes it itself.
pub fn flush_all() {
    OPEN.with(|open| {
        for s in open.borrow().iter().filter_map(Weak::upgrade) {
            if let Ok(mut s) = s.try_borrow_mut() {
                let _ = s.flush(&mut io::sink());
            }
        }
    });
}

fn bad_fd() -> io::Error {
    #[cfg(unix)]
    {
        io::Error::from_raw_os_error(9)
    }
    #[cfg(not(unix))]
    {
        io::Error::other("Bad file descriptor")
    }
}

fn illegal_seek() -> io::Error {
    #[cfg(unix)]
    {
        io::Error::from_raw_os_error(29)
    }
    #[cfg(not(unix))]
    {
        io::Error::other("Illegal seek")
    }
}

impl Stream {
    fn new(kind: Kind, readable: bool, writable: bool) -> Stream {
        Stream {
            kind,
            readable,
            writable,
            rbuf: Vec::new(),
            rpos: 0,
            wbuf: Vec::new(),
            buffering: Buffering::Full(BUFSIZ),
        }
    }

    fn is_closed(&self) -> bool {
        matches!(self.kind, Kind::Closed)
    }

    fn is_std(&self) -> bool {
        matches!(self.kind, Kind::Stdin | Kind::Stdout | Kind::Stderr)
    }

    /// Write the write buffer out.
    fn flush(&mut self, out: &mut dyn Write) -> io::Result<()> {
        if self.wbuf.is_empty() {
            return match self.kind {
                Kind::Stdout => out.flush(),
                _ => Ok(()),
            };
        }
        let buf = std::mem::take(&mut self.wbuf);
        match &mut self.kind {
            Kind::File(f) => f.write_all(&buf),
            Kind::Stdout => out.write_all(&buf).and_then(|()| out.flush()),
            Kind::Stderr => io::stderr().write_all(&buf),
            Kind::PipeWrite(c) => match c.stdin.as_mut() {
                Some(w) => w.write_all(&buf),
                None => Err(bad_fd()),
            },
            Kind::Stdin | Kind::PipeRead(_) | Kind::Closed => Err(bad_fd()),
        }
    }

    /// Give back read-ahead bytes before writing or seeking: a file's
    /// position moves back over them.
    fn unread(&mut self) -> io::Result<()> {
        let n = self.rbuf.len() - self.rpos;
        self.rbuf.clear();
        self.rpos = 0;
        if n > 0
            && let Kind::File(f) = &mut self.kind
        {
            f.seek(SeekFrom::Current(-(n as i64)))?;
        }
        Ok(())
    }

    fn write(&mut self, bytes: &[u8], out: &mut dyn Write) -> io::Result<()> {
        if !self.writable {
            return Err(bad_fd());
        }
        if self.rpos < self.rbuf.len() {
            self.unread()?;
        }
        if let Kind::Stdout = self.kind {
            // The isolate's standard output does the buffering.
            return out.write_all(bytes);
        }
        self.wbuf.extend_from_slice(bytes);
        match self.buffering {
            Buffering::No => self.flush(out),
            Buffering::Line(_) if bytes.contains(&b'\n') => self.flush(out),
            Buffering::Full(n) | Buffering::Line(n) if self.wbuf.len() >= n.max(1) => {
                self.flush(out)
            }
            _ => Ok(()),
        }
    }

    /// Make sure there is a byte to read, unless at the end. Returns
    /// whether there is.
    fn fill(&mut self, out: &mut dyn Write) -> io::Result<bool> {
        if self.rpos < self.rbuf.len() {
            return Ok(true);
        }
        if !self.readable {
            return Err(bad_fd());
        }
        if !self.wbuf.is_empty() {
            self.flush(out)?;
        }
        if let Kind::Stdin = self.kind {
            // Input from a terminal should see the prompt first.
            let _ = out.flush();
        }
        self.rbuf.resize(BUFSIZ, 0);
        self.rpos = 0;
        let r = loop {
            let r = match &mut self.kind {
                Kind::File(f) => f.read(&mut self.rbuf),
                Kind::Stdin => io::stdin().read(&mut self.rbuf),
                Kind::PipeRead(c) => match c.stdout.as_mut() {
                    Some(r) => r.read(&mut self.rbuf),
                    None => Err(bad_fd()),
                },
                _ => Err(bad_fd()),
            };
            match r {
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                r => break r,
            }
        };
        match r {
            Ok(n) => {
                self.rbuf.truncate(n);
                Ok(n > 0)
            }
            Err(e) => {
                self.rbuf.clear();
                Err(e)
            }
        }
    }

    fn peek(&mut self, out: &mut dyn Write) -> io::Result<Option<u8>> {
        Ok(if self.fill(out)? {
            Some(self.rbuf[self.rpos])
        } else {
            None
        })
    }

    /// Read up to `n` bytes into `into`.
    fn read_bytes(
        &mut self,
        mut n: usize,
        into: &mut Vec<u8>,
        out: &mut dyn Write,
    ) -> io::Result<()> {
        while n > 0 && self.fill(out)? {
            let avail = &self.rbuf[self.rpos..];
            let k = avail.len().min(n);
            append(into, &avail[..k])?;
            self.rpos += k;
            n -= k;
        }
        Ok(())
    }

    /// Read through the next newline (kept in `into` if `keep`). Returns
    /// whether anything was read, newline included.
    fn read_line(
        &mut self,
        keep: bool,
        into: &mut Vec<u8>,
        out: &mut dyn Write,
    ) -> io::Result<bool> {
        let mut any = false;
        while self.fill(out)? {
            any = true;
            let avail = &self.rbuf[self.rpos..];
            match avail.iter().position(|&b| b == b'\n') {
                Some(i) => {
                    append(into, &avail[..if keep { i + 1 } else { i }])?;
                    self.rpos += i + 1;
                    return Ok(true);
                }
                None => {
                    append(into, avail)?;
                    self.rpos = self.rbuf.len();
                }
            }
        }
        Ok(any)
    }

    fn read_all(&mut self, into: &mut Vec<u8>, out: &mut dyn Write) -> io::Result<()> {
        while self.fill(out)? {
            append(into, &self.rbuf[self.rpos..])?;
            self.rpos = self.rbuf.len();
        }
        Ok(())
    }

    /// Read a numeral as `read("n")` does: the longest prefix that can
    /// start a numeral, at most [`MAX_NUMERAL`] bytes, looking one byte
    /// ahead. Returns the bytes, or `None` if they ran too long.
    fn read_numeral(&mut self, out: &mut dyn Write) -> io::Result<Option<Vec<u8>>> {
        let mut buf = Vec::new();
        let mut too_long = false;
        while let Some(c) = self.peek(out)? {
            if !matches!(c, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c) {
                break;
            }
            self.rpos += 1;
        }
        // Take the next byte if `accept` likes it.
        let mut take =
            |s: &mut Stream, buf: &mut Vec<u8>, accept: &dyn Fn(u8) -> bool| -> io::Result<bool> {
                match s.peek(out)? {
                    Some(c) if accept(c) && !too_long => {
                        if buf.len() >= MAX_NUMERAL {
                            too_long = true;
                            return Ok(false);
                        }
                        buf.push(c);
                        s.rpos += 1;
                        Ok(true)
                    }
                    _ => Ok(false),
                }
            };
        take(self, &mut buf, &|c| c == b'-' || c == b'+')?;
        let mut count = 0;
        let mut hex = false;
        if take(self, &mut buf, &|c| c == b'0')? {
            if take(self, &mut buf, &|c| c == b'x' || c == b'X')? {
                hex = true;
            } else {
                count = 1;
            }
        }
        let digit = move |c: u8| {
            if hex {
                c.is_ascii_hexdigit()
            } else {
                c.is_ascii_digit()
            }
        };
        while take(self, &mut buf, &digit)? {
            count += 1;
        }
        if take(self, &mut buf, &|c| c == b'.')? {
            while take(self, &mut buf, &digit)? {
                count += 1;
            }
        }
        if count > 0 {
            let exp = move |c: u8| {
                if hex {
                    c == b'p' || c == b'P'
                } else {
                    c == b'e' || c == b'E'
                }
            };
            if take(self, &mut buf, &exp)? {
                take(self, &mut buf, &|c| c == b'-' || c == b'+')?;
                while take(self, &mut buf, &|c: u8| c.is_ascii_digit())? {}
            }
        }
        Ok(if too_long { None } else { Some(buf) })
    }

    fn seek(&mut self, from: SeekFrom, out: &mut dyn Write) -> io::Result<u64> {
        self.flush(out)?;
        match &mut self.kind {
            Kind::File(_) => {
                self.unread()?;
                let Kind::File(f) = &mut self.kind else {
                    unreachable!()
                };
                f.seek(from)
            }
            _ => Err(illegal_seek()),
        }
    }

    /// Flush and close. Returns the child's status for a pipe.
    fn close(&mut self, out: &mut dyn Write) -> io::Result<Option<std::process::ExitStatus>> {
        let flushed = self.flush(out);
        let kind = std::mem::replace(&mut self.kind, Kind::Closed);
        self.rbuf.clear();
        self.rpos = 0;
        let status = match kind {
            Kind::PipeRead(mut c) | Kind::PipeWrite(mut c) => {
                // Closing our end first lets a writer or reader finish.
                drop(c.stdin.take());
                drop(c.stdout.take());
                Some(c.wait()?)
            }
            Kind::File(f) => {
                drop(f);
                None
            }
            _ => None,
        };
        flushed?;
        Ok(status)
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        // A collected handle is closed, as `__gc` does in the reference.
        // Standard output's buffer belongs to the isolate, which flushes it.
        if !matches!(self.kind, Kind::Stdout | Kind::Closed) {
            let _ = self.close(&mut io::sink());
        }
    }
}

// ---- handles ---------------------------------------------------------------

/// A default file (`io.input`, `io.output`): the handle, and its stream.
struct Default {
    file: Pinned<Userdata<'static>>,
    stream: Shared,
}

/// The library's state, which its functions share.
struct Io {
    class: Class<LuaFile>,
    input: Default,
    output: Default,
}

type State = Rc<RefCell<Io>>;

/// A new `FILE*` handle for `stream`.
fn new_file<'s>(
    s: &'s Scope<'s>,
    class: &Class<LuaFile>,
    stream: Stream,
) -> Result<(Userdata<'s>, Shared), Error> {
    let rc = Rc::new(RefCell::new(stream));
    OPEN.with(|open| {
        let mut open = open.borrow_mut();
        open.retain(|w| w.strong_count() > 0);
        open.push(Rc::downgrade(&rc));
    });
    let u = class.instance(s, LuaFile(rc.clone()))?;
    Ok((u, rc))
}

/// The stream of a `FILE*` value.
fn stream_of(v: Value<'_>) -> Option<Shared> {
    let u = v.as_userdata()?;
    let f = u.borrow::<LuaFile>().ok()?;
    Some(f.0.clone())
}

/// The stream of a `FILE*` argument, open or closed.
fn stream_arg<'s>(c: &'s Call<'s>, i: usize) -> Result<Shared, Error> {
    stream_of(c.arg(i)).ok_or_else(|| c.type_error(i, FILE_TYPE))
}

/// The stream of an open `FILE*` argument.
fn open_arg<'s>(c: &'s Call<'s>, i: usize) -> Result<Shared, Error> {
    let s = stream_arg(c, i)?;
    if s.borrow().is_closed() {
        return Err(c.error("attempt to use a closed file"));
    }
    Ok(s)
}

/// The default input or output stream, which must be open.
fn default_file<'s>(
    c: &'s Call<'s>,
    state: &State,
    input: bool,
) -> Result<(Userdata<'s>, Shared), Error> {
    let st = state.borrow();
    let d = if input { &st.input } else { &st.output };
    if d.stream.borrow().is_closed() {
        let what = if input { "input" } else { "output" };
        return Err(c.error(format!("default {what} file is closed")));
    }
    Ok((d.file.get(c)?, d.stream.clone()))
}

/// `io.open`'s mode: `[rwa]%+?b*`.
fn open_options(mode: &[u8]) -> Option<(OpenOptions, bool, bool)> {
    let (&first, rest) = mode.split_first()?;
    let (plus, rest) = match rest.first() {
        Some(b'+') => (true, &rest[1..]),
        _ => (false, rest),
    };
    if !rest.iter().all(|&b| b == b'b') {
        return None;
    }
    let mut o = OpenOptions::new();
    let (r, w) = match first {
        b'r' => {
            o.read(true).write(plus);
            (true, plus)
        }
        b'w' => {
            o.write(true).create(true).truncate(true).read(plus);
            (plus, true)
        }
        b'a' => {
            o.append(true).create(true).read(plus);
            (plus, true)
        }
        _ => return None,
    };
    Some((o, r, w))
}

fn open_file(name: &[u8], mode: &[u8]) -> Option<io::Result<Stream>> {
    let (o, r, w) = open_options(mode)?;
    Some(
        o.open(os_path(name))
            .map(|f| Stream::new(Kind::File(f), r, w)),
    )
}

/// The message `io.lines` and `io.input` raise for a file they can't open.
fn cannot_open(name: &[u8], e: &io::Error) -> String {
    format!(
        "cannot open file '{}' ({})",
        String::from_utf8_lossy(name),
        os_error_text(e)
    )
}

/// Argument `i` as a string, or `default` when nil or absent.
fn opt_bytes<'s>(c: &'s Call<'s>, i: usize, default: &[u8]) -> Result<Vec<u8>, Error> {
    Ok(c.opt::<Option<Vec<u8>>>(i, None)?
        .unwrap_or_else(|| default.to_vec()))
}

// ---- library functions -------------------------------------------------------

fn io_open<'s>(c: &'s Call<'s>, state: &State) -> Result<Ret, Error> {
    let name: Vec<u8> = c.check(0)?;
    let mode = opt_bytes(c, 1, b"r")?;
    match open_file(&name, &mode) {
        None => Err(c.arg_error(1, "invalid mode")),
        Some(Ok(stream)) => {
            let (f, _) = new_file(c, &state.borrow().class, stream)?;
            c.ret(f)
        }
        Some(Err(e)) => file_error(c, &e, Some(&name)),
    }
}

fn io_popen<'s>(c: &'s Call<'s>, state: &State) -> Result<Ret, Error> {
    let cmd: Vec<u8> = c.check(0)?;
    let mode = opt_bytes(c, 1, b"r")?;
    let reading = match &mode[..] {
        b"r" => true,
        b"w" => false,
        _ => return Err(c.arg_error(1, "invalid mode")),
    };
    // The child writes to the same terminal; keep our output first.
    flush_stdout(c);
    let mut command = shell(os_path(&cmd).as_os_str());
    if reading {
        command.stdout(Stdio::piped());
    } else {
        command.stdin(Stdio::piped());
    }
    match command.spawn() {
        Ok(child) => {
            let stream = if reading {
                Stream::new(Kind::PipeRead(child), true, false)
            } else {
                Stream::new(Kind::PipeWrite(child), false, true)
            };
            let (f, _) = new_file(c, &state.borrow().class, stream)?;
            c.ret(f)
        }
        Err(e) => file_error(c, &e, Some(&cmd)),
    }
}

fn io_tmpfile<'s>(c: &'s Call<'s>, state: &State) -> Result<Ret, Error> {
    // Removed at once: the open handle keeps the file until it is closed.
    match crate::sys::temp_file() {
        Ok((path, _)) => {
            let opened = OpenOptions::new().read(true).write(true).open(&path);
            let _ = std::fs::remove_file(&path);
            match opened {
                Ok(f) => {
                    let stream = Stream::new(Kind::File(f), true, true);
                    let (f, _) = new_file(c, &state.borrow().class, stream)?;
                    c.ret(f)
                }
                Err(e) => file_error(c, &e, None),
            }
        }
        Err(e) => file_error(c, &e, None),
    }
}

#[native(name = "io.type")]
fn io_type(s: &mut Stack<'_>, v: Any) -> Option<&'static str> {
    // A file whose method is running reads as not a file, as before.
    let closed = s.with_userdata::<LuaFile, _>(v.0, |f| Some(f.as_ref()?.0.borrow().is_closed()));
    match closed.flatten()? {
        true => Some("closed file"),
        false => Some("file"),
    }
}

/// `io.input` and `io.output`.
fn set_default<'s>(c: &'s Call<'s>, state: &State, input: bool) -> Result<Ret, Error> {
    let new = match c.arg(0) {
        Value::Nil => None,
        Value::Str(_) | Value::Int(_) | Value::Float(_) => {
            let name: Vec<u8> = c.check(0)?;
            let mode: &[u8] = if input { b"r" } else { b"w" };
            match open_file(&name, mode).expect("valid mode") {
                Ok(stream) => Some(new_file(c, &state.borrow().class, stream)?),
                Err(e) => return Err(c.error(cannot_open(&name, &e))),
            }
        }
        v => {
            let s = open_arg(c, 0)?;
            Some((v.as_userdata().expect("a file is userdata"), s))
        }
    };
    if let Some((file, stream)) = new {
        let d = Default {
            file: file.pin(),
            stream,
        };
        let mut st = state.borrow_mut();
        if input {
            st.input = d;
        } else {
            st.output = d;
        }
    }
    let st = state.borrow();
    let d = if input { &st.input } else { &st.output };
    c.ret(d.file.get(c)?)
}

fn io_close<'s>(c: &'s Call<'s>, state: &State) -> Result<Ret, Error> {
    if c.arg(0).is_nil() {
        let (_, s) = default_file(c, state, false)?;
        return close(c, &s);
    }
    f_close(c)
}

fn f_close<'s>(c: &'s Call<'s>) -> Result<Ret, Error> {
    let s = open_arg(c, 0)?;
    close(c, &s)
}

fn close<'s>(c: &'s Call<'s>, s: &Shared) -> Result<Ret, Error> {
    if s.borrow().is_std() {
        return c.ret((Value::Nil, "cannot close standard file"));
    }
    let r = c.with_stdout(|out| s.borrow_mut().close(out));
    match r {
        Ok(Some(status)) => exit_status(c, status),
        Ok(None) => c.ret(true),
        Err(e) => file_error(c, &e, None),
    }
}

fn io_flush<'s>(c: &'s Call<'s>, state: &State) -> Result<Ret, Error> {
    let (f, s) = default_file(c, state, false)?;
    flush_result(c, &s, f)
}

fn f_flush<'s>(c: &'s Call<'s>) -> Result<Ret, Error> {
    let s = open_arg(c, 0)?;
    flush_result(c, &s, c.arg(0))
}

fn flush_result<'s>(
    c: &'s Call<'s>,
    s: &Shared,
    file: impl orbit::IntoValue<'s>,
) -> Result<Ret, Error> {
    let r = c.with_stdout(|out| s.borrow_mut().flush(out));
    match r {
        Ok(()) => c.ret(file),
        Err(e) => file_error(c, &e, None),
    }
}

fn io_write<'s>(c: &'s Call<'s>, state: &State) -> Result<Ret, Error> {
    let (f, s) = default_file(c, state, false)?;
    write(c, &s, f.to_value(), 0)
}

fn f_write<'s>(c: &'s Call<'s>) -> Result<Ret, Error> {
    let s = open_arg(c, 0)?;
    write(c, &s, c.arg(0), 1)
}

/// Write arguments `first..` (strings or numbers) to `s`. Returns the file,
/// or `nil, message, code, bytes written`.
fn write<'s>(c: &'s Call<'s>, s: &Shared, file: Value<'s>, first: usize) -> Result<Ret, Error> {
    let mut written = 0usize;
    let mut bytes = Vec::new();
    for i in first..c.nargs() {
        bytes.clear();
        if !c.arg(i).write_text(&mut bytes) {
            return Err(c.type_error(i, "string"));
        }
        let r = c.with_stdout(|out| s.borrow_mut().write(&bytes, out));
        if let Err(e) = r {
            let msg = os_error_text(&e);
            let code = i64::from(e.raw_os_error().unwrap_or(0));
            return c.ret((Value::Nil, msg, code, written as i64));
        }
        written += bytes.len();
    }
    c.ret(file)
}

fn f_seek<'s>(c: &'s Call<'s>) -> Result<Ret, Error> {
    let s = open_arg(c, 0)?;
    let whence = opt_bytes(c, 1, b"cur")?;
    let off: i64 = c.opt(2, 0)?;
    let from = match &whence[..] {
        b"set" => {
            if off < 0 {
                return file_error(c, &io::Error::from(io::ErrorKind::InvalidInput), None);
            }
            SeekFrom::Start(off as u64)
        }
        b"cur" => SeekFrom::Current(off),
        b"end" => SeekFrom::End(off),
        _ => {
            let msg = format!("invalid option '{}'", String::from_utf8_lossy(&whence));
            return Err(c.arg_error(1, msg));
        }
    };
    let r = c.with_stdout(|out| s.borrow_mut().seek(from, out));
    match r {
        Ok(p) => c.ret(p as i64),
        Err(e) => file_error(c, &e, None),
    }
}

fn f_setvbuf<'s>(c: &'s Call<'s>) -> Result<Ret, Error> {
    let s = open_arg(c, 0)?;
    let mode: Vec<u8> = c.check(1)?;
    let size = c.opt::<i64>(2, BUFSIZ as i64)?.max(0) as usize;
    let b = match &mode[..] {
        b"no" => Buffering::No,
        b"line" => Buffering::Line(size),
        b"full" => Buffering::Full(size),
        _ => {
            let msg = format!("invalid option '{}'", String::from_utf8_lossy(&mode));
            return Err(c.arg_error(1, msg));
        }
    };
    let r = c.with_stdout(|out| {
        let mut st = s.borrow_mut();
        let r = st.flush(out);
        st.buffering = b;
        r
    });
    match r {
        Ok(()) => c.ret(true),
        Err(e) => file_error(c, &e, None),
    }
}

// ---- reading ---------------------------------------------------------------

fn io_read<'s>(c: &'s Call<'s>, state: &State) -> Result<Ret, Error> {
    let (_, s) = default_file(c, state, true)?;
    let fmts: Vec<FormatArg> = (0..c.nargs()).map(|i| FormatArg::of(c.arg(i))).collect();
    read(c, &s, &fmts, 0)
}

fn f_read<'s>(c: &'s Call<'s>) -> Result<Ret, Error> {
    let s = open_arg(c, 0)?;
    let fmts: Vec<FormatArg> = (1..c.nargs()).map(|i| FormatArg::of(c.arg(i))).collect();
    read(c, &s, &fmts, 1)
}

/// `read`: the values, or `nil, message, code` on an I/O error.
fn read<'s>(c: &'s Call<'s>, s: &Shared, fmts: &[FormatArg], arg: usize) -> Result<Ret, Error> {
    match read_values(c, s, fmts, arg)? {
        Ok(values) => c.ret(values),
        Err(e) => file_error(c, &e, None),
    }
}

/// One `read` format.
#[derive(Clone, Copy)]
enum Format {
    Count(usize),
    Number,
    Line { keep: bool },
    All,
}

/// A `read` format argument, kept by a `lines` iterator until it reads.
#[derive(Clone)]
enum FormatArg {
    Int(i64),
    Float(f64),
    Str(Vec<u8>),
    /// Anything else, which is no format.
    Other,
}

impl FormatArg {
    fn of(v: Value<'_>) -> FormatArg {
        match v {
            Value::Int(n) => FormatArg::Int(n),
            Value::Float(f) => FormatArg::Float(f),
            Value::Str(s) => FormatArg::Str(s.to_bytes()),
            _ => FormatArg::Other,
        }
    }
}

fn format<'s>(c: &'s Call<'s>, v: &FormatArg, arg: usize) -> Result<Format, Error> {
    let count = |n: i64| Format::Count(usize::try_from(n).unwrap_or(0));
    match *v {
        FormatArg::Int(n) => Ok(count(n)),
        FormatArg::Float(f) if f.fract() == 0.0 && (-9.3e18..9.3e18).contains(&f) => {
            Ok(count(f as i64))
        }
        FormatArg::Float(_) => Err(c.arg_error(arg, "number has no integer representation")),
        FormatArg::Str(ref b) => {
            let b = b.strip_prefix(b"*").unwrap_or(b);
            match b.first() {
                Some(b'n') => Ok(Format::Number),
                Some(b'l') => Ok(Format::Line { keep: false }),
                Some(b'L') => Ok(Format::Line { keep: true }),
                Some(b'a') => Ok(Format::All),
                _ => Err(c.arg_error(arg, "invalid format")),
            }
        }
        FormatArg::Other => Err(c.type_error(arg, "string")),
    }
}

/// Append `bytes` to a read buffer. A buffer that can't grow fails with
/// [`io::ErrorKind::OutOfMemory`], which `read` raises as Lua's
/// "not enough memory".
fn append(into: &mut Vec<u8>, bytes: &[u8]) -> io::Result<()> {
    into.try_reserve(bytes.len())
        .map_err(|_| io::Error::from(io::ErrorKind::OutOfMemory))?;
    into.extend_from_slice(bytes);
    Ok(())
}

/// Read with formats `fmts` (none: one line) from `s`: one value per
/// format, stopping after the first that fails (nil). `arg` is the
/// argument index of the first format, for messages. As in lua5.5, each
/// format is checked when its turn comes, so formats after a failed read
/// are never looked at.
fn read_values<'s>(
    c: &'s Call<'s>,
    s: &Shared,
    fmts: &[FormatArg],
    arg: usize,
) -> Result<io::Result<Vec<Value<'s>>>, Error> {
    let n = fmts.len().max(1);
    let mut values = Vec::with_capacity(n);
    for i in 0..n {
        let fmt = match fmts.get(i) {
            Some(f) => format(c, f, arg + i)?,
            None => Format::Line { keep: false },
        };
        match read_one(c, s, fmt) {
            Ok(v) => {
                let failed = v.is_nil();
                values.push(v);
                if failed {
                    break;
                }
            }
            Err(e) if e.kind() == io::ErrorKind::OutOfMemory => return Err(Error::memory()),
            Err(e) => return Ok(Err(e)),
        }
    }
    Ok(Ok(values))
}

/// What one format read.
enum ReadOutcome {
    Bytes(Vec<u8>),
    Numeral(Option<Vec<u8>>),
    Nothing,
}

/// One format's value, or nil if it fails.
fn read_one<'s>(c: &'s Call<'s>, s: &Shared, fmt: Format) -> io::Result<Value<'s>> {
    let read = c.with_stdout(|out| -> io::Result<ReadOutcome> {
        let mut st = s.borrow_mut();
        let mut buf = Vec::new();
        let ok = match fmt {
            Format::Count(0) => st.fill(out)?,
            Format::Count(n) => {
                st.read_bytes(n, &mut buf, out)?;
                !buf.is_empty()
            }
            Format::Line { keep } => st.read_line(keep, &mut buf, out)?,
            Format::All => {
                st.read_all(&mut buf, out)?;
                true
            }
            Format::Number => return Ok(ReadOutcome::Numeral(st.read_numeral(out)?)),
        };
        Ok(if ok {
            ReadOutcome::Bytes(buf)
        } else {
            ReadOutcome::Nothing
        })
    })?;
    Ok(match read {
        ReadOutcome::Bytes(b) => c.create_string(b).to_value(),
        ReadOutcome::Numeral(digits) => digits
            .and_then(|d| c.parse_number(&d))
            .unwrap_or(Value::Nil),
        ReadOutcome::Nothing => Value::Nil,
    })
}

// ---- lines -----------------------------------------------------------------

fn io_lines<'s>(c: &'s Call<'s>, state: &State) -> Result<Ret, Error> {
    let (file, stream, close) = if c.arg(0).is_nil() {
        let (f, s) = default_file(c, state, true)?;
        (f, s, false)
    } else {
        let name: Vec<u8> = c.check(0)?;
        match open_file(&name, b"r").expect("valid mode") {
            Ok(stream) => {
                let (f, s) = new_file(c, &state.borrow().class, stream)?;
                (f, s, true)
            }
            Err(e) => return Err(c.error(cannot_open(&name, &e))),
        }
    };
    let iter = lines_iterator(c, file, stream, 1, close)?;
    let to_close = if close { file.to_value() } else { Value::Nil };
    c.ret((iter, Value::Nil, Value::Nil, to_close))
}

fn f_lines<'s>(c: &'s Call<'s>) -> Result<Ret, Error> {
    let stream = open_arg(c, 0)?;
    let file = c.arg(0).as_userdata().expect("checked");
    let iter = lines_iterator(c, file, stream, 1, false)?;
    c.ret(iter)
}

/// A function that reads `file` with the formats in arguments `first..`,
/// closing the file at the end when `close`.
fn lines_iterator<'s>(
    c: &'s Call<'s>,
    file: Userdata<'s>,
    stream: Shared,
    first: usize,
    close: bool,
) -> Result<orbit::Function<'s>, Error> {
    let n = c.nargs().saturating_sub(first);
    if n > MAX_LINE_FORMATS {
        return Err(c.arg_error(first + MAX_LINE_FORMATS, "too many arguments"));
    }
    let fmts: Vec<FormatArg> = (0..n).map(|i| FormatArg::of(c.arg(first + i))).collect();
    // Keeps the handle alive while the iterator lives, as Lua's upvalue
    // does.
    let file = file.pin();
    Ok(c.create_function(move |c| {
        let _ = &file;
        if stream.borrow().is_closed() {
            return Err(c.error("file is already closed"));
        }
        match read_values(c, &stream, &fmts, 1)? {
            Ok(values) if values.first().is_some_and(Value::truthy) => c.ret(values),
            Ok(_) => {
                // The end: no values, and the file closes if `lines` opened it.
                if close {
                    let _ = c.with_stdout(|out| stream.borrow_mut().close(out));
                }
                c.ret(())
            }
            Err(e) => Err(c.error(os_error_text(&e))),
        }
    }))
}

// ---- metamethods and opening ----------------------------------------------

fn f_tostring<'s>(c: &'s Call<'s>) -> Result<Ret, Error> {
    let s = stream_arg(c, 0)?;
    let text = if s.borrow().is_closed() {
        "file (closed)".to_string()
    } else {
        format!("file (0x{:012x})", c.arg(0).address().unwrap_or(0))
    };
    c.ret(text)
}

/// `__close` and `__gc`: close the file unless it is closed or
/// standard.
fn f_gc<'s>(c: &'s Call<'s>) -> Result<Ret, Error> {
    let s = stream_arg(c, 0)?;
    let open = { !s.borrow().is_closed() && !s.borrow().is_std() };
    if open {
        let _ = c.with_stdout(|out| s.borrow_mut().close(out));
    }
    c.ret(())
}

/// Open the library: the `io` table, the `FILE*` class and the standard
/// files, which are also the default input and output. Returns the table.
pub fn open<'s>(s: &'s Scope<'s>) -> Result<Table<'s>, Error> {
    let class = s.define_class::<LuaFile>(FILE_TYPE, |b| {
        b.function("close", f_close)?
            .function("flush", f_flush)?
            .function("lines", f_lines)?
            .function("read", f_read)?
            .function("seek", f_seek)?
            .function("setvbuf", f_setvbuf)?
            .function("write", f_write)?
            .metamethod("__tostring", f_tostring)?
            .metamethod("__close", f_gc)?;
        Ok(())
    })?;
    // `T`'s `Drop` would close a collected file too, but Lua's metatable
    // has a `__gc` scripts can see and call.
    let meta = class.metatable(s)?;
    meta.raw_set("__gc", s.create_function(f_gc))?;
    s.registry().raw_set(FILE_TYPE, meta)?;
    let io = s.create_table();
    let mut defaults = Vec::new();
    for (name, kind, r, w) in [
        ("stdin", Kind::Stdin, true, false),
        ("stdout", Kind::Stdout, false, true),
        ("stderr", Kind::Stderr, false, true),
    ] {
        let mut stream = Stream::new(kind, r, w);
        if name == "stderr" {
            stream.buffering = Buffering::No;
        }
        let (f, rc) = new_file(s, &class, stream)?;
        io.raw_set(name, f)?;
        defaults.push(Default {
            file: f.pin(),
            stream: rc,
        });
    }
    defaults.truncate(2);
    let output = defaults.pop().expect("stdout");
    let input = defaults.pop().expect("stdin");
    let state: State = Rc::new(RefCell::new(Io {
        class,
        input,
        output,
    }));
    type WithState = for<'c> fn(&'c Call<'c>, &State) -> Result<Ret, Error>;
    let with_state: [(&str, WithState); 10] = [
        ("close", io_close),
        ("flush", io_flush),
        ("input", |c, st| set_default(c, st, true)),
        ("lines", io_lines),
        ("open", io_open),
        ("output", |c, st| set_default(c, st, false)),
        ("popen", io_popen),
        ("read", io_read),
        ("tmpfile", io_tmpfile),
        ("write", io_write),
    ];
    for (name, f) in with_state {
        let st = state.clone();
        io.raw_set(name, s.create_function(move |c| f(c, &st)))?;
    }
    io.raw_set("type", s.create_native::<io_type>())?;
    s.before_exit(flush_all);
    Ok(io)
}
