//! The Rust side of the differential tests against a reference `lua5.5`; see
//! `difftest/README.md`.
//!
//! ```text
//! difftest messages CORPUS       per chunk: OK or the message load(chunk, "=t") gives
//! difftest ctc CORPUS NAME       per chunk: whether local NAME is a compile-time constant
//! difftest print OUT_DIR FILE... print each file's AST as OUT_DIR/STEM.{plain,renamed}.lua
//! ```
//!
//! A corpus is a sequence of chunks, each a little-endian u32 length and the bytes.

use std::io::Write;
use std::path::Path;

use orbit_lua_syntax::ast::LocalKind;
use orbit_lua_syntax::printer::{PrintOptions, print_chunk};
use orbit_lua_syntax::{ParseOptions, parse_with, skip_file_prefix};

fn corpus(path: &str) -> Vec<Vec<u8>> {
    let data = std::fs::read(path).expect("readable corpus");
    let mut chunks = Vec::new();
    let mut pos = 0;
    while pos + 4 <= data.len() {
        let len = u32::from_le_bytes(data[pos..pos + 4].try_into().unwrap()) as usize;
        chunks.push(data[pos + 4..pos + 4 + len].to_vec());
        pos += 4 + len;
    }
    chunks
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    // `lua5.5` running `load` from a script's main chunk has 2 of its 200 levels in use.
    let options = ParseOptions {
        max_depth: 198,
        ..ParseOptions::default()
    };
    let mut out = std::io::BufWriter::new(std::io::stdout().lock());
    match args.first().map(String::as_str) {
        Some("messages") => {
            for chunk in corpus(&args[1]) {
                // Binary chunks are the loader's business, not the parser's.
                let line = if chunk.first() == Some(&0x1b) {
                    b"binary chunk".to_vec()
                } else {
                    match parse_with(&chunk, &options) {
                        Ok(_) => b"OK".to_vec(),
                        Err(e) => e.to_lua_message(b"=t"),
                    }
                };
                // Messages can hold any byte; escape newlines to keep one per line.
                for &c in &line {
                    if c == b'\n' {
                        out.write_all(b"\\n").unwrap();
                    } else {
                        out.write_all(&[c]).unwrap();
                    }
                }
                out.write_all(b"\n").unwrap();
            }
        }
        Some("ctc") => {
            let name = &args[2];
            for chunk in corpus(&args[1]) {
                let line = match parse_with(&chunk, &options) {
                    Err(_) => "ERR",
                    Ok(c) => match c.locals.iter().find(|l| &*l.name == name) {
                        Some(l) if matches!(l.kind, LocalKind::CompileTimeConst(_)) => "ctc",
                        Some(_) => "reg",
                        None => "missing",
                    },
                };
                writeln!(out, "{line}").unwrap();
            }
        }
        Some("print") => {
            for path in &args[2..] {
                let src = std::fs::read(path).expect("readable file");
                let chunk = match parse_with(skip_file_prefix(&src), &options) {
                    Ok(c) => c,
                    Err(e) => {
                        eprintln!("{path}: {e}");
                        continue;
                    }
                };
                let stem = Path::new(path).file_stem().unwrap().to_string_lossy();
                for (suffix, rename_locals) in [("plain", false), ("renamed", true)] {
                    let text = print_chunk(&chunk, &PrintOptions { rename_locals });
                    let file = Path::new(&args[1]).join(format!("{stem}.{suffix}.lua"));
                    std::fs::write(file, text).expect("writable OUT_DIR");
                }
            }
        }
        _ => {
            eprintln!("usage: difftest messages CORPUS | ctc CORPUS NAME | print OUT_DIR FILE...");
            std::process::exit(2);
        }
    }
}
