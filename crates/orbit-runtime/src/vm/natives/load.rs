//! Loading chunks: `load` (`loadfile` and `dofile` read files, so they
//! are `orbit-std`'s)
//! ([design](https://github.com/orbitscript-lang/OrbitScript/blob/main/docs/design/chunks-io-os-cli.md)).
//!
//! Source text goes through the compiler the host installed in
//! [`Host::set_compilers`](crate::vm::host::Host::set_compilers), so this library needs
//! no front end of its own. Binary chunks (`string.dump` output) start with
//! [`BINARY_SIGNATURE`].

use crate::object::Value;
use crate::vm::op::UnlinkedChunk;
use crate::vm::{LuaError, NativeCtx, NativeFn, NativeReturn, VmState};

use crate::stdlib::{BINARY_SIGNATURE, chunk_id, is_orbitscript_file};

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
    let seed = ctx.vm.hash_seed();
    let chunk = unlinked(&ctx.vm.rt, seed, bytes, source, mode)?;
    Ok(ctx.link(&chunk, env))
}

/// [`load_chunk`] for other libraries (the C API's `lua_load`): `mode` is
/// `load`'s mode string (`b"bt"` for both kinds of chunk).
pub(crate) fn load_chunk_in(
    ctx: &mut NativeCtx<'_>,
    bytes: &[u8],
    source: &[u8],
    mode: &[u8],
    env: Value,
) -> Result<Value, String> {
    load_chunk(ctx, bytes, source, &Mode(mode), env)
}

/// Decode (and check) or compile `bytes`.
/// `seed` is the isolate's hash seed, for the compiler's maps.
fn unlinked(
    rt: &VmState,
    seed: u64,
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
        use crate::vm::op::format;
        use crate::vm::op::verify;
        let chunk = format::decode(bytes).map_err(|e| bad(&e))?;
        if let Some(level) = crate::stdlib::checks::level(rt) {
            verify::verify(&chunk, level).map_err(|e| bad(&e))?;
        }
        return Ok(chunk);
    }
    let name = chunk_id(source);
    let compile = rt.host.compiler(is_orbitscript_file(source));
    let Some(compile) = compile else {
        return Err(format!("{name}: no compiler for source chunks"));
    };
    let mut chunk = compile(bytes, &name, seed)?;
    // `debug.getinfo`'s `source` is the name as given, not its short form.
    chunk.source = source.into();
    Ok(chunk)
}

/// Return the function, or nil and the message.
fn ret_loaded(ctx: &mut NativeCtx<'_>, r: Result<Value, String>) -> R {
    match r {
        Ok(f) => ctx.ret([f]),
        Err(msg) => {
            let m = ctx.new_str(msg.as_bytes());
            ctx.ret([Value::Nil, m])
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

fn load(ctx: &mut NativeCtx<'_>) -> R {
    let env = if ctx.nargs() >= 4 {
        ctx.arg(3)
    } else {
        Value::Table(ctx.vm.rt.globals)
    };
    let bytes = match ctx.arg(0) {
        Value::Str(_) | Value::Int(_) | Value::Float(_) => {
            let s = ctx.check_str(0)?;
            let bytes = ctx.bytes(s).to_vec();
            let source = opt_bytes(ctx, 1, &bytes)?;
            let mode = load_mode(ctx)?;
            let r = load_chunk(ctx, &bytes, &source, &Mode(&mode), env);
            return ret_loaded(ctx, r);
        }
        Value::Closure(_) | Value::Native(_) => {
            // Check the other arguments now, as the reader may run long.
            opt_bytes(ctx, 1, b"")?;
            load_mode(ctx)?;
            ctx.truncate(4.min(ctx.len()));
            match read_all(ctx)? {
                Ok(bytes) => bytes,
                Err(e) => {
                    ctx.truncate(0);
                    return ctx.ret([Value::Nil, e]);
                }
            }
        }
        _ => return Err(ctx.type_error(0, "function")),
    };
    let source = opt_bytes(ctx, 1, b"=(load)")?;
    let mode = opt_bytes(ctx, 2, b"bt")?;
    let r = load_chunk(ctx, &bytes, &source, &Mode(&mode), env);
    ctx.truncate(0);
    ret_loaded(ctx, r)
}

/// Call the reader function in window slot 0 until it returns nil or an
/// empty string, and give the pieces joined, or the error `load` returns:
/// the reader's own error (the calls are protected, and nested, so nothing
/// may yield across them, as in the reference) or a bad piece.
fn read_all(ctx: &mut NativeCtx<'_>) -> Result<Result<Vec<u8>, Value>, LuaError> {
    let mut bytes = Vec::new();
    let at = ctx.len();
    loop {
        let reader = ctx.arg(0);
        ctx.push(reader);
        let piece = match ctx.call_want(0, 1) {
            Ok(_) => ctx.arg(at),
            Err(e) if !e.is_exit() => return Ok(Err(e.value)),
            Err(e) => return Err(e),
        };
        ctx.truncate(at);
        match piece {
            Value::Nil => return Ok(Ok(bytes)),
            Value::Str(s) if ctx.bytes(s).is_empty() => return Ok(Ok(bytes)),
            Value::Str(_) | Value::Int(_) | Value::Float(_) => ctx.display(piece, &mut bytes),
            _ => {
                // Level 1 is load's caller.
                return Ok(Err(ctx.positioned(b"reader function must return a string")));
            }
        }
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
