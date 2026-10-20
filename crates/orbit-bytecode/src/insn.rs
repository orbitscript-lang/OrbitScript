//! The packed instruction word the interpreter runs: generated from `Op`
//! by `tools/gen_insn.py`, do not edit by hand.
//!
//! Every instruction is 8 bytes: the opcode, three 8-bit slots `a`, `b`,
//! `c`, and a 32-bit slot `d` that holds one 32-bit operand, two 16-bit
//! ones (`dlo`, `dhi`), or a fourth 8-bit one (`e`). A variant's 8-bit
//! fields take `a`, `b`, `c` (then `e`) in declaration order, its 16-bit
//! fields `dlo` then `dhi`, and a 32-bit field `d`, so the dispatch loop
//! reads each operand from a fixed place without decoding the whole `Op`.

use super::Op;

/// The operation of an [`Insn`].
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Opcode {
    Move,
    LoadNil,
    LoadBool,
    LoadInt,
    LoadConst,
    LoadConstX,
    CellNew,
    CellGet,
    CellSet,
    CellClear,
    CapGet,
    CapSet,
    CapSelf,
    GetGlobal,
    SetGlobal,
    CheckUndefined,
    NewTable,
    GetField,
    SetField,
    GetIndex,
    SetIndex,
    GetIndexInt,
    SetIndexInt,
    SetList,
    SelfCall,
    Add,
    Sub,
    Mul,
    Div,
    IDiv,
    Mod,
    Pow,
    AddInt,
    SubInt,
    AddK,
    SubK,
    MulK,
    DivK,
    IDivK,
    ModK,
    PowK,
    KAdd,
    KSub,
    KMul,
    KDiv,
    BAnd,
    BOr,
    BXor,
    Shl,
    Shr,
    Neg,
    BNot,
    Not,
    Len,
    Cast,
    Concat,
    Jump,
    JumpIfTrue,
    JumpIfFalse,
    JumpLt,
    JumpNotLt,
    JumpLe,
    JumpNotLe,
    JumpEq,
    JumpNe,
    JumpEqInt,
    JumpLtInt,
    JumpLeInt,
    JumpGtInt,
    JumpGeInt,
    Call,
    TailCall,
    Return,
    Return0,
    Return1,
    Vararg,
    VarargTable,
    VarargGet,
    VarargLen,
    VarargGetInt,
    VarargUnpack,
    ForPrep,
    ForPrepExclusive,
    ForLoop,
    GenericCall,
    GenericLoop,
    Closure,
    Tbc,
    Close,
    NativeEntry,
    /// Not an instruction: an opcode byte past the last opcode.
    Invalid,
}

/// One packed instruction, a single 64-bit word so the dispatch loop keeps
/// it in one register and reads operands out of it with shifts. Byte 0 is
/// the opcode, bytes 1 to 3 the slots `a`, `b`, `c`, bytes 4 to 7 `d`.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Insn(u64);

/// Every opcode, in order; the table maps an opcode byte back to it.
const OPCODES: [Opcode; 90] = [
    Opcode::Move,
    Opcode::LoadNil,
    Opcode::LoadBool,
    Opcode::LoadInt,
    Opcode::LoadConst,
    Opcode::LoadConstX,
    Opcode::CellNew,
    Opcode::CellGet,
    Opcode::CellSet,
    Opcode::CellClear,
    Opcode::CapGet,
    Opcode::CapSet,
    Opcode::CapSelf,
    Opcode::GetGlobal,
    Opcode::SetGlobal,
    Opcode::CheckUndefined,
    Opcode::NewTable,
    Opcode::GetField,
    Opcode::SetField,
    Opcode::GetIndex,
    Opcode::SetIndex,
    Opcode::GetIndexInt,
    Opcode::SetIndexInt,
    Opcode::SetList,
    Opcode::SelfCall,
    Opcode::Add,
    Opcode::Sub,
    Opcode::Mul,
    Opcode::Div,
    Opcode::IDiv,
    Opcode::Mod,
    Opcode::Pow,
    Opcode::AddInt,
    Opcode::SubInt,
    Opcode::AddK,
    Opcode::SubK,
    Opcode::MulK,
    Opcode::DivK,
    Opcode::IDivK,
    Opcode::ModK,
    Opcode::PowK,
    Opcode::KAdd,
    Opcode::KSub,
    Opcode::KMul,
    Opcode::KDiv,
    Opcode::BAnd,
    Opcode::BOr,
    Opcode::BXor,
    Opcode::Shl,
    Opcode::Shr,
    Opcode::Neg,
    Opcode::BNot,
    Opcode::Not,
    Opcode::Len,
    Opcode::Cast,
    Opcode::Concat,
    Opcode::Jump,
    Opcode::JumpIfTrue,
    Opcode::JumpIfFalse,
    Opcode::JumpLt,
    Opcode::JumpNotLt,
    Opcode::JumpLe,
    Opcode::JumpNotLe,
    Opcode::JumpEq,
    Opcode::JumpNe,
    Opcode::JumpEqInt,
    Opcode::JumpLtInt,
    Opcode::JumpLeInt,
    Opcode::JumpGtInt,
    Opcode::JumpGeInt,
    Opcode::Call,
    Opcode::TailCall,
    Opcode::Return,
    Opcode::Return0,
    Opcode::Return1,
    Opcode::Vararg,
    Opcode::VarargTable,
    Opcode::VarargGet,
    Opcode::VarargLen,
    Opcode::VarargGetInt,
    Opcode::VarargUnpack,
    Opcode::ForPrep,
    Opcode::ForPrepExclusive,
    Opcode::ForLoop,
    Opcode::GenericCall,
    Opcode::GenericLoop,
    Opcode::Closure,
    Opcode::Tbc,
    Opcode::Close,
    Opcode::NativeEntry,
];

/// The opcode for each byte value, [`Opcode::Invalid`] past the last.
static OPCODE_OF: [Opcode; 256] = {
    let mut t = [Opcode::Invalid; 256];
    let mut i = 0;
    while i < OPCODES.len() {
        t[i] = OPCODES[i];
        i += 1;
    }
    t
};

