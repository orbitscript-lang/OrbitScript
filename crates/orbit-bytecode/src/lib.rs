//! The register bytecode shared by the compiler and the VM (ADR-4).
//!
//! The instruction set and its 32-bit encoding follow Lua 5.5's `lopcodes.h` exactly, so
//! the code generator and the interpreter can be ported from the reference faithfully,
//! and so can the debug machinery that reconstructs variable names from bytecode for
//! error messages ("attempt to index a nil value (global 'x')"). The format is our own:
//! `string.dump` writes it, and PUC binary chunks are not accepted.
//!
//! ```text
//!           3 3 2 2 2 2 2 2 2 2 2 2 1 1 1 1 1 1 1 1 1 1 0 0 0 0 0 0 0 0 0 0
//!           1 0 9 8 7 6 5 4 3 2 1 0 9 8 7 6 5 4 3 2 1 0 9 8 7 6 5 4 3 2 1 0
//! iABC          C(8)     |      B(8)     |k|     A(8)      |   Op(7)     |
//! ivABC         vC(10)     |     vB(6)   |k|     A(8)      |   Op(7)     |
//! iABx                Bx(17)               |     A(8)      |   Op(7)     |
//! iAsBx              sBx (signed)(17)      |     A(8)      |   Op(7)     |
//! iAx                           Ax(25)                     |   Op(7)     |
//! isJ                           sJ (signed)(25)            |   Op(7)     |
//! ```
//!
//! [`FnInfo`] holds everything about a compiled function that is not a garbage-collected
//! value: code, line numbers, upvalue descriptors and local-variable debug information.
//! Constants and nested prototypes live in the heap's `Proto` object, which points at
//! its `FnInfo`.

use std::fmt;

/// An encoded instruction.
pub type Instruction = u32;

/// Instruction formats (`OpMode`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpMode {
    /// `A B C k`
    ABC,
    /// `A vB vC k`
    VABC,
    /// `A Bx`
    ABx,
    /// `A sBx`
    AsBx,
    /// `Ax`
    Ax,
    /// `sJ k`
    SJ,
}

macro_rules! opcodes {
    ($( $name:ident = $mode:ident, $a:literal, $t:literal, $mm:literal, $str:literal; )*) => {
        /// Opcodes, in the order of Lua 5.5's `lopcodes.h`.
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
        #[repr(u8)]
        pub enum OpCode {
            $(
                #[doc = $str]
                $name,
            )*
        }

        /// Every opcode, indexed by its number.
        pub const OPCODES: &[OpCode] = &[$(OpCode::$name,)*];

        const PROPS: &[(OpMode, bool, bool, bool, &str)] = &[$((OpMode::$mode, $a, $t, $mm, $str),)*];
    };
}

