//! The `io` library: file handles as the Lua C API defines them, a full
//! userdata whose memory is a `luaL_Stream` (`FILE *f; lua_CFunction
//! closef`) with the `FILE*` metatable (ADR-15).
//!
//! Every handle's stream is a C library stream ([`CFile`]) and every read
//! and write goes through it, so a C module that takes a handle with
//! `luaL_checkudata(L, i, LUA_FILEHANDLE)` shares its buffer and position
//! with scripts, as in Lua. Rust still opens files, pipes and temporary
//! files and hands the descriptor to the C library, so paths, errors and
//! the shell are the same as elsewhere in `orbit-std`.
//!
//! What Rust keeps about a handle (its stream, a pipe's child process) is
//! in the isolate's [`IoState`], by the handle's memory, never in the
//! memory C sees: Rust writes `f` and `closef` there for C and never reads
//! them back for its own handles. A `luaL_Stream` a C module made (not in
//! [`IoState`]) is a foreign stream, which the `io` functions use through
//! its `f` and close through its `closef`, once `orbit-capi` has said C
//! code may run ([`share_streams_with_c`]).
//!
//! On Windows a stream opened without `b`, a pipe and a standard stream
//! are the C runtime's text streams when the isolate's [`LineEndings`]
//! translate.

use std::cell::{Cell, RefCell};
use std::ffi::{CString, c_void};
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom};
use std::process::{Child, Stdio};
use std::rc::{Rc, Weak};

use orbit::stack::{
    Any, Arg, Nil, REGISTRY_INDEX, Rest, Results, Return, Stack, StackResult, Type, upvalue_index,
};
use orbit::text::END_OF_TEXT;
use orbit::{Error, LineEndings, OrbitString, Scope, Table, native};

use crate::stdio::{self, Buffering, CFile, Locked, STREAM_BLOCK_SIZE, Std, Whence};
use crate::sys::{
    exit_results, file_failure, flush_stdout, os_error_code, os_error_text, os_path, shell,
};

/// The C library's `BUFSIZ`, the default buffer size for `setvbuf`.
const BUFSIZ: usize = 8192;

/// The most formats `lines` takes, as in the reference.
const MAX_LINE_FORMATS: usize = 250;

/// The longest numeral `read("n")` reads.
const MAX_NUMERAL: usize = 200;

/// The handles' type name, also the registry key of their metatable
/// (`LUA_FILEHANDLE`).
const FILE_TYPE: &str = "FILE*";

/// What a handle's stream is.
#[derive(Debug)]
enum Kind {
    File,
    Std(Std),
    /// `io.popen(cmd, "r")` or `"w"`: the child, waited for at close.
    Pipe(Child),
}

/// One of our handles, as Rust knows it.
#[derive(Debug)]
struct Stream {
    /// `None` once closed.
    file: Option<CFile>,
    kind: Kind,
    /// The handle's `luaL_Stream` block, which C sees.
    block: usize,
    /// The identity of the `FILE*` metatable it was made with, which it
    /// must still have to be a file (as `luaL_checkudata` checks).
    meta: u64,
    /// A buffer reads fill before their bytes become a string, kept for
    /// the next read.
    spare: Vec<u8>,
}

/// The most a handle keeps in [`Stream::spare`] between reads.
const SPARE_MAX: usize = 8 * 1024;

type Shared = Rc<RefCell<Stream>>;

