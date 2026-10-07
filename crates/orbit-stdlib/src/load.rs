//! Loading chunks: `load`, `loadfile` and `dofile`
//! ([design](https://github.com/tymlipari/OrbitScript/blob/main/docs/design/chunks-io-os-cli.md)).
//!
//! Source text goes through the compiler the host installed in
//! [`Runtime::compile`](orbit_vm::Runtime::compile), so this library needs
//! no front end of its own. Binary chunks (`string.dump` output) start with
//! [`BINARY_SIGNATURE`].

use std::io::Read;

use orbit_vm::orbit_core::Value;
use orbit_vm::{CallOutcome, LuaError, NativeCtx, NativeFn, NativeReturn};

type R = Result<NativeReturn, LuaError>;

pub(crate) const FUNCTIONS: &[(&str, NativeFn)] =
    &[("load", load), ("loadfile", loadfile), ("dofile", dofile)];

/// The first byte of every binary chunk. Text never starts with it, so
/// `load` tells the two apart by this byte alone, as Lua does.
pub const BINARY_SIGNATURE: u8 = 0x1b;

/// Most bytes of a chunk name that messages show (Lua's `short_src`).
const ID_SIZE: usize = 59;

/// The name messages use for a chunk whose source name is `source`
/// (`short_src`): `=name` shows `name`, `@file` shows the file name (keeping
/// its end when it is too long), and anything else is source text, shown as
/// `[string "first line..."]`.
pub fn chunk_id(source: &[u8]) -> String {
    let text = |b: &[u8]| String::from_utf8_lossy(b).into_owned();
    match source.first() {
        Some(b'=') => text(&source[1..(ID_SIZE + 1).min(source.len())]),
        Some(b'@') => {
            let name = &source[1..];
            if name.len() <= ID_SIZE {
                text(name)
            } else {
                format!("...{}", text(&name[name.len() - (ID_SIZE - 3)..]))
            }
        }
        _ => {
            // Room for the text inside `[string "..."]`.
            const ROOM: usize = ID_SIZE - "[string \"...\"]".len();
            let line_end = source.iter().position(|&b| b == b'\n');
            match line_end {
                None if source.len() < ROOM => format!("[string \"{}\"]", text(source)),
                _ => {
                    let line = &source[..line_end.unwrap_or(source.len())];
                    format!("[string \"{}...\"]", text(&line[..line.len().min(ROOM)]))
                }
            }
        }
    }
}

/// Which kinds of chunk a `mode` string allows.
pub(crate) struct Mode<'a>(pub(crate) &'a [u8]);

impl Mode<'_> {
    fn check(&self, binary: bool) -> Result<(), String> {
        let (want, what) = if binary {
            (b'b', "binary")
        } else {
            (b't', "text")
        };
        if self.0.contains(&want) {
            Ok(())
        } else {
            Err(format!(
                "attempt to load a {what} chunk (mode is '{}')",
                String::from_utf8_lossy(self.0)
            ))
        }
    }
}

/// Compile and link `bytes` as a chunk named `source` (a Lua source name:
/// `@file`, `=name` or the text itself), with `env` as its `_ENV`. The
/// function is unrooted; the error is the message `load` returns.
pub(crate) fn load_chunk(
    ctx: &mut NativeCtx<'_>,
    bytes: &[u8],
    source: &[u8],
    mode: &Mode<'_>,
    env: Value,
) -> Result<Value, String> {
    let binary = bytes.first() == Some(&BINARY_SIGNATURE);
    mode.check(binary)?;
    let name = chunk_id(source);
    if binary {
        return Err(format!("{name}: bad binary format (not a binary chunk)"));
    }
    let Some(compile) = ctx.rt.compile else {
        return Err(format!("{name}: no compiler for source chunks"));
    };
    let mut chunk = compile(bytes, &name)?;
    // `debug.getinfo`'s `source` is the name as given, not its short form.
    chunk.source = source.into();
    Ok(ctx.link(&chunk, env))
}

/// Return the function, or nil and the message.
fn ret_loaded(ctx: &mut NativeCtx<'_>, r: Result<Value, String>) -> R {
    match r {
        Ok(f) => ctx.ret1(f),
        Err(msg) => {
            let m = ctx.new_str(msg.as_bytes());
            ctx.push(Value::Nil);
            ctx.push(m);
            Ok(NativeReturn::Return(2))
        }
    }
}

