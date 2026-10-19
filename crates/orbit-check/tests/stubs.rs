//! The bundled stubs name exactly what the runtime provides: every global and library field the
//! runtime installs has a stub, and every stub names something real. The prelude also matches
//! the front end's own list of names that need no declaration.

use std::collections::BTreeSet;
use std::path::Path;

use orbit::{Chunk, Config, Runtime};

fn read(rel: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(rel);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// The public items a stub declares at its top level, and the modules it `use`s.
fn stub_names(text: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for line in text.lines() {
        let rest = if let Some(r) = line.strip_prefix("pub fn ") {
            r
        } else if let Some(r) = line.strip_prefix("pub const ") {
            r
        } else if let Some(r) = line.strip_prefix("use ") {
            r
        } else {
            continue;
        };
        let rest = rest.strip_prefix("r#").unwrap_or(rest);
        let end = rest
            .find(|c: char| !(c.is_alphanumeric() || c == '_'))
            .unwrap_or(rest.len());
        out.insert(rest[..end].to_string());
    }
    out
}

/// The methods a stub declares inside `impl` blocks.
fn stub_methods(text: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for line in text.lines() {
        if let Some(r) = line.trim_start().strip_prefix("pub fn ")
            && line.starts_with("    ")
        {
            let end = r.find('(').unwrap_or(r.len());
            out.insert(r[..end].to_string());
        }
    }
    out
}

/// The byte-string names listed in a Rust `&[...]` constant that starts at `start`.
fn listed(source: &str, start: &str) -> BTreeSet<String> {
    let from = source.find(start).expect("the list") + start.len();
    let list = &source[from..from + source[from..].find("];").expect("the list's end")];
    list.split("b\"")
        .skip(1)
        .map(|s| s[..s.find('"').expect("a closing quote")].to_string())
        .collect()
}

/// `(global, its fields)` for everything the runtime installs, and the methods of files.
fn runtime() -> (BTreeSet<String>, BTreeSet<String>) {
    let mut iso = Runtime::new().new_isolate(Config::default());
    orbit_std::install(&mut iso, &orbit_std::Options::default()).unwrap();
    let src = r#"
        local names = {}
        for k, v in pairs(_G) do
            names[#names + 1] = k
            if type(v) == "table" and k ~= "_G" then
                for f in pairs(v) do names[#names + 1] = k .. "." .. f end
            end
        end
        local methods = {}
        for m in pairs(getmetatable(io.stdout).__index) do methods[#methods + 1] = m end
        return table.concat(names, " "), table.concat(methods, " ")
    "#;
    let out: Vec<String> = iso
        .scope(|s| {
            let values = s.load(Chunk::new(src).name("=stubs"))?.call(())?;
            values
                .iter()
                .map(|v| Ok(String::from_utf8_lossy(&v.to_display()?).into_owned()))
                .collect::<Result<Vec<_>, orbit::Error>>()
        })
        .unwrap();
    let split = |s: &str| s.split_whitespace().map(String::from).collect();
    (split(&out[0]), split(&out[1]))
}

#[test]
fn stubs_match_the_runtime() {
    let (names, file_methods) = runtime();
    let mut expected = BTreeSet::new();
    let mut stubbed = BTreeSet::new();
    // Globals the CLI or a feature adds, which `orbit_std::install` alone doesn't.
    let host_only = ["arg", "ffi"];
    for n in &names {
        expected.insert(n.clone());
    }
    for n in stub_names(&read("stubs/prelude.orbi")) {
        if !host_only.contains(&n.as_str()) && n != "panic" {
            stubbed.insert(n.clone());
        }
        if host_only.contains(&n.as_str()) || n == "_G" {
            continue;
        }
        let path = format!("stubs/{n}.orbi");
        if Path::new(env!("CARGO_MANIFEST_DIR")).join(&path).exists() {
            for f in stub_names(&read(&path)) {
                stubbed.insert(format!("{n}.{f}"));
            }
        }
    }
    assert_eq!(
        expected.difference(&stubbed).collect::<Vec<_>>(),
        Vec::<&String>::new(),
        "in the runtime but not the stubs"
    );
    assert_eq!(
        stubbed.difference(&expected).collect::<Vec<_>>(),
        Vec::<&String>::new(),
        "in the stubs but not the runtime"
    );

    let io = read("stubs/io.orbi");
    assert_eq!(stub_methods(&io), file_methods, "file methods");

    // The `ffi` library, which this test's runtime doesn't install, lists its functions.
    let ffi = listed(
        &read("../orbit-ffi/src/lib.rs"),
        "const FUNCTIONS: &[(&[u8], Push)] = &[",
    );
    assert_eq!(stub_names(&read("stubs/ffi.orbi")), ffi, "ffi functions");
}

/// The prelude stub names what the front end lets a module read without declaring it.
#[test]
fn prelude_matches_the_front_end() {
    let front_end = listed(
        &read("../orbit-orb-syntax/src/lower/mod.rs"),
        "const PRELUDE: &[&[u8]] = &[",
    );
    assert_eq!(stub_names(&read("stubs/prelude.orbi")), front_end);
}
