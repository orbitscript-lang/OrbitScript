//! The template backend: each instruction expands to a fixed x86-64
//! sequence (dynasm), with no optimizer and no IR. The same plan as the
//! Cranelift backend: registers stay in the register stack, the common
//! cases are inline behind tag checks, the rest calls the runtime's
//! helpers.
//!
//! Machine registers: r12 the VM, r13 the shared `JitCtx`, r14 the frame's
//! register window (reloaded from the `JitCtx` after every full helper),
//! r15 the running closure's handle bits.

use dynasmrt::x64::Assembler;
use dynasmrt::{DynamicLabel, DynasmApi, DynasmLabelApi, ExecutableBuffer, dynasm};

use orbit_runtime::object::{ProtoCode, Value};
use orbit_runtime::vm::op::Op;
use orbit_runtime::vm::{
    CTX_INTERRUPT, CTX_TABLES, CTX_TABLES_LEN, EXIT, HelperFn, Helpers, JitBackend, JitFn,
    NO_VALUE, TAKEN, const_parts,
};

use crate::layout::{self, Inline};
use crate::{entries, jump_target};

const TAG_NIL: i32 = 0;
const TAG_BOOL: i32 = 1;
const TAG_INT: i32 = 2;
const TAG_FLOAT: i32 = 3;
const TAG_STR: i32 = 4;
const TAG_TABLE: i32 = 5;
const TAG_CLOSURE: i32 = 6;

/// The template backend. Each function gets its own executable mapping,
/// kept until the backend (the runtime) is dropped.
#[derive(Default)]
pub struct Template {
    code: Vec<ExecutableBuffer>,
    bytes: usize,
}

impl std::fmt::Debug for Template {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Template")
            .field("functions", &self.code.len())
            .field("bytes", &self.bytes)
            .finish()
    }
}

impl Template {
    pub fn new() -> Option<Template> {
        Some(Template::default())
    }
}

impl JitBackend for Template {
    fn compile(&mut self, code: &ProtoCode, helpers: &Helpers) -> Option<JitFn> {
        if code.code.is_empty() {
            return None;
        }
        let mut ops = Assembler::new().ok()?;
        let start = ops.offset();
        let blocks: Vec<DynamicLabel> = (0..code.code.len() + 1)
            .map(|_| ops.new_dynamic_label())
            .collect();
        let ret = ops.new_dynamic_label();
        let mut t = Tr {
            ops,
            code,
            helpers,
            blocks,
            ret,
            polls: Vec::new(),
            inline: layout::inline(),
        };
        t.prologue();
        for p in 0..code.code.len() {
            let l = t.blocks[p];
            dynasm!(t.ops ; .arch x64 ; =>l);
            t.op(p);
        }
        // Past the end: never reached by valid code.
        let l = t.blocks[code.code.len()];
        dynasm!(t.ops ; .arch x64 ; =>l);
        t.exit(code.code.len());
        // Back edges poll the interrupt flag and leave at the loop header.
        for (pb, h) in std::mem::take(&mut t.polls) {
            let head = t.blocks[h];
            dynasm!(t.ops
                ; .arch x64
                ; =>pb
                ; mov rax, [r13 + CTX_INTERRUPT]
                ; cmp BYTE [rax], 0
                ; je =>head
            );
            t.exit(h);
        }
        t.epilogue();
        let buf = t.ops.finalize().ok()?;
        let addr = buf.ptr(start);
        self.bytes += buf.len();
        if std::env::var_os("ORBIT_JIT_PERFMAP").is_some() {
            use std::io::Write;
            if let Ok(mut f) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(format!("/tmp/perf-{}.map", std::process::id()))
            {
                let _ = writeln!(
                    f,
                    "{:x} {:x} orbit_tpl_{}",
                    addr as usize,
                    buf.len(),
                    self.code.len()
                );
            }
        }
        self.code.push(buf);
        // SAFETY: `addr` starts the function just assembled, which follows
        // the System V calling convention with `JitFn`'s arguments; its
        // buffer lives as long as the backend, which the runtime owns.
        Some(unsafe { std::mem::transmute::<*const u8, JitFn>(addr) })
    }

    fn code_bytes(&self) -> usize {
        self.bytes
    }
}

struct Tr<'a> {
    ops: Assembler,
    code: &'a ProtoCode,
    helpers: &'a Helpers,
    blocks: Vec<DynamicLabel>,
    ret: DynamicLabel,
    /// Back edges' interrupt polls: label, loop header.
    polls: Vec<(DynamicLabel, usize)>,
    /// The pinned table layout, when inline table access is on.
    inline: Option<&'static Inline>,
}

/// Where a register's tag and payload live, from r14.
fn at(r: u8) -> i32 {
    r as i32 * 16
}

fn pl(r: u8) -> i32 {
    r as i32 * 16 + 8
}

#[derive(Clone, Copy)]
enum Cmp {
    Lt,
    Le,
    Gt,
    Ge,
    Eq,
}

