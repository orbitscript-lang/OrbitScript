//! Chunk names and kinds, which hosts need whatever libraries are built.

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

/// Whether a chunk's source names an OrbitScript file (`@path.orb`).
pub fn is_orbitscript_file(source: &[u8]) -> bool {
    source.first() == Some(&b'@') && source.ends_with(b".orb")
}
