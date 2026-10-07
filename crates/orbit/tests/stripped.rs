//! Error positions in a chunk without line information (a stripped binary
//! chunk), as lua5.5 reports them: `error` adds no position, and a runtime
//! error says `chunk:?:`.

use orbit::{Lua, compile};
use orbit_lua_syntax::ParseOptions;

fn run_stripped(src: &str) -> String {
    let mut chunk = compile(src.as_bytes(), "?", &ParseOptions::default()).unwrap();
    for p in &mut chunk.protos {
        p.debug.lines.clear();
    }
    let mut lua = Lua::new();
    let vm = lua.vm();
    let f = vm.load(&chunk);
    let out = lua.call(f, &[]).unwrap();
    let vm = lua.vm();
    String::from_utf8_lossy(&vm.to_display(out[0])).into_owned()
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
