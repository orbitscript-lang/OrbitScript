//! Prints the analysed HIR of each file named on the command line.

fn main() {
    for arg in std::env::args_os().skip(1) {
        let src = std::fs::read(&arg).expect("read source");
        let path = arg.to_string_lossy();
        let options = orbit_lua_syntax::ParseOptions {
            skip_comment_line: true,
            ..Default::default()
        };
        match orbit_lua_syntax::to_hir(&path, &src, &options) {
            Ok(chunk) => print!("{}", orbit_hir::print(&chunk)),
            Err(d) => {
                eprintln!("{}", d.to_message(&path));
                std::process::exit(1);
            }
        }
    }
}