impl Insn {
    /// The operation.
    #[inline(always)]
    pub fn op(self) -> Opcode {
        OPCODE_OF[self.0 as u8 as usize]
    }
    /// Slot `a`, byte 1.
    #[inline(always)]
    pub fn a(self) -> u8 {
        (self.0 >> 8) as u8
    }
    /// Slot `b`, byte 2.
    #[inline(always)]
    pub fn b(self) -> u8 {
        (self.0 >> 16) as u8
    }
    /// Slot `c`, byte 3.
    #[inline(always)]
    pub fn c(self) -> u8 {
        (self.0 >> 24) as u8
    }
    /// Slot `d`, bytes 4 to 7.
    #[inline(always)]
    pub fn d(self) -> u32 {
        (self.0 >> 32) as u32
    }
    /// The low 16 bits of `d`.
    #[inline(always)]
    pub fn dlo(self) -> u16 {
        (self.0 >> 32) as u16
    }
    /// The high 16 bits of `d`.
    #[inline(always)]
    pub fn dhi(self) -> u16 {
        (self.0 >> 48) as u16
    }
    /// The low byte of `d`, a fourth 8-bit operand.
    #[inline(always)]
    pub fn e(self) -> u8 {
        (self.0 >> 32) as u8
    }
    /// `d` as a signed offset.
    #[inline(always)]
    pub fn sd(self) -> i32 {
        (self.0 >> 32) as i32
    }
    /// Pack the parts.
    #[inline(always)]
    const fn new(op: Opcode, a: u8, b: u8, c: u8, d: u32) -> Insn {
        Insn(op as u64 | (a as u64) << 8 | (b as u64) << 16 | (c as u64) << 24 | (d as u64) << 32)
    }