// name = mode, sets A, is test, calls metamethod, name
opcodes! {
    Move = ABC, true, false, false, "MOVE";
    LoadI = AsBx, true, false, false, "LOADI";
    LoadF = AsBx, true, false, false, "LOADF";
    LoadK = ABx, true, false, false, "LOADK";
    LoadKX = ABx, true, false, false, "LOADKX";
    LoadFalse = ABC, true, false, false, "LOADFALSE";
    LFalseSkip = ABC, true, false, false, "LFALSESKIP";
    LoadTrue = ABC, true, false, false, "LOADTRUE";
    LoadNil = ABC, true, false, false, "LOADNIL";
    GetUpval = ABC, true, false, false, "GETUPVAL";
    SetUpval = ABC, false, false, false, "SETUPVAL";
    GetTabUp = ABC, true, false, false, "GETTABUP";
    GetTable = ABC, true, false, false, "GETTABLE";
    GetI = ABC, true, false, false, "GETI";
    GetField = ABC, true, false, false, "GETFIELD";
    SetTabUp = ABC, false, false, false, "SETTABUP";
    SetTable = ABC, false, false, false, "SETTABLE";
    SetI = ABC, false, false, false, "SETI";
    SetField = ABC, false, false, false, "SETFIELD";
    NewTable = VABC, true, false, false, "NEWTABLE";
    SelfOp = ABC, true, false, false, "SELF";
    AddI = ABC, true, false, false, "ADDI";
    AddK = ABC, true, false, false, "ADDK";
    SubK = ABC, true, false, false, "SUBK";
    MulK = ABC, true, false, false, "MULK";
    ModK = ABC, true, false, false, "MODK";
    PowK = ABC, true, false, false, "POWK";
    DivK = ABC, true, false, false, "DIVK";
    IDivK = ABC, true, false, false, "IDIVK";
    BAndK = ABC, true, false, false, "BANDK";
    BOrK = ABC, true, false, false, "BORK";
    BXorK = ABC, true, false, false, "BXORK";
    ShlI = ABC, true, false, false, "SHLI";
    ShrI = ABC, true, false, false, "SHRI";
    Add = ABC, true, false, false, "ADD";
    Sub = ABC, true, false, false, "SUB";
    Mul = ABC, true, false, false, "MUL";
    Mod = ABC, true, false, false, "MOD";
    Pow = ABC, true, false, false, "POW";
    Div = ABC, true, false, false, "DIV";
    IDiv = ABC, true, false, false, "IDIV";
    BAnd = ABC, true, false, false, "BAND";
    BOr = ABC, true, false, false, "BOR";
    BXor = ABC, true, false, false, "BXOR";
    Shl = ABC, true, false, false, "SHL";
    Shr = ABC, true, false, false, "SHR";
    MmBin = ABC, false, false, true, "MMBIN";
    MmBinI = ABC, false, false, true, "MMBINI";
    MmBinK = ABC, false, false, true, "MMBINK";
    Unm = ABC, true, false, false, "UNM";
    BNot = ABC, true, false, false, "BNOT";
    Not = ABC, true, false, false, "NOT";
    Len = ABC, true, false, false, "LEN";
    Concat = ABC, true, false, false, "CONCAT";
    Close = ABC, false, false, false, "CLOSE";
    Tbc = ABC, false, false, false, "TBC";
    Jmp = SJ, false, false, false, "JMP";
    Eq = ABC, false, true, false, "EQ";
    Lt = ABC, false, true, false, "LT";
    Le = ABC, false, true, false, "LE";
    EqK = ABC, false, true, false, "EQK";
    EqI = ABC, false, true, false, "EQI";
    LtI = ABC, false, true, false, "LTI";
    LeI = ABC, false, true, false, "LEI";
    GtI = ABC, false, true, false, "GTI";
    GeI = ABC, false, true, false, "GEI";
    Test = ABC, false, true, false, "TEST";
    TestSet = ABC, true, true, false, "TESTSET";
    Call = ABC, true, false, false, "CALL";
    TailCall = ABC, true, false, false, "TAILCALL";
    Return = ABC, false, false, false, "RETURN";
    Return0 = ABC, false, false, false, "RETURN0";
    Return1 = ABC, false, false, false, "RETURN1";
    ForLoop = ABx, true, false, false, "FORLOOP";
    ForPrep = ABx, true, false, false, "FORPREP";
    TForPrep = ABx, false, false, false, "TFORPREP";
    TForCall = ABC, false, false, false, "TFORCALL";
    TForLoop = ABx, true, false, false, "TFORLOOP";
    SetList = VABC, false, false, false, "SETLIST";
    Closure = ABx, true, false, false, "CLOSURE";
    VarArg = ABC, true, false, false, "VARARG";
    GetVarg = ABC, true, false, false, "GETVARG";
    ErrNNil = ABx, false, false, false, "ERRNNIL";
    VarArgPrep = ABC, false, false, false, "VARARGPREP";
    ExtraArg = Ax, false, false, false, "EXTRAARG";
}

/// Number of opcodes.
pub const NUM_OPCODES: usize = OPCODES.len();

impl OpCode {
    /// The opcode of an instruction.
    #[inline(always)]
    pub fn of(i: Instruction) -> OpCode {
        OPCODES[(i & 0x7f) as usize]
    }

    /// The instruction format.
    pub fn mode(self) -> OpMode {
        PROPS[self as usize].0
    }

    /// Whether the instruction sets register A (`testAMode`).
    pub fn sets_a(self) -> bool {
        PROPS[self as usize].1
    }

    /// Whether the instruction is a test whose next instruction is a jump (`testTMode`).
    pub fn is_test(self) -> bool {
        PROPS[self as usize].2
    }

    /// Whether the instruction calls a metamethod (`testMMMode`).
    pub fn is_mm(self) -> bool {
        PROPS[self as usize].3
    }

    /// The reference's name for the opcode, such as `GETFIELD`.
    pub fn name(self) -> &'static str {
        PROPS[self as usize].4
    }
}

impl fmt::Display for OpCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

// ---- operand encoding -------------------------------------------------------

