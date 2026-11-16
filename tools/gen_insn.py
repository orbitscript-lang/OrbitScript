#!/usr/bin/env python3
"""Generate insn.rs (packed instruction word) from the Op enum in lib.rs."""
import re, sys
src = open(sys.argv[1]).read()
enum = src[src.index('pub enum Op {'):]
enum = enum[:enum.index('\n}\n')]
# strip comments
enum = re.sub(r'//[^\n]*', '', enum)
body = enum[enum.index('{')+1:]
variants = []
for m in re.finditer(r'([A-Z]\w*)\s*(\{[^}]*\})?\s*,', body):
    name, fields = m.group(1), m.group(2)
    fl = []
    if fields:
        for f in re.finditer(r'(\w+):\s*(\w+)', fields):
            fl.append((f.group(1), f.group(2)))
    variants.append((name, fl))
assert len(variants) > 80, len(variants)
W8 = {'Reg': 'u8', 'u8': 'u8', 'i8': 'i8', 'bool': 'bool'}
W16 = {'Const': 'u16', 'Ic': 'u16', 'u16': 'u16', 'i16': 'i16'}
W32 = {'Offset': 'i32', 'i32': 'i32', 'u32': 'u32'}
out = []
P = out.append
P('//! The packed instruction word the interpreter runs: generated from `Op`')
P('//! by `tools/gen_insn.py`, do not edit by hand.')
P('//!')
P('//! Every instruction is 8 bytes: the opcode, three 8-bit slots `a`, `b`,')
P('//! `c`, and a 32-bit slot `d` that holds one 32-bit operand, two 16-bit')
P('//! ones (`dlo`, `dhi`), or a fourth 8-bit one (`e`). A variant\'s 8-bit')
P('//! fields take `a`, `b`, `c` (then `e`) in declaration order, its 16-bit')
P('//! fields `dlo` then `dhi`, and a 32-bit field `d`, so the dispatch loop')
P('//! reads each operand from a fixed place without decoding the whole `Op`.')
P('')
P('use super::Op;')
P('')
P('/// The operation of an [`Insn`].')
P('#[repr(u8)]')
P('#[derive(Debug, Clone, Copy, PartialEq, Eq)]')
P('pub enum Opcode {')
for name, _ in variants:
    P(f'    {name},')
P('    /// Not an instruction: an opcode byte past the last opcode.')
P('    Invalid,')
P('}')
P('')
P('/// One packed instruction, a single 64-bit word so the dispatch loop keeps')
P('/// it in one register and reads operands out of it with shifts. Byte 0 is')
P('/// the opcode, bytes 1 to 3 the slots `a`, `b`, `c`, bytes 4 to 7 `d`.')
P('#[derive(Clone, Copy, PartialEq, Eq)]')
P('pub struct Insn(u64);')
P('')
P('/// Every opcode, in order; the table maps an opcode byte back to it.')
P(f'const OPCODES: [Opcode; {len(variants)}] = [')
for name, _ in variants:
    P(f'    Opcode::{name},')
