//! The code generator: `lcode.c` plus the code-emitting parts of `lparser.c`.
//!
//! Function and variable names follow the reference so the two can be read side by side
//! (`exp2anyreg` is `luaK_exp2anyreg`, and so on). Deviations are commented.

use crate::{CompiledFn, Constant};
use orbit_bytecode::*;
use orbit_lua_syntax::ast::*;
use orbit_lua_syntax::fold::{Arith, fold_arith};
use orbit_lua_syntax::{ErrorKind, Span, SyntaxError};
use std::collections::HashMap;

/// End of a jump list, and "no jump".
const NO_JUMP: i32 = -1;
/// `LUA_MULTRET`
const MULTRET: i32 = -1;
/// Maximum constant index usable as an RK operand (`MAXINDEXRK`).
const MAXINDEXRK: i32 = MAXARG_B;
/// Longest string that is a short string (`LUAI_MAXSHORTLEN`).
const MAX_SHORT_LEN: usize = 40;
/// Maximum number of items in a table constructor (`MAX_CNST`).
const MAX_CNST: i64 = i32::MAX as i64 / 2;

/// An expression descriptor (`expdesc`): a possibly delayed value, plus the jump lists
/// of the `and`/`or` operators that can also produce it.
#[derive(Clone, Copy, Debug)]
struct ExpDesc {
    k: Kind,
    /// Patch list of "exit when true".
    t: i32,
    /// Patch list of "exit when false".
    f: i32,
}

/// Expression kinds (`expkind`). Field meanings follow `lparser.h`.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Kind {
    /// No value (an empty expression list).
    Void,
    Nil,
    True,
    False,
    /// A constant in the constant table.
    K(i32),
    KFlt(f64),
    KInt(i64),
    /// A string constant, by index into the compiler's string pool.
    KStr(u32),
    /// The value is in a fixed register.
    NonReloc(i32),
    /// A local variable in a register.
    Local(i32),
    /// The named vararg parameter, in a register.
    VargVar(i32),
    /// An upvalue.
    Upval(i32),
    /// A compile-time constant local.
    Const(LocalId),
    /// `R[t][R[idx]]`
    Indexed {
        t: i32,
        idx: i32,
        keystr: i32,
    },
    /// `vararg[R[idx]]`
    VargInd {
        t: i32,
        idx: i32,
        keystr: i32,
    },
    /// `Upval[t][K[idx]]`
    IndexUp {
        t: i32,
        idx: i32,
        keystr: i32,
    },
    /// `R[t][idx]` for a small integer
    IndexI {
        t: i32,
        idx: i32,
    },
    /// `R[t][K[idx]]` for a short string
    IndexStr {
        t: i32,
        idx: i32,
        keystr: i32,
    },
    /// A comparison or test; the value is the pc of its jump.
    Jmp(i32),
    /// Can put its result in any register; the pc of its instruction.
    Reloc(i32),
    /// An open call; the pc of its instruction.
    Call(i32),
    /// `...`; the pc of its instruction.
    Vararg(i32),
}

impl Kind {
    fn is_indexed(self) -> bool {
        matches!(
            self,
            Kind::Indexed { .. }
                | Kind::VargInd { .. }
                | Kind::IndexUp { .. }
                | Kind::IndexI { .. }
                | Kind::IndexStr { .. }
        )
    }

    fn is_multret(self) -> bool {
        matches!(self, Kind::Call(_) | Kind::Vararg(_))
    }
}

impl ExpDesc {
    fn new(k: Kind) -> Self {
        ExpDesc {
            k,
            t: NO_JUMP,
            f: NO_JUMP,
        }
    }

    fn has_jumps(&self) -> bool {
        self.t != self.f
    }

    /// The register of a `NonReloc` expression.
    fn reg(&self) -> i32 {
        match self.k {
            Kind::NonReloc(r) => r,
            _ => unreachable!("expression not in a register: {:?}", self.k),
        }
    }

    /// The pc of a relocatable, call, vararg or jump expression.
    fn pc(&self) -> usize {
        match self.k {
            Kind::Reloc(pc) | Kind::Call(pc) | Kind::Vararg(pc) | Kind::Jmp(pc) => pc as usize,
            _ => unreachable!("expression has no instruction: {:?}", self.k),
        }
    }
}

/// Constant-table cache keys. Floats are keyed by their bits, which keeps them apart
/// from integers with the same value.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum KKey {
    Nil,
    Bool(bool),
    Int(i64),
    Float(u64),
    Str(u32),
}

/// An active variable declaration (`Vardesc`).
#[derive(Clone, Copy, Debug)]
struct VarDesc {
    /// Whether it lives in a register (not a compile-time constant or global).
    in_reg: bool,
    ridx: i32,
    /// Index in the function's `locvars`, or -1.
    pidx: i32,
}

/// A label or a pending goto (`Labeldesc`). A `None` name is the implicit label of a
/// `break`.
#[derive(Clone, Debug)]
struct LabelDesc {
    name: Option<Box<str>>,
    pc: i32,
    nactvar: usize,
    close: bool,
}

/// A block (`BlockCnt`).
#[derive(Clone, Copy, Debug)]
struct BlockCnt {
    firstlabel: usize,
    firstgoto: usize,
    nactvar: usize,
    upval: bool,
    /// 1 if the block is a loop, 2 if it also has pending breaks.
    isloop: u8,
    insidetbc: bool,
}

/// Per-function state (`FuncState` plus the parts of `Proto` it builds).
struct FuncState {
    code: Vec<Instruction>,
    lines: Vec<u32>,
    constants: Vec<Constant>,
    kcache: HashMap<KKey, i32>,
    protos: Vec<CompiledFn>,
    upvalues: Vec<UpvalDesc>,
    locvars: Vec<LocVar>,
    num_params: u8,
    flags: u8,
    max_stack: i32,
    line_defined: u32,
    last_line_defined: u32,
    lasttarget: i32,
    freereg: i32,
    actvar: Vec<VarDesc>,
    blocks: Vec<BlockCnt>,
    labels: Vec<LabelDesc>,
    gotos: Vec<LabelDesc>,
    needclose: bool,
}

impl FuncState {
    fn pc(&self) -> i32 {
        self.code.len() as i32
    }
}

struct Compiler<'a> {
    chunk: &'a Chunk,
    fs: Vec<FuncState>,
    /// Register of each local, by `LocalId`, once it is active.
    local_reg: Vec<i32>,
    /// The line given to new instructions (`ls->lastline`).
    line: u32,
    /// The highest line seen, for the main function's final `return`.
    max_line: u32,
    strings: Vec<Box<[u8]>>,
    string_ids: HashMap<Box<[u8]>, u32>,
    /// The first error found. Code generation carries on with harmless values, and the
    /// error is reported at the end.
    error: Option<SyntaxError>,
}

pub(crate) fn compile(chunk: &Chunk) -> Result<CompiledFn, SyntaxError> {
    let mut c = Compiler {
        chunk,
        fs: Vec::new(),
        local_reg: vec![-1; chunk.locals.len()],
        line: 1,
        max_line: 1,
        strings: Vec::new(),
        string_ids: HashMap::new(),
        error: None,
    };
    let f = c.main_func(&chunk.body);
    match c.error {
        Some(e) => Err(e),
        None => Ok(f),
    }
}

impl<'a> Compiler<'a> {
    // ---- errors -------------------------------------------------------------

    fn syntax_error(&mut self, msg: String) {
        if self.error.is_none() {
            self.error = Some(SyntaxError {
                kind: ErrorKind::Syntax,
                message: msg,
                line: self.line,
                near: None,
                span: Span::default(),
            });
        }
    }

    /// `luaY_checklimit`
    fn check_limit(&mut self, v: i64, l: i64, what: &str) {
        if v > l {
            let line = self.fs().line_defined;
            let where_ = if line == 0 {
                "main function".to_string()
            } else {
                format!("function at line {line}")
            };
            self.syntax_error(format!("too many {what} (limit is {l}) in {where_}"));
        }
    }

    // ---- small helpers ------------------------------------------------------

    fn fs(&self) -> &FuncState {
        self.fs.last().expect("a function is open")
    }

    fn fs_mut(&mut self) -> &mut FuncState {
        self.fs.last_mut().expect("a function is open")
    }

    /// An offset inside the token before the one at `offset`, for [`Chunk::line_before`].
    fn token_start_before(&self, offset: u32) -> u32 {
        let ends = &self.chunk.token_ends;
        let i = ends.partition_point(|&e| e <= offset);
        if i >= 1 { ends[i - 1] - 1 } else { 0 }
    }

    fn set_line(&mut self, line: u32) {
        if line > 0 {
            self.line = line;
            self.max_line = self.max_line.max(line);
        }
    }

    fn intern(&mut self, s: &[u8]) -> u32 {
        if let Some(&id) = self.string_ids.get(s) {
            return id;
        }
        let id = self.strings.len() as u32;
        self.strings.push(s.into());
        self.string_ids.insert(s.into(), id);
        id
    }

