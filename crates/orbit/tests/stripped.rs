//! Error positions in a chunk without line information (a stripped binary
//! chunk), as lua5.5 reports them: `error` adds no position, and a runtime
//! error says `chunk:?:`.

use orbit::{Chunk, Compiler, Config, Language, Runtime};
use orbit_bytecode::format;

fn run_stripped(src: &str) -> String {
    let mut chunk = Compiler
        .compile(src.as_bytes(), "?", Language::Lua)
        .unwrap();
    // Only the line information goes, as in the chunk a C host strips by
    // hand; names of locals stay.
    for p in &mut chunk.protos {
        p.debug.lines.clear();
    }
    let bytes = format::encode(&chunk, false);
    let mut iso = Runtime::new().new_isolate(Config::default());
    iso.scope(|s| {
        let out = s.load(Chunk::new(&bytes).name("=?"))?.call(())?;
        out[0].get()
    })
    .unwrap()
}

#[test]
fn stripped_chunks_have_no_error_lines() {
    assert_eq!(run_stripped("return select(2, pcall(error, 'e'))"), "e");
    assert_eq!(
        run_stripped("return select(2, pcall(function() error('e') end))"),
        "e"
    );
    assert_eq!(
        run_stripped("return select(2, pcall(function() local x; return x.y end))"),
        "?:?: attempt to index a nil value (local 'x')"
    );
}
