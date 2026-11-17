//! The plain interpreter: one function per instruction (ADR-16).
//!
//! A handler is `fn(&mut Vm, Regs, Ip, Op) -> Step`: the VM, the running
//! frame's registers, the instruction after its own, and its own
//! instruction. It ends by calling [`next`], which reads the instruction
//! at `Ip` and calls that instruction's handler ([`handler`]) in tail
//! position. The compiler turns such a call into a jump, so a run of
//! instructions is a chain of jumps from handler to handler, with the
//! registers and the code position in machine registers throughout.
//!
//! Most instructions have a common case that needs only the registers and
//! the heap, and a general case that needs the whole VM: it may call a
//! metamethod, raise an error, grow the stack or let the collector run.
//! The handler does the common case itself. For the general case it calls
//! a second function, again in tail position, which reads the instruction
//! again from the code, gives up the registers ([`leave`]), works on the
//! VM, and returns to the trampoline, which starts a new chain from the
//! frame on top ([`resume`]). The two sit next to each other in the file
//! for their kind of instruction:
//!
//! | File | Instructions |
//! |---|---|
//! | `loads.rs` | moves, constants, cells and captures |
//! | `tables.rs` | globals, fields, indexing, method lookup, table construction |
//! | `arith.rs` | arithmetic, bitwise operators, `not`, `#`, `..` |
//! | `branches.rs` | jumps and compare-and-jumps |
//! | `calls.rs` | calls, returns, varargs |
//! | `loops.rs` | numeric and generic `for` |
//! | `other.rs` | closures, to-be-closed variables, and the rare instructions |
//! | `regs.rs` | the register and code pointers, and the only `unsafe` besides `calls.rs` |
//!
//! Four things keep a handler's common case free of saved registers and
//! stack adjustment. `tools/check-handlers.py` lists the handlers of a
//! build that break them, from its disassembly:
//! - every handler has this one signature and returns what the next
//!   handler returns;
//! - anything that needs a real call (a write barrier, a lookup that
//!   misses, growing a vector, a panic with a message) lives in the
//!   general case or another `#[inline(never)]` function reached by a
//!   call in tail position, because one real call on any path gives every
//!   path of the function a prologue;
//! - no value built on the handler's own stack is passed on by address;
//! - the handler does not pass its instruction on to the general case,
//!   which reads it from the code again ([`Ip::handled`]): otherwise the
//!   instruction stays in a machine register across the common case.
//!
//! Nothing depends on the jumps for correctness. A chain spends fuel on
//! every backward jump and call (on every instruction in a debug build)
//! and returns to the trampoline ([`Vm::run_handlers`]) when it runs out.

mod arith;
mod branches;
mod calls;
mod loads;
mod loops;
mod other;
mod regs;
mod tables;

use std::rc::Rc;

use crate::object::{Closure, Gc, Value};
use crate::vm::interp::{Flow, Ran, WINDOW, Window};
use crate::vm::ops::Exc;
use crate::vm::{FrameKind, LuaError, Vm};
use orbit_bytecode::{Offset, Op, Reg};

use regs::{Ip, Regs};

/// What a chain of handlers tells the trampoline when it returns.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub(crate) enum Step {
    /// The frame's pc is saved: go on with it.
    Continue,
    /// The frame stack changed, or a collection step is due: start again
    /// from the frame on top.
    Reload,
    /// The run is finished.
    Done,
    /// A hook was armed: the hooked loop takes over.
    Switch,
    /// The heap is over its limit after a collection.
    NoMemory,
    /// An error (or metamethod request) is in `rt.pending`.
    Throw,
    /// A handler was handed an instruction that is not its own, or
    /// something the bytecode verifier rules out. The trampoline panics.
    Invalid,
}

type Handler = fn(&mut Vm, Regs, Ip, Op) -> Step;

/// The general case of an instruction. It is not passed the instruction:
/// it reads it again from the code ([`Ip::handled`]), so that the handler
/// need not keep it in a machine register for the uncommon path.
type General = fn(&mut Vm, Regs, Ip) -> Step;

/// Backward jumps and calls a chain makes before it returns to the
/// trampoline. When every handler's last call is a jump this hardly
/// matters; when it is not (a debug build), it bounds the native stack a
/// chain uses.
const FUEL: u32 = 256;

