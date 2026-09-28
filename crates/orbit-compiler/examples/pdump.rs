//! Dumps a compiled chunk in a plain line format, for comparing the code generator with
//! the reference compiler instruction by instruction.
//!
//! Usage: `cargo run -p orbit-compiler --example pdump -- file.lua`

use orbit_compiler::{CompiledFn, Constant};

fn quote(s: &[u8]) -> String {
    let mut out = String::from("\"");
    for &c in s {
        match c {
            b'"' | b'\\' => {
                out.push('\\');
                out.push(c as char);
            }
            32..=126 => out.push(c as char),
            _ => out.push_str(&format!("\\x{c:02x}")),
        }
    }
    out.push('"');
    out
}

fn dump(f: &CompiledFn) {
    let i = &f.info;
    println!(
        "function {} {} params={} flag={} stack={}",
        i.line_defined, i.last_line_defined, i.num_params, i.flags, i.max_stack
    );
    for u in i.upvalues.iter() {
        println!(
            "upval {} {} {} {}",
            quote(u.name.as_bytes()),
            u.in_stack as u8,
            u.index,
            u.kind
        );
    }
    for k in &f.constants {
        match k {
            Constant::Nil => println!("k nil"),
            Constant::Bool(b) => println!("k {b}"),
            Constant::Int(n) => println!("k int {n}"),
            Constant::Float(x) => println!("k flt {:016x}", x.to_bits()),
            Constant::Str(s) => println!("k str {}", quote(s)),
        }
    }
    for (pc, (&code, &line)) in i.code.iter().zip(i.lines.iter()).enumerate() {
        println!("code {pc} {code:08x} {line}");
    }
    for l in i.locvars.iter() {
        println!(
            "local {} {} {}",
            quote(l.name.as_bytes()),
            l.start_pc,
            l.end_pc
        );
    }
    for p in &f.protos {
        dump(p);
    }
    println!("end");
}

fn main() {
    let path = std::env::args().nth(1).expect("usage: pdump file.lua");
    let mut src = std::fs::read(&path).expect("read file");
    if src.first() == Some(&b'#') {
        let end = src.iter().position(|&c| c == b'\n').unwrap_or(src.len());
        src.drain(..end);
    }
    match orbit_lua_syntax::parse(&src) {
        Ok(chunk) => match orbit_compiler::compile(&chunk) {
            Ok(f) => dump(&f),
            Err(e) => println!("error {e}"),
        },
        Err(e) => println!("error {e}"),
    }
}
