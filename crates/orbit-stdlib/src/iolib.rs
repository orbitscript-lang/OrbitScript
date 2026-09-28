//! The I/O library (`liolib.c`).
//!
//! A file handle is a userdata holding a [`Stream`], which emulates the parts of a C
//! `FILE` the library relies on: buffered reads with one character of look-ahead,
//! buffered writes, and seeking that accounts for both buffers. The standard output
//! and error streams write straight to Rust's handles, so `io.write` and `print`
//! stay in order.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::process::{Child, ChildStdin, ChildStdout};

use orbit_core::Value;
use orbit_core::object::UserData;
use orbit_core::value::{TableRef, UserDataRef};
use orbit_vm::{Action, LuaResult, Vm};

use crate::oslib::{exec_result, file_error, file_result, os_path};

const FILEHANDLE: &[u8] = b"FILE*";
const IO_INPUT: &[u8] = b"_IO_input";
const IO_OUTPUT: &[u8] = b"_IO_output";
const IO_READLINE: &[u8] = b"_IO_readline";
const MAXARGLINE: usize = 250;
const BUFSIZE: usize = 4096;
const L_MAXLENNUM: usize = 200;

/// Opens the io library.
pub fn open_io(vm: &mut Vm) {
    let io = vm.new_lib(
        "io",
        &[
            ("close", io_close),
            ("flush", io_flush),
            ("input", io_input),
            ("lines", io_lines),
            ("open", io_open),
            ("output", io_output),
            ("popen", io_popen),
            ("read", io_read),
            ("tmpfile", io_tmpfile),
            ("type", io_type),
            ("write", io_write),
        ],
    );
    // The metatable for file handles, with the methods as its __index.
    let mt = vm.heap.new_table();
    let methods = vm.heap.new_table();
    let meth: [(&'static str, orbit_vm::NativeFn); 7] = [
        ("read", f_read),
        ("write", f_write),
        ("lines", f_lines),
        ("flush", f_flush),
        ("seek", f_seek),
        ("close", f_close),
        ("setvbuf", f_setvbuf),
    ];
    for (name, f) in meth {
        let v = vm.register(name, f);
        set(vm, methods, name.as_bytes(), v);
    }
    let metameth: [(&'static str, orbit_vm::NativeFn); 3] = [
        ("__gc", f_gc),
        ("__close", f_gc),
        ("__tostring", f_tostring),
    ];
    for (name, f) in metameth {
        let v = vm.register(name, f);
        set(vm, mt, name.as_bytes(), v);
    }
    set(vm, mt, b"__index", Value::Table(methods));
    let name = vm.heap.str_value("FILE*");
    set(vm, mt, b"__name", name);
    let reg = vm.registry();
    set(vm, reg, FILEHANDLE, Value::Table(mt));
    let readline = vm.register("for iterator", io_readline);
    set(vm, reg, IO_READLINE, readline);

    let std_files = [
        (Handle::Stdin, Some(IO_INPUT), "stdin"),
        (Handle::Stdout, Some(IO_OUTPUT), "stdout"),
        (Handle::Stderr, None, "stderr"),
    ];
    for (h, key, name) in std_files {
        let mut s = Stream::new(h);
        s.closef = CloseKind::NoClose;
        s.writable = key != Some(IO_INPUT);
        let f = new_stream(vm, s);
        if let Some(k) = key {
            set(vm, reg, k, f);
        }
        set(vm, io, name.as_bytes(), f);
    }
}

fn set(vm: &mut Vm, t: TableRef, k: &[u8], v: Value) {
    vm.heap.raw_set_name(t, k, v).expect("string key");
}

// ---- streams -------------------------------------------------------------------

enum Handle {
    File(File),
    Stdin,
    Stdout,
    Stderr,
    PipeRead(Child, ChildStdout),
    PipeWrite(Child, Option<ChildStdin>),
    None,
}

/// How a handle is closed (`closef` in `luaL_Stream`).
#[derive(Clone, Copy, PartialEq)]
enum CloseKind {
    /// Already closed.
    Closed,
    Regular,
    Popen,
    /// A standard file, which is never closed.
    NoClose,
}

#[derive(Clone, Copy, PartialEq)]
enum BufMode {
    No,
    Full,
    Line,
}

/// A C-like `FILE`.
pub(crate) struct Stream {
    h: Handle,
    closef: CloseKind,
    rbuf: Vec<u8>,
    rpos: usize,
    wbuf: Vec<u8>,
    mode: BufMode,
    bufsize: usize,
    /// The error indicator (`ferror`), with the error that set it.
    err: Option<io::Error>,
    /// Whether the mode allows writing. Like C streams, a write to a read-only file
    /// fails at once rather than when the buffer is flushed.
    writable: bool,
}

impl Stream {
    fn new(h: Handle) -> Self {
        Stream {
            h,
            closef: CloseKind::Regular,
            rbuf: Vec::new(),
            rpos: 0,
            wbuf: Vec::new(),
            mode: BufMode::Full,
            bufsize: BUFSIZE,
            err: None,
            writable: true,
        }
    }

    fn raw_read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            let r = match &mut self.h {
                Handle::File(f) => f.read(buf),
                Handle::Stdin => io::stdin().read(buf),
                Handle::PipeRead(_, out) => out.read(buf),
                Handle::Stdout | Handle::Stderr | Handle::PipeWrite(..) | Handle::None => {
                    Err(io::Error::from_raw_os_error(9)) // EBADF
                }
            };
            match r {
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                r => return r,
            }
        }
    }

    fn raw_write(&mut self, data: &[u8]) -> io::Result<()> {
        match &mut self.h {
            Handle::File(f) => f.write_all(data),
            Handle::Stdout => io::stdout().write_all(data),
            Handle::Stderr => io::stderr().write_all(data),
            Handle::PipeWrite(_, Some(inp)) => inp.write_all(data),
            _ => Err(io::Error::from_raw_os_error(9)),
        }
    }

    /// Writes out the write buffer.
    fn flush_wbuf(&mut self) -> io::Result<()> {
        if self.wbuf.is_empty() {
            return Ok(());
        }
        let data = std::mem::take(&mut self.wbuf);
        self.raw_write(&data)
    }

    fn flush(&mut self) -> io::Result<()> {
        let r = self.flush_wbuf();
        let r2 = match &mut self.h {
            Handle::Stdout => io::stdout().flush(),
            Handle::Stderr => io::stderr().flush(),
            Handle::File(f) => f.flush(),
            Handle::PipeWrite(_, Some(inp)) => inp.flush(),
            _ => Ok(()),
        };
        r.and(r2)
    }

    /// Drops unread buffered input, moving a seekable file back to the logical
    /// position.
    fn drop_rbuf(&mut self) -> io::Result<()> {
        let unread = self.rbuf.len() - self.rpos;
        self.rbuf.clear();
        self.rpos = 0;
        if unread > 0
            && let Handle::File(f) = &mut self.h
        {
            f.seek(SeekFrom::Current(-(unread as i64)))?;
        }
        Ok(())
    }

    /// Refills the read buffer; `false` at end of file or on error.
    fn fill(&mut self) -> bool {
        if let Err(e) = self.flush_wbuf() {
            self.err = Some(e);
            return false;
        }
        if matches!(self.h, Handle::Stdin) {
            // Flush pending output before waiting for input, as C streams do.
            let _ = io::stdout().flush();
        }
        let mut buf = vec![0u8; self.bufsize.max(1)];
        match self.raw_read(&mut buf) {
            Ok(0) => false,
            Ok(n) => {
                buf.truncate(n);
                self.rbuf = buf;
                self.rpos = 0;
                true
            }
            Err(e) => {
                self.err = Some(e);
                false
            }
        }
    }

    fn getc(&mut self) -> Option<u8> {
        if self.rpos >= self.rbuf.len() && !self.fill() {
            return None;
        }
        let c = self.rbuf[self.rpos];
        self.rpos += 1;
        Some(c)
    }

    fn ungetc(&mut self, c: Option<u8>) {
        if let Some(c) = c {
            if self.rpos > 0 {
                self.rpos -= 1;
                self.rbuf[self.rpos] = c;
            } else {
                self.rbuf.insert(0, c);
            }
        }
    }

    fn read_line(&mut self, chop: bool) -> (Vec<u8>, bool) {
        let mut out = Vec::new();
        loop {
            if self.rpos >= self.rbuf.len() && !self.fill() {
                let ok = !out.is_empty();
                return (out, ok);
            }
            let avail = &self.rbuf[self.rpos..];
            match avail.iter().position(|&c| c == b'\n') {
                Some(i) => {
                    out.extend_from_slice(&avail[..i]);
                    if !chop {
                        out.push(b'\n');
                    }
                    self.rpos += i + 1;
                    return (out, true);
                }
                None => {
                    out.extend_from_slice(avail);
                    self.rpos = self.rbuf.len();
                }
            }
        }
    }

    fn read_chars(&mut self, n: usize) -> Vec<u8> {
        let mut out = Vec::new();
        while out.len() < n {
            if self.rpos >= self.rbuf.len() && !self.fill() {
                break;
            }
            let take = (n - out.len()).min(self.rbuf.len() - self.rpos);
            out.extend_from_slice(&self.rbuf[self.rpos..self.rpos + take]);
            self.rpos += take;
        }
        out
    }

    fn read_all(&mut self) -> Vec<u8> {
        let mut out = self.rbuf[self.rpos..].to_vec();
        self.rbuf.clear();
        self.rpos = 0;
        while self.fill() {
            out.extend_from_slice(&self.rbuf);
            self.rbuf.clear();
            self.rpos = 0;
        }
        out
    }

    fn write(&mut self, data: &[u8]) -> io::Result<()> {
        if !self.writable {
            return Err(io::Error::from_raw_os_error(9)); // EBADF
        }
        self.drop_rbuf()?;
        match self.h {
            // Unbuffered here: Rust's own stdout buffer is shared with `print`.
            Handle::Stdout | Handle::Stderr => return self.raw_write(data),
            _ => {}
        }
        self.wbuf.extend_from_slice(data);
        let flush = match self.mode {
            BufMode::No => true,
            BufMode::Line => data.contains(&b'\n'),
            BufMode::Full => self.wbuf.len() >= self.bufsize,
        };
        if flush { self.flush_wbuf() } else { Ok(()) }
    }

    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        self.flush_wbuf()?;
        let unread = (self.rbuf.len() - self.rpos) as i64;
        let pos = match pos {
            SeekFrom::Current(off) => SeekFrom::Current(off - unread),
            p => p,
        };
        match &mut self.h {
            Handle::File(f) => {
                let r = f.seek(pos)?;
                self.rbuf.clear();
                self.rpos = 0;
                Ok(r)
            }
            _ => Err(io::Error::from_raw_os_error(29)), // ESPIPE
        }
    }

    /// Closes a regular file.
    fn close_file(&mut self) -> io::Result<()> {
        let r = self.flush_wbuf();
        self.h = Handle::None;
        r
    }

    /// Closes a pipe and waits for the command.
    fn close_pipe(&mut self) -> io::Result<std::process::ExitStatus> {
        let _ = self.flush_wbuf();
        match std::mem::replace(&mut self.h, Handle::None) {
            Handle::PipeRead(mut child, out) => {
                drop(out);
                child.wait()
            }
            Handle::PipeWrite(mut child, inp) => {
                drop(inp);
                child.wait()
            }
            _ => Err(io::Error::from_raw_os_error(9)),
        }
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        let _ = self.flush_wbuf();
    }
}

