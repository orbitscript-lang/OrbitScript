//! Parses Lua files and reports the first error in each, as `luac -p` would.
//!
//! Usage: `cargo run -p orbit-lua-syntax --example parse_files -- FILE...`

use std::process::ExitCode;

fn main() -> ExitCode {
    let mut failed = false;
    for path in std::env::args().skip(1) {
        let src = match std::fs::read(&path) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("{path}: {e}");
                failed = true;
                continue;
            }
        };
        let src = orbit_lua_syntax::skip_file_prefix(&src);
        if let Err(e) = orbit_lua_syntax::parse(src) {
            let chunkname = format!("@{path}");
            let msg = e.to_lua_message(chunkname.as_bytes());
            eprintln!("{}", String::from_utf8_lossy(&msg));
            failed = true;
        }
    }
    if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}
