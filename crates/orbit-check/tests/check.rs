//! Checks the `tests/cases` programs against the codes marked in them, the repository's
//! OrbitScript programs for no errors, and the language server's name queries.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use orbit_check::{Config, DiskSources, Sources, SymbolKind, Workspace};

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
        let (_, found) = check_dir(&dir);
        assert_eq!(found, expected, "diagnostics in {}", dir.display());
    }
}

/// Every program the front end runs must resolve cleanly, since lowering accepts them, and
/// type check with only the findings listed here.
#[test]
fn repository_programs_check() {
    // `rounds` comes from the command line and may be 0, which leaves `dist` nil.
    let known: BTreeSet<_> = [("dijkstra.orb", 97, "T0207")]
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
    let (_, found) = check_dir(&root().join("crates/orbit-runtime/src/stdlib/builtins"));
    // The built-ins take their intrinsics from a host module that isn't a file.
    assert!(
        found.iter().all(|(_, _, code)| code == "T0002"),
        "built-ins: {found:?}"
    );
}

/// In-memory sources, as the language server's open buffers would be.
struct Memory(Vec<(&'static str, &'static str)>);

impl Sources for Memory {
    fn read(&self, path: &Path) -> Option<Vec<u8>> {
        let p = path.to_string_lossy();
        self.0
            .iter()
            .find(|(name, _)| *name == p)
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
    assert!(ws.diagnostics(app).is_empty(), "{:?}", ws.diagnostics(app));
    // `helper` on line 4 of app.orb refers to lib's `helper`.
    let at = APP.find("helper();").unwrap() as u32;
    let id = ws.symbol_at(app, at).expect("a symbol");
    let def = ws.definition(id).unwrap();
    assert_eq!(ws.path(def.file), Path::new("/w/lib.orb"));
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
    assert!(ws.diagnostics(app).is_empty(), "{:?}", ws.diagnostics(app));
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
    assert!(ws.diagnostics(app).is_empty());
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
    assert!(ws.diagnostics(f).is_empty(), "{:?}", ws.diagnostics(f));
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
    assert!(ws.diagnostics(f).is_empty(), "{:?}", ws.diagnostics(f));
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
