//! Prints each file named on the command line back as fully parenthesised Lua source.

use std::io::Write;

fn main() {
    let mut out = std::io::stdout().lock();
    for path in std::env::args().skip(1) {
        let src = std::fs::read(&path).expect("read source");
        match orbit_lua_syntax::parse(&path, &src) {
            Ok(ast) => out.write_all(&orbit_lua_syntax::unparse(&ast)).unwrap(),
            Err(d) => {
                eprintln!("{}", d.to_lua_message(&path));
                std::process::exit(1);
            }
        }
    }
}
