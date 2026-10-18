//! The `io` library: file handles as `FILE*` userdata.
//!
//! A handle owns a [`Stream`]: a file, a standard stream or a child process
//! from `io.popen`, with our own read and write buffers so reads and writes
//! can interleave, `setvbuf` means what it says, and `read("n")` can look one
//! byte ahead. Writes to standard output go through the buffer `print`
//! uses, so the two keep their order.

use std::cell::RefCell;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::process::{Child, Stdio};
use std::rc::{Rc, Weak};

use crate::object::table as tbl;
use crate::object::{Gc, Table, UserData, Value};
use crate::vm::{LuaError, NativeCtx, NativeFn, NativeReturn};

use crate::stdlib::sys::{exit_status, file_error, shell};

type R = Result<NativeReturn, LuaError>;

pub(crate) const FUNCTIONS: &[(&str, NativeFn)] = &[
    ("io.close", io_close),
    ("io.flush", io_flush),
    ("io.input", io_input),
    ("io.lines", io_lines),
    ("io.open", io_open),
    ("io.output", io_output),
    ("io.popen", io_popen),
    ("io.read", io_read),
    ("io.tmpfile", io_tmpfile),
    ("io.type", io_type),
    ("io.write", io_write),
];

const METHODS: &[(&str, NativeFn)] = &[
    ("close", f_close),
    ("flush", f_flush),
    ("lines", f_lines),
    ("read", f_read),
    ("seek", f_seek),
    ("setvbuf", f_setvbuf),
    ("write", f_write),
];

/// Registry keys.
const FILE_META: &[u8] = b"FILE*";
const INPUT: &[u8] = b"_IO_input";
const OUTPUT: &[u8] = b"_IO_output";

/// The C library's `BUFSIZ`, the default buffer size.
const BUFSIZ: usize = 8192;

/// The most formats `lines` takes, as in the reference.
const MAX_LINE_FORMATS: usize = 250;

/// The longest numeral `read("n")` reads.
const MAX_NUMERAL: usize = 200;

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
pub(crate) struct Stream {
    kind: Kind,
    readable: bool,
    writable: bool,
    rbuf: Vec<u8>,
    rpos: usize,
    wbuf: Vec<u8>,
    buffering: Buffering,
}

/// A `FILE*` userdata's payload. Shared, so `os.exit` can flush every open
/// stream ([`flush_all`]).
#[derive(Debug, Clone)]
pub(crate) struct LuaFile(Rc<RefCell<Stream>>);

thread_local! {
    static OPEN: RefCell<Vec<Weak<RefCell<Stream>>>> = const { RefCell::new(Vec::new()) };
}

