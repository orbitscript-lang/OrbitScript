//! Lowering's two modes agree. `lower` is the compiler's: it stops at the first error.
//! `lower_collecting` is the checker's: it recovers and gives every error. The first error
//! it gives is the compiler's, it gives none exactly when the compiler accepts the module, and
//! it only reads the tree.

use std::path::{Path, PathBuf};

use orbit_orb_syntax::{
    Code, ParseOptions, code, lower, lower_collecting, parse_recovering, parse_with,
};

/// The OrbitScript programs of the examples, the benchmarks and this crate's tests.
fn programs() -> Vec<PathBuf> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut out = Vec::new();
    let mut dirs = vec![
        root.join("examples"),
        root.join("bench/orb"),
        root.join("crates/orbit-orb-syntax/tests/programs"),
        root.join("crates/orbit-check/tests/cases"),
    ];
    while let Some(dir) = dirs.pop() {
        for e in std::fs::read_dir(&dir).unwrap() {
            let p = e.unwrap().path();
            if p.is_dir() {
                dirs.push(p);
            } else if p.extension().is_some_and(|x| x == "orb") {
                out.push(p);
            }
        }
    }
    out.sort();
    assert!(out.len() > 60, "found {}", out.len());
    out
}

/// The options a checker parses with, so the tree has everything a tool keeps.
fn options() -> ParseOptions {
    ParseOptions {
        type_trees: true,
        doc_comments: true,
        keep_trivia: true,
        ..Default::default()
    }
}

fn codes(src: &str) -> Vec<Code> {
    let (ast, syntax) = parse_recovering("test.orb", src.as_bytes(), &options());
    assert!(syntax.is_empty(), "{syntax:?}");
    lower_collecting(&ast).iter().map(|d| d.code).collect()
}

#[test]
fn the_first_error_collected_is_the_compilers() {
    let (mut lowered, mut refused) = (0, 0);
    for path in programs() {
        let name = path.display().to_string();
        let src = std::fs::read(&path).unwrap();
        let Ok(ast) = parse_with(&name, &src, &options()) else {
            continue;
        };
        let before = format!("{:?}", ast.body);
        let names = ast.strings.len();
        let collected = lower_collecting(&ast);
        // The tree and its pool are untouched: lowering's own names went into a copy.
        assert_eq!(format!("{:?}", ast.body), before, "{name}");
        assert_eq!(ast.strings.len(), names, "{name}");

        match lower(ast) {
            Ok(_) => {
                assert_eq!(collected, [], "{name}");
                lowered += 1;
            }
            Err(first) => {
                assert_eq!(collected.first(), Some(&first), "{name}");
                refused += 1;
            }
        }
    }
    // Programs that compile, and the checker's cases that break a rule.
    assert!(
        lowered > 20 && refused > 20,
        "{lowered} lowered, {refused} refused"
    );
}

#[test]
fn every_error_is_collected() {
    let src = r#"
fn f(a, a) {
    break;
    undefined_one + undefined_two
}
const N = f(1, 2);
let x = 1;
x = 2;
struct P { x, x }
let p = P { y: 1 };
"#;
    assert_eq!(
        codes(src),
        [
            code::DUPLICATE_PARAMETER,
            code::OUTSIDE_LOOP,
            code::UNKNOWN_NAME,
            code::UNKNOWN_NAME,
            code::CONST_NOT_LITERAL,
            code::ASSIGN_TO_IMMUTABLE,
            code::DUPLICATE_FIELD,
            code::UNKNOWN_FIELD,
            code::MISSING_FIELD,
        ]
    );
}

#[test]
fn one_mistake_is_one_error() {
    // An unknown name is not also an immutable variable, a non-constant or a non-type.
    assert_eq!(codes("nowhere = 1;"), [code::UNKNOWN_NAME]);
    assert_eq!(codes("const N = nowhere;"), [code::UNKNOWN_NAME]);
    assert_eq!(
        codes("impl Nowhere { fn new() -> Self { Self {} } fn get(self) { Nowhere::new() } }"),
        [code::UNKNOWN_NAME]
    );
    // A pattern `let` can't have still declares its names.
    assert_eq!(codes("let (a, 1) = (1, 2); print(a);"), [code::LET_PATTERN]);
    assert_eq!(codes("let v | 1 = 1; print(v);"), [code::LET_PATTERN]);
    // A path is read for its kind and again for its value; its error is reported once.
    assert_eq!(
        codes("enum E { A } match 1 { E::B => 1, _ => 2 };"),
        [code::NO_ASSOCIATED_ITEM]
    );
    // An item used before its definition is then known.
    assert_eq!(
        codes("let a = f(); let b = f(); fn f() { 1 }"),
        [code::USED_BEFORE_DEFINITION, code::USED_BEFORE_DEFINITION]
    );
    // `as` with a built-in type name looks nothing up.
    assert_eq!(codes("let a = 1 as int; let b = a as? string;"), []);
}

#[test]
fn syntax_errors_are_passed_over() {
    let src = "let a = ;\nfn f() { let b = 1 + ; missing }\nlet c = a;";
    let (ast, syntax) = parse_recovering("test.orb", src.as_bytes(), &options());
    assert!(!syntax.is_empty());
    let found: Vec<Code> = lower_collecting(&ast).iter().map(|d| d.code).collect();
    assert_eq!(found, [code::UNKNOWN_NAME]);
}