/// The handler of each instruction. Written as a `match` so that a new
/// instruction does not compile until it has one; the compiler makes a
/// table of it, indexed by the instruction's tag byte.
#[inline(always)]
fn handler(op: &Op) -> Handler {
    match op {
        Op::Move { .. } => loads::move_,
        Op::LoadNil { .. } => loads::load_nil,
        Op::LoadBool { .. } => loads::load_bool,
        Op::LoadInt { .. } => loads::load_int,
        Op::LoadConst { .. } => loads::load_const,
        Op::CellNew { .. } => loads::cell_new,
        Op::CellGet { .. } => loads::cell_get,
        Op::CellSet { .. } => loads::cell_set,
        Op::CapGet { .. } => loads::cap_get,
        Op::CapSet { .. } => loads::cap_set,
        Op::CapSelf { .. } => loads::cap_self,

        Op::GetGlobal { .. } => tables::get_global,
        Op::NewTable { .. } => tables::new_table,
        Op::GetField { .. } => tables::get_field,
        Op::SetField { .. } => tables::set_field,
        Op::GetIndex { .. } => tables::get_index,
        Op::SetIndex { .. } => tables::set_index,
        Op::GetIndexInt { .. } => tables::get_index_int,
        Op::SetIndexInt { .. } => tables::set_index_int,
        Op::SetList { .. } => tables::set_list,
        Op::SelfCall { .. } => tables::self_call,

        Op::Add { .. } => arith::add,
        Op::Sub { .. } => arith::sub,
        Op::Mul { .. } => arith::mul,
        Op::Div { .. } => arith::div,
        Op::Mod { .. } => arith::mod_,
        Op::AddInt { .. } => arith::add_int,
        Op::SubInt { .. } => arith::sub_int,
        Op::AddK { .. } => arith::add_k,
        Op::SubK { .. } => arith::sub_k,
        Op::MulK { .. } => arith::mul_k,
        Op::DivK { .. } => arith::div_k,
        Op::ModK { .. } => arith::mod_k,
        Op::KAdd { .. } => arith::k_add,
        Op::KSub { .. } => arith::k_sub,
        Op::KMul { .. } => arith::k_mul,
        Op::KDiv { .. } => arith::k_div,
        Op::BAnd { .. } => arith::b_and,
        Op::BOr { .. } => arith::b_or,
        Op::BXor { .. } => arith::b_xor,
        Op::Shl { .. } => arith::shl,
        Op::Shr { .. } => arith::shr,
        Op::Not { .. } => arith::not,
        Op::Len { .. } => arith::len,
        Op::Concat { .. } => arith::concat,

        Op::Jump { .. } => branches::jump,
        Op::JumpIfTrue { .. } => branches::jump_if_true,
        Op::JumpIfFalse { .. } => branches::jump_if_false,
        Op::JumpLt { .. } => branches::jump_lt,
        Op::JumpNotLt { .. } => branches::jump_not_lt,
        Op::JumpLe { .. } => branches::jump_le,
        Op::JumpNotLe { .. } => branches::jump_not_le,
        Op::JumpEq { .. } => branches::jump_eq,
        Op::JumpNe { .. } => branches::jump_ne,
        Op::JumpEqInt { .. } => branches::jump_eq_int,
        Op::JumpLtInt { .. } => branches::jump_lt_int,
        Op::JumpLeInt { .. } => branches::jump_le_int,
        Op::JumpGtInt { .. } => branches::jump_gt_int,
        Op::JumpGeInt { .. } => branches::jump_ge_int,

        Op::Call { .. } => calls::call,
        Op::TailCall { .. } => calls::tail_call,
        Op::Return { .. } => calls::return_,
        Op::Return0 => calls::return0,
        Op::Return1 { .. } => calls::return1,
        Op::VarargLen { .. } => calls::vararg_len,
        Op::VarargGetInt { .. } => calls::vararg_get_int,

        Op::ForPrep { .. } => loops::for_prep,
        Op::ForLoop { .. } => loops::for_loop,
        Op::GenericCall { .. } => loops::generic_call,
        Op::GenericLoop { .. } => loops::generic_loop,

        Op::Closure { .. } => other::closure,
        Op::Tbc { .. } => other::tbc,
        Op::Close { .. } => other::close,

        // Seldom run: these go through the function the hooked loop keeps
        // them in.
        Op::LoadConstX { .. }
        | Op::CellClear { .. }
        | Op::SetGlobal { .. }
        | Op::CheckUndefined { .. }
        | Op::IDiv { .. }
        | Op::Pow { .. }
        | Op::IDivK { .. }
        | Op::PowK { .. }
        | Op::Neg { .. }
        | Op::BNot { .. }
        | Op::Cast { .. }
        | Op::Vararg { .. }
        | Op::VarargTable { .. }
        | Op::VarargGet { .. }
        | Op::VarargUnpack { .. }
        | Op::ForPrepExclusive { .. }
        | Op::NativeEntry { .. } => other::rare,
    }
}

