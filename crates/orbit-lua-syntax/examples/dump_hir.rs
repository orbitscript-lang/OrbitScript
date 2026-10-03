//! Prints the analysed HIR of each file named on the command line.

fn main() {
    for path in std::env::args().skip(1) {
        let src = std::fs::read(&path).expect("read source");
        let options = orbit_lua_syntax::ParseOptions {
            skip_comment_line: true,
            ..Default::default()
        };
        match orbit_lua_syntax::to_hir(&path, &src, &options) {
            Ok(chunk) => print!("{}", orbit_hir::print(&chunk)),
            Err(d) => {
                eprintln!("{}", d.to_lua_message(&path));
                std::process::exit(1);
            }
        }
    }
}
