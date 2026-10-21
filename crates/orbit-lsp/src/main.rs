//! `orbit-lsp`: the language server for OrbitScript and Lua (ADR-12, D2 and
//! D9), speaking LSP over standard input and output.
//!
//! The protocol is handled by OrbitScript scripts bundled in the binary
//! (`scripts/`): framing, JSON, the open documents, positions and turning
//! answers into LSP's shapes. They run in an isolate on the public `orbit`
//! API with `orbit-std` for `io` and `std.json`, and ask the `analysis`
//! module ([`analysis`]) what the checker (`orbit-check`) knows.

mod analysis;

use std::process::ExitCode;

use orbit::{Chunk, Config, Error, Language, Runtime, Table};

/// The bundled scripts: the module each is `require`d as, and its text. The
/// first runs the server.
const SCRIPTS: &[(&str, &str)] = &[
    ("lsp.server", include_str!("../scripts/lsp/server.orb")),
    ("lsp.rpc", include_str!("../scripts/lsp/rpc.orb")),
    ("lsp.text", include_str!("../scripts/lsp/text.orb")),
    (
        "lsp.documents",
        include_str!("../scripts/lsp/documents.orb"),
    ),
    ("lsp.features", include_str!("../scripts/lsp/features.orb")),
];

fn main() -> ExitCode {
    if let Some(arg) = std::env::args().nth(1) {
        match arg.as_str() {
            "--version" => {
                println!("orbit-lsp {}", env!("CARGO_PKG_VERSION"));
                return ExitCode::SUCCESS;
            }
            // Editors pass it; standard input and output are the only way.
            "--stdio" => {}
            _ => {
                eprintln!("usage: orbit-lsp [--stdio | --version]");
                return ExitCode::from(2);
            }
        }
    }
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("orbit-lsp: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Set up the isolate and run the server, which ends the process with
/// `os.exit` when the editor says `exit`.
fn run() -> Result<(), Error> {
    let mut iso = Runtime::new().new_isolate(Config::default());
    let options = orbit_std::Options {
        json: true,
        ..Default::default()
    };
    orbit_std::install(&mut iso, &options)?;
    analysis::install(&mut iso)?;
    iso.scope(|s| {
        // Each script is `require`d (`use lsp::rpc;`) from the preload table.
        let package: Table<'_> = s.globals().get("package")?;
        let preload: Table<'_> = package.get("preload")?;
        for &(name, text) in SCRIPTS {
            let file = name.trim_start_matches("lsp.");
            let chunk = Chunk::new(text)
                .name(format!("@<orbit-lsp>/{file}.orb"))
                .language(Language::OrbitScript);
            preload.set(name, s.load(chunk)?)?;
        }
        let server = preload.get::<orbit::Function<'_>>(SCRIPTS[0].0)?;
        server.call(()).map(drop)
    })
}
