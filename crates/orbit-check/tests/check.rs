//! Checks the `tests/cases` programs against the codes marked in them, the repository's
//! OrbitScript programs for no errors, and the language server's name queries.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use orbit_check::{Code, Config, DiskSources, Sources, SymbolKind, Workspace};

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// Checks every `.orb` under `dir` with `dir` as the search root; returns (file, line, code).
fn check_dir(dir: &Path) -> (Workspace, BTreeSet<(String, u32, String)>) {
    let mut ws = Workspace::new(
        Config {
            root: dir.to_path_buf(),
            ..Config::default()
        },
        Box::new(DiskSources),
    );
    let mut files: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|x| x == "orb"))
        .collect();
    files.sort();
    for f in &files {
        ws.add_file(f);
    }
    let results: Vec<_> = ws
        .check_all()
        .into_iter()
        .map(|(f, d)| (f, d.to_vec()))
        .collect();
    let mut found = BTreeSet::new();
    for (file, diags) in results {
        let name = ws
            .path(file)
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        for d in diags {
            let (line, _) = ws.line_col(file, d.span.start);
            found.insert((name.clone(), line, d.code.as_str().to_string()));
        }
    }
    (ws, found)
}

#[test]
fn cases() {
    let cases = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/cases");
    for dir in std::fs::read_dir(&cases).unwrap() {
        let dir = dir.unwrap().path();
        let mut expected = BTreeSet::new();
        for f in std::fs::read_dir(&dir).unwrap() {
            let f = f.unwrap().path();
            let text = std::fs::read_to_string(&f).unwrap();
            let name = f.file_name().unwrap().to_string_lossy().into_owned();
            for (i, line) in text.lines().enumerate() {
                if let Some((_, codes)) = line.split_once("//~") {
                    for code in codes.split_whitespace() {
                        expected.insert((name.clone(), i as u32 + 1, code.to_string()));
                    }
                }
            }
        }
        let (_, mut found) = check_dir(&dir);
        // Most cases bind values only to look at their types, so the unused-name lints are
        // compared only in the directory about lints.
        if !dir.ends_with("lints") {
            found.retain(|(_, _, code)| code != "T0309" && code != "T0310");
        }
        assert_eq!(found, expected, "diagnostics in {}", dir.display());
    }
}

/// Every program the front end runs must resolve cleanly, since lowering accepts them, and
/// type check with only the findings listed here.
#[test]
fn repository_programs_check() {
    // `rounds` comes from the command line and may be 0, which leaves `dist` nil.
    let known: BTreeSet<_> = [("dijkstra.orb", 100, "T0207")]
        .into_iter()
        .map(|(f, l, c)| (f.to_string(), l, c.to_string()))
        .collect();
    let mut all = BTreeSet::new();
    let examples = root().join("examples");
    let mut dirs: Vec<_> = std::fs::read_dir(&examples)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.is_dir())
        .collect();
    dirs.sort();
    assert!(dirs.len() >= 10, "examples found: {dirs:?}");
    for dir in dirs {
        let (_, found) = check_dir(&dir);
        all.extend(found);
    }
    assert_eq!(all, known, "findings in examples/");
    // The front end's sample programs (`proposal.orb` opts out with `//! orbit-check: off`).
    for dir in ["crates/orbit-orb-syntax/tests/programs", "bench/orb"] {
        let (_, found) = check_dir(&root().join(dir));
        assert!(found.is_empty(), "findings in {dir}: {found:?}");
    }
    let (_, found) = check_dir(&root().join("crates/orbit-runtime/src/stdlib/builtins"));
    // The built-ins take their intrinsics from a host module that isn't a file.
    assert!(
        found.iter().all(|(_, _, code)| code == "T0002"),
        "built-ins: {found:?}"
    );
}

/// The unused-name lints, which tests that bind values only to look at them leave out.
fn is_unused_lint(d: &orbit_check::Diagnostic) -> bool {
    matches!(d.code, Code::UnusedLocal | Code::UnusedImport)
}

/// What `file` reports apart from the unused-name lints.
fn findings(ws: &mut Workspace, file: orbit_check::FileId) -> Vec<orbit_check::Diagnostic> {
    ws.diagnostics(file)
        .iter()
        .filter(|d| !is_unused_lint(d))
        .cloned()
        .collect()
}

/// In-memory sources, as the language server's open buffers would be.
struct Memory(Vec<(&'static str, &'static str)>);