fn new_stream(vm: &mut Vm, s: Stream) -> Value {
    let u = vm.heap.new_userdata(s, 0);
    let reg = vm.registry();
    if let Value::Table(mt) = vm.heap.raw_get_name(reg, FILEHANDLE) {
        vm.heap.set_userdata_metatable(u, Some(mt));
    }
    Value::UserData(u)
}

fn with_stream<R>(vm: &mut Vm, u: UserDataRef, f: impl FnOnce(&mut Stream) -> R) -> R {
    vm.heap.update(u, |ud: &mut UserData| {
        f(ud.downcast_mut::<Stream>().expect("file handle"))
    })
}

fn is_stream(vm: &Vm, v: Value) -> Option<UserDataRef> {
    match v {
        Value::UserData(u) if vm.heap.get(u).downcast_ref::<Stream>().is_some() => Some(u),
        _ => None,
    }
}

fn is_closed(vm: &Vm, u: UserDataRef) -> bool {
    vm.heap
        .get(u)
        .downcast_ref::<Stream>()
        .is_none_or(|s| s.closef == CloseKind::Closed)
}

/// `luaL_checkudata(L, arg, "FILE*")`.
fn tolstream(vm: &mut Vm, arg: usize) -> LuaResult<UserDataRef> {
    match is_stream(vm, vm.arg(arg)) {
        Some(u) => Ok(u),
        None => Err(vm.type_arg_error(arg, "FILE*")),
    }
}

