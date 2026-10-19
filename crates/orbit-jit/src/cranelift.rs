//! The Cranelift backend.
//!
//! One Cranelift function per prototype: each instruction becomes its own
//! block, registers stay in the interpreter's register stack (loaded and
//! stored around every instruction, never cached across one), and the
//! common cases are inlined behind tag checks.

use cranelift_codegen::ir::condcodes::{FloatCC, IntCC};
use cranelift_codegen::ir::{
    AbiParam, Block, InstBuilder, MemFlagsData, SigRef, Signature, Value as CValue, types,
};
use cranelift_codegen::settings::{self, Configurable};
use cranelift_codegen::{Context, ir};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext, Switch, Variable};
use cranelift_jit::{JITBuilder, JITModule};
use cranelift_module::{Linkage, Module, default_libcall_names};

use orbit_runtime::object::{FieldCache, ProtoCode, Value};
use orbit_runtime::vm::op::{MULTI, Op};

use crate::layout::{self, Calls, Inline};
use crate::{entries, jump_target};
use orbit_runtime::vm::{
    CTX_CLOSURES, CTX_CLOSURES_LEN, CTX_DEPTH, CTX_INTERRUPT, CTX_TABLES, CTX_TABLES_LEN, EXIT,
    HelperFn, Helpers, JitBackend, JitFn, NO_VALUE, RET, TAKEN, const_parts,
};

const TAG_NIL: i64 = 0;
const TAG_BOOL: i64 = 1;
const TAG_INT: i64 = 2;
const TAG_FLOAT: i64 = 3;
const TAG_STR: i64 = 4;
const TAG_TABLE: i64 = 5;
const TAG_CLOSURE: i64 = 6;

/// The Cranelift backend: one JIT module per runtime; code lives until the
/// runtime is dropped.
pub struct Cranelift {
    module: JITModule,
    ctx: Context,
    fctx: FunctionBuilderContext,
    bytes: usize,
    count: usize,
}

impl std::fmt::Debug for Cranelift {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Cranelift")
            .field("functions", &self.count)
            .field("bytes", &self.bytes)
            .finish()
    }
}

impl Cranelift {
    /// A backend for the host machine, or `None` if Cranelift doesn't
    /// support it.
    pub fn new() -> Option<Cranelift> {
        let mut flags = settings::builder();
        flags.set("opt_level", "speed").ok()?;
        flags.set("is_pic", "false").ok()?;
        flags.set("use_colocated_libcalls", "false").ok()?;
        let isa = cranelift_native::builder()
            .ok()?
            .finish(settings::Flags::new(flags))
            .ok()?;
        let module = JITModule::new(JITBuilder::with_isa(isa, default_libcall_names()));
        let ctx = module.make_context();
        Some(Cranelift {
            module,
            ctx,
            fctx: FunctionBuilderContext::new(),
            bytes: 0,
            count: 0,
        })
    }
}

impl JitBackend for Cranelift {
    fn compile(&mut self, code: &ProtoCode, helpers: &Helpers) -> Option<JitFn> {
        let ptr = types::I64;
        let mut sig = self.module.make_signature();
        sig.params.push(AbiParam::new(ptr));
        sig.params.push(AbiParam::new(ptr));
        sig.params.push(AbiParam::new(types::I32));
        sig.params.push(AbiParam::new(types::I64));
        sig.returns.push(AbiParam::new(types::I64));
        self.module.clear_context(&mut self.ctx);
        self.ctx.func.signature = sig.clone();
        let name = format!("orbit_jit_{}", self.count);
        let id = self
            .module
            .declare_function(&name, Linkage::Local, &sig)
            .ok()?;
        {
            let mut b = FunctionBuilder::new(&mut self.ctx.func, &mut self.fctx);
            let call_conv = self.module.isa().default_call_conv();
            if !translate(&mut b, code, helpers, call_conv) {
                return None;
            }
            b.seal_all_blocks();
            b.finalize(self.module.isa().frontend_config());
        }
        self.module.define_function(id, &mut self.ctx).ok()?;
        let size = self
            .ctx
            .compiled_code()
            .map_or(0, |c| c.code_buffer().len());
        self.bytes += size;
        self.module.clear_context(&mut self.ctx);
        self.module.finalize_definitions().ok()?;
        self.count += 1;
        let addr = self.module.get_finalized_function(id);
        if std::env::var_os("ORBIT_JIT_PERFMAP").is_some() {
            use std::io::Write;
            if let Ok(mut f) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(format!("/tmp/perf-{}.map", std::process::id()))
            {
                let _ = writeln!(f, "{:x} {:x} {}", addr as usize, size, name);
            }
        }
        // SAFETY: `addr` is the entry of the function just finalized, built
        // with the signature above, which is `JitFn`'s with the system call
        // convention; the module keeps the code mapped for as long as the
        // backend lives, and the runtime owns the backend.
        Some(unsafe { std::mem::transmute::<*const u8, JitFn>(addr) })
    }

    fn code_bytes(&self) -> usize {
        self.bytes
    }
}

/// Where an operand comes from.
#[derive(Clone, Copy)]
enum Opnd {
    Reg(u8),
    Int(i64),
    Float(f64),
    /// A constant the inline paths don't handle.
    Other,
}

/// What an instruction's arithmetic does inline.
#[derive(Clone, Copy, PartialEq)]
enum Ar {
    Add,
    Sub,
    Mul,
    Div,
    /// Only the helper (IDiv, Mod, Pow).
    Slow,
}

struct Tr<'a, 'b> {
    b: &'a mut FunctionBuilder<'b>,
    code: &'a ProtoCode,
    blocks: Vec<Block>,
    regs: Variable,
    vm: CValue,
    ctx: CValue,
    closure: CValue,
    hsig: SigRef,
    powsig: SigRef,
    getsig: SigRef,
    setsig: SigRef,
    helpers: &'a Helpers,
    flags: MemFlagsData,
    /// Back edges' interrupt polls: block, loop header.
    polls: Vec<(Block, usize)>,
    /// Flags for loads and stores that may be unaligned (array slots).
    uflags: MemFlagsData,
    /// The pinned table layout, when inline table access is on.
    inline: Option<&'static Inline>,
    /// The pinned frame layout, when inline calls are on.
    calls: Option<&'static Calls>,
    /// Compiled functions, and [`Helpers::finish_call`].
    jsig: SigRef,
    fsig: SigRef,
}

/// What a table access indexes with.
#[derive(Clone, Copy)]
enum Key<'c> {
    /// An integer key, the array part first.
    Int(Opnd),
    /// A string constant through its field cache.
    Field(&'c FieldCache),
}

/// Build the function: an entry switch on the pc, then a block per
/// instruction.
fn translate(
    b: &mut FunctionBuilder<'_>,
    code: &ProtoCode,
    helpers: &Helpers,
    call_conv: cranelift_codegen::isa::CallConv,
) -> bool {
    let n = code.code.len();
    if n == 0 {
        return false;
    }
    let entry = b.create_block();
    b.append_block_params_for_function_params(entry);
    b.switch_to_block(entry);
    let params = b.block_params(entry).to_vec();
    let (vm, ctx, pc, closure) = (params[0], params[1], params[2], params[3]);

    let mut hs = Signature::new(call_conv);
    for t in [types::I64, types::I64, types::I64, types::I32] {
        hs.params.push(AbiParam::new(t));
    }
    hs.returns.push(AbiParam::new(types::I64));
    let hsig = b.import_signature(hs);
    let mut ps = Signature::new(call_conv);
    ps.params.push(AbiParam::new(types::F64));
    ps.params.push(AbiParam::new(types::F64));
    ps.returns.push(AbiParam::new(types::F64));
    let powsig = b.import_signature(ps);
    let mut gs = Signature::new(call_conv);
    for _ in 0..3 {
        gs.params.push(AbiParam::new(types::I64));
    }
    gs.returns.push(AbiParam::new(types::I64));
    gs.returns.push(AbiParam::new(types::I64));
    let getsig = b.import_signature(gs);
    let mut ss = Signature::new(call_conv);
    for _ in 0..5 {
        ss.params.push(AbiParam::new(types::I64));
    }
    ss.returns.push(AbiParam::new(types::I8));
    let setsig = b.import_signature(ss);
    let mut js = Signature::new(call_conv);
    for t in [types::I64, types::I64, types::I32, types::I64] {
        js.params.push(AbiParam::new(t));
    }
    js.returns.push(AbiParam::new(types::I64));
    let jsig = b.import_signature(js);
    let mut fs = Signature::new(call_conv);
    for _ in 0..3 {
        fs.params.push(AbiParam::new(types::I64));
    }
    fs.returns.push(AbiParam::new(types::I64));
    let fsig = b.import_signature(fs);

    let flags = MemFlagsData::trusted();
    let regs = b.declare_var(types::I64);
    let r0 = b.ins().load(types::I64, flags, ctx, 0);
    b.def_var(regs, r0);

    let blocks: Vec<Block> = (0..n).map(|_| b.create_block()).collect();

    let entries = entries(code);
    let unknown = b.create_block();
    let mut sw = Switch::new();
    for &e in &entries {
        sw.set_entry(e as u128, blocks[e]);
    }
    sw.emit(b, pc, unknown);
    b.switch_to_block(unknown);
    let pc64 = b.ins().uextend(types::I64, pc);
    let sh = b.ins().ishl_imm_u(pc64, 8);
    let st = b.ins().bor_imm_u(sh, EXIT as i64);
    b.ins().return_(&[st]);

    let mut t = Tr {
        b,
        code,
        blocks,
        regs,
        vm,
        ctx,
        closure,
        hsig,
        powsig,
        getsig,
        setsig,
        helpers,
        flags,
        polls: Vec::new(),
        uflags: MemFlagsData::new().with_notrap(),
        inline: layout::inline(),
        calls: layout::calls(),
        jsig,
        fsig,
    };
    for p in 0..n {
        t.b.switch_to_block(t.blocks[p]);
        t.op(p);
    }
    // Back edges poll the interrupt flag and leave at the loop header.
    for (pb, h) in std::mem::take(&mut t.polls) {
        t.b.switch_to_block(pb);
        let ptr = t.b.ins().load(types::I64, t.flags, t.ctx, CTX_INTERRUPT);
        let set = t.b.ins().uload8(types::I32, t.flags, ptr, 0);
        let out = t.b.create_block();
        t.b.ins().brif(set, out, &[], t.blocks[h], &[]);
        t.b.switch_to_block(out);
        t.exit(h);
    }
    true
}