pub const SIZE_C: u32 = 8;
pub const SIZE_VC: u32 = 10;
pub const SIZE_B: u32 = 8;
pub const SIZE_VB: u32 = 6;
pub const SIZE_BX: u32 = SIZE_C + SIZE_B + 1;
pub const SIZE_A: u32 = 8;
pub const SIZE_AX: u32 = SIZE_BX + SIZE_A;
pub const SIZE_SJ: u32 = SIZE_BX + SIZE_A;
pub const SIZE_OP: u32 = 7;

pub const POS_OP: u32 = 0;
pub const POS_A: u32 = POS_OP + SIZE_OP;
pub const POS_K: u32 = POS_A + SIZE_A;
pub const POS_B: u32 = POS_K + 1;
pub const POS_VB: u32 = POS_K + 1;
pub const POS_C: u32 = POS_B + SIZE_B;
pub const POS_VC: u32 = POS_VB + SIZE_VB;
pub const POS_BX: u32 = POS_K;
pub const POS_AX: u32 = POS_A;
pub const POS_SJ: u32 = POS_A;

pub const MAXARG_BX: i32 = (1 << SIZE_BX) - 1;
pub const OFFSET_SBX: i32 = MAXARG_BX >> 1;
pub const MAXARG_AX: i32 = (1 << SIZE_AX) - 1;
pub const MAXARG_SJ: i32 = (1 << SIZE_SJ) - 1;
pub const OFFSET_SJ: i32 = MAXARG_SJ >> 1;
pub const MAXARG_A: i32 = (1 << SIZE_A) - 1;
pub const MAXARG_B: i32 = (1 << SIZE_B) - 1;
pub const MAXARG_VB: i32 = (1 << SIZE_VB) - 1;
pub const MAXARG_C: i32 = (1 << SIZE_C) - 1;
pub const MAXARG_VC: i32 = (1 << SIZE_VC) - 1;
pub const OFFSET_SC: i32 = MAXARG_C >> 1;

/// Maximum number of registers of a Lua function (`MAX_FSTACK`).
pub const MAX_FSTACK: i32 = MAXARG_A;
/// An invalid register (`NO_REG`).
pub const NO_REG: i32 = MAX_FSTACK;

/// Converts a signed value to its excess-K `sC` encoding.
#[inline(always)]
pub const fn int2sc(i: i32) -> i32 {
    i + OFFSET_SC
}

/// Decodes an excess-K `sB`/`sC` operand.
#[inline(always)]
pub const fn sc2int(i: i32) -> i32 {
    i - OFFSET_SC
}

#[inline(always)]
const fn mask1(n: u32, p: u32) -> u32 {
    (!((!0u32) << n)) << p
}

#[inline(always)]
const fn getarg(i: Instruction, pos: u32, size: u32) -> i32 {
    ((i >> pos) & mask1(size, 0)) as i32
}

#[inline(always)]
fn setarg(i: &mut Instruction, v: i32, pos: u32, size: u32) {
    *i = (*i & !mask1(size, pos)) | (((v as u32) << pos) & mask1(size, pos));
}

/// Operand A.
#[inline(always)]
pub const fn arg_a(i: Instruction) -> i32 {
    getarg(i, POS_A, SIZE_A)
}
/// Operand B.
#[inline(always)]
pub const fn arg_b(i: Instruction) -> i32 {
    getarg(i, POS_B, SIZE_B)
}
/// Operand vB (`ivABC` format).
#[inline(always)]
pub const fn arg_vb(i: Instruction) -> i32 {
    getarg(i, POS_VB, SIZE_VB)
}
/// Operand sB (signed B).
#[inline(always)]
pub const fn arg_sb(i: Instruction) -> i32 {
    sc2int(arg_b(i))
}
/// Operand C.
#[inline(always)]
pub const fn arg_c(i: Instruction) -> i32 {
    getarg(i, POS_C, SIZE_C)
}
/// Operand vC (`ivABC` format).
#[inline(always)]
pub const fn arg_vc(i: Instruction) -> i32 {
    getarg(i, POS_VC, SIZE_VC)
}
/// Operand sC (signed C).
#[inline(always)]
pub const fn arg_sc(i: Instruction) -> i32 {
    sc2int(arg_c(i))
}
/// Flag k.
#[inline(always)]
pub const fn arg_k(i: Instruction) -> bool {
    i & (1 << POS_K) != 0
}
/// Operand Bx.
#[inline(always)]
pub const fn arg_bx(i: Instruction) -> i32 {
    getarg(i, POS_BX, SIZE_BX)
}
/// Operand sBx.
#[inline(always)]
pub const fn arg_sbx(i: Instruction) -> i32 {
    getarg(i, POS_BX, SIZE_BX) - OFFSET_SBX
}
/// Operand Ax.
#[inline(always)]
pub const fn arg_ax(i: Instruction) -> i32 {
    getarg(i, POS_AX, SIZE_AX)
}
/// Operand sJ.
#[inline(always)]
pub const fn arg_sj(i: Instruction) -> i32 {
    getarg(i, POS_SJ, SIZE_SJ) - OFFSET_SJ
}

