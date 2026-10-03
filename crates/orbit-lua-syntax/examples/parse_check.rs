//! Runs the front end (parse and lower) on each file named on the command line and prints `ok`
//! or the Lua-format error, one record per file. Used for differential syntax testing against the reference `load`.

use std::io::Write;

fn main() {
    let mut out = std::io::stdout().lock();
    for path in std::env::args().skip(1) {
        let src = std::fs::read(&path).expect("read source");
        let options = orbit_lua_syntax::ParseOptions::default();
        let line = match orbit_lua_syntax::to_hir("s.lua", &src, &options) {
            Ok(_) => b"ok".to_vec(),
            Err(d) => d.to_lua_message_bytes(b"s.lua"),
        };
        out.write_all(&line).unwrap();
        out.write_all(b"\x1e").unwrap();
    }
}