P('];')
P('')
P('/// The opcode for each byte value, [`Opcode::Invalid`] past the last.')
P('static OPCODE_OF: [Opcode; 256] = {')
P('    let mut t = [Opcode::Invalid; 256];')
P('    let mut i = 0;')
P('    while i < OPCODES.len() {')
P('        t[i] = OPCODES[i];')
P('        i += 1;')
P('    }')
P('    t')
P('};')
P('')
P('impl Insn {')
P('    /// The operation.')
P('    #[inline(always)]')
P('    pub fn op(self) -> Opcode {')
P('        OPCODE_OF[self.0 as u8 as usize]')
P('    }')
P('    /// Slot `a`, byte 1.')
P('    #[inline(always)]')
P('    pub fn a(self) -> u8 {')
P('        (self.0 >> 8) as u8')
P('    }')
P('    /// Slot `b`, byte 2.')
P('    #[inline(always)]')
P('    pub fn b(self) -> u8 {')
P('        (self.0 >> 16) as u8')
P('    }')
P('    /// Slot `c`, byte 3.')
P('    #[inline(always)]')
P('    pub fn c(self) -> u8 {')
P('        (self.0 >> 24) as u8')
P('    }')
P('    /// Slot `d`, bytes 4 to 7.')
P('    #[inline(always)]')
P('    pub fn d(self) -> u32 {')
P('        (self.0 >> 32) as u32')
P('    }')
P('    /// The low 16 bits of `d`.')
P('    #[inline(always)]')
P('    pub fn dlo(self) -> u16 {')
P('        (self.0 >> 32) as u16')
P('    }')
P('    /// The high 16 bits of `d`.')
P('    #[inline(always)]')
P('    pub fn dhi(self) -> u16 {')
P('        (self.0 >> 48) as u16')
P('    }')
P('    /// The low byte of `d`, a fourth 8-bit operand.')
P('    #[inline(always)]')
P('    pub fn e(self) -> u8 {')
P('        (self.0 >> 32) as u8')
P('    }')
P('    /// `d` as a signed offset.')
P('    #[inline(always)]')
P('    pub fn sd(self) -> i32 {')
P('        (self.0 >> 32) as i32')
P('    }')
P('    /// The opcode byte, an index into a 256-entry table.')
P('    #[inline(always)]')
P('    pub fn code(self) -> usize {')
P('        (self.0 & 0xff) as usize')
P('    }')
P('    /// Pack the parts.')
P('    #[inline(always)]')
P('    const fn new(op: Opcode, a: u8, b: u8, c: u8, d: u32) -> Insn {')
P('        Insn(op as u64 | (a as u64) << 8 | (b as u64) << 16 | (c as u64) << 24 | (d as u64) << 32)')
P('    }')
P('')
P('    /// The instruction as an [`Op`].')
P('    pub fn decode(self) -> Op {')
P('        match self.op() {')
layouts = {}
for name, fl in variants:
    slots8 = ['a', 'b', 'c', 'e']; slots16 = ['dlo', 'dhi']
    used_d = False; assign = []
    for fname, ty in fl:
        ty = W8.get(ty) or W16.get(ty) or W32.get(ty) or ty
        if ty in W8:
            s = slots8.pop(0); assign.append((fname, ty, s))
            if s == 'e': used_d = True
        elif ty in W16:
            s = slots16.pop(0); assign.append((fname, ty, s)); used_d = True
        elif ty in W32:
            assert not used_d and 'd' not in [a[2] for a in assign], name
            assign.append((fname, ty, 'd')); used_d = True
        else:
            raise SystemExit(f'unknown type {ty} in {name}')
    if any(s == 'd' for _, _, s in assign):
        assert all(s in ('a', 'b', 'c', 'd') for _, _, s in assign), name
    layouts[name] = assign
    if not fl:
        P(f'            Opcode::{name} => Op::{name},')
        continue
    parts = []
    for fname, ty, s in assign:
        rd = {'a': 'self.a()', 'b': 'self.b()', 'c': 'self.c()', 'e': 'self.e()', 'dlo': 'self.dlo()', 'dhi': 'self.dhi()', 'd': 'self.d()'}[s]
        if ty == 'bool': expr = f'{rd} != 0'
        elif ty == 'i8': expr = f'{rd} as i8'
        elif ty == 'i16': expr = f'{rd} as i16'
        elif ty == 'i32': expr = f'{rd} as i32'
        else: expr = rd
        parts.append(f'{fname}: {expr}')
    P(f'            Opcode::{name} => Op::{name} {{ {", ".join(parts)} }},')