// Going on to the next instruction.

/// Run the instruction at `ip`.
#[inline(always)]
fn next(vm: &mut Vm, regs: Regs, ip: Ip) -> Step {
    if cfg!(debug_assertions) && !spend_fuel(vm) {
        return out_of_fuel(vm, regs, ip);
    }
    let op = ip.op();
    handler(&op)(vm, regs, ip.after(), op)
}

/// [`next`] where a chain could otherwise go round for ever: after a
/// call, and after a backward jump ([`next_backward`]).
#[inline(always)]
fn next_spending_fuel(vm: &mut Vm, regs: Regs, ip: Ip) -> Step {
    if !cfg!(debug_assertions) && !spend_fuel(vm) {
        return out_of_fuel(vm, regs, ip);
    }
    next(vm, regs, ip)
}

/// [`next`] at the target of a backward jump. This is where a requested
/// interrupt is noticed, so that it is raised in the function that loops.
#[inline(always)]
fn next_backward(vm: &mut Vm, regs: Regs, ip: Ip) -> Step {
    #[cfg(feature = "hooks")]
    if vm.rt.host.interrupt_requested() {
        return interrupted(vm, regs, ip);
    }
    next_spending_fuel(vm, regs, ip)
}

/// Go on `off` instructions from `ip`.
#[inline(always)]
fn jump_by(vm: &mut Vm, regs: Regs, ip: Ip, off: Offset) -> Step {
    if off < 0 {
        next_backward(vm, regs, ip.jump(off))
    } else {
        next(vm, regs, ip.jump(off))
    }
}

/// The end of a conditional jump: by `off` when `taken`, else straight on.
#[inline(always)]
fn branch(vm: &mut Vm, regs: Regs, ip: Ip, off: Offset, taken: bool) -> Step {
    if taken {
        jump_by(vm, regs, ip, off)
    } else {
        next(vm, regs, ip)
    }
}

/// [`next`] after an instruction that allocated: a collection step that
/// has come due runs first, from the trampoline.
#[inline(always)]
fn next_after_allocating(vm: &mut Vm, regs: Regs, ip: Ip) -> Step {
    if vm.heap.should_step() {
        return collect(vm, regs, ip);
    }
    next(vm, regs, ip)
}

#[inline(always)]
fn spend_fuel(vm: &mut Vm) -> bool {
    vm.rt.fuel = vm.rt.fuel.wrapping_sub(1);
    vm.rt.fuel != 0
}

/// The step a function that ends a chain returns. When the compiler can
/// see that such a function always returns the same step, it stops
/// jumping to it: the handler calls it and returns the constant itself,
/// which costs every path of the handler a prologue. `black_box` keeps
/// the value out of the compiler's sight.
#[inline(always)]
fn back_to_trampoline(step: Step) -> Step {
    std::hint::black_box(step)
}

/// Back to the trampoline, which starts a new chain at `ip`.
#[cold]
#[inline(never)]
fn out_of_fuel(vm: &mut Vm, regs: Regs, ip: Ip) -> Step {
    let at = leave(vm, regs, ip);
    save_pc(vm, at.pc);
    back_to_trampoline(Step::Continue)
}

/// Back to the trampoline, which lets the collector run before it starts
/// a new chain at `ip`.
#[cold]
#[inline(never)]
fn collect(vm: &mut Vm, regs: Regs, ip: Ip) -> Step {
    let at = leave(vm, regs, ip);
    save_pc(vm, at.pc);
    back_to_trampoline(Step::Reload)
}

/// Leave to the hooked loop, which raises the interrupt before the
/// instruction at `ip`.
#[cfg(feature = "hooks")]
#[cold]
#[inline(never)]
fn interrupted(vm: &mut Vm, regs: Regs, ip: Ip) -> Step {
    let at = leave(vm, regs, ip);
    save_pc(vm, at.pc);
    back_to_trampoline(arm_interrupt(vm))
}

/// An interrupt was requested ([`crate::vm::Interrupt`]) and the frame's
/// pc is saved: the hooked loop raises it there.
#[cfg(feature = "hooks")]
#[cold]
fn arm_interrupt(vm: &mut Vm) -> Step {
    vm.rt.hooks.touch();
    vm.rt.stack.hook.mask |= crate::vm::hook::INTERRUPT;
    Step::Switch
}