impl Tr<'_, '_> {
    fn regs(&mut self) -> CValue {
        self.b.use_var(self.regs)
    }

    fn tag(&mut self, r: u8) -> CValue {
        let regs = self.regs();
        self.b
            .ins()
            .uload8(types::I64, self.flags, regs, r as i32 * 16)
    }

    fn payload(&mut self, r: u8, ty: ir::Type) -> CValue {
        let regs = self.regs();
        self.b.ins().load(ty, self.flags, regs, r as i32 * 16 + 8)
    }

    fn store(&mut self, r: u8, tag: i64, payload: CValue) {
        let regs = self.regs();
        let t = self.b.ins().iconst(types::I64, tag);
        self.b.ins().store(self.flags, t, regs, r as i32 * 16);
        self.b
            .ins()
            .store(self.flags, payload, regs, r as i32 * 16 + 8);
    }

    fn store_tag(&mut self, r: u8, tag: i64) {
        let regs = self.regs();
        let t = self.b.ins().iconst(types::I64, tag);
        self.b.ins().store(self.flags, t, regs, r as i32 * 16);
    }

    fn next(&self, p: usize) -> Option<Block> {
        self.blocks.get(p + 1).copied()
    }

    /// Fall through to instruction `p + 1`.
    fn fall(&mut self, p: usize) {
        match self.next(p) {
            Some(nb) => {
                self.b.ins().jump(nb, &[]);
            }
            None => self.exit(p + 1),
        }
    }

    fn exit(&mut self, p: usize) {
        let st = self
            .b
            .ins()
            .iconst(types::I64, (EXIT | (p as u64) << 8) as i64);
        self.b.ins().return_(&[st]);
    }

    /// The block a jump at `p` goes to; a backward jump goes through a
    /// poll of the interrupt flag first.
    fn target(&mut self, p: usize, op: Op) -> Block {
        let t = jump_target(p, op).expect("a jump");
        if t > p {
            return self.blocks[t];
        }
        let pb = self.b.create_block();
        self.polls.push((pb, t));
        pb
    }

    /// Call helper `h` for instruction `p`; returns its status, with the
    /// register address reloaded.
    fn helper(&mut self, h: HelperFn, p: usize) -> CValue {
        let callee = self.b.ins().iconst(types::I64, h as usize as i64);
        let code = self
            .b
            .ins()
            .iconst(types::I64, self.code as *const ProtoCode as i64);
        let pc = self.b.ins().iconst(types::I32, p as i64);
        let call = self
            .b
            .ins()
            .call_indirect(self.hsig, callee, &[self.vm, self.ctx, code, pc]);
        let r = self.b.inst_results(call)[0];
        let regs = self.b.ins().load(types::I64, self.flags, self.ctx, 0);
        self.b.def_var(self.regs, regs);
        r
    }

    /// Return status `r` unless it is 0; then go to `next`.
    fn check(&mut self, r: CValue, next: Option<Block>, p: usize) {
        let out = self.b.create_block();
        let go = match next {
            Some(nb) => nb,
            None => {
                let e = self.b.create_block();
                self.b.ins().brif(r, out, &[], e, &[]);
                self.b.switch_to_block(e);
                self.exit(p + 1);
                self.b.switch_to_block(out);
                self.b.ins().return_(&[r]);
                return;
            }
        };
        self.b.ins().brif(r, out, &[], go, &[]);
        self.b.switch_to_block(out);
        self.b.ins().return_(&[r]);
    }

    /// A helper call that goes on to the next instruction.
    fn helper_then_next(&mut self, h: HelperFn, p: usize) {
        let r = self.helper(h, p);
        let next = self.next(p);
        self.check(r, next, p);
    }

    /// A helper that answers a branch: 0 falls through, [`TAKEN`] jumps.
    fn helper_branch(&mut self, h: HelperFn, p: usize, taken: Block) {
        let r = self.helper(h, p);
        let nz = self.b.create_block();
        let fall = match self.next(p) {
            Some(nb) => nb,
            None => {
                let e = self.b.create_block();
                let cur = self.b.current_block().expect("block");
                self.b.switch_to_block(e);
                self.exit(p + 1);
                self.b.switch_to_block(cur);
                e
            }
        };
        self.b.ins().brif(r, nz, &[], fall, &[]);
        self.b.switch_to_block(nz);
        let is_taken = self.b.ins().icmp_imm_s(IntCC::Equal, r, TAKEN as i64);
        let out = self.b.create_block();
        self.b.ins().brif(is_taken, taken, &[], out, &[]);
        self.b.switch_to_block(out);
        self.b.ins().return_(&[r]);
    }

    /// Both tags equal `tag`.
    fn both(&mut self, ta: CValue, tb: CValue, tag: i64) -> CValue {
        let s = self.b.ins().ishl_imm_u(ta, 8);
        let c = self.b.ins().bor(s, tb);
        self.b.ins().icmp_imm_s(IntCC::Equal, c, tag << 8 | tag)
    }

    /// An operand's tag (constants give theirs).
    fn opnd_tag(&mut self, o: Opnd) -> CValue {
        match o {
            Opnd::Reg(r) => self.tag(r),
            Opnd::Int(_) => self.b.ins().iconst(types::I64, TAG_INT),
            Opnd::Float(_) => self.b.ins().iconst(types::I64, TAG_FLOAT),
            Opnd::Other => self.b.ins().iconst(types::I64, -1),
        }
    }

    fn opnd_int(&mut self, o: Opnd) -> CValue {
        match o {
            Opnd::Reg(r) => self.payload(r, types::I64),
            Opnd::Int(i) => self.b.ins().iconst(types::I64, i),
            // On a path the tag test never takes.
            _ => self.b.ins().iconst(types::I64, 0),
        }
    }

    fn konst(&self, k: u16) -> Opnd {
        match self.code.consts[k as usize] {
            Value::Int(i) => Opnd::Int(i),
            Value::Float(f) => Opnd::Float(f),
            _ => Opnd::Other,
        }
    }

