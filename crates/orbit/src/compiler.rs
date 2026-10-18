//! [`Compiler`]: the front ends and the bytecode compiler, with no isolate.

use orbit_compiler::bytecode::UnlinkedChunk;
use orbit_lua_syntax::ParseOptions;

use crate::chunk::Chunk;
use crate::error::{Error, ErrorKind};

/// The language of source text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Language {
    /// OrbitScript for a chunk named `@...orb`, Lua otherwise.
    #[default]
    Auto,
    Lua,
    OrbitScript,
}

/// Compiles source text to bytecode without an isolate, so it can run on
/// any thread (it is `Send` and `Sync`). Like the rest of the runtime it
/// reads no files: it takes the bytes the host read.
#[derive(Debug, Clone, Copy, Default)]
pub struct Compiler;

impl Compiler {
    /// Compile `src` as `language` ([`Language::Auto`] means Lua here,
    /// since there is no file name to go by). `chunk_name` is the name
    /// messages show (see [`Compiler::chunk_id`]).
    pub fn compile(
        &self,
        src: &[u8],
        chunk_name: &str,
        language: Language,
    ) -> Result<UnlinkedChunk, SyntaxError> {
        match language {
            Language::Auto | Language::Lua => {
                self.compile_lua(src, chunk_name, &ParseOptions::default())
            }
            Language::OrbitScript => self.compile_orbitscript(
                src,
                chunk_name,
                &orbit_orb_syntax::ParseOptions::default(),
            ),
        }
    }

    /// Compile a chunk's source text, picking the language from its name
    /// when it is [`Language::Auto`].
    pub fn compile_chunk(&self, chunk: &Chunk<'_>) -> Result<UnlinkedChunk, SyntaxError> {
        let mut c = self.compile(
            chunk.bytes,
            &Compiler::chunk_id(chunk.source()),
            chunk.resolved_language(),
        )?;
        c.source = chunk.source().into();
        Ok(c)
    }

    /// Parse, lower, analyse and compile Lua source.
    pub fn compile_lua(
        &self,
        src: &[u8],
        chunk_name: &str,
        options: &ParseOptions,
    ) -> Result<UnlinkedChunk, SyntaxError> {
        let hir = orbit_lua_syntax::to_hir(chunk_name, src, options)
            .map_err(|d| SyntaxError(d.to_lua_message(chunk_name)))?;
        compile_hir(&hir)
    }

    /// Parse, lower, analyse and compile OrbitScript source.
    pub fn compile_orbitscript(
        &self,
        src: &[u8],
        chunk_name: &str,
        options: &orbit_orb_syntax::ParseOptions,
    ) -> Result<UnlinkedChunk, SyntaxError> {
        let hir = orbit_orb_syntax::to_hir(chunk_name, src, options)
            .map_err(|d| SyntaxError(d.to_message(chunk_name)))?;
        compile_hir(&hir)
    }

    /// The name messages show for a chunk whose source name is `source`
    /// (Lua's `short_src`): `@file` shows the file, `=name` the name, and
    /// anything else is quoted source text.
    pub fn chunk_id(source: &[u8]) -> String {
        orbit_runtime::stdlib::chunk_id(source)
    }

    /// Whether a source name names an OrbitScript file (`@path.orb`).
    pub fn is_orbitscript_name(source: &[u8]) -> bool {
        orbit_runtime::stdlib::is_orbitscript_file(source)
    }
}

/// A compile error: `chunk:line: message`. Unlike [`Error`] it holds no
/// script value, so it can be sent between threads with the compiler.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyntaxError(pub String);

impl std::fmt::Display for SyntaxError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for SyntaxError {}

impl From<SyntaxError> for Error {
    fn from(e: SyntaxError) -> Error {
        Error::new(ErrorKind::Syntax, e.0)
    }
}

/// Compile analysed HIR from either front end.
fn compile_hir(hir: &orbit_hir::Chunk) -> Result<UnlinkedChunk, SyntaxError> {
    let chunk = orbit_compiler::compile(hir).map_err(|e| SyntaxError(e.to_string()))?;
    // Debug builds check the compiler against the binary-chunk verifier, so
    // every test that compiles Lua keeps the two in agreement.
    #[cfg(debug_assertions)]
    if let Err(e) = orbit_compiler::bytecode::verify::verify(
        &chunk,
        orbit_compiler::bytecode::verify::Level::STRONGEST,
    ) {
        panic!("compiler output fails verification: {e}");
    }
    Ok(chunk)
}

/// The compiler `load` uses for Lua source ([`orbit_runtime::vm::CompileFn`]).
#[cfg(runtime_backend)]
pub(crate) fn compile_lua(src: &[u8], chunk_name: &str) -> Result<UnlinkedChunk, String> {
    Compiler
        .compile(src, chunk_name, Language::Lua)
        .map_err(|e| e.to_string())
}

/// The compiler `load` uses for `.orb` files.
#[cfg(runtime_backend)]
pub(crate) fn compile_orb(src: &[u8], chunk_name: &str) -> Result<UnlinkedChunk, String> {
    Compiler
        .compile(src, chunk_name, Language::OrbitScript)
        .map_err(|e| e.to_string())
}