// The general case of an instruction: on the whole VM.

/// Where a handler stands once it has given up its registers: the running
/// frame's base, and the index of the instruction after its own.
struct Place {
    base: usize,
    pc: usize,
}

/// Give up the registers and the code position to work on the whole VM.
/// The registers are taken by value and gone: whatever the handler calls
/// from here on may move the stack. It ends with one of [`Place`]'s
/// methods or with [`resume`].
#[inline(always)]
fn leave(vm: &mut Vm, _regs: Regs, ip: Ip) -> Place {
    let pc = ip.pc(&vm.rt);
    sync_code(vm);
    Place {
        base: vm.rt.stack.running().base as usize,
        pc,
    }
}

impl Place {
    /// The instruction is done: go on with the one after it.
    #[inline(always)]
    fn done(self, vm: &mut Vm) -> Step {
        save_pc(vm, self.pc);
        resume()
    }

    /// [`Place::done`] after allocating: a collection step that has come
    /// due runs first.
    #[inline(always)]
    fn done_after_allocating(self, vm: &mut Vm) -> Step {
        if vm.heap.should_step() {
            save_pc(vm, self.pc);
            let live = (self.base + WINDOW).max(vm.rt.top);
            if vm.gc_safepoint(live) {
                return Step::NoMemory;
            }
        }
        self.done(vm)
    }

    /// The instruction is done: go on `off` instructions from the one
    /// after it. A backward jump notices a requested interrupt, as
    /// [`next_backward`] does.
    #[inline(always)]
    fn jump(self, vm: &mut Vm, off: Offset) -> Step {
        save_pc(vm, (self.pc as isize + off as isize) as usize);
        #[cfg(feature = "hooks")]
        if off < 0 && vm.rt.host.interrupt_requested() {
            return arm_interrupt(vm);
        }
        resume()
    }

    /// [`Place::jump`] by `off` when `taken`, else [`Place::done`].
    #[inline(always)]
    fn branch(self, vm: &mut Vm, off: Offset, taken: bool) -> Step {
        if taken {
            self.jump(vm, off)
        } else {
            self.done(vm)
        }
    }

    /// The instruction raised `x`: hand it to the trampoline.
    #[cold]
    #[inline(never)]
    fn raise(self, vm: &mut Vm, x: Exc) -> Step {
        vm.rt.pending = Some((x, self.pc));
        Step::Throw
    }

    /// An indexing instruction with destination `dst` raised `x`. A
    /// request for `__index` is followed through the metatable chain here
    /// and the instruction finished; anything else is raised.
    #[inline(never)]
    fn raise_indexing(self, vm: &mut Vm, dst: Reg, x: Exc) -> Step {
        let Exc::Index { obj, key } = x else {
            return self.raise(vm, x);
        };
        match crate::vm::meta::index(&vm.heap, &vm.rt.metas, obj, key) {
            Ok(v) => {
                let mut w = Window::take(&mut vm.rt.stack.regs, self.base);
                w[dst] = v;
                self.done(vm)
            }
            Err(e) => self.raise(vm, e),
        }
    }

    /// Go on after a jump of the run to another frame or out of it.
    #[inline(always)]
    fn flow(self, flow: Flow) -> Step {
        match flow {
            Flow::Reload | Flow::Switched => Step::Reload,
            Flow::Done => Step::Done,
        }
    }
}

#[inline(always)]
fn save_pc(vm: &mut Vm, pc: usize) {
    vm.rt.stack.frames.last_mut().expect("the running frame").pc = pc as u32;
}

/// Go on from the frame on top, at its saved pc: back to the trampoline,
/// which starts a new chain there. A general case ends this way and does
/// not start the chain itself, because it is rarely compiled to end in a
/// jump (a result it was handed through its own stack frame is enough to
/// prevent that), and a real call here would nest one native frame per
/// instruction.
#[inline(always)]
fn resume() -> Step {
    back_to_trampoline(Step::Continue)
}

/// Run the frame on top from its saved pc. `rt.cur` is its code.
#[inline(always)]
fn start_chain(vm: &mut Vm) -> Step {
    let f = vm.rt.stack.running();
    let (base, pc) = (f.base as usize, f.pc as usize);
    let ip = Ip::at(&vm.rt.cur, pc);
    vm.rt.code = Rc::as_ptr(&vm.rt.cur);
    vm.rt.first_op = vm.rt.cur.code.as_ptr();
    let (regs, len) = Regs::of_frame(&mut vm.rt.stack.regs, base);
    vm.rt.stack_len = len;
    next(vm, regs, ip)
}