    /// Binary arithmetic: integer and float cases inline, the rest through
    /// the helper.
    fn arith(&mut self, p: usize, ar: Ar, dst: u8, x: Opnd, y: Opnd) {
        let slow = self.b.create_block();
        if ar != Ar::Slow && !matches!(x, Opnd::Other) && !matches!(y, Opnd::Other) {
            let ta = self.opnd_tag(x);
            let tb = self.opnd_tag(y);
            let ints = self.both(ta, tb, TAG_INT);
            let int_b = self.b.create_block();
            let not_int = self.b.create_block();
            self.b.ins().brif(ints, int_b, &[], not_int, &[]);

            self.b.switch_to_block(int_b);
            let i = self.opnd_int(x);
            let j = self.opnd_int(y);
            match ar {
                Ar::Div => {
                    let f = self.b.ins().fcvt_from_sint(types::F64, i);
                    let g = self.b.ins().fcvt_from_sint(types::F64, j);
                    let v = self.b.ins().fdiv(f, g);
                    self.store(dst, TAG_FLOAT, v);
                }
                _ => {
                    let v = match ar {
                        Ar::Add => self.b.ins().iadd(i, j),
                        Ar::Sub => self.b.ins().isub(i, j),
                        _ => self.b.ins().imul(i, j),
                    };
                    self.store(dst, TAG_INT, v);
                }
            }
            self.fall(p);

            // Both numbers, at least one a float: convert the other.
            self.b.switch_to_block(not_int);
            let nx = self.is_number(ta);
            let ny = self.is_number(tb);
            let nums = self.b.ins().band(nx, ny);
            let float_b = self.b.create_block();
            self.b.ins().brif(nums, float_b, &[], slow, &[]);
            self.b.switch_to_block(float_b);
            let f = self.as_float(x, ta);
            let g = self.as_float(y, tb);
            let v = match ar {
                Ar::Add => self.b.ins().fadd(f, g),
                Ar::Sub => self.b.ins().fsub(f, g),
                Ar::Mul => self.b.ins().fmul(f, g),
                _ => self.b.ins().fdiv(f, g),
            };
            self.store(dst, TAG_FLOAT, v);
            self.fall(p);
        } else {
            self.b.ins().jump(slow, &[]);
        }
        self.b.switch_to_block(slow);
        self.helper_then_next(self.helpers.arith, p);
    }

    /// `R[a] < R[b]` style compare-and-jump on two registers.
    fn cmp_jump(&mut self, p: usize, op: Op, a: u8, b: u8, icc: IntCC, fcc: FloatCC, want: bool) {
        let taken = self.target(p, op);
        let fall = self.fall_block(p);
        let (yes, no) = if want { (taken, fall) } else { (fall, taken) };
        let ta = self.tag(a);
        let tb = self.tag(b);
        let ints = self.both(ta, tb, TAG_INT);
        let int_b = self.b.create_block();
        let not_int = self.b.create_block();
        self.b.ins().brif(ints, int_b, &[], not_int, &[]);
        self.b.switch_to_block(int_b);
        let i = self.payload(a, types::I64);
        let j = self.payload(b, types::I64);
        let c = self.b.ins().icmp(icc, i, j);
        self.b.ins().brif(c, yes, &[], no, &[]);
        self.b.switch_to_block(not_int);
        let floats = self.both(ta, tb, TAG_FLOAT);
        let float_b = self.b.create_block();
        let slow = self.b.create_block();
        self.b.ins().brif(floats, float_b, &[], slow, &[]);
        self.b.switch_to_block(float_b);
        let f = self.payload(a, types::F64);
        let g = self.payload(b, types::F64);
        let c = self.b.ins().fcmp(fcc, f, g);
        self.b.ins().brif(c, yes, &[], no, &[]);
        self.b.switch_to_block(slow);
        self.helper_branch(self.helpers.compare, p, taken);
    }

    /// `R[a] <op> imm` compare-and-jump.
    fn cmp_imm_jump(
        &mut self,
        p: usize,
        op: Op,
        a: u8,
        imm: i8,
        icc: IntCC,
        fcc: FloatCC,
        want: bool,
    ) {
        let taken = self.target(p, op);
        let fall = self.fall_block(p);
        let (yes, no) = if want { (taken, fall) } else { (fall, taken) };
        let ta = self.tag(a);
        let is_int = self.b.ins().icmp_imm_s(IntCC::Equal, ta, TAG_INT);
        let int_b = self.b.create_block();
        let not_int = self.b.create_block();
        self.b.ins().brif(is_int, int_b, &[], not_int, &[]);
        self.b.switch_to_block(int_b);
        let i = self.payload(a, types::I64);
        let c = self.b.ins().icmp_imm_s(icc, i, imm as i64);
        self.b.ins().brif(c, yes, &[], no, &[]);
        self.b.switch_to_block(not_int);
        let is_float = self.b.ins().icmp_imm_s(IntCC::Equal, ta, TAG_FLOAT);
        let float_b = self.b.create_block();
        let slow = self.b.create_block();
        self.b.ins().brif(is_float, float_b, &[], slow, &[]);
        self.b.switch_to_block(float_b);
        let f = self.payload(a, types::F64);
        let k = self.b.ins().f64const(imm as f64);
        let c = self.b.ins().fcmp(fcc, f, k);
        self.b.ins().brif(c, yes, &[], no, &[]);
        self.b.switch_to_block(slow);
        self.helper_branch(self.helpers.compare, p, taken);
    }

    /// The block after `p`, or one that exits there.
    fn fall_block(&mut self, p: usize) -> Block {
        match self.next(p) {
            Some(nb) => nb,
            None => {
                let cur = self.b.current_block().expect("block");
                let e = self.b.create_block();
                self.b.switch_to_block(e);
                self.exit(p + 1);
                self.b.switch_to_block(cur);
                e
            }
        }
    }

    /// Whether `R[r]` is falsy, as an i8.
    fn falsy(&mut self, r: u8) -> CValue {
        let t = self.tag(r);
        let is_nil = self.b.ins().icmp_imm_s(IntCC::Equal, t, TAG_NIL);
        let is_bool = self.b.ins().icmp_imm_s(IntCC::Equal, t, TAG_BOOL);
        let regs = self.regs();
        let v = self
            .b
            .ins()
            .uload8(types::I64, self.flags, regs, r as i32 * 16 + 8);
        let is_false = self.b.ins().icmp_imm_s(IntCC::Equal, v, 0);
        let f = self.b.ins().band(is_bool, is_false);
        self.b.ins().bor(is_nil, f)
    }

    /// Whether tag `t` is an integer's or a float's.
    fn is_number(&mut self, t: CValue) -> CValue {
        let k = self.b.ins().iadd_imm_s(t, -TAG_INT);
        self.b.ins().icmp_imm_u(IntCC::UnsignedLessThan, k, 2)
    }

    /// Operand `o`, a number with tag `t`, as a float.
    fn as_float(&mut self, o: Opnd, t: CValue) -> CValue {
        match o {
            Opnd::Int(i) => self.b.ins().f64const(i as f64),
            Opnd::Float(f) => self.b.ins().f64const(f),
            Opnd::Reg(r) => {
                let i = self.payload(r, types::I64);
                let as_f = self.b.ins().fcvt_from_sint(types::F64, i);
                let f = self.b.ins().bitcast(types::F64, MemFlagsData::new(), i);
                let is_int = self.b.ins().icmp_imm_s(IntCC::Equal, t, TAG_INT);
                self.b.ins().select(is_int, as_f, f)
            }
            Opnd::Other => unreachable!(),
        }
    }

    /// Store a fast helper's result, or go to `slow` when it declined.
    fn store_parts_or(&mut self, p: usize, dst: u8, t: CValue, v: CValue, slow: Block) {
        let declined = self.b.ins().icmp_imm_s(IntCC::Equal, t, NO_VALUE as i64);
        let ok = self.b.create_block();
        self.b.ins().brif(declined, slow, &[], ok, &[]);
        self.b.switch_to_block(ok);
        let regs = self.regs();
        self.b.ins().store(self.flags, t, regs, dst as i32 * 16);
        self.b.ins().store(self.flags, v, regs, dst as i32 * 16 + 8);
        self.fall(p);
    }

    /// The arena slot of the table in `R[obj]` (already known to hold a
    /// table), bounds- and occupancy-checked; `miss` otherwise.
    fn table_slot(&mut self, l: &Inline, obj: u8, miss: Block) -> CValue {
        let regs = self.regs();
        let idx = self.b.ins().uload32(self.flags, regs, obj as i32 * 16 + 8);
        let lenp = self
            .b
            .ins()
            .load(types::I64, self.flags, self.ctx, CTX_TABLES_LEN);
        let len = self.b.ins().load(types::I64, self.flags, lenp, 0);
        let inb = self.b.ins().icmp(IntCC::UnsignedLessThan, idx, len);
        let ok = self.b.create_block();
        self.b.ins().brif(inb, ok, &[], miss, &[]);
        self.b.switch_to_block(ok);
        let base = self
            .b
            .ins()
            .load(types::I64, self.flags, self.ctx, CTX_TABLES);
        let off = self.b.ins().imul_imm_s(idx, l.slot_size);
        let slot = self.b.ins().iadd(base, off);
        let mark = match l.empty_width {
            1 => self
                .b
                .ins()
                .uload8(types::I64, self.uflags, slot, l.empty_at),
            2 => self
                .b
                .ins()
                .uload16(types::I64, self.uflags, slot, l.empty_at),
            4 => self.b.ins().uload32(self.uflags, slot, l.empty_at),
            _ => self.b.ins().load(types::I64, self.uflags, slot, l.empty_at),
        };
        let empty = self
            .b
            .ins()
            .icmp_imm_s(IntCC::Equal, mark, l.empty_value as i64);
        let ok = self.b.create_block();
        self.b.ins().brif(empty, miss, &[], ok, &[]);
        self.b.switch_to_block(ok);
        slot
    }