/// `name` against the current directory, as the workspace keeps paths.
fn absolute(name: &str) -> PathBuf {
    std::env::current_dir().unwrap().join(name)
}

impl Sources for Memory {
    /// `path` comes made absolute: `/w/lib.orb` is `D:\w\lib.orb` on Windows.
    fn read(&self, path: &Path) -> Option<Vec<u8>> {
        self.0
            .iter()
            .find(|(name, _)| absolute(name) == path)
            .map(|(_, text)| text.as_bytes().to_vec())
    }
}

const LIB: &str = "pub struct P { x: int }\nimpl P {\n    pub fn new(x) -> P { P { x } }\n    pub fn get(self) -> int { self.x }\n}\npub fn helper() {}\n";
const APP: &str = "use lib::{P, helper};\nuse lib;\nlet p = P::new(1);\nhelper();\nlib::helper();\nprint(p.get());\n";

fn workspace() -> Workspace {
    Workspace::new(
        Config {
            root: PathBuf::from("/w"),
            ..Config::default()
        },
        Box::new(Memory(vec![("/w/lib.orb", LIB), ("/w/app.orb", APP)])),
    )
}

#[test]
fn definition_and_references_across_modules() {
    let mut ws = workspace();
    let app = ws.add_file(Path::new("/w/app.orb"));
    assert!(
        findings(&mut ws, app).is_empty(),
        "{:?}",
        findings(&mut ws, app)
    );
    // `helper` on line 4 of app.orb refers to lib's `helper`.
    let at = APP.find("helper();").unwrap() as u32;
    let id = ws.symbol_at(app, at).expect("a symbol");
    let def = ws.definition(id).unwrap();
    assert_eq!(ws.path(def.file), absolute("/w/lib.orb"));
    assert_eq!(
        &LIB[def.span.start as usize..def.span.end as usize],
        "helper"
    );
    assert_eq!(ws.symbol(id).unwrap().kind, SymbolKind::Function);
    // The definition, the `use` member, the call and `lib::helper`.
    assert_eq!(ws.references(id).len(), 4);

    let at = APP.find("new").unwrap() as u32;
    let id = ws.symbol_at(app, at).expect("P::new");
    assert_eq!(ws.symbol(id).unwrap().kind, SymbolKind::AssocFn);
    assert!(ws.can_rename(id));
}

#[test]
fn one_file_id_per_file() {
    let mut ws = workspace();
    let a = ws.add_file(Path::new("/w/app.orb"));
    assert_eq!(ws.add_file(Path::new("/w/./x/../app.orb")), a);
    let cwd = std::env::current_dir().unwrap();
    let b = ws.add_file(Path::new("rel.orb"));
    assert_eq!(ws.add_file(&cwd.join("./rel.orb")), b);
}

#[test]
fn lua_modules_are_any() {
    let mut ws = Workspace::new(
        Config {
            root: PathBuf::from("/w"),
            ..Config::default()
        },
        Box::new(Memory(vec![
            ("/w/old.lua", "return { f = function() end }"),
            (
                "/w/app.orb",
                "use old;
use old::{f, g};
old::anything();
f();
",
            ),
        ])),
    );
    let app = ws.add_file(Path::new("/w/app.orb"));
    assert!(
        findings(&mut ws, app).is_empty(),
        "{:?}",
        findings(&mut ws, app)
    );
}

#[test]
fn outline() {
    let mut ws = workspace();
    let lib = ws.add_file(Path::new("/w/lib.orb"));
    let outline = ws.document_symbols(lib);
    let names: Vec<_> = outline.iter().map(|d| d.name.as_str()).collect();
    assert_eq!(names, ["P", "helper"]);
    let members: Vec<_> = outline[0]
        .children
        .iter()
        .map(|d| (d.name.as_str(), d.kind))
        .collect();
    assert_eq!(
        members,
        [
            ("x", SymbolKind::Field),
            ("new", SymbolKind::AssocFn),
            ("get", SymbolKind::Method)
        ]
    );
}

#[test]
fn editing_a_dependency_rechecks_its_users() {
    let mut ws = workspace();
    let app = ws.add_file(Path::new("/w/app.orb"));
    assert!(
        findings(&mut ws, app).is_empty(),
        "{:?}",
        findings(&mut ws, app)
    );
    ws.set_source(
        Path::new("/w/lib.orb"),
        b"pub struct P { x: int }\n".to_vec(),
    );
    let codes: Vec<_> = ws
        .diagnostics(app)
        .iter()
        .map(|d| d.code.as_str())
        .collect();
    // `helper` is gone from the `use` list and from `lib::helper`; `P::new` too.
    assert_eq!(codes, ["T0103", "T0104", "T0103"]);
}

