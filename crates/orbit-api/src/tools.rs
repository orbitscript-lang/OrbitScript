//! The compiler's tools: what `orbitc` prints and writes. Each takes a
//! chunk's bytes and its source name (`@file` or `=stdin`), as
//! [`read_chunk_file`] returns them; OrbitScript source is told apart by a
//! `.orb` file name.

use crate::imp;

/// A compiled chunk, as `string.dump` writes it, and its listing when
/// asked for.
#[derive(Clone, Debug)]
pub struct Compiled {
    pub chunk: Vec<u8>,
    pub listing: Option<String>,
}

/// A file's contents with any first `#` line blanked, and its source name;
/// `None` reads standard input.
pub fn read_chunk_file(path: Option<&[u8]>) -> Result<(Vec<u8>, Vec<u8>), String> {
    imp::tools::read_chunk_file(path)
}

/// A source name as messages show it (`file.lua` for `@file.lua`).
pub fn chunk_id(source: &[u8]) -> String {
    imp::tools::chunk_id(source)
}

/// The syntax tree, printed back as source (Lua only for now).
pub fn syntax_tree(bytes: &[u8], source: &[u8]) -> Result<Vec<u8>, String> {
    imp::tools::syntax_tree(bytes, source)
}

/// The HIR, printed.
pub fn hir(bytes: &[u8], source: &[u8]) -> Result<String, String> {
    imp::tools::hir(bytes, source)
}

/// Compile source, or check a binary chunk; `strip` drops debug
/// information and `list` adds the bytecode listing.
pub fn compile(bytes: &[u8], source: &[u8], strip: bool, list: bool) -> Result<Compiled, String> {
    imp::tools::compile(bytes, source, strip, list)
}