/// Sets the opcode.
pub fn set_opcode(i: &mut Instruction, op: OpCode) {
    setarg(i, op as i32, POS_OP, SIZE_OP);
}
/// Sets operand A.
pub fn set_a(i: &mut Instruction, v: i32) {
    setarg(i, v, POS_A, SIZE_A);
}
/// Sets operand B.
pub fn set_b(i: &mut Instruction, v: i32) {
    setarg(i, v, POS_B, SIZE_B);
}
/// Sets operand C.
pub fn set_c(i: &mut Instruction, v: i32) {
    setarg(i, v, POS_C, SIZE_C);
}
/// Sets flag k.
pub fn set_k(i: &mut Instruction, v: bool) {
    setarg(i, v as i32, POS_K, 1);
}
/// Sets operand Bx.
pub fn set_bx(i: &mut Instruction, v: i32) {
    setarg(i, v, POS_BX, SIZE_BX);
}
/// Sets operand sJ.
pub fn set_sj(i: &mut Instruction, j: i32) {
    setarg(i, j + OFFSET_SJ, POS_SJ, SIZE_SJ);
}

/// Encodes an `iABC` instruction.
pub const fn create_abck(o: OpCode, a: i32, b: i32, c: i32, k: bool) -> Instruction {
    ((o as u32) << POS_OP)
        | ((a as u32) << POS_A)
        | ((b as u32) << POS_B)
        | ((c as u32) << POS_C)
        | ((k as u32) << POS_K)
}

/// Encodes an `ivABC` instruction.
pub const fn create_vabck(o: OpCode, a: i32, b: i32, c: i32, k: bool) -> Instruction {
    ((o as u32) << POS_OP)
        | ((a as u32) << POS_A)
        | ((b as u32) << POS_VB)
        | ((c as u32) << POS_VC)
        | ((k as u32) << POS_K)
}

/// Encodes an `iABx` instruction.
pub const fn create_abx(o: OpCode, a: i32, bx: i32) -> Instruction {
    ((o as u32) << POS_OP) | ((a as u32) << POS_A) | ((bx as u32) << POS_BX)
}

/// Encodes an `iAx` instruction.
pub const fn create_ax(o: OpCode, ax: i32) -> Instruction {
    ((o as u32) << POS_OP) | ((ax as u32) << POS_AX)
}

/// Encodes an `isJ` instruction; `j` is already in excess-K form.
pub const fn create_sj(o: OpCode, j: i32, k: bool) -> Instruction {
    ((o as u32) << POS_OP) | ((j as u32) << POS_SJ) | ((k as u32) << POS_K)
}

/// Whether an instruction uses the stack top set by the previous one (`luaP_isIT`).
pub fn is_it(i: Instruction) -> bool {
    match OpCode::of(i) {
        OpCode::SetList => arg_vb(i) == 0,
        OpCode::Call | OpCode::TailCall => arg_b(i) == 0,
        OpCode::Return => arg_b(i) == 0,
        _ => false,
    }
}

/// Whether an instruction leaves multiple results up to the stack top (`luaP_isOT`).
pub fn is_ot(i: Instruction) -> bool {
    match OpCode::of(i) {
        OpCode::TailCall => true,
        OpCode::Call | OpCode::VarArg => arg_c(i) == 0,
        _ => false,
    }
}

// ---- function information ---------------------------------------------------

/// Prototype flag: the function has hidden vararg arguments (`PF_VAHID`).
pub const PF_VAHID: u8 = 1;
/// Prototype flag: the function has a vararg table (`PF_VATAB`).
pub const PF_VATAB: u8 = 2;

/// Kinds of variables, as recorded in upvalue descriptors (`VDKREG` and friends).
pub mod var_kind {
    /// A regular local.
    pub const REG: u8 = 0;
    /// A `<const>` local.
    pub const CONST: u8 = 1;
    /// A named vararg parameter.
    pub const VAVAR: u8 = 2;
    /// A `<close>` local.
    pub const TOCLOSE: u8 = 3;
}