#[test]
fn syntax_error_keeps_the_rest_of_the_module() {
    let mut ws = workspace();
    let lib = ws.add_file(Path::new("/w/lib.orb"));
    assert_eq!(ws.document_symbols(lib).len(), 2);
    let text =
        "pub fn ok() -> int { 1 }\npub fn broken( {\npub fn after() {}\nlet n: string = ok();\n";
    ws.set_source(Path::new("/w/lib.orb"), text.as_bytes().to_vec());
    let codes: Vec<_> = ws
        .diagnostics(lib)
        .iter()
        .filter(|d| !is_unused_lint(d))
        .map(|d| d.code.as_str())
        .collect();
    // The broken item is reported, and the code after it is still checked.
    assert_eq!(codes, ["T0001", "T0201"]);
    let names: Vec<String> = ws
        .document_symbols(lib)
        .into_iter()
        .map(|s| s.name)
        .collect();
    assert_eq!(names, ["ok", "after"]);
}

#[test]
fn types_for_hover() {
    let src = "struct P { x: float }\n\
               fn get(p: P?) -> float { if p != nil { p.x } else { 0.0 } }\n\
               let rec = #{ a: 1, b: \"s\" };\n\
               let pair = |n: int| (n, tostring(n));\n\
               let mut acc = 0;\n\
               acc += 0.5;\n";
    let mut ws = Workspace::new(Config::default(), Box::new(Memory(Vec::new())));
    let f = ws.set_source(Path::new("/m/hover.orb"), src.as_bytes().to_vec());
    assert!(
        findings(&mut ws, f).is_empty(),
        "{:?}",
        findings(&mut ws, f)
    );
    let at = |ws: &mut Workspace, needle: &str, skip: usize| {
        let off = src.match_indices(needle).nth(skip).unwrap().0 as u32;
        ws.type_at(f, off)
    };
    assert_eq!(at(&mut ws, "get", 0).as_deref(), Some("fn(P?) -> float"));
    // `p` narrowed inside the `if`, not before.
    assert_eq!(at(&mut ws, "p != nil", 0).as_deref(), Some("P?"));
    assert_eq!(at(&mut ws, "p.x", 0).as_deref(), Some("P"));
    assert_eq!(at(&mut ws, "x }", 0).as_deref(), Some("float"));
    assert_eq!(
        at(&mut ws, "rec", 0).as_deref(),
        Some("#{ a: int, b: string }")
    );
    assert_eq!(
        at(&mut ws, "pair", 0).as_deref(),
        Some("fn(int) -> (int, string)")
    );
    assert_eq!(at(&mut ws, "acc", 0).as_deref(), Some("number"));
}

#[test]
fn editor_queries() {
    let src = "struct V { x: float, y: float }\n\
               impl V {\n    \
                   fn new(x: float, y: float) -> V { V { x, y } }\n    \
                   fn dot(self, o: V) -> float { self.x * o.x + self.y * o.y }\n    \
                   fn __add(a, b) { a }\n\
               }\n\
               let v = V::new(1.0, 2.0);\n\
               let d = v.dot(v);\n\
               let e = v.x;\n\
               let n = 3;\n\
               let m: int = 4;\n";
    let mut ws = Workspace::new(Config::default(), Box::new(Memory(Vec::new())));
    let f = ws.set_source(Path::new("/m/q.orb"), src.as_bytes().to_vec());
    assert!(
        findings(&mut ws, f).is_empty(),
        "{:?}",
        findings(&mut ws, f)
    );
    let off = |needle: &str| src.find(needle).unwrap() as u32;

    let labels =
        |c: orbit_check::Completions| c.items.into_iter().map(|c| c.label).collect::<Vec<_>>();
    // The partly typed name `x` is what a completion replaces.
    let x = off("x;");
    assert_eq!(
        ws.completions_at(f, x + 1).replace,
        orbit_check::Span {
            start: x,
            end: x + 1
        }
    );
    assert_eq!(labels(ws.completions_at(f, off("x;"))), ["x", "y", "dot"]);
    assert_eq!(
        labels(ws.completions_at(f, off("new(1"))),
        ["new", "dot", "__add"]
    );

    let sig = ws.signature_at(f, off("2.0)")).unwrap();
    assert_eq!(sig.label, "new(x: float, y: float) -> V");
    assert_eq!(sig.active, Some(1));
    let (s, e) = sig.params[1];
    assert_eq!(&sig.label[s as usize..e as usize], "y: float");
    let sig = ws.signature_at(f, off("v);")).unwrap();
    assert_eq!(sig.label, "dot(o: V) -> float");
    assert_eq!(sig.active, Some(0));

    let hints = ws.inlay_hints(
        f,
        orbit_check::Span {
            start: 0,
            end: src.len() as u32,
        },
    );
    let shown: Vec<String> = hints
        .iter()
        .map(|h| {
            let name = src[..h.offset as usize].rsplit(' ').next().unwrap();
            format!("{name}{}", h.label)
        })
        .collect();
    assert_eq!(shown, ["v: V", "d: float", "e: float", "n: int"]);
}

