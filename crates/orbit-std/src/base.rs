//! The base library's I/O: `print`, `loadfile`, `dofile`, and
//! `debug.debug`'s command loop.

use std::io::{BufRead, Read, Write};

use orbit::stack::{Either, Nil, Rest, Return, Stack, StackResult, Status};
use orbit::{Chunk, Error, Mode, Scope, Table, native};

use crate::sys::{os_error_text, os_path};

/// The chunk name `lua` gives standard input.
pub const STDIN: &[u8] = b"=stdin";

/// The first byte of a binary chunk.
const BINARY_SIGNATURE: u8 = 0x1b;

/// Read a script as `loadfile` does: a file (`None`: standard input), less
/// a UTF-8 byte-order mark and a first line starting with `#` (keeping the
/// line break, so line numbers stay right, and dropping it too when a
/// binary chunk follows). Returns the bytes and the chunk's source name
/// (`@path`, or `=stdin`), or the message `loadfile` gives.
pub fn read_chunk_file(path: Option<&[u8]>) -> Result<(Vec<u8>, Vec<u8>), Vec<u8>> {
    let (mut bytes, source) = match path {
        None => {
            let mut b = Vec::new();
            std::io::stdin()
                .read_to_end(&mut b)
                .map_err(|_| b"cannot read stdin".to_vec())?;
            (b, STDIN.to_vec())
        }
        Some(p) => {
            let mut f = std::fs::File::open(os_path(p)).map_err(|e| {
                let why = os_error_text(&e);
                [b"cannot open ", p, b": ", why.as_bytes()].concat()
            })?;
            let mut b = Vec::new();
            f.read_to_end(&mut b)
                .map_err(|_| [b"cannot read ", p].concat())?;
            let mut source = b"@".to_vec();
            source.extend_from_slice(p);
            (b, source)
        }
    };
    let mut start = 0;
    if bytes.starts_with(b"\xEF\xBB\xBF") {
        start = 3;
    }
    if bytes.get(start) == Some(&b'#') {
        let end = bytes[start..]
            .iter()
            .position(|&b| b == b'\n')
            .map_or(bytes.len(), |i| start + i);
        start = match bytes.get(end + 1) {
            Some(&BINARY_SIGNATURE) => end + 1,
            _ => end,
        };
    }
    bytes.drain(..start);
    Ok((bytes, source))
}

/// `print`: each argument as `tostring` gives it, tab-separated, then a
/// newline, to the isolate's standard output.
#[native]
fn print(s: &mut Stack<'_>, args: Rest) -> StackResult<()> {
    let mut line = Vec::new();
    for i in args.indices() {
        if i > args.first {
            line.push(b'\t');
        }
        let top = s.top();
        line.extend_from_slice(s.to_display_string(i)?);
        s.set_top(top)?;
    }
    line.push(b'\n');
    s.with_stdout(|out| {
        let _ = out.write_all(&line);
        // lua flushes after every `print`, so output reaches a pipe at once.
        let _ = out.flush();
    });
    Ok(())
}

/// Load the file `path` names (`None`: standard input) with `mode`
/// (`load`'s mode string) and the value at `env` as its `_ENV` (0: the
/// globals): push the function, or give the message.
fn load_file(s: &mut Stack<'_>, path: Option<&[u8]>, mode: &[u8], env: i32) -> Result<(), Vec<u8>> {
    let (bytes, source) = read_chunk_file(path)?;
    let binary = bytes.first() == Some(&BINARY_SIGNATURE);
    let (want, what) = if binary {
        (b'b', &b"binary"[..])
    } else {
        (b't', &b"text"[..])
    };
    if !mode.contains(&want) {
        return Err([
            b"attempt to load a ",
            what,
            b" chunk (mode is '",
            mode,
            b"')",
        ]
        .concat());
    }
    let chunk = Chunk::new(&bytes).name(&source).mode(Mode::Both);
    match s.load_chunk(&chunk, env) {
        Status::Ok => Ok(()),
        _ => {
            let msg = s.string_at(-1).unwrap_or_default().to_vec();
            let _ = s.set_top(-2);
            Err(msg)
        }
    }
}

#[native]
fn loadfile(
    s: &mut Stack<'_>,
    path: Option<Vec<u8>>,
    mode: Option<Vec<u8>>,
) -> Either<Return, (Nil, Vec<u8>)> {
    let mode = mode.unwrap_or_else(|| b"bt".to_vec());
    // An `env` argument, even nil, replaces the globals.
    let env = if s.top() >= 3 { 3 } else { 0 };
    match load_file(s, path.as_deref(), &mode, env) {
        Ok(()) => Either::Left(Return::values(1)),
        Err(msg) => Either::Right((Nil, msg)),
    }
}

#[native]
fn dofile(s: &mut Stack<'_>, path: Option<Vec<u8>>) -> Result<Return, Vec<u8>> {
    load_file(s, path.as_deref(), b"bt", 0)?;
    // lua5.5 lets the chunk yield across `dofile`.
    Ok(Return::tail_call(0))
}

/// `debug.debug`: run each line read from stdin as a chunk until a line
/// reading `cont` (or end of input). Prompts and error messages go to
/// stderr, as in the reference.
#[native(name = "debug.debug")]
fn debug_loop(s: &mut Stack<'_>) {
    s.with_scope(debug_lines);
}

fn debug_lines<'s>(c: &'s Scope<'s>) {
    loop {
        crate::sys::flush_stdout(c);
        let _ = std::io::stderr().write_all(b"lua_debug> ");
        let mut line = Vec::new();
        match std::io::stdin().lock().read_until(b'\n', &mut line) {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
        if line == b"cont\n" || line == b"cont" {
            return;
        }
        let ran = c
            .load(Chunk::new(&line).name("=(debug command)"))
            .and_then(|f| f.call(()).map(drop));
        if let Err(e) = ran {
            // The message as `tostring` would give it.
            let text = match e.value().map(|v| v.get(c)) {
                Some(Ok(v)) => v.to_display().unwrap_or_else(|e| e.message().into()),
                _ => e.message().as_bytes().to_vec(),
            };
            let mut err = std::io::stderr().lock();
            let _ = err.write_all(&text);
            let _ = err.write_all(b"\n");
        }
    }
}

/// Add `print`, `loadfile` and `dofile` to the global table.
pub fn open<'s>(s: &'s Scope<'s>) -> Result<(), Error> {
    let g = s.globals();
    g.raw_set("print", s.create_native::<print>())?;
    g.raw_set("loadfile", s.create_native::<loadfile>())?;
    g.raw_set("dofile", s.create_native::<dofile>())?;
    Ok(())
}

/// Add `debug.debug` to the `debug` library, if it is open.
pub fn open_debug<'s>(s: &'s Scope<'s>) -> Result<(), Error> {
    if let Some(debug) = s.globals().raw_get::<Option<Table>>("debug")? {
        debug.raw_set("debug", s.create_native::<debug_loop>())?;
    }
    Ok(())
}
