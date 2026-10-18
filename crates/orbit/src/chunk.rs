//! [`Chunk`]: code for an isolate to load, as bytes the host supplies.

use orbit_runtime::stdlib::is_orbitscript_file;

use crate::compiler::Language;

/// Which kinds of chunk [`Scope::load`] accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mode {
    /// Source text only.
    Text,
    /// Binary chunks (`orbitc` or `string.dump` output) only.
    Binary,
    /// Either, told apart by the first byte, as `load` does.
    #[default]
    Both,
}

/// Code to load: source text or a binary chunk, with the name messages
/// give it. The runtime reads no files: the host reads the bytes (from a
/// file, stdin, the network, its own binary) and hands them over.
///
/// ```
/// # use orbit::Chunk;
/// let code = b"return 6 * 7";
/// let chunk = Chunk::new(code).name("=answer");
/// ```
#[derive(Debug, Clone)]
pub struct Chunk<'a> {
    pub(crate) bytes: &'a [u8],
    pub(crate) name: Option<Vec<u8>>,
    pub(crate) language: Language,
    pub(crate) mode: Mode,
}

impl<'a> Chunk<'a> {
    pub fn new(bytes: &'a (impl AsRef<[u8]> + ?Sized)) -> Chunk<'a> {
        Chunk {
            bytes: bytes.as_ref(),
            name: None,
            language: Language::Auto,
            mode: Mode::Both,
        }
    }

    /// The chunk's name, as Lua writes source names: `@path` for a file
    /// (messages show the path), `=name` for anything else (messages show
    /// `name`). Without one, messages quote the source text, as for `load`.
    pub fn name(mut self, name: impl AsRef<[u8]>) -> Chunk<'a> {
        self.name = Some(name.as_ref().to_vec());
        self
    }

    /// The source language ([`Language::Auto`] by default: OrbitScript for
    /// a name ending in `.orb`, Lua otherwise).
    pub fn language(mut self, language: Language) -> Chunk<'a> {
        self.language = language;
        self
    }

    /// Which kinds of chunk to accept ([`Mode::Both`] by default).
    pub fn mode(mut self, mode: Mode) -> Chunk<'a> {
        self.mode = mode;
        self
    }

    /// The source name: the one given, or the text itself.
    pub(crate) fn source(&self) -> &[u8] {
        self.name.as_deref().unwrap_or(self.bytes)
    }

    /// The language to compile source text as.
    pub(crate) fn resolved_language(&self) -> Language {
        match self.language {
            Language::Auto if is_orbitscript_file(self.source()) => Language::OrbitScript,
            Language::Auto => Language::Lua,
            l => l,
        }
    }

    #[cfg(runtime_backend)]
    pub(crate) fn is_binary(&self) -> bool {
        self.bytes.first() == Some(&orbit_runtime::stdlib::BINARY_SIGNATURE)
    }
}

#[cfg(runtime_backend)]
pub(crate) use loading::load;

/// Loading over the runtime linked into this program; on the shared
/// backend, `liborbit` loads.
#[cfg(runtime_backend)]
mod loading {
    use orbit_runtime::object as raw;
    use orbit_runtime::stdlib::chunk_id;
    use orbit_runtime::vm::NativeCtx;

    use super::{Chunk, Mode};
    use crate::compiler::Compiler;

    /// Compile or decode `chunk` and link it with `env` as its `_ENV`,
    /// returning its main function (not yet rooted), or the message.
    pub(crate) fn load(
        ctx: &mut NativeCtx<'_>,
        chunk: &Chunk<'_>,
        env: raw::Value,
    ) -> Result<raw::Value, String> {
        let binary = chunk.is_binary();
        let allowed = match chunk.mode {
            Mode::Text => !binary,
            Mode::Binary => binary,
            Mode::Both => true,
        };
        if !allowed {
            let (what, mode) = if binary {
                ("binary", "t")
            } else {
                ("text", "b")
            };
            return Err(format!("attempt to load a {what} chunk (mode is '{mode}')"));
        }
        if binary {
            return load_binary(ctx, chunk, env);
        }
        let name = chunk_id(chunk.source());
        let mut unlinked = Compiler
            .compile(chunk.bytes, &name, chunk.resolved_language())
            .map_err(|e| e.0)?;
        // `debug.getinfo`'s `source` is the name as given, not its short form.
        unlinked.source = chunk.source().into();
        Ok(ctx.link(&unlinked, env))
    }

    #[cfg(feature = "dump")]
    fn load_binary(
        ctx: &mut NativeCtx<'_>,
        chunk: &Chunk<'_>,
        env: raw::Value,
    ) -> Result<raw::Value, String> {
        orbit_runtime::stdlib::load_chunk_in(ctx, chunk.bytes, chunk.source(), b"b", env)
    }

    #[cfg(not(feature = "dump"))]
    fn load_binary(
        _: &mut NativeCtx<'_>,
        chunk: &Chunk<'_>,
        _: raw::Value,
    ) -> Result<raw::Value, String> {
        Err(format!(
            "{}: bad binary format (binary chunks not enabled)",
            chunk_id(chunk.source())
        ))
    }
}
