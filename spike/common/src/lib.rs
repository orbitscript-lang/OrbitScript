//! Shared pieces of the S0 spike: the bytecode both micro-VMs run, a tiny
//! assembler, the superinstruction pass, and the hand-assembled benchmarks.
//!
//! The bytecode is register based and modeled on Lua 5.4/5.5. Jump targets are
//! absolute instruction indexes to keep the hand assembly readable.

use std::rc::Rc;

pub type Reg = u8;

/// One instruction. `size_of::<Op>()` is 12 bytes (PUC Lua uses 4).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Op {
    Move {
        a: Reg,
        b: Reg,
    },
    LoadI {
        a: Reg,
        i: i32,
    },
    LoadK {
        a: Reg,
        k: u16,
    },
    LoadNil {
        a: Reg,
    },
    GetUpval {
        a: Reg,
        u: u8,
    },
    SetUpval {
        a: Reg,
        u: u8,
    },
    NewTable {
        a: Reg,
    },
    /// R[a] = R[b][R[c]]
    GetTable {
        a: Reg,
        b: Reg,
        c: Reg,
    },
    /// R[a][R[b]] = R[c]
    SetTable {
        a: Reg,
        b: Reg,
        c: Reg,
    },
    /// R[a] = R[b][K[k]] (K[k] is a string)
    GetField {
        a: Reg,
        b: Reg,
        k: u8,
    },
    /// R[a][K[k]] = R[c]
    SetField {
        a: Reg,
        k: u8,
        c: Reg,
    },
    Add {
        a: Reg,
        b: Reg,
        c: Reg,
    },
    Sub {
        a: Reg,
        b: Reg,
        c: Reg,
    },
    Mul {
        a: Reg,
        b: Reg,
        c: Reg,
    },
    Mod {
        a: Reg,
        b: Reg,
        c: Reg,
    },
    AddI {
        a: Reg,
        b: Reg,
        i: i16,
    },
    /// R[a] = R[b] .. R[c]
    Concat {
        a: Reg,
        b: Reg,
        c: Reg,
    },
    Len {
        a: Reg,
        b: Reg,
    },
    /// if (R[a] < R[b]) != k then skip the next instruction (a Jmp)
    Lt {
        a: Reg,
        b: Reg,
        k: bool,
    },
    Le {
        a: Reg,
        b: Reg,
        k: bool,
    },
    LtI {
        a: Reg,
        i: i16,
        k: bool,
    },
    Jmp {
        to: u32,
    },
    /// Integer numeric for loop: R[a]=init, R[a+1]=limit, R[a+2]=step, R[a+3]=control.
    ForPrep {
        a: Reg,
        exit: u32,
    },
    ForLoop {
        a: Reg,
        body: u32,
    },
    Closure {
        a: Reg,
        p: u16,
    },
    /// Close open upvalues at or above R[a].
    Close {
        a: Reg,
    },
    /// Call R[a] with R[a+1..=a+nargs]; the result (if any) lands in R[a].
    Call {
        a: Reg,
        nargs: u8,
        nres: u8,
    },
    Return {
        a: Reg,
        n: u8,
    },

    // Superinstructions produced by `fuse`. Each replaces `cmp; Jmp` and keeps
    // the dead Jmp in place so no offsets move: jump to `to` when cond == k,
    // otherwise skip both slots.
    LtJmp {
        a: Reg,
        b: Reg,
        k: bool,
        to: u32,
    },
    LeJmp {
        a: Reg,
        b: Reg,
        k: bool,
        to: u32,
    },
    LtIJmp {
        a: Reg,
        i: i16,
        k: bool,
        to: u32,
    },
    /// GetUpval + AddI + Call(1 arg, 1 result): the fib call shape.
    /// R[a] = U[u](R[b] + i)
    CallUpvalAddI {
        a: Reg,
        u: u8,
        b: Reg,
        i: i16,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub enum Const {
    Int(i64),
    Str(Rc<str>),
}

/// Where a new closure's upvalue comes from.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum UpvalDesc {
    /// A register of the enclosing function (becomes an open upvalue).
    Reg(Reg),
    /// An upvalue of the enclosing closure.
    Upval(u8),
}

#[derive(Debug)]
pub struct Proto {
    pub name: &'static str,
    pub nparams: u8,
    pub nregs: u8,
    pub code: Vec<Op>,
    pub consts: Vec<Const>,
    pub protos: Vec<Rc<Proto>>,
    pub upvals: Vec<UpvalDesc>,
}

/// A tiny assembler with forward labels.
#[derive(Debug, Default)]
pub struct Asm {
    code: Vec<Op>,
    consts: Vec<Const>,
    protos: Vec<Rc<Proto>>,
    patches: Vec<(usize, usize)>,
    labels: Vec<Option<u32>>,
}

#[derive(Clone, Copy, Debug)]
pub struct Label(usize);

