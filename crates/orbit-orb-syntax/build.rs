//! Writes the list of prelude names (`PRELUDE` in `src/lower/mod.rs`) from the checker's
//! prelude stub, `orbit-check/stubs/prelude.orbi`, so the names a module can read without
//! declaring them are written down once: what the stub declares is what lowering accepts.

use std::fmt::Write as _;
use std::path::Path;

/// The public items a stub declares at its top level and the modules it `use`s, in the order
/// written, each once (an overloaded function has a line per signature).
fn names(stub: &str) -> Vec<&str> {
    let mut out = Vec::new();
    for line in stub.lines() {
        let rest = ["pub fn ", "pub const ", "use "]
            .iter()
            .find_map(|prefix| line.strip_prefix(prefix));
        let Some(rest) = rest else { continue };
        let rest = rest.strip_prefix("r#").unwrap_or(rest);
        let end = rest
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .unwrap_or(rest.len());
        let name = &rest[..end];
        assert!(!name.is_empty(), "no name in `{line}`");
        if !out.contains(&name) {
            out.push(name);
        }
    }
    out
}

fn main() {
    let manifest = std::env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    let stub = Path::new(&manifest).join("../orbit-check/stubs/prelude.orbi");
    println!("cargo:rerun-if-changed={}", stub.display());
    println!("cargo:rerun-if-changed=build.rs");
    let text = std::fs::read_to_string(&stub)
        .unwrap_or_else(|e| panic!("reading {}: {e}", stub.display()));
    let names = names(&text);
    assert!(
        names.contains(&"print"),
        "{} lists no `print`",
        stub.display()
    );

    let mut code = String::from("&[\n");
    for name in names {
        writeln!(code, "    b\"{name}\",").expect("writing to a string");
    }
    code.push_str("]\n");
    let out = std::env::var_os("OUT_DIR").expect("OUT_DIR");
    std::fs::write(Path::new(&out).join("prelude.rs"), code).expect("writing prelude.rs");
}