/// An open file argument (`tofile`).
fn tofile(vm: &mut Vm, arg: usize) -> LuaResult<UserDataRef> {
    let u = tolstream(vm, arg)?;
    if is_closed(vm, u) {
        return Err(vm.error("attempt to use a closed file"));
    }
    Ok(u)
}

/// Closes the stream at `u` (`aux_close`), pushing its results.
fn aux_close(vm: &mut Vm, u: UserDataRef) -> LuaResult<Action> {
    let kind = with_stream(vm, u, |s| {
        std::mem::replace(&mut s.closef, CloseKind::Closed)
    });
    match kind {
        CloseKind::Regular => {
            let r = with_stream(vm, u, |s| s.close_file());
            file_result(vm, r, None)
        }
        CloseKind::Popen => {
            let r = with_stream(vm, u, |s| s.close_pipe());
            exec_result(vm, r)
        }
        CloseKind::NoClose => {
            with_stream(vm, u, |s| s.closef = CloseKind::NoClose);
            let m = vm.heap.str_value("cannot close standard file");
            vm.push(Value::Nil);
            vm.push(m);
            Ok(Action::Return(2))
        }
        CloseKind::Closed => Ok(Action::Return(0)),
    }
}

fn io_type(vm: &mut Vm) -> LuaResult<Action> {
    vm.check_any(1)?;
    let r = match is_stream(vm, vm.arg(1)) {
        None => Value::Nil,
        Some(u) if is_closed(vm, u) => vm.heap.str_value("closed file"),
        Some(_) => vm.heap.str_value("file"),
    };
    vm.push(r);
    Ok(Action::Return(1))
}