/// A C function C code can call as an open handle's `closef`, and the
/// way to call a foreign stream's `closef` ([`share_streams_with_c`]).
pub type CallCloseF = fn(&mut Stack<'_>, i32, usize) -> StackResult<Return>;

/// The `io` library's per-isolate state.
#[derive(Debug, Default)]
struct IoState {
    /// The handles made while no C code could run, whose blocks get
    /// `orbit-capi`'s `closef` when it installs ([`share_streams_with_c`]).
    early: RefCell<Vec<Weak<RefCell<Stream>>>>,
    /// The identity of the `FILE*` metatable.
    meta: Cell<u64>,
    /// What an open handle's `closef` holds: `orbit-capi`'s C function,
    /// or [`stdio::no_closef_address`] while no C code can run.
    closef: Cell<usize>,
    /// How to call a foreign stream's `closef`; `None` while no C code can
    /// run, when blocks we didn't make are no files.
    call_closef: Cell<Option<CallCloseF>>,
    /// Whether the isolate's standard output is the C library's `stdout`
    /// ([`stdio::CStdout`]), so `io.stdout` can use it directly.
    c_stdout: Cell<bool>,
}

/// Run `f` on the isolate's [`IoState`], made on first use.
fn io_state<R>(s: &mut Stack<'_>, f: impl FnOnce(&IoState) -> R) -> R {
    s.extension(|io: &mut IoState| f(io))
}

/// Let C code and the `io` library share streams: open handles get
/// `closef` (a C function that closes the handle at index 1), and a
/// `luaL_Stream` C code made with the `FILE*` metatable becomes a file
/// `io` uses, closing it through `call_closef`. `orbit-capi` calls this
/// when it installs.
pub fn share_streams_with_c(s: &mut Stack<'_>, closef: usize, call_closef: CallCloseF) {
    let early = io_state(s, |io| {
        io.closef.set(closef);
        io.call_closef.set(Some(call_closef));
        io.early.take()
    });
    for st in early.iter().filter_map(Weak::upgrade) {
        let st = st.borrow();
        if let Some(f) = &st.file {
            stdio::write_stream_block(st.block as *mut c_void, f.as_raw(), closef);
        }
    }
}

/// Record that the isolate's standard output is the C library's `stdout`.
pub(crate) fn set_c_stdout(s: &mut Stack<'_>) {
    io_state(s, |io| io.c_stdout.set(true));
}

/// A file handle argument.
#[derive(Debug, Clone)]
enum Handle {
    Ours(Shared),
    /// A `luaL_Stream` C code made, by its block's address.
    Foreign(usize),
}

/// The handle at `idx`, if it is a userdata with the `FILE*` metatable
/// that we made, or one C code made while C code can run
/// (`luaL_testudata`).
fn handle_at(s: &mut Stack<'_>, idx: i32) -> Option<Handle> {
    let idx = s.abs_index(idx);
    let meta = s.metatable_identity(idx)?;
    // Ours carry their state where only Rust reaches it, so a script
    // can't pass anything else off as one.
    if let Some(st) = s.userdata_host::<Shared>(idx) {
        return (st.borrow().meta == meta).then(|| Handle::Ours(st.clone()));
    }
    if s.type_at(idx) != Type::Userdata {
        return None;
    }
    let block = s.to_userdata(idx) as usize;
    let foreign =
        s.find_extension(|io: &IoState| meta == io.meta.get() && io.call_closef.get().is_some())?;
    if foreign && block != 0 && s.raw_len(idx) as usize >= STREAM_BLOCK_SIZE {
        Some(Handle::Foreign(block))
    } else {
        None
    }
}

/// A file handle parameter: Lua's type error naming `FILE*` otherwise.
#[derive(Debug, Clone)]
struct FileArg(Handle);

impl<'a> Arg<'a> for FileArg {
    type Checked = FileArg;

    fn check(s: &mut Stack<'_>, i: i32) -> StackResult<FileArg> {
        match handle_at(s, i) {
            Some(h) => Ok(FileArg(h)),
            None => Err(s.type_error(i, FILE_TYPE)),
        }
    }

    fn get(_: &'a Stack<'_>, _: i32, v: FileArg) -> FileArg {
        v
    }
}

impl Handle {
    fn is_closed(&self) -> bool {
        match self {
            Handle::Ours(st) => st.borrow().file.is_none(),
            Handle::Foreign(block) => stdio::read_stream_block(*block as *mut c_void).1 == 0,
        }
    }

    fn is_std(&self) -> bool {
        matches!(self, Handle::Ours(st) if matches!(st.borrow().kind, Kind::Std(_)))
    }

    /// Whether this is `io.stdout` with the isolate's standard output not
    /// the C library's, so writes go through the isolate's writer.
    fn routed_stdout(&self, s: &mut Stack<'_>) -> bool {
        match self {
            Handle::Ours(st) if matches!(st.borrow().kind, Kind::Std(Std::Out)) => {
                s.stdout_redirected() || !io_state(s, |io| io.c_stdout.get())
            }
            _ => false,
        }
    }

    /// A buffer for a read: the handle's spare one, empty.
    fn take_buffer(&self) -> Vec<u8> {
        match self {
            Handle::Ours(st) => std::mem::take(&mut st.borrow_mut().spare),
            Handle::Foreign(_) => Vec::new(),
        }
    }

    /// Give back [`Handle::take_buffer`]'s buffer, unless it grew large.
    fn give_back_buffer(&self, mut buf: Vec<u8>) {
        if let Handle::Ours(st) = self {
            if buf.capacity() <= SPARE_MAX {
                buf.clear();
                st.borrow_mut().spare = buf;
            }
        }
    }

    /// Run `f` on the open stream; `None` if closed.
    fn with_file<R>(&self, f: impl FnOnce(&mut CFile) -> R) -> Option<R> {
        match self {
            Handle::Ours(st) => st.borrow_mut().file.as_mut().map(f),
            Handle::Foreign(block) => {
                let block = *block as *mut c_void;
                if stdio::read_stream_block(block).1 == 0 {
                    return None;
                }
                CFile::foreign(block).as_mut().map(f)
            }
        }
    }
}

/// The handle, which must be open.
fn check_open(s: &mut Stack<'_>, h: &Handle) -> StackResult<()> {
    if h.is_closed() {
        return Err(s.error("attempt to use a closed file"));
    }
    Ok(())
}

fn closed_error() -> io::Error {
    crate::crt::system_error(libc::EBADF)
}

// ---- handles ---------------------------------------------------------------

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

/// Push a new handle for `file`.
fn push_file(s: &mut Stack<'_>, file: CFile, kind: Kind) -> StackResult<Shared> {
    let (closef, meta, early) = io_state(s, |io| {
        if io.closef.get() == 0 {
            io.closef.set(stdio::no_closef_address());
        }
        (
            io.closef.get(),
            io.meta.get(),
            io.call_closef.get().is_none(),
        )
    });
    let raw = file.as_raw();
    let st = Rc::new(RefCell::new(Stream {
        file: Some(file),
        kind,
        block: 0,
        meta,
        spare: Vec::new(),
    }));
    let block = s.new_userdata_with_host(STREAM_BLOCK_SIZE, 0, st.clone())?;
    st.borrow_mut().block = block as usize;
    stdio::write_stream_block(block, raw, closef);
    s.raw_get_field(REGISTRY_INDEX, FILE_TYPE.as_bytes())?;
    s.set_metatable(-2)?;
    if early {
        io_state(s, |io| {
            let mut early = io.early.borrow_mut();
            // Forget closed handles now and then.
            if early.len() == early.capacity() {
                early.retain(|w| w.strong_count() > 0);
            }
            early.push(Rc::downgrade(&st));
        });
    }
    Ok(st)
}

/// Push the default input or output handle, which must be open.
fn push_default(s: &mut Stack<'_>, input: bool) -> StackResult<Handle> {
    s.raw_get_field(REGISTRY_INDEX, default_key(input))?;
    match handle_at(s, -1).filter(|h| !h.is_closed()) {
        Some(h) => Ok(h),
        None => {
            let what = if input { "input" } else { "output" };
            Err(s.error(format!("default {what} file is closed").as_str()))
        }
    }
}

/// `io.open`'s mode: `[rwa]%+?b*`. Gives the options and the mode for the
/// C library, `b` and all.
fn open_options(mode: &[u8]) -> Option<(OpenOptions, CString)> {
    let (&first, rest) = mode.split_first()?;
    let (plus, rest) = match rest.first() {
        Some(b'+') => (true, &rest[1..]),
        _ => (false, rest),
    };
    if !rest.iter().all(|&b| b == b'b') {
        return None;
    }
    let mut o = OpenOptions::new();
    match first {
        b'r' => o.read(true).write(plus),
        b'w' => o.write(true).create(true).truncate(true).read(plus),
        b'a' => o.append(true).create(true).read(plus),
        _ => return None,
    };
    let c_mode = CString::new(mode).ok()?;
    Some((o, c_mode))
}

/// Hand `f`, opened by Rust, to the C library with `mode`.
fn c_stream(f: File, mode: &std::ffi::CStr, text: bool) -> io::Result<CFile> {
    #[cfg(unix)]
    {
        CFile::from_fd(f.into(), mode, text)
    }
    #[cfg(windows)]
    {
        CFile::from_handle(f.into(), mode, text)
    }
}

/// Open `name` with `io.open`'s `mode`; `None` for a bad mode. Without `b`
/// the stream is a text stream.
fn open_file(name: &[u8], mode: &[u8], line_endings: LineEndings) -> Option<io::Result<CFile>> {
    let (o, c_mode) = open_options(mode)?;
    let text = !mode.contains(&b'b') && line_endings.translates();
    let path = os_path(name);
    if text && mode.starts_with(b"a+") {
        // A handle opened to append can't truncate, so this one does. If
        // it can't open the file (`a+` creates a missing one), the open
        // below reports why.
        if let Ok(mut f) = OpenOptions::new().read(true).write(true).open(&path) {
            if let Err(e) = drop_end_of_text(&mut f) {
                return Some(Err(e));
            }
        }
    }
    let opened = o.open(&path).map_err(|e| windows_directory_error(e, name));
    Some(opened.and_then(|f| c_stream(f, &c_mode, text)))
}

/// Remove a Ctrl-Z that ends `f`, as the C library does when it opens a
/// text stream with `a+`, so what is appended can be read back.
fn drop_end_of_text(f: &mut File) -> io::Result<()> {
    let len = f.seek(SeekFrom::End(0))?;
    if len == 0 {
        return Ok(());
    }
    f.seek(SeekFrom::End(-1))?;
    let mut last = [0];
    f.read_exact(&mut last)?;
    if last[0] == END_OF_TEXT {
        f.set_len(len - 1)?;
    }
    Ok(())
}

/// The error Windows gives the C runtime for opening a directory named
/// with a trailing `/` (`"/"`, `"./"`): access denied. std rewrites a
/// relative or rooted path into a `\\?\` path with backslashes first, and a
/// directory with a trailing backslash is "path not found" instead.
fn windows_directory_error(e: io::Error, name: &[u8]) -> io::Error {
    const ERROR_PATH_NOT_FOUND: i32 = 3;
    const ERROR_ACCESS_DENIED: i32 = 5;
    if !cfg!(windows) || e.raw_os_error() != Some(ERROR_PATH_NOT_FOUND) || !name.ends_with(b"/") {
        return e;
    }
    let dir = &name[..name.iter().rposition(|&b| b != b'/').map_or(0, |i| i + 1)];
    if dir.is_empty() || os_path(dir).is_dir() {
        io::Error::from_raw_os_error(ERROR_ACCESS_DENIED)
    } else {
        e
    }
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
    match open_file(&name, &mode, s.line_endings()) {
        None => Err(s.arg_error(2, "invalid mode")),
        Some(Ok(f)) => {
            push_file(s, f, Kind::File)?;
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
    // The child may read files we wrote and writes to the same terminal:
    // flush every C stream (our files and C modules' alike) and the
    // isolate's standard output first, as `lua5.4` and `lua5.5` do
    // (`os.execute` flushes nothing, in both).
    flush_stdout(s);
    flush_all();
    let mut command = shell(os_path(&cmd).as_os_str());
    if reading {
        command.stdout(Stdio::piped());
    } else {
        command.stdin(Stdio::piped());
    }
    let text = s.line_endings().translates();
    let spawned = command.spawn().and_then(|mut child| {
        let file = if reading {
            let out = child.stdout.take().ok_or_else(closed_error)?;
            pipe_stream(out, stdio::mode(b"r\0"), text)
        } else {
            let inp = child.stdin.take().ok_or_else(closed_error)?;
            pipe_stream(inp, stdio::mode(b"w\0"), text)
        };
        match file {
            Ok(f) => Ok((f, child)),
            Err(e) => {
                let _ = child.wait();
                Err(e)
            }
        }
    });
    match spawned {
        Ok((f, child)) => {
            push_file(s, f, Kind::Pipe(child))?;
            Ok(Return::values(1))
        }
        Err(e) => push_failure(s, &e, Some(&cmd)),
    }
}

#[cfg(unix)]
fn pipe_stream(
    end: impl Into<std::os::fd::OwnedFd>,
    mode: &std::ffi::CStr,
    text: bool,
) -> io::Result<CFile> {
    CFile::from_fd(end.into(), mode, text)
}

#[cfg(windows)]
fn pipe_stream(
    end: impl Into<std::os::windows::io::OwnedHandle>,
    mode: &std::ffi::CStr,
    text: bool,
) -> io::Result<CFile> {
    CFile::from_handle(end.into(), mode, text)
}

#[native(name = "io.tmpfile")]
fn io_tmpfile(s: &mut Stack<'_>) -> StackResult<Return> {
    // Removed at once: the open handle keeps the file until it is closed.
    let opened = crate::sys::temp_file().and_then(|(path, _)| {
        let opened = OpenOptions::new().read(true).write(true).open(&path);
        let _ = std::fs::remove_file(&path);
        opened.and_then(|f| c_stream(f, stdio::mode(b"w+b\0"), false))
    });
    match opened {
        Ok(f) => {
            push_file(s, f, Kind::File)?;
            Ok(Return::values(1))
        }
        Err(e) => push_failure(s, &e, None),
    }
}

#[native(name = "io.type")]
fn io_type(s: &mut Stack<'_>, v: Any) -> Option<&'static str> {
    let h = handle_at(s, v.0)?;
    Some(if h.is_closed() { "closed file" } else { "file" })
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
            match open_file(&name, mode, s.line_endings()).expect("valid mode") {
                Ok(f) => {
                    push_file(s, f, Kind::File)?;
                }
                Err(e) => return Err(s.error(&cannot_open(&name, &e))),
            }
            s.raw_set_field(REGISTRY_INDEX, default_key(input))?;
        }
        _ => {
            let FileArg(h) = FileArg::arg(s, 1)?;
            check_open(s, &h)?;
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
        let h = push_default(s, false)?;
        let at = s.top();
        return close(s, &h, at);
    }
    let FileArg(h) = FileArg::arg(s, 1)?;
    check_open(s, &h)?;
    close(s, &h, 1)
}

// Methods are unnamed in messages, as in the reference: `bad argument #2
// to '?'` when no call names them.

#[native(name = "?")]
fn f_close(s: &mut Stack<'_>, file: FileArg) -> StackResult<Return> {
    check_open(s, &file.0)?;
    close(s, &file.0, 1)
}

/// Close the open handle `h`, which is at `idx`: `true`, a pipe's exit
/// results, or `nil, message, code`.
fn close(s: &mut Stack<'_>, h: &Handle, idx: i32) -> StackResult<Return> {
    let st = match h {
        Handle::Ours(st) => st.clone(),
        Handle::Foreign(block) => return close_foreign(s, *block, idx),
    };
    if h.is_std() {
        return (Nil, "cannot close standard file").push_results(s);
    }
    let (file, kind, block) = {
        let mut st = st.borrow_mut();
        let kind = std::mem::replace(&mut st.kind, Kind::File);
        (st.file.take(), kind, st.block)
    };
    if let Some(f) = &file {
        stdio::write_stream_block(block as *mut c_void, f.as_raw(), 0);
    }
    let closed = file.map_or(Ok(()), CFile::close);
    match kind {
        Kind::Pipe(mut child) => match child.wait() {
            Ok(status) => exit_results(status).push_results(s),
            Err(e) => push_failure(s, &e, None),
        },
        _ => match closed {
            Ok(()) => true.push_results(s),
            Err(e) => push_failure(s, &e, None),
        },
    }
}

/// Close a stream C code made, through its `closef`, which gets it at
/// index 1, as Lua calls it.
fn close_foreign(s: &mut Stack<'_>, block: usize, idx: i32) -> StackResult<Return> {
    let (f, closef) = stdio::read_stream_block(block as *mut c_void);
    let Some(call) = io_state(s, |io| io.call_closef.get()) else {
        return Err(s.error("attempt to use a closed file"));
    };
    stdio::write_stream_block(block as *mut c_void, f, 0);
    call(s, idx, closef)
}

#[native(name = "io.flush")]
fn io_flush(s: &mut Stack<'_>) -> StackResult<Return> {
    let h = push_default(s, false)?;
    flush_result(s, &h)
}

#[native(name = "?")]
fn f_flush(s: &mut Stack<'_>, file: FileArg) -> StackResult<Return> {
    check_open(s, &file.0)?;
    s.push_copy(1);
    flush_result(s, &file.0)
}

/// Flush `h`: the file on top, or `nil, message, code`.
fn flush_result(s: &mut Stack<'_>, h: &Handle) -> StackResult<Return> {
    let r = if h.routed_stdout(s) {
        s.with_stdout(|out| out.flush())
    } else {
        h.with_file(CFile::flush)
            .unwrap_or_else(|| Err(closed_error()))
    };
    match r {
        Ok(()) => Ok(Return::values(1)),
        Err(e) => push_failure(s, &e, None),
    }
}

#[native(name = "io.write")]
fn io_write(s: &mut Stack<'_>, args: Rest) -> StackResult<Return> {
    let h = push_default(s, false)?;
    write(s, &h, args)
}

#[native(name = "?")]
fn f_write(s: &mut Stack<'_>, file: FileArg, args: Rest) -> StackResult<Return> {
    check_open(s, &file.0)?;
    s.push_copy(1);
    write(s, &file.0, args)
}

/// Write `args` (strings or numbers) to `h`. Returns the file on top, or
/// `nil, message, code, bytes written` (5.4: without the count).
fn write(s: &mut Stack<'_>, h: &Handle, args: Rest) -> StackResult<Return> {
    let mut written = 0usize;
    let stop = if h.routed_stdout(s) {
        write_routed(s, args, &mut written)
    } else {
        let s = &*s;
        // One lock of the stream for all the arguments. Nothing in here
        // can run a script (no allocation, no error raised), which could
        // reach this handle again.
        h.with_file(|f| {
            let mut f = f.lock();
            for i in args.indices() {
                let r = match s.type_at(i) {
                    Type::String => {
                        let bytes = s.string_at(i).unwrap_or_default();
                        f.write(&bytes).map(|()| bytes.len())
                    }
                    // With the locale's decimal point, as `tostring` writes it.
                    Type::Number => {
                        let text = s.number_text(i).unwrap_or_default();
                        f.write(text.as_bytes()).map(|()| text.len())
                    }
                    _ => return Err(WriteStop::NotString(i)),
                };
                written += r.map_err(WriteStop::Failed)?;
            }
            Ok(())
        })
        .unwrap_or_else(|| Err(WriteStop::Failed(closed_error())))
    };
    match stop {
        Ok(()) => Ok(Return::values(1)),
        Err(WriteStop::NotString(i)) => Err(s.type_error(i, "string")),
        Err(WriteStop::Failed(e)) => {
            let (msg, code) = (os_error_text(&e), os_error_code(&e));
            // Lua 5.4 doesn't report the bytes written (ADR-14).
            if orbit::LUA.is_5_4() {
                return (Nil, msg, code).push_results(s);
            }
            (Nil, msg, code, written as i64).push_results(s)
        }
    }
}

/// Why `write` stopped before the last argument.
enum WriteStop {
    /// The argument there is neither a string nor a number.
    NotString(i32),
    Failed(io::Error),
}

/// `write`'s arguments to the isolate's standard output.
fn write_routed(s: &mut Stack<'_>, args: Rest, written: &mut usize) -> Result<(), WriteStop> {
    for i in args.indices() {
        let bytes = match s.type_at(i) {
            Type::String => s.string_at(i).unwrap_or_default().to_vec(),
            Type::Number => s.number_text(i).unwrap_or_default().into_bytes(),
            _ => return Err(WriteStop::NotString(i)),
        };
        s.with_stdout(|out| out.write_all(&bytes))
            .map_err(WriteStop::Failed)?;
        *written += bytes.len();
    }
    Ok(())
}

#[native(name = "?")]
fn f_seek(
    s: &mut Stack<'_>,
    file: FileArg,
    whence: Option<Vec<u8>>,
    off: Option<i64>,
) -> StackResult<Return> {
    check_open(s, &file.0)?;
    let off = off.unwrap_or(0);
    let whence = match whence.as_deref().unwrap_or(b"cur") {
        b"set" => Whence::Set,
        b"cur" => Whence::Cur,
        b"end" => Whence::End,
        other => {
            let msg = OrbitString::concat(&[b"invalid option '", other, b"'"]);
            return Err(s.arg_error(2, &msg));
        }
    };
    if file.0.routed_stdout(s) {
        s.with_stdout(|out| {
            let _ = out.flush();
        });
    }
    match file.0.with_file(|f| f.seek(whence, off)) {
        Some(Ok(p)) => (p as i64).push_results(s),
        Some(Err(e)) => push_failure(s, &e, None),
        None => push_failure(s, &closed_error(), None),
    }
}

#[native(name = "?")]
fn f_setvbuf(
    s: &mut Stack<'_>,
    file: FileArg,
    mode: Vec<u8>,
    size: Option<i64>,
) -> StackResult<Return> {
    check_open(s, &file.0)?;
    let size = size.unwrap_or(BUFSIZ as i64).max(0) as usize;
    let b = match &mode[..] {
        b"no" => Buffering::No,
        b"line" => Buffering::Line,
        b"full" => Buffering::Full,
        _ => {
            let msg = OrbitString::concat(&[b"invalid option '", &mode, b"'"]);
            return Err(s.arg_error(2, &msg));
        }
    };
    match file.0.with_file(|f| f.set_buffering(b, size)) {
        Some(Ok(())) => true.push_results(s),
        Some(Err(e)) => push_failure(s, &e, None),
        None => push_failure(s, &closed_error(), None),
    }
}

// ---- reading ---------------------------------------------------------------

#[native(name = "io.read")]
fn io_read(s: &mut Stack<'_>, formats: Rest) -> StackResult<Return> {
    let h = push_default(s, true)?;
    let fmts = format_args(s, formats);
    read(s, &h, &fmts, formats.first)
}

#[native(name = "?")]
fn f_read(s: &mut Stack<'_>, file: FileArg, formats: Rest) -> StackResult<Return> {
    check_open(s, &file.0)?;
    let fmts = format_args(s, formats);
    read(s, &file.0, &fmts, formats.first)
}

/// `read`: the values, or `nil, message, code` on an I/O error.
fn read(s: &mut Stack<'_>, h: &Handle, fmts: &[FormatArg], arg: i32) -> StackResult<Return> {
    match read_values(s, h, fmts, arg)? {
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
        FormatArg::Float(_) => Err(s.arg_error(arg, orbit::arg_errors::NO_INTEGER_REP)),
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

/// Read with formats `fmts` (none: one line) from `h`, pushing one value
/// per format and stopping after the first that fails (nil). `arg` is the
/// argument number of the first format, for messages. As in lua5.5, each
/// format is checked when its turn comes, so formats after a failed read
/// are never looked at. Gives how many values it pushed, or the I/O error
/// the stream reports afterwards, with the values taken off again.
fn read_values(
    s: &mut Stack<'_>,
    h: &Handle,
    fmts: &[FormatArg],
    arg: i32,
) -> StackResult<io::Result<usize>> {
    let top = s.top();
    let n = fmts.len().max(1);
    let mut pushed = n;
    for i in 0..n {
        let fmt = match fmts.get(i) {
            Some(f) => format(s, f, arg + i as i32)?,
            None => Format::Line { keep: false },
        };
        match read_one(s, h, fmt, i == 0) {
            Ok(true) => {}
            Ok(false) => {
                pushed = i + 1;
                break;
            }
            Err(e) if e.kind() == io::ErrorKind::OutOfMemory => return Err(s.memory_error()),
            Err(e) => {
                s.set_top(top)?;
                return Ok(Err(e));
            }
        }
    }
    Ok(Ok(pushed))
}

/// What one format read.
enum ReadOutcome {
    /// The bytes are in the buffer.
    Bytes,
    Numeral(Option<Vec<u8>>),
    Nothing,
}

/// Push one format's value, or nil if it fails. Gives whether it read, or
/// the error the stream reports after reading. `first` forgets an earlier
/// error or end of file first. The stream is locked once for all of it,
/// and not while values are pushed (which can run finalizers).
fn read_one(s: &mut Stack<'_>, h: &Handle, fmt: Format, first: bool) -> io::Result<bool> {
    // The first byte of the locale's decimal point, as lua5.5 reads it.
    let point = s
        .decimal_point()
        .as_bytes()
        .first()
        .copied()
        .unwrap_or(b'.');
    let mut buf = h.take_buffer();
    let read = h
        .with_file(|f| -> io::Result<ReadOutcome> {
            let mut f = f.lock();
            if first {
                f.clear_error();
            }
            let ok = match fmt {
                Format::Count(0) => match f.getc() {
                    Some(c) => {
                        f.ungetc(c);
                        true
                    }
                    None => false,
                },
                Format::Count(n) => f.read_into(n, &mut buf)?,
                Format::Line { keep } => f.read_line(keep, &mut buf)?,
                Format::All => {
                    f.read_into(usize::MAX, &mut buf)?;
                    true
                }
                Format::Number => {
                    let numeral = read_numeral(&mut f, point);
                    return f.error().map_or(Ok(ReadOutcome::Numeral(numeral)), Err);
                }
            };
            if let Some(e) = f.error() {
                return Err(e);
            }
            Ok(if ok {
                ReadOutcome::Bytes
            } else {
                ReadOutcome::Nothing
            })
        })
        .unwrap_or_else(|| Err(closed_error()));
    let pushed = match read {
        Ok(ReadOutcome::Bytes) => {
            s.push_bytes(&buf[..]);
            Ok(true)
        }
        Ok(ReadOutcome::Numeral(Some(d))) if s.push_numeral(&d[..]) != 0 => Ok(true),
        Ok(ReadOutcome::Numeral(_) | ReadOutcome::Nothing) => {
            s.push_nil();
            Ok(false)
        }
        Err(e) => Err(e),
    };
    h.give_back_buffer(buf);
    pushed
}

/// Read a numeral as `read("n")` does: the longest prefix that can start a
/// numeral, at most [`MAX_NUMERAL`] bytes, looking one byte ahead (which
/// goes back to the stream). The decimal point is `.` or the locale's
/// `point`. Returns the bytes, or `None` if they ran too long.
fn read_numeral(f: &mut Locked<'_>, point: u8) -> Option<Vec<u8>> {
    let mut buf = Vec::new();
    let mut c = f.getc();
    while matches!(c, Some(b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c)) {
        c = f.getc();
    }
    let mut too_long = false;
    // Take the current byte if `accept` likes it, and move on.
    let mut take =
        |f: &mut Locked<'_>, c: &mut Option<u8>, buf: &mut Vec<u8>, accept: &dyn Fn(u8) -> bool| {
            match *c {
                Some(b) if accept(b) && !too_long => {
                    if buf.len() >= MAX_NUMERAL {
                        too_long = true;
                        return false;
                    }
                    buf.push(b);
                    *c = f.getc();
                    true
                }
                _ => false,
            }
        };
    take(f, &mut c, &mut buf, &|b| b == b'-' || b == b'+');
    let mut count = 0;
    let mut hex = false;
    if take(f, &mut c, &mut buf, &|b| b == b'0') {
        if take(f, &mut c, &mut buf, &|b| b == b'x' || b == b'X') {
            hex = true;
        } else {
            count = 1;
        }
    }
    let digit = move |b: u8| {
        if hex {
            b.is_ascii_hexdigit()
        } else {
            b.is_ascii_digit()
        }
    };
    while take(f, &mut c, &mut buf, &digit) {
        count += 1;
    }
    if take(f, &mut c, &mut buf, &|b| b == b'.' || b == point) {
        while take(f, &mut c, &mut buf, &digit) {
            count += 1;
        }
    }
    if count > 0 {
        let exp = move |b: u8| {
            if hex {
                b == b'p' || b == b'P'
            } else {
                b == b'e' || b == b'E'
            }
        };
        if take(f, &mut c, &mut buf, &exp) {
            take(f, &mut c, &mut buf, &|b| b == b'-' || b == b'+');
            while take(f, &mut c, &mut buf, &|b: u8| b.is_ascii_digit()) {}
        }
    }
    if let Some(b) = c {
        f.ungetc(b);
    }
    if too_long { None } else { Some(buf) }
}

// ---- lines -----------------------------------------------------------------

#[native(name = "io.lines")]
fn io_lines(s: &mut Stack<'_>, formats: Rest) -> StackResult<Return> {
    let close = !matches!(s.type_at(1), Type::None | Type::Nil);
    if close {
        let name = s.check_string(1)?.to_vec();
        match open_file(&name, b"r", s.line_endings()).expect("valid mode") {
            Ok(f) => {
                push_file(s, f, Kind::File)?;
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
fn f_lines(s: &mut Stack<'_>, file: FileArg, formats: Rest) -> StackResult<Return> {
    check_open(s, &file.0)?;
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
    let h = match handle_at(s, upvalue_index(1)) {
        Some(h) if !h.is_closed() => h,
        _ => return Err(s.error("file is already closed")),
    };
    let fmts: Vec<FormatArg> = (2..n + 2)
        .map(|i| FormatArg::at(s, upvalue_index(i)))
        .collect();
    let top = s.top();
    match read_values(s, &h, &fmts, 2)? {
        Ok(n) if s.to_boolean(top + 1) => Ok(Return::values(n)),
        Ok(_) => {
            // The end: no values, and the file closes if `lines` opened it.
            if close {
                s.set_top(top)?;
                s.push_copy(upvalue_index(1));
                let at = s.top();
                close_quietly(s, &h, at);
                s.set_top(top)?;
            }
            Ok(Return::values(0))
        }
        Err(e) => Err(s.error(os_error_text(&e).as_str())),
    }
}

/// Close the file handle at `idx` as its `close` method does, pushing what
/// that returns: for `orbit-capi`'s `closef`.
pub fn close_file(s: &mut Stack<'_>, idx: i32) -> StackResult<Return> {
    let idx = s.abs_index(idx);
    let FileArg(h) = FileArg::arg(s, idx)?;
    check_open(s, &h)?;
    close(s, &h, idx)
}

/// Close `h` (at `idx`) for `__gc`, `__close` or a finished `lines`,
/// ignoring the outcome; standard streams stay open.
fn close_quietly(s: &mut Stack<'_>, h: &Handle, idx: i32) {
    if h.is_closed() || h.is_std() {
        return;
    }
    let top = s.top();
    let _ = close(s, h, idx);
    let _ = s.set_top(top);
}

// ---- metamethods and opening ----------------------------------------------

#[native(name = "?")]
fn f_tostring(s: &mut Stack<'_>, file: FileArg) -> String {
    if file.0.is_closed() {
        "file (closed)".to_string()
    } else {
        format!("file (0x{:012x})", s.identity(1).unwrap_or(0))
    }
}

/// `__close`: close the file unless it is closed or standard.
#[native(name = "?")]
fn f_close_metamethod(s: &mut Stack<'_>, file: FileArg) {
    close_quietly(s, &file.0, 1);
}

/// `__gc`: close the file unless it is closed or standard.
#[native(name = "?")]
fn f_gc(s: &mut Stack<'_>, file: FileArg) {
    close_quietly(s, &file.0, 1);
}

/// Flush every C stream, our files among them, as the C library does at
/// exit.
pub fn flush_all() {
    crate::crt::flush_c_streams();
}

/// Set each named native in the table at `idx`.
macro_rules! set_natives {
    ($s:expr, $idx:expr, [$($name:literal => $f:ident),* $(,)?]) => {{
        let idx = $s.abs_index($idx);
        $(
            $s.push_native::<$f>();
            $s.raw_set_field(idx, $name.as_bytes())?;
        )*
    }};
}

/// Build the library on the stack: the `FILE*` metatable and the standard
/// files, which are also the default input and output. Leaves the `io`
/// table on top.
#[native(name = "?")]
fn io_setup(s: &mut Stack<'_>) -> StackResult<Return> {
    // The metatable, `__index` holding the methods: the one there is if
    // the library is opened again, so earlier handles stay files.
    if s.raw_get_field(REGISTRY_INDEX, FILE_TYPE.as_bytes())? == Type::Table {
        let id = s.identity(-1).unwrap_or(0);
        io_state(s, |io| io.meta.set(id));
        s.set_top(-2)?;
        return push_library(s);
    }
    s.set_top(-2)?;
    s.create_table(0, 6);
    let meta = s.top();
    s.create_table(0, 7);
    set_natives!(s, -1, [
        "close" => f_close,
        "flush" => f_flush,
        "lines" => f_lines,
        "read" => f_read,
        "seek" => f_seek,
        "setvbuf" => f_setvbuf,
        "write" => f_write,
    ]);
    s.raw_set_field(meta, b"__index".as_slice())?;
    set_natives!(s, meta, [
        "__tostring" => f_tostring,
        "__close" => f_close_metamethod,
        "__gc" => f_gc,
    ]);
    s.push_bytes(FILE_TYPE.as_bytes());
    s.raw_set_field(meta, b"__name".as_slice())?;
    s.push_copy(meta);
    s.raw_set_field(REGISTRY_INDEX, FILE_TYPE.as_bytes())?;
    let id = s.identity(meta).unwrap_or(0);
    io_state(s, |io| io.meta.set(id));
    s.set_top(-2)?;
    push_library(s)
}

/// Push the `io` table, with new standard files that are also the default
/// input and output.
fn push_library(s: &mut Stack<'_>) -> StackResult<Return> {
    s.create_table(0, 14);
    let io = s.top();
    #[cfg(windows)]
    let binary_std = !s.line_endings().translates();
    for (name, which) in [
        ("stdin", Std::In),
        ("stdout", Std::Out),
        ("stderr", Std::Err),
    ] {
        #[cfg(windows)]
        let file = if binary_std {
            CFile::standard_binary(which)
        } else {
            CFile::standard(which)
        };
        #[cfg(not(windows))]
        let file = CFile::standard(which);
        let Some(file) = file else { continue };
        push_file(s, file, Kind::Std(which))?;
        s.push_copy(-1);
        s.raw_set_field(io, name.as_bytes())?;
        match which {
            Std::In => s.raw_set_field(REGISTRY_INDEX, default_key(true))?,
            Std::Out => s.raw_set_field(REGISTRY_INDEX, default_key(false))?,
            Std::Err => s.set_top(-2)?,
        }
    }
    set_natives!(s, io, [
        "close" => io_close,
        "flush" => io_flush,
        "input" => io_input,
        "lines" => io_lines,
        "open" => io_open,
        "output" => io_output,
        "popen" => io_popen,
        "read" => io_read,
        "tmpfile" => io_tmpfile,
        "type" => io_type,
        "write" => io_write,
    ]);
    s.before_exit(flush_all);
    s.push_copy(io);
    Ok(Return::values(1))
}

/// Open the library: the `io` table, the `FILE*` metatable and the
/// standard files, which are also the default input and output. Returns
/// the table.
pub fn open<'s>(s: &'s Scope<'s>) -> Result<Table<'s>, Error> {
    let setup = s.create_native::<io_setup>();
    let made = setup.call(())?;
    Ok(made
        .into_iter()
        .next()
        .and_then(|v| v.as_table())
        .expect("io_setup returns the io table"))
}