/// `load`'s mode argument. `B` (a chunk in a fixed buffer, from the C
/// API) is something Lua code can't ask for.
fn load_mode(ctx: &mut NativeCtx<'_>) -> Result<Vec<u8>, LuaError> {
    let mode = opt_bytes(ctx, 2, b"bt")?;
    if mode.contains(&b'B') {
        return Err(ctx.arg_error(2, "invalid mode"));
    }
    Ok(mode)
}

/// An optional string argument (numbers convert), as bytes.
fn opt_bytes(ctx: &mut NativeCtx<'_>, i: usize, default: &[u8]) -> Result<Vec<u8>, LuaError> {
    if ctx.arg(i).is_nil() {
        return Ok(default.to_vec());
    }
    let s = ctx.check_str(i)?;
    Ok(ctx.bytes(s).to_vec())
}

// Window slots `load` keeps while it calls a reader function: the four
// arguments, whether `env` was given, then the pieces read so far.
const ENV_GIVEN: usize = 4;
const PIECES: usize = 5;

fn load(ctx: &mut NativeCtx<'_>) -> R {
    let env = if ctx.nargs() >= 4 {
        ctx.arg(3)
    } else {
        Value::Table(ctx.rt.globals)
    };
    match ctx.arg(0) {
        Value::Str(_) | Value::Int(_) | Value::Float(_) => {
            let s = ctx.check_str(0)?;
            let bytes = ctx.bytes(s).to_vec();
            let source = opt_bytes(ctx, 1, &bytes)?;
            let mode = load_mode(ctx)?;
            let r = load_chunk(ctx, &bytes, &source, &Mode(&mode), env);
            ret_loaded(ctx, r)
        }
        Value::Closure(_) | Value::Native(_) => {
            // Check the other arguments now, as the reader may run long.
            opt_bytes(ctx, 1, b"")?;
            load_mode(ctx)?;
            let given = ctx.nargs() >= 4;
            ctx.truncate(ENV_GIVEN.min(ctx.len()));
            ctx.set(ENV_GIVEN, Value::Bool(given));
            read_more(ctx)
        }
        _ => Err(ctx.type_error(0, "function")),
    }
}

/// Call the reader for the next piece.
fn read_more(ctx: &mut NativeCtx<'_>) -> R {
    Ok(NativeReturn::CallThen {
        f: ctx.arg(0),
        nargs: 0,
        k: read_piece,
        protected: true,
    })
}

fn read_piece(ctx: &mut NativeCtx<'_>, outcome: CallOutcome) -> R {
    let n = match outcome {
        CallOutcome::Ok(n) => n,
        CallOutcome::Err(e) => {
            ctx.truncate(0);
            ctx.push(Value::Nil);
            ctx.push(e);
            return Ok(NativeReturn::Return(2));
        }
    };
    let first = ctx.len() - n;
    let piece = if n > 0 { ctx.arg(first) } else { Value::Nil };
    ctx.truncate(first);
    let done = match piece {
        Value::Nil => true,
        Value::Str(s) => {
            let empty = ctx.bytes(s).is_empty();
            if !empty {
                ctx.push(piece);
            }
            empty
        }
        Value::Int(_) | Value::Float(_) => {
            let mut text = Vec::new();
            ctx.display(piece, &mut text);
            let s = ctx.new_str_owned(text);
            ctx.push(s);
            false
        }
        _ => {
            // Level 1 is load's caller, from its continuations too.
            let msg = format!("{}reader function must return a string", ctx.where_(1));
            let m = ctx.new_str(msg.as_bytes());
            ctx.truncate(0);
            ctx.push(Value::Nil);
            ctx.push(m);
            return Ok(NativeReturn::Return(2));
        }
    };
    if !done {
        return read_more(ctx);
    }
    let mut bytes = Vec::new();
    for i in PIECES..ctx.len() {
        if let Value::Str(s) = ctx.arg(i) {
            bytes.extend_from_slice(ctx.bytes(s));
        }
    }
    let source = opt_bytes(ctx, 1, b"=(load)")?;
    let mode = opt_bytes(ctx, 2, b"bt")?;
    let env = if ctx.arg(ENV_GIVEN).is_falsy() {
        Value::Table(ctx.rt.globals)
    } else {
        ctx.arg(3)
    };
    let r = load_chunk(ctx, &bytes, &source, &Mode(&mode), env);
    ctx.truncate(0);
    ret_loaded(ctx, r)
}