fn f_tostring(vm: &mut Vm) -> LuaResult<Action> {
    let u = tolstream(vm, 1)?;
    let s = if is_closed(vm, u) {
        "file (closed)".to_string()
    } else {
        format!(
            "file ({:#x})",
            orbit_vm::value_addr(&vm.heap, Value::UserData(u))
        )
    };
    let v = vm.heap.str_value(s);
    vm.push(v);
    Ok(Action::Return(1))
}

fn f_close(vm: &mut Vm) -> LuaResult<Action> {
    let u = tofile(vm, 1)?;
    aux_close(vm, u)
}

fn io_close(vm: &mut Vm) -> LuaResult<Action> {
    if vm.top() == 0 {
        let reg = vm.registry();
        let out = vm.heap.raw_get_name(reg, IO_OUTPUT);
        vm.push(out);
    }
    f_close(vm)
}

fn f_gc(vm: &mut Vm) -> LuaResult<Action> {
    let u = tolstream(vm, 1)?;
    if !is_closed(vm, u) {
        aux_close(vm, u)?;
    }
    Ok(Action::Return(0))
}

/// Checks an `fopen` mode (`l_checkmode`).
fn check_mode(mode: &[u8]) -> bool {
    let mut m = mode;
    match m.first() {
        Some(b'r' | b'w' | b'a') => m = &m[1..],
        _ => return false,
    }
    if m.first() == Some(&b'+') {
        m = &m[1..];
    }
    m.iter().all(|&c| c == b'b')
}

