//! The printer walks chains of operators and suffixes and `elseif` chains as long as a
//! program makes them with loops, as the parser does: 200,000 links must fit a 2 MiB thread
//! stack, in debug builds too. The shapes are orbit-lua-syntax's (tests/chains.rs).

use orbit_lua_syntax::{ParseOptions, parse_with};
use orbit_lua_unparse::unparse;

const STACK: usize = 2 << 20;
const LINKS: usize = 200_000;

/// A name, and the program with a chain of `n` links.
type Shape = (&'static str, fn(usize) -> String);

const SHAPES: &[Shape] = &[
    ("additions", |n| format!("return 1{}", " + 1".repeat(n))),
    ("comparisons", |n| format!("return a{}", " < a".repeat(n))),
    ("fields", |n| format!("return a{}", ".b".repeat(n))),
    ("indexing", |n| format!("return a{}", "[1]".repeat(n))),
    ("calls", |n| format!("return f{}", "()".repeat(n))),
    ("method calls", |n| format!("return a{}", ":m()".repeat(n))),
    ("string arguments", |n| {
        format!("return f{}", "'x'".repeat(n))
    }),
    ("table arguments", |n| format!("return f{}", "{}".repeat(n))),
    ("mixed suffixes", |n| {
        format!("return a{}", ".b[1]:m()(2)".repeat(n / 4))
    }),
    ("ands", |n| format!("return a{}", " and a".repeat(n))),
    ("ors", |n| format!("return a{}", " or a".repeat(n))),
    ("conditions", |n| {
        format!("while a{} do end", " and a or a".repeat(n / 2))
    }),
    ("elseifs", |n| {
        format!("if a then {}end", "elseif a then ".repeat(n))
    }),
];

#[test]
fn long_chains_print_on_a_2_mib_stack() {
    std::thread::Builder::new()
        .stack_size(STACK)
        .spawn(|| {
            let options = ParseOptions::default();
            for (shape, chain) in SHAPES {
                let src = chain(LINKS);
                let ast = parse_with("test", src.as_bytes(), &options)
                    .unwrap_or_else(|d| panic!("{shape}: {}", d.to_message("test")));
                // Fully parenthesised, so too deeply nested to parse back.
                assert!(!unparse(&ast).is_empty(), "{shape}");
            }
        })
        .unwrap()
        .join()
        .unwrap();
}