    /// Continue in a new block when `cond` holds, else go to `miss`.
    fn guard(&mut self, cond: CValue, miss: Block) {
        let ok = self.b.create_block();
        self.b.ins().brif(cond, ok, &[], miss, &[]);
        self.b.switch_to_block(ok);
    }

    /// `R[dst] = tag, payload`.
    fn store_dyn(&mut self, dst: u8, t: CValue, v: CValue) {
        let regs = self.regs();
        self.b.ins().store(self.flags, t, regs, dst as i32 * 16);
        self.b.ins().store(self.flags, v, regs, dst as i32 * 16 + 8);
    }

    /// The inline read of a present value: the array slot or cached hash
    /// entry, stored in `R[dst]`; `miss` for anything else.
    fn inline_get(&mut self, l: &Inline, dst: u8, obj: u8, key: Key, miss: Block) {
        let slot = self.table_slot(l, obj, miss);
        let (t, v) = match key {
            Key::Int(k) => {
                let k = self.opnd_int(k);
                let arr = self.b.ins().load(types::I64, self.flags, slot, l.array_ptr);
                let cap = self.b.ins().load(types::I64, self.flags, slot, l.array_cap);
                let k1 = self.b.ins().iadd_imm_s(k, -1);
                let inb = self.b.ins().icmp(IntCC::UnsignedLessThan, k1, cap);
                self.guard(inb, miss);
                let off = self.b.ins().imul_imm_s(k1, l.array_slot_size);
                let a = self.b.ins().iadd(arr, off);
                let t = self
                    .b
                    .ins()
                    .uload8(types::I64, self.uflags, a, l.array_slot_tag);
                let live = self.b.ins().icmp_imm_s(IntCC::NotEqual, t, TAG_NIL);
                self.guard(live, miss);
                let v = self.b.ins().load(types::I64, self.uflags, a, 0);
                (t, v)
            }
            Key::Field(c) => {
                let e = self.cached_entry(l, slot, c, miss);
                let t = self
                    .b
                    .ins()
                    .uload8(types::I64, self.flags, e, l.entry_value_tag);
                let live = self.b.ins().icmp_imm_s(IntCC::NotEqual, t, TAG_NIL);
                self.guard(live, miss);
                let v = self
                    .b
                    .ins()
                    .load(types::I64, self.flags, e, l.entry_value_payload);
                (t, v)
            }
        };
        self.store_dyn(dst, t, v);
    }

    /// The hash entry field cache `c` points at, if it holds `c`'s key.
    fn cached_entry(&mut self, l: &Inline, slot: CValue, c: &FieldCache, miss: Block) -> CValue {
        let ca = self
            .b
            .ins()
            .iconst(types::I64, c as *const FieldCache as i64);
        let cs = self.b.ins().uload32(self.flags, ca, l.cache_slot);
        let ep = self
            .b
            .ins()
            .load(types::I64, self.flags, slot, l.entries_ptr);
        let el = self
            .b
            .ins()
            .load(types::I64, self.flags, slot, l.entries_len);
        let inb = self.b.ins().icmp(IntCC::UnsignedLessThan, cs, el);
        self.guard(inb, miss);
        let off = self.b.ins().imul_imm_s(cs, l.entry_size);
        let e = self.b.ins().iadd(ep, off);
        let kt = self
            .b
            .ins()
            .uload8(types::I64, self.flags, e, l.entry_key_tag);
        let is_str = self.b.ins().icmp_imm_s(IntCC::Equal, kt, TAG_STR);
        self.guard(is_str, miss);
        let kp = self
            .b
            .ins()
            .load(types::I64, self.flags, e, l.entry_key_payload);
        let want = const_parts(Value::Str(c.key)).1;
        let same = self.b.ins().icmp_imm_s(IntCC::Equal, kp, want as i64);
        self.guard(same, miss);
        e
    }

    /// The inline store of a number or boolean over a present value (no
    /// write barrier needed: it refers to no object); `miss` otherwise.
    fn inline_set(&mut self, l: &Inline, obj: u8, key: Key, src: u8, miss: Block) {
        let regs = self.regs();
        let vt = self
            .b
            .ins()
            .uload8(types::I64, self.flags, regs, src as i32 * 16);
        let vt1 = self.b.ins().iadd_imm_s(vt, -TAG_BOOL);
        let plain =
            self.b
                .ins()
                .icmp_imm_u(IntCC::UnsignedLessThanOrEqual, vt1, TAG_FLOAT - TAG_BOOL);
        self.guard(plain, miss);
        let slot = self.table_slot(l, obj, miss);
        let vp = self.payload(src, types::I64);
        match key {
            Key::Int(k) => {
                let k = self.opnd_int(k);
                let len = self.b.ins().uload32(self.flags, slot, l.array_len);
                let k1 = self.b.ins().iadd_imm_s(k, -1);
                let inb = self.b.ins().icmp(IntCC::UnsignedLessThan, k1, len);
                self.guard(inb, miss);
                let arr = self.b.ins().load(types::I64, self.flags, slot, l.array_ptr);
                let off = self.b.ins().imul_imm_s(k1, l.array_slot_size);
                let a = self.b.ins().iadd(arr, off);
                let old = self
                    .b
                    .ins()
                    .uload8(types::I64, self.uflags, a, l.array_slot_tag);
                let live = self.b.ins().icmp_imm_s(IntCC::NotEqual, old, TAG_NIL);
                self.guard(live, miss);
                self.b.ins().store(self.uflags, vp, a, 0);
                self.b.ins().istore8(self.uflags, vt, a, l.array_slot_tag);
            }
            Key::Field(c) => {
                let e = self.cached_entry(l, slot, c, miss);
                let old = self
                    .b
                    .ins()
                    .uload8(types::I64, self.flags, e, l.entry_value_tag);
                let live = self.b.ins().icmp_imm_s(IntCC::NotEqual, old, TAG_NIL);
                self.guard(live, miss);
                self.b.ins().store(self.flags, vp, e, l.entry_value_payload);
                self.b.ins().istore8(self.flags, vt, e, l.entry_value_tag);
                let zero = self.b.ins().iconst(types::I16, 0);
                self.b.ins().store(self.flags, zero, slot, l.absent_meta);
            }
        }
    }

    /// Whether the arena slot at `slot` holds an object: go to `miss` when
    /// it carries the empty marker.
    fn occupied(&mut self, slot: CValue, at: i32, width: u8, value: u64, miss: Block) {
        let mark = match width {
            1 => self.b.ins().uload8(types::I64, self.uflags, slot, at),
            2 => self.b.ins().uload16(types::I64, self.uflags, slot, at),
            4 => self.b.ins().uload32(self.uflags, slot, at),
            _ => self.b.ins().load(types::I64, self.uflags, slot, at),
        };
        let empty = self.b.ins().icmp_imm_s(IntCC::Equal, mark, value as i64);
        let ok = self.b.create_block();
        self.b.ins().brif(empty, miss, &[], ok, &[]);
        self.b.switch_to_block(ok);
    }