/// `fopen(name, mode)`, for a mode that passed [`check_mode`].
fn fopen(name: &[u8], mode: &[u8]) -> io::Result<Stream> {
    let plus = mode.contains(&b'+');
    let mut o = std::fs::OpenOptions::new();
    match mode[0] {
        b'r' => o.read(true).write(plus),
        b'w' => o.write(true).create(true).truncate(true).read(plus),
        _ => o.append(true).create(true).read(plus),
    };
    let mut s = Stream::new(Handle::File(o.open(os_path(name))?));
    s.writable = mode[0] != b'r' || plus;
    Ok(s)
}

fn string_arg(vm: &mut Vm, arg: usize) -> LuaResult<Vec<u8>> {
    let s = vm.check_string(arg)?;
    Ok(vm.heap.string(s).as_bytes().to_vec())
}

fn opt_string_arg(vm: &mut Vm, arg: usize, dft: &[u8]) -> LuaResult<Vec<u8>> {
    Ok(match vm.opt_string(arg)? {
        Some(s) => vm.heap.string(s).as_bytes().to_vec(),
        None => dft.to_vec(),
    })
}

fn io_open(vm: &mut Vm) -> LuaResult<Action> {
    let name = string_arg(vm, 1)?;
    let mode = opt_string_arg(vm, 2, b"r")?;
    if !check_mode(&mode) {
        return Err(vm.arg_error(2, "invalid mode"));
    }
    match fopen(&name, &mode) {
        Ok(s) => {
            let v = new_stream(vm, s);
            vm.push(v);
            Ok(Action::Return(1))
        }
        Err(e) => file_error(vm, &e, Some(&name)),
    }
}

/// Opens `name` or raises an error (`opencheck`).
fn opencheck(vm: &mut Vm, name: &[u8], mode: &[u8]) -> LuaResult<Value> {
    match fopen(name, mode) {
        Ok(s) => Ok(new_stream(vm, s)),
        Err(e) => {
            let msg = format!(
                "cannot open file '{}' ({})",
                String::from_utf8_lossy(name),
                orbit_vm::baselib::io_error_text(&e)
            );
            Err(vm.error(msg))
        }
    }
}

fn io_popen(vm: &mut Vm) -> LuaResult<Action> {
    let cmd = string_arg(vm, 1)?;
    let mode = opt_string_arg(vm, 2, b"r")?;
    if mode != b"r" && mode != b"w" {
        return Err(vm.arg_error(2, "invalid mode"));
    }
    let _ = io::stdout().flush();
    let mut c = crate::oslib::shell(&cmd);
    let h = if mode == b"r" {
        c.stdout(std::process::Stdio::piped())
            .spawn()
            .map(|mut ch| {
                let out = ch.stdout.take().expect("piped stdout");
                Handle::PipeRead(ch, out)
            })
    } else {
        c.stdin(std::process::Stdio::piped()).spawn().map(|mut ch| {
            let inp = ch.stdin.take();
            Handle::PipeWrite(ch, inp)
        })
    };
    match h {
        Ok(h) => {
            let mut s = Stream::new(h);
            s.closef = CloseKind::Popen;
            s.writable = mode == b"w";
            let v = new_stream(vm, s);
            vm.push(v);
            Ok(Action::Return(1))
        }
        Err(e) => file_error(vm, &e, Some(&cmd)),
    }
}

fn io_tmpfile(vm: &mut Vm) -> LuaResult<Action> {
    match crate::oslib::make_temp() {
        Ok((path, f)) => {
            let _ = std::fs::remove_file(&path);
            let v = new_stream(vm, Stream::new(Handle::File(f)));
            vm.push(v);
            Ok(Action::Return(1))
        }
        Err(e) => file_error(vm, &e, None),
    }
}

/// The default input or output file (`getiofile`).
fn getiofile(vm: &mut Vm, key: &[u8]) -> LuaResult<UserDataRef> {
    let reg = vm.registry();
    let v = vm.heap.raw_get_name(reg, key);
    match is_stream(vm, v) {
        Some(u) if !is_closed(vm, u) => Ok(u),
        _ => {
            let which = String::from_utf8_lossy(&key[4..]).into_owned();
            Err(vm.error(format!("default {which} file is closed")))
        }
    }
}