    fn local_info(&self, id: LocalId) -> &'a LocalInfo {
        self.chunk.local(id)
    }

    fn code_at(&mut self, pc: usize) -> &mut Instruction {
        &mut self.fs_mut().code[pc]
    }

    // ---- emitting (lcode.c) -------------------------------------------------

    /// `previousinstruction`: the last instruction, unless a jump target may lie
    /// between it and the current position.
    fn previous_instruction(&self) -> Option<usize> {
        let fs = self.fs();
        if fs.pc() > fs.lasttarget {
            Some(fs.code.len() - 1)
        } else {
            None
        }
    }

    /// `luaK_nil`
    fn nil(&mut self, mut from: i32, n: i32) {
        let mut l = from + n - 1;
        if let Some(p) = self.previous_instruction() {
            let prev = self.fs().code[p];
            if OpCode::of(prev) == OpCode::LoadNil {
                let pfrom = arg_a(prev);
                let pl = pfrom + arg_b(prev);
                if (pfrom <= from && from <= pl + 1) || (from <= pfrom && pfrom <= l + 1) {
                    if pfrom < from {
                        from = pfrom;
                    }
                    if pl > l {
                        l = pl;
                    }
                    let i = self.code_at(p);
                    set_a(i, from);
                    set_b(i, l - from);
                    return;
                }
            }
        }
        self.code_abc(OpCode::LoadNil, from, n - 1, 0);
    }

    /// `getjump`
    fn get_jump(&self, pc: i32) -> i32 {
        let offset = arg_sj(self.fs().code[pc as usize]);
        if offset == NO_JUMP {
            NO_JUMP
        } else {
            pc + 1 + offset
        }
    }

    /// `fixjump`
    fn fix_jump(&mut self, pc: i32, dest: i32) {
        let offset = dest - (pc + 1);
        if !(-OFFSET_SJ..=MAXARG_SJ - OFFSET_SJ).contains(&offset) {
            self.syntax_error("control structure too long".into());
            return;
        }
        set_sj(self.code_at(pc as usize), offset);
    }

    /// `luaK_concat`
    fn concat(&mut self, l1: &mut i32, l2: i32) {
        if l2 == NO_JUMP {
            return;
        }
        if *l1 == NO_JUMP {
            *l1 = l2;
        } else {
            let mut list = *l1;
            loop {
                let next = self.get_jump(list);
                if next == NO_JUMP {
                    break;
                }
                list = next;
            }
            self.fix_jump(list, l2);
        }
    }

    /// `luaK_jump`
    fn jump(&mut self) -> i32 {
        self.code_sj(OpCode::Jmp, NO_JUMP, false)
    }

    /// `luaK_ret`
    fn ret(&mut self, first: i32, nret: i32) {
        let op = match nret {
            0 => OpCode::Return0,
            1 => OpCode::Return1,
            _ => OpCode::Return,
        };
        self.check_limit(i64::from(nret) + 1, i64::from(MAXARG_B), "returns");
        self.code_abc(op, first, nret + 1, 0);
    }

    /// `condjump`
    fn cond_jump(&mut self, op: OpCode, a: i32, b: i32, c: i32, k: bool) -> i32 {
        self.code_abck(op, a, b, c, k);
        self.jump()
    }

    /// `luaK_getlabel`
    fn get_label(&mut self) -> i32 {
        let fs = self.fs_mut();
        fs.lasttarget = fs.pc();
        fs.pc()
    }

    /// `getjumpcontrol`: the position of the instruction controlling a jump.
    fn get_jump_control(&self, pc: i32) -> usize {
        let pc = pc as usize;
        if pc >= 1 && OpCode::of(self.fs().code[pc - 1]).is_test() {
            pc - 1
        } else {
            pc
        }
    }

    /// `patchtestreg`
    fn patch_test_reg(&mut self, node: i32, reg: i32) -> bool {
        let ctl = self.get_jump_control(node);
        let i = self.fs().code[ctl];
        if OpCode::of(i) != OpCode::TestSet {
            return false;
        }
        let code = self.code_at(ctl);
        if reg != NO_REG && reg != arg_b(i) {
            set_a(code, reg);
        } else {
            *code = create_abck(OpCode::Test, arg_b(i), 0, 0, arg_k(i));
        }
        true
    }

    /// `removevalues`
    fn remove_values(&mut self, mut list: i32) {
        while list != NO_JUMP {
            self.patch_test_reg(list, NO_REG);
            list = self.get_jump(list);
        }
    }

    /// `patchlistaux`
    fn patch_list_aux(&mut self, mut list: i32, vtarget: i32, reg: i32, dtarget: i32) {
        while list != NO_JUMP {
            let next = self.get_jump(list);
            if self.patch_test_reg(list, reg) {
                self.fix_jump(list, vtarget);
            } else {
                self.fix_jump(list, dtarget);
            }
            list = next;
        }
    }

    /// `luaK_patchlist`
    fn patch_list(&mut self, list: i32, target: i32) {
        self.patch_list_aux(list, target, NO_REG, target);
    }

    /// `luaK_patchtohere`
    fn patch_to_here(&mut self, list: i32) {
        let hr = self.get_label();
        self.patch_list(list, hr);
    }

    /// `removelastinstruction`
    fn remove_last_instruction(&mut self) {
        let fs = self.fs_mut();
        fs.code.pop();
        fs.lines.pop();
    }

    /// `luaK_code`
    fn code(&mut self, i: Instruction) -> i32 {
        let line = self.line;
        let fs = self.fs_mut();
        fs.code.push(i);
        fs.lines.push(line);
        fs.pc() - 1
    }

    fn code_abc(&mut self, o: OpCode, a: i32, b: i32, c: i32) -> i32 {
        self.code_abck(o, a, b, c, false)
    }

    /// `luaK_codeABCk`
    fn code_abck(&mut self, o: OpCode, a: i32, b: i32, c: i32, k: bool) -> i32 {
        self.code(create_abck(o, a, b, c, k))
    }

    /// `luaK_codevABCk`
    fn code_vabck(&mut self, o: OpCode, a: i32, b: i32, c: i32, k: bool) -> i32 {
        self.code(create_vabck(o, a, b, c, k))
    }

    /// `luaK_codeABx`
    fn code_abx(&mut self, o: OpCode, a: i32, bx: i32) -> i32 {
        self.code(create_abx(o, a, bx))
    }

    /// `codeAsBx`
    fn code_asbx(&mut self, o: OpCode, a: i32, sbx: i32) -> i32 {
        self.code(create_abx(o, a, sbx + OFFSET_SBX))
    }

    /// `codesJ`
    fn code_sj(&mut self, o: OpCode, sj: i32, k: bool) -> i32 {
        self.code(create_sj(o, sj + OFFSET_SJ, k))
    }

    /// `codeextraarg`
    fn code_extra_arg(&mut self, a: i32) -> i32 {
        self.code(create_ax(OpCode::ExtraArg, a))
    }

    /// `luaK_codek`
    fn code_k(&mut self, reg: i32, k: i32) -> i32 {
        if k <= MAXARG_BX {
            self.code_abx(OpCode::LoadK, reg, k)
        } else {
            let p = self.code_abx(OpCode::LoadKX, reg, 0);
            self.code_extra_arg(k);
            p
        }
    }

    /// `luaK_checkstack`
    fn check_stack(&mut self, n: i32) {
        let newstack = self.fs().freereg + n;
        if newstack > self.fs().max_stack {
            self.check_limit(i64::from(newstack), i64::from(MAX_FSTACK), "registers");
            self.fs_mut().max_stack = newstack.min(MAX_FSTACK);
        }
    }

    /// `luaK_reserveregs`
    fn reserve_regs(&mut self, n: i32) {
        self.check_stack(n);
        self.fs_mut().freereg += n;
    }

    /// `freereg`
    fn free_reg(&mut self, reg: i32) {
        if reg >= self.nvarstack() {
            self.fs_mut().freereg -= 1;
            debug_assert_eq!(reg, self.fs().freereg, "registers freed out of order");
        }
    }

    /// `freeregs`
    fn free_regs(&mut self, r1: i32, r2: i32) {
        if r1 > r2 {
            self.free_reg(r1);
            self.free_reg(r2);
        } else {
            self.free_reg(r2);
            self.free_reg(r1);
        }
    }

    /// `freeexp`
    fn free_exp(&mut self, e: &ExpDesc) {
        if let Kind::NonReloc(r) = e.k {
            self.free_reg(r);
        }
    }

    /// `freeexps`
    fn free_exps(&mut self, e1: &ExpDesc, e2: &ExpDesc) {
        let r1 = if let Kind::NonReloc(r) = e1.k { r } else { -1 };
        let r2 = if let Kind::NonReloc(r) = e2.k { r } else { -1 };
        self.free_regs(r1, r2);
    }

    // ---- constants ----------------------------------------------------------

    /// `addk`
    fn add_k(&mut self, v: Constant) -> i32 {
        let fs = self.fs_mut();
        fs.constants.push(v);
        fs.constants.len() as i32 - 1
    }

    /// `k2proto`
    fn k2proto(&mut self, key: KKey, v: Constant) -> i32 {
        if let Some(&k) = self.fs().kcache.get(&key) {
            return k;
        }
        let k = self.add_k(v);
        self.fs_mut().kcache.insert(key, k);
        k
    }

    /// `stringK`
    fn string_k(&mut self, s: u32) -> i32 {
        let bytes = self.strings[s as usize].clone();
        self.k2proto(KKey::Str(s), Constant::Str(bytes))
    }

    /// `luaK_intK`
    fn int_k(&mut self, n: i64) -> i32 {
        self.k2proto(KKey::Int(n), Constant::Int(n))
    }

    /// `luaK_numberK`
    fn number_k(&mut self, r: f64) -> i32 {
        self.k2proto(KKey::Float(r.to_bits()), Constant::Float(r))
    }

    fn bool_k(&mut self, b: bool) -> i32 {
        self.k2proto(KKey::Bool(b), Constant::Bool(b))
    }

    fn nil_k(&mut self) -> i32 {
        self.k2proto(KKey::Nil, Constant::Nil)
    }

    fn is_short_string_k(&self, k: i32) -> bool {
        matches!(&self.fs().constants[k as usize], Constant::Str(s) if s.len() <= MAX_SHORT_LEN)
    }

    /// `luaK_int`
    fn int(&mut self, reg: i32, i: i64) {
        if fits_bx(i) {
            self.code_asbx(OpCode::LoadI, reg, i as i32);
        } else {
            let k = self.int_k(i);
            self.code_k(reg, k);
        }
    }

    /// `luaK_float`
    fn float(&mut self, reg: i32, f: f64) {
        match float_to_int(f) {
            Some(fi) if fits_bx(fi) => {
                self.code_asbx(OpCode::LoadF, reg, fi as i32);
            }
            _ => {
                let k = self.number_k(f);
                self.code_k(reg, k);
            }
        }
    }

    /// The value of a compile-time constant local, as an expression kind.
    fn const_kind(&mut self, id: LocalId) -> Kind {
        match &self.local_info(id).kind {
            LocalKind::CompileTimeConst(c) => match c {
                Const::Nil => Kind::Nil,
                Const::Bool(true) => Kind::True,
                Const::Bool(false) => Kind::False,
                Const::Int(i) => Kind::KInt(*i),
                Const::Float(f) => Kind::KFlt(*f),
                Const::Str(s) => Kind::KStr(self.intern(s)),
            },
            _ => unreachable!("not a compile-time constant"),
        }
    }

    /// `luaK_codecheckglobal`
    fn code_check_global(&mut self, var: &mut ExpDesc, k: i32, line: u32) {
        self.exp2anyreg(var);
        self.fix_line(line);
        let k = if k >= MAXARG_BX { 0 } else { k + 1 };
        self.code_abx(OpCode::ErrNNil, var.reg(), k);
        self.fix_line(line);
        self.free_exp(var);
    }

    // ---- expressions to registers ----------------------------------------------

    /// `luaK_setreturns`
    fn set_returns(&mut self, e: &mut ExpDesc, nresults: i32) {
        self.check_limit(
            i64::from(nresults) + 1,
            i64::from(MAXARG_C),
            "multiple results",
        );
        let pc = e.pc();
        let freereg = self.fs().freereg;
        let i = self.code_at(pc);
        set_c(i, nresults + 1);
        if let Kind::Vararg(_) = e.k {
            set_a(i, freereg);
            self.reserve_regs(1);
        }
    }

    fn set_multret(&mut self, e: &mut ExpDesc) {
        self.set_returns(e, MULTRET);
    }

    /// `str2K`
    fn str2k(&mut self, e: &mut ExpDesc) -> i32 {
        let Kind::KStr(s) = e.k else {
            unreachable!("not a string constant")
        };
        let k = self.string_k(s);
        e.k = Kind::K(k);
        k
    }

    /// `luaK_setoneret`
    fn set_one_ret(&mut self, e: &mut ExpDesc) {
        match e.k {
            Kind::Call(pc) => {
                e.k = Kind::NonReloc(arg_a(self.fs().code[pc as usize]));
            }
            Kind::Vararg(pc) => {
                set_c(self.code_at(pc as usize), 2);
                e.k = Kind::Reloc(pc);
            }
            _ => {}
        }
    }

    /// `needvatab`
    fn need_vatab(&mut self) {
        self.fs_mut().flags |= PF_VATAB;
    }

    /// `luaK_dischargevars`
    fn discharge_vars(&mut self, e: &mut ExpDesc) {
        match e.k {
            Kind::Const(id) => e.k = self.const_kind(id),
            Kind::VargVar(r) => {
                self.need_vatab();
                e.k = Kind::NonReloc(r);
            }
            Kind::Local(r) => e.k = Kind::NonReloc(r),
            Kind::Upval(u) => {
                let pc = self.code_abc(OpCode::GetUpval, 0, u, 0);
                e.k = Kind::Reloc(pc);
            }
            Kind::IndexUp { t, idx, .. } => {
                let pc = self.code_abc(OpCode::GetTabUp, 0, t, idx);
                e.k = Kind::Reloc(pc);
            }
            Kind::IndexI { t, idx } => {
                self.free_reg(t);
                let pc = self.code_abc(OpCode::GetI, 0, t, idx);
                e.k = Kind::Reloc(pc);
            }
            Kind::IndexStr { t, idx, .. } => {
                self.free_reg(t);
                let pc = self.code_abc(OpCode::GetField, 0, t, idx);
                e.k = Kind::Reloc(pc);
            }
            Kind::Indexed { t, idx, .. } => {
                self.free_regs(t, idx);
                let pc = self.code_abc(OpCode::GetTable, 0, t, idx);
                e.k = Kind::Reloc(pc);
            }
            Kind::VargInd { t, idx, .. } => {
                self.free_regs(t, idx);
                let pc = self.code_abc(OpCode::GetVarg, 0, t, idx);
                e.k = Kind::Reloc(pc);
            }
            Kind::Vararg(_) | Kind::Call(_) => self.set_one_ret(e),
            _ => {}
        }
    }

    /// `discharge2reg`
    fn discharge2reg(&mut self, e: &mut ExpDesc, reg: i32) {
        self.discharge_vars(e);
        match e.k {
            Kind::Nil => self.nil(reg, 1),
            Kind::False => {
                self.code_abc(OpCode::LoadFalse, reg, 0, 0);
            }
            Kind::True => {
                self.code_abc(OpCode::LoadTrue, reg, 0, 0);
            }
            Kind::KStr(_) => {
                let k = self.str2k(e);
                self.code_k(reg, k);
            }
            Kind::K(k) => {
                self.code_k(reg, k);
            }
            Kind::KFlt(f) => self.float(reg, f),
            Kind::KInt(i) => self.int(reg, i),
            Kind::Reloc(pc) => set_a(self.code_at(pc as usize), reg),
            Kind::NonReloc(r) => {
                if reg != r {
                    self.code_abc(OpCode::Move, reg, r, 0);
                }
            }
            _ => {
                debug_assert!(matches!(e.k, Kind::Jmp(_)));
                return;
            }
        }
        e.k = Kind::NonReloc(reg);
    }

    /// `discharge2anyreg`
    fn discharge2anyreg(&mut self, e: &mut ExpDesc) {
        if !matches!(e.k, Kind::NonReloc(_)) {
            self.reserve_regs(1);
            let r = self.fs().freereg - 1;
            self.discharge2reg(e, r);
        }
    }

    /// `code_loadbool`
    fn code_loadbool(&mut self, a: i32, op: OpCode) -> i32 {
        self.get_label();
        self.code_abc(op, a, 0, 0)
    }

    /// `need_value`
    fn need_value(&self, mut list: i32) -> bool {
        while list != NO_JUMP {
            let i = self.fs().code[self.get_jump_control(list)];
            if OpCode::of(i) != OpCode::TestSet {
                return true;
            }
            list = self.get_jump(list);
        }
        false
    }

    /// `exp2reg`
    fn exp2reg(&mut self, e: &mut ExpDesc, reg: i32) {
        self.discharge2reg(e, reg);
        if let Kind::Jmp(pc) = e.k {
            let mut t = e.t;
            self.concat(&mut t, pc);
            e.t = t;
        }
        if e.has_jumps() {
            let mut p_f = NO_JUMP;
            let mut p_t = NO_JUMP;
            if self.need_value(e.t) || self.need_value(e.f) {
                let fj = if matches!(e.k, Kind::Jmp(_)) {
                    NO_JUMP
                } else {
                    self.jump()
                };
                p_f = self.code_loadbool(reg, OpCode::LFalseSkip);
                p_t = self.code_loadbool(reg, OpCode::LoadTrue);
                self.patch_to_here(fj);
            }
            let final_ = self.get_label();
            self.patch_list_aux(e.f, final_, reg, p_f);
            self.patch_list_aux(e.t, final_, reg, p_t);
        }
        e.f = NO_JUMP;
        e.t = NO_JUMP;
        e.k = Kind::NonReloc(reg);
    }

    /// `luaK_exp2nextreg`
    fn exp2nextreg(&mut self, e: &mut ExpDesc) {
        self.discharge_vars(e);
        self.free_exp(e);
        self.reserve_regs(1);
        let r = self.fs().freereg - 1;
        self.exp2reg(e, r);
    }

    /// `luaK_exp2anyreg`
    fn exp2anyreg(&mut self, e: &mut ExpDesc) -> i32 {
        self.discharge_vars(e);
        if let Kind::NonReloc(r) = e.k {
            if !e.has_jumps() {
                return r;
            }
            if r >= self.nvarstack() {
                self.exp2reg(e, r);
                return r;
            }
        }
        self.exp2nextreg(e);
        e.reg()
    }

    /// `luaK_exp2anyregup`
    fn exp2anyregup(&mut self, e: &mut ExpDesc) {
        if !matches!(e.k, Kind::Upval(_) | Kind::VargVar(_)) || e.has_jumps() {
            self.exp2anyreg(e);
        }
    }

    /// `luaK_exp2val`
    fn exp2val(&mut self, e: &mut ExpDesc) {
        if matches!(e.k, Kind::Jmp(_)) || e.has_jumps() {
            self.exp2anyreg(e);
        } else {
            self.discharge_vars(e);
        }
    }

    /// `luaK_exp2K`
    fn exp2k(&mut self, e: &mut ExpDesc) -> bool {
        if !e.has_jumps() {
            let info = match e.k {
                Kind::True => self.bool_k(true),
                Kind::False => self.bool_k(false),
                Kind::Nil => self.nil_k(),
                Kind::KInt(i) => self.int_k(i),
                Kind::KFlt(f) => self.number_k(f),
                Kind::KStr(s) => self.string_k(s),
                Kind::K(k) => k,
                _ => return false,
            };
            if info <= MAXINDEXRK {
                e.k = Kind::K(info);
                return true;
            }
        }
        false
    }

    /// `exp2RK`
    fn exp2rk(&mut self, e: &mut ExpDesc) -> bool {
        if self.exp2k(e) {
            true
        } else {
            self.exp2anyreg(e);
            false
        }
    }

    /// The register or constant index of an RK expression.
    fn rk_info(e: &ExpDesc) -> i32 {
        match e.k {
            Kind::K(k) => k,
            Kind::NonReloc(r) => r,
            _ => unreachable!("not an RK expression"),
        }
    }

    /// `codeABRK`
    fn code_abrk(&mut self, o: OpCode, a: i32, b: i32, ec: &mut ExpDesc) {
        let k = self.exp2rk(ec);
        let c = Self::rk_info(ec);
        self.code_abck(o, a, b, c, k);
    }

    /// `luaK_storevar`
    fn store_var(&mut self, var: &ExpDesc, ex: &mut ExpDesc) {
        match var.k {
            Kind::Local(r) => {
                self.free_exp(ex);
                self.exp2reg(ex, r);
                return;
            }
            Kind::Upval(u) => {
                let e = self.exp2anyreg(ex);
                self.code_abc(OpCode::SetUpval, e, u, 0);
            }
            Kind::IndexUp { t, idx, .. } => self.code_abrk(OpCode::SetTabUp, t, idx, ex),
            Kind::IndexI { t, idx } => self.code_abrk(OpCode::SetI, t, idx, ex),
            Kind::IndexStr { t, idx, .. } => self.code_abrk(OpCode::SetField, t, idx, ex),
            Kind::VargInd { t, idx, .. } => {
                self.need_vatab();
                self.code_abrk(OpCode::SetTable, t, idx, ex);
            }
            Kind::Indexed { t, idx, .. } => self.code_abrk(OpCode::SetTable, t, idx, ex),
            _ => unreachable!("invalid variable kind to store: {:?}", var.k),
        }
        self.free_exp(ex);
    }

    /// `negatecondition`
    fn negate_condition(&mut self, e: &ExpDesc) {
        let ctl = self.get_jump_control(e.pc() as i32);
        let i = self.code_at(ctl);
        let k = arg_k(*i);
        set_k(i, !k);
    }

    /// `jumponcond`
    fn jump_on_cond(&mut self, e: &mut ExpDesc, cond: bool) -> i32 {
        if let Kind::Reloc(pc) = e.k {
            let ie = self.fs().code[pc as usize];
            if OpCode::of(ie) == OpCode::Not {
                self.remove_last_instruction();
                return self.cond_jump(OpCode::Test, arg_b(ie), 0, 0, !cond);
            }
        }
        self.discharge2anyreg(e);
        self.free_exp(e);
        self.cond_jump(OpCode::TestSet, NO_REG, e.reg(), 0, cond)
    }

    /// `luaK_goiftrue`
    fn go_if_true(&mut self, e: &mut ExpDesc) {
        self.discharge_vars(e);
        let pc = match e.k {
            Kind::Jmp(pc) => {
                self.negate_condition(e);
                pc
            }
            Kind::K(_) | Kind::KFlt(_) | Kind::KInt(_) | Kind::KStr(_) | Kind::True => NO_JUMP,
            _ => self.jump_on_cond(e, false),
        };
        let mut f = e.f;
        self.concat(&mut f, pc);
        e.f = f;
        self.patch_to_here(e.t);
        e.t = NO_JUMP;
    }

    /// `luaK_goiffalse`
    fn go_if_false(&mut self, e: &mut ExpDesc) {
        self.discharge_vars(e);
        let pc = match e.k {
            Kind::Jmp(pc) => pc,
            Kind::Nil | Kind::False => NO_JUMP,
            _ => self.jump_on_cond(e, true),
        };
        let mut t = e.t;
        self.concat(&mut t, pc);
        e.t = t;
        self.patch_to_here(e.f);
        e.f = NO_JUMP;
    }

    /// `codenot`
    fn code_not(&mut self, e: &mut ExpDesc) {
        match e.k {
            Kind::Nil | Kind::False => e.k = Kind::True,
            Kind::K(_) | Kind::KFlt(_) | Kind::KInt(_) | Kind::KStr(_) | Kind::True => {
                e.k = Kind::False
            }
            Kind::Jmp(_) => self.negate_condition(e),
            Kind::Reloc(_) | Kind::NonReloc(_) => {
                self.discharge2anyreg(e);
                self.free_exp(e);
                let pc = self.code_abc(OpCode::Not, 0, e.reg(), 0);
                e.k = Kind::Reloc(pc);
            }
            _ => unreachable!("cannot negate {:?}", e.k),
        }
        std::mem::swap(&mut e.f, &mut e.t);
        self.remove_values(e.f);
        self.remove_values(e.t);
    }

    /// `isKstr`
    fn is_kstr(&self, e: &ExpDesc) -> bool {
        matches!(e.k, Kind::K(k) if !e.has_jumps() && k <= MAXINDEXRK && self.is_short_string_k(k))
    }

    /// `luaK_self`
    fn self_(&mut self, e: &mut ExpDesc, key: &mut ExpDesc) {
        self.exp2anyreg(e);
        let ereg = e.reg();
        self.free_exp(e);
        let base = self.fs().freereg;
        e.k = Kind::NonReloc(base);
        self.reserve_regs(2);
        let Kind::KStr(s) = key.k else {
            unreachable!("method names are strings")
        };
        if self.strings[s as usize].len() <= MAX_SHORT_LEN && self.exp2k(key) {
            self.code_abck(OpCode::SelfOp, base, ereg, Self::rk_info(key), false);
        } else {
            self.exp2anyreg(key);
            self.code_abc(OpCode::Move, base + 1, ereg, 0);
            self.code_abc(OpCode::GetTable, base, ereg, key.reg());
        }
        self.free_exp(key);
    }

    /// `luaK_indexed`
    fn indexed(&mut self, t: &mut ExpDesc, k: &mut ExpDesc) {
        let mut keystr = -1;
        if let Kind::KStr(_) = k.k {
            keystr = self.str2k(k);
        }
        if matches!(t.k, Kind::Upval(_)) && !self.is_kstr(k) {
            self.exp2anyreg(t);
        }
        t.k = match t.k {
            Kind::Upval(u) => Kind::IndexUp {
                t: u,
                idx: Self::rk_info(k),
                keystr,
            },
            Kind::VargVar(vreg) => {
                let kreg = self.exp2anyreg(k);
                Kind::VargInd {
                    t: vreg,
                    idx: kreg,
                    keystr,
                }
            }
            Kind::Local(r) | Kind::NonReloc(r) => {
                if self.is_kstr(k) {
                    Kind::IndexStr {
                        t: r,
                        idx: Self::rk_info(k),
                        keystr,
                    }
                } else if let Kind::KInt(i) = k.k
                    && !k.has_jumps()
                    && (i as u64) <= MAXARG_C as u64
                {
                    Kind::IndexI {
                        t: r,
                        idx: i as i32,
                    }
                } else {
                    let idx = self.exp2anyreg(k);
                    Kind::Indexed { t: r, idx, keystr }
                }
            }
            _ => unreachable!("cannot index {:?}", t.k),
        };
    }

    // ---- operators ----------------------------------------------------------

    /// `constfolding`
    fn const_folding(&mut self, op: Arith, e1: &mut ExpDesc, e2: &ExpDesc) -> bool {
        let (Some(v1), Some(v2)) = (to_numeral(e1), to_numeral(e2)) else {
            return false;
        };
        match fold_arith(op, &v1, &v2) {
            Some(Const::Int(i)) => {
                e1.k = Kind::KInt(i);
                true
            }
            Some(Const::Float(f)) => {
                e1.k = Kind::KFlt(f);
                true
            }
            _ => false,
        }
    }

    /// `codeunexpval`
    fn code_un_exp_val(&mut self, op: OpCode, e: &mut ExpDesc, line: u32) {
        let r = self.exp2anyreg(e);
        self.free_exp(e);
        let pc = self.code_abc(op, 0, r, 0);
        e.k = Kind::Reloc(pc);
        self.fix_line(line);
    }

    /// `finishbinexpval`
    #[allow(clippy::too_many_arguments)]
    fn finish_bin_exp_val(
        &mut self,
        e1: &mut ExpDesc,
        e2: &ExpDesc,
        op: OpCode,
        v2: i32,
        flip: bool,
        line: u32,
        mmop: OpCode,
        event: i32,
    ) {
        let v1 = self.exp2anyreg(e1);
        let pc = self.code_abck(op, 0, v1, v2, false);
        self.free_exps(e1, e2);
        e1.k = Kind::Reloc(pc);
        self.fix_line(line);
        self.code_abck(mmop, v1, v2, event, flip);
        self.fix_line(line);
    }

    /// `codebinexpval`
    fn code_bin_exp_val(&mut self, opr: BinOp, e1: &mut ExpDesc, e2: &mut ExpDesc, line: u32) {
        let op = arith_op(opr, OpCode::Add);
        let v2 = self.exp2anyreg(e2);
        self.finish_bin_exp_val(e1, e2, op, v2, false, line, OpCode::MmBin, tm_of(opr));
    }

    /// `codebini`
    #[allow(clippy::too_many_arguments)]
    fn code_bin_i(
        &mut self,
        op: OpCode,
        e1: &mut ExpDesc,
        e2: &ExpDesc,
        flip: bool,
        line: u32,
        event: i32,
    ) {
        let Kind::KInt(i) = e2.k else {
            unreachable!("immediate operand")
        };
        let v2 = int2sc(i as i32);
        self.finish_bin_exp_val(e1, e2, op, v2, flip, line, OpCode::MmBinI, event);
    }

    /// `codebinK`
    fn code_bin_k(&mut self, opr: BinOp, e1: &mut ExpDesc, e2: &ExpDesc, flip: bool, line: u32) {
        let event = tm_of(opr);
        let v2 = Self::rk_info(e2);
        let op = arith_op(opr, OpCode::AddK);
        self.finish_bin_exp_val(e1, e2, op, v2, flip, line, OpCode::MmBinK, event);
    }

    /// `finishbinexpneg`
    fn finish_bin_exp_neg(
        &mut self,
        e1: &mut ExpDesc,
        e2: &ExpDesc,
        op: OpCode,
        line: u32,
        event: i32,
    ) -> bool {
        let Kind::KInt(i2) = e2.k else {
            return false;
        };
        if e2.has_jumps() || !(fits_c(i2) && fits_c(i2.wrapping_neg())) {
            return false;
        }
        let v2 = i2 as i32;
        self.finish_bin_exp_val(e1, e2, op, int2sc(-v2), false, line, OpCode::MmBinI, event);
        let last = self.fs().code.len() - 1;
        set_b(self.code_at(last), int2sc(v2));
        true
    }

    /// `codebinNoK`
    fn code_bin_nok(
        &mut self,
        opr: BinOp,
        e1: &mut ExpDesc,
        e2: &mut ExpDesc,
        flip: bool,
        line: u32,
    ) {
        if flip {
            std::mem::swap(e1, e2);
        }
        self.code_bin_exp_val(opr, e1, e2, line);
    }

    /// `codearith`
    fn code_arith(
        &mut self,
        opr: BinOp,
        e1: &mut ExpDesc,
        e2: &mut ExpDesc,
        flip: bool,
        line: u32,
    ) {
        if to_numeral(e2).is_some() && self.exp2k(e2) {
            self.code_bin_k(opr, e1, e2, flip, line);
        } else {
            self.code_bin_nok(opr, e1, e2, flip, line);
        }
    }

    /// `codecommutative`
    fn code_commutative(&mut self, op: BinOp, e1: &mut ExpDesc, e2: &mut ExpDesc, line: u32) {
        let mut flip = false;
        if to_numeral(e1).is_some() {
            std::mem::swap(e1, e2);
            flip = true;
        }
        if op == BinOp::Add && is_scint(e2) {
            self.code_bin_i(OpCode::AddI, e1, e2, flip, line, tm_of(BinOp::Add));
        } else {
            self.code_arith(op, e1, e2, flip, line);
        }
    }

    /// `codebitwise`
    fn code_bitwise(&mut self, opr: BinOp, e1: &mut ExpDesc, e2: &mut ExpDesc, line: u32) {
        let mut flip = false;
        if let Kind::KInt(_) = e1.k {
            std::mem::swap(e1, e2);
            flip = true;
        }
        if matches!(e2.k, Kind::KInt(_)) && self.exp2k(e2) {
            self.code_bin_k(opr, e1, e2, flip, line);
        } else {
            self.code_bin_nok(opr, e1, e2, flip, line);
        }
    }

    /// `codeorder`. `opr` is `Lt` or `Le`.
    fn code_order(&mut self, opr: BinOp, e1: &mut ExpDesc, e2: &mut ExpDesc) {
        let (r1, r2, op);
        let mut isfloat = false;
        if let Some(im) = is_scnumber(e2, &mut isfloat) {
            r1 = self.exp2anyreg(e1);
            r2 = im;
            op = if opr == BinOp::Lt {
                OpCode::LtI
            } else {
                OpCode::LeI
            };
        } else if let Some(im) = is_scnumber(e1, &mut isfloat) {
            r1 = self.exp2anyreg(e2);
            r2 = im;
            op = if opr == BinOp::Lt {
                OpCode::GtI
            } else {
                OpCode::GeI
            };
        } else {
            r1 = self.exp2anyreg(e1);
            r2 = self.exp2anyreg(e2);
            op = if opr == BinOp::Lt {
                OpCode::Lt
            } else {
                OpCode::Le
            };
        }
        self.free_exps(e1, e2);
        let pc = self.cond_jump(op, r1, r2, i32::from(isfloat), true);
        e1.k = Kind::Jmp(pc);
    }

    /// `codeeq`
    fn code_eq(&mut self, opr: BinOp, e1: &mut ExpDesc, e2: &mut ExpDesc) {
        if !matches!(e1.k, Kind::NonReloc(_)) {
            std::mem::swap(e1, e2);
        }
        let r1 = self.exp2anyreg(e1);
        let (op, r2);
        // As in the reference, a float operand sets the flag even when it does not fit
        // an immediate operand.
        let mut isfloat = false;
        if let Some(im) = is_scnumber(e2, &mut isfloat) {
            op = OpCode::EqI;
            r2 = im;
        } else if self.exp2rk(e2) {
            op = OpCode::EqK;
            r2 = Self::rk_info(e2);
        } else {
            op = OpCode::Eq;
            r2 = self.exp2anyreg(e2);
        }
        self.free_exps(e1, e2);
        let pc = self.cond_jump(op, r1, r2, i32::from(isfloat), opr == BinOp::Eq);
        e1.k = Kind::Jmp(pc);
    }

    /// `luaK_prefix`
    fn prefix(&mut self, op: UnOp, e: &mut ExpDesc, line: u32) {
        let ef = ExpDesc::new(Kind::KInt(0));
        self.discharge_vars(e);
        match op {
            UnOp::Neg | UnOp::BNot => {
                let arith = if op == UnOp::Neg {
                    Arith::Unm
                } else {
                    Arith::BNot
                };
                if !self.const_folding(arith, e, &ef) {
                    let opc = if op == UnOp::Neg {
                        OpCode::Unm
                    } else {
                        OpCode::BNot
                    };
                    self.code_un_exp_val(opc, e, line);
                }
            }
            UnOp::Len => self.code_un_exp_val(OpCode::Len, e, line),
            UnOp::Not => self.code_not(e),
        }
    }

    /// `luaK_infix`
    fn infix(&mut self, op: BinOp, v: &mut ExpDesc) {
        self.discharge_vars(v);
        match op {
            BinOp::And => self.go_if_true(v),
            BinOp::Or => self.go_if_false(v),
            BinOp::Concat => self.exp2nextreg(v),
            BinOp::Add
            | BinOp::Sub
            | BinOp::Mul
            | BinOp::Div
            | BinOp::IDiv
            | BinOp::Mod
            | BinOp::Pow
            | BinOp::BAnd
            | BinOp::BOr
            | BinOp::BXor
            | BinOp::Shl
            | BinOp::Shr => {
                if to_numeral(v).is_none() {
                    self.exp2anyreg(v);
                }
            }
            BinOp::Eq | BinOp::Ne => {
                if to_numeral(v).is_none() {
                    self.exp2rk(v);
                }
            }
            BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => {
                if is_scnumber(v, &mut false).is_none() {
                    self.exp2anyreg(v);
                }
            }
        }
    }

    /// `codeconcat`
    fn code_concat(&mut self, e1: &mut ExpDesc, e2: &ExpDesc, line: u32) {
        if let Some(p) = self.previous_instruction() {
            let ie2 = self.fs().code[p];
            if OpCode::of(ie2) == OpCode::Concat {
                let n = arg_b(ie2);
                self.free_exp(e2);
                let r = e1.reg();
                let i = self.code_at(p);
                set_a(i, r);
                set_b(i, n + 1);
                return;
            }
        }
        self.code_abc(OpCode::Concat, e1.reg(), 2, 0);
        self.free_exp(e2);
        self.fix_line(line);
    }

    /// `luaK_posfix`
    fn posfix(&mut self, mut opr: BinOp, e1: &mut ExpDesc, e2: &mut ExpDesc, line: u32) {
        self.discharge_vars(e2);
        if let Some(arith) = fold_op(opr)
            && self.const_folding(arith, e1, e2)
        {
            return;
        }
        match opr {
            BinOp::And => {
                debug_assert_eq!(e1.t, NO_JUMP);
                let mut f = e2.f;
                self.concat(&mut f, e1.f);
                e2.f = f;
                *e1 = *e2;
            }
            BinOp::Or => {
                debug_assert_eq!(e1.f, NO_JUMP);
                let mut t = e2.t;
                self.concat(&mut t, e1.t);
                e2.t = t;
                *e1 = *e2;
            }
            BinOp::Concat => {
                self.exp2nextreg(e2);
                self.code_concat(e1, e2, line);
            }
            BinOp::Add | BinOp::Mul => self.code_commutative(opr, e1, e2, line),
            BinOp::Sub => {
                if !self.finish_bin_exp_neg(e1, e2, OpCode::AddI, line, tm_of(BinOp::Sub)) {
                    self.code_arith(opr, e1, e2, false, line);
                }
            }
            BinOp::Div | BinOp::IDiv | BinOp::Mod | BinOp::Pow => {
                self.code_arith(opr, e1, e2, false, line)
            }
            BinOp::BAnd | BinOp::BOr | BinOp::BXor => self.code_bitwise(opr, e1, e2, line),
            BinOp::Shl => {
                if is_scint(e1) {
                    std::mem::swap(e1, e2);
                    self.code_bin_i(OpCode::ShlI, e1, e2, true, line, tm_of(BinOp::Shl));
                } else if self.finish_bin_exp_neg(e1, e2, OpCode::ShrI, line, tm_of(BinOp::Shl)) {
                    // coded as (r1 >> -I)
                } else {
                    self.code_bin_exp_val(opr, e1, e2, line);
                }
            }
            BinOp::Shr => {
                if is_scint(e2) {
                    self.code_bin_i(OpCode::ShrI, e1, e2, false, line, tm_of(BinOp::Shr));
                } else {
                    self.code_bin_exp_val(opr, e1, e2, line);
                }
            }
            BinOp::Eq | BinOp::Ne => self.code_eq(opr, e1, e2),
            BinOp::Gt | BinOp::Ge | BinOp::Lt | BinOp::Le => {
                if matches!(opr, BinOp::Gt | BinOp::Ge) {
                    std::mem::swap(e1, e2);
                    opr = if opr == BinOp::Gt {
                        BinOp::Lt
                    } else {
                        BinOp::Le
                    };
                }
                self.code_order(opr, e1, e2);
            }
        }
    }

    /// `luaK_fixline`
    fn fix_line(&mut self, line: u32) {
        if let Some(l) = self.fs_mut().lines.last_mut() {
            *l = line;
        }
    }

    /// `luaK_settablesize`
    fn set_table_size(&mut self, pc: usize, ra: i32, asize: i32, hsize: i32) {
        let extra = asize / (MAXARG_VC + 1);
        let rc = asize % (MAXARG_VC + 1);
        let k = extra > 0;
        let hsize = if hsize != 0 {
            ceil_log2(hsize as u32) as i32 + 1
        } else {
            0
        };
        let fs = self.fs_mut();
        fs.code[pc] = create_vabck(OpCode::NewTable, ra, hsize, rc, k);
        fs.code[pc + 1] = create_ax(OpCode::ExtraArg, extra);
    }

    /// `luaK_setlist`
    fn set_list(&mut self, base: i32, mut nelems: i32, mut tostore: i32) {
        if tostore == MULTRET {
            tostore = 0;
        }
        if nelems <= MAXARG_VC {
            self.code_vabck(OpCode::SetList, base, tostore, nelems, false);
        } else {
            let extra = nelems / (MAXARG_VC + 1);
            nelems %= MAXARG_VC + 1;
            self.code_vabck(OpCode::SetList, base, tostore, nelems, true);
            self.code_extra_arg(extra);
        }
        self.fs_mut().freereg = base + 1;
    }

    /// `luaK_finish`
    fn finish(&mut self) {
        let needclose = self.fs().needclose;
        let fs = self.fs_mut();
        if fs.flags & PF_VATAB != 0 {
            fs.flags &= !PF_VAHID;
        }
        let flags = fs.flags;
        let num_params = i32::from(fs.num_params);
        for i in 0..fs.code.len() {
            let pc = &mut fs.code[i];
            match OpCode::of(*pc) {
                OpCode::Return0 | OpCode::Return1 | OpCode::Return | OpCode::TailCall => {
                    let op = OpCode::of(*pc);
                    if matches!(op, OpCode::Return0 | OpCode::Return1) {
                        if !(needclose || flags & PF_VAHID != 0) {
                            continue;
                        }
                        set_opcode(pc, OpCode::Return);
                    }
                    if needclose {
                        set_k(pc, true);
                    }
                    if flags & PF_VAHID != 0 {
                        set_c(pc, num_params + 1);
                    }
                }
                OpCode::GetVarg => {
                    if flags & PF_VATAB != 0 {
                        set_opcode(pc, OpCode::GetTable);
                    }
                }
                OpCode::VarArg => {
                    if flags & PF_VATAB != 0 {
                        set_k(pc, true);
                    }
                }
                OpCode::Jmp => {
                    let target = final_target(&fs.code, i);
                    let offset = target as i32 - (i as i32 + 1);
                    set_sj(&mut fs.code[i], offset);
                }
                _ => {}
            }
        }
    }

    // ---- variables and blocks (lparser.c) --------------------------------------

    /// `reglevel`: the number of registers used by the first `nvar` active variables.
    fn reg_level(&self, nvar: usize) -> i32 {
        reg_level_in(&self.fs().actvar, nvar)
    }

    /// `luaY_nvarstack`
    fn nvarstack(&self) -> i32 {
        self.reg_level(self.fs().actvar.len())
    }

    /// `registerlocalvar`
    fn register_local_var(&mut self, name: &str) -> i32 {
        let fs = self.fs_mut();
        let start_pc = fs.pc() as u32;
        fs.locvars.push(LocVar {
            name: name.into(),
            start_pc,
            end_pc: 0,
        });
        fs.locvars.len() as i32 - 1
    }

    /// Activates a register variable (one step of `adjustlocalvars`). `id` links it to a
    /// declared local; internal variables have none.
    fn activate_var(&mut self, name: &str, id: Option<LocalId>) {
        let ridx = self.nvarstack();
        let pidx = self.register_local_var(name);
        self.fs_mut().actvar.push(VarDesc {
            in_reg: true,
            ridx,
            pidx,
        });
        self.check_limit(i64::from(ridx) + 1, 200, "local variables");
        if let Some(id) = id {
            self.local_reg[id.0 as usize] = ridx;
            let info = self.local_info(id);
            if info.captured {
                // `markupval`, done when the variable is declared rather than when a
                // nested function first uses it: the result is the same by the time
                // anything reads the flags.
                self.mark_upval();
            }
        }
    }

    /// `adjustlocalvars` for declared locals.
    fn activate_locals(&mut self, decls: &[&LocalDecl]) {
        for d in decls {
            self.activate_var(&d.name.name, Some(d.id));
        }
    }

    /// Adds an active declaration that has no register (a compile-time constant or a
    /// `global` declaration), so goto bookkeeping counts it as the reference does.
    fn add_non_reg_var(&mut self) {
        self.fs_mut().actvar.push(VarDesc {
            in_reg: false,
            ridx: 0,
            pidx: -1,
        });
    }

    /// `removevars`
    fn remove_vars(&mut self, tolevel: usize) {
        let fs = self.fs_mut();
        let pc = fs.pc() as u32;
        while fs.actvar.len() > tolevel {
            let vd = fs.actvar.pop().expect("active variable");
            if vd.in_reg && vd.pidx >= 0 {
                fs.locvars[vd.pidx as usize].end_pc = pc;
            }
        }
    }

    /// `markupval` for the innermost block, which is where a variable being declared lives.
    fn mark_upval(&mut self) {
        let fs = self.fs_mut();
        if let Some(bl) = fs.blocks.last_mut() {
            bl.upval = true;
        }
        fs.needclose = true;
    }

    /// `marktobeclosed`
    fn mark_to_be_closed(&mut self) {
        let fs = self.fs_mut();
        let bl = fs.blocks.last_mut().expect("open block");
        bl.upval = true;
        bl.insidetbc = true;
        fs.needclose = true;
    }

    /// `enterblock`
    fn enter_block(&mut self, isloop: u8) {
        let fs = self.fs_mut();
        let insidetbc = fs.blocks.last().is_some_and(|b| b.insidetbc);
        let bl = BlockCnt {
            firstlabel: fs.labels.len(),
            firstgoto: fs.gotos.len(),
            nactvar: fs.actvar.len(),
            upval: false,
            isloop,
            insidetbc,
        };
        fs.blocks.push(bl);
    }

    /// `leaveblock`
    fn leave_block(&mut self) {
        let bl = *self.fs().blocks.last().expect("open block");
        let stklevel = self.reg_level(bl.nactvar);
        let has_previous = self.fs().blocks.len() > 1;
        if has_previous && bl.upval {
            self.code_abc(OpCode::Close, stklevel, 0, 0);
        }
        self.fs_mut().freereg = stklevel;
        // The reference keeps removed variables readable in its array, and the goto
        // bookkeeping below still asks for their registers.
        let vars = self.fs().actvar.clone();
        self.remove_vars(bl.nactvar);
        if bl.isloop == 2 {
            self.create_label(None, false);
        }
        self.solve_gotos(&bl, &vars);
        self.fs_mut().blocks.pop();
    }

    /// `createlabel`
    fn create_label(&mut self, name: Option<Box<str>>, last: bool) {
        let pc = self.get_label();
        let fs = self.fs_mut();
        let nactvar = if last {
            fs.blocks.last().expect("open block").nactvar
        } else {
            fs.actvar.len()
        };
        fs.labels.push(LabelDesc {
            name,
            pc,
            nactvar,
            close: false,
        });
    }

    /// `newgotoentry`
    fn new_goto_entry(&mut self, name: Option<Box<str>>) {
        let pc = self.jump();
        self.code_abc(OpCode::Close, 0, 1, 0); // placeholder, marked as dead
        let nactvar = self.fs().actvar.len();
        self.fs_mut().gotos.push(LabelDesc {
            name,
            pc,
            nactvar,
            close: false,
        });
    }

    /// `solvegotos`
    fn solve_gotos(&mut self, bl: &BlockCnt, vars: &[VarDesc]) {
        let outlevel = reg_level_in(vars, bl.nactvar);
        let mut igt = bl.firstgoto;
        while igt < self.fs().gotos.len() {
            let gt = self.fs().gotos[igt].clone();
            let found = self.fs().labels[bl.firstlabel..]
                .iter()
                .position(|l| l.name == gt.name)
                .map(|i| self.fs().labels[bl.firstlabel + i].clone());
            if let Some(lb) = found {
                self.close_goto(igt, &lb, bl.upval, vars);
            } else {
                let needs_close = bl.upval && reg_level_in(vars, gt.nactvar) > outlevel;
                let g = &mut self.fs_mut().gotos[igt];
                if needs_close {
                    g.close = true;
                }
                g.nactvar = bl.nactvar;
                igt += 1;
            }
        }
        self.fs_mut().labels.truncate(bl.firstlabel);
    }

    /// `closegoto`
    fn close_goto(&mut self, g: usize, label: &LabelDesc, bup: bool, vars: &[VarDesc]) {
        let gt = self.fs().gotos[g].clone();
        let mut gpc = gt.pc;
        if gt.close || (label.nactvar < gt.nactvar && bup) {
            let stklevel = reg_level_in(vars, label.nactvar);
            let fs = self.fs_mut();
            let p = gpc as usize;
            fs.code[p + 1] = fs.code[p];
            fs.code[p] = create_abck(OpCode::Close, stklevel, 0, 0, false);
            gpc += 1;
        }
        self.patch_list(gpc, label.pc);
        self.fs_mut().gotos.remove(g);
    }

    // ---- functions ------------------------------------------------------------

    /// `open_func`
    fn open_func(&mut self, line_defined: u32, last_line_defined: u32) {
        self.fs.push(FuncState {
            code: Vec::new(),
            lines: Vec::new(),
            constants: Vec::new(),
            kcache: HashMap::new(),
            protos: Vec::new(),
            upvalues: Vec::new(),
            locvars: Vec::new(),
            num_params: 0,
            flags: 0,
            max_stack: 2,
            line_defined,
            last_line_defined,
            lasttarget: 0,
            freereg: 0,
            actvar: Vec::new(),
            blocks: Vec::new(),
            labels: Vec::new(),
            gotos: Vec::new(),
            needclose: false,
        });
        self.enter_block(0);
    }

    /// `close_func`: the final `return`, then the finished function.
    fn close_func(&mut self, last_line: u32) -> CompiledFn {
        self.set_line(last_line);
        let n = self.nvarstack();
        self.ret(n, 0);
        self.leave_block();
        self.finish();
        let fs = self.fs.pop().expect("open function");
        CompiledFn {
            info: FnInfo {
                code: fs.code.into_boxed_slice(),
                lines: fs.lines.into_boxed_slice(),
                upvalues: fs.upvalues.into_boxed_slice(),
                locvars: fs.locvars.into_boxed_slice(),
                num_params: fs.num_params,
                flags: fs.flags,
                max_stack: fs.max_stack.clamp(0, 255) as u8,
                line_defined: fs.line_defined,
                last_line_defined: fs.last_line_defined,
            },
            constants: fs.constants,
            protos: fs.protos,
        }
    }

    /// `setvararg`
    fn set_vararg(&mut self) {
        self.fs_mut().flags |= PF_VAHID;
        self.code_abc(OpCode::VarArgPrep, 0, 0, 0);
    }

    /// The kind recorded in upvalue descriptors for a local.
    fn var_kind_of(&self, id: LocalId) -> u8 {
        match self.local_info(id).kind {
            LocalKind::Regular => var_kind::REG,
            LocalKind::Const | LocalKind::ForControl => var_kind::CONST,
            LocalKind::Close => var_kind::TOCLOSE,
            LocalKind::VarargTable => var_kind::VAVAR,
            LocalKind::CompileTimeConst(_) => var_kind::CONST,
        }
    }

    /// Upvalue descriptors for a function about to be opened, resolved against the
    /// enclosing (current) function.
    fn upvalue_descs(&self, body: &FuncBody) -> Vec<UpvalDesc> {
        body.upvalues
            .iter()
            .map(|u| match u.source {
                UpvalSource::Local(id) => UpvalDesc {
                    name: u.name.clone(),
                    in_stack: true,
                    index: self.local_reg[id.0 as usize] as u8,
                    kind: self.var_kind_of(id),
                },
                UpvalSource::Upvalue(i) => UpvalDesc {
                    name: u.name.clone(),
                    in_stack: false,
                    index: i as u8,
                    kind: self.fs().upvalues[i as usize].kind,
                },
                UpvalSource::ChunkEnv => UpvalDesc {
                    name: u.name.clone(),
                    in_stack: true,
                    index: 0,
                    kind: var_kind::REG,
                },
            })
            .collect()
    }

    /// `mainfunc`
    fn main_func(&mut self, body: &FuncBody) -> CompiledFn {
        self.open_func(0, 0);
        self.set_vararg();
        self.fs_mut().upvalues = self.upvalue_descs_main(body);
        self.stat_list(&body.block, false);
        let last = self.max_line;
        self.close_func(last)
    }

    fn upvalue_descs_main(&self, body: &FuncBody) -> Vec<UpvalDesc> {
        body.upvalues
            .iter()
            .map(|u| UpvalDesc {
                name: u.name.clone(),
                in_stack: true,
                index: 0,
                kind: var_kind::REG,
            })
            .collect()
    }

    /// `body`: compiles a nested function and leaves its closure in the next register.
    fn body(&mut self, body: &FuncBody, e: &mut ExpDesc) {
        let upvalues = self.upvalue_descs(body);
        self.open_func(body.line_defined, body.last_line_defined);
        self.set_line(body.line_defined);
        self.fs_mut().upvalues = upvalues;
        // `parlist` (including the implicit `self` of methods)
        let params: Vec<&LocalDecl> = body.params.iter().collect();
        self.activate_locals(&params);
        let nparams = self.fs().actvar.len();
        self.fs_mut().num_params = nparams as u8;
        if let Some(va) = &body.vararg {
            self.set_vararg();
            match &va.name {
                Some(decl) => {
                    self.activate_var(&decl.name.name, Some(decl.id));
                    if self.local_info(decl.id).captured {
                        // Used as an upvalue: `singlevaraux` turns it into a real table.
                        self.need_vatab();
                    }
                }
                None => self.activate_var("(vararg table)", None),
            }
        }
        let n = self.fs().actvar.len() as i32;
        self.reserve_regs(n);
        self.stat_list(&body.block, false);
        let f = self.close_func(body.last_line_defined);
        // `codeclosure`, in the enclosing function
        self.set_line(body.last_line_defined);
        let fs = self.fs_mut();
        fs.protos.push(f);
        let np = fs.protos.len() as i32 - 1;
        let pc = self.code_abx(OpCode::Closure, 0, np);
        *e = ExpDesc::new(Kind::Reloc(pc));
        self.exp2nextreg(e);
    }

    // ---- expressions ----------------------------------------------------------

    /// The expression for a variable reference (`singlevar`).
    fn single_var(&mut self, n: &NameRef) -> ExpDesc {
        match n.binding {
            Binding::Local(id) => {
                let r = self.local_reg[id.0 as usize];
                if self.local_info(id).kind == LocalKind::VarargTable {
                    ExpDesc::new(Kind::VargVar(r))
                } else {
                    ExpDesc::new(Kind::Local(r))
                }
            }
            Binding::Upvalue(i) => ExpDesc::new(Kind::Upval(i as i32)),
            Binding::Const(id) => ExpDesc::new(Kind::Const(id)),
            Binding::Global(env) => self.build_global(n.name.name.as_bytes(), env),
        }
    }

    /// The expression for `_ENV` given an [`EnvRef`].
    fn env_var(&mut self, env: EnvRef) -> ExpDesc {
        match env {
            EnvRef::Local(id) => {
                if let LocalKind::CompileTimeConst(_) = self.local_info(id).kind {
                    ExpDesc::new(Kind::Const(id))
                } else {
                    ExpDesc::new(Kind::Local(self.local_reg[id.0 as usize]))
                }
            }
            EnvRef::Upvalue(i) => ExpDesc::new(Kind::Upval(i as i32)),
        }
    }

    /// `buildglobal`: `_ENV[name]`.
    fn build_global(&mut self, name: &[u8], env: EnvRef) -> ExpDesc {
        let mut var = self.env_var(env);
        self.exp2anyregup(&mut var);
        let s = self.intern(name);
        let mut key = ExpDesc::new(Kind::KStr(s));
        self.indexed(&mut var, &mut key);
        var
    }

    /// Compiles an expression. Afterwards the current line is where the expression ends,
    /// which is the line the reference gives to code that discharges it.
    fn expr(&mut self, e: &Expr) -> ExpDesc {
        let v = self.expr_inner(e);
        self.set_line(self.chunk.end_line(e.span));
        v
    }

    fn expr_inner(&mut self, e: &Expr) -> ExpDesc {
        self.set_line(e.span.line);
        match &e.kind {
            ExprKind::Nil => ExpDesc::new(Kind::Nil),
            ExprKind::True => ExpDesc::new(Kind::True),
            ExprKind::False => ExpDesc::new(Kind::False),
            ExprKind::Int(i) => ExpDesc::new(Kind::KInt(*i)),
            ExprKind::Float(f) => ExpDesc::new(Kind::KFlt(*f)),
            ExprKind::Str(s) => ExpDesc::new(Kind::KStr(self.intern(s))),
            ExprKind::Vararg => {
                // Emitted before `...` is read.
                self.set_line(self.chunk.line_before(e.span.start));
                let np = i32::from(self.fs().num_params);
                let pc = self.code_abc(OpCode::VarArg, 0, np, 1);
                ExpDesc::new(Kind::Vararg(pc))
            }
            ExprKind::Function(body) => {
                let mut v = ExpDesc::new(Kind::Void);
                self.body(body, &mut v);
                v
            }
            ExprKind::Table(fields) => {
                // `NEWTABLE` is emitted before `{` is read.
                self.set_line(self.chunk.line_before(e.span.start));
                self.constructor(fields, e.span)
            }
            ExprKind::Name(n) => self.single_var(n),
            ExprKind::Index { obj, key } => {
                let mut v = self.expr(obj);
                self.exp2anyregup(&mut v);
                let mut k = self.expr(key);
                self.exp2val(&mut k);
                self.indexed(&mut v, &mut k);
                v
            }
            ExprKind::Call(call) => {
                let mut v = self.expr(&call.func);
                self.exp2nextreg(&mut v);
                self.func_args(&mut v, &call.args, call.line);
                v
            }
            ExprKind::MethodCall(mc) => {
                let mut v = self.expr(&mc.obj);
                let s = self.intern(mc.method.name.as_bytes());
                let mut key = ExpDesc::new(Kind::KStr(s));
                self.self_(&mut v, &mut key);
                self.func_args(&mut v, &mc.args, mc.line);
                v
            }
            ExprKind::Paren(inner) => {
                let mut v = self.expr(inner);
                self.discharge_vars(&mut v);
                v
            }
            ExprKind::Unary { op, operand, line } => {
                let mut v = self.expr(operand);
                self.prefix(*op, &mut v, *line);
                v
            }
            ExprKind::Binary { op, lhs, rhs, line } => {
                let mut v = self.expr(lhs);
                self.infix(*op, &mut v);
                let mut v2 = self.expr(rhs);
                self.posfix(*op, &mut v, &mut v2, *line);
                v
            }
        }
    }

    /// `explist`: all but the last expression go to consecutive registers; the last is
    /// returned undischarged. Returns the number of expressions.
    fn exp_list(&mut self, es: &[Expr], last: &mut ExpDesc) -> i32 {
        let Some((lst, init)) = es.split_last() else {
            *last = ExpDesc::new(Kind::Void);
            return 0;
        };
        for e in init {
            let mut v = self.expr(e);
            self.exp2nextreg(&mut v);
        }
        *last = self.expr(lst);
        es.len() as i32
    }

    /// `funcargs`
    fn func_args(&mut self, f: &mut ExpDesc, args: &[Expr], line: u32) {
        let mut a = ExpDesc::new(Kind::Void);
        self.exp_list(args, &mut a);
        if a.k.is_multret() {
            self.set_multret(&mut a);
        }
        let base = f.reg();
        let nparams = if a.k.is_multret() {
            MULTRET
        } else {
            if !matches!(a.k, Kind::Void) {
                self.exp2nextreg(&mut a);
            }
            self.fs().freereg - (base + 1)
        };
        let pc = self.code_abc(OpCode::Call, base, nparams + 1, 2);
        f.k = Kind::Call(pc);
        self.fix_line(line);
        self.fs_mut().freereg = base + 1;
    }

    /// `constructor`
    fn constructor(&mut self, fields: &[Field], span: Span) -> ExpDesc {
        let pc = self.code_vabck(OpCode::NewTable, 0, 0, 0, false) as usize;
        self.code(0); // space for the extra argument
        let freereg = self.fs().freereg;
        let t = ExpDesc::new(Kind::NonReloc(freereg));
        self.reserve_regs(1);
        let mut na: i32 = 0;
        let mut nh: i32 = 0;
        let mut tostore: i32 = 0;
        let mut v = ExpDesc::new(Kind::Void);
        let maxtostore = self.max_to_store();
        for field in fields {
            if !matches!(v.k, Kind::Void) {
                // `closelistfield`, run after the separator is read
                let first = match field {
                    Field::Positional(e) => e.span.start,
                    Field::Named(name, _) => name.span.start,
                    // the `[` before the key
                    Field::Keyed(k, _) => self.token_start_before(k.span.start),
                };
                self.set_line(self.chunk.line_before(first));
                self.exp2nextreg(&mut v);
                v.k = Kind::Void;
                if tostore >= maxtostore {
                    self.set_list(t.reg(), na, tostore);
                    na += tostore;
                    tostore = 0;
                }
            }
            match field {
                Field::Positional(e) => {
                    v = self.expr(e);
                    tostore += 1;
                }
                Field::Named(name, val) => {
                    let s = self.intern(name.name.as_bytes());
                    let key = ExpDesc::new(Kind::KStr(s));
                    let reg = self.fs().freereg;
                    self.rec_field(reg, &t, key, val);
                    nh += 1;
                }
                Field::Keyed(k, val) => {
                    let reg = self.fs().freereg;
                    let mut key = self.expr(k);
                    self.exp2val(&mut key);
                    self.rec_field(reg, &t, key, val);
                    nh += 1;
                }
            }
            self.check_limit(
                i64::from(tostore) + i64::from(na) + i64::from(nh),
                MAX_CNST,
                "items in a constructor",
            );
        }
        // `lastlistfield`, run after `}` is read
        self.set_line(self.chunk.end_line(span));
        if tostore != 0 {
            if v.k.is_multret() {
                self.set_multret(&mut v);
                self.set_list(t.reg(), na, MULTRET);
                na -= 1;
            } else {
                if !matches!(v.k, Kind::Void) {
                    self.exp2nextreg(&mut v);
                }
                self.set_list(t.reg(), na, tostore);
            }
            na += tostore;
        }
        self.set_table_size(pc, t.reg(), na, nh);
        t
    }

    /// `recfield`
    /// `reg` is the first free register before the key was evaluated.
    fn rec_field(&mut self, reg: i32, t: &ExpDesc, mut key: ExpDesc, val: &Expr) {
        let mut tab = *t;
        self.indexed(&mut tab, &mut key);
        let mut v = self.expr(val);
        self.store_var(&tab, &mut v);
        self.fs_mut().freereg = reg;
    }

    /// `maxtostore`
    fn max_to_store(&self) -> i32 {
        let numfreeregs = MAX_FSTACK - self.fs().freereg;
        if numfreeregs >= 160 {
            numfreeregs / 5
        } else if numfreeregs >= 80 {
            10
        } else {
            1
        }
    }

    // ---- statements -----------------------------------------------------------

    /// `statlist`. `in_repeat` is true for the body of `repeat`, whose labels are never
    /// "last" because `until` does not end a block for that purpose.
    fn stat_list(&mut self, block: &Block, in_repeat: bool) {
        for (i, stat) in block.stats.iter().enumerate() {
            let last = !in_repeat
                && block.ret.is_none()
                && block.stats[i + 1..]
                    .iter()
                    .all(|s| matches!(s.kind, StatKind::Label(_)));
            self.statement(stat, last);
        }
        if let Some(r) = &block.ret {
            self.ret_stat(r);
            self.set_line(self.chunk.end_line(r.span));
            let n = self.nvarstack();
            self.fs_mut().freereg = n;
        }
    }

    /// `block`
    fn block(&mut self, b: &Block) {
        self.enter_block(0);
        self.stat_list(b, false);
        self.leave_block();
    }

    /// `statement`. `last` tells a label whether only labels follow it in its block.
    fn statement(&mut self, stat: &Stat, last: bool) {
        let line = stat.span.line;
        self.set_line(line);
        match &stat.kind {
            StatKind::If(s) => self.if_stat(s),
            StatKind::While(s) => self.while_stat(s),
            StatKind::Do(b) => self.block(b),
            StatKind::NumericFor(s) => self.for_num(s, line, self.chunk.end_line(stat.span)),
            StatKind::GenericFor(s) => self.for_list(s, self.chunk.end_line(stat.span)),
            StatKind::Repeat(s) => self.repeat_stat(s),
            StatKind::Function { name, body } => self.func_stat(name, body, line),
            StatKind::LocalFunction { var, body } => self.local_func(var, body),
            StatKind::Local { vars, values } => self.local_stat(vars, values),
            StatKind::Global { vars, values, env } => self.global_stat(vars, values, *env, line),
            StatKind::GlobalAll { .. } => self.add_non_reg_var(),
            StatKind::GlobalFunction { name, env, body } => {
                self.global_func(name, *env, body, line)
            }
            StatKind::Label(name) => {
                self.create_label(Some(name.name.clone()), last);
            }
            StatKind::Goto(name) => self.new_goto_entry(Some(name.name.clone())),
            StatKind::Break => {
                let fs = self.fs_mut();
                if let Some(bl) = fs.blocks.iter_mut().rev().find(|b| b.isloop != 0) {
                    bl.isloop = 2;
                }
                self.new_goto_entry(None);
            }
            StatKind::Assign { targets, values } => self.assign(targets, values),
            StatKind::Call(call) => {
                let v = self.expr(call);
                let pc = v.pc();
                set_c(self.code_at(pc), 1);
            }
        }
        let n = self.nvarstack();
        self.fs_mut().freereg = n;
        self.set_line(self.chunk.end_line(stat.span));
    }

    /// `test_then_block` and `ifstat`
    fn if_stat(&mut self, s: &IfStat) {
        let mut escapelist = NO_JUMP;
        let n = s.clauses.len();
        for (i, clause) in s.clauses.iter().enumerate() {
            let condtrue = self.cond(&clause.cond);
            self.block(&clause.block);
            if i + 1 < n || s.else_block.is_some() {
                let j = self.jump();
                self.concat(&mut escapelist, j);
            }
            self.patch_to_here(condtrue);
        }
        if let Some(b) = &s.else_block {
            self.block(b);
        }
        self.patch_to_here(escapelist);
    }

    /// `cond`
    fn cond(&mut self, e: &Expr) -> i32 {
        let mut v = self.expr(e);
        if let Kind::Nil = v.k {
            v.k = Kind::False;
        }
        self.go_if_true(&mut v);
        v.f
    }

    /// `whilestat`
    fn while_stat(&mut self, s: &WhileStat) {
        let whileinit = self.get_label();
        let condexit = self.cond(&s.cond);
        self.enter_block(1);
        self.block(&s.body);
        let j = self.jump();
        self.patch_list(j, whileinit);
        self.leave_block();
        self.patch_to_here(condexit);
    }

    /// `repeatstat`
    fn repeat_stat(&mut self, s: &RepeatStat) {
        let repeat_init = self.get_label();
        self.enter_block(1);
        self.enter_block(0);
        self.stat_list(&s.body, true);
        let mut condexit = self.cond(&s.cond);
        let bl2 = *self.fs().blocks.last().expect("scope block");
        if bl2.upval {
            let exit = self.jump();
            self.patch_to_here(condexit);
            let level = self.reg_level(bl2.nactvar);
            self.code_abc(OpCode::Close, level, 0, 0);
            condexit = self.jump();
            self.patch_to_here(exit);
        }
        self.patch_list(condexit, repeat_init);
        self.leave_block();
        self.leave_block();
    }

    /// `exp1`
    fn exp1(&mut self, e: &Expr) {
        let mut v = self.expr(e);
        self.exp2nextreg(&mut v);
    }

    /// `fixforjump`
    fn fix_for_jump(&mut self, pc: i32, dest: i32, back: bool) {
        let mut offset = dest - (pc + 1);
        if back {
            offset = -offset;
        }
        if offset > MAXARG_BX {
            self.syntax_error("control structure too long".into());
            return;
        }
        set_bx(self.code_at(pc as usize), offset);
    }

    /// `forbody`
    fn for_body(&mut self, base: i32, line: u32, vars: &[&LocalDecl], body: &Block, isgen: bool) {
        let (prep_op, loop_op) = if isgen {
            (OpCode::TForPrep, OpCode::TForLoop)
        } else {
            (OpCode::ForPrep, OpCode::ForLoop)
        };
        let prep = self.code_abx(prep_op, base, 0);
        self.fs_mut().freereg -= 1;
        self.enter_block(0);
        self.activate_locals(vars);
        self.reserve_regs(vars.len() as i32);
        self.block(body);
        self.leave_block();
        let here = self.get_label();
        self.fix_for_jump(prep, here, false);
        self.set_line(line);
        if isgen {
            self.code_abc(OpCode::TForCall, base, 0, vars.len() as i32);
            self.fix_line(line);
        }
        let endfor = self.code_abx(loop_op, base, 0);
        self.fix_for_jump(endfor, prep + 1, true);
        self.fix_line(line);
    }

    /// `forstat` with `fornum`
    fn for_num(&mut self, s: &NumericFor, line: u32, end_line: u32) {
        self.enter_block(1);
        let base = self.fs().freereg;
        self.exp1(&s.start);
        self.exp1(&s.limit);
        match &s.step {
            Some(step) => self.exp1(step),
            None => {
                let r = self.fs().freereg;
                self.int(r, 1);
                self.reserve_regs(1);
            }
        }
        self.activate_var("(for state)", None);
        self.activate_var("(for state)", None);
        self.set_line(line);
        self.for_body(base, line, &[&s.var], &s.body, false);
        self.set_line(end_line);
        self.leave_block();
    }

    /// `forstat` with `forlist`
    fn for_list(&mut self, s: &GenericFor, end_line: u32) {
        self.enter_block(1);
        let base = self.fs().freereg;
        let mut e = ExpDesc::new(Kind::Void);
        let nexps = self.exp_list(&s.exprs, &mut e);
        self.adjust_assign(4, nexps, &mut e);
        self.activate_var("(for state)", None);
        self.activate_var("(for state)", None);
        self.activate_var("(for state)", None);
        self.mark_to_be_closed();
        self.check_stack(2);
        let vars: Vec<&LocalDecl> = s.vars.iter().collect();
        self.set_line(s.exprs_line);
        self.for_body(base, s.exprs_line, &vars, &s.body, true);
        self.set_line(end_line);
        self.leave_block();
    }

    /// `adjust_assign`
    fn adjust_assign(&mut self, nvars: i32, nexps: i32, e: &mut ExpDesc) {
        let needed = nvars - nexps;
        self.check_stack(needed);
        if e.k.is_multret() {
            let extra = (needed + 1).max(0);
            self.set_returns(e, extra);
        } else {
            if !matches!(e.k, Kind::Void) {
                self.exp2nextreg(e);
            }
            if needed > 0 {
                let r = self.fs().freereg;
                self.nil(r, needed);
            }
        }
        if needed > 0 {
            self.reserve_regs(needed);
        } else {
            self.fs_mut().freereg += needed;
        }
    }

    /// `localfunc`
    fn local_func(&mut self, var: &LocalDecl, body: &FuncBody) {
        let fvar = self.fs().actvar.len();
        self.activate_locals(&[var]);
        let mut b = ExpDesc::new(Kind::Void);
        self.body(body, &mut b);
        // Debug information sees the variable only after this point.
        let fs = self.fs_mut();
        let pidx = fs.actvar[fvar].pidx as usize;
        fs.locvars[pidx].start_pc = fs.pc() as u32;
    }

    /// `localstat`
    fn local_stat(&mut self, vars: &[LocalVar], values: &[Expr]) {
        let nvars = vars.len() as i32;
        let toclose = vars.iter().position(|v| v.attrib == Some(Attrib::Close));
        let last = vars.last().expect("at least one variable");
        let last_is_ctc = matches!(
            self.local_info(last.decl.id).kind,
            LocalKind::CompileTimeConst(_)
        );
        let decls: Vec<&LocalDecl> = vars.iter().map(|v| &v.decl).collect();
        if last_is_ctc {
            // The last value is folded into the variable; the others are evaluated.
            for e in &values[..values.len() - 1] {
                let mut v = self.expr(e);
                self.exp2nextreg(&mut v);
            }
            self.activate_locals(&decls[..decls.len() - 1]);
            self.add_non_reg_var();
        } else {
            let mut e = ExpDesc::new(Kind::Void);
            let nexps = self.exp_list(values, &mut e);
            self.adjust_assign(nvars, nexps, &mut e);
            self.activate_locals(&decls);
        }
        if let Some(i) = toclose {
            // `checktoclose`
            self.mark_to_be_closed();
            let level = self.fs().actvar.len() - vars.len() + i;
            let r = self.reg_level(level);
            self.code_abc(OpCode::Tbc, r, 0, 0);
        }
    }

    /// `checkglobal`
    fn check_global(&mut self, name: &[u8], env: EnvRef, line: u32) {
        let mut var = self.build_global(name, env);
        let k = match var.k {
            Kind::IndexUp { keystr, .. }
            | Kind::IndexStr { keystr, .. }
            | Kind::Indexed { keystr, .. }
            | Kind::VargInd { keystr, .. } => keystr,
            _ => -1,
        };
        self.code_check_global(&mut var, k, line);
    }

    /// `globalnames` with `initglobal`
    fn global_stat(&mut self, vars: &[GlobalVar], values: &[Expr], env: Option<EnvRef>, line: u32) {
        if let Some(env) = env {
            let mut targets = Vec::with_capacity(vars.len());
            for v in vars {
                targets.push(self.build_global(v.name.name.as_bytes(), env));
            }
            let mut e = ExpDesc::new(Kind::Void);
            let nexps = self.exp_list(values, &mut e);
            self.adjust_assign(vars.len() as i32, nexps, &mut e);
            for (v, t) in vars.iter().zip(targets).rev() {
                self.check_global(v.name.name.as_bytes(), env, line);
                self.store_var_top(&t);
            }
        }
        for _ in vars {
            self.add_non_reg_var();
        }
    }

    /// `globalfunc`
    fn global_func(&mut self, name: &Name, env: EnvRef, body: &FuncBody, line: u32) {
        self.add_non_reg_var();
        let var = self.build_global(name.name.as_bytes(), env);
        let mut b = ExpDesc::new(Kind::Void);
        self.body(body, &mut b);
        self.check_global(name.name.as_bytes(), env, line);
        self.store_var(&var, &mut b);
        self.fix_line(line);
    }

    /// `funcstat`
    fn func_stat(&mut self, name: &FuncName, body: &FuncBody, line: u32) {
        // `funcname`
        let mut v = self.single_var(&name.base);
        for f in name.fields.iter().chain(name.method.iter()) {
            self.exp2anyregup(&mut v);
            let s = self.intern(f.name.as_bytes());
            let mut key = ExpDesc::new(Kind::KStr(s));
            self.indexed(&mut v, &mut key);
        }
        self.readonly_fixup(&mut v);
        let mut b = ExpDesc::new(Kind::Void);
        self.body(body, &mut b);
        self.store_var(&v, &mut b);
        self.fix_line(line);
    }

    /// The code-changing part of `check_readonly`: assigning to a field of the vararg
    /// table needs a real table. (The parser has already reported read-only errors.)
    fn readonly_fixup(&mut self, e: &mut ExpDesc) {
        if let Kind::VargInd { t, idx, keystr } = e.k {
            self.need_vatab();
            e.k = Kind::Indexed { t, idx, keystr };
        }
    }

    /// `check_conflict`
    fn check_conflict(&mut self, lh: &mut [ExpDesc], v: &ExpDesc) {
        let extra = self.fs().freereg;
        let mut conflict = false;
        for prev in lh.iter_mut() {
            match prev.k {
                Kind::IndexUp { t, idx, keystr } => {
                    if let Kind::Upval(u) = v.k
                        && t == u
                    {
                        conflict = true;
                        prev.k = Kind::IndexStr {
                            t: extra,
                            idx,
                            keystr,
                        };
                    }
                }
                Kind::IndexStr { t, .. }
                | Kind::IndexI { t, .. }
                | Kind::Indexed { t, .. }
                | Kind::VargInd { t, .. } => {
                    if let Kind::Local(r) = v.k {
                        if t == r {
                            conflict = true;
                            set_table_reg(&mut prev.k, extra);
                        }
                        if let Kind::Indexed { idx, .. } = prev.k
                            && idx == r
                        {
                            conflict = true;
                            if let Kind::Indexed { idx, .. } = &mut prev.k {
                                *idx = extra;
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        if conflict {
            match v.k {
                Kind::Local(r) => {
                    self.code_abc(OpCode::Move, extra, r, 0);
                }
                Kind::Upval(u) => {
                    self.code_abc(OpCode::GetUpval, extra, u, 0);
                }
                _ => {}
            }
            self.reserve_regs(1);
        }
    }

    /// `storevartop`
    fn store_var_top(&mut self, var: &ExpDesc) {
        let r = self.fs().freereg - 1;
        let mut e = ExpDesc::new(Kind::NonReloc(r));
        self.store_var(var, &mut e);
    }

    /// `restassign`
    fn assign(&mut self, targets: &[Expr], values: &[Expr]) {
        let mut lh: Vec<ExpDesc> = Vec::with_capacity(targets.len());
        for t in targets {
            let mut v = self.expr(t);
            self.readonly_fixup(&mut v);
            if !lh.is_empty() && !v.k.is_indexed() {
                self.check_conflict(&mut lh, &v);
            }
            lh.push(v);
        }
        let nvars = lh.len() as i32;
        let mut e = ExpDesc::new(Kind::Void);
        let nexps = self.exp_list(values, &mut e);
        let mut rest = lh.len();
        if nexps != nvars {
            self.adjust_assign(nvars, nexps, &mut e);
        } else {
            self.set_one_ret(&mut e);
            let last = lh[lh.len() - 1];
            self.store_var(&last, &mut e);
            rest -= 1;
        }
        for v in lh[..rest].iter().rev() {
            self.store_var_top(v);
        }
    }

    /// `retstat`
    fn ret_stat(&mut self, r: &Return) {
        self.set_line(r.span.line);
        let mut first = self.nvarstack();
        let mut nret;
        if r.values.is_empty() {
            nret = 0;
        } else {
            let mut e = ExpDesc::new(Kind::Void);
            nret = self.exp_list(&r.values, &mut e);
            if e.k.is_multret() {
                self.set_multret(&mut e);
                let insidetbc = self.fs().blocks.last().is_some_and(|b| b.insidetbc);
                if let Kind::Call(pc) = e.k
                    && nret == 1
                    && !insidetbc
                {
                    set_opcode(self.code_at(pc as usize), OpCode::TailCall);
                }
                nret = MULTRET;
            } else if nret == 1 {
                first = self.exp2anyreg(&mut e);
            } else {
                self.exp2nextreg(&mut e);
            }
        }
        self.ret(first, nret);
    }
}

/// Sets the table register of an indexed expression (used by `check_conflict`).
fn set_table_reg(k: &mut Kind, reg: i32) {
    match k {
        Kind::IndexStr { t, .. }
        | Kind::IndexI { t, .. }
        | Kind::Indexed { t, .. }
        | Kind::VargInd { t, .. } => *t = reg,
        _ => {}
    }
}

/// `tonumeral`
fn to_numeral(e: &ExpDesc) -> Option<Const> {
    if e.has_jumps() {
        return None;
    }
    match e.k {
        Kind::KInt(i) => Some(Const::Int(i)),
        Kind::KFlt(f) => Some(Const::Float(f)),
        _ => None,
    }
}

/// `fitsC`
fn fits_c(i: i64) -> bool {
    (i as u64).wrapping_add(OFFSET_SC as u64) <= MAXARG_C as u64
}

/// `fitsBx`
fn fits_bx(i: i64) -> bool {
    -i64::from(OFFSET_SBX) <= i && i <= i64::from(MAXARG_BX - OFFSET_SBX)
}

/// `isSCint`
fn is_scint(e: &ExpDesc) -> bool {
    matches!(e.k, Kind::KInt(i) if !e.has_jumps() && fits_c(i))
}

/// `isSCnumber`: the encoded immediate and whether the original was a float.
/// `isSCnumber`: the immediate operand for a numeric constant, if it fits. Sets
/// `isfloat` for an integral float, even one that does not fit.
fn is_scnumber(e: &ExpDesc, isfloat: &mut bool) -> Option<i32> {
    let i = match e.k {
        Kind::KInt(i) => i,
        Kind::KFlt(f) => {
            let i = float_to_int(f)?;
            *isfloat = true;
            i
        }
        _ => return None,
    };
    if !e.has_jumps() && fits_c(i) {
        Some(int2sc(i as i32))
    } else {
        None
    }
}

fn float_to_int(f: f64) -> Option<i64> {
    orbit_lua_syntax::fold::float_to_int(f)
}

/// The folding operator of a binary operator (`foldbinop`).
fn fold_op(op: BinOp) -> Option<Arith> {
    Some(match op {
        BinOp::Add => Arith::Add,
        BinOp::Sub => Arith::Sub,
        BinOp::Mul => Arith::Mul,
        BinOp::Mod => Arith::Mod,
        BinOp::Pow => Arith::Pow,
        BinOp::Div => Arith::Div,
        BinOp::IDiv => Arith::IDiv,
        BinOp::BAnd => Arith::BAnd,
        BinOp::BOr => Arith::BOr,
        BinOp::BXor => Arith::BXor,
        BinOp::Shl => Arith::Shl,
        BinOp::Shr => Arith::Shr,
        _ => return None,
    })
}

/// Position of an arithmetic or bitwise operator in `ORDER OPR`.
fn arith_index(op: BinOp) -> i32 {
    match op {
        BinOp::Add => 0,
        BinOp::Sub => 1,
        BinOp::Mul => 2,
        BinOp::Mod => 3,
        BinOp::Pow => 4,
        BinOp::Div => 5,
        BinOp::IDiv => 6,
        BinOp::BAnd => 7,
        BinOp::BOr => 8,
        BinOp::BXor => 9,
        BinOp::Shl => 10,
        BinOp::Shr => 11,
        _ => unreachable!("not an arithmetic operator"),
    }
}

/// `binopr2op`: the opcode for `op` in the block starting at `base` (`ADD` or `ADDK`).
fn arith_op(op: BinOp, base: OpCode) -> OpCode {
    OPCODES[base as usize + arith_index(op) as usize]
}

/// `binopr2TM`: the metamethod event, as the VM numbers them (`TM_ADD` is 6).
fn tm_of(op: BinOp) -> i32 {
    6 + arith_index(op)
}

/// `luaO_ceillog2`
fn ceil_log2(x: u32) -> u32 {
    if x <= 1 {
        0
    } else {
        32 - (x - 1).leading_zeros()
    }
}

/// `finaltarget`
fn final_target(code: &[Instruction], mut i: usize) -> usize {
    for _ in 0..100 {
        let pc = code[i];
        if OpCode::of(pc) != OpCode::Jmp {
            break;
        }
        i = (i as i64 + i64::from(arg_sj(pc)) + 1) as usize;
    }
    i
}

/// `reglevel` over a variable list: the number of registers the first `nvar` variables
/// use.
fn reg_level_in(vars: &[VarDesc], nvar: usize) -> i32 {
    vars[..nvar]
        .iter()
        .rev()
        .find(|vd| vd.in_reg)
        .map_or(0, |vd| vd.ridx + 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn compile_src(src: &str) -> CompiledFn {
        let chunk = orbit_lua_syntax::parse(src.as_bytes()).expect("parses");
        compile(&chunk).expect("compiles")
    }

    fn ops(f: &CompiledFn) -> Vec<OpCode> {
        f.info.code.iter().map(|&i| OpCode::of(i)).collect()
    }

    #[test]
    fn simple_chunk() {
        let f = compile_src("local a = 1; return a + 2");
        use OpCode::*;
        assert_eq!(
            ops(&f),
            vec![VarArgPrep, LoadI, AddI, MmBinI, Return, Return]
        );
        assert_eq!(f.info.max_stack, 2);
    }

    #[test]
    fn globals_and_calls() {
        let f = compile_src("print('hi', x)");
        use OpCode::*;
        assert_eq!(
            ops(&f),
            vec![VarArgPrep, GetTabUp, LoadK, GetTabUp, Call, Return]
        );
        assert_eq!(f.constants[0], Constant::Str(b"print".as_slice().into()));
    }

    #[test]
    fn constant_folding_and_conditions() {
        let f = compile_src("local x = 2^10; if x > 3 and x < 5 then x = nil end");
        use OpCode::*;
        assert_eq!(
            ops(&f),
            vec![VarArgPrep, LoadF, GtI, Jmp, LtI, Jmp, LoadNil, Return]
        );
    }

    #[test]
    fn closures_and_upvalues() {
        let f = compile_src("local a = 0; local function f() a = a + 1 return a end");
        assert_eq!(f.protos.len(), 1);
        let inner = &f.protos[0];
        assert_eq!(inner.info.upvalues[0].name.as_ref(), "a");
        assert!(inner.info.upvalues[0].in_stack);
        use OpCode::*;
        assert_eq!(ops(&f), vec![VarArgPrep, LoadI, Closure, Return]);
        assert_eq!(
            ops(inner),
            vec![GetUpval, AddI, MmBinI, SetUpval, GetUpval, Return1, Return0]
        );
    }

    #[test]
    fn loops_and_gotos() {
        let f = compile_src(
            "for i = 1, 10 do if i == 5 then break end end \
             for k, v in pairs(t) do end \
             do goto l; local x; ::l:: end",
        );
        use OpCode::*;
        let o = ops(&f);
        assert!(o.contains(&ForPrep) && o.contains(&ForLoop));
        assert!(o.contains(&TForPrep) && o.contains(&TForCall) && o.contains(&TForLoop));
    }

    #[test]
    fn locvars_recorded() {
        let f = compile_src("local a, b = 1, 2; do local c end");
        let names: Vec<&str> = f.info.locvars.iter().map(|v| v.name.as_ref()).collect();
        assert_eq!(names, vec!["a", "b", "c"]);
    }
}