    /// `R[func](args)` for a compiled Lua function with exactly `nargs`
    /// parameters and no captured locals: pushes its frame in place and
    /// calls its code directly. Anything else goes through the `call`
    /// helper.
    fn inline_call(&mut self, p: usize, l: &Calls, func: u8, nargs: u8, want: u8) {
        let slow = self.b.create_block();
        let fl_ = self.flags;
        let t = self.tag(func);
        let is_c = self.b.ins().icmp_imm_s(IntCC::Equal, t, TAG_CLOSURE);
        self.guard(is_c, slow);
        let regs = self.regs();
        let cbits = self
            .b
            .ins()
            .load(types::I64, fl_, regs, func as i32 * 16 + 8);
        let cidx = self.b.ins().uload32(fl_, regs, func as i32 * 16 + 8);
        let lenp = self
            .b
            .ins()
            .load(types::I64, fl_, self.ctx, CTX_CLOSURES_LEN);
        let len = self.b.ins().load(types::I64, fl_, lenp, 0);
        let inb = self.b.ins().icmp(IntCC::UnsignedLessThan, cidx, len);
        self.guard(inb, slow);
        let cbase = self.b.ins().load(types::I64, fl_, self.ctx, CTX_CLOSURES);
        let off = self.b.ins().imul_imm_s(cidx, l.closure_slot_size);
        let slot = self.b.ins().iadd(cbase, off);
        self.occupied(slot, l.empty_at, l.empty_width, l.empty_value, slow);
        let rcp = self.b.ins().load(types::I64, fl_, slot, l.closure_code);
        let code = self.b.ins().iadd_imm_s(rcp, l.rc_value as i64);
        let entry = self.b.ins().load(types::I64, fl_, code, l.proto_entry);
        let compiled = self.b.ins().icmp_imm_s(IntCC::NotEqual, entry, 0);
        self.guard(compiled, slow);
        let np = self.b.ins().uload8(types::I64, fl_, code, l.proto_nparams);
        let same = self.b.ins().icmp_imm_s(IntCC::Equal, np, nargs as i64);
        self.guard(same, slow);
        let va = self
            .b
            .ins()
            .uload8(types::I64, fl_, code, l.proto_is_vararg);
        let nc = self.b.ins().uload8(types::I64, fl_, code, l.proto_ncells);
        let either = self.b.ins().bor(va, nc);
        let plain = self.b.ins().icmp_imm_s(IntCC::Equal, either, 0);
        self.guard(plain, slow);
        let nr = self.b.ins().uload8(types::I64, fl_, code, l.proto_nregs);
        let depth = self.b.ins().uload32(fl_, self.ctx, CTX_DEPTH);
        let shallow = self
            .b
            .ins()
            .icmp_imm_u(IntCC::UnsignedLessThan, depth, l.max_depth as i64);
        self.guard(shallow, slow);
        let vm = self.vm;
        let fp = self.b.ins().load(types::I64, fl_, vm, l.frames_ptr);
        let fln = self.b.ins().load(types::I64, fl_, vm, l.frames_len);
        let fcap = self.b.ins().load(types::I64, fl_, vm, l.frames_cap);
        let room = self.b.ins().icmp(IntCC::UnsignedLessThan, fln, fcap);
        self.guard(room, slow);
        let top_off = self.b.ins().imul_imm_s(fln, l.frame_size);
        let nf = self.b.ins().iadd(fp, top_off);
        let caller = self.b.ins().iadd_imm_s(nf, -l.frame_size);
        let cb = self.b.ins().uload32(fl_, caller, l.frame_base);
        let fabs = self.b.ins().iadd_imm_s(cb, func as i64);
        let nb = self.b.ins().iadd_imm_s(fabs, 1);
        let na = self.b.ins().iconst(types::I64, nargs as i64);
        let big = self.b.ins().umax(nr, na);
        let need = self.b.ins().iadd(nb, big);
        let maxs = self.b.ins().load(types::I64, fl_, vm, l.max_stack);
        let fits = self
            .b
            .ins()
            .icmp(IntCC::UnsignedLessThanOrEqual, need, maxs);
        self.guard(fits, slow);
        let rl = self.b.ins().load(types::I64, fl_, vm, l.regs_len);
        let wend = self.b.ins().iadd_imm_s(nb, l.window);
        let whole = self.b.ins().icmp(IntCC::UnsignedLessThanOrEqual, wend, rl);
        self.guard(whole, slow);
        // Commit: the caller's return pc, then the callee's frame.
        let ret_pc = self.b.ins().iconst(types::I64, p as i64 + 1);
        self.b.ins().istore32(fl_, ret_pc, caller, l.frame_pc);
        let zero = self.b.ins().iconst(types::I64, 0);
        let tag = self.b.ins().iconst(types::I64, l.lua_kind_tag as i64);
        self.b.ins().istore8(fl_, tag, nf, l.frame_kind);
        self.b.ins().istore32(fl_, cidx, nf, l.frame_closure);
        self.b.ins().store(fl_, code, nf, l.frame_code_id);
        self.b.ins().istore32(fl_, fabs, nf, l.frame_func);
        self.b.ins().istore32(fl_, nb, nf, l.frame_base);
        self.b.ins().istore32(fl_, zero, nf, l.frame_pc);
        let cl = self.b.ins().load(types::I64, fl_, vm, l.cells_len);
        self.b.ins().istore32(fl_, cl, nf, l.frame_cell_base);
        self.b.ins().istore32(fl_, zero, nf, l.frame_nvarargs);
        let w = self.b.ins().iconst(types::I64, want as i64);
        self.b.ins().istore8(fl_, w, nf, l.frame_want);
        self.b.ins().istore8(fl_, zero, nf, l.frame_tail);
        self.b.ins().istore8(fl_, zero, nf, l.frame_extra);
        self.b.ins().istore8(fl_, zero, nf, l.frame_cdepth);
        let fl1 = self.b.ins().iadd_imm_s(fln, 1);
        self.b.ins().store(fl_, fl1, vm, l.frames_len);
        let rp = self.b.ins().load(types::I64, fl_, vm, l.regs_ptr);
        let woff = self.b.ins().ishl_imm_u(nb, 4);
        let win = self.b.ins().iadd(rp, woff);
        self.b.ins().store(fl_, win, self.ctx, 0);
        let d1 = self.b.ins().iadd_imm_s(depth, 1);
        self.b.ins().istore32(fl_, d1, self.ctx, CTX_DEPTH);
        let pc0 = self.b.ins().iconst(types::I32, 0);
        let call = self
            .b
            .ins()
            .call_indirect(self.jsig, entry, &[vm, self.ctx, pc0, cbits]);
        let st = self.b.inst_results(call)[0];
        self.b.ins().istore32(fl_, depth, self.ctx, CTX_DEPTH);
        let r0 = self.b.ins().load(types::I64, fl_, self.ctx, 0);
        self.b.def_var(self.regs, r0);
        let is_ret = self.b.ins().icmp_imm_s(IntCC::Equal, st, RET as i64);
        let done = self.b.create_block();
        let other = self.b.create_block();
        self.b.ins().brif(is_ret, done, &[], other, &[]);
        self.b.switch_to_block(other);
        let fin = self
            .b
            .ins()
            .iconst(types::I64, self.helpers.finish_call as usize as i64);
        let call = self
            .b
            .ins()
            .call_indirect(self.fsig, fin, &[vm, self.ctx, st]);
        let r = self.b.inst_results(call)[0];
        let r0 = self.b.ins().load(types::I64, fl_, self.ctx, 0);
        self.b.def_var(self.regs, r0);
        let next = self.next(p);
        self.check(r, next, p);
        self.b.switch_to_block(done);
        self.fall(p);
        self.b.switch_to_block(slow);
        self.helper_then_next(self.helpers.call, p);
    }

    /// `Return0` / `Return1` into a Lua caller wanting at most one result,
    /// with no captured locals to drop: pops the frame in place and returns
    /// [`RET`]. Anything else goes to `slow`.
    fn inline_ret(&mut self, l: &Calls, src: Option<u8>, slow: Block) {
        let fl_ = self.flags;
        let vm = self.vm;
        let fp = self.b.ins().load(types::I64, fl_, vm, l.frames_ptr);
        let fln = self.b.ins().load(types::I64, fl_, vm, l.frames_len);
        let two = self
            .b
            .ins()
            .icmp_imm_u(IntCC::UnsignedGreaterThanOrEqual, fln, 2);
        self.guard(two, slow);
        let fl1 = self.b.ins().iadd_imm_s(fln, -1);
        let off = self.b.ins().imul_imm_s(fl1, l.frame_size);
        let f = self.b.ins().iadd(fp, off);
        let caller = self.b.ins().iadd_imm_s(f, -l.frame_size);
        let ck = self.b.ins().uload8(types::I64, fl_, caller, l.frame_kind);
        let lua = self
            .b
            .ins()
            .icmp_imm_s(IntCC::Equal, ck, l.lua_kind_tag as i64);
        self.guard(lua, slow);
        let want = self.b.ins().uload8(types::I64, fl_, f, l.frame_want);
        let small = self
            .b
            .ins()
            .icmp_imm_u(IntCC::UnsignedLessThanOrEqual, want, 1);
        self.guard(small, slow);
        let cb = self.b.ins().uload32(fl_, f, l.frame_cell_base);
        let cl = self.b.ins().load(types::I64, fl_, vm, l.cells_len);
        let nocells = self.b.ins().icmp(IntCC::Equal, cb, cl);
        self.guard(nocells, slow);
        let func = self.b.ins().uload32(fl_, f, l.frame_func);
        let rp = self.b.ins().load(types::I64, fl_, vm, l.regs_ptr);
        let foff = self.b.ins().ishl_imm_u(func, 4);
        let dst = self.b.ins().iadd(rp, foff);
        let copy = self.b.create_block();
        let after = self.b.create_block();
        self.b.ins().brif(want, copy, &[], after, &[]);
        self.b.switch_to_block(copy);
        match src {
            Some(s) => {
                let regs = self.regs();
                let t = self.b.ins().load(types::I64, fl_, regs, s as i32 * 16);
                let v = self.b.ins().load(types::I64, fl_, regs, s as i32 * 16 + 8);
                self.b.ins().store(fl_, t, dst, 0);
                self.b.ins().store(fl_, v, dst, 8);
            }
            None => {
                let z = self.b.ins().iconst(types::I64, TAG_NIL);
                self.b.ins().store(fl_, z, dst, 0);
            }
        }
        self.b.ins().jump(after, &[]);
        self.b.switch_to_block(after);
        let top = self.b.ins().iadd(func, want);
        self.b.ins().store(fl_, top, vm, l.top);
        self.b.ins().store(fl_, fl1, vm, l.frames_len);
        let cbase = self.b.ins().uload32(fl_, caller, l.frame_base);
        let coff = self.b.ins().ishl_imm_u(cbase, 4);
        let win = self.b.ins().iadd(rp, coff);
        self.b.ins().store(fl_, win, self.ctx, 0);
        let r = self.b.ins().iconst(types::I64, RET as i64);
        self.b.ins().return_(&[r]);
    }

