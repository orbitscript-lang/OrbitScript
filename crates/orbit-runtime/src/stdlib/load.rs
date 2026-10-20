//! Loading chunks: `load` (`loadfile` and `dofile` read files, so they
//! are `orbit-std`'s)
//! ([design](https://github.com/tymlipari/OrbitScript/blob/main/docs/design/chunks-io-os-cli.md)).
//!
//! Source text goes through the compiler the host installed in
//! [`Runtime::compile`](crate::vm::Runtime::compile), so this library needs
//! no front end of its own. Binary chunks (`string.dump` output) start with
//! [`BINARY_SIGNATURE`].

use crate::object::Value;
use crate::vm::op::UnlinkedChunk;
use crate::vm::{CallOutcome, LuaError, NativeCtx, NativeFn, NativeReturn, Runtime, Vm};

pub use super::chunk::{BINARY_SIGNATURE, chunk_id, is_orbitscript_file};

type R = Result<NativeReturn, LuaError>;

pub(crate) const FUNCTIONS: &[(&str, NativeFn)] = &[("load", load)];

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
    let chunk = unlinked(&ctx.vm.rt, bytes, source, mode)?;
    Ok(ctx.link(&chunk, env))
}

/// [`load_chunk`] for other libraries (the C API's `lua_load`): `mode` is
/// `load`'s mode string (`b"bt"` for both kinds of chunk).
pub fn load_chunk_in(
    ctx: &mut NativeCtx<'_>,
    bytes: &[u8],
    source: &[u8],
    mode: &[u8],
    env: Value,
) -> Result<Value, String> {
    load_chunk(ctx, bytes, source, &Mode(mode), env)
}

/// [`load_chunk`] for a host: load `bytes`, source text or a binary chunk,
/// as a chunk named `source`, with the global table as its `_ENV`, as the
/// standalone interpreter loads scripts. The function is unrooted.
pub fn load_buffer(vm: &mut Vm, bytes: &[u8], source: &[u8]) -> Result<Value, String> {
    let chunk = unlinked(&vm.rt, bytes, source, &Mode(b"bt"))?;
    Ok(vm.load(&chunk))
}

/// Decode (and check) or compile `bytes`.
fn unlinked(
    rt: &Runtime,
    bytes: &[u8],
    source: &[u8],
    mode: &Mode<'_>,
) -> Result<UnlinkedChunk, String> {
    let binary = bytes.first() == Some(&BINARY_SIGNATURE);
    mode.check(binary)?;
    if binary {
        // Messages name a chunk whose source is itself binary this way.
        let name = if source.first() == Some(&BINARY_SIGNATURE) {
            "binary string".to_string()
        } else {
            chunk_id(source)
        };
        let bad = |e: &dyn std::fmt::Display| format!("{name}: bad binary format ({e})");
        #[cfg(feature = "dump")]
        {
            use crate::vm::op::format;
            #[cfg(feature = "verify")]
            use crate::vm::op::verify;
            let chunk = format::decode(bytes).map_err(|e| bad(&e))?;
            #[cfg(feature = "verify")]
            if let Some(level) = crate::stdlib::checks::level(rt) {
                verify::verify(&chunk, level).map_err(|e| bad(&e))?;
            }
            return Ok(chunk);
        }
        #[cfg(not(feature = "dump"))]
        return Err(bad(&"binary chunks not enabled"));
    }
    let name = chunk_id(source);
    let compile = if is_orbitscript_file(source) {
        rt.compile_orbitscript
    } else {
        rt.compile
    };
    let Some(compile) = compile else {
        return Err(format!("{name}: no compiler for source chunks"));
    };
    let mut chunk = compile(bytes, &name)?;
    // `debug.getinfo`'s `source` is the name as given, not its short form.
    chunk.source = source.into();
    Ok(chunk)
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
        Value::Table(ctx.vm.rt.globals)
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
        Value::Table(ctx.vm.rt.globals)
    } else {
        ctx.arg(3)
    };
    let r = load_chunk(ctx, &bytes, &source, &Mode(&mode), env);
    ctx.truncate(0);
    ret_loaded(ctx, r)
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