fn g_iofile(vm: &mut Vm, key: &[u8], mode: &[u8]) -> LuaResult<Action> {
    let reg = vm.registry();
    let a = vm.arg(1);
    if !a.is_nil() {
        let f = match a {
            Value::Str(s) => {
                let name = vm.heap.string(s).as_bytes().to_vec();
                opencheck(vm, &name, mode)?
            }
            Value::Int(_) | Value::Float(_) => {
                let name = orbit_vm::number::fmt_number(a).into_bytes();
                opencheck(vm, &name, mode)?
            }
            _ => {
                tofile(vm, 1)?;
                a
            }
        };
        set(vm, reg, key, f);
    }
    let v = vm.heap.raw_get_name(reg, key);
    vm.push(v);
    Ok(Action::Return(1))
}

fn io_input(vm: &mut Vm) -> LuaResult<Action> {
    g_iofile(vm, IO_INPUT, b"r")
}

fn io_output(vm: &mut Vm) -> LuaResult<Action> {
    g_iofile(vm, IO_OUTPUT, b"w")
}

/// The iterator for `lines` (`aux_lines`): a closure over the file, the number of
/// formats, whether to close the file at the end, and the formats.
fn aux_lines(vm: &mut Vm, file: Value, toclose: bool) -> LuaResult<Value> {
    let n = vm.top() - 1;
    if n > MAXARGLINE {
        return Err(vm.arg_error(MAXARGLINE + 2, "too many arguments"));
    }
    let mut upvals = vec![file, Value::Int(n as i64), Value::Bool(toclose)];
    for i in 2..=vm.top() {
        upvals.push(vm.arg(i));
    }
    let reg = vm.registry();
    let f = vm.heap.raw_get_name(reg, IO_READLINE);
    Ok(vm.native_closure_of(f, upvals))
}

fn f_lines(vm: &mut Vm) -> LuaResult<Action> {
    tofile(vm, 1)?;
    let file = vm.arg(1);
    let it = aux_lines(vm, file, false)?;
    vm.push(it);
    Ok(Action::Return(1))
}

fn io_lines(vm: &mut Vm) -> LuaResult<Action> {
    if vm.top() == 0 {
        vm.push(Value::Nil);
    }
    let toclose = if vm.arg(1).is_nil() {
        let reg = vm.registry();
        let inp = vm.heap.raw_get_name(reg, IO_INPUT);
        vm.set(1, inp);
        tofile(vm, 1)?;
        false
    } else {
        let name = string_arg(vm, 1)?;
        let f = opencheck(vm, &name, b"r")?;
        vm.set(1, f);
        true
    };
    let file = vm.arg(1);
    let it = aux_lines(vm, file, toclose)?;
    vm.push(it);
    if toclose {
        vm.push(Value::Nil);
        vm.push(Value::Nil);
        vm.push(file);
        Ok(Action::Return(4))
    } else {
        Ok(Action::Return(1))
    }
}

// ---- reading --------------------------------------------------------------------

fn test2(s: &mut Stream, c: &mut Option<u8>, buf: &mut Vec<u8>, set: &[u8; 2]) -> bool {
    match *c {
        Some(ch) if ch == set[0] || ch == set[1] => nextc(s, c, buf),
        _ => false,
    }
}

fn nextc(s: &mut Stream, c: &mut Option<u8>, buf: &mut Vec<u8>) -> bool {
    if buf.len() >= L_MAXLENNUM {
        buf[0] = 0; // invalidate the result, as the reference does
        return false;
    }
    buf.push(c.unwrap_or(0));
    *c = s.getc();
    true
}

fn readdigits(s: &mut Stream, c: &mut Option<u8>, buf: &mut Vec<u8>, hex: bool) -> usize {
    let mut count = 0;
    while let Some(ch) = *c {
        let ok = if hex {
            ch.is_ascii_hexdigit()
        } else {
            ch.is_ascii_digit()
        };
        if !(ok && nextc(s, c, buf)) {
            break;
        }
        count += 1;
    }
    count
}

