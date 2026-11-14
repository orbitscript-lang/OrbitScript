//! Chains of operators and suffixes (`a + b + c`, `a.b(c)[d]`) and `else if` chains as long as
//! a program makes them, as Lua allows. Each is a tree as deep as it is long, so every pass
//! walks it with a loop: 200,000 links must fit a 2 MiB thread stack (Rust's default for
//! spawned threads), in debug builds too.

use orbit_orb_syntax::{ParseOptions, parse_with, to_hir};

const STACK: usize = 2 << 20;
const LINKS: usize = 200_000;

/// A name, and the program with a chain of `n` links.
type Shape = (&'static str, fn(usize) -> String);

const SHAPES: &[Shape] = &[
    ("additions", |n| format!("let x = 1{};", " + 1".repeat(n))),
    ("fields", |n| {
        format!("let a = #{{}}; let x = a{};", ".b".repeat(n))
    }),
    ("paths", |n| {
        format!("let a = #{{}}; let x = a{};", "::b".repeat(n))
    }),
    ("indexing", |n| {
        format!("let a = [1]; let x = a{};", "[1]".repeat(n))
    }),
    ("calls", |n| {
        format!("fn f() {{}} let x = f{};", "()".repeat(n))
    }),
    ("method calls", |n| {
        format!("let a = #{{}}; let x = a{};", ".m()".repeat(n))
    }),
    ("casts", |n| {
        format!("let a = 1; let x = a{};", " as int".repeat(n))
    }),
    ("mixed suffixes", |n| {
        format!("let a = #{{}}; let x = a{};", ".b[1].m()(2)".repeat(n / 4))
    }),
    ("ands", |n| {
        format!("let a = 1; let x = a{};", " && a".repeat(n))
    }),
    ("ors", |n| {
        format!("let a = 1; let x = a{};", " || a".repeat(n))
    }),
    ("conditions", |n| {
        format!("let a = 1; while a{} {{}}", " && a || a".repeat(n / 2))
    }),
    ("else ifs", |n| {
        format!(
            "let a = 1; let x = if a {{ 1 }} {}else {{ 2 }};",
            "else if a { 1 } ".repeat(n)
        )
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
                    .unwrap_or_else(|d| panic!("{shape}: {}", d.to_message("test")));
                assert!(!format!("{:?}", ast.body).is_empty(), "{shape}");
                drop(ast);
                let hir = to_hir("test", src.as_bytes(), &options)
                    .unwrap_or_else(|d| panic!("{shape}: {}", d.to_message("test")));
                assert!(!orbit_hir::print(&hir).is_empty(), "{shape}");
                orbit_compiler::compile(&hir).unwrap_or_else(|e| panic!("{shape}: {e}"));
            }
            // Each `?` holds its values in locals of their own, so a long chain of them runs
            // out of registers, but it's an error, not a crash.
            let src = format!("fn f(a) {{ let x = a{}; }}", "?".repeat(LINKS));
            let hir = to_hir("test", src.as_bytes(), &options).unwrap();
            let err = orbit_compiler::compile(&hir).unwrap_err().to_string();
            assert!(err.contains("too many registers"), "{err}");
        })
        .unwrap()
        .join()
        .unwrap();
}