    /// The instruction as an [`Op`].
    pub fn decode(self) -> Op {
        match self.op() {
            Opcode::Move => Op::Move {
                dst: self.a(),
                src: self.b(),
            },
            Opcode::LoadNil => Op::LoadNil {
                dst: self.a(),
                count: self.b(),
            },
            Opcode::LoadBool => Op::LoadBool {
                dst: self.a(),
                value: self.b() != 0,
            },
            Opcode::LoadInt => Op::LoadInt {
                dst: self.a(),
                value: self.d() as i32,
            },
            Opcode::LoadConst => Op::LoadConst {
                dst: self.a(),
                k: self.dlo(),
            },
            Opcode::LoadConstX => Op::LoadConstX {
                dst: self.a(),
                k: self.d(),
            },
            Opcode::CellNew => Op::CellNew {
                cell: self.a(),
                src: self.b(),
            },
            Opcode::CellGet => Op::CellGet {
                dst: self.a(),
                cell: self.b(),
            },
            Opcode::CellSet => Op::CellSet {
                cell: self.a(),
                src: self.b(),
            },
            Opcode::CellClear => Op::CellClear {
                from: self.a(),
                count: self.b(),
            },
            Opcode::CapGet => Op::CapGet {
                dst: self.a(),
                cap: self.b(),
            },
            Opcode::CapSet => Op::CapSet {
                cap: self.a(),
                src: self.b(),
            },
            Opcode::CapSelf => Op::CapSelf { dst: self.a() },
            Opcode::GetGlobal => Op::GetGlobal {
                dst: self.a(),
                name: self.dlo(),
                ic: self.dhi(),
            },
            Opcode::SetGlobal => Op::SetGlobal {
                src: self.a(),
                name: self.dlo(),
                ic: self.dhi(),
            },
            Opcode::CheckUndefined => Op::CheckUndefined {
                src: self.a(),
                name: self.dlo(),
            },
            Opcode::NewTable => Op::NewTable {
                dst: self.a(),
                array_hint: self.dlo(),
                hash_hint: self.dhi(),
            },
            Opcode::GetField => Op::GetField {
                dst: self.a(),
                obj: self.b(),
                key: self.dlo(),
                ic: self.dhi(),
            },
            Opcode::SetField => Op::SetField {
                obj: self.a(),
                key: self.dlo(),
                src: self.b(),
                ic: self.dhi(),
            },
            Opcode::GetIndex => Op::GetIndex {
                dst: self.a(),
                obj: self.b(),
                key: self.c(),
            },
            Opcode::SetIndex => Op::SetIndex {
                obj: self.a(),
                key: self.b(),
                src: self.c(),
            },
            Opcode::GetIndexInt => Op::GetIndexInt {
                dst: self.a(),
                obj: self.b(),
                key: self.dlo(),
            },
            Opcode::SetIndexInt => Op::SetIndexInt {
                obj: self.a(),
                key: self.dlo(),
                src: self.b(),
            },
            Opcode::SetList => Op::SetList {
                table: self.a(),
                first: self.b(),
                count: self.c(),
                start: self.d(),
            },
            Opcode::SelfCall => Op::SelfCall {
                dst: self.a(),
                obj: self.b(),
                key: self.dlo(),
                ic: self.dhi(),
            },
            Opcode::Add => Op::Add {
                dst: self.a(),
                a: self.b(),
                b: self.c(),
            },
            Opcode::Sub => Op::Sub {
                dst: self.a(),
                a: self.b(),
                b: self.c(),
            },
            Opcode::Mul => Op::Mul {
                dst: self.a(),
                a: self.b(),
                b: self.c(),
            },
            Opcode::Div => Op::Div {
                dst: self.a(),
                a: self.b(),
                b: self.c(),
            },
            Opcode::IDiv => Op::IDiv {
                dst: self.a(),
                a: self.b(),
                b: self.c(),
            },
            Opcode::Mod => Op::Mod {
                dst: self.a(),
                a: self.b(),
                b: self.c(),
            },
            Opcode::Pow => Op::Pow {
                dst: self.a(),
                a: self.b(),
                b: self.c(),
            },
            Opcode::AddInt => Op::AddInt {
                dst: self.a(),
                a: self.b(),
                imm: self.dlo() as i16,
            },
            Opcode::SubInt => Op::SubInt {
                dst: self.a(),
                a: self.b(),
                imm: self.dlo() as i16,
            },
            Opcode::AddK => Op::AddK {
                dst: self.a(),
                a: self.b(),
                k: self.dlo(),
            },
            Opcode::SubK => Op::SubK {
                dst: self.a(),
                a: self.b(),
                k: self.dlo(),
            },
            Opcode::MulK => Op::MulK {
                dst: self.a(),
                a: self.b(),
                k: self.dlo(),
            },
            Opcode::DivK => Op::DivK {
                dst: self.a(),
                a: self.b(),
                k: self.dlo(),
            },
            Opcode::IDivK => Op::IDivK {
                dst: self.a(),
                a: self.b(),
                k: self.dlo(),
            },
            Opcode::ModK => Op::ModK {
                dst: self.a(),
                a: self.b(),
                k: self.dlo(),
            },
            Opcode::PowK => Op::PowK {
                dst: self.a(),
                a: self.b(),
                k: self.dlo(),
            },
            Opcode::KAdd => Op::KAdd {
                dst: self.a(),
                k: self.dlo(),
                b: self.b(),
            },
            Opcode::KSub => Op::KSub {
                dst: self.a(),
                k: self.dlo(),
                b: self.b(),
            },
            Opcode::KMul => Op::KMul {
                dst: self.a(),
                k: self.dlo(),
                b: self.b(),
            },
            Opcode::KDiv => Op::KDiv {
                dst: self.a(),
                k: self.dlo(),
                b: self.b(),
            },
            Opcode::BAnd => Op::BAnd {
                dst: self.a(),
                a: self.b(),
                b: self.c(),
            },
            Opcode::BOr => Op::BOr {
                dst: self.a(),
                a: self.b(),
                b: self.c(),
            },
            Opcode::BXor => Op::BXor {
                dst: self.a(),
                a: self.b(),
                b: self.c(),
            },
            Opcode::Shl => Op::Shl {
                dst: self.a(),
                a: self.b(),
                b: self.c(),
            },
            Opcode::Shr => Op::Shr {
                dst: self.a(),
                a: self.b(),
                b: self.c(),
            },
            Opcode::Neg => Op::Neg {
                dst: self.a(),
                src: self.b(),
            },
            Opcode::BNot => Op::BNot {
                dst: self.a(),
                src: self.b(),
            },
            Opcode::Not => Op::Not {
                dst: self.a(),
                src: self.b(),
            },
            Opcode::Len => Op::Len {
                dst: self.a(),
                src: self.b(),
            },
            Opcode::Cast => Op::Cast {
                dst: self.a(),
                src: self.b(),
                ty: self.c(),
                mode: self.e(),
            },
            Opcode::Concat => Op::Concat {
                dst: self.a(),
                first: self.b(),
                count: self.c(),
            },
            Opcode::Jump => Op::Jump {
                off: self.d() as i32,
            },
            Opcode::JumpIfTrue => Op::JumpIfTrue {
                src: self.a(),
                off: self.d() as i32,
            },
            Opcode::JumpIfFalse => Op::JumpIfFalse {
                src: self.a(),
                off: self.d() as i32,
            },
            Opcode::JumpLt => Op::JumpLt {
                a: self.a(),
                b: self.b(),
                off: self.d() as i32,
            },
            Opcode::JumpNotLt => Op::JumpNotLt {
                a: self.a(),
                b: self.b(),
                off: self.d() as i32,
            },
            Opcode::JumpLe => Op::JumpLe {
                a: self.a(),
                b: self.b(),
                off: self.d() as i32,
            },
            Opcode::JumpNotLe => Op::JumpNotLe {
                a: self.a(),
                b: self.b(),
                off: self.d() as i32,
            },
            Opcode::JumpEq => Op::JumpEq {
                a: self.a(),
                b: self.b(),
                off: self.d() as i32,
            },
            Opcode::JumpNe => Op::JumpNe {
                a: self.a(),
                b: self.b(),
                off: self.d() as i32,
            },
            Opcode::JumpEqInt => Op::JumpEqInt {
                a: self.a(),
                imm: self.b() as i8,
                want: self.c() != 0,
                off: self.d() as i32,
            },
            Opcode::JumpLtInt => Op::JumpLtInt {
                a: self.a(),
                imm: self.b() as i8,
                want: self.c() != 0,
                off: self.d() as i32,
            },
            Opcode::JumpLeInt => Op::JumpLeInt {
                a: self.a(),
                imm: self.b() as i8,
                want: self.c() != 0,
                off: self.d() as i32,
            },
            Opcode::JumpGtInt => Op::JumpGtInt {
                a: self.a(),
                imm: self.b() as i8,
                want: self.c() != 0,
                off: self.d() as i32,
            },
            Opcode::JumpGeInt => Op::JumpGeInt {
                a: self.a(),
                imm: self.b() as i8,
                want: self.c() != 0,
                off: self.d() as i32,
            },
            Opcode::Call => Op::Call {
                func: self.a(),
                nargs: self.b(),
                nresults: self.c(),
            },
            Opcode::TailCall => Op::TailCall {
                func: self.a(),
                nargs: self.b(),
            },
            Opcode::Return => Op::Return {
                first: self.a(),
                count: self.b(),
            },
            Opcode::Return0 => Op::Return0,
            Opcode::Return1 => Op::Return1 { src: self.a() },
            Opcode::Vararg => Op::Vararg {
                dst: self.a(),
                count: self.b(),
            },
            Opcode::VarargTable => Op::VarargTable { dst: self.a() },
            Opcode::VarargGet => Op::VarargGet {
                dst: self.a(),
                key: self.b(),
            },
            Opcode::VarargLen => Op::VarargLen { dst: self.a() },
            Opcode::VarargGetInt => Op::VarargGetInt {
                dst: self.a(),
                index: self.b(),
            },
            Opcode::VarargUnpack => Op::VarargUnpack {
                dst: self.a(),
                table: self.b(),
                count: self.c(),
            },
            Opcode::ForPrep => Op::ForPrep {
                base: self.a(),
                exit: self.d() as i32,
            },
            Opcode::ForPrepExclusive => Op::ForPrepExclusive {
                base: self.a(),
                exit: self.d() as i32,
            },
            Opcode::ForLoop => Op::ForLoop {
                base: self.a(),
                back: self.d() as i32,
            },
            Opcode::GenericCall => Op::GenericCall {
                base: self.a(),
                nvars: self.b(),
            },
            Opcode::GenericLoop => Op::GenericLoop {
                base: self.a(),
                back: self.d() as i32,
            },
            Opcode::Closure => Op::Closure {
                dst: self.a(),
                proto: self.dlo(),
            },
            Opcode::Tbc => Op::Tbc { reg: self.a() },
            Opcode::Close => Op::Close { from: self.a() },
            Opcode::NativeEntry => Op::NativeEntry { id: self.d() },
            Opcode::Invalid => unreachable!("an Insn always holds an instruction"),
        }
    }
}

impl std::fmt::Debug for Insn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.decode().fmt(f)
    }
}