    /// A table read: inline when the layout is pinned, then through fast
    /// helper `fast`, then through the full helper `full`.
    fn fast_get(&mut self, p: usize, dst: u8, obj: u8, key: Key, fast: usize, full: HelperFn) {
        let slow = self.b.create_block();
        let go = self.b.create_block();
        let to = self.tag(obj);
        let mut ok = self.b.ins().icmp_imm_s(IntCC::Equal, to, TAG_TABLE);
        if let Key::Int(Opnd::Reg(k)) = key {
            let tk = self.tag(k);
            let ik = self.b.ins().icmp_imm_s(IntCC::Equal, tk, TAG_INT);
            ok = self.b.ins().band(ok, ik);
        }
        self.b.ins().brif(ok, go, &[], slow, &[]);
        self.b.switch_to_block(go);
        if let Some(l) = self.inline {
            let lean = self.b.create_block();
            self.inline_get(l, dst, obj, key, lean);
            self.fall(p);
            self.b.switch_to_block(lean);
        }
        let t = self.payload(obj, types::I64);
        let third = match key {
            Key::Int(k) => self.opnd_int(k),
            Key::Field(c) => self
                .b
                .ins()
                .iconst(types::I64, c as *const FieldCache as i64),
        };
        let callee = self.b.ins().iconst(types::I64, fast as i64);
        let call = self
            .b
            .ins()
            .call_indirect(self.getsig, callee, &[self.vm, t, third]);
        let (rt, rv) = (self.b.inst_results(call)[0], self.b.inst_results(call)[1]);
        self.store_parts_or(p, dst, rt, rv, slow);
        self.b.switch_to_block(slow);
        self.helper_then_next(full, p);
    }

    /// A table store, as [`Tr::fast_get`].
    fn fast_set(&mut self, p: usize, obj: u8, key: Key, src: u8, fast: usize, full: HelperFn) {
        let slow = self.b.create_block();
        let go = self.b.create_block();
        let to = self.tag(obj);
        let mut ok = self.b.ins().icmp_imm_s(IntCC::Equal, to, TAG_TABLE);
        if let Key::Int(Opnd::Reg(k)) = key {
            let tk = self.tag(k);
            let ik = self.b.ins().icmp_imm_s(IntCC::Equal, tk, TAG_INT);
            ok = self.b.ins().band(ok, ik);
        }
        self.b.ins().brif(ok, go, &[], slow, &[]);
        self.b.switch_to_block(go);
        if let Some(l) = self.inline {
            let lean = self.b.create_block();
            self.inline_set(l, obj, key, src, lean);
            self.fall(p);
            self.b.switch_to_block(lean);
        }
        let t = self.payload(obj, types::I64);
        let third = match key {
            Key::Int(k) => self.opnd_int(k),
            Key::Field(c) => self
                .b
                .ins()
                .iconst(types::I64, c as *const FieldCache as i64),
        };
        let regs = self.regs();
        let vt = self
            .b
            .ins()
            .uload8(types::I64, self.flags, regs, src as i32 * 16);
        let vp = self.payload(src, types::I64);
        let callee = self.b.ins().iconst(types::I64, fast as i64);
        let call = self
            .b
            .ins()
            .call_indirect(self.setsig, callee, &[self.vm, t, third, vt, vp]);
        let done = self.b.inst_results(call)[0];
        let next = self.fall_block(p);
        self.b.ins().brif(done, next, &[], slow, &[]);
        self.b.switch_to_block(slow);
        self.helper_then_next(full, p);
    }

    /// `//` (or with `modulo`, `%`) on integers inline: a zero divisor, and
    /// everything that isn't two integers, through the helper.
    fn int_div(&mut self, p: usize, dst: u8, a: u8, y: Opnd, modulo: bool) {
        let slow = self.b.create_block();
        if matches!(y, Opnd::Float(_) | Opnd::Other) {
            self.b.ins().jump(slow, &[]);
        } else {
            let ta = self.tag(a);
            let tb = self.opnd_tag(y);
            let ints = self.both(ta, tb, TAG_INT);
            let int_b = self.b.create_block();
            self.b.ins().brif(ints, int_b, &[], slow, &[]);
            self.b.switch_to_block(int_b);
            let i = self.payload(a, types::I64);
            let j = self.opnd_int(y);
            // j in {0, -1} (zero, and MIN / -1) goes to the helper.
            let j1 = self.b.ins().iadd_imm_s(j, 1);
            let small = self
                .b
                .ins()
                .icmp_imm_u(IntCC::UnsignedLessThanOrEqual, j1, 1);
            let div_b = self.b.create_block();
            self.b.ins().brif(small, slow, &[], div_b, &[]);
            self.b.switch_to_block(div_b);
            let q = self.b.ins().sdiv(i, j);
            let r = self.b.ins().srem(i, j);
            // Floor: adjust when the remainder is nonzero and its sign
            // differs from the divisor's.
            let x = self.b.ins().bxor(r, j);
            let neg = self.b.ins().icmp_imm_s(IntCC::SignedLessThan, x, 0);
            let nz = self.b.ins().icmp_imm_s(IntCC::NotEqual, r, 0);
            let adj = self.b.ins().band(neg, nz);
            let v = if modulo {
                let rj = self.b.ins().iadd(r, j);
                self.b.ins().select(adj, rj, r)
            } else {
                let q1 = self.b.ins().iadd_imm_s(q, -1);
                self.b.ins().select(adj, q1, q)
            };
            self.store(dst, TAG_INT, v);
            self.fall(p);
        }
        self.b.switch_to_block(slow);
        self.helper_then_next(self.helpers.arith, p);
    }

