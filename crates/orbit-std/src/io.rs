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

use orbit::stack::{
    Any, Arg, Nil, REGISTRY_INDEX, Rest, Results, Return, Stack, StackResult, Type, Ud, UserType,
    upvalue_index,
};
use orbit::{Error, OrbitString, Scope, Table, native};

use crate::sys::{
    exit_results, file_failure, flush_stdout, os_error_code, os_error_text, os_path, shell,
};

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
        if n > 0 {
            if let Kind::File(f) = &mut self.kind {
                f.seek(SeekFrom::Current(-(n as i64)))?;
            }
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
    /// ahead. The decimal point is `.` or the locale's `point`. Returns the
    /// bytes, or `None` if they ran too long.
    fn read_numeral(&mut self, point: u8, out: &mut dyn Write) -> io::Result<Option<Vec<u8>>> {
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
        if take(self, &mut buf, &|c| c == b'.' || c == point)? {
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

impl UserType for LuaFile {
    const NAME: &'static str = FILE_TYPE;
}

/// The registry keys of the default input and output handles (`io.input`,
/// `io.output`).
const INPUT_KEY: &str = "orbit.io.input";
const OUTPUT_KEY: &str = "orbit.io.output";

fn default_key(input: bool) -> &'static [u8] {
    if input {
        INPUT_KEY.as_bytes()
    } else {
        OUTPUT_KEY.as_bytes()
    }
}

/// Push a new `FILE*` handle for `stream`, and give its stream.
fn push_file(s: &mut Stack<'_>, stream: Stream) -> StackResult<Shared> {
    let rc = Rc::new(RefCell::new(stream));
    OPEN.with(|open| {
        let mut open = open.borrow_mut();
        open.retain(|w| w.strong_count() > 0);
        open.push(Rc::downgrade(&rc));
    });
    s.push_userdata(LuaFile(rc.clone()));
    s.raw_get_field(REGISTRY_INDEX, FILE_TYPE.as_bytes())?;
    s.set_metatable(-2)?;
    Ok(rc)
}

/// The stream of the `FILE*` at `idx`, open or closed.
fn stream_at(s: &mut Stack<'_>, idx: i32) -> Option<Shared> {
    s.with_userdata::<LuaFile, _>(idx, |f| f.as_ref().map(|f| f.0.clone()))
        .flatten()
}

/// The stream of a file, which must be open.
fn open_stream(s: &mut Stack<'_>, file: &LuaFile) -> StackResult<Shared> {
    if file.0.borrow().is_closed() {
        return Err(s.error("attempt to use a closed file"));
    }
    Ok(file.0.clone())
}

/// Push the default input or output handle, and give its stream, which
/// must be open.
fn push_default(s: &mut Stack<'_>, input: bool) -> StackResult<Shared> {
    s.raw_get_field(REGISTRY_INDEX, default_key(input))?;
    let stream = stream_at(s, -1).filter(|st| !st.borrow().is_closed());
    match stream {
        Some(st) => Ok(st),
        None => {
            let what = if input { "input" } else { "output" };
            Err(s.error(format!("default {what} file is closed").as_str()))
        }
    }
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
fn cannot_open(name: &[u8], e: &io::Error) -> OrbitString {
    let why = os_error_text(e);
    OrbitString::concat(&[b"cannot open file '", name, b"' (", why.as_bytes(), b")"])
}

/// Push `nil, message, code` for a failed file operation.
fn push_failure(s: &mut Stack<'_>, e: &io::Error, name: Option<&[u8]>) -> StackResult<Return> {
    file_failure(e, name).push_results(s)
}

// ---- library functions -------------------------------------------------------

#[native(name = "io.open")]
fn io_open(s: &mut Stack<'_>, name: Vec<u8>, mode: Option<Vec<u8>>) -> StackResult<Return> {
    let mode = mode.unwrap_or_else(|| b"r".to_vec());
    match open_file(&name, &mode) {
        None => Err(s.arg_error(2, "invalid mode")),
        Some(Ok(stream)) => {
            push_file(s, stream)?;
            Ok(Return::values(1))
        }
        Some(Err(e)) => push_failure(s, &e, Some(&name)),
    }
}

#[native(name = "io.popen")]
fn io_popen(s: &mut Stack<'_>, cmd: Vec<u8>, mode: Option<Vec<u8>>) -> StackResult<Return> {
    let reading = match mode.as_deref() {
        None | Some(b"r") => true,
        Some(b"w") => false,
        _ => return Err(s.arg_error(2, "invalid mode")),
    };
    // The child writes to the same terminal; keep our output first.
    flush_stdout(s);
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
            push_file(s, stream)?;
            Ok(Return::values(1))
        }
        Err(e) => push_failure(s, &e, Some(&cmd)),
    }
}

#[native(name = "io.tmpfile")]
fn io_tmpfile(s: &mut Stack<'_>) -> StackResult<Return> {
    // Removed at once: the open handle keeps the file until it is closed.
    let opened = crate::sys::temp_file().and_then(|(path, _)| {
        let opened = OpenOptions::new().read(true).write(true).open(&path);
        let _ = std::fs::remove_file(&path);
        opened
    });
    match opened {
        Ok(f) => {
            push_file(s, Stream::new(Kind::File(f), true, true))?;
            Ok(Return::values(1))
        }
        Err(e) => push_failure(s, &e, None),
    }
}

#[native(name = "io.type")]
fn io_type(s: &mut Stack<'_>, v: Any) -> Option<&'static str> {
    let closed = s.with_userdata::<LuaFile, _>(v.0, |f| Some(f.as_ref()?.0.borrow().is_closed()));
    match closed.flatten()? {
        true => Some("closed file"),
        false => Some("file"),
    }
}

#[native(name = "io.input")]
fn io_input(s: &mut Stack<'_>) -> StackResult<Return> {
    set_default(s, true)
}

#[native(name = "io.output")]
fn io_output(s: &mut Stack<'_>) -> StackResult<Return> {
    set_default(s, false)
}

/// `io.input` and `io.output`: with a file name or a file, make it the
/// default; return the default.
fn set_default(s: &mut Stack<'_>, input: bool) -> StackResult<Return> {
    match s.type_at(1) {
        Type::None | Type::Nil => {}
        Type::String | Type::Number => {
            let name = s.check_string(1)?.to_vec();
            let mode: &[u8] = if input { b"r" } else { b"w" };
            match open_file(&name, mode).expect("valid mode") {
                Ok(stream) => {
                    push_file(s, stream)?;
                }
                Err(e) => return Err(s.error(&cannot_open(&name, &e))),
            }
            s.raw_set_field(REGISTRY_INDEX, default_key(input))?;
        }
        _ => {
            let file = Ud::<LuaFile>::arg(s, 1)?;
            open_stream(s, &file)?;
            s.push_copy(1);
            s.raw_set_field(REGISTRY_INDEX, default_key(input))?;
        }
    }
    s.raw_get_field(REGISTRY_INDEX, default_key(input))?;
    Ok(Return::values(1))
}

#[native(name = "io.close")]
fn io_close(s: &mut Stack<'_>) -> StackResult<Return> {
    if matches!(s.type_at(1), Type::None | Type::Nil) {
        let stream = push_default(s, false)?;
        return close(s, &stream);
    }
    let file = Ud::<LuaFile>::arg(s, 1)?;
    f_close(s, file)
}

// Methods are unnamed in messages, as in the reference: `bad argument #2
// to '?'` when no call names them.

#[native(name = "?")]
fn f_close(s: &mut Stack<'_>, file: Ud<LuaFile>) -> StackResult<Return> {
    let stream = open_stream(s, &file)?;
    close(s, &stream)
}

fn close(s: &mut Stack<'_>, stream: &Shared) -> StackResult<Return> {
    if stream.borrow().is_std() {
        return (Nil, "cannot close standard file").push_results(s);
    }
    match s.with_stdout(|out| stream.borrow_mut().close(out)) {
        Ok(Some(status)) => exit_results(status).push_results(s),
        Ok(None) => true.push_results(s),
        Err(e) => push_failure(s, &e, None),
    }
}

#[native(name = "io.flush")]
fn io_flush(s: &mut Stack<'_>) -> StackResult<Return> {
    let stream = push_default(s, false)?;
    flush_result(s, &stream)
}

#[native(name = "?")]
fn f_flush(s: &mut Stack<'_>, file: Ud<LuaFile>) -> StackResult<Return> {
    let stream = open_stream(s, &file)?;
    s.push_copy(1);
    flush_result(s, &stream)
}

/// Flush `stream`: the file on top, or `nil, message, code`.
fn flush_result(s: &mut Stack<'_>, stream: &Shared) -> StackResult<Return> {
    match s.with_stdout(|out| stream.borrow_mut().flush(out)) {
        Ok(()) => Ok(Return::values(1)),
        Err(e) => push_failure(s, &e, None),
    }
}

#[native(name = "io.write")]
fn io_write(s: &mut Stack<'_>, args: Rest) -> StackResult<Return> {
    let stream = push_default(s, false)?;
    write(s, &stream, args)
}

#[native(name = "?")]
fn f_write(s: &mut Stack<'_>, file: Ud<LuaFile>, args: Rest) -> StackResult<Return> {
    let stream = open_stream(s, &file)?;
    s.push_copy(1);
    write(s, &stream, args)
}

/// Write `args` (strings or numbers) to `stream`. Returns the file on top,
/// or `nil, message, code, bytes written` (5.4: without the count).
fn write(s: &mut Stack<'_>, stream: &Shared, args: Rest) -> StackResult<Return> {
    let mut written = 0usize;
    let mut bytes = Vec::new();
    for i in args.indices() {
        bytes.clear();
        match s.type_at(i) {
            Type::String => bytes.extend_from_slice(&s.string_at(i).unwrap_or_default()),
            // With the locale's decimal point, as `tostring` writes it.
            Type::Number => {
                bytes.extend_from_slice(s.number_text(i).unwrap_or_default().as_bytes())
            }
            _ => return Err(s.type_error(i, "string")),
        }
        if let Err(e) = s.with_stdout(|out| stream.borrow_mut().write(&bytes, out)) {
            let (msg, code) = (os_error_text(&e), os_error_code(&e));
            // Lua 5.4 doesn't report the bytes written (ADR-14).
            if orbit::LUA.is_5_4() {
                return (Nil, msg, code).push_results(s);
            }
            return (Nil, msg, code, written as i64).push_results(s);
        }
        written += bytes.len();
    }
    Ok(Return::values(1))
}

#[native(name = "?")]
fn f_seek(
    s: &mut Stack<'_>,
    file: Ud<LuaFile>,
    whence: Option<Vec<u8>>,
    off: Option<i64>,
) -> StackResult<Return> {
    let stream = open_stream(s, &file)?;
    let off = off.unwrap_or(0);
    let from = match whence.as_deref().unwrap_or(b"cur") {
        b"set" => {
            if off < 0 {
                return push_failure(s, &io::Error::from(io::ErrorKind::InvalidInput), None);
            }
            SeekFrom::Start(off as u64)
        }
        b"cur" => SeekFrom::Current(off),
        b"end" => SeekFrom::End(off),
        other => {
            let msg = OrbitString::concat(&[b"invalid option '", other, b"'"]);
            return Err(s.arg_error(2, &msg));
        }
    };
    match s.with_stdout(|out| stream.borrow_mut().seek(from, out)) {
        Ok(p) => (p as i64).push_results(s),
        Err(e) => push_failure(s, &e, None),
    }
}

#[native(name = "?")]
fn f_setvbuf(
    s: &mut Stack<'_>,
    file: Ud<LuaFile>,
    mode: Vec<u8>,
    size: Option<i64>,
) -> StackResult<Return> {
    let stream = open_stream(s, &file)?;
    let size = size.unwrap_or(BUFSIZ as i64).max(0) as usize;
    let b = match &mode[..] {
        b"no" => Buffering::No,
        b"line" => Buffering::Line(size),
        b"full" => Buffering::Full(size),
        _ => {
            let msg = OrbitString::concat(&[b"invalid option '", &mode, b"'"]);
            return Err(s.arg_error(2, &msg));
        }
    };
    let r = s.with_stdout(|out| {
        let mut st = stream.borrow_mut();
        let r = st.flush(out);
        st.buffering = b;
        r
    });
    match r {
        Ok(()) => true.push_results(s),
        Err(e) => push_failure(s, &e, None),
    }
}

// ---- reading ---------------------------------------------------------------

#[native(name = "io.read")]
fn io_read(s: &mut Stack<'_>, formats: Rest) -> StackResult<Return> {
    let stream = push_default(s, true)?;
    let fmts = format_args(s, formats);
    read(s, &stream, &fmts, formats.first)
}

#[native(name = "?")]
fn f_read(s: &mut Stack<'_>, file: Ud<LuaFile>, formats: Rest) -> StackResult<Return> {
    let stream = open_stream(s, &file)?;
    let fmts = format_args(s, formats);
    read(s, &stream, &fmts, formats.first)
}

/// `read`: the values, or `nil, message, code` on an I/O error.
fn read(s: &mut Stack<'_>, stream: &Shared, fmts: &[FormatArg], arg: i32) -> StackResult<Return> {
    match read_values(s, stream, fmts, arg)? {
        Ok(n) => Ok(Return::values(n)),
        Err(e) => push_failure(s, &e, None),
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
    /// The format at `idx`.
    fn at(s: &Stack<'_>, idx: i32) -> FormatArg {
        match s.type_at(idx) {
            Type::Number if s.is_integer(idx) => FormatArg::Int(s.to_integer(idx).unwrap_or(0)),
            Type::Number => FormatArg::Float(s.to_number(idx).unwrap_or(0.0)),
            Type::String => FormatArg::Str(s.string_at(idx).unwrap_or_default().to_vec()),
            _ => FormatArg::Other,
        }
    }
}

/// The formats among the arguments.
fn format_args(s: &Stack<'_>, args: Rest) -> Vec<FormatArg> {
    args.indices().map(|i| FormatArg::at(s, i)).collect()
}

/// 2^63 as a float: the first float above every `i64`.
const TWO_63: f64 = 9_223_372_036_854_775_808.0;

/// Format `v`, argument `arg`.
fn format(s: &mut Stack<'_>, v: &FormatArg, arg: i32) -> StackResult<Format> {
    let count = |n: i64| Format::Count(usize::try_from(n).unwrap_or(0));
    match *v {
        FormatArg::Int(n) => Ok(count(n)),
        // Only floats with an `i64` value, as Lua's integer arguments.
        FormatArg::Float(f) if f.fract() == 0.0 && (-TWO_63..TWO_63).contains(&f) => {
            Ok(count(f as i64))
        }
        FormatArg::Float(_) => Err(s.arg_error(arg, "number has no integer representation")),
        FormatArg::Str(ref b) => {
            let b = b.strip_prefix(b"*").unwrap_or(b);
            match b.first() {
                Some(b'n') => Ok(Format::Number),
                Some(b'l') => Ok(Format::Line { keep: false }),
                Some(b'L') => Ok(Format::Line { keep: true }),
                Some(b'a') => Ok(Format::All),
                _ => Err(s.arg_error(arg, "invalid format")),
            }
        }
        FormatArg::Other => Err(s.type_error(arg, "string")),
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

/// Read with formats `fmts` (none: one line) from `stream`, pushing one
/// value per format and stopping after the first that fails (nil). `arg` is
/// the argument number of the first format, for messages. As in lua5.5,
/// each format is checked when its turn comes, so formats after a failed
/// read are never looked at. Gives how many values it pushed, or the I/O
/// error, with the values taken off again.
fn read_values(
    s: &mut Stack<'_>,
    stream: &Shared,
    fmts: &[FormatArg],
    arg: i32,
) -> StackResult<io::Result<usize>> {
    let top = s.top();
    let n = fmts.len().max(1);
    for i in 0..n {
        let fmt = match fmts.get(i) {
            Some(f) => format(s, f, arg + i as i32)?,
            None => Format::Line { keep: false },
        };
        match read_one(s, stream, fmt) {
            Ok(true) => {}
            Ok(false) => return Ok(Ok(i + 1)),
            Err(e) if e.kind() == io::ErrorKind::OutOfMemory => return Err(s.memory_error()),
            Err(e) => {
                s.set_top(top)?;
                return Ok(Err(e));
            }
        }
    }
    Ok(Ok(n))
}

/// What one format read.
enum ReadOutcome {
    Bytes(Vec<u8>),
    Numeral(Option<Vec<u8>>),
    Nothing,
}

/// Push one format's value, or nil if it fails. Gives whether it read.
fn read_one(s: &mut Stack<'_>, stream: &Shared, fmt: Format) -> io::Result<bool> {
    // The first byte of the locale's decimal point, as lua5.5 reads it.
    let point = s
        .decimal_point()
        .as_bytes()
        .first()
        .copied()
        .unwrap_or(b'.');
    let read = s.with_stdout(|out| -> io::Result<ReadOutcome> {
        let mut st = stream.borrow_mut();
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
            Format::Number => return Ok(ReadOutcome::Numeral(st.read_numeral(point, out)?)),
        };
        Ok(if ok {
            ReadOutcome::Bytes(buf)
        } else {
            ReadOutcome::Nothing
        })
    })?;
    Ok(match read {
        ReadOutcome::Bytes(b) => {
            s.push_bytes(&b[..]);
            true
        }
        ReadOutcome::Numeral(Some(d)) if s.push_numeral(&d[..]) != 0 => true,
        ReadOutcome::Numeral(_) | ReadOutcome::Nothing => {
            s.push_nil();
            false
        }
    })
}

// ---- lines -----------------------------------------------------------------

#[native(name = "io.lines")]
fn io_lines(s: &mut Stack<'_>, formats: Rest) -> StackResult<Return> {
    let close = !matches!(s.type_at(1), Type::None | Type::Nil);
    if close {
        let name = s.check_string(1)?.to_vec();
        match open_file(&name, b"r").expect("valid mode") {
            Ok(stream) => {
                push_file(s, stream)?;
            }
            Err(e) => return Err(s.error(&cannot_open(&name, &e))),
        }
    } else {
        push_default(s, true)?;
    }
    // [file?, formats..., file]
    let file = s.top();
    push_lines_iterator(
        s,
        file,
        formats.first + 1,
        formats.count.saturating_sub(1),
        close,
    )?;
    if close {
        // The iterator, two nils, and the file to close.
        s.push_nil();
        s.push_nil();
        s.push_copy(file);
    } else {
        s.push_nil();
        s.push_nil();
        s.push_nil();
    }
    Ok(Return::values(4))
}

#[native(name = "?")]
fn f_lines(s: &mut Stack<'_>, file: Ud<LuaFile>, formats: Rest) -> StackResult<Return> {
    open_stream(s, &file)?;
    push_lines_iterator(s, 1, formats.first, formats.count, false)?;
    Ok(Return::values(1))
}

/// Push a function that reads the file at `file` with the `n` formats from
/// `first`, closing the file at the end when `close`. The file and the
/// formats are its upvalues, which keep the handle alive, as Lua's do.
fn push_lines_iterator(
    s: &mut Stack<'_>,
    file: i32,
    first: i32,
    n: usize,
    close: bool,
) -> StackResult<()> {
    if n > MAX_LINE_FORMATS {
        return Err(s.arg_error(first + MAX_LINE_FORMATS as i32, "too many arguments"));
    }
    s.push_copy(file);
    for i in 0..n as i32 {
        s.push_copy(first + i);
    }
    let data = (n as u64) << 1 | u64::from(close);
    s.push_native_closure::<lines_next>(n + 1, data)
}

#[native(name = "?")]
fn lines_next(s: &mut Stack<'_>) -> StackResult<Return> {
    let data = s.closure_data();
    let (n, close) = ((data >> 1) as i32, data & 1 != 0);
    let Some(stream) = stream_at(s, upvalue_index(1)) else {
        return Err(s.error("file is already closed"));
    };
    if stream.borrow().is_closed() {
        return Err(s.error("file is already closed"));
    }
    let fmts: Vec<FormatArg> = (2..n + 2)
        .map(|i| FormatArg::at(s, upvalue_index(i)))
        .collect();
    let top = s.top();
    match read_values(s, &stream, &fmts, 2)? {
        Ok(n) if s.to_boolean(top + 1) => Ok(Return::values(n)),
        Ok(_) => {
            // The end: no values, and the file closes if `lines` opened it.
            if close {
                let _ = s.with_stdout(|out| stream.borrow_mut().close(out));
            }
            Ok(Return::values(0))
        }
        Err(e) => Err(s.error(os_error_text(&e).as_str())),
    }
}

// ---- metamethods and opening ----------------------------------------------

#[native(name = "?")]
fn f_tostring(s: &mut Stack<'_>, file: Ud<LuaFile>) -> String {
    if (file.0).0.borrow().is_closed() {
        "file (closed)".to_string()
    } else {
        format!("file (0x{:012x})", s.identity(1).unwrap_or(0))
    }
}

/// `__close` and `__gc`: close the file unless it is closed or standard.
#[native(name = "?")]
fn f_gc(s: &mut Stack<'_>, file: Ud<LuaFile>) {
    let stream = &(file.0).0;
    let open = !stream.borrow().is_closed() && !stream.borrow().is_std();
    if open {
        let _ = s.with_stdout(|out| stream.borrow_mut().close(out));
    }
}

/// Open the library: the `io` table, the `FILE*` class and the standard
/// files, which are also the default input and output. Returns the table.
pub fn open<'s>(s: &'s Scope<'s>) -> Result<Table<'s>, Error> {
    let class = s.define_class::<LuaFile>(FILE_TYPE, |b| {
        b.native::<f_close>("close")?
            .native::<f_flush>("flush")?
            .native::<f_lines>("lines")?
            .native::<f_read>("read")?
            .native::<f_seek>("seek")?
            .native::<f_setvbuf>("setvbuf")?
            .native::<f_write>("write")?
            .native_metamethod::<f_tostring>("__tostring")?
            .native_metamethod::<f_gc>("__close")?;
        Ok(())
    })?;
    // `T`'s `Drop` would close a collected file too, but Lua's metatable
    // has a `__gc` scripts can see and call.
    let meta = class.metatable(s)?;
    meta.raw_set("__gc", s.create_native::<f_gc>())?;
    s.registry().raw_set(FILE_TYPE, meta)?;
    let io = s.create_table();
    for (name, kind, r, w) in [
        ("stdin", Kind::Stdin, true, false),
        ("stdout", Kind::Stdout, false, true),
        ("stderr", Kind::Stderr, false, true),
    ] {
        let mut stream = Stream::new(kind, r, w);
        if name == "stderr" {
            stream.buffering = Buffering::No;
        }
        let rc = Rc::new(RefCell::new(stream));
        OPEN.with(|open| open.borrow_mut().push(Rc::downgrade(&rc)));
        let f = class.instance(s, LuaFile(rc))?;
        io.raw_set(name, f)?;
        match name {
            "stdin" => s.registry().raw_set(INPUT_KEY, f)?,
            "stdout" => s.registry().raw_set(OUTPUT_KEY, f)?,
            _ => {}
        }
    }
    io.add_native::<io_close>()?;
    io.add_native::<io_flush>()?;
    io.add_native::<io_input>()?;
    io.add_native::<io_lines>()?;
    io.add_native::<io_open>()?;
    io.add_native::<io_output>()?;
    io.add_native::<io_popen>()?;
    io.add_native::<io_read>()?;
    io.add_native::<io_tmpfile>()?;
    io.add_native::<io_type>()?;
    io.add_native::<io_write>()?;
    s.before_exit(flush_all);
    Ok(io)
}