impl Op {
    /// The instruction packed for the interpreter.
    pub fn encode(self) -> Insn {
        match self {
            Op::Move { dst, src } => Insn::new(Opcode::Move, dst, src, 0, 0),
            Op::LoadNil { dst, count } => Insn::new(Opcode::LoadNil, dst, count, 0, 0),
            Op::LoadBool { dst, value } => Insn::new(Opcode::LoadBool, dst, value as u8, 0, 0),
            Op::LoadInt { dst, value } => Insn::new(Opcode::LoadInt, dst, 0, 0, value as u32),
            Op::LoadConst { dst, k } => Insn::new(Opcode::LoadConst, dst, 0, 0, k as u32),
            Op::LoadConstX { dst, k } => Insn::new(Opcode::LoadConstX, dst, 0, 0, k),
            Op::CellNew { cell, src } => Insn::new(Opcode::CellNew, cell, src, 0, 0),
            Op::CellGet { dst, cell } => Insn::new(Opcode::CellGet, dst, cell, 0, 0),
            Op::CellSet { cell, src } => Insn::new(Opcode::CellSet, cell, src, 0, 0),
            Op::CellClear { from, count } => Insn::new(Opcode::CellClear, from, count, 0, 0),
            Op::CapGet { dst, cap } => Insn::new(Opcode::CapGet, dst, cap, 0, 0),
            Op::CapSet { cap, src } => Insn::new(Opcode::CapSet, cap, src, 0, 0),
            Op::CapSelf { dst } => Insn::new(Opcode::CapSelf, dst, 0, 0, 0),
            Op::GetGlobal { dst, name, ic } => Insn::new(
                Opcode::GetGlobal,
                dst,
                0,
                0,
                name as u32 | ((ic as u32) << 16),
            ),
            Op::SetGlobal { src, name, ic } => Insn::new(
                Opcode::SetGlobal,
                src,
                0,
                0,
                name as u32 | ((ic as u32) << 16),
            ),
            Op::CheckUndefined { src, name } => {
                Insn::new(Opcode::CheckUndefined, src, 0, 0, name as u32)
            }
            Op::NewTable {
                dst,
                array_hint,
                hash_hint,
            } => Insn::new(
                Opcode::NewTable,
                dst,
                0,
                0,
                array_hint as u32 | ((hash_hint as u32) << 16),
            ),
            Op::GetField { dst, obj, key, ic } => Insn::new(
                Opcode::GetField,
                dst,
                obj,
                0,
                key as u32 | ((ic as u32) << 16),
            ),
            Op::SetField { obj, key, src, ic } => Insn::new(
                Opcode::SetField,
                obj,
                src,
                0,
                key as u32 | ((ic as u32) << 16),
            ),
            Op::GetIndex { dst, obj, key } => Insn::new(Opcode::GetIndex, dst, obj, key, 0),
            Op::SetIndex { obj, key, src } => Insn::new(Opcode::SetIndex, obj, key, src, 0),
            Op::GetIndexInt { dst, obj, key } => {
                Insn::new(Opcode::GetIndexInt, dst, obj, 0, key as u32)
            }
            Op::SetIndexInt { obj, key, src } => {
                Insn::new(Opcode::SetIndexInt, obj, src, 0, key as u32)
            }
            Op::SetList {
                table,
                first,
                count,
                start,
            } => Insn::new(Opcode::SetList, table, first, count, start),
            Op::SelfCall { dst, obj, key, ic } => Insn::new(
                Opcode::SelfCall,
                dst,
                obj,
                0,
                key as u32 | ((ic as u32) << 16),
            ),
            Op::Add { dst, a, b } => Insn::new(Opcode::Add, dst, a, b, 0),
            Op::Sub { dst, a, b } => Insn::new(Opcode::Sub, dst, a, b, 0),
            Op::Mul { dst, a, b } => Insn::new(Opcode::Mul, dst, a, b, 0),
            Op::Div { dst, a, b } => Insn::new(Opcode::Div, dst, a, b, 0),
            Op::IDiv { dst, a, b } => Insn::new(Opcode::IDiv, dst, a, b, 0),
            Op::Mod { dst, a, b } => Insn::new(Opcode::Mod, dst, a, b, 0),
            Op::Pow { dst, a, b } => Insn::new(Opcode::Pow, dst, a, b, 0),
            Op::AddInt { dst, a, imm } => Insn::new(Opcode::AddInt, dst, a, 0, imm as u16 as u32),
            Op::SubInt { dst, a, imm } => Insn::new(Opcode::SubInt, dst, a, 0, imm as u16 as u32),
            Op::AddK { dst, a, k } => Insn::new(Opcode::AddK, dst, a, 0, k as u32),
            Op::SubK { dst, a, k } => Insn::new(Opcode::SubK, dst, a, 0, k as u32),
            Op::MulK { dst, a, k } => Insn::new(Opcode::MulK, dst, a, 0, k as u32),
            Op::DivK { dst, a, k } => Insn::new(Opcode::DivK, dst, a, 0, k as u32),
            Op::IDivK { dst, a, k } => Insn::new(Opcode::IDivK, dst, a, 0, k as u32),
            Op::ModK { dst, a, k } => Insn::new(Opcode::ModK, dst, a, 0, k as u32),
            Op::PowK { dst, a, k } => Insn::new(Opcode::PowK, dst, a, 0, k as u32),
            Op::KAdd { dst, k, b } => Insn::new(Opcode::KAdd, dst, b, 0, k as u32),
            Op::KSub { dst, k, b } => Insn::new(Opcode::KSub, dst, b, 0, k as u32),
            Op::KMul { dst, k, b } => Insn::new(Opcode::KMul, dst, b, 0, k as u32),
            Op::KDiv { dst, k, b } => Insn::new(Opcode::KDiv, dst, b, 0, k as u32),
            Op::BAnd { dst, a, b } => Insn::new(Opcode::BAnd, dst, a, b, 0),
            Op::BOr { dst, a, b } => Insn::new(Opcode::BOr, dst, a, b, 0),
            Op::BXor { dst, a, b } => Insn::new(Opcode::BXor, dst, a, b, 0),
            Op::Shl { dst, a, b } => Insn::new(Opcode::Shl, dst, a, b, 0),
            Op::Shr { dst, a, b } => Insn::new(Opcode::Shr, dst, a, b, 0),
            Op::Neg { dst, src } => Insn::new(Opcode::Neg, dst, src, 0, 0),
            Op::BNot { dst, src } => Insn::new(Opcode::BNot, dst, src, 0, 0),
            Op::Not { dst, src } => Insn::new(Opcode::Not, dst, src, 0, 0),
            Op::Len { dst, src } => Insn::new(Opcode::Len, dst, src, 0, 0),
            Op::Cast { dst, src, ty, mode } => Insn::new(Opcode::Cast, dst, src, ty, mode as u32),
            Op::Concat { dst, first, count } => Insn::new(Opcode::Concat, dst, first, count, 0),
            Op::Jump { off } => Insn::new(Opcode::Jump, 0, 0, 0, off as u32),
            Op::JumpIfTrue { src, off } => Insn::new(Opcode::JumpIfTrue, src, 0, 0, off as u32),
            Op::JumpIfFalse { src, off } => Insn::new(Opcode::JumpIfFalse, src, 0, 0, off as u32),
            Op::JumpLt { a, b, off } => Insn::new(Opcode::JumpLt, a, b, 0, off as u32),
            Op::JumpNotLt { a, b, off } => Insn::new(Opcode::JumpNotLt, a, b, 0, off as u32),
            Op::JumpLe { a, b, off } => Insn::new(Opcode::JumpLe, a, b, 0, off as u32),
            Op::JumpNotLe { a, b, off } => Insn::new(Opcode::JumpNotLe, a, b, 0, off as u32),
            Op::JumpEq { a, b, off } => Insn::new(Opcode::JumpEq, a, b, 0, off as u32),
            Op::JumpNe { a, b, off } => Insn::new(Opcode::JumpNe, a, b, 0, off as u32),
            Op::JumpEqInt { a, imm, want, off } => {
                Insn::new(Opcode::JumpEqInt, a, imm as u8, want as u8, off as u32)
            }
            Op::JumpLtInt { a, imm, want, off } => {
                Insn::new(Opcode::JumpLtInt, a, imm as u8, want as u8, off as u32)
            }
            Op::JumpLeInt { a, imm, want, off } => {
                Insn::new(Opcode::JumpLeInt, a, imm as u8, want as u8, off as u32)
            }
            Op::JumpGtInt { a, imm, want, off } => {
                Insn::new(Opcode::JumpGtInt, a, imm as u8, want as u8, off as u32)
            }
            Op::JumpGeInt { a, imm, want, off } => {
                Insn::new(Opcode::JumpGeInt, a, imm as u8, want as u8, off as u32)
            }
            Op::Call {
                func,
                nargs,
                nresults,
            } => Insn::new(Opcode::Call, func, nargs, nresults, 0),
            Op::TailCall { func, nargs } => Insn::new(Opcode::TailCall, func, nargs, 0, 0),
            Op::Return { first, count } => Insn::new(Opcode::Return, first, count, 0, 0),
            Op::Return0 => Insn::new(Opcode::Return0, 0, 0, 0, 0),
            Op::Return1 { src } => Insn::new(Opcode::Return1, src, 0, 0, 0),
            Op::Vararg { dst, count } => Insn::new(Opcode::Vararg, dst, count, 0, 0),
            Op::VarargTable { dst } => Insn::new(Opcode::VarargTable, dst, 0, 0, 0),
            Op::VarargGet { dst, key } => Insn::new(Opcode::VarargGet, dst, key, 0, 0),
            Op::VarargLen { dst } => Insn::new(Opcode::VarargLen, dst, 0, 0, 0),
            Op::VarargGetInt { dst, index } => Insn::new(Opcode::VarargGetInt, dst, index, 0, 0),
            Op::VarargUnpack { dst, table, count } => {
                Insn::new(Opcode::VarargUnpack, dst, table, count, 0)
            }
            Op::ForPrep { base, exit } => Insn::new(Opcode::ForPrep, base, 0, 0, exit as u32),
            Op::ForPrepExclusive { base, exit } => {
                Insn::new(Opcode::ForPrepExclusive, base, 0, 0, exit as u32)
            }
            Op::ForLoop { base, back } => Insn::new(Opcode::ForLoop, base, 0, 0, back as u32),
            Op::GenericCall { base, nvars } => Insn::new(Opcode::GenericCall, base, nvars, 0, 0),
            Op::GenericLoop { base, back } => {
                Insn::new(Opcode::GenericLoop, base, 0, 0, back as u32)
            }
            Op::Closure { dst, proto } => Insn::new(Opcode::Closure, dst, 0, 0, proto as u32),
            Op::Tbc { reg } => Insn::new(Opcode::Tbc, reg, 0, 0, 0),
            Op::Close { from } => Insn::new(Opcode::Close, from, 0, 0, 0),
            Op::NativeEntry { id } => Insn::new(Opcode::NativeEntry, 0, 0, 0, id),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::Op;

    #[test]
    fn round_trip() {
        let ops = [
            Op::Move { dst: 37, src: 37 },
            Op::Move { dst: 74, src: 74 },
            Op::Move { dst: 111, src: 111 },
            Op::LoadNil { dst: 37, count: 53 },
            Op::LoadNil {
                dst: 74,
                count: 106,
            },
            Op::LoadNil {
                dst: 111,
                count: 159,
            },
            Op::LoadBool {
                dst: 37,
                value: true,
            },
            Op::LoadBool {
                dst: 74,
                value: false,
            },
            Op::LoadBool {
                dst: 111,
                value: true,
            },
            Op::LoadInt {
                dst: 37,
                value: -899997,
            },
            Op::LoadInt {
                dst: 74,
                value: -799994,
            },
            Op::LoadInt {
                dst: 111,
                value: -699991,
            },
            Op::LoadConst { dst: 37, k: 1234 },
            Op::LoadConst { dst: 74, k: 2468 },
            Op::LoadConst { dst: 111, k: 3702 },
            Op::LoadConstX { dst: 37, k: 100003 },
            Op::LoadConstX { dst: 74, k: 200006 },
            Op::LoadConstX {
                dst: 111,
                k: 300009,
            },
            Op::CellNew { cell: 53, src: 37 },
            Op::CellNew { cell: 106, src: 74 },
            Op::CellNew {
                cell: 159,
                src: 111,
            },
            Op::CellGet { dst: 37, cell: 53 },
            Op::CellGet { dst: 74, cell: 106 },
            Op::CellGet {
                dst: 111,
                cell: 159,
            },
            Op::CellSet { cell: 53, src: 37 },
            Op::CellSet { cell: 106, src: 74 },
            Op::CellSet {
                cell: 159,
                src: 111,
            },
            Op::CellClear {
                from: 53,
                count: 53,
            },
            Op::CellClear {
                from: 106,
                count: 106,
            },
            Op::CellClear {
                from: 159,
                count: 159,
            },
            Op::CapGet { dst: 37, cap: 53 },
            Op::CapGet { dst: 74, cap: 106 },
            Op::CapGet { dst: 111, cap: 159 },
            Op::CapSet { cap: 53, src: 37 },
            Op::CapSet { cap: 106, src: 74 },
            Op::CapSet { cap: 159, src: 111 },
            Op::CapSelf { dst: 37 },
            Op::CapSelf { dst: 74 },
            Op::CapSelf { dst: 111 },
            Op::GetGlobal {
                dst: 37,
                name: 1234,
                ic: 4321,
            },
            Op::GetGlobal {
                dst: 74,
                name: 2468,
                ic: 8642,
            },
            Op::GetGlobal {
                dst: 111,
                name: 3702,
                ic: 12963,
            },
            Op::SetGlobal {
                src: 37,
                name: 1234,
                ic: 4321,
            },
            Op::SetGlobal {
                src: 74,
                name: 2468,
                ic: 8642,
            },
            Op::SetGlobal {
                src: 111,
                name: 3702,
                ic: 12963,
            },
            Op::CheckUndefined {
                src: 37,
                name: 1234,
            },
            Op::CheckUndefined {
                src: 74,
                name: 2468,
            },
            Op::CheckUndefined {
                src: 111,
                name: 3702,
            },
            Op::NewTable {
                dst: 37,
                array_hint: 999,
                hash_hint: 999,
            },
            Op::NewTable {
                dst: 74,
                array_hint: 1998,
                hash_hint: 1998,
            },
            Op::NewTable {
                dst: 111,
                array_hint: 2997,
                hash_hint: 2997,
            },
            Op::GetField {
                dst: 37,
                obj: 37,
                key: 1234,
                ic: 4321,
            },
            Op::GetField {
                dst: 74,
                obj: 74,
                key: 2468,
                ic: 8642,
            },
            Op::GetField {
                dst: 111,
                obj: 111,
                key: 3702,
                ic: 12963,
            },
            Op::SetField {
                obj: 37,
                key: 1234,
                src: 37,
                ic: 4321,
            },
            Op::SetField {
                obj: 74,
                key: 2468,
                src: 74,
                ic: 8642,
            },
            Op::SetField {
                obj: 111,
                key: 3702,
                src: 111,
                ic: 12963,
            },
            Op::GetIndex {
                dst: 37,
                obj: 37,
                key: 37,
            },
            Op::GetIndex {
                dst: 74,
                obj: 74,
                key: 74,
            },
            Op::GetIndex {
                dst: 111,
                obj: 111,
                key: 111,
            },
            Op::SetIndex {
                obj: 37,
                key: 37,
                src: 37,
            },
            Op::SetIndex {
                obj: 74,
                key: 74,
                src: 74,
            },
            Op::SetIndex {
                obj: 111,
                key: 111,
                src: 111,
            },
            Op::GetIndexInt {
                dst: 37,
                obj: 37,
                key: 999,
            },
            Op::GetIndexInt {
                dst: 74,
                obj: 74,
                key: 1998,
            },
            Op::GetIndexInt {
                dst: 111,
                obj: 111,
                key: 2997,
            },
            Op::SetIndexInt {
                obj: 37,
                key: 999,
                src: 37,
            },
            Op::SetIndexInt {
                obj: 74,
                key: 1998,
                src: 74,
            },
            Op::SetIndexInt {
                obj: 111,
                key: 2997,
                src: 111,
            },
            Op::SetList {
                table: 37,
                first: 37,
                count: 53,
                start: 100003,
            },
            Op::SetList {
                table: 74,
                first: 74,
                count: 106,
                start: 200006,
            },
            Op::SetList {
                table: 111,
                first: 111,
                count: 159,
                start: 300009,
            },
            Op::SelfCall {
                dst: 37,
                obj: 37,
                key: 1234,
                ic: 4321,
            },
            Op::SelfCall {
                dst: 74,
                obj: 74,
                key: 2468,
                ic: 8642,
            },
            Op::SelfCall {
                dst: 111,
                obj: 111,
                key: 3702,
                ic: 12963,
            },
            Op::Add {
                dst: 37,
                a: 37,
                b: 37,
            },
            Op::Add {
                dst: 74,
                a: 74,
                b: 74,
            },
            Op::Add {
                dst: 111,
                a: 111,
                b: 111,
            },
            Op::Sub {
                dst: 37,
                a: 37,
                b: 37,
            },
            Op::Sub {
                dst: 74,
                a: 74,
                b: 74,
            },
            Op::Sub {
                dst: 111,
                a: 111,
                b: 111,
            },
            Op::Mul {
                dst: 37,
                a: 37,
                b: 37,
            },
            Op::Mul {
                dst: 74,
                a: 74,
                b: 74,
            },
            Op::Mul {
                dst: 111,
                a: 111,
                b: 111,
            },
            Op::Div {
                dst: 37,
                a: 37,
                b: 37,
            },
            Op::Div {
                dst: 74,
                a: 74,
                b: 74,
            },
            Op::Div {
                dst: 111,
                a: 111,
                b: 111,
            },
            Op::IDiv {
                dst: 37,
                a: 37,
                b: 37,
            },
            Op::IDiv {
                dst: 74,
                a: 74,
                b: 74,
            },
            Op::IDiv {
                dst: 111,
                a: 111,
                b: 111,
            },
            Op::Mod {
                dst: 37,
                a: 37,
                b: 37,
            },
            Op::Mod {
                dst: 74,
                a: 74,
                b: 74,
            },
            Op::Mod {
                dst: 111,
                a: 111,
                b: 111,
            },
            Op::Pow {
                dst: 37,
                a: 37,
                b: 37,
            },
            Op::Pow {
                dst: 74,
                a: 74,
                b: 74,
            },
            Op::Pow {
                dst: 111,
                a: 111,
                b: 111,
            },
            Op::AddInt {
                dst: 37,
                a: 37,
                imm: -31991,
            },
            Op::AddInt {
                dst: 74,
                a: 74,
                imm: -31214,
            },
            Op::AddInt {
                dst: 111,
                a: 111,
                imm: -30437,
            },
            Op::SubInt {
                dst: 37,
                a: 37,
                imm: -31991,
            },
            Op::SubInt {
                dst: 74,
                a: 74,
                imm: -31214,
            },
            Op::SubInt {
                dst: 111,
                a: 111,
                imm: -30437,
            },
            Op::AddK {
                dst: 37,
                a: 37,
                k: 1234,
            },
            Op::AddK {
                dst: 74,
                a: 74,
                k: 2468,
            },
            Op::AddK {
                dst: 111,
                a: 111,
                k: 3702,
            },
            Op::SubK {
                dst: 37,
                a: 37,
                k: 1234,
            },
            Op::SubK {
                dst: 74,
                a: 74,
                k: 2468,
            },
            Op::SubK {
                dst: 111,
                a: 111,
                k: 3702,
            },
            Op::MulK {
                dst: 37,
                a: 37,
                k: 1234,
            },
            Op::MulK {
                dst: 74,
                a: 74,
                k: 2468,
            },
            Op::MulK {
                dst: 111,
                a: 111,
                k: 3702,
            },
            Op::DivK {
                dst: 37,
                a: 37,
                k: 1234,
            },
            Op::DivK {
                dst: 74,
                a: 74,
                k: 2468,
            },
            Op::DivK {
                dst: 111,
                a: 111,
                k: 3702,
            },
            Op::IDivK {
                dst: 37,
                a: 37,
                k: 1234,
            },
            Op::IDivK {
                dst: 74,
                a: 74,
                k: 2468,
            },
            Op::IDivK {
                dst: 111,
                a: 111,
                k: 3702,
            },
            Op::ModK {
                dst: 37,
                a: 37,
                k: 1234,
            },
            Op::ModK {
                dst: 74,
                a: 74,
                k: 2468,
            },
            Op::ModK {
                dst: 111,
                a: 111,
                k: 3702,
            },
            Op::PowK {
                dst: 37,
                a: 37,
                k: 1234,
            },
            Op::PowK {
                dst: 74,
                a: 74,
                k: 2468,
            },
            Op::PowK {
                dst: 111,
                a: 111,
                k: 3702,
            },
            Op::KAdd {
                dst: 37,
                k: 1234,
                b: 37,
            },
            Op::KAdd {
                dst: 74,
                k: 2468,
                b: 74,
            },
            Op::KAdd {
                dst: 111,
                k: 3702,
                b: 111,
            },
            Op::KSub {
                dst: 37,
                k: 1234,
                b: 37,
            },
            Op::KSub {
                dst: 74,
                k: 2468,
                b: 74,
            },
            Op::KSub {
                dst: 111,
                k: 3702,
                b: 111,
            },
            Op::KMul {
                dst: 37,
                k: 1234,
                b: 37,
            },
            Op::KMul {
                dst: 74,
                k: 2468,
                b: 74,
            },
            Op::KMul {
                dst: 111,
                k: 3702,
                b: 111,
            },
            Op::KDiv {
                dst: 37,
                k: 1234,
                b: 37,
            },
            Op::KDiv {
                dst: 74,
                k: 2468,
                b: 74,
            },
            Op::KDiv {
                dst: 111,
                k: 3702,
                b: 111,
            },
            Op::BAnd {
                dst: 37,
                a: 37,
                b: 37,
            },
            Op::BAnd {
                dst: 74,
                a: 74,
                b: 74,
            },
            Op::BAnd {
                dst: 111,
                a: 111,
                b: 111,
            },
            Op::BOr {
                dst: 37,
                a: 37,
                b: 37,
            },
            Op::BOr {
                dst: 74,
                a: 74,
                b: 74,
            },
            Op::BOr {
                dst: 111,
                a: 111,
                b: 111,
            },
            Op::BXor {
                dst: 37,
                a: 37,
                b: 37,
            },
            Op::BXor {
                dst: 74,
                a: 74,
                b: 74,
            },
            Op::BXor {
                dst: 111,
                a: 111,
                b: 111,
            },
            Op::Shl {
                dst: 37,
                a: 37,
                b: 37,
            },
            Op::Shl {
                dst: 74,
                a: 74,
                b: 74,
            },
            Op::Shl {
                dst: 111,
                a: 111,
                b: 111,
            },
            Op::Shr {
                dst: 37,
                a: 37,
                b: 37,
            },
            Op::Shr {
                dst: 74,
                a: 74,
                b: 74,
            },
            Op::Shr {
                dst: 111,
                a: 111,
                b: 111,
            },
            Op::Neg { dst: 37, src: 37 },
            Op::Neg { dst: 74, src: 74 },
            Op::Neg { dst: 111, src: 111 },
            Op::BNot { dst: 37, src: 37 },
            Op::BNot { dst: 74, src: 74 },
            Op::BNot { dst: 111, src: 111 },
            Op::Not { dst: 37, src: 37 },
            Op::Not { dst: 74, src: 74 },
            Op::Not { dst: 111, src: 111 },
            Op::Len { dst: 37, src: 37 },
            Op::Len { dst: 74, src: 74 },
            Op::Len { dst: 111, src: 111 },
            Op::Cast {
                dst: 37,
                src: 37,
                ty: 37,
                mode: 53,
            },
            Op::Cast {
                dst: 74,
                src: 74,
                ty: 74,
                mode: 106,
            },
            Op::Cast {
                dst: 111,
                src: 111,
                ty: 111,
                mode: 159,
            },
            Op::Concat {
                dst: 37,
                first: 37,
                count: 53,
            },
            Op::Concat {
                dst: 74,
                first: 74,
                count: 106,
            },
            Op::Concat {
                dst: 111,
                first: 111,
                count: 159,
            },
            Op::Jump { off: -899997 },
            Op::Jump { off: -799994 },
            Op::Jump { off: -699991 },
            Op::JumpIfTrue {
                src: 37,
                off: -899997,
            },
            Op::JumpIfTrue {
                src: 74,
                off: -799994,
            },
            Op::JumpIfTrue {
                src: 111,
                off: -699991,
            },
            Op::JumpIfFalse {
                src: 37,
                off: -899997,
            },
            Op::JumpIfFalse {
                src: 74,
                off: -799994,
            },
            Op::JumpIfFalse {
                src: 111,
                off: -699991,
            },
            Op::JumpLt {
                a: 37,
                b: 37,
                off: -899997,
            },
            Op::JumpLt {
                a: 74,
                b: 74,
                off: -799994,
            },
            Op::JumpLt {
                a: 111,
                b: 111,
                off: -699991,
            },
            Op::JumpNotLt {
                a: 37,
                b: 37,
                off: -899997,
            },
            Op::JumpNotLt {
                a: 74,
                b: 74,
                off: -799994,
            },
            Op::JumpNotLt {
                a: 111,
                b: 111,
                off: -699991,
            },
            Op::JumpLe {
                a: 37,
                b: 37,
                off: -899997,
            },
            Op::JumpLe {
                a: 74,
                b: 74,
                off: -799994,
            },
            Op::JumpLe {
                a: 111,
                b: 111,
                off: -699991,
            },
            Op::JumpNotLe {
                a: 37,
                b: 37,
                off: -899997,
            },
            Op::JumpNotLe {
                a: 74,
                b: 74,
                off: -799994,
            },
            Op::JumpNotLe {
                a: 111,
                b: 111,
                off: -699991,
            },
            Op::JumpEq {
                a: 37,
                b: 37,
                off: -899997,
            },
            Op::JumpEq {
                a: 74,
                b: 74,
                off: -799994,
            },
            Op::JumpEq {
                a: 111,
                b: 111,
                off: -699991,
            },
            Op::JumpNe {
                a: 37,
                b: 37,
                off: -899997,
            },
            Op::JumpNe {
                a: 74,
                b: 74,
                off: -799994,
            },
            Op::JumpNe {
                a: 111,
                b: 111,
                off: -699991,
            },
            Op::JumpEqInt {
                a: 37,
                imm: -57,
                want: true,
                off: -899997,
            },
            Op::JumpEqInt {
                a: 74,
                imm: 14,
                want: false,
                off: -799994,
            },
            Op::JumpEqInt {
                a: 111,
                imm: 85,
                want: true,
                off: -699991,
            },
            Op::JumpLtInt {
                a: 37,
                imm: -57,
                want: true,
                off: -899997,
            },
            Op::JumpLtInt {
                a: 74,
                imm: 14,
                want: false,
                off: -799994,
            },
            Op::JumpLtInt {
                a: 111,
                imm: 85,
                want: true,
                off: -699991,
            },
            Op::JumpLeInt {
                a: 37,
                imm: -57,
                want: true,
                off: -899997,
            },
            Op::JumpLeInt {
                a: 74,
                imm: 14,
                want: false,
                off: -799994,
            },
            Op::JumpLeInt {
                a: 111,
                imm: 85,
                want: true,
                off: -699991,
            },
            Op::JumpGtInt {
                a: 37,
                imm: -57,
                want: true,
                off: -899997,
            },
            Op::JumpGtInt {
                a: 74,
                imm: 14,
                want: false,
                off: -799994,
            },
            Op::JumpGtInt {
                a: 111,
                imm: 85,
                want: true,
                off: -699991,
            },
            Op::JumpGeInt {
                a: 37,
                imm: -57,
                want: true,
                off: -899997,
            },
            Op::JumpGeInt {
                a: 74,
                imm: 14,
                want: false,
                off: -799994,
            },
            Op::JumpGeInt {
                a: 111,
                imm: 85,
                want: true,
                off: -699991,
            },
            Op::Call {
                func: 37,
                nargs: 53,
                nresults: 53,
            },
            Op::Call {
                func: 74,
                nargs: 106,
                nresults: 106,
            },
            Op::Call {
                func: 111,
                nargs: 159,
                nresults: 159,
            },
            Op::TailCall {
                func: 37,
                nargs: 53,
            },
            Op::TailCall {
                func: 74,
                nargs: 106,
            },
            Op::TailCall {
                func: 111,
                nargs: 159,
            },
            Op::Return {
                first: 37,
                count: 53,
            },
            Op::Return {
                first: 74,
                count: 106,
            },
            Op::Return {
                first: 111,
                count: 159,
            },
            Op::Return0,
            Op::Return0,
            Op::Return0,
            Op::Return1 { src: 37 },
            Op::Return1 { src: 74 },
            Op::Return1 { src: 111 },
            Op::Vararg { dst: 37, count: 53 },
            Op::Vararg {
                dst: 74,
                count: 106,
            },
            Op::Vararg {
                dst: 111,
                count: 159,
            },
            Op::VarargTable { dst: 37 },
            Op::VarargTable { dst: 74 },
            Op::VarargTable { dst: 111 },
            Op::VarargGet { dst: 37, key: 37 },
            Op::VarargGet { dst: 74, key: 74 },
            Op::VarargGet { dst: 111, key: 111 },
            Op::VarargLen { dst: 37 },
            Op::VarargLen { dst: 74 },
            Op::VarargLen { dst: 111 },
            Op::VarargGetInt { dst: 37, index: 53 },
            Op::VarargGetInt {
                dst: 74,
                index: 106,
            },
            Op::VarargGetInt {
                dst: 111,
                index: 159,
            },
            Op::VarargUnpack {
                dst: 37,
                table: 37,
                count: 53,
            },
            Op::VarargUnpack {
                dst: 74,
                table: 74,
                count: 106,
            },
            Op::VarargUnpack {
                dst: 111,
                table: 111,
                count: 159,
            },
            Op::ForPrep {
                base: 37,
                exit: -899997,
            },
            Op::ForPrep {
                base: 74,
                exit: -799994,
            },
            Op::ForPrep {
                base: 111,
                exit: -699991,
            },
            Op::ForPrepExclusive {
                base: 37,
                exit: -899997,
            },
            Op::ForPrepExclusive {
                base: 74,
                exit: -799994,
            },
            Op::ForPrepExclusive {
                base: 111,
                exit: -699991,
            },
            Op::ForLoop {
                base: 37,
                back: -899997,
            },
            Op::ForLoop {
                base: 74,
                back: -799994,
            },
            Op::ForLoop {
                base: 111,
                back: -699991,
            },
            Op::GenericCall {
                base: 37,
                nvars: 53,
            },
            Op::GenericCall {
                base: 74,
                nvars: 106,
            },
            Op::GenericCall {
                base: 111,
                nvars: 159,
            },
            Op::GenericLoop {
                base: 37,
                back: -899997,
            },
            Op::GenericLoop {
                base: 74,
                back: -799994,
            },
            Op::GenericLoop {
                base: 111,
                back: -699991,
            },
            Op::Closure {
                dst: 37,
                proto: 999,
            },
            Op::Closure {
                dst: 74,
                proto: 1998,
            },
            Op::Closure {
                dst: 111,
                proto: 2997,
            },
            Op::Tbc { reg: 37 },
            Op::Tbc { reg: 74 },
            Op::Tbc { reg: 111 },
            Op::Close { from: 37 },
            Op::Close { from: 74 },
            Op::Close { from: 111 },
            Op::NativeEntry { id: 100003 },
            Op::NativeEntry { id: 200006 },
            Op::NativeEntry { id: 300009 },
        ];
        for op in ops {
            assert_eq!(op.encode().decode(), op, "{op:?}");
        }
        assert_eq!(std::mem::size_of::<super::Insn>(), 8);
    }
}