/// Describes an upvalue of a function (`Upvaldesc`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UpvalDesc {
    /// The variable name (debug information).
    pub name: Box<str>,
    /// True if it captures a register of the enclosing function, false if it copies an
    /// upvalue of the enclosing function.
    pub in_stack: bool,
    /// The register or upvalue index in the enclosing function.
    pub index: u8,
    /// The kind of variable ([`var_kind`]).
    pub kind: u8,
}

/// Debug information for a local variable (`LocVar`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocVar {
    /// Its name.
    pub name: Box<str>,
    /// First instruction where the variable is active.
    pub start_pc: u32,
    /// First instruction where the variable is dead.
    pub end_pc: u32,
}

/// Everything about a compiled function that isn't a GC value.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FnInfo {
    /// The instructions.
    pub code: Box<[Instruction]>,
    /// Source line of each instruction.
    pub lines: Box<[u32]>,
    /// Upvalue descriptors.
    pub upvalues: Box<[UpvalDesc]>,
    /// Local variable debug information, in declaration order.
    pub locvars: Box<[LocVar]>,
    /// Number of fixed parameters (including `self`).
    pub num_params: u8,
    /// [`PF_VAHID`] and [`PF_VATAB`].
    pub flags: u8,
    /// Registers needed by the function.
    pub max_stack: u8,
    /// Line where the definition starts (0 for a main chunk).
    pub line_defined: u32,
    /// Line where the definition ends (0 for a main chunk).
    pub last_line_defined: u32,
}

impl FnInfo {
    /// Whether the function is variadic.
    pub fn is_vararg(&self) -> bool {
        self.flags & (PF_VAHID | PF_VATAB) != 0
    }

    /// The name of the `n`-th (1-based) local variable active at instruction `pc`
    /// (`luaF_getlocalname`).
    pub fn local_name(&self, mut n: u32, pc: u32) -> Option<&str> {
        for v in self.locvars.iter() {
            if v.start_pc > pc {
                break;
            }
            if pc < v.end_pc {
                n -= 1;
                if n == 0 {
                    return Some(&v.name);
                }
            }
        }
        None
    }

    /// The line of instruction `pc`, or `None` without line information.
    pub fn line(&self, pc: usize) -> Option<u32> {
        self.lines.get(pc).copied()
    }
}

/// A human-readable listing of an instruction, for tests and debugging.
pub fn disassemble(i: Instruction) -> String {
    let op = OpCode::of(i);
    let (a, k) = (arg_a(i), arg_k(i));
    let k = if k { "k" } else { "" };
    match op.mode() {
        OpMode::ABC => format!(
            "{:<10} {a} {} {}{}",
            op.name(),
            arg_b(i),
            arg_c(i),
            if k.is_empty() { "" } else { " k" }
        ),
        OpMode::VABC => format!(
            "{:<10} {a} {} {}{}",
            op.name(),
            arg_vb(i),
            arg_vc(i),
            if k.is_empty() { "" } else { " k" }
        ),
        OpMode::ABx => format!("{:<10} {a} {}", op.name(), arg_bx(i)),
        OpMode::AsBx => format!("{:<10} {a} {}", op.name(), arg_sbx(i)),
        OpMode::Ax => format!("{:<10} {}", op.name(), arg_ax(i)),
        OpMode::SJ => format!("{:<10} {}", op.name(), arg_sj(i)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opcode_numbers_match_lua() {
        assert_eq!(NUM_OPCODES, 85);
        assert_eq!(OpCode::GetTabUp as u8, 11);
        assert_eq!(OpCode::MmBin as u8, 46);
        assert_eq!(OpCode::Jmp as u8, 56);
        assert_eq!(OpCode::ExtraArg as u8, 84);
        for (n, op) in OPCODES.iter().enumerate() {
            assert_eq!(*op as usize, n);
        }
    }

    #[test]
    fn round_trips() {
        let i = create_abck(OpCode::Add, 3, 200, 17, true);
        assert_eq!(OpCode::of(i), OpCode::Add);
        assert_eq!((arg_a(i), arg_b(i), arg_c(i), arg_k(i)), (3, 200, 17, true));
        let mut j = create_sj(OpCode::Jmp, OFFSET_SJ - 5, false);
        assert_eq!(arg_sj(j), -5);
        set_sj(&mut j, 1000);
        assert_eq!(arg_sj(j), 1000);
        let l = create_abx(OpCode::LoadI, 1, -7 + OFFSET_SBX);
        assert_eq!(arg_sbx(l), -7);
        let v = create_vabck(OpCode::NewTable, 2, 5, 1000, false);
        assert_eq!((arg_vb(v), arg_vc(v)), (5, 1000));
    }
}