impl Asm {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn op(&mut self, op: Op) -> &mut Self {
        self.code.push(op);
        self
    }
    pub fn here(&self) -> u32 {
        self.code.len() as u32
    }
    pub fn label(&mut self) -> Label {
        self.labels.push(None);
        Label(self.labels.len() - 1)
    }
    pub fn bind(&mut self, l: Label) {
        self.labels[l.0] = Some(self.here());
    }
    /// Emit an op whose target is `l`; the op's target field is patched later.
    pub fn op_to(&mut self, op: Op, l: Label) -> &mut Self {
        self.patches.push((self.code.len(), l.0));
        self.code.push(op);
        self
    }
    pub fn k_int(&mut self, v: i64) -> u16 {
        self.konst(Const::Int(v))
    }
    pub fn k_str(&mut self, s: &str) -> u16 {
        self.konst(Const::Str(s.into()))
    }
    fn konst(&mut self, c: Const) -> u16 {
        if let Some(i) = self.consts.iter().position(|x| *x == c) {
            return i as u16;
        }
        self.consts.push(c);
        (self.consts.len() - 1) as u16
    }
    pub fn proto(&mut self, p: Proto) -> u16 {
        self.protos.push(Rc::new(p));
        (self.protos.len() - 1) as u16
    }
    pub fn finish(
        mut self,
        name: &'static str,
        nparams: u8,
        nregs: u8,
        upvals: Vec<UpvalDesc>,
    ) -> Proto {
        for &(at, l) in &self.patches {
            let t = self.labels[l].expect("unbound label");
            match &mut self.code[at] {
                Op::Jmp { to } => *to = t,
                Op::ForPrep { exit, .. } => *exit = t,
                Op::ForLoop { body, .. } => *body = t,
                other => panic!("op {other:?} takes no label"),
            }
        }
        Proto {
            name,
            nparams,
            nregs,
            code: self.code,
            consts: self.consts,
            protos: self.protos,
            upvals,
        }
    }
}

/// Peephole pass that rewrites `cmp; Jmp` pairs (and the fib call shape) into
/// superinstructions. Instruction indexes never move.
pub fn fuse(p: &Proto) -> Proto {
    let mut targets = vec![false; p.code.len() + 1];
    for op in &p.code {
        match *op {
            Op::Jmp { to } => targets[to as usize] = true,
            Op::ForPrep { exit, .. } => targets[exit as usize] = true,
            Op::ForLoop { body, .. } => targets[body as usize] = true,
            _ => {}
        }
    }
    let mut code = p.code.clone();
    let mut i = 0;
    while i + 1 < code.len() {
        if let Op::Jmp { to } = code[i + 1]
            && !targets[i + 1]
        {
            let fused = match code[i] {
                Op::Lt { a, b, k } => Some(Op::LtJmp { a, b, k, to }),
                Op::Le { a, b, k } => Some(Op::LeJmp { a, b, k, to }),
                Op::LtI { a, i, k } => Some(Op::LtIJmp { a, i, k, to }),
                _ => None,
            };
            if let Some(f) = fused {
                code[i] = f;
                i += 2;
                continue;
            }
        }
        if i + 2 < code.len()
            && !targets[i + 1]
            && !targets[i + 2]
            && let (
                Op::GetUpval { a, u },
                Op::AddI { a: a2, b, i: imm },
                Op::Call {
                    a: a3,
                    nargs: 1,
                    nres: 1,
                },
            ) = (code[i], code[i + 1], code[i + 2])
            && a3 == a
            && a2 == a + 1
        {
            // Keep the three slots; only the first runs.
            code[i] = Op::CallUpvalAddI { a, u, b, i: imm };
            i += 3;
            continue;
        }
        i += 1;
    }
    Proto {
        name: p.name,
        nparams: p.nparams,
        nregs: p.nregs,
        code,
        consts: p.consts.clone(),
        protos: p.protos.iter().map(|c| Rc::new(fuse(c))).collect(),
        upvals: p.upvals.clone(),
    }
}

/// Lua's integer floor modulo.
pub fn lua_imod(a: i64, b: i64) -> i64 {
    let m = a.wrapping_rem(b);
    if m != 0 && (m ^ b) < 0 { m + b } else { m }
}

/// FxHash-style hasher (no dependency). Good enough for integer and cached
/// string-hash keys.
#[derive(Default, Clone, Copy, Debug)]
pub struct FxHasher(u64);

impl std::hash::Hasher for FxHasher {
    fn finish(&self) -> u64 {
        self.0
    }
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.write_u64(b as u64);
        }
    }
    fn write_u8(&mut self, n: u8) {
        self.write_u64(n as u64)
    }
    fn write_u32(&mut self, n: u32) {
        self.write_u64(n as u64)
    }
    fn write_u64(&mut self, n: u64) {
        self.0 = (self.0.rotate_left(5) ^ n).wrapping_mul(0x51_7c_c1_b7_27_22_0a_95);
    }
    fn write_i64(&mut self, n: i64) {
        self.write_u64(n as u64)
    }
    fn write_usize(&mut self, n: usize) {
        self.write_u64(n as u64)
    }
}

pub type FxBuild = std::hash::BuildHasherDefault<FxHasher>;

/// Hash for string contents (cached in each string object).
pub fn str_hash(b: &[u8]) -> u64 {
    // FNV-1a
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &x in b {
        h ^= x as u64;
        h = h.wrapping_mul(0x100_0000_01b3);
    }
    h
}

pub mod benches;

/// Runtime error. Boxed so `Result<Value, VmError>` stays small on the hot path.
#[derive(Debug)]
pub struct VmError(pub Box<str>);

impl From<&str> for VmError {
    #[cold]
    fn from(s: &str) -> Self {
        VmError(s.into())
    }
}

impl From<String> for VmError {
    #[cold]
    fn from(s: String) -> Self {
        VmError(s.into())
    }
}

impl std::fmt::Display for VmError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