    fn op(&mut self, p: usize) {
        let op = self.code.code[p];
        let h = self.helpers;
        match op {
            Op::Move { dst, src } => {
                let regs = self.regs();
                let t = self
                    .b
                    .ins()
                    .load(types::I64, self.flags, regs, src as i32 * 16);
                let v = self
                    .b
                    .ins()
                    .load(types::I64, self.flags, regs, src as i32 * 16 + 8);
                self.b.ins().store(self.flags, t, regs, dst as i32 * 16);
                self.b.ins().store(self.flags, v, regs, dst as i32 * 16 + 8);
                self.fall(p);
            }
            Op::LoadNil { dst, count } => {
                for r in dst..dst + count {
                    self.store_tag(r, TAG_NIL);
                }
                self.fall(p);
            }
            Op::LoadBool { dst, value } => {
                let v = self.b.ins().iconst(types::I64, value as i64);
                self.store(dst, TAG_BOOL, v);
                self.fall(p);
            }
            Op::LoadInt { dst, value } => {
                let v = self.b.ins().iconst(types::I64, value as i64);
                self.store(dst, TAG_INT, v);
                self.fall(p);
            }
            Op::LoadConst { dst, k } => self.load_const(p, dst, k as usize),
            Op::LoadConstX { dst, k } => self.load_const(p, dst, k as usize),

            Op::Add { dst, a, b } => self.arith(p, Ar::Add, dst, Opnd::Reg(a), Opnd::Reg(b)),
            Op::Sub { dst, a, b } => self.arith(p, Ar::Sub, dst, Opnd::Reg(a), Opnd::Reg(b)),
            Op::Mul { dst, a, b } => self.arith(p, Ar::Mul, dst, Opnd::Reg(a), Opnd::Reg(b)),
            Op::Div { dst, a, b } => self.arith(p, Ar::Div, dst, Opnd::Reg(a), Opnd::Reg(b)),
            Op::IDiv { dst, a, b } => self.int_div(p, dst, a, Opnd::Reg(b), false),
            Op::Mod { dst, a, b } => self.int_div(p, dst, a, Opnd::Reg(b), true),
            Op::Pow { dst, a, b } => self.arith(p, Ar::Slow, dst, Opnd::Reg(a), Opnd::Reg(b)),
            Op::AddK { dst, a, k } => {
                let y = self.konst(k);
                self.arith(p, Ar::Add, dst, Opnd::Reg(a), y)
            }
            Op::SubK { dst, a, k } => {
                let y = self.konst(k);
                self.arith(p, Ar::Sub, dst, Opnd::Reg(a), y)
            }
            Op::MulK { dst, a, k } => {
                let y = self.konst(k);
                self.arith(p, Ar::Mul, dst, Opnd::Reg(a), y)
            }
            Op::DivK { dst, a, k } => {
                let y = self.konst(k);
                self.arith(p, Ar::Div, dst, Opnd::Reg(a), y)
            }
            Op::IDivK { dst, a, k } => {
                let y = self.konst(k);
                self.int_div(p, dst, a, y, false)
            }
            Op::ModK { dst, a, k } => {
                let y = self.konst(k);
                self.int_div(p, dst, a, y, true)
            }
            Op::PowK { dst, a, k } => self.pow_k(p, dst, a, k),
            Op::KAdd { dst, k, b } => {
                let x = self.konst(k);
                self.arith(p, Ar::Add, dst, x, Opnd::Reg(b))
            }
            Op::KSub { dst, k, b } => {
                let x = self.konst(k);
                self.arith(p, Ar::Sub, dst, x, Opnd::Reg(b))
            }
            Op::KMul { dst, k, b } => {
                let x = self.konst(k);
                self.arith(p, Ar::Mul, dst, x, Opnd::Reg(b))
            }
            Op::KDiv { dst, k, b } => {
                let x = self.konst(k);
                self.arith(p, Ar::Div, dst, x, Opnd::Reg(b))
            }
            Op::AddInt { dst, a, imm } => {
                self.arith_imm(p, dst, a, imm as i64);
            }
            Op::SubInt { dst, a, imm } => {
                // As the interpreter: a float adds the negated integer.
                self.arith_imm(p, dst, a, -(imm as i64));
            }
            Op::BAnd { dst, a, b } | Op::BOr { dst, a, b } | Op::BXor { dst, a, b } => {
                let ta = self.tag(a);
                let tb = self.tag(b);
                let ints = self.both(ta, tb, TAG_INT);
                let int_b = self.b.create_block();
                let slow = self.b.create_block();
                self.b.ins().brif(ints, int_b, &[], slow, &[]);
                self.b.switch_to_block(int_b);
                let i = self.payload(a, types::I64);
                let j = self.payload(b, types::I64);
                let v = match op {
                    Op::BAnd { .. } => self.b.ins().band(i, j),
                    Op::BOr { .. } => self.b.ins().bor(i, j),
                    _ => self.b.ins().bxor(i, j),
                };
                self.store(dst, TAG_INT, v);
                self.fall(p);
                self.b.switch_to_block(slow);
                self.helper_then_next(h.bitwise, p);
            }
            Op::Shl { .. } | Op::Shr { .. } | Op::BNot { .. } => {
                self.helper_then_next(h.bitwise, p)
            }
            Op::Neg { dst, src } => {
                let t = self.tag(src);
                let is_int = self.b.ins().icmp_imm_s(IntCC::Equal, t, TAG_INT);
                let int_b = self.b.create_block();
                let not_int = self.b.create_block();
                self.b.ins().brif(is_int, int_b, &[], not_int, &[]);
                self.b.switch_to_block(int_b);
                let i = self.payload(src, types::I64);
                let v = self.b.ins().ineg(i);
                self.store(dst, TAG_INT, v);
                self.fall(p);
                self.b.switch_to_block(not_int);
                let is_float = self.b.ins().icmp_imm_s(IntCC::Equal, t, TAG_FLOAT);
                let float_b = self.b.create_block();
                let slow = self.b.create_block();
                self.b.ins().brif(is_float, float_b, &[], slow, &[]);
                self.b.switch_to_block(float_b);
                let f = self.payload(src, types::F64);
                let v = self.b.ins().fneg(f);
                self.store(dst, TAG_FLOAT, v);
                self.fall(p);
                self.b.switch_to_block(slow);
                self.helper_then_next(h.arith, p);
            }
            Op::Not { dst, src } => {
                let f = self.falsy(src);
                let v = self.b.ins().uextend(types::I64, f);
                self.store(dst, TAG_BOOL, v);
                self.fall(p);
            }
            Op::Len { .. } => self.helper_then_next(h.len, p),
            Op::Concat { .. } => self.helper_then_next(h.concat, p),

            Op::Jump { .. } => {
                let t = self.target(p, op);
                self.b.ins().jump(t, &[]);
            }
            Op::JumpIfTrue { src, .. } | Op::JumpIfFalse { src, .. } => {
                let t = self.target(p, op);
                let fall = self.fall_block(p);
                let f = self.falsy(src);
                if matches!(op, Op::JumpIfFalse { .. }) {
                    self.b.ins().brif(f, t, &[], fall, &[]);
                } else {
                    self.b.ins().brif(f, fall, &[], t, &[]);
                }
            }
            Op::JumpLt { a, b, .. } => {
                self.cmp_jump(p, op, a, b, IntCC::SignedLessThan, FloatCC::LessThan, true)
            }
            Op::JumpNotLt { a, b, .. } => {
                self.cmp_jump(p, op, a, b, IntCC::SignedLessThan, FloatCC::LessThan, false)
            }
            Op::JumpLe { a, b, .. } => self.cmp_jump(
                p,
                op,
                a,
                b,
                IntCC::SignedLessThanOrEqual,
                FloatCC::LessThanOrEqual,
                true,
            ),
            Op::JumpNotLe { a, b, .. } => self.cmp_jump(
                p,
                op,
                a,
                b,
                IntCC::SignedLessThanOrEqual,
                FloatCC::LessThanOrEqual,
                false,
            ),
            Op::JumpEq { a, b, .. } => {
                self.cmp_jump(p, op, a, b, IntCC::Equal, FloatCC::Equal, true)
            }
            Op::JumpNe { a, b, .. } => {
                self.cmp_jump(p, op, a, b, IntCC::Equal, FloatCC::Equal, false)
            }
            Op::JumpEqInt { a, imm, want, .. } => {
                let taken = self.target(p, op);
                let fall = self.fall_block(p);
                let (yes, no) = if want { (taken, fall) } else { (fall, taken) };
                let ta = self.tag(a);
                let is_int = self.b.ins().icmp_imm_s(IntCC::Equal, ta, TAG_INT);
                let int_b = self.b.create_block();
                let not_int = self.b.create_block();
                self.b.ins().brif(is_int, int_b, &[], not_int, &[]);
                self.b.switch_to_block(int_b);
                let i = self.payload(a, types::I64);
                let c = self.b.ins().icmp_imm_s(IntCC::Equal, i, imm as i64);
                self.b.ins().brif(c, yes, &[], no, &[]);
                self.b.switch_to_block(not_int);
                let is_float = self.b.ins().icmp_imm_s(IntCC::Equal, ta, TAG_FLOAT);
                let float_b = self.b.create_block();
                self.b.ins().brif(is_float, float_b, &[], no, &[]);
                self.b.switch_to_block(float_b);
                let f = self.payload(a, types::F64);
                let k = self.b.ins().f64const(imm as f64);
                let c = self.b.ins().fcmp(FloatCC::Equal, f, k);
                self.b.ins().brif(c, yes, &[], no, &[]);
            }
            Op::JumpLtInt { a, imm, want, .. } => self.cmp_imm_jump(
                p,
                op,
                a,
                imm,
                IntCC::SignedLessThan,
                FloatCC::LessThan,
                want,
            ),
            Op::JumpLeInt { a, imm, want, .. } => self.cmp_imm_jump(
                p,
                op,
                a,
                imm,
                IntCC::SignedLessThanOrEqual,
                FloatCC::LessThanOrEqual,
                want,
            ),
            Op::JumpGtInt { a, imm, want, .. } => self.cmp_imm_jump(
                p,
                op,
                a,
                imm,
                IntCC::SignedGreaterThan,
                FloatCC::GreaterThan,
                want,
            ),
            Op::JumpGeInt { a, imm, want, .. } => self.cmp_imm_jump(
                p,
                op,
                a,
                imm,
                IntCC::SignedGreaterThanOrEqual,
                FloatCC::GreaterThanOrEqual,
                want,
            ),

            Op::GetField { dst, obj, ic, .. } => {
                let code = self.code;
                let c = Key::Field(&code.caches[ic as usize]);
                self.fast_get(p, dst, obj, c, h.field_get as usize, h.get_field)
            }
            Op::SetField { obj, src, ic, .. } => {
                let code = self.code;
                let c = Key::Field(&code.caches[ic as usize]);
                self.fast_set(p, obj, c, src, h.field_set as usize, h.set_field)
            }
            Op::GetIndex { dst, obj, key } => {
                let k = Key::Int(Opnd::Reg(key));
                self.fast_get(p, dst, obj, k, h.array_get as usize, h.get_index)
            }
            Op::GetIndexInt { dst, obj, key } => {
                let k = Key::Int(Opnd::Int(key as i64));
                self.fast_get(p, dst, obj, k, h.array_get as usize, h.get_index)
            }
            Op::SetIndex { obj, key, src } => {
                let k = Key::Int(Opnd::Reg(key));
                self.fast_set(p, obj, k, src, h.array_set as usize, h.set_index)
            }
            Op::SetIndexInt { obj, key, src } => {
                let k = Key::Int(Opnd::Int(key as i64));
                self.fast_set(p, obj, k, src, h.array_set as usize, h.set_index)
            }
            Op::GetGlobal { dst, ic, .. } => {
                let cache = &self.code.caches[ic as usize] as *const _ as i64;
                let slow = self.b.create_block();
                let callee = self
                    .b
                    .ins()
                    .iconst(types::I64, h.global_get as usize as i64);
                let c = self.b.ins().iconst(types::I64, cache);
                let call =
                    self.b
                        .ins()
                        .call_indirect(self.getsig, callee, &[self.vm, self.closure, c]);
                let (t, v) = (self.b.inst_results(call)[0], self.b.inst_results(call)[1]);
                self.store_parts_or(p, dst, t, v, slow);
                self.b.switch_to_block(slow);
                self.helper_then_next(h.get_global, p);
            }
            Op::CapSelf { dst } => {
                let regs = self.regs();
                let t = self.b.ins().iconst(types::I64, TAG_CLOSURE);
                self.b.ins().store(self.flags, t, regs, dst as i32 * 16);
                self.b
                    .ins()
                    .store(self.flags, self.closure, regs, dst as i32 * 16 + 8);
                self.fall(p);
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

            Op::Call {
                func,
                nargs,
                nresults,
            } if nargs != MULTI && nresults != MULTI && self.calls.is_some() => {
                let l = self.calls.expect("calls");
                self.inline_call(p, l, func, nargs, nresults)
            }
            Op::Call { nresults, .. } if nresults != MULTI || p + 1 < self.code.code.len() => {
                self.helper_then_next(h.call, p)
            }
            Op::GenericCall { .. } => self.helper_then_next(h.call, p),
            Op::Return0 | Op::Return1 { .. } if self.calls.is_some() => {
                let l = self.calls.expect("calls");
                let src = match op {
                    Op::Return1 { src } => Some(src),
                    _ => None,
                };
                let slow = self.b.create_block();
                self.inline_ret(l, src, slow);
                self.b.switch_to_block(slow);
                let r = self.helper(h.ret, p);
                self.b.ins().return_(&[r]);
            }
            Op::Return { .. } | Op::Return0 | Op::Return1 { .. } => {
                let r = self.helper(h.ret, p);
                self.b.ins().return_(&[r]);
            }

            Op::TailCall { .. } => self.helper_then_next(h.tail_call, p),
            Op::SetList { .. } => self.helper_then_next(h.set_list, p),
            Op::ForPrep { .. } => {
                let t = self.target(p, op);
                self.helper_branch(h.for_prep, p, t);
            }
            Op::ForLoop { base, .. } => self.for_loop(p, op, base),
            Op::GenericLoop { base, .. } => {
                let t = self.target(p, op);
                let fall = self.fall_block(p);
                let tag = self.tag(base + 4);
                let go = self.b.create_block();
                self.b.ins().brif(tag, go, &[], fall, &[]);
                self.b.switch_to_block(go);
                let regs = self.regs();
                let s = (base as i32 + 4) * 16;
                let d = (base as i32 + 2) * 16;
                let tv = self.b.ins().load(types::I64, self.flags, regs, s);
                let pv = self.b.ins().load(types::I64, self.flags, regs, s + 8);
                self.b.ins().store(self.flags, tv, regs, d);
                self.b.ins().store(self.flags, pv, regs, d + 8);
                self.b.ins().jump(t, &[]);
            }

            // The rest runs in the interpreter.
            _ => self.exit(p),
        }
    }

    fn load_const(&mut self, p: usize, dst: u8, k: usize) {
        let (tag, bits) = const_parts(self.code.consts[k]);
        let v = self.b.ins().iconst(types::I64, bits as i64);
        self.store(dst, tag as i64, v);
        self.fall(p);
    }

    /// `R[dst] = R[a] + imm` (`SubInt` passes the negated immediate).
    fn arith_imm(&mut self, p: usize, dst: u8, a: u8, imm: i64) {
        let t = self.tag(a);
        let is_int = self.b.ins().icmp_imm_s(IntCC::Equal, t, TAG_INT);
        let int_b = self.b.create_block();
        let not_int = self.b.create_block();
        self.b.ins().brif(is_int, int_b, &[], not_int, &[]);
        self.b.switch_to_block(int_b);
        let i = self.payload(a, types::I64);
        let v = self.b.ins().iadd_imm_s(i, imm);
        self.store(dst, TAG_INT, v);
        self.fall(p);
        self.b.switch_to_block(not_int);
        let is_float = self.b.ins().icmp_imm_s(IntCC::Equal, t, TAG_FLOAT);
        let float_b = self.b.create_block();
        let slow = self.b.create_block();
        self.b.ins().brif(is_float, float_b, &[], slow, &[]);
        self.b.switch_to_block(float_b);
        let f = self.payload(a, types::F64);
        let k = self.b.ins().f64const(imm as f64);
        let v = self.b.ins().fadd(f, k);
        self.store(dst, TAG_FLOAT, v);
        self.fall(p);
        self.b.switch_to_block(slow);
        self.helper_then_next(self.helpers.arith, p);
    }

    /// `R[dst] = R[a] ^ K[k]`: floats inline through `pow`.
    fn pow_k(&mut self, p: usize, dst: u8, a: u8, k: u16) {
        let Opnd::Float(g) = self.konst(k) else {
            return self.arith(p, Ar::Slow, dst, Opnd::Reg(a), Opnd::Other);
        };
        let t = self.tag(a);
        let is_float = self.b.ins().icmp_imm_s(IntCC::Equal, t, TAG_FLOAT);
        let float_b = self.b.create_block();
        let slow = self.b.create_block();
        self.b.ins().brif(is_float, float_b, &[], slow, &[]);
        self.b.switch_to_block(float_b);
        let f = self.payload(a, types::F64);
        let g = self.b.ins().f64const(g);
        let callee = self
            .b
            .ins()
            .iconst(types::I64, self.helpers.pow as usize as i64);
        let call = self.b.ins().call_indirect(self.powsig, callee, &[f, g]);
        let v = self.b.inst_results(call)[0];
        self.store(dst, TAG_FLOAT, v);
        self.fall(p);
        self.b.switch_to_block(slow);
        self.helper_then_next(self.helpers.arith, p);
    }

    /// `ForLoop` on the loop state `ForPrep` left: integers count down,
    /// floats compare with the limit.
    fn for_loop(&mut self, p: usize, op: Op, base: u8) {
        let back = self.target(p, op);
        let fall = self.fall_block(p);
        let t = self.tag(base);
        let is_int = self.b.ins().icmp_imm_s(IntCC::Equal, t, TAG_INT);
        let int_b = self.b.create_block();
        let float_b = self.b.create_block();
        self.b.ins().brif(is_int, int_b, &[], float_b, &[]);

        self.b.switch_to_block(int_b);
        let count = self.payload(base + 1, types::I64);
        let more = self.b.ins().icmp_imm_s(IntCC::SignedGreaterThan, count, 0);
        let step_b = self.b.create_block();
        self.b.ins().brif(more, step_b, &[], fall, &[]);
        self.b.switch_to_block(step_b);
        let regs = self.regs();
        let c1 = self.b.ins().iadd_imm_s(count, -1);
        self.b
            .ins()
            .store(self.flags, c1, regs, (base as i32 + 1) * 16 + 8);
        let cur = self.payload(base, types::I64);
        let s = self.payload(base + 2, types::I64);
        let next = self.b.ins().iadd(cur, s);
        let regs = self.regs();
        self.b
            .ins()
            .store(self.flags, next, regs, base as i32 * 16 + 8);
        self.store(base + 3, TAG_INT, next);
        self.b.ins().jump(back, &[]);

        self.b.switch_to_block(float_b);
        let cur = self.payload(base, types::F64);
        let lim = self.payload(base + 1, types::F64);
        let s = self.payload(base + 2, types::F64);
        let next = self.b.ins().fadd(cur, s);
        let zero = self.b.ins().f64const(0.0);
        let up = self.b.ins().fcmp(FloatCC::GreaterThan, s, zero);
        let le_up = self.b.ins().fcmp(FloatCC::LessThanOrEqual, next, lim);
        let le_down = self.b.ins().fcmp(FloatCC::LessThanOrEqual, lim, next);
        let ok = self.b.ins().select(up, le_up, le_down);
        let go = self.b.create_block();
        self.b.ins().brif(ok, go, &[], fall, &[]);
        self.b.switch_to_block(go);
        let regs = self.regs();
        self.b
            .ins()
            .store(self.flags, next, regs, base as i32 * 16 + 8);
        self.store(base + 3, TAG_FLOAT, next);
        self.b.ins().jump(back, &[]);
    }
}