/// Reads a numeral prefix (`read_number`), returning its text.
fn read_numeral(s: &mut Stream) -> Vec<u8> {
    let mut buf = Vec::new();
    let mut c = s.getc();
    while matches!(c, Some(b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c)) {
        c = s.getc();
    }
    let mut count = 0;
    let mut hex = false;
    test2(s, &mut c, &mut buf, b"-+");
    if test2(s, &mut c, &mut buf, b"00") {
        if test2(s, &mut c, &mut buf, b"xX") {
            hex = true;
        } else {
            count = 1;
        }
    }
    count += readdigits(s, &mut c, &mut buf, hex);
    if test2(s, &mut c, &mut buf, b"..") {
        count += readdigits(s, &mut c, &mut buf, hex);
    }
    if count > 0 && test2(s, &mut c, &mut buf, if hex { b"pP" } else { b"eE" }) {
        test2(s, &mut c, &mut buf, b"-+");
        readdigits(s, &mut c, &mut buf, false);
    }
    s.ungetc(c);
    buf
}

/// One item read by `g_read`.
enum Item {
    Str(Vec<u8>, bool),
    Num(Option<Value>),
}

fn g_read(vm: &mut Vm, u: UserDataRef, first: usize) -> LuaResult<Action> {
    let nargs = vm.top() + 1 - first;
    with_stream(vm, u, |s| s.err = None);
    let mut results = Vec::new();
    let mut success = true;
    if nargs == 0 {
        let (l, ok) = with_stream(vm, u, |s| s.read_line(true));
        results.push(Item::Str(l, ok));
        success = ok;
    } else {
        vm.check_stack(nargs + 20)?;
        let mut n = first;
        while n < first + nargs && success {
            let item = match vm.arg(n) {
                Value::Int(_) | Value::Float(_) => {
                    let l = vm.check_integer(n)? as u64;
                    if l == 0 {
                        let eof = with_stream(vm, u, |s| {
                            let c = s.getc();
                            s.ungetc(c);
                            c.is_none()
                        });
                        Item::Str(Vec::new(), !eof)
                    } else {
                        let l = usize::try_from(l).unwrap_or(usize::MAX);
                        let d = with_stream(vm, u, |s| s.read_chars(l));
                        let ok = !d.is_empty();
                        Item::Str(d, ok)
                    }
                }
                _ => {
                    let p = string_arg(vm, n)?;
                    let p = p.strip_prefix(b"*").unwrap_or(&p);
                    match p.first() {
                        Some(b'n') => {
                            let text = with_stream(vm, u, read_numeral);
                            Item::Num(orbit_vm::number::str_to_number(&text))
                        }
                        Some(b'l') => {
                            let (l, ok) = with_stream(vm, u, |s| s.read_line(true));
                            Item::Str(l, ok)
                        }
                        Some(b'L') => {
                            let (l, ok) = with_stream(vm, u, |s| s.read_line(false));
                            Item::Str(l, ok)
                        }
                        Some(b'a') => Item::Str(with_stream(vm, u, |s| s.read_all()), true),
                        _ => return Err(vm.arg_error(n, "invalid format")),
                    }
                }
            };
            success = match &item {
                Item::Str(_, ok) => *ok,
                Item::Num(v) => v.is_some(),
            };
            results.push(item);
            n += 1;
        }
    }
    if let Some(e) = with_stream(vm, u, |s| s.err.take()) {
        return file_error(vm, &e, None);
    }
    let count = results.len();
    vm.check_stack(count)?;
    for (i, item) in results.into_iter().enumerate() {
        let v = if i + 1 == count && !success {
            Value::Nil
        } else {
            match item {
                Item::Str(d, _) => vm.heap.str_value(d),
                Item::Num(v) => v.unwrap_or(Value::Nil),
            }
        };
        vm.push(v);
    }
    Ok(Action::Return(count))
}

fn io_read(vm: &mut Vm) -> LuaResult<Action> {
    let u = getiofile(vm, IO_INPUT)?;
    g_read(vm, u, 1)
}

fn f_read(vm: &mut Vm) -> LuaResult<Action> {
    let u = tofile(vm, 1)?;
    g_read(vm, u, 2)
}

