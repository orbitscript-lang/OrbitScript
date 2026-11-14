//! Prints each file named on the command line back as fully parenthesised Lua source.

use std::io::Write;

fn main() {
    let mut out = std::io::stdout().lock();
    for arg in std::env::args_os().skip(1) {
        let src = std::fs::read(&arg).expect("read source");
        let path = arg.to_string_lossy();
        match orbit_lua_syntax::parse(&path, &src) {
            Ok(ast) => out.write_all(&orbit_lua_syntax::unparse(&ast)).unwrap(),
            Err(d) => {
                eprintln!("{}", d.to_message(&path));
                std::process::exit(1);
            }
        }
    }
}
