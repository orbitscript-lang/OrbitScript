//! Reading scripts from files and standard input, which is the host's job:
//! the runtime only takes bytes.

/// The chunk name `lua` gives standard input.
pub const STDIN: &[u8] = b"=stdin";

/// Read a script as `loadfile` does: a file (`None`: standard input), less
/// a UTF-8 byte-order mark and a first line starting with `#`. Returns the
/// bytes and the chunk's source name (`@path`).
pub fn read_chunk_file(path: Option<&str>) -> Result<(Vec<u8>, Vec<u8>), String> {
    use std::io::Read;
    let (mut bytes, source) = match path {
        None => {
            let mut b = Vec::new();
            std::io::stdin()
                .read_to_end(&mut b)
                .map_err(|_| "cannot read stdin".to_string())?;
            (b, STDIN.to_vec())
        }
        Some(p) => {
            let b =
                std::fs::read(p).map_err(|e| format!("cannot open {p}: {}", os_error_text(&e)))?;
            (b, format!("@{p}").into_bytes())
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
        // A binary chunk may follow the `#` line directly.
        start = match bytes.get(end + 1) {
            Some(&0x1b) => end + 1,
            _ => end,
        };
    }
    bytes.drain(..start);
    Ok((bytes, source))
}

/// An I/O error as C's `strerror` words it, which `lua` prints.
fn os_error_text(e: &std::io::Error) -> String {
    let text = e.to_string();
    // Rust appends ` (os error N)`.
    match text.find(" (os error") {
        Some(i) => text[..i].to_string(),
        None => text,
    }
}