/// Flush every open stream's buffer, as the C library does at exit.
/// Used by `os.exit` and the host's `flush_files`.
pub(crate) fn flush_all(out: &mut dyn Write) {
    OPEN.with(|open| {
        for s in open.borrow().iter().filter_map(Weak::upgrade) {
            let _ = s.borrow_mut().flush(out);
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
            // `print`'s buffer does the buffering.
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
        // Standard output's buffer belongs to the VM, which flushes it.
        if !matches!(self.kind, Kind::Stdout | Kind::Closed) {
            let _ = self.close(&mut io::sink());
        }
    }
}

// ---- handles ---------------------------------------------------------------

fn registry_get(ctx: &mut NativeCtx<'_>, key: &[u8]) -> Value {
    let reg = ctx.vm.rt.registry;
    let k = ctx.new_str(key);
    tbl::get(&ctx.vm.heap, reg, k)
}

fn registry_set(ctx: &mut NativeCtx<'_>, key: &[u8], v: Value) {
    let reg = ctx.vm.rt.registry;
    let k = ctx.new_str(key);
    let _ = tbl::set(&mut ctx.vm.heap, reg, k, v);
}

/// A new `FILE*` userdata for `stream`.
fn new_file(ctx: &mut NativeCtx<'_>, stream: Stream) -> Value {
    let rc = Rc::new(RefCell::new(stream));
    OPEN.with(|open| {
        let mut open = open.borrow_mut();
        open.retain(|w| w.strong_count() > 0);
        open.push(Rc::downgrade(&rc));
    });
    let u = ctx.new_userdata(LuaFile(rc));
    let meta = match registry_get(ctx, FILE_META) {
        Value::Table(t) => Some(t),
        _ => None,
    };
    ctx.vm.heap.update(u, |u: &mut UserData| u.meta = meta);
    Value::UserData(u)
}

/// The stream of a `FILE*` argument, open or closed.
fn stream_arg(ctx: &mut NativeCtx<'_>, i: usize) -> Result<Rc<RefCell<Stream>>, LuaError> {
    let u = ctx.check_userdata::<LuaFile>(i, "FILE*")?;
    Ok(ctx.userdata_ref::<LuaFile>(u).expect("checked").0.clone())
}

/// The stream of an open `FILE*` argument.
fn open_arg(ctx: &mut NativeCtx<'_>, i: usize) -> Result<Rc<RefCell<Stream>>, LuaError> {
    let s = stream_arg(ctx, i)?;
    if s.borrow().is_closed() {
        return Err(ctx.error("attempt to use a closed file"));
    }
    Ok(s)
}

fn stream_of(ctx: &NativeCtx<'_>, v: Value) -> Option<Rc<RefCell<Stream>>> {
    match v {
        Value::UserData(u) => ctx.userdata_ref::<LuaFile>(u).map(|f| f.0.clone()),
        _ => None,
    }
}

/// The default input or output stream, which must be open.
fn default_file(
    ctx: &mut NativeCtx<'_>,
    key: &[u8],
) -> Result<(Value, Rc<RefCell<Stream>>), LuaError> {
    let v = registry_get(ctx, key);
    match stream_of(ctx, v) {
        Some(s) if !s.borrow().is_closed() => Ok((v, s)),
        _ => {
            let what = if key == INPUT { "input" } else { "output" };
            Err(ctx.error(format!("default {what} file is closed")))
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
        o.open(crate::stdlib::os_path(name))
            .map(|f| Stream::new(Kind::File(f), r, w)),
    )
}

// ---- library functions -------------------------------------------------------

fn io_open(ctx: &mut NativeCtx<'_>) -> R {
    let s = ctx.check_str(0)?;
    let name = ctx.bytes(s).to_vec();
    let mode = if ctx.arg(1).is_nil() {
        b"r".to_vec()
    } else {
        let m = ctx.check_str(1)?;
        ctx.bytes(m).to_vec()
    };
    match open_file(&name, &mode) {
        None => Err(ctx.arg_error(1, "invalid mode")),
        Some(Ok(stream)) => {
            let f = new_file(ctx, stream);
            ctx.ret1(f)
        }
        Some(Err(e)) => file_error(ctx, &e, Some(&name)),
    }
}

fn io_popen(ctx: &mut NativeCtx<'_>) -> R {
    let s = ctx.check_str(0)?;
    let cmd = ctx.bytes(s).to_vec();
    let mode = if ctx.arg(1).is_nil() {
        b"r".to_vec()
    } else {
        let m = ctx.check_str(1)?;
        ctx.bytes(m).to_vec()
    };
    let reading = match &mode[..] {
        b"r" => true,
        b"w" => false,
        _ => return Err(ctx.arg_error(1, "invalid mode")),
    };
    // The child writes to the same terminal; keep our output first.
    let _ = ctx.vm.rt.out.flush();
    let mut c = shell(crate::stdlib::os_path(&cmd).as_os_str());
    if reading {
        c.stdout(Stdio::piped());
    } else {
        c.stdin(Stdio::piped());
    }
    match c.spawn() {
        Ok(child) => {
            let stream = if reading {
                Stream::new(Kind::PipeRead(child), true, false)
            } else {
                Stream::new(Kind::PipeWrite(child), false, true)
            };
            let f = new_file(ctx, stream);
            ctx.ret1(f)
        }
        Err(e) => file_error(ctx, &e, Some(&cmd)),
    }
}

fn io_tmpfile(ctx: &mut NativeCtx<'_>) -> R {
    // Removed at once: the open handle keeps the file until it is closed.
    match crate::stdlib::temp_file() {
        Ok((path, _)) => {
            let opened = OpenOptions::new().read(true).write(true).open(&path);
            let _ = std::fs::remove_file(&path);
            match opened {
                Ok(f) => {
                    let v = new_file(ctx, Stream::new(Kind::File(f), true, true));
                    ctx.ret1(v)
                }
                Err(e) => file_error(ctx, &e, None),
            }
        }
        Err(e) => file_error(ctx, &e, None),
    }
}

fn io_type(ctx: &mut NativeCtx<'_>) -> R {
    ctx.check_any(0)?;
    let v = ctx.arg(0);
    let r = match stream_of(ctx, v) {
        Some(s) if s.borrow().is_closed() => ctx.new_str(b"closed file"),
        Some(_) => ctx.new_str(b"file"),
        None => Value::Nil,
    };
    ctx.ret1(r)
}

/// `io.input` and `io.output`.
fn set_default(ctx: &mut NativeCtx<'_>, key: &[u8], mode: &[u8]) -> R {
    match ctx.arg(0) {
        Value::Nil => {}
        Value::Str(_) | Value::Int(_) | Value::Float(_) => {
            let s = ctx.check_str(0)?;
            let name = ctx.bytes(s).to_vec();
            match open_file(&name, mode).expect("valid mode") {
                Ok(stream) => {
                    let f = new_file(ctx, stream);
                    registry_set(ctx, key, f);
                }
                Err(e) => {
                    let msg = format!(
                        "cannot open file '{}' ({})",
                        String::from_utf8_lossy(&name),
                        crate::stdlib::os_error_text(&e)
                    );
                    return Err(ctx.error(msg));
                }
            }
        }
        v => {
            open_arg(ctx, 0)?;
            registry_set(ctx, key, v);
        }
    }
    let v = registry_get(ctx, key);
    ctx.ret1(v)
}

fn io_input(ctx: &mut NativeCtx<'_>) -> R {
    set_default(ctx, INPUT, b"r")
}

fn io_output(ctx: &mut NativeCtx<'_>) -> R {
    set_default(ctx, OUTPUT, b"w")
}

fn io_close(ctx: &mut NativeCtx<'_>) -> R {
    if ctx.arg(0).is_nil() {
        let (v, _) = default_file(ctx, OUTPUT)?;
        ctx.truncate(0);
        ctx.push(v);
    }
    f_close(ctx)
}

fn f_close(ctx: &mut NativeCtx<'_>) -> R {
    let s = open_arg(ctx, 0)?;
    if s.borrow().is_std() {
        ctx.push(Value::Nil);
        let m = ctx.new_str(b"cannot close standard file");
        ctx.push(m);
        return Ok(NativeReturn::Return(2));
    }
    let r = s.borrow_mut().close(&mut *ctx.vm.rt.out);
    match r {
        Ok(Some(status)) => exit_status(ctx, status),
        Ok(None) => ctx.ret1(Value::Bool(true)),
        Err(e) => file_error(ctx, &e, None),
    }
}

fn io_flush(ctx: &mut NativeCtx<'_>) -> R {
    let (v, s) = default_file(ctx, OUTPUT)?;
    flush_result(ctx, &s, v)
}

fn f_flush(ctx: &mut NativeCtx<'_>) -> R {
    let s = open_arg(ctx, 0)?;
    let v = ctx.arg(0);
    flush_result(ctx, &s, v)
}

fn flush_result(ctx: &mut NativeCtx<'_>, s: &Rc<RefCell<Stream>>, file: Value) -> R {
    let r = s.borrow_mut().flush(&mut *ctx.vm.rt.out);
    match r {
        Ok(()) => ctx.ret1(file),
        Err(e) => file_error(ctx, &e, None),
    }
}

fn io_write(ctx: &mut NativeCtx<'_>) -> R {
    let (v, s) = default_file(ctx, OUTPUT)?;
    write(ctx, &s, v, 0)
}

fn f_write(ctx: &mut NativeCtx<'_>) -> R {
    let s = open_arg(ctx, 0)?;
    let v = ctx.arg(0);
    write(ctx, &s, v, 1)
}

/// Write arguments `first..` (strings or numbers) to `s`. Returns the file,
/// or `nil, message, code, bytes written`.
fn write(ctx: &mut NativeCtx<'_>, s: &Rc<RefCell<Stream>>, file: Value, first: usize) -> R {
    let mut written = 0usize;
    for i in first..ctx.nargs() {
        let bytes = match ctx.arg(i) {
            Value::Str(st) => ctx.bytes(st).to_vec(),
            v @ (Value::Int(_) | Value::Float(_)) => {
                let mut b = Vec::new();
                ctx.display(v, &mut b);
                b
            }
            _ => return Err(ctx.type_error(i, "string")),
        };
        let r = s.borrow_mut().write(&bytes, &mut *ctx.vm.rt.out);
        if let Err(e) = r {
            file_error(ctx, &e, None)?;
            ctx.push(Value::Int(written as i64));
            return Ok(NativeReturn::Return(4));
        }
        written += bytes.len();
    }
    ctx.ret1(file)
}

fn f_seek(ctx: &mut NativeCtx<'_>) -> R {
    let s = open_arg(ctx, 0)?;
    let whence = if ctx.arg(1).is_nil() {
        b"cur".to_vec()
    } else {
        let w = ctx.check_str(1)?;
        ctx.bytes(w).to_vec()
    };
    let off = ctx.opt_int(2, 0)?;
    let from = match &whence[..] {
        b"set" => {
            if off < 0 {
                return file_error(ctx, &io::Error::from(io::ErrorKind::InvalidInput), None);
            }
            SeekFrom::Start(off as u64)
        }
        b"cur" => SeekFrom::Current(off),
        b"end" => SeekFrom::End(off),
        _ => {
            let msg = format!("invalid option '{}'", String::from_utf8_lossy(&whence));
            return Err(ctx.arg_error(1, msg));
        }
    };
    let r = s.borrow_mut().seek(from, &mut *ctx.vm.rt.out);
    match r {
        Ok(p) => ctx.ret1(Value::Int(p as i64)),
        Err(e) => file_error(ctx, &e, None),
    }
}

fn f_setvbuf(ctx: &mut NativeCtx<'_>) -> R {
    let s = open_arg(ctx, 0)?;
    let m = ctx.check_str(1)?;
    let mode = ctx.bytes(m).to_vec();
    let size = ctx.opt_int(2, BUFSIZ as i64)?.max(0) as usize;
    let b = match &mode[..] {
        b"no" => Buffering::No,
        b"line" => Buffering::Line(size),
        b"full" => Buffering::Full(size),
        _ => {
            let msg = format!("invalid option '{}'", String::from_utf8_lossy(&mode));
            return Err(ctx.arg_error(1, msg));
        }
    };
    let r = {
        let mut st = s.borrow_mut();
        let r = st.flush(&mut *ctx.vm.rt.out);
        st.buffering = b;
        r
    };
    match r {
        Ok(()) => ctx.ret1(Value::Bool(true)),
        Err(e) => file_error(ctx, &e, None),
    }
}

// ---- reading ---------------------------------------------------------------

fn io_read(ctx: &mut NativeCtx<'_>) -> R {
    let (_, s) = default_file(ctx, INPUT)?;
    let fmts: Vec<Value> = (0..ctx.nargs()).map(|i| ctx.arg(i)).collect();
    read(ctx, &s, &fmts, 0)
}

fn f_read(ctx: &mut NativeCtx<'_>) -> R {
    let s = open_arg(ctx, 0)?;
    let fmts: Vec<Value> = (1..ctx.nargs()).map(|i| ctx.arg(i)).collect();
    read(ctx, &s, &fmts, 1)
}

/// `read`: the values, or `nil, message, code` on an I/O error.
fn read(ctx: &mut NativeCtx<'_>, s: &Rc<RefCell<Stream>>, fmts: &[Value], arg: usize) -> R {
    match read_values(ctx, s, fmts, arg)? {
        Ok(n) => Ok(NativeReturn::Return(n)),
        Err(e) => file_error(ctx, &e, None),
    }
}

/// One `read` format.
enum Format {
    Count(usize),
    Number,
    Line { keep: bool },
    All,
}

fn format(ctx: &mut NativeCtx<'_>, v: Value, arg: usize) -> Result<Format, LuaError> {
    let count = |n: i64| Format::Count(usize::try_from(n).unwrap_or(0));
    match v {
        Value::Int(n) => Ok(count(n)),
        Value::Float(f) if f.fract() == 0.0 && (-9.3e18..9.3e18).contains(&f) => {
            Ok(count(f as i64))
        }
        Value::Float(_) => Err(ctx.arg_error(arg, "number has no integer representation")),
        Value::Str(s) => {
            let b = ctx.bytes(s);
            let b = b.strip_prefix(b"*").unwrap_or(b);
            match b.first() {
                Some(b'n') => Ok(Format::Number),
                Some(b'l') => Ok(Format::Line { keep: false }),
                Some(b'L') => Ok(Format::Line { keep: true }),
                Some(b'a') => Ok(Format::All),
                _ => Err(ctx.arg_error(arg, "invalid format")),
            }
        }
        _ => Err(ctx.type_error(arg, "string")),
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

/// Read with formats `fmts` (none: one line) from `s`, pushing one value
/// per format and stopping after the first that fails (nil). `arg` is the
/// window index of the first format, for messages. On an I/O error nothing
/// is pushed.
fn read_values(
    ctx: &mut NativeCtx<'_>,
    s: &Rc<RefCell<Stream>>,
    fmts: &[Value],
    arg: usize,
) -> Result<io::Result<usize>, LuaError> {
    let mut parsed = Vec::with_capacity(fmts.len().max(1));
    for (i, &f) in fmts.iter().enumerate() {
        parsed.push(format(ctx, f, arg + i)?);
    }
    if parsed.is_empty() {
        parsed.push(Format::Line { keep: false });
    }
    let len = ctx.len();
    let mut n = 0;
    for fmt in parsed {
        match read_one(ctx, s, fmt) {
            Ok(v) => {
                let failed = v.is_nil();
                ctx.push(v);
                n += 1;
                if failed {
                    break;
                }
            }
            Err(e) if e.kind() == io::ErrorKind::OutOfMemory => {
                ctx.truncate(len);
                return Err(ctx.no_memory());
            }
            Err(e) => {
                ctx.truncate(len);
                return Ok(Err(e));
            }
        }
    }
    Ok(Ok(n))
}

/// One format's value, or nil if it fails.
fn read_one(ctx: &mut NativeCtx<'_>, s: &Rc<RefCell<Stream>>, fmt: Format) -> io::Result<Value> {
    let mut buf = Vec::new();
    let ok = {
        let mut st = s.borrow_mut();
        let out: &mut dyn Write = &mut *ctx.vm.rt.out;
        match fmt {
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
            Format::Number => {
                let digits = st.read_numeral(out)?;
                return Ok(digits
                    .and_then(|d| crate::object::number::str_to_number(&d))
                    .unwrap_or(Value::Nil));
            }
        }
    };
    Ok(if ok {
        ctx.new_str_owned(buf)
    } else {
        Value::Nil
    })
}

// ---- lines -----------------------------------------------------------------

fn io_lines(ctx: &mut NativeCtx<'_>) -> R {
    let (file, close) = if ctx.arg(0).is_nil() {
        let (v, _) = default_file(ctx, INPUT)?;
        (v, false)
    } else {
        let s = ctx.check_str(0)?;
        let name = ctx.bytes(s).to_vec();
        match open_file(&name, b"r").expect("valid mode") {
            Ok(stream) => (new_file(ctx, stream), true),
            Err(e) => {
                let msg = format!(
                    "cannot open file '{}' ({})",
                    String::from_utf8_lossy(&name),
                    crate::stdlib::os_error_text(&e)
                );
                return Err(ctx.error(msg));
            }
        }
    };
    let iter = lines_iterator(ctx, file, 1, close)?;
    ctx.push(iter);
    ctx.push(Value::Nil);
    ctx.push(Value::Nil);
    ctx.push(if close { file } else { Value::Nil });
    Ok(NativeReturn::Return(4))
}

fn f_lines(ctx: &mut NativeCtx<'_>) -> R {
    open_arg(ctx, 0)?;
    let file = ctx.arg(0);
    let iter = lines_iterator(ctx, file, 1, false)?;
    ctx.ret1(iter)
}

/// A function that reads `file` with the formats in arguments `first..`,
/// closing the file at the end when `close`: a native closure with
/// upvalues 0 the file, 1 `close`, 2 the number of formats, then the
/// formats.
fn lines_iterator(
    ctx: &mut NativeCtx<'_>,
    file: Value,
    first: usize,
    close: bool,
) -> Result<Value, LuaError> {
    let n = ctx.nargs().saturating_sub(first);
    if n > MAX_LINE_FORMATS {
        return Err(ctx.arg_error(first + MAX_LINE_FORMATS, "too many arguments"));
    }
    let mut ups = vec![file, Value::Bool(close), Value::Int(n as i64)];
    ups.extend((0..n).map(|i| ctx.arg(first + i)));
    Ok(ctx.new_closure("io.lines iterator", lines_step, &ups))
}

fn lines_step(ctx: &mut NativeCtx<'_>) -> R {
    let file = ctx.upvalue(0);
    let s = stream_of(ctx, file).expect("lines file");
    if s.borrow().is_closed() {
        return Err(ctx.error("file is already closed"));
    }
    let close = !ctx.upvalue(1).is_falsy();
    let Value::Int(n) = ctx.upvalue(2) else {
        unreachable!("lines format count")
    };
    let fmts: Vec<Value> = (0..n as usize).map(|i| ctx.upvalue(i + 3)).collect();
    let len = ctx.len();
    match read_values(ctx, &s, &fmts, 1)? {
        Ok(k) if !ctx.arg(len).is_falsy() => Ok(NativeReturn::Return(k)),
        Ok(_) => {
            // The end: no values, and the file closes if `lines` opened it.
            if close {
                let _ = s.borrow_mut().close(&mut *ctx.vm.rt.out);
            }
            Ok(NativeReturn::Return(0))
        }
        Err(e) => Err(ctx.error(crate::stdlib::os_error_text(&e))),
    }
}

// ---- metamethods and opening ----------------------------------------------

fn f_tostring(ctx: &mut NativeCtx<'_>) -> R {
    let s = stream_arg(ctx, 0)?;
    let text = if s.borrow().is_closed() {
        "file (closed)".to_string()
    } else {
        format!("file (0x{:012x})", ctx.arg(0).identity().unwrap_or(0))
    };
    let v = ctx.new_str(text.as_bytes());
    ctx.ret1(v)
}

/// `__gc` and `__close`: close the file unless it is closed or standard.
fn f_gc(ctx: &mut NativeCtx<'_>) -> R {
    let s = stream_arg(ctx, 0)?;
    let open = { !s.borrow().is_closed() && !s.borrow().is_std() };
    if open {
        let _ = s.borrow_mut().close(&mut *ctx.vm.rt.out);
    }
    Ok(NativeReturn::Return(0))
}

/// Open the library: the `io` table, the `FILE*` metatable and the
/// standard files, which are also the default input and output.
pub(crate) fn open(vm: &mut crate::vm::Vm) {
    let io = crate::stdlib::open_lib(vm, "io", FUNCTIONS);
    let meta = vm.new_table();
    vm.pin(Value::Table(meta));
    let methods = vm.new_table();
    crate::stdlib::add_functions(vm, methods, METHODS);
    let set = |vm: &mut crate::vm::Vm, t: Gc<Table>, k: &str, v: Value| {
        let k = vm.new_str(k.as_bytes());
        vm.table_set(t, k, v);
    };
    set(vm, meta, "__index", Value::Table(methods));
    let name = vm.new_str(FILE_META);
    set(vm, meta, "__name", name);
    for (k, f) in [
        ("__tostring", f_tostring as NativeFn),
        ("__gc", f_gc),
        ("__close", f_gc),
    ] {
        let v = vm.register_native(k, f);
        set(vm, meta, k, v);
    }
    let reg = vm.rt.registry;
    set(vm, reg, "FILE*", Value::Table(meta));
    for (name, kind, r, w, key) in [
        ("stdin", Kind::Stdin, true, false, Some(INPUT)),
        ("stdout", Kind::Stdout, false, true, Some(OUTPUT)),
        ("stderr", Kind::Stderr, false, true, None),
    ] {
        let mut stream = Stream::new(kind, r, w);
        if name == "stderr" {
            stream.buffering = Buffering::No;
        }
        let rc = Rc::new(RefCell::new(stream));
        OPEN.with(|open| open.borrow_mut().push(Rc::downgrade(&rc)));
        let u = vm.new_userdata(LuaFile(rc));
        vm.set_metatable(Value::UserData(u), Some(meta));
        set(vm, io, name, Value::UserData(u));
        if let Some(key) = key {
            let key = std::str::from_utf8(key).expect("ASCII key");
            set(vm, reg, key, Value::UserData(u));
        }
    }
    vm.unpin(Value::Table(meta));
}