#[test]
fn scope_and_keyword_completion() {
    let src = "fn helper(a: int) -> int { a }\n\
               let count = 1;\n\
               fn run() {\n    \
                   let inner = 2;\n    \
                   let later_name = co;\n    \
                   let after = 3;\n\
               }\n";
    let mut ws = Workspace::new(Config::default(), Box::new(Memory(Vec::new())));
    let f = ws.set_source(Path::new("/m/scope.orb"), src.as_bytes().to_vec());
    let at = src.find("co;").unwrap() as u32 + 2;
    let c = ws.completions_at(f, at);
    assert_eq!(
        c.replace,
        orbit_check::Span {
            start: at - 2,
            end: at
        }
    );
    let labels: Vec<&str> = c.items.iter().map(|c| c.label.as_str()).collect();
    for want in [
        "inner",
        "count",
        "helper",
        "run",
        "coroutine",
        "print",
        "continue",
        "const",
    ] {
        assert!(labels.contains(&want), "{want} missing from {labels:?}");
    }
    // Not yet defined, out of scope, or not a name.
    for unwanted in ["after", "a", "later_name"] {
        assert!(!labels.contains(&unwanted), "{unwanted} in {labels:?}");
    }
    let print = c.items.iter().find(|c| c.label == "print").unwrap();
    assert_eq!(print.detail.as_deref(), Some("fn(...any)"));
    assert!(c.items.iter().find(|c| c.label == "const").unwrap().keyword);
}

#[test]
fn completion_after_a_bare_separator() {
    let src = "struct P { x: int }\n\
               let p = P { x: 1 };\n\
               let s = \"text\";\n\
               let a = p.\n\
               let b = s.\n\
               let c = math::\n";
    let mut ws = Workspace::new(Config::default(), Box::new(Memory(Vec::new())));
    let f = ws.set_source(Path::new("/m/bare.orb"), src.as_bytes().to_vec());
    let labels = |ws: &mut Workspace, needle: &str| {
        let at = (src.find(needle).unwrap() + needle.len()) as u32;
        let c = ws.completions_at(f, at);
        assert_eq!(c.replace.start, at);
        c.items.into_iter().map(|c| c.label).collect::<Vec<_>>()
    };
    assert_eq!(labels(&mut ws, "p."), ["x"]);
    let string = labels(&mut ws, "s.");
    assert!(string.contains(&"upper".to_string()), "{string:?}");
    let math = labels(&mut ws, "math::");
    assert!(
        math.contains(&"sqrt".to_string()) && math.contains(&"pi".to_string()),
        "{math:?}"
    );
}

#[test]
fn completion_of_missing_struct_fields() {
    let labels = |src: &str, needle: &str| {
        let mut ws = Workspace::new(Config::default(), Box::new(Memory(Vec::new())));
        let f = ws.set_source(Path::new("/m/lit.orb"), src.as_bytes().to_vec());
        let at = (src.find(needle).unwrap() + needle.len()) as u32;
        let c = ws.completions_at(f, at);
        c.items.into_iter().map(|c| c.label).collect::<Vec<_>>()
    };
    let decls = "struct V { x: float, y: float, z: float }\n\
                 enum Shape { Circle { r: float }, Rect { w: float, h: float } }\n";
    // After `{` or `,`: the fields not written yet, in declaration order.
    let src = decls.to_string() + "let a = V { x: 1.0, };\nlet b = V { };\n";
    assert_eq!(labels(&src, "x: 1.0, "), ["y", "z"]);
    assert_eq!(labels(&src, "b = V { "), ["x", "y", "z"]);
    // A name being typed (parsed as a shorthand field) still counts as missing.
    let src = decls.to_string() + "let a = V { y: 1.0, z };\n";
    assert_eq!(labels(&src, "y: 1.0, z"), ["x", "z"]);
    // Variants with fields, and a literal missing its `;`.
    let src = decls.to_string() + "let s = Shape::Rect { w: 1.0, }";
    assert_eq!(labels(&src, "w: 1.0, "), ["h"]);
    // Inside a value, completion is the usual names in scope.
    let src = decls.to_string() + "let n = 1.0;\nlet a = V { x: n, y: math::max(n, ) };\n";
    let inner = labels(&src, "max(n, ");
    assert!(inner.contains(&"n".to_string()), "{inner:?}");
}