P('            Opcode::Invalid => unreachable!("an Insn always holds an instruction"),')
P('        }')
P('    }')
P('}')
P('')
P('/// Operand readers, one per instruction: the fields of the `Op` variant')
P('/// in declaration order.')
P('#[allow(missing_docs, clippy::type_complexity)]')
P('impl Insn {')
import re as _re
for name, fl in variants:
    assign = layouts[name]
    if not fl: continue
    snake = _re.sub(r'(?<!^)(?=[A-Z])', '_', name).lower()
    tys = []; exprs = []
    for fname, ty, sl in assign:
        rd = {'a': 'self.a()', 'b': 'self.b()', 'c': 'self.c()', 'e': 'self.e()', 'dlo': 'self.dlo()', 'dhi': 'self.dhi()', 'd': 'self.d()'}[sl]
        if ty == 'bool': expr = f'{rd} != 0'
        elif ty in ('i8','i16','i32'): expr = f'{rd} as {ty}'
        else: expr = rd
        tys.append(ty); exprs.append(expr)
    if len(tys) == 1:
        P(f'    #[inline(always)]\n    pub fn r_{snake}(self) -> {tys[0]} {{\n        {exprs[0]}\n    }}')
    else:
        P(f'    #[inline(always)]\n    pub fn r_{snake}(self) -> ({", ".join(tys)}) {{\n        ({", ".join(exprs)})\n    }}')
P('}')
P('')
P('impl std::fmt::Debug for Insn {')
P('    fn fmt(&self, f: &mut std::fmt::Formatter<\'_>) -> std::fmt::Result {')
P('        self.decode().fmt(f)')
P('    }')
P('}')
P('')
P('impl Op {')
P('    /// The instruction packed for the interpreter.')
P('    pub fn encode(self) -> Insn {')
P('        match self {')
for name, fl in variants:
    assign = layouts[name]
    if not fl:
        P(f'            Op::{name} => Insn::new(Opcode::{name}, 0, 0, 0, 0),')
        continue
    names = ', '.join(f for f, _, _ in assign)
    vals = {'a': '0', 'b': '0', 'c': '0'}; dparts = []
    for fname, ty, s in assign:
        if ty == 'bool': v = f'{fname} as u8'
        elif ty == 'i8': v = f'{fname} as u8'
        elif ty == 'i16': v = f'{fname} as u16'
        elif ty == 'i32': v = f'{fname} as u32'
        else: v = fname
        if s in ('a', 'b', 'c'): vals[s] = v
        elif s == 'e': dparts.append(f'{v} as u32')
        elif s == 'dlo': dparts.append(f'{v} as u32')
        elif s == 'dhi': dparts.append(f'(({v} as u32) << 16)')
        elif s == 'd': dparts.append(v)
    d = ' | '.join(dparts) if dparts else '0'
    P(f'            Op::{name} {{ {names} }} => Insn::new(Opcode::{name}, {vals["a"]}, {vals["b"]}, {vals["c"]}, {d}),')
P('        }')
P('    }')
P('}')
P('')
P('#[cfg(test)]')
P('mod tests {')
P('    use super::super::Op;')
P('')
P('    #[test]')
P('    fn round_trip() {')
P('        let ops = [')
# sample values
def sample(ty, i):
    return {'Reg': f'{(i*37)%256}', 'u8': f'{(i*53)%256}', 'i8': f'{((i*71)%256)-128}', 'bool': 'true' if i%2 else 'false',
            'Const': f'{(i*1234)%65536}', 'Ic': f'{(i*4321)%65536}', 'u16': f'{(i*999)%65536}', 'i16': f'{((i*777)%65536)-32768}',
            'Offset': f'{(i*100003)%2000000-1000000}', 'i32': f'{(i*100003)%2000000-1000000}', 'u32': f'{(i*100003)%33554432}'}[ty]
for name, fl in variants:
    for i in (1, 2, 3):
        if fl:
            P(f'            Op::{name} {{ {", ".join(f"{f}: {sample(t, i)}" for f, t in fl)} }},')
        else:
            P(f'            Op::{name},')
P('        ];')
P('        for op in ops {')
P('            assert_eq!(op.encode().decode(), op, "{op:?}");')
P('        }')
P('        assert_eq!(std::mem::size_of::<super::Insn>(), 8);')
P('    }')
P('}')
open(sys.argv[2], 'w').write('\n'.join(out) + '\n')
print(len(variants), 'variants')
