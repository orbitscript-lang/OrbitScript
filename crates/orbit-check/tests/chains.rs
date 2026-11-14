//! Chains of operators and suffixes (`a + b + c`, `a.b(c)[d]`) and `else if` chains as long as
//! a program makes them. Each is a tree as deep as it is long, so checking, and the editor
//! queries on the result, walk it with a loop: 200,000 links must fit a 2 MiB thread stack
//! (Rust's default for spawned threads), in debug builds too.

use std::path::Path;

use orbit_check::{Config, Sources, Span, Workspace};

const STACK: usize = 2 << 20;
const LINKS: usize = 200_000;

/// A name, and the program with a chain of `n` links.
type Shape = (&'static str, fn(usize) -> String);

const SHAPES: &[Shape] = &[
    ("additions", |n| format!("let x = 1{};", " + 1".repeat(n))),
    ("fields", |n| {
        format!("extern a; let x = a{};", ".b".repeat(n))
    }),
    ("paths", |n| {
        format!("extern a; let x = a{};", "::b".repeat(n))
    }),
    ("indexing", |n| {
        format!("let a = [1]; let x = a{};", "[1]".repeat(n))
    }),
    ("calls", |n| {
        format!("extern f; let x = f{};", "(1)".repeat(n))
    }),
    ("method calls", |n| {
        format!("extern a; let x = a{};", ".m(1)".repeat(n))
    }),
    ("casts", |n| {
        format!("let a = 1; let x = a{};", " as int".repeat(n))
    }),
    ("tries", |n| {
        format!("extern a; fn f() {{ let x = a{}; x }}", "?".repeat(n))
    }),
    ("mixed suffixes", |n| {
        format!("extern a; let x = a{};", ".b[1].m()(2)?".repeat(n / 5))
    }),
    ("ands", |n| {
        format!("let a = 1; let x = a{};", " && a".repeat(n))
    }),
    ("ors", |n| {
        format!("let a = 1; let x = a{};", " || a".repeat(n))
    }),
    ("conditions", |n| {
        format!("let a = true; while a{} {{}}", " && a || a".repeat(n / 2))
    }),
    ("else ifs", |n| {
        format!(
            "let a = 1; let x = if a == 1 {{ 1 }} {}else {{ 2 }};",
            "else if a == 2 { 1 } ".repeat(n)
        )
    }),
];

struct NoFiles;

impl Sources for NoFiles {
    fn read(&self, _: &Path) -> Option<Vec<u8>> {
        None
    }
}

#[test]
fn long_chains_fit_a_2_mib_stack() {
    std::thread::Builder::new()
        .stack_size(STACK)
        .spawn(|| {
            for (shape, chain) in SHAPES {
                let src = chain(LINKS);
                let mut ws = Workspace::new(Config::default(), Box::new(NoFiles));
                let f = ws.set_source(Path::new("/m/chain.orb"), src.clone().into_bytes());
                // Some shapes get a diagnostic or two (unused or never-nil values), but
                // none per link, and no syntax error.
                let found: Vec<_> = ws
                    .diagnostics(f)
                    .iter()
                    .map(|d| format!("{}: {}", d.code.as_str(), d.message))
                    .collect();
                assert!(
                    found.len() < 5 && !found.iter().any(|d| d.starts_with("T0001")),
                    "{shape}: {} diagnostics, {:?}",
                    found.len(),
                    &found[..found.len().min(3)]
                );
                let end = src.len() as u32 - 2;
                ws.type_at(f, end);
                ws.symbol_at(f, end);
                ws.completions_at(f, end);
                ws.signature_at(f, end);
                ws.document_symbols(f);
                ws.inlay_hints(
                    f,
                    Span {
                        start: 0,
                        end: src.len() as u32,
                    },
                );
            }
        })
        .unwrap()
        .join()
        .unwrap();
}