/// The language server shows the checker's errors for `.orb` files, so an `as` target the
/// compiler rejects must get the same message from both.
#[test]
fn cast_errors_match_the_compiler() {
    let cases = [
        "let x = y as [int];",
        "let x = y as #{ a: int, b: string? };",
        "let x = y as #{[string]: int};",
        "let x = y as fn(int) -> string;",
        "let x = y as (int, string);",
        "let x = y as (int | [int]);",
        "let x = y as? [int]?;",
        "fn f<T>(y) { y as T }",
        "struct S<T> { v: T } impl<T> S<T> { fn get(self) { self.v as T } }",
        "struct S<T> { v: T } let x = y as S<int>;",
        "type Ids = [int]; let x = y as Ids;",
        "fn f(y) { y as Id } type Id = int;",
        "let x = y as table;",
        "let x = y as function?;",
        "let x = y as (int | thread);",
        "let x = y as userdata;",
    ];
    for src in cases {
        let src = format!("extern y; {src}");
        let compiler = orbit_orb_syntax::to_hir("t", src.as_bytes(), &Default::default())
            .expect_err("the compiler rejects the cast")
            .message;
        let mut ws = Workspace::new(
            Config {
                root: PathBuf::from("/w"),
                ..Config::default()
            },
            Box::new(Memory(vec![])),
        );
        let file = ws.set_source(Path::new("/w/t.orb"), src.clone().into_bytes());
        let checker: Vec<_> = ws
            .diagnostics(file)
            .iter()
            .filter(|d| !is_unused_lint(d))
            .map(|d| (d.code.as_str(), d.message.clone()))
            .collect();
        assert_eq!(checker, [("T0210", compiler)], "{src}");
    }
}

/// What one module reports as `(line, code, message)`, checked alone.
fn diagnostics_of(src: &str, strict: bool) -> Vec<(u32, &'static str, String)> {
    let mut ws = Workspace::new(
        Config {
            root: PathBuf::from("/p"),
            strict,
            ..Config::default()
        },
        Box::new(Memory(vec![])),
    );
    let f = ws.set_source(Path::new("/p/m.orb"), src.as_bytes().to_vec());
    let diags = ws.diagnostics(f).to_vec();
    diags
        .iter()
        .map(|d| {
            (
                ws.line_col(f, d.span.start).0,
                d.code.as_str(),
                d.message.clone(),
            )
        })
        .collect()
}

#[test]
fn pragmas() {
    let codes = |src: &str, strict: bool| -> Vec<(u32, &'static str)> {
        diagnostics_of(src, strict)
            .into_iter()
            .map(|(l, c, _)| (l, c))
            .collect()
    };
    // An `ignore` for another code leaves the error; one for nothing is reported in strict mode.
    let src =
        "let _a: string = 3; // orbit-check: ignore[T0207]\nlet _b = 1; // orbit-check: ignore\n";
    assert_eq!(codes(src, false), [(1, "T0201")]);
    assert_eq!(codes(src, true), [(1, "T0201"), (1, "T0305"), (2, "T0305")]);
    // Several codes; a syntax error is never ignored.
    let src = "let _a: string = nil; // orbit-check: ignore[T0201, T0207]\nlet = ; // orbit-check: ignore\n";
    assert_eq!(codes(src, false), [(2, "T0001")]);
    // `Config::strict` checks every module strictly; `//! orbit-check: off` still wins.
    assert_eq!(codes("pub fn f(x) {}\n", true), [(1, "T0501")]);
    assert_eq!(codes("//! orbit-check: off\npub fn f(x) {}\n", true), []);
    // Pragmas it doesn't understand.
    let bad = diagnostics_of(
        "// orbit-check: strict\nlet _a = 1; // orbit-check: ignore[T9999]\n// orbit-check: igonre\n",
        false,
    );
    let shown: Vec<_> = bad.iter().map(|(l, c, m)| format!("{l} {c} {m}")).collect();
    assert_eq!(
        shown,
        [
            "1 T0306 `strict` applies to a whole module: write `//! orbit-check: strict`",
            "2 T0306 `T9999` is not a diagnostic code",
            "3 T0306 `orbit-check: igonre` is not a pragma; a line takes `// orbit-check: ignore` or `ignore[T0201]`, a module `//! orbit-check: strict` or `off`",
        ]
    );
}

