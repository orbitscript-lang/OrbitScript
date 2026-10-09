//! Compiles the first-light programs and benchmarks through the real front end and checks
//! that the bytecode is well formed: every register, cell, capture, constant, cache slot,
//! child and jump target in range.

use std::path::Path;

use orbit_bytecode::{CaptureSource, MULTI, Op, UnlinkedChunk};
use orbit_compiler::{compile, disassemble};

fn check(chunk: &UnlinkedChunk) -> Result<(), String> {
    for (i, p) in chunk.protos.iter().enumerate() {
        let err =
            |pc: usize, what: &str| Err(format!("proto {i} pc {pc}: {what}: {:?}", p.code.get(pc)));
        let nregs = p.nregs as usize;
        let reg = |r: u8| (r as usize) < nregs;
        let regs = |r: u8, n: u8| n == MULTI || r as usize + n as usize <= nregs;
        for (pc, op) in p.code.iter().enumerate() {
            let target = |off: i32| {
                let t = pc as i64 + 1 + off as i64;
                t >= 0 && (t as usize) < p.code.len()
            };
            let ok = match *op {
                Op::Move { dst, src } => reg(dst) && reg(src),
                Op::LoadNil { dst, count } => regs(dst, count),
                Op::LoadBool { dst, .. } | Op::LoadInt { dst, .. } => reg(dst),
                Op::LoadConst { dst, k } => reg(dst) && (k as usize) < p.consts.len(),
                Op::LoadConstX { dst, k } => reg(dst) && (k as usize) < p.consts.len(),
                Op::CellNew { cell, src } | Op::CellSet { cell, src } => {
                    cell < p.ncells && reg(src)
                }
                Op::CellGet { dst, cell } => cell < p.ncells && reg(dst),
                Op::CapGet { dst, cap } => (cap as usize) < p.captures.len() && reg(dst),
                Op::CapSelf { dst } => reg(dst),
                Op::CapSet { cap, src } => (cap as usize) < p.captures.len() && reg(src),
                Op::GetGlobal { dst: r, name, ic } | Op::SetGlobal { src: r, name, ic } => {
                    reg(r)
                        && (name as usize) < p.consts.len()
                        && ic < p.ncaches
                        && !p.captures.is_empty()
                }
                Op::CheckUndefined { src, name } => reg(src) && (name as usize) < p.consts.len(),
                Op::NewTable { dst, .. } => reg(dst),
                Op::VarargTable { dst } => p.is_vararg && reg(dst),
                Op::VarargGet { dst, key } => p.is_vararg && reg(dst) && reg(key),
                Op::VarargUnpack { dst, table, count } => {
                    p.is_vararg && reg(table) && regs(dst, count)
                }
                Op::GetField { dst, obj, key, ic } => {
                    reg(dst) && reg(obj) && (key as usize) < p.consts.len() && ic < p.ncaches
                }
                Op::SetField { obj, key, src, ic } => {
                    reg(obj) && reg(src) && (key as usize) < p.consts.len() && ic < p.ncaches
                }
                Op::GetIndex { dst, obj, key } => reg(dst) && reg(obj) && reg(key),
                Op::SetIndex { obj, key, src } => reg(obj) && reg(key) && reg(src),
                Op::GetIndexInt { dst, obj, .. } => reg(dst) && reg(obj),
                Op::SetIndexInt { obj, src, .. } => reg(obj) && reg(src),
                Op::SetList {
                    table,
                    first,
                    count,
                    ..
                } => reg(table) && regs(first, count),
                Op::SelfCall { dst, obj, key, .. } => {
                    regs(dst, 2) && reg(obj) && (key as usize) < p.consts.len()
                }
                Op::Add { dst, a, b }
                | Op::Sub { dst, a, b }
                | Op::Mul { dst, a, b }
                | Op::Div { dst, a, b }
                | Op::IDiv { dst, a, b }
                | Op::Mod { dst, a, b }
                | Op::Pow { dst, a, b }
                | Op::BAnd { dst, a, b }
                | Op::BOr { dst, a, b }
                | Op::BXor { dst, a, b }
                | Op::Shl { dst, a, b }
                | Op::Shr { dst, a, b } => reg(dst) && reg(a) && reg(b),
                Op::AddInt { dst, a, .. } | Op::SubInt { dst, a, .. } => reg(dst) && reg(a),
                Op::Neg { dst, src }
                | Op::BNot { dst, src }
                | Op::Not { dst, src }
                | Op::Len { dst, src } => reg(dst) && reg(src),
                Op::Concat { dst, first, count } => reg(dst) && count >= 2 && regs(first, count),
                Op::Jump { off } => target(off),
                Op::JumpIfTrue { src, off } | Op::JumpIfFalse { src, off } => {
                    reg(src) && target(off)
                }
                Op::JumpLt { a, b, off }
                | Op::JumpNotLt { a, b, off }
                | Op::JumpLe { a, b, off }
                | Op::JumpNotLe { a, b, off }
                | Op::JumpEq { a, b, off }
                | Op::JumpNe { a, b, off } => reg(a) && reg(b) && target(off),
                Op::JumpEqInt { a, off, .. }
                | Op::JumpLtInt { a, off, .. }
                | Op::JumpLeInt { a, off, .. }
                | Op::JumpGtInt { a, off, .. }
                | Op::JumpGeInt { a, off, .. } => reg(a) && target(off),
                Op::Call {
                    func,
                    nargs,
                    nresults,
                } => {
                    reg(func)
                        && regs(func + 1, nargs)
                        && (nresults == MULTI || func as usize + nresults as usize <= nregs)
                }
                Op::TailCall { func, nargs } => reg(func) && regs(func + 1, nargs),
                Op::Return { first, count } => regs(first, count),
                Op::Return0 => true,
                Op::Return1 { src } => reg(src),
                Op::Vararg { dst, count } => p.is_vararg && (count == MULTI || regs(dst, count)),
                Op::ForPrep { base, exit } => regs(base, 4) && target(exit),
                Op::ForLoop { base, back } => regs(base, 4) && target(back),
                Op::GenericCall { base, nvars } => {
                    base as usize + 4 + (nvars as usize).max(3) <= nregs
                }
                Op::GenericLoop { base, back } => regs(base, 5) && target(back),
                Op::Closure { dst, proto } => reg(dst) && (proto as usize) < p.children.len(),
                Op::Tbc { reg: r } => reg(r),
                Op::Close { from } => reg(from),
                // Only the VM's native-closure stubs hold it.
                Op::NativeEntry { .. } => false,
            };
            if !ok {
                return err(pc, "operand out of range");
            }
        }
        match p.code.last() {
            Some(Op::Return0 | Op::Return1 { .. } | Op::Return { .. } | Op::TailCall { .. }) => {}
            _ => {
                return err(
                    p.code.len().saturating_sub(1),
                    "code doesn't end in a return",
                );
            }
        }
        for &c in &p.children {
            let child = chunk
                .protos
                .get(c as usize)
                .ok_or(format!("proto {i}: child {c} missing"))?;
            if c as usize <= i {
                return Err(format!("proto {i}: child {c} comes before its parent"));
            }
            for s in &child.captures {
                let ok = match *s {
                    CaptureSource::Cell(cell) => cell < p.ncells,
                    CaptureSource::Capture(u) => (u as usize) < p.captures.len(),
                    CaptureSource::Env => false,
                };
                if !ok {
                    return Err(format!(
                        "proto {c}: capture source {s:?} invalid in parent {i}"
                    ));
                }
            }
        }
        if i == 0 && p.captures.iter().any(|&s| s != CaptureSource::Env) {
            return Err("main captures something other than _ENV".into());
        }
        if p.debug.capture_names.len() != p.captures.len() {
            return Err(format!("proto {i}: capture names don't match captures"));
        }
    }
    Ok(())
}

fn compile_file(path: &Path) -> UnlinkedChunk {
    let src = std::fs::read(path).unwrap();
    let name = path.file_name().unwrap().to_str().unwrap();
    let hir = orbit_lua_syntax::to_hir(name, &src, &Default::default())
        .unwrap_or_else(|e| panic!("{name}: front end: {e:?}"));
    let out = compile(&hir).unwrap_or_else(|e| panic!("{name}: {e}"));
    if let Err(e) = check(&out) {
        panic!("{name}: {e}\n{}", disassemble(&out));
    }
    out
}

#[test]
fn first_light_programs_compile_to_well_formed_bytecode() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut files: Vec<_> = std::fs::read_dir(root.join("tests/first-light"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "lua"))
        .collect();
    files.push(root.join("bench/fib.lua"));
    files.push(root.join("bench/binary_trees.lua"));
    assert!(files.len() >= 8);
    for f in files {
        compile_file(&f);
    }
}