/// Bring the VM's own, counted reference to the running code (`rt.cur`)
/// up to date with the pointer the chain keeps (`rt.code`), which calls
/// and returns between handlers change alone.
#[inline(always)]
fn sync_code(vm: &mut Vm) {
    if vm.rt.code != Rc::as_ptr(&vm.rt.cur) {
        sync_code_changed(vm);
    }
}

#[inline(never)]
fn sync_code_changed(vm: &mut Vm) {
    let (closure, code_id) = (vm.rt.closure, vm.rt.code as usize);
    let _ = switch_code(vm, closure, code_id);
}

/// Make `closure`, running the code `code_id`, the running function.
/// Returns whether the debugger wants the hooked loop for it.
#[inline(always)]
fn switch_code(vm: &mut Vm, closure: Gc<Closure>, code_id: usize) -> bool {
    vm.rt.closure = closure;
    if code_id != Rc::as_ptr(&vm.rt.cur) as usize {
        match &mut vm.rt.prev_code {
            Some(p) if Rc::as_ptr(p) as usize == code_id => std::mem::swap(&mut vm.rt.cur, p),
            _ => {
                let new = vm.heap.get(closure).code.clone();
                vm.rt.prev_code = Some(std::mem::replace(&mut vm.rt.cur, new));
            }
        }
        #[cfg(feature = "debugger")]
        if vm.rt.debug.watch && vm.debug_switches(&vm.rt.cur, closure, vm.rt.stack.depth(), false) {
            return true;
        }
    }
    false
}

/// Whether moving to another function has to go through the general case,
/// where the debugger is asked.
#[inline(always)]
fn debugger_watches(_vm: &Vm) -> bool {
    #[cfg(feature = "debugger")]
    return _vm.rt.debug.watch;
    #[cfg(not(feature = "debugger"))]
    false
}

/// `store(v)` for the value of `src`, with the variant known in each
/// case: a number or a boolean then needs no write barrier, and is stored
/// as a tag and a payload rather than as a 16-byte copy.
#[inline(always)]
fn with_known_variant<R>(src: &Value, mut store: impl FnMut(Value) -> R) -> R {
    match *src {
        Value::Int(x) => store(Value::Int(x)),
        Value::Float(f) => store(Value::Float(f)),
        Value::Bool(b) => store(Value::Bool(b)),
        v => store(v),
    }
}

impl Vm {
    /// The plain interpreter: run the frame on top until the run is
    /// finished or a hook wants the hooked loop.
    pub(crate) fn run_handlers(&mut self) -> Result<Ran, LuaError> {
        'reload: loop {
            if self.heap.should_step() {
                let base = self.rt.stack.running().base as usize;
                if self.gc_safepoint((base + WINDOW).max(self.rt.top)) {
                    return Ok(Ran::NoMemory);
                }
            }
            #[cfg(not(feature = "debugger"))]
            if self.rt.stack.hook.armed() {
                return Ok(Ran::Switch);
            }
            let frame = *self.rt.stack.running();
            let FrameKind::Lua { closure, code_id } = frame.kind else {
                unreachable!("a run starts on a Lua frame")
            };
            self.rt.closure = closure;
            if code_id != Rc::as_ptr(&self.rt.cur) as usize {
                self.rt.cur = self.heap.get(closure).code.clone();
            }
            #[cfg(feature = "debugger")]
            if self.hooked_here(&self.rt.cur, closure, self.rt.stack.depth()) {
                return Ok(Ran::Switch);
            }
            loop {
                self.rt.fuel = FUEL;
                match start_chain(self) {
                    Step::Continue => {}
                    Step::Reload => continue 'reload,
                    Step::Done => return Ok(Ran::Done),
                    Step::Switch => return Ok(Ran::Switch),
                    Step::NoMemory => return Ok(Ran::NoMemory),
                    Step::Invalid => {
                        panic!("a handler was handed an instruction it cannot run")
                    }
                    Step::Throw => {
                        let (exc, at) = self.rt.pending.take().expect("a pending error");
                        match self.raise_at(exc, at) {
                            Ok(Flow::Reload | Flow::Switched) => continue 'reload,
                            Ok(Flow::Done) => return Ok(Ran::Done),
                            Err(e) => return Err(e),
                        }
                    }
                }
            }
        }
    }
}
