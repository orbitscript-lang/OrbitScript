//! Chains of operators and suffixes (`a + b + c`, `a.b(c)[d]`) and `elseif` chains as long as
//! a program makes them, which lua5.5 accepts. Each is a tree as deep as it is long, so every
//! pass walks it with a loop: 200,000 links must fit a 2 MiB thread stack (Rust's default for
//! spawned threads), in debug builds too.

use orbit_lua_syntax::{ParseOptions, parse_with, to_hir, unparse};

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
fn long_chains_fit_a_2_mib_stack() {
    std::thread::Builder::new()
        .stack_size(STACK)
        .spawn(|| {
            let options = ParseOptions::default();
            for (shape, chain) in SHAPES {
                let src = chain(LINKS);
                let ast = parse_with("test", src.as_bytes(), &options)
                    .unwrap_or_else(|d| panic!("{shape}: {}", d.to_lua_message("test")));
                assert!(!unparse(&ast).is_empty(), "{shape}");
                drop(ast);
                let hir = to_hir("test", src.as_bytes(), &options)
                    .unwrap_or_else(|d| panic!("{shape}: {}", d.to_lua_message("test")));
                assert!(!orbit_hir::print(&hir).is_empty(), "{shape}");
                orbit_compiler::compile(&hir).unwrap_or_else(|e| panic!("{shape}: {e}"));
            }
        })
        .unwrap()
        .join()
        .unwrap();
}