#[test]
fn strict_messages() {
    let src = "pub fn add(a: int, b, ...rest) { a + b }\nfn c(n: int?) { if n { } }\n";
    let shown: Vec<_> = diagnostics_of(src, true)
        .into_iter()
        .map(|(l, c, m)| format!("{l} {c} {m}"))
        .collect();
    assert_eq!(
        shown,
        [
            "1 T0502 public function `add` returns `(any)` but has no result type (strict mode)",
            "1 T0501 parameter `b` of public function `add` needs a type (strict mode)",
            "1 T0501 parameter `...rest` of public function `add` needs a type (strict mode)",
            "2 T0503 this condition has type `int?`, not `bool` (strict mode); compare it, such as `x != nil`",
        ]
    );
}

/// The language server keeps one workspace for a whole editing session. Each edit that shifts a
/// module's symbols makes new types for everything naming them; the store must not keep them all.
#[test]
fn long_editing_sessions_keep_the_type_store_bounded() {
    let mut body = String::new();
    for i in 0..40 {
        body += &format!(
            "struct S{i}<T> {{ a: int, b: T }}\n\
             impl<T> S{i}<T> {{\n    fn get(self) -> T {{ self.b }}\n    \
             fn pair(self, o: S{i}<int>) -> (int, T) {{ (o.a, self.b) }}\n}}\n\
             let v{i} = S{i} {{ a: 1, b: \"x\" }};\nlet w{i}: int = v{i}.get();\n"
        );
    }
    let geo = "pub struct Vec2 { x: float, y: float }\n\
               impl Vec2 { pub fn new(x: float, y: float) -> Vec2 { Vec2 { x, y } } }\n";
    let main = |lines: usize| {
        let top: String = (0..lines).map(|i| format!("let n{i} = {i};\n")).collect();
        format!("use geo::{{Vec2}};\n{top}let p = Vec2::new(1.0, 2.0);\n{body}")
    };
    let config = || Config {
        root: PathBuf::from("/w"),
        ..Config::default()
    };
    let mut ws = Workspace::new(config(), Box::new(Memory(Vec::new())));
    ws.set_source(Path::new("/w/geo.orb"), geo.as_bytes().to_vec());
    let mut most = 0;
    let mut grew_past_first_edit = false;
    let mut first = None;
    for lines in 0..250 {
        let f = ws.set_source(Path::new("/w/main.orb"), main(lines).into_bytes());
        ws.check_all();
        ws.type_at(f, 30);
        let n = ws.type_count();
        most = most.max(n);
        let first = *first.get_or_insert(n);
        grew_past_first_edit |= n > 4 * first;
    }
    // Without rebuilding, these edits leave about 30000 types.
    assert!(
        grew_past_first_edit,
        "the edits should make garbage to drop"
    );
    assert!(most < 20_000, "the type store reached {most} types");
    // What the workspace answers after rebuilds matches a fresh one.
    let text = main(249);
    let mut fresh = Workspace::new(config(), Box::new(Memory(Vec::new())));
    fresh.set_source(Path::new("/w/geo.orb"), geo.as_bytes().to_vec());
    let answers = |ws: &mut Workspace| {
        let f = ws.set_source(Path::new("/w/main.orb"), text.clone().into_bytes());
        let diags: Vec<String> = ws
            .diagnostics(f)
            .iter()
            .map(|d| d.message.clone())
            .collect();
        let types: Vec<Option<String>> = ["p = ", "v39 = ", "w12"]
            .iter()
            .map(|n| ws.type_at(f, text.find(n).unwrap() as u32))
            .collect();
        let hints = ws
            .inlay_hints(
                f,
                orbit_check::Span {
                    start: 0,
                    end: text.len() as u32,
                },
            )
            .len();
        (diags, types, hints)
    };
    assert_eq!(answers(&mut ws), answers(&mut fresh));
}
