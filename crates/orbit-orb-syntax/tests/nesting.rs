//! Deeply nested programs give a chunk or a "nested too deeply" error, never a stack
//! overflow. Parsing, lowering and compiling all recurse, so the parser's depth limit only
//! protects the stack if every pass fits within it: 200 levels must fit a 2 MiB thread stack
//! (Rust's default for spawned threads) on every platform, in debug builds too, whose frames
//! are much larger than release ones.

use orbit_orb_syntax::{ParseOptions, to_hir};

const STACK: usize = 2 << 20;

/// A name, and the program nesting one construct `n` deep.
type Shape = (&'static str, fn(usize) -> String);

const SHAPES: &[Shape] = &[
    ("parentheses", |n| {
        format!("let x = {}1{};", "(".repeat(n), ")".repeat(n))
    }),
    ("arrays", |n| {
        format!("let x = {}1{};", "[".repeat(n), "]".repeat(n))
    }),
    ("maps", |n| {
        format!("let x = {}1{};", "#{a: ".repeat(n), "}".repeat(n))
    }),
    ("unary operators", |n| {
        format!("let x = {}1;", "-".repeat(n))
    }),
    ("binary operators", |n| {
        format!("let x = {}1;", "1 ** ".repeat(n))
    }),
    ("calls", |n| {
        format!(
            "fn f(x) {{ x }} let x = {}1{};",
            "f(".repeat(n),
            ")".repeat(n)
        )
    }),
    ("method calls", |n| {
        format!(
            "let a = #{{}}; let x = {}1{};",
            "a.m(".repeat(n),
            ")".repeat(n)
        )
    }),
    ("indexing", |n| {
        format!("let a = [1]; let x = {}1{};", "a[".repeat(n), "]".repeat(n))
    }),
    ("closures", |n| format!("let x = {}1;", "|| ".repeat(n))),
    ("blocks", |n| {
        format!("let x = {}1{};", "{".repeat(n), "}".repeat(n))
    }),
    ("blocks with statements", |n| {
        format!("let x = {}1{};", "{ let y = ".repeat(n), "; y }".repeat(n))
    }),
    ("ifs", |n| {
        format!("let a = 1; {}{}", "if a {".repeat(n), "}".repeat(n))
    }),
    ("matches", |n| {
        format!(
            "let a = 1; let x = {}1{};",
            "match a { _ => ".repeat(n),
            "}".repeat(n)
        )
    }),
    ("loops", |n| {
        format!("let a = 1; {}{}", "while a {".repeat(n), "}".repeat(n))
    }),
    ("functions", |n| {
        format!("{}{}", "fn f() {".repeat(n), "}".repeat(n))
    }),
];

/// Runs the whole front end and the compiler on `src`.
fn compile(src: &str) -> Result<(), String> {
    let hir = to_hir("test", src.as_bytes(), &ParseOptions::default())
        .map_err(|d| d.to_message("test"))?;
    orbit_compiler::compile(&hir).map_err(|e| e.to_string())?;
    Ok(())
}

#[test]
fn nesting_at_the_limit_fits_a_2_mib_stack() {
    std::thread::Builder::new()
        .stack_size(STACK)
        .spawn(|| {
            for (shape, nest) in SHAPES {
                // Far past the limit: an error.
                let err = compile(&nest(1000)).expect_err(shape);
                assert!(err.contains("nested too deeply"), "{shape}: {err}");
                // The deepest nesting accepted, which every pass must handle. (Some shapes
                // run out of registers first.)
                let (mut ok, mut too_deep) = (1, 1000);
                compile(&nest(ok)).unwrap_or_else(|e| panic!("{shape}: {e}"));
                while too_deep - ok > 1 {
                    let mid = (ok + too_deep) / 2;
                    match compile(&nest(mid)) {
                        Ok(()) => ok = mid,
                        Err(_) => too_deep = mid,
                    }
                }
                assert!(ok >= 40, "{shape}: only {ok} levels accepted");
            }
        })
        .unwrap()
        .join()
        .unwrap();
}
