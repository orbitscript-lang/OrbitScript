//! Compiles the built-in OrbitScript modules (`src/stdlib/builtins/*.orb`)
//! to binary chunks in `OUT_DIR`, which `stdlib::builtins` embeds (ADR-11).
//! The compiler is a build dependency only: the runtime carries the
//! bytecode, not the compiler.

use std::path::Path;

const MODULES: &[&str] = &["base", "table"];

fn main() {
    let out = std::env::var_os("OUT_DIR").expect("cargo sets OUT_DIR");
    for name in MODULES {
        let src_path = format!("src/stdlib/builtins/{name}.orb");
        println!("cargo:rerun-if-changed={src_path}");
        let src = std::fs::read(&src_path).unwrap_or_else(|e| panic!("{src_path}: {e}"));
        let chunk_name = format!("{name}.orb");
        let hir = orbit_orb_syntax::to_hir(&chunk_name, &src, &Default::default())
            .unwrap_or_else(|d| panic!("{}", d.to_message(&chunk_name)));
        let chunk = orbit_compiler::compile(&hir).unwrap_or_else(|e| panic!("{chunk_name}: {e}"));
        // Built-ins show as C functions: no line or local information.
        let bytes = orbit_bytecode::format::encode(&chunk, true);
        std::fs::write(Path::new(&out).join(format!("{name}.orbc")), bytes)
            .expect("writing to OUT_DIR");
    }
}