fn io_readline(vm: &mut Vm) -> LuaResult<Action> {
    let file = vm.upvalue(1);
    let Some(u) = is_stream(vm, file) else {
        return Err(vm.error("file is already closed"));
    };
    if is_closed(vm, u) {
        return Err(vm.error("file is already closed"));
    }
    let n = match vm.upvalue(2) {
        Value::Int(n) => n as usize,
        _ => 0,
    };
    vm.set_top(0);
    vm.check_stack(n + 1)?;
    vm.push(file);
    for i in 1..=n {
        let f = vm.upvalue(3 + i);
        vm.push(f);
    }
    let Action::Return(nres) = g_read(vm, u, 2)? else {
        unreachable!()
    };
    let top = vm.top();
    let firstv = vm.get(top - nres + 1);
    if firstv.is_truthy() {
        return Ok(Action::Return(nres));
    }
    if nres > 1 {
        let msg = vm.get(top - nres + 2);
        let m = match msg {
            Value::Str(s) => vm.heap.string(s).as_bytes().to_vec(),
            _ => Vec::new(),
        };
        return Err(vm.error(m));
    }
    if vm.upvalue(3).is_truthy() {
        vm.set_top(0);
        aux_close(vm, u)?;
    }
    vm.set_top(0);
    Ok(Action::Return(0))
}

// ---- writing and the rest --------------------------------------------------------

fn g_write(vm: &mut Vm, u: UserDataRef, first: usize) -> LuaResult<Action> {
    let nargs = vm.top() + 1 - first;
    let mut total = 0i64;
    for arg in first..first + nargs {
        let v = vm.arg(arg);
        let data = match v {
            Value::Int(_) | Value::Float(_) => orbit_vm::number::fmt_number(v).into_bytes(),
            _ => string_arg(vm, arg)?,
        };
        if let Err(e) = with_stream(vm, u, |s| s.write(&data)) {
            file_error(vm, &e, None)?;
            vm.push(Value::Int(total));
            return Ok(Action::Return(4));
        }
        total += data.len() as i64;
    }
    // The file handle is the result.
    vm.push(Value::UserData(u));
    Ok(Action::Return(1))
}

fn io_write(vm: &mut Vm) -> LuaResult<Action> {
    let u = getiofile(vm, IO_OUTPUT)?;
    g_write(vm, u, 1)
}

fn f_write(vm: &mut Vm) -> LuaResult<Action> {
    let u = tofile(vm, 1)?;
    g_write(vm, u, 2)
}

fn f_seek(vm: &mut Vm) -> LuaResult<Action> {
    let u = tofile(vm, 1)?;
    let op = vm.check_option(2, Some("cur"), &["set", "cur", "end"])?;
    let off = vm.opt_integer(3, 0)?;
    let pos = match op {
        0 => {
            if off < 0 {
                let e = io::Error::from_raw_os_error(22); // EINVAL
                return file_error(vm, &e, None);
            }
            SeekFrom::Start(off as u64)
        }
        1 => SeekFrom::Current(off),
        _ => SeekFrom::End(off),
    };
    match with_stream(vm, u, |s| s.seek(pos)) {
        Ok(p) => {
            vm.push(Value::Int(p as i64));
            Ok(Action::Return(1))
        }
        Err(e) => file_error(vm, &e, None),
    }
}

fn f_setvbuf(vm: &mut Vm) -> LuaResult<Action> {
    let u = tofile(vm, 1)?;
    let op = vm.check_option(2, None, &["no", "full", "line"])?;
    let sz = vm.opt_integer(3, BUFSIZE as i64)?;
    let mode = [BufMode::No, BufMode::Full, BufMode::Line][op];
    let r = with_stream(vm, u, |s| {
        let r = s.flush_wbuf();
        s.mode = mode;
        s.bufsize = usize::try_from(sz).unwrap_or(BUFSIZE).max(1);
        r
    });
    file_result(vm, r, None)
}

fn f_flush(vm: &mut Vm) -> LuaResult<Action> {
    let u = tofile(vm, 1)?;
    let r = with_stream(vm, u, |s| s.flush());
    file_result(vm, r, None)
}

fn io_flush(vm: &mut Vm) -> LuaResult<Action> {
    let u = getiofile(vm, IO_OUTPUT)?;
    let r = with_stream(vm, u, |s| s.flush());
    file_result(vm, r, None)
}