impl Tr<'_> {
    fn prologue(&mut self) {
        let entries = entries(self.code);
        let ret = self.ret;
        dynasm!(self.ops
            ; .arch x64
            ; push rbx
            ; push rbp
            ; push r12
            ; push r13
            ; push r14
            ; push r15
            ; sub rsp, 8
            ; mov r12, rdi
            ; mov r13, rsi
            ; mov r15, rcx
            ; mov r14, [r13]
        );
        for e in entries {
            let l = self.blocks[e];
            dynasm!(self.ops ; .arch x64 ; cmp edx, e as i32 ; je =>l);
        }
        dynasm!(self.ops
            ; .arch x64
            ; mov eax, edx
            ; shl rax, 8
            ; or rax, EXIT as i32
            ; jmp =>ret
        );
    }

    fn epilogue(&mut self) {
        let ret = self.ret;
        dynasm!(self.ops
            ; .arch x64
            ; =>ret
            ; add rsp, 8
            ; pop r15
            ; pop r14
            ; pop r13
            ; pop r12
            ; pop rbp
            ; pop rbx
            ; ret
        );
    }

    fn exit(&mut self, p: usize) {
        let ret = self.ret;
        let st = (EXIT | (p as u64) << 8) as i64;
        dynasm!(self.ops ; .arch x64 ; mov rax, QWORD st ; jmp =>ret);
    }

    fn next(&self, p: usize) -> DynamicLabel {
        self.blocks[p + 1]
    }

    /// Where a jump at `p` goes; a backward jump goes through a poll of
    /// the interrupt flag first.
    fn target(&mut self, p: usize, op: Op) -> DynamicLabel {
        let t = jump_target(p, op).expect("a jump");
        if t > p {
            return self.blocks[t];
        }
        let pb = self.ops.new_dynamic_label();
        self.polls.push((pb, t));
        pb
    }

    /// Call full helper `h` for instruction `p`, leaving its status in rax
    /// and the register window reloaded.
    fn helper(&mut self, h: HelperFn, p: usize) {
        let code = self.code as *const ProtoCode as i64;
        dynasm!(self.ops
            ; .arch x64
            ; mov rdi, r12
            ; mov rsi, r13
            ; mov rdx, QWORD code
            ; mov ecx, p as i32
            ; mov rax, QWORD h as usize as i64
            ; call rax
            ; mov r14, [r13]
        );
    }

    /// A full helper, then the next instruction unless it returned a status.
    fn helper_then_next(&mut self, h: HelperFn, p: usize) {
        self.helper(h, p);
        let ret = self.ret;
        let next = self.next(p);
        dynasm!(self.ops ; .arch x64 ; test rax, rax ; jnz =>ret ; jmp =>next);
    }

    /// A branch helper: 0 falls through, `TAKEN` jumps to `taken`.
    fn helper_branch(&mut self, h: HelperFn, p: usize, taken: DynamicLabel) {
        self.helper(h, p);
        let ret = self.ret;
        let next = self.next(p);
        dynasm!(self.ops
            ; .arch x64
            ; test rax, rax
            ; jz =>next
            ; cmp rax, TAKEN as i32
            ; je =>taken
            ; jmp =>ret
        );
    }

    /// Store tag `tag` and the payload in rax into `R[r]`.
    fn store_rax(&mut self, r: u8, tag: i32) {
        dynasm!(self.ops
            ; .arch x64
            ; mov QWORD [r14 + at(r)], tag
            ; mov [r14 + pl(r)], rax
        );
    }

    fn store_xmm0(&mut self, r: u8) {
        dynasm!(self.ops
            ; .arch x64
            ; mov QWORD [r14 + at(r)], TAG_FLOAT
            ; movsd [r14 + pl(r)], xmm0
        );
    }

    /// eax = both tags as `ta << 8 | tb`, for one compare.
    fn tags2(&mut self, a: u8, b: u8) {
        dynasm!(self.ops
            ; .arch x64
            ; movzx eax, BYTE [r14 + at(a)]
            ; movzx ecx, BYTE [r14 + at(b)]
            ; shl eax, 8
            ; or eax, ecx
        );
    }

    /// xmm1 = constant `f`.
    fn xmm1_const(&mut self, f: f64) {
        dynasm!(self.ops
            ; .arch x64
            ; mov rax, QWORD f.to_bits() as i64
            ; movq xmm1, rax
        );
    }

    /// Jump to `yes` or `no` on the flags of `ucomisd` per `c` (operands
    /// already in xmm0, xmm1).
    fn fbranch(&mut self, c: Cmp, yes: DynamicLabel, no: DynamicLabel) {
        match c {
            Cmp::Lt => dynasm!(self.ops ; .arch x64 ; ucomisd xmm1, xmm0 ; ja =>yes ; jmp =>no),
            Cmp::Le => dynasm!(self.ops ; .arch x64 ; ucomisd xmm1, xmm0 ; jae =>yes ; jmp =>no),
            Cmp::Gt => dynasm!(self.ops ; .arch x64 ; ucomisd xmm0, xmm1 ; ja =>yes ; jmp =>no),
            Cmp::Ge => dynasm!(self.ops ; .arch x64 ; ucomisd xmm0, xmm1 ; jae =>yes ; jmp =>no),
            Cmp::Eq => dynasm!(self.ops
                ; .arch x64
                ; ucomisd xmm0, xmm1
                ; jne =>no
                ; jp =>no
                ; jmp =>yes
            ),
        }
    }

    /// Integer compare of rax with rcx.
    fn ibranch(&mut self, c: Cmp, yes: DynamicLabel, no: DynamicLabel) {
        dynasm!(self.ops ; .arch x64 ; cmp rax, rcx);
        match c {
            Cmp::Lt => dynasm!(self.ops ; .arch x64 ; jl =>yes),
            Cmp::Le => dynasm!(self.ops ; .arch x64 ; jle =>yes),
            Cmp::Gt => dynasm!(self.ops ; .arch x64 ; jg =>yes),
            Cmp::Ge => dynasm!(self.ops ; .arch x64 ; jge =>yes),
            Cmp::Eq => dynasm!(self.ops ; .arch x64 ; je =>yes),
        }
        dynasm!(self.ops ; .arch x64 ; jmp =>no);
    }

    fn cmp_jump(&mut self, p: usize, op: Op, a: u8, b: u8, c: Cmp, want: bool) {
        let taken = self.target(p, op);
        let next = self.next(p);
        let (yes, no) = if want { (taken, next) } else { (next, taken) };
        let not_int = self.ops.new_dynamic_label();
        let slow = self.ops.new_dynamic_label();
        self.tags2(a, b);
        dynasm!(self.ops
            ; .arch x64
            ; cmp eax, TAG_INT << 8 | TAG_INT
            ; jne =>not_int
            ; mov rax, [r14 + pl(a)]
            ; mov rcx, [r14 + pl(b)]
        );
        self.ibranch(c, yes, no);
        dynasm!(self.ops
            ; .arch x64
            ; =>not_int
            ; cmp eax, TAG_FLOAT << 8 | TAG_FLOAT
            ; jne =>slow
            ; movsd xmm0, [r14 + pl(a)]
            ; movsd xmm1, [r14 + pl(b)]
        );
        self.fbranch(c, yes, no);
        dynasm!(self.ops ; .arch x64 ; =>slow);
        self.helper_branch(self.helpers.compare, p, taken);
    }

    fn cmp_imm_jump(&mut self, p: usize, op: Op, a: u8, imm: i8, c: Cmp, want: bool) {
        let taken = self.target(p, op);
        let next = self.next(p);
        let (yes, no) = if want { (taken, next) } else { (next, taken) };
        let not_int = self.ops.new_dynamic_label();
        let slow = self.ops.new_dynamic_label();
        dynasm!(self.ops
            ; .arch x64
            ; movzx eax, BYTE [r14 + at(a)]
            ; cmp eax, TAG_INT
            ; jne =>not_int
            ; mov rax, [r14 + pl(a)]
            ; mov rcx, imm as i32
        );
        self.ibranch(c, yes, no);
        dynasm!(self.ops
            ; .arch x64
            ; =>not_int
            ; cmp eax, TAG_FLOAT
            ; jne =>slow
        );
        self.xmm1_const(imm as f64);
        dynasm!(self.ops ; .arch x64 ; movsd xmm0, [r14 + pl(a)]);
        self.fbranch(c, yes, no);
        dynasm!(self.ops ; .arch x64 ; =>slow);
        if matches!(op, Op::JumpEqInt { .. }) {
            // Not a number: never equal.
            dynasm!(self.ops ; .arch x64 ; jmp =>no);
        } else {
            self.helper_branch(self.helpers.compare, p, taken);
        }
    }

    /// Binary arithmetic on `R[a]` and either `R[b]` or a number constant.
    fn arith(&mut self, p: usize, kind: Op, dst: u8, a: u8, b: Option<u8>, k: Option<Value>) {
        let next = self.next(p);
        let slow = self.ops.new_dynamic_label();
        let not_int = self.ops.new_dynamic_label();
        let is_div = matches!(kind, Op::Div { .. });
        // The tags: with a constant, only R[a]'s varies.
        match (b, k) {
            (Some(b), _) => self.tags2(a, b),
            (None, Some(Value::Int(_))) => dynasm!(self.ops
                ; .arch x64
                ; movzx eax, BYTE [r14 + at(a)]
                ; shl eax, 8
                ; or eax, TAG_INT
            ),
            (None, Some(Value::Float(_))) => dynasm!(self.ops
                ; .arch x64
                ; movzx eax, BYTE [r14 + at(a)]
                ; shl eax, 8
                ; or eax, TAG_FLOAT
            ),
            _ => {
                dynasm!(self.ops ; .arch x64 ; jmp =>slow);
                dynasm!(self.ops ; .arch x64 ; =>slow);
                return self.helper_then_next(self.helpers.arith, p);
            }
        }
        // Integers.
        dynasm!(self.ops
            ; .arch x64
            ; cmp eax, TAG_INT << 8 | TAG_INT
            ; jne =>not_int
            ; mov rax, [r14 + pl(a)]
        );
        match (b, k) {
            (Some(b), _) => dynasm!(self.ops ; .arch x64 ; mov rcx, [r14 + pl(b)]),
            (None, Some(Value::Int(i))) => dynasm!(self.ops ; .arch x64 ; mov rcx, QWORD i),
            _ => {}
        }
        if is_div {
            dynasm!(self.ops
                ; .arch x64
                ; cvtsi2sd xmm0, rax
                ; cvtsi2sd xmm1, rcx
                ; divsd xmm0, xmm1
            );
            self.store_xmm0(dst);
        } else {
            match kind {
                Op::Add { .. } => dynasm!(self.ops ; .arch x64 ; add rax, rcx),
                Op::Sub { .. } => dynasm!(self.ops ; .arch x64 ; sub rax, rcx),
                _ => dynasm!(self.ops ; .arch x64 ; imul rax, rcx),
            }
            self.store_rax(dst, TAG_INT);
        }
        dynasm!(self.ops ; .arch x64 ; jmp =>next ; =>not_int);
        // Floats, either side possibly an integer to convert.
        let a_int = self.ops.new_dynamic_label();
        let have_a = self.ops.new_dynamic_label();
        let b_int = self.ops.new_dynamic_label();
        let have_b = self.ops.new_dynamic_label();
        dynasm!(self.ops
            ; .arch x64
            ; mov ecx, eax
            ; shr ecx, 8
            ; cmp ecx, TAG_INT
            ; je =>a_int
            ; cmp ecx, TAG_FLOAT
            ; jne =>slow
            ; movsd xmm0, [r14 + pl(a)]
            ; jmp =>have_a
            ; =>a_int
            ; cvtsi2sd xmm0, QWORD [r14 + pl(a)]
            ; =>have_a
            ; and eax, 0xff
        );
        match (b, k) {
            (Some(b), _) => dynasm!(self.ops
                ; .arch x64
                ; cmp eax, TAG_INT
                ; je =>b_int
                ; cmp eax, TAG_FLOAT
                ; jne =>slow
                ; movsd xmm1, [r14 + pl(b)]
                ; jmp =>have_b
                ; =>b_int
                ; cvtsi2sd xmm1, QWORD [r14 + pl(b)]
                ; =>have_b
            ),
            (None, Some(Value::Int(i))) => self.xmm1_const(i as f64),
            (None, Some(Value::Float(f))) => self.xmm1_const(f),
            _ => unreachable!(),
        }
        match kind {
            Op::Add { .. } => dynasm!(self.ops ; .arch x64 ; addsd xmm0, xmm1),
            Op::Sub { .. } => dynasm!(self.ops ; .arch x64 ; subsd xmm0, xmm1),
            Op::Mul { .. } => dynasm!(self.ops ; .arch x64 ; mulsd xmm0, xmm1),
            _ => dynasm!(self.ops ; .arch x64 ; divsd xmm0, xmm1),
        }
        self.store_xmm0(dst);
        dynasm!(self.ops ; .arch x64 ; jmp =>next ; =>slow);
        self.helper_then_next(self.helpers.arith, p);
    }

    /// `R[dst] = K + R[b]` style: constant first. Commutative ops reuse
    /// `arith`; the others go through the helper unless both are integers
    /// or floats.
    fn karith(&mut self, p: usize, kind: Op, dst: u8, k: Value, b: u8) {
        match kind {
            Op::Add { .. } | Op::Mul { .. } => self.arith(p, kind, dst, b, None, Some(k)),
            _ => {
                // K - R[b], K / R[b]: floats only, the rest through the helper.
                let next = self.next(p);
                let slow = self.ops.new_dynamic_label();
                let f = match k {
                    Value::Float(f) => f,
                    Value::Int(i) => i as f64,
                    _ => {
                        return self.helper_then_next(self.helpers.arith, p);
                    }
                };
                let is_int_k = matches!(k, Value::Int(_));
                dynasm!(self.ops
                    ; .arch x64
                    ; movzx eax, BYTE [r14 + at(b)]
                    ; cmp eax, TAG_FLOAT
                    ; jne =>slow
                );
                if is_int_k && matches!(kind, Op::Sub { .. }) {
                    // int - float is a float either way.
                }
                self.xmm1_const(f);
                dynasm!(self.ops ; .arch x64 ; movapd xmm0, xmm1 ; movsd xmm1, [r14 + pl(b)]);
                match kind {
                    Op::Sub { .. } => dynasm!(self.ops ; .arch x64 ; subsd xmm0, xmm1),
                    _ => dynasm!(self.ops ; .arch x64 ; divsd xmm0, xmm1),
                }
                self.store_xmm0(dst);
                dynasm!(self.ops ; .arch x64 ; jmp =>next ; =>slow);
                self.helper_then_next(self.helpers.arith, p);
            }
        }
    }

    /// `R[dst] = R[a] + imm` (`SubInt` passes `-imm`).
    fn arith_imm(&mut self, p: usize, dst: u8, a: u8, imm: i64) {
        let next = self.next(p);
        let not_int = self.ops.new_dynamic_label();
        let slow = self.ops.new_dynamic_label();
        dynasm!(self.ops
            ; .arch x64
            ; movzx eax, BYTE [r14 + at(a)]
            ; cmp eax, TAG_INT
            ; jne =>not_int
            ; mov rax, [r14 + pl(a)]
            ; add rax, imm as i32
        );
        self.store_rax(dst, TAG_INT);
        dynasm!(self.ops
            ; .arch x64
            ; jmp =>next
            ; =>not_int
            ; cmp eax, TAG_FLOAT
            ; jne =>slow
        );
        self.xmm1_const(imm as f64);
        dynasm!(self.ops ; .arch x64 ; movsd xmm0, [r14 + pl(a)] ; addsd xmm0, xmm1);
        self.store_xmm0(dst);
        dynasm!(self.ops ; .arch x64 ; jmp =>next ; =>slow);
        self.helper_then_next(self.helpers.arith, p);
    }

    /// `//` or `%` on two integers (divisor not 0 or -1) inline.
    fn int_div(&mut self, p: usize, dst: u8, a: u8, b: Option<u8>, k: Option<i64>, modulo: bool) {
        let next = self.next(p);
        let slow = self.ops.new_dynamic_label();
        match (b, k) {
            (Some(b), _) => {
                self.tags2(a, b);
                dynasm!(self.ops
                    ; .arch x64
                    ; cmp eax, TAG_INT << 8 | TAG_INT
                    ; jne =>slow
                    ; mov rcx, [r14 + pl(b)]
                );
            }
            (None, Some(k)) => dynasm!(self.ops
                ; .arch x64
                ; movzx eax, BYTE [r14 + at(a)]
                ; cmp eax, TAG_INT
                ; jne =>slow
                ; mov rcx, QWORD k
            ),
            _ => {
                return self.helper_then_next(self.helpers.arith, p);
            }
        }
        dynasm!(self.ops
            ; .arch x64
            ; lea rax, [rcx + 1]
            ; cmp rax, 1
            ; jbe =>slow
            ; mov rax, [r14 + pl(a)]
            ; cqo
            ; idiv rcx
            // rax = quotient, rdx = remainder; floor when the remainder is
            // nonzero and its sign differs from the divisor's.
            ; test rdx, rdx
            ; jz >done
            ; mov r8, rdx
            ; xor r8, rcx
            ; jns >done
        );
        if modulo {
            dynasm!(self.ops ; .arch x64 ; add rdx, rcx ; done: ; mov rax, rdx);
        } else {
            dynasm!(self.ops ; .arch x64 ; dec rax ; done:);
        }
        self.store_rax(dst, TAG_INT);
        dynasm!(self.ops ; .arch x64 ; jmp =>next ; =>slow);
        self.helper_then_next(self.helpers.arith, p);
    }

    /// rax = 1 if `R[r]` is falsy, else 0.
    fn falsy(&mut self, r: u8) {
        dynasm!(self.ops
            ; .arch x64
            ; movzx ecx, BYTE [r14 + at(r)]
            ; xor eax, eax
            ; cmp ecx, TAG_NIL
            ; je >yes
            ; cmp ecx, TAG_BOOL
            ; jne >done
            ; cmp BYTE [r14 + pl(r)], 0
            ; jne >done
            ; yes:
            ; mov eax, 1
            ; done:
        );
    }

    /// rax = the arena slot of the table in `R[obj]` (known to hold a
    /// table), bounds- and occupancy-checked; `miss` otherwise. Clobbers rcx.
    fn table_slot(&mut self, l: &Inline, obj: u8, miss: DynamicLabel) {
        dynasm!(self.ops
            ; .arch x64
            ; mov eax, DWORD [r14 + pl(obj)]
            ; mov rcx, [r13 + CTX_TABLES_LEN]
            ; cmp rax, [rcx]
            ; jae =>miss
            ; imul rax, rax, l.slot_size as i32
            ; add rax, [r13 + CTX_TABLES]
        );
        let (at, v) = (l.empty_at, l.empty_value);
        match l.empty_width {
            1 => dynasm!(self.ops ; .arch x64 ; cmp BYTE [rax + at], v as u8 as i8),
            2 => dynasm!(self.ops ; .arch x64 ; cmp WORD [rax + at], v as u16 as i16),
            4 => dynasm!(self.ops ; .arch x64 ; cmp DWORD [rax + at], v as u32 as i32),
            _ => dynasm!(self.ops
                ; .arch x64
                ; mov rcx, QWORD v as i64
                ; cmp [rax + at], rcx
            ),
        }
        dynasm!(self.ops ; .arch x64 ; je =>miss);
    }

    /// rcx = the hash entry the field cache in rdx points at, if it holds
    /// the cache's key (payload bits `fkey`); `miss` otherwise. rax = the
    /// table's slot. Clobbers rsi.
    fn cached_entry(&mut self, l: &Inline, fkey: u64, miss: DynamicLabel) {
        dynasm!(self.ops
            ; .arch x64
            ; mov ecx, DWORD [rdx + l.cache_slot]
            ; cmp rcx, [rax + l.entries_len]
            ; jae =>miss
            ; imul rcx, rcx, l.entry_size as i32
            ; add rcx, [rax + l.entries_ptr]
            ; cmp BYTE [rcx + l.entry_key_tag], TAG_STR as i8
            ; jne =>miss
            ; mov rsi, QWORD fkey as i64
            ; cmp [rcx + l.entry_key_payload], rsi
            ; jne =>miss
        );
    }

    /// A fast table read: `fast(vm, table, third)` returns the value in
    /// rax:rdx, tag `NO_VALUE` if it declined; then `full`.
    fn fast_get(
        &mut self,
        p: usize,
        dst: u8,
        obj: u8,
        key: Option<KeyOp>,
        fast: usize,
        third: i64,
        fkey: u64,
        full: HelperFn,
    ) {
        let next = self.next(p);
        let slow = self.ops.new_dynamic_label();
        let lean = self.ops.new_dynamic_label();
        dynasm!(self.ops
            ; .arch x64
            ; cmp BYTE [r14 + at(obj)], TAG_TABLE as i8
            ; jne =>slow
        );
        match key {
            Some(KeyOp::Reg(k)) => dynasm!(self.ops
                ; .arch x64
                ; cmp BYTE [r14 + at(k)], TAG_INT as i8
                ; jne =>slow
                ; mov rdx, [r14 + pl(k)]
            ),
            Some(KeyOp::Int(i)) => dynasm!(self.ops ; .arch x64 ; mov rdx, QWORD i),
            None => dynasm!(self.ops ; .arch x64 ; mov rdx, QWORD third),
        }
        if let Some(l) = self.inline {
            self.table_slot(l, obj, lean);
            if key.is_some() {
                // rdx = the integer key.
                dynasm!(self.ops
                    ; .arch x64
                    ; lea rcx, [rdx - 1]
                    ; cmp rcx, [rax + l.array_cap]
                    ; jae =>lean
                    ; imul rcx, rcx, l.array_slot_size as i32
                    ; add rcx, [rax + l.array_ptr]
                    ; movzx esi, BYTE [rcx + l.array_slot_tag]
                    ; test esi, esi
                    ; jz =>lean
                    ; mov rdi, [rcx]
                );
            } else {
                // rdx = the field cache.
                self.cached_entry(l, fkey, lean);
                dynasm!(self.ops
                    ; .arch x64
                    ; movzx esi, BYTE [rcx + l.entry_value_tag]
                    ; test esi, esi
                    ; jz =>lean
                    ; mov rdi, [rcx + l.entry_value_payload]
                );
            }
            dynasm!(self.ops
                ; .arch x64
                ; mov [r14 + at(dst)], rsi
                ; mov [r14 + pl(dst)], rdi
                ; jmp =>next
            );
        }
        dynasm!(self.ops
            ; .arch x64
            ; =>lean
            ; mov rdi, r12
            ; mov rsi, [r14 + pl(obj)]
            ; mov rax, QWORD fast as i64
            ; call rax
            ; cmp rax, NO_VALUE as i32
            ; je =>slow
            ; mov [r14 + at(dst)], rax
            ; mov [r14 + pl(dst)], rdx
            ; jmp =>next
            ; =>slow
        );
        self.helper_then_next(full, p);
    }

    fn fast_set(
        &mut self,
        p: usize,
        obj: u8,
        key: Option<KeyOp>,
        src: u8,
        fast: usize,
        third: i64,
        fkey: u64,
        full: HelperFn,
    ) {
        let next = self.next(p);
        let slow = self.ops.new_dynamic_label();
        let lean = self.ops.new_dynamic_label();
        dynasm!(self.ops
            ; .arch x64
            ; cmp BYTE [r14 + at(obj)], TAG_TABLE as i8
            ; jne =>slow
        );
        match key {
            Some(KeyOp::Reg(k)) => dynasm!(self.ops
                ; .arch x64
                ; cmp BYTE [r14 + at(k)], TAG_INT as i8
                ; jne =>slow
                ; mov rdx, [r14 + pl(k)]
            ),
            Some(KeyOp::Int(i)) => dynasm!(self.ops ; .arch x64 ; mov rdx, QWORD i),
            None => dynasm!(self.ops ; .arch x64 ; mov rdx, QWORD third),
        }
        if let Some(l) = self.inline {
            // Numbers and booleans only: they need no write barrier.
            dynasm!(self.ops
                ; .arch x64
                ; movzx r8d, BYTE [r14 + at(src)]
                ; lea r9d, [r8 - TAG_BOOL]
                ; cmp r9d, TAG_FLOAT - TAG_BOOL
                ; ja =>lean
            );
            self.table_slot(l, obj, lean);
            if key.is_some() {
                dynasm!(self.ops
                    ; .arch x64
                    ; lea rcx, [rdx - 1]
                    ; mov r10d, DWORD [rax + l.array_len]
                    ; cmp rcx, r10
                    ; jae =>lean
                    ; imul rcx, rcx, l.array_slot_size as i32
                    ; add rcx, [rax + l.array_ptr]
                    ; cmp BYTE [rcx + l.array_slot_tag], TAG_NIL as i8
                    ; je =>lean
                    ; mov r9, [r14 + pl(src)]
                    ; mov [rcx], r9
                    ; mov [rcx + l.array_slot_tag], r8b
                    ; jmp =>next
                );
            } else {
                self.cached_entry(l, fkey, lean);
                dynasm!(self.ops
                    ; .arch x64
                    ; cmp BYTE [rcx + l.entry_value_tag], TAG_NIL as i8
                    ; je =>lean
                    ; mov r9, [r14 + pl(src)]
                    ; mov [rcx + l.entry_value_payload], r9
                    ; mov [rcx + l.entry_value_tag], r8b
                    ; mov WORD [rax + l.absent_meta], 0
                    ; jmp =>next
                );
            }
        }
        dynasm!(self.ops
            ; .arch x64
            ; =>lean
            ; mov rdi, r12
            ; mov rsi, [r14 + pl(obj)]
            ; movzx ecx, BYTE [r14 + at(src)]
            ; mov r8, [r14 + pl(src)]
            ; mov rax, QWORD fast as i64
            ; call rax
            ; test al, al
            ; jnz =>next
            ; =>slow
        );
        self.helper_then_next(full, p);
    }

    fn for_loop(&mut self, p: usize, op: Op, b: u8) {
        let back = self.target(p, op);
        let next = self.next(p);
        let float = self.ops.new_dynamic_label();
        let down = self.ops.new_dynamic_label();
        let go = self.ops.new_dynamic_label();
        dynasm!(self.ops
            ; .arch x64
            ; cmp BYTE [r14 + at(b)], TAG_INT as i8
            ; jne =>float
            ; mov rax, [r14 + pl(b + 1)]
            ; test rax, rax
            ; jle =>next
            ; dec rax
            ; mov [r14 + pl(b + 1)], rax
            ; mov rax, [r14 + pl(b)]
            ; add rax, [r14 + pl(b + 2)]
            ; mov [r14 + pl(b)], rax
            ; mov QWORD [r14 + at(b + 3)], TAG_INT
            ; mov [r14 + pl(b + 3)], rax
            ; jmp =>back
            ; =>float
            ; movsd xmm0, [r14 + pl(b)]
            ; addsd xmm0, [r14 + pl(b + 2)]
            ; movsd xmm1, [r14 + pl(b + 1)]
            ; movsd xmm3, [r14 + pl(b + 2)]
            ; xorpd xmm2, xmm2
            ; ucomisd xmm3, xmm2
            ; jbe =>down
            ; ucomisd xmm1, xmm0
            ; jae =>go
            ; jmp =>next
            ; =>down
            ; ucomisd xmm0, xmm1
            ; jae =>go
            ; jmp =>next
            ; =>go
            ; movsd [r14 + pl(b)], xmm0
        );
        self.store_xmm0(b + 3);
        dynasm!(self.ops ; .arch x64 ; jmp =>back);
    }

    fn konst(&self, k: u16) -> Value {
        self.code.consts[k as usize]
    }

    fn op(&mut self, p: usize) {
        let op = self.code.code[p];
        let h = self.helpers;
        let next = self.next(p);
        match op {
            Op::Move { dst, src } => {
                dynasm!(self.ops
                    ; .arch x64
                    ; mov rax, [r14 + at(src)]
                    ; mov rcx, [r14 + pl(src)]
                    ; mov [r14 + at(dst)], rax
                    ; mov [r14 + pl(dst)], rcx
                );
            }
            Op::LoadNil { dst, count } => {
                for r in dst..dst + count {
                    dynasm!(self.ops ; .arch x64 ; mov QWORD [r14 + at(r)], TAG_NIL);
                }
            }
            Op::LoadBool { dst, value } => {
                dynasm!(self.ops ; .arch x64 ; mov eax, value as i32);
                self.store_rax(dst, TAG_BOOL);
            }
            Op::LoadInt { dst, value } => {
                dynasm!(self.ops ; .arch x64 ; mov rax, QWORD value as i64);
                self.store_rax(dst, TAG_INT);
            }
            Op::LoadConst { dst, k } => self.load_const(dst, k as usize),
            Op::LoadConstX { dst, k } => self.load_const(dst, k as usize),
            Op::CapSelf { dst } => {
                dynasm!(self.ops ; .arch x64 ; mov rax, r15);
                self.store_rax(dst, TAG_CLOSURE);
            }

            Op::Add { dst, a, b }
            | Op::Sub { dst, a, b }
            | Op::Mul { dst, a, b }
            | Op::Div { dst, a, b } => self.arith(p, op, dst, a, Some(b), None),
            Op::AddK { dst, a, k } => {
                let k = self.konst(k);
                self.arith(p, Op::Add { dst, a, b: 0 }, dst, a, None, Some(k))
            }
            Op::SubK { dst, a, k } => {
                let k = self.konst(k);
                self.arith(p, Op::Sub { dst, a, b: 0 }, dst, a, None, Some(k))
            }
            Op::MulK { dst, a, k } => {
                let k = self.konst(k);
                self.arith(p, Op::Mul { dst, a, b: 0 }, dst, a, None, Some(k))
            }
            Op::DivK { dst, a, k } => {
                let k = self.konst(k);
                self.arith(p, Op::Div { dst, a, b: 0 }, dst, a, None, Some(k))
            }
            Op::KAdd { dst, k, b } => {
                let k = self.konst(k);
                self.karith(p, Op::Add { dst, a: 0, b }, dst, k, b)
            }
            Op::KSub { dst, k, b } => {
                let k = self.konst(k);
                self.karith(p, Op::Sub { dst, a: 0, b }, dst, k, b)
            }
            Op::KMul { dst, k, b } => {
                let k = self.konst(k);
                self.karith(p, Op::Mul { dst, a: 0, b }, dst, k, b)
            }
            Op::KDiv { dst, k, b } => {
                let k = self.konst(k);
                self.karith(p, Op::Div { dst, a: 0, b }, dst, k, b)
            }
            Op::AddInt { dst, a, imm } => self.arith_imm(p, dst, a, imm as i64),
            Op::SubInt { dst, a, imm } => self.arith_imm(p, dst, a, -(imm as i64)),
            Op::IDiv { dst, a, b } => self.int_div(p, dst, a, Some(b), None, false),
            Op::Mod { dst, a, b } => self.int_div(p, dst, a, Some(b), None, true),
            Op::IDivK { dst, a, k } | Op::ModK { dst, a, k } => {
                let modulo = matches!(op, Op::ModK { .. });
                let k = match self.konst(k) {
                    Value::Int(i) => Some(i),
                    _ => None,
                };
                self.int_div(p, dst, a, None, k, modulo)
            }
            Op::PowK { dst, a, k } => {
                let Value::Float(g) = self.konst(k) else {
                    return self.helper_then_next(h.arith, p);
                };
                let slow = self.ops.new_dynamic_label();
                dynasm!(self.ops
                    ; .arch x64
                    ; cmp BYTE [r14 + at(a)], TAG_FLOAT as i8
                    ; jne =>slow
                );
                self.xmm1_const(g);
                dynasm!(self.ops
                    ; .arch x64
                    ; movsd xmm0, [r14 + pl(a)]
                    ; mov rax, QWORD h.pow as usize as i64
                    ; call rax
                );
                self.store_xmm0(dst);
                dynasm!(self.ops ; .arch x64 ; jmp =>next ; =>slow);
                self.helper_then_next(h.arith, p)
            }
            Op::Pow { .. } => self.helper_then_next(h.arith, p),
            Op::BAnd { dst, a, b } | Op::BOr { dst, a, b } | Op::BXor { dst, a, b } => {
                let slow = self.ops.new_dynamic_label();
                self.tags2(a, b);
                dynasm!(self.ops
                    ; .arch x64
                    ; cmp eax, TAG_INT << 8 | TAG_INT
                    ; jne =>slow
                    ; mov rax, [r14 + pl(a)]
                );
                match op {
                    Op::BAnd { .. } => dynasm!(self.ops ; .arch x64 ; and rax, [r14 + pl(b)]),
                    Op::BOr { .. } => dynasm!(self.ops ; .arch x64 ; or rax, [r14 + pl(b)]),
                    _ => dynasm!(self.ops ; .arch x64 ; xor rax, [r14 + pl(b)]),
                }
                self.store_rax(dst, TAG_INT);
                dynasm!(self.ops ; .arch x64 ; jmp =>next ; =>slow);
                self.helper_then_next(h.bitwise, p)
            }
            Op::Shl { .. } | Op::Shr { .. } | Op::BNot { .. } => {
                self.helper_then_next(h.bitwise, p)
            }
            Op::Neg { dst, src } => {
                let not_int = self.ops.new_dynamic_label();
                let slow = self.ops.new_dynamic_label();
                dynasm!(self.ops
                    ; .arch x64
                    ; movzx eax, BYTE [r14 + at(src)]
                    ; cmp eax, TAG_INT
                    ; jne =>not_int
                    ; mov rax, [r14 + pl(src)]
                    ; neg rax
                );
                self.store_rax(dst, TAG_INT);
                dynasm!(self.ops
                    ; .arch x64
                    ; jmp =>next
                    ; =>not_int
                    ; cmp eax, TAG_FLOAT
                    ; jne =>slow
                    ; mov rax, [r14 + pl(src)]
                    ; btc rax, 63
                );
                self.store_rax(dst, TAG_FLOAT);
                dynasm!(self.ops ; .arch x64 ; jmp =>next ; =>slow);
                self.helper_then_next(h.arith, p)
            }
            Op::Not { dst, src } => {
                self.falsy(src);
                self.store_rax(dst, TAG_BOOL);
            }
            Op::Len { .. } => self.helper_then_next(h.len, p),
            Op::Concat { .. } => self.helper_then_next(h.concat, p),

            Op::Jump { .. } => {
                let t = self.target(p, op);
                dynasm!(self.ops ; .arch x64 ; jmp =>t);
            }
            Op::JumpIfTrue { src, .. } | Op::JumpIfFalse { src, .. } => {
                let t = self.target(p, op);
                self.falsy(src);
                dynasm!(self.ops ; .arch x64 ; test eax, eax);
                if matches!(op, Op::JumpIfFalse { .. }) {
                    dynasm!(self.ops ; .arch x64 ; jnz =>t);
                } else {
                    dynasm!(self.ops ; .arch x64 ; jz =>t);
                }
            }
            Op::JumpLt { a, b, .. } => self.cmp_jump(p, op, a, b, Cmp::Lt, true),
            Op::JumpNotLt { a, b, .. } => self.cmp_jump(p, op, a, b, Cmp::Lt, false),
            Op::JumpLe { a, b, .. } => self.cmp_jump(p, op, a, b, Cmp::Le, true),
            Op::JumpNotLe { a, b, .. } => self.cmp_jump(p, op, a, b, Cmp::Le, false),
            Op::JumpEq { a, b, .. } => self.cmp_jump(p, op, a, b, Cmp::Eq, true),
            Op::JumpNe { a, b, .. } => self.cmp_jump(p, op, a, b, Cmp::Eq, false),
            Op::JumpEqInt { a, imm, want, .. } => self.cmp_imm_jump(p, op, a, imm, Cmp::Eq, want),
            Op::JumpLtInt { a, imm, want, .. } => self.cmp_imm_jump(p, op, a, imm, Cmp::Lt, want),
            Op::JumpLeInt { a, imm, want, .. } => self.cmp_imm_jump(p, op, a, imm, Cmp::Le, want),
            Op::JumpGtInt { a, imm, want, .. } => self.cmp_imm_jump(p, op, a, imm, Cmp::Gt, want),
            Op::JumpGeInt { a, imm, want, .. } => self.cmp_imm_jump(p, op, a, imm, Cmp::Ge, want),

            Op::GetField { dst, obj, ic, .. } => {
                let c = &self.code.caches[ic as usize];
                let (cache, fkey) = (c as *const _ as i64, const_parts(Value::Str(c.key)).1);
                self.fast_get(
                    p,
                    dst,
                    obj,
                    None,
                    h.field_get as usize,
                    cache,
                    fkey,
                    h.get_field,
                )
            }
            Op::SetField { obj, src, ic, .. } => {
                let c = &self.code.caches[ic as usize];
                let (cache, fkey) = (c as *const _ as i64, const_parts(Value::Str(c.key)).1);
                self.fast_set(
                    p,
                    obj,
                    None,
                    src,
                    h.field_set as usize,
                    cache,
                    fkey,
                    h.set_field,
                )
            }
            Op::GetIndex { dst, obj, key } => {
                let k = Some(KeyOp::Reg(key));
                self.fast_get(p, dst, obj, k, h.array_get as usize, 0, 0, h.get_index)
            }
            Op::GetIndexInt { dst, obj, key } => {
                let k = Some(KeyOp::Int(key as i64));
                self.fast_get(p, dst, obj, k, h.array_get as usize, 0, 0, h.get_index)
            }
            Op::SetIndex { obj, key, src } => {
                let k = Some(KeyOp::Reg(key));
                self.fast_set(p, obj, k, src, h.array_set as usize, 0, 0, h.set_index)
            }
            Op::SetIndexInt { obj, key, src } => {
                let k = Some(KeyOp::Int(key as i64));
                self.fast_set(p, obj, k, src, h.array_set as usize, 0, 0, h.set_index)
            }
            Op::GetGlobal { dst, ic, .. } => {
                let cache = &self.code.caches[ic as usize] as *const _ as i64;
                let slow = self.ops.new_dynamic_label();
                dynasm!(self.ops
                    ; .arch x64
                    ; mov rdi, r12
                    ; mov rsi, r15
                    ; mov rdx, QWORD cache
                    ; mov rax, QWORD h.global_get as usize as i64
                    ; call rax
                    ; cmp rax, NO_VALUE as i32
                    ; je =>slow
                    ; mov [r14 + at(dst)], rax
                    ; mov [r14 + pl(dst)], rdx
                    ; jmp =>next
                    ; =>slow
                );
                self.helper_then_next(h.get_global, p)
            }
            Op::SetGlobal { .. } => self.helper_then_next(h.set_global, p),
            Op::SelfCall { .. } => self.helper_then_next(h.self_call, p),
            Op::NewTable { .. } => self.helper_then_next(h.new_table, p),
            Op::CellNew { .. }
            | Op::CellGet { .. }
            | Op::CellSet { .. }
            | Op::CellClear { .. }
            | Op::CapGet { .. }
            | Op::CapSet { .. } => self.helper_then_next(h.cell, p),
            Op::Closure { .. } => self.helper_then_next(h.closure, p),
            Op::SetList { .. } => self.helper_then_next(h.set_list, p),

            Op::Call { .. } | Op::GenericCall { .. } => self.helper_then_next(h.call, p),
            Op::TailCall { .. } => self.helper_then_next(h.tail_call, p),
            Op::Return { .. } | Op::Return0 | Op::Return1 { .. } => {
                self.helper(h.ret, p);
                let ret = self.ret;
                dynasm!(self.ops ; .arch x64 ; jmp =>ret);
            }
            Op::ForPrep { .. } => {
                let t = self.target(p, op);
                self.helper_branch(h.for_prep, p, t)
            }
            Op::ForLoop { base, .. } => self.for_loop(p, op, base),
            Op::GenericLoop { base, .. } => {
                let t = self.target(p, op);
                dynasm!(self.ops
                    ; .arch x64
                    ; cmp BYTE [r14 + at(base + 4)], TAG_NIL as i8
                    ; je =>next
                    ; mov rax, [r14 + at(base + 4)]
                    ; mov rcx, [r14 + pl(base + 4)]
                    ; mov [r14 + at(base + 2)], rax
                    ; mov [r14 + pl(base + 2)], rcx
                    ; jmp =>t
                );
            }

            // The rest runs in the interpreter.
            _ => self.exit(p),
        }
        // Simple instructions fall through to the next block.
    }

    fn load_const(&mut self, dst: u8, k: usize) {
        let (tag, bits) = const_parts(self.code.consts[k]);
        dynasm!(self.ops ; .arch x64 ; mov rax, QWORD bits as i64);
        self.store_rax(dst, tag as i32);
    }
}

#[derive(Clone, Copy)]
enum KeyOp {
    Reg(u8),
    Int(i64),
}
