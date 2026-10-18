//! The base library's I/O: `print`, `loadfile`, `dofile`, and
//! `debug.debug`'s command loop.

use std::io::{BufRead, Read, Write};

use orbit::{Call, Chunk, Error, Function, Mode, Ret, Scope, Table, Value};

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
pub fn read_chunk_file(path: Option<&[u8]>) -> Result<(Vec<u8>, Vec<u8>), String> {
    let (mut bytes, source) = match path {
        None => {
            let mut b = Vec::new();
            std::io::stdin()
                .read_to_end(&mut b)
                .map_err(|_| "cannot read stdin".to_string())?;
            (b, STDIN.to_vec())
        }
        Some(p) => {
            let name = String::from_utf8_lossy(p).into_owned();
            let mut f = std::fs::File::open(os_path(p))
                .map_err(|e| format!("cannot open {name}: {}", os_error_text(&e)))?;
            let mut b = Vec::new();
            f.read_to_end(&mut b)
                .map_err(|_| format!("cannot read {name}"))?;
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
fn print<'s>(c: &'s Call<'s>) -> Result<Ret, Error> {
    let mut line = Vec::new();
    for i in 0..c.nargs() {
        if i > 0 {
            line.push(b'\t');
        }
        c.arg(i).write_display(&mut line)?;
    }
    line.push(b'\n');
    c.with_stdout(|out| {
        let _ = out.write_all(&line);
        // lua flushes after every `print`, so output reaches a pipe at once.
        let _ = out.flush();
    });
    c.ret(())
}

/// Load the file argument 0 names (nil: standard input) with `mode`
/// (`load`'s mode string) and `env`: the function, or the message.
fn load_file<'s>(
    c: &'s Call<'s>,
    mode: &[u8],
    env: Option<Value<'s>>,
) -> Result<Result<Function<'s>, String>, Error> {
    let path: Option<Vec<u8>> = c.check(0)?;
    let (bytes, source) = match read_chunk_file(path.as_deref()) {
        Ok(read) => read,
        Err(msg) => return Ok(Err(msg)),
    };
    let binary = bytes.first() == Some(&BINARY_SIGNATURE);
    let (want, what) = if binary {
        (b'b', "binary")
    } else {
        (b't', "text")
    };
    if !mode.contains(&want) {
        return Ok(Err(format!(
            "attempt to load a {what} chunk (mode is '{}')",
            String::from_utf8_lossy(mode)
        )));
    }
    let chunk = Chunk::new(&bytes).name(&source).mode(Mode::Both);
    let loaded = match env {
        Some(env) => c.load_in(chunk, env),
        None => c.load(chunk),
    };
    Ok(loaded.map_err(|e| e.message().to_owned()))
}

fn loadfile<'s>(c: &'s Call<'s>) -> Result<Ret, Error> {
    let mode = c
        .opt::<Option<Vec<u8>>>(1, None)?
        .unwrap_or_else(|| b"bt".to_vec());
    let env = (c.nargs() >= 3).then(|| c.arg(2));
    match load_file(c, &mode, env)? {
        Ok(f) => c.ret(f),
        Err(msg) => c.ret((Value::Nil, msg)),
    }
}

fn dofile<'s>(c: &'s Call<'s>) -> Result<Ret, Error> {
    match load_file(c, b"bt", None)? {
        // lua5.5 lets the chunk yield across `dofile`.
        Ok(f) => c.tail_call(f, ()),
        Err(msg) => Err(Error::runtime(msg)),
    }
}

/// `debug.debug`: run each line read from stdin as a chunk until a line
/// reading `cont` (or end of input). Prompts and error messages go to
/// stderr, as in the reference.
fn debug_loop<'s>(c: &'s Call<'s>) -> Result<Ret, Error> {
    loop {
        crate::sys::flush_stdout(c);
        let _ = std::io::stderr().write_all(b"lua_debug> ");
        let mut line = Vec::new();
        match std::io::stdin().lock().read_until(b'\n', &mut line) {
            Ok(0) | Err(_) => return c.ret(()),
            Ok(_) => {}
        }
        if line == b"cont\n" || line == b"cont" {
            return c.ret(());
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
    g.raw_set("print", s.create_function(print))?;
    g.raw_set("loadfile", s.create_function(loadfile))?;
    g.raw_set("dofile", s.create_function(dofile))?;
    Ok(())
}

/// Add `debug.debug` to the `debug` library, if it is open.
pub fn open_debug<'s>(s: &'s Scope<'s>) -> Result<(), Error> {
    if let Some(debug) = s.globals().raw_get::<Option<Table>>("debug")? {
        debug.raw_set("debug", s.create_function(debug_loop))?;
    }
    Ok(())
}
