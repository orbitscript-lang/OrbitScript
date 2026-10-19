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

/// Every program the front end runs must resolve cleanly, since lowering accepts them.
#[test]
fn repository_programs_resolve() {
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
        assert!(found.is_empty(), "{}: {found:?}", dir.display());
    }
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
fn syntax_error_keeps_previous_symbols() {
    let mut ws = workspace();
    let lib = ws.add_file(Path::new("/w/lib.orb"));
    assert_eq!(ws.document_symbols(lib).len(), 2);
    ws.set_source(Path::new("/w/lib.orb"), b"pub fn broken( {".to_vec());
    let codes: Vec<_> = ws
        .diagnostics(lib)
        .iter()
        .map(|d| d.code.as_str())
        .collect();
    assert_eq!(codes, ["T0001"]);
    assert_eq!(ws.document_symbols(lib).len(), 2);
}