/// Read a file (`None`: standard input) the way `loadfile` does: a UTF-8
/// byte-order mark and a first line starting with `#` are dropped, keeping
/// the line break so line numbers stay right (and dropping it too when a
/// binary chunk follows). Returns the bytes and the chunk's source name.
pub fn read_chunk_file(path: Option<&[u8]>) -> Result<(Vec<u8>, Vec<u8>), String> {
    let (mut bytes, source) = match path {
        None => {
            let mut b = Vec::new();
            std::io::stdin()
                .read_to_end(&mut b)
                .map_err(|_| "cannot read stdin".to_string())?;
            (b, b"=stdin".to_vec())
        }
        Some(p) => {
            let name = String::from_utf8_lossy(p).into_owned();
            let mut f = std::fs::File::open(crate::os_path(p))
                .map_err(|e| format!("cannot open {name}: {}", crate::os_error_text(&e)))?;
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

/// `loadfile` and `dofile`'s first steps: the file name argument (nil for
/// standard input) and the loaded function or message.
fn load_file_arg(
    ctx: &mut NativeCtx<'_>,
    mode: &[u8],
    env: Value,
) -> Result<Result<Value, String>, LuaError> {
    let path = match ctx.arg(0) {
        Value::Nil => None,
        _ => {
            let s = ctx.check_str(0)?;
            Some(ctx.bytes(s).to_vec())
        }
    };
    Ok(read_chunk_file(path.as_deref())
        .and_then(|(bytes, source)| load_chunk(ctx, &bytes, &source, &Mode(mode), env)))
}

fn loadfile(ctx: &mut NativeCtx<'_>) -> R {
    let mode = opt_bytes(ctx, 1, b"bt")?;
    let env = if ctx.nargs() >= 3 {
        ctx.arg(2)
    } else {
        Value::Table(ctx.rt.globals)
    };
    let r = load_file_arg(ctx, &mode, env)?;
    ret_loaded(ctx, r)
}

fn dofile(ctx: &mut NativeCtx<'_>) -> R {
    let env = Value::Table(ctx.rt.globals);
    match load_file_arg(ctx, b"bt", env)? {
        Ok(f) => {
            ctx.truncate(0);
            // lua5.5 lets the chunk yield across `dofile`.
            Ok(NativeReturn::CallThenYieldable {
                f,
                nargs: 0,
                k: dofile_done,
                protected: false,
            })
        }
        Err(msg) => {
            let m = ctx.new_str(msg.as_bytes());
            Err(ctx.error_value(m))
        }
    }
}

fn dofile_done(_ctx: &mut NativeCtx<'_>, outcome: CallOutcome) -> R {
    match outcome {
        CallOutcome::Ok(n) => Ok(NativeReturn::Return(n)),
        CallOutcome::Err(e) => Err(LuaError::new(e)),
    }
}

#[cfg(test)]
mod tests {
    use super::chunk_id;

    #[test]
    fn chunk_ids_match_the_reference() {
        // Expected values printed by the reference lua5.5.
        assert_eq!(chunk_id(b"=abc"), "abc");
        assert_eq!(chunk_id(b"@file.lua"), "file.lua");
        assert_eq!(chunk_id(b"plain"), "[string \"plain\"]");
        assert_eq!(chunk_id(b""), "[string \"\"]");
        assert_eq!(chunk_id(b"line1\nline2"), "[string \"line1...\"]");
        assert_eq!(chunk_id(b"\n x = = 1"), "[string \"...\"]");
        let a = "a".repeat(70);
        assert_eq!(chunk_id(format!("={a}").as_bytes()), "a".repeat(59));
        let b = |n| format!("@{}", "b".repeat(n));
        assert_eq!(chunk_id(b(59).as_bytes()), "b".repeat(59));
        assert_eq!(chunk_id(b(60).as_bytes()), format!("...{}", "b".repeat(56)));
        let c = |n| "c".repeat(n);
        assert_eq!(
            chunk_id(c(44).as_bytes()),
            format!("[string \"{}\"]", c(44))
        );
        assert_eq!(
            chunk_id(c(45).as_bytes()),
            format!("[string \"{}...\"]", c(45))
        );
        assert_eq!(
            chunk_id(c(70).as_bytes()),
            format!("[string \"{}...\"]", c(45))
        );
    }
}
