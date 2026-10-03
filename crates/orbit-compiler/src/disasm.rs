//! A text listing of compiled bytecode, for golden tests and `--dump-bytecode`.

use std::fmt::Write;

use orbit_bytecode::{Constant, FunctionName, LocalSlot, Op, Origin, UnlinkedChunk};

fn text(bytes: &[u8]) -> String {
    format!("{:?}", String::from_utf8_lossy(bytes))
}

fn origin(o: &Origin) -> String {
    let (kind, name) = match o {
        Origin::Local(n) => ("local", n),
        Origin::Global(n) => ("global", n),
        Origin::Field(n) => ("field", n),
        Origin::Upvalue(n) => ("upvalue", n),
        Origin::Method(n) => ("method", n),
        Origin::Constant(n) => ("constant", n),
    };
    format!("{kind} {}", text(name))
}

/// Renders every prototype: header, constants, instructions with lines, jump targets and
/// operand origins, locals and captures. The format is stable for golden tests.
pub fn disassemble(chunk: &UnlinkedChunk) -> String {
    let mut out = String::new();
    for (i, p) in chunk.protos.iter().enumerate() {
        let name = match &p.name {
            FunctionName::Main => "main".to_string(),
            FunctionName::Global(n) => format!("global {}", text(n)),
            FunctionName::Local(n) => format!("local {}", text(n)),
            FunctionName::Field(n) => format!("field {}", text(n)),
            FunctionName::Method(n) => format!("method {}", text(n)),
            FunctionName::Anonymous => "anonymous".to_string(),
        };
        let _ = writeln!(
            out,
            "proto {i} {name} lines {}-{} params {}{} regs {} cells {} caches {} children {:?}",
            p.line_defined,
            p.last_line,
            p.nparams,
            if p.is_vararg { "+..." } else { "" },
            p.nregs,
            p.ncells,
            p.ncaches,
            p.children
        );
        for (k, c) in p.consts.iter().enumerate() {
            let c = match c {
                Constant::Int(i) => format!("{i}"),
                Constant::Float(x) => format!("{x:?}"),
                Constant::Str(s) => text(s),
            };
            let _ = writeln!(out, "  k{k} = {c}");
        }
        for (k, c) in p.captures.iter().enumerate() {
            let name = p
                .debug
                .capture_names
                .get(k)
                .map(|n| text(n))
                .unwrap_or_default();
            let _ = writeln!(out, "  u{k} {name} <- {c:?}");
        }
        for (pc, op) in p.code.iter().enumerate() {
            let mut line = format!("  {pc:>4} [{:>3}] {op:?}", p.debug.line(pc as u32));
            let target = match *op {
                Op::Jump { off }
                | Op::JumpIfTrue { off, .. }
                | Op::JumpIfFalse { off, .. }
                | Op::JumpLt { off, .. }
                | Op::JumpNotLt { off, .. }
                | Op::JumpLe { off, .. }
                | Op::JumpNotLe { off, .. }
                | Op::JumpEq { off, .. }
                | Op::JumpNe { off, .. }
                | Op::ForPrep { exit: off, .. }
                | Op::ForLoop { back: off, .. }
                | Op::GenericLoop { back: off, .. } => Some(pc as i64 + 1 + off as i64),
                _ => None,
            };
            if let Some(t) = target {
                let _ = write!(line, " -> {t}");
            }
            for o in p.debug.origins.iter().filter(|o| o.pc == pc as u32) {
                let _ = write!(line, " ; {}: {}", o.operand, origin(&o.origin));
            }
            let _ = writeln!(out, "{line}");
        }
        for l in &p.debug.locals {
            let slot = match l.slot {
                LocalSlot::Reg(r) => format!("r{r}"),
                LocalSlot::Cell(c) => format!("c{c}"),
            };
            let _ = writeln!(
                out,
                "  local {} {slot} pc {}..{}",
                text(&l.name),
                l.start_pc,
                l.end_pc
            );
        }
    }
    out
}
