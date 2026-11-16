//! SPIKE: the interpreter as one small function per instruction.
//!
//! Each instruction has a handler `fn(&mut Vm, pc, insn, bf) -> Step`. A
//! handler does its work and ends by calling the next instruction's handler
//! in tail position, which the compiler turns into a jump when it can (a
//! sibling call). Nothing depends on that for correctness: `bf` carries a
//! fuel count that every dispatch decrements, and a handler that runs out
//! returns to [`Vm::run_threaded`], so the native stack stays bounded even
//! when every dispatch is a real call (a debug build).
//!
//! The state a handler gets in registers is the VM, the pc of the next
//! instruction, its own instruction word, and `bf`: the frame's base in the
//! high half, the fuel in the low half. Everything else (the running code,
//! the closure, the register window) is read from the VM when a handler
//! needs it.

use std::rc::Rc;

use crate::object::table::{self, IntStore};
use crate::object::{Closure, Gc, Tag, Value};
use orbit_bytecode::insn::{Insn, Opcode};
use orbit_bytecode::number::BitOp;
use orbit_bytecode::{MULTI, Op};

use crate::vm::cold::{env_error, get_str_miss, op_len, set_str_miss};
use crate::vm::dispatch::{Cold, Frames, LuaFrame, NativeStep, method_lookup};
use crate::vm::fast::{
    self, Cmp,
    Src::{K, R},
};
use crate::vm::interp::{
    Flow, Ran, Shape, WINDOW, Window, const_str, enter_lua_frame_fast, enter_lua_frame_general,
};
use crate::vm::native::FastCtx;
use crate::vm::ops::{self, Arith, Exc};
use crate::vm::{FrameKind, LuaError, Vm};

/// What a handler chain tells [`Vm::run_threaded`] when it returns.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub(crate) enum Step {
    /// Out of fuel: the frame's pc is saved, go on with it.
    Continue,
    /// The frame stack changed: reload the frame on top.
    Reload,
    /// The run is finished.
    Done,
    /// A hook was armed: the other loop takes over.
    Switch,
    /// The heap is over its limit after a collection.
    NoMemory,
    /// An error (or metamethod request) is in `rt.pending`.
    Throw,
    /// The second cut left the instruction at the frame's pc alone.
    Slow,
    /// The second cut returned to the caller's frame.
    Returned,
}

type Handler = fn(&mut Vm, usize, Insn, u64) -> Step;

/// Dispatches between two returns to the trampoline. With guaranteed
/// sibling calls this could be anything; without them it bounds the native
/// stack a chain uses.
const FUEL: u64 = match option_env!("ORBIT_FUEL") {
    Some(s) => parse_u64(s),
    None => 1 << 14,
};

const fn parse_u64(s: &str) -> u64 {
    let b = s.as_bytes();
    let mut i = 0;
    let mut n = 0;
    while i < b.len() {
        n = n * 10 + (b[i] - b'0') as u64;
        i += 1;
    }
    n
}

#[inline(always)]
fn base_of(bf: u64) -> usize {
    (bf >> 32) as usize
}

#[inline(always)]
fn with_base(base: usize, bf: u64) -> u64 {
    (base as u64) << 32 | (bf & 0xffff_ffff)
}

/// Run the instruction at `pc`.
#[inline(always)]
fn next(vm: &mut Vm, pc: usize, bf: u64) -> Step {
    if bf as u32 == 0 {
        return out_of_fuel(vm, pc);
    }
    // Both ways out are calls in tail position, so a handler keeps
    // nothing across a call on its own path.
    match vm.rt.cur.words.get(pc) {
        Some(&i) => HANDLERS[i.code()](vm, pc + 1, i, bf - 1),
        None => bad_pc(),
    }
}

#[cold]
#[inline(never)]
fn bad_pc() -> Step {
    unreachable!("the pc left the function's code")
}

#[inline(always)]
fn save_pc(vm: &mut Vm, pc: usize) {
    vm.rt
        .stack
        .frames
        .last_mut()
        .expect("the running frame")
        .pc = pc as u32;
}

#[cold]
#[inline(never)]
fn out_of_fuel(vm: &mut Vm, pc: usize) -> Step {
    save_pc(vm, pc);
    // The interrupt flag is polled here rather than on backward jumps.
    #[cfg(feature = "hooks")]
    if vm.rt.host.interrupt_requested() {
        vm.rt.hooks.touch();
        vm.rt.stack.hook.mask |= crate::vm::hook::INTERRUPT;
        return Step::Switch;
    }
    Step::Continue
}

/// Hand `x`, raised by the instruction before `pc`, to the trampoline.
#[cold]
#[inline(never)]
fn throw(vm: &mut Vm, pc: usize, x: Exc) -> Step {
    vm.rt.pending = Some((x, pc));
    Step::Throw
}

/// An instruction's slow exit: an `__index` request is followed through
/// the metatable chain here and the instruction finished; anything else
/// is thrown.
#[inline(never)]
fn raise_step(vm: &mut Vm, pc: usize, i: Insn, bf: u64, x: Exc) -> Step {
    if let Exc::Index { obj, key } = x {
        return match crate::vm::meta::index(&vm.heap, &vm.rt.metas, obj, key) {
            Ok(v) => {
                // Every indexing instruction has its destination first.
                let mut w = Window::take(&mut vm.rt.stack.regs, base_of(bf));
                w[i.a()] = v;
                next(vm, pc, bf)
            }
            Err(e) => throw(vm, pc, e),
        };
    }
    throw(vm, pc, x)
}

#[inline(always)]
fn flow_step(f: Flow) -> Step {
    match f {
        Flow::Reload | Flow::Switched => Step::Reload,
        Flow::Done => Step::Done,
    }
}

/// A collection step is due after the instruction before `pc`.
#[cold]
#[inline(never)]
fn gc_point(vm: &mut Vm, pc: usize, base: usize) -> bool {
    save_pc(vm, pc);
    let live = (base + WINDOW).max(vm.rt.top);
    vm.gc_safepoint(live)
}

macro_rules! safepoint {
    ($vm:ident, $pc:expr, $bf:ident) => {
        if $vm.heap.should_step() && gc_point($vm, $pc, base_of($bf)) {
            return Step::NoMemory;
        }
    };
}

/// The frame's register window, at the start of a handler. When the stack
/// doesn't cover it (a native shrank it), the handler leaves through
/// [`regrow`], which runs the instruction again: a call in tail position,
/// so the handler's own path keeps nothing across a call and needs no
/// saved registers. Only for use before the handler has changed anything.
macro_rules! win {
    ($vm:ident, $pc:ident, $i:ident, $bf:ident) => {
        match Window::get(&mut $vm.rt.stack.regs, base_of($bf)) {
            Some(w) => w,
            None => return regrow($vm, $pc, $i, $bf),
        }
    };
}

/// The window again, after the handler called something that may have
/// resized the stack.
macro_rules! rewin {
    ($vm:ident, $bf:ident) => {
        Window::take(&mut $vm.rt.stack.regs, base_of($bf))
    };
}

/// Extend the stack to cover the frame's window and run instruction `i`
/// (whose successor is `pc`) again.
#[cold]
#[inline(never)]
fn regrow(vm: &mut Vm, pc: usize, i: Insn, bf: u64) -> Step {
    let _ = Window::take(&mut vm.rt.stack.regs, base_of(bf));
    HANDLERS[i.code()](vm, pc, i, bf)
}

macro_rules! tri {
    ($vm:ident, $pc:ident, $i:ident, $bf:ident; $e:expr) => {
        match $e {
            Ok(v) => v,
            Err(x) => return raise_step($vm, $pc, $i, $bf, x),
        }
    };
}

macro_rules! throw {
    ($vm:ident, $pc:ident, $i:ident, $bf:ident; $x:expr) => {{
        let x = $x;
        return raise_step($vm, $pc, $i, $bf, x);
    }};
}

macro_rules! by_parts {
    ($w:ident, $src:expr, |$v:ident| $body:expr) => {
        match $w[$src] {
            Value::Int(x) => {
                let $v = Value::Int(x);
                $body
            }
            Value::Float(f) => {
                let $v = Value::Float(f);
                $body
            }
            Value::Bool(b) => {
                let $v = Value::Bool(b);
                $body
            }
            $v => $body,
        }
    };
}

#[inline(always)]
fn jump(pc: usize, off: i32) -> usize {
    (pc as isize + off as isize) as usize
}

/// Make `closure`, running the code `code_id`, the running function.
/// Returns whether the debugger wants the other loop for it.
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
        if vm.rt.debug.watch
            && vm.debug_switches(&vm.rt.cur, closure, vm.rt.stack.depth(), false)
        {
            return true;
        }
    }
    false
}

/// Go on after a call or return made by the instruction before `pc`.
#[inline(always)]
fn frames_step(vm: &mut Vm, pc: usize, i: Insn, bf: u64, r: Result<Frames, Exc>) -> Step {
    match r {
        Ok(Frames::Lua(f)) => {
            if switch_code(vm, f.closure, f.code_id) {
                save_pc(vm, f.pc);
                return Step::Switch;
            }
            // Size the stack for the frame's window.
            let _ = Window::take(&mut vm.rt.stack.regs, f.base);
            next(vm, f.pc, with_base(f.base, bf))
        }
        Ok(Frames::Same) => next(vm, pc, bf),
        Ok(Frames::SameAllocated) => {
            safepoint!(vm, pc, bf);
            next(vm, pc, bf)
        }
        Ok(Frames::SameResumed) => resumed(vm, pc, bf),
        Ok(Frames::Flow(f)) => flow_step(f),
        Err(x) => raise_step(vm, pc, i, bf, x),
    }
}

/// Something returned into the running frame through the general path: it
/// may have armed a hook.
#[inline(never)]
fn resumed(vm: &mut Vm, pc: usize, bf: u64) -> Step {
    if vm.hooked_here(&vm.rt.cur, vm.rt.closure, vm.rt.stack.depth()) {
        return Step::Switch;
    }
    safepoint!(vm, pc, bf);
    next(vm, pc, bf)
}

impl Vm {
    /// The plain (unhooked) interpreter: run the frame on top until the
    /// run is finished or a hook wants the other loop.
    pub(crate) fn run_threaded(&mut self) -> Result<Ran, LuaError> {
        'reload: loop {
            if self.heap.should_step() {
                let base = self.rt.stack.frames.last().expect("a frame").base as usize;
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
                unreachable!("run starts on a Lua frame")
            };
            self.rt.closure = closure;
            if code_id != Rc::as_ptr(&self.rt.cur) as usize {
                self.rt.cur = self.heap.get(closure).code.clone();
            }
            #[cfg(feature = "debugger")]
            if self.hooked_here(&self.rt.cur, closure, self.rt.stack.depth()) {
                return Ok(Ran::Switch);
            }
            let mut base = frame.base as usize;
            let mut pc = frame.pc as usize;
            loop {
                let _ = Window::take(&mut self.rt.stack.regs, base);
                let i = self.rt.cur.words[pc];
                match HANDLERS[i.code()](self, pc + 1, i, with_base(base, FUEL)) {
                    Step::Continue => {
                        let f = self.rt.stack.running();
                        base = f.base as usize;
                        pc = f.pc as usize;
                    }
                    Step::Reload => continue 'reload,
                    Step::Done => return Ok(Ran::Done),
                    Step::Switch => return Ok(Ran::Switch),
                    Step::NoMemory => return Ok(Ran::NoMemory),
                    Step::Slow | Step::Returned => unreachable!("only the second cut"),
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

    /// [`Vm::call_from_loop`] for the threaded interpreter: the running
    /// closure and code come from the VM.
    #[inline(always)]
    fn call_threaded(
        &mut self,
        callee: Value,
        base: usize,
        pc: usize,
        f: usize,
        n: usize,
        want: u8,
    ) -> Result<Frames, Exc> {
        let fa = base + f;
        match callee {
            Value::Closure(c) => {
                let shape = if c == self.rt.closure {
                    Shape::of(&self.rt.cur)
                } else {
                    Shape::of(&self.heap.get(c).code)
                };
                let new_base =
                    match enter_lua_frame_fast(&mut self.rt, &shape, c, fa, n, want, pc as u32) {
                        Some(b) => b,
                        None => return self.call_lua_general(c, fa, n, want, pc),
                    };
                Ok(Frames::Lua(LuaFrame {
                    closure: c,
                    base: new_base,
                    pc: 0,
                    code_id: shape.code_id,
                }))
            }
            _ => self.call_other(callee, fa, n, want, pc),
        }
    }

    #[inline(never)]
    fn call_lua_general(
        &mut self,
        c: Gc<Closure>,
        fa: usize,
        n: usize,
        want: u8,
        pc: usize,
    ) -> Result<Frames, Exc> {
        save_pc(self, pc);
        let shape = Shape::of(&self.heap.get(c).code);
        let e = enter_lua_frame_general(&mut self.rt, shape, c, fa, n, want)?;
        Ok(Frames::Lua(LuaFrame {
            closure: c,
            base: e.base,
            pc: 0,
            code_id: shape.code_id,
        }))
    }

    /// A call of anything but a Lua closure: a native (its fast path, or
    /// in place), or a value with `__call`.
    #[inline(never)]
    fn call_other(
        &mut self,
        callee: Value,
        fa: usize,
        n: usize,
        want: u8,
        pc: usize,
    ) -> Result<Frames, Exc> {
        match callee {
            Value::Native(id) => {
                let fast = match self.rt.natives.get(id).fast {
                    Some(fast) if want <= 1 || want == MULTI => {
                        let cx = FastCtx::new(self, want);
                        self.rt
                            .stack
                            .regs
                            .get(fa + 1..fa + 1 + n)
                            .and_then(|a| fast(cx, a))
                    }
                    _ => None,
                };
                if let Some(v) = fast {
                    if want != 0 {
                        self.rt.stack.regs[fa] = v;
                    }
                    self.rt.top = fa + want.min(1) as usize;
                    return Ok(Frames::Same);
                }
                save_pc(self, pc);
                let depth = self.rt.stack.depth();
                Ok(match self.call_native_in_loop(fa, n, want, id, depth)? {
                    NativeStep::Returned => Frames::SameAllocated,
                    NativeStep::Resumed => Frames::SameResumed,
                    NativeStep::Flow(flow) => Frames::Flow(flow),
                })
            }
            _ => {
                save_pc(self, pc);
                let d = self.rt.stack.depth();
                Ok(match self.call_value(fa, n, want)? {
                    Flow::Reload if self.rt.stack.frames.len() == d => Frames::SameResumed,
                    flow => Frames::Flow(flow),
                })
            }
        }
    }
}

// Loads.

#[inline(never)]

fn h_move(vm: &mut Vm, pc: usize, i: Insn, bf: u64) -> Step {
    let (dst, src) = i.r_move();
    let mut w = win!(vm, pc, i, bf);
    match w[src] {
        Value::Int(x) => w[dst] = Value::Int(x),
        Value::Float(f) => w[dst] = Value::Float(f),
        Value::Table(t) => w[dst] = Value::Table(t),
        Value::Str(x) => w[dst] = Value::Str(x),
        Value::Closure(c) => w[dst] = Value::Closure(c),
        Value::Bool(b) => w[dst] = Value::Bool(b),
        v => w[dst] = v,
    }
    next(vm, pc, bf)
}

#[inline(never)]

fn h_load_nil(vm: &mut Vm, pc: usize, i: Insn, bf: u64) -> Step {
    let (dst, count) = i.r_load_nil();
    let mut w = win!(vm, pc, i, bf);
    let at = dst as usize;
    for r in &mut w[at..at + count as usize] {
        *r = Value::Nil;
    }
    next(vm, pc, bf)
}

#[inline(never)]

fn h_load_bool(vm: &mut Vm, pc: usize, i: Insn, bf: u64) -> Step {
    let (dst, value) = i.r_load_bool();
    let mut w = win!(vm, pc, i, bf);
    w[dst] = Value::Bool(value);
    next(vm, pc, bf)
}

#[inline(never)]

fn h_load_int(vm: &mut Vm, pc: usize, i: Insn, bf: u64) -> Step {
    let (dst, value) = i.r_load_int();
    let mut w = win!(vm, pc, i, bf);
    w[dst] = Value::Int(value as i64);
    next(vm, pc, bf)
}

#[inline(never)]

fn h_load_const(vm: &mut Vm, pc: usize, i: Insn, bf: u64) -> Step {
    let (dst, k) = i.r_load_const();
    let mut w = win!(vm, pc, i, bf);
    w[dst] = vm.rt.cur.consts[k as usize];
    next(vm, pc, bf)
}

// Cells and captures.

#[inline(always)]
fn cell_base(vm: &Vm) -> usize {
    vm.rt.stack.running().cell_base as usize
}

#[inline(never)]

fn h_cell_get(vm: &mut Vm, pc: usize, i: Insn, bf: u64) -> Step {
    let (dst, cell) = i.r_cell_get();
    let c = vm.rt.stack.cells[cell_base(vm) + cell as usize];
    let mut w = win!(vm, pc, i, bf);
    w[dst] = vm.heap.get(c).value;
    next(vm, pc, bf)
}

#[inline(never)]

fn h_cap_get(vm: &mut Vm, pc: usize, i: Insn, bf: u64) -> Step {
    let (dst, cap) = i.r_cap_get();
    let c = vm.heap.get(vm.rt.closure).captures[cap as usize];
    let mut w = win!(vm, pc, i, bf);
    w[dst] = vm.heap.get(c).value;
    next(vm, pc, bf)
}

#[inline(never)]

fn h_cap_self(vm: &mut Vm, pc: usize, i: Insn, bf: u64) -> Step {
    let dst = i.r_cap_self();
    let mut w = win!(vm, pc, i, bf);
    w[dst] = Value::Closure(vm.rt.closure);
    next(vm, pc, bf)
}

#[inline(never)]

fn h_cap_set(vm: &mut Vm, pc: usize, i: Insn, bf: u64) -> Step {
    let (cap, src) = i.r_cap_set();
    let c = vm.heap.get(vm.rt.closure).captures[cap as usize];
    let w = win!(vm, pc, i, bf);
    by_parts!(w, src, |v| vm.heap.write_for(c, &v).value = v);
    next(vm, pc, bf)
}

#[inline(never)]

fn h_cell_set(vm: &mut Vm, pc: usize, i: Insn, bf: u64) -> Step {
    let (cell, src) = i.r_cell_set();
    let c = vm.rt.stack.cells[cell_base(vm) + cell as usize];
    let w = win!(vm, pc, i, bf);
    let v = w[src];
    vm.heap.write_for(c, &v).value = v;
    next(vm, pc, bf)
}

#[inline(never)]

fn h_cell_new(vm: &mut Vm, pc: usize, i: Insn, bf: u64) -> Step {
    let (cell, src) = i.r_cell_new();
    let at = cell_base(vm) + cell as usize;
    let w = win!(vm, pc, i, bf);
    let v = w[src];
    vm.op_cell_new(at, v);
    safepoint!(vm, pc, bf);
    next(vm, pc, bf)
}

// Globals and tables.

#[inline(never)]

fn h_get_global(vm: &mut Vm, pc: usize, i: Insn, bf: u64) -> Step {
    let (dst, _name, ic) = i.r_get_global();
    let env_cell = vm.heap.get(vm.rt.closure).captures[0];
    match vm.heap.get(env_cell).value {
        Value::Table(t) => {
            let cur = &vm.rt.cur;
            let cache = &cur.caches[ic as usize];
            let tab = vm.heap.get(t);
            let mut w = win!(vm, pc, i, bf);
            match tab.cached_str_packed(cache.slot.get() as usize, cache.key) {
                Some((tag, p)) if tag != Tag::Nil || tab.meta.is_none() => {
                    Value::write_unpacked(&mut w[dst], tag, p)
                }
                _ => return get_global_miss(vm, pc, i, bf),
            }
        }
        _ => return get_global_miss(vm, pc, i, bf),
    }
    next(vm, pc, bf)
}

#[inline(never)]
fn get_global_miss(vm: &mut Vm, pc: usize, i: Insn, bf: u64) -> Step {
    let (dst, name, ic) = i.r_get_global();
    let env_cell = vm.heap.get(vm.rt.closure).captures[0];
    match vm.heap.get(env_cell).value {
        Value::Table(t) => {
            let cur = &vm.rt.cur;
            let cache = &cur.caches[ic as usize];
            let k = const_str(&cur.consts, name);
            let v = tri!(vm, pc, i, bf; get_str_miss(&vm.heap, &vm.rt.metas, t, k, cache));
            let mut w = rewin!(vm, bf);
            w[dst] = v;
        }
        other => throw!(vm, pc, i, bf; env_error(&vm.heap, &vm.rt.metas, other)),
    }
    next(vm, pc, bf)
}

#[inline(never)]

fn h_get_field(vm: &mut Vm, pc: usize, i: Insn, bf: u64) -> Step {
    let (dst, obj, _key, ic) = i.r_get_field();
    let mut w = win!(vm, pc, i, bf);
    let &Value::Table(t) = &w[obj] else {
        return get_field_miss(vm, pc, i, bf);
    };
    let cache = &vm.rt.cur.caches[ic as usize];
    let tab = vm.heap.get(t);
    match tab.cached_str_packed(cache.slot.get() as usize, cache.key) {
        Some((tag, p)) if tag != Tag::Nil || tab.meta.is_none() => {
            Value::write_unpacked(&mut w[dst], tag, p)
        }
        _ => return get_field_miss(vm, pc, i, bf),
    }
    next(vm, pc, bf)
}

#[inline(never)]
fn get_field_miss(vm: &mut Vm, pc: usize, i: Insn, bf: u64) -> Step {
    let (dst, obj, key, ic) = i.r_get_field();
    let mut w = win!(vm, pc, i, bf);
    let cur = &vm.rt.cur;
    let &Value::Table(t) = &w[obj] else {
        throw!(vm, pc, i, bf; Exc::index(w[obj], cur.consts[key as usize]))
    };
    let cache = &cur.caches[ic as usize];
    let k = const_str(&cur.consts, key);
    w[dst] = tri!(vm, pc, i, bf; get_str_miss(&vm.heap, &vm.rt.metas, t, k, cache));
    next(vm, pc, bf)
}

#[inline(never)]

fn h_set_field(vm: &mut Vm, pc: usize, i: Insn, bf: u64) -> Step {
    let (obj, _key, src, ic) = i.r_set_field();
    let w = win!(vm, pc, i, bf);
    let &Value::Table(t) = &w[obj] else {
        return set_field_miss(vm, pc, i, bf);
    };
    let cache = &vm.rt.cur.caches[ic as usize];
    let slot = cache.slot.get() as usize;
    let tab = vm.heap.get(t);
    if matches!(tab.cached_str_packed(slot, cache.key),
        Some((tag, _)) if tab.meta.is_none() || tag != Tag::Nil)
    {
        by_parts!(w, src, |v| vm.heap.write_for(t, &v).set_slot(slot, v));
    } else {
        return set_field_miss(vm, pc, i, bf);
    }
    next(vm, pc, bf)
}

#[inline(never)]
fn set_field_miss(vm: &mut Vm, pc: usize, i: Insn, bf: u64) -> Step {
    let (obj, key, src, ic) = i.r_set_field();
    let w = win!(vm, pc, i, bf);
    let cur = &vm.rt.cur;
    let &Value::Table(t) = &w[obj] else {
        throw!(vm, pc, i, bf; Exc::new_index(w[obj], cur.consts[key as usize], w[src]))
    };
    let cache = &cur.caches[ic as usize];
    let k = const_str(&cur.consts, key);
    let v = w[src];
    tri!(vm, pc, i, bf; set_str_miss(&mut vm.heap, &vm.rt.metas, t, k, v, cache));
    safepoint!(vm, pc, bf);
    next(vm, pc, bf)
}

#[inline(never)]

fn h_get_index(vm: &mut Vm, pc: usize, i: Insn, bf: u64) -> Step {
    let (dst, obj, key) = i.r_get_index();
    let mut w = win!(vm, pc, i, bf);
    if let (&Value::Table(t), &Value::Int(k)) = (&w[obj], &w[key]) {
        if vm.heap.get(t).read_int_present(k, &mut w[dst]) {
            return next(vm, pc, bf);
        }
    }
    get_index_slow(vm, pc, i, bf)
}

#[inline(never)]
fn get_index_slow(vm: &mut Vm, pc: usize, i: Insn, bf: u64) -> Step {
    let (dst, obj, key) = i.r_get_index();
    let mut w = win!(vm, pc, i, bf);
    let &Value::Table(t) = &w[obj] else {
        throw!(vm, pc, i, bf; Exc::index(w[obj], w[key]))
    };
    let tab = vm.heap.get(t);
    match w[key] {
        Value::Int(k) => {
            if !tab.read_int_present(k, &mut w[dst]) {
                let v = tab.get_int(k);
                if v.is_nil() && tab.meta.is_some() {
                    throw!(vm, pc, i, bf; Exc::index(Value::Table(t), Value::Int(k)))
                }
                w[dst] = v;
            }
        }
        k => {
            w[dst] = tab.get(&vm.heap, k);
            if tab.meta.is_some() && w[dst].is_nil() {
                throw!(vm, pc, i, bf; Exc::index(Value::Table(t), k))
            }
        }
    }
    next(vm, pc, bf)
}

#[inline(never)]

fn h_set_index(vm: &mut Vm, pc: usize, i: Insn, bf: u64) -> Step {
    let (obj, key, src) = i.r_set_index();
    let w = win!(vm, pc, i, bf);
    if let (&Value::Table(t), &Value::Int(k)) = (&w[obj], &w[key]) {
        if let IntStore::Array = by_parts!(w, src, |v| table::store_int(&mut vm.heap, t, k, v)) {
            return next(vm, pc, bf);
        }
    }
    set_index_slow(vm, pc, i, bf)
}

/// The general `R[obj][R[key]] = R[src]`. An integer store that already
/// took the array path is repeated here, which stores the same value again.
#[inline(never)]
fn set_index_slow(vm: &mut Vm, pc: usize, i: Insn, bf: u64) -> Step {
    let (obj, key, src) = i.r_set_index();
    let w = win!(vm, pc, i, bf);
    let &Value::Table(t) = &w[obj] else {
        throw!(vm, pc, i, bf; Exc::new_index(w[obj], w[key], w[src]))
    };
    match w[key] {
        Value::Int(k) => match table::store_int(&mut vm.heap, t, k, w[src]) {
            IntStore::Array => {}
            IntStore::Other => safepoint!(vm, pc, bf),
            IntStore::Slow => {
                throw!(vm, pc, i, bf; Exc::new_index(w[obj], w[key], w[src]))
            }
        },
        k => {
            let v = w[src];
            if vm.heap.get(t).meta.is_some() {
                throw!(vm, pc, i, bf; Exc::new_index(w[obj], k, v))
            }
            if let Err(e) = table::set(&mut vm.heap, t, k, v) {
                throw!(vm, pc, i, bf; ops::store_failed(e, t, k, v));
            }
            safepoint!(vm, pc, bf);
        }
    }
    next(vm, pc, bf)
}

#[inline(never)]

fn h_get_index_int(vm: &mut Vm, pc: usize, i: Insn, bf: u64) -> Step {
    let (dst, obj, key) = i.r_get_index_int();
    let mut w = win!(vm, pc, i, bf);
    if let &Value::Table(t) = &w[obj] {
        if vm.heap.get(t).read_int_present(key as i64, &mut w[dst]) {
            return next(vm, pc, bf);
        }
    }
    get_index_int_slow(vm, pc, i, bf)
}

#[inline(never)]
fn get_index_int_slow(vm: &mut Vm, pc: usize, i: Insn, bf: u64) -> Step {
    let (dst, obj, key) = i.r_get_index_int();
    let mut w = win!(vm, pc, i, bf);
    let &Value::Table(t) = &w[obj] else {
        throw!(vm, pc, i, bf; Exc::index(w[obj], Value::Int(key as i64)))
    };
    let tab = vm.heap.get(t);
    let v = tab.get_int(key as i64);
    if v.is_nil() && tab.meta.is_some() {
        throw!(vm, pc, i, bf; Exc::index(Value::Table(t), Value::Int(key as i64)))
    }
    w[dst] = v;
    next(vm, pc, bf)
}

#[inline(never)]

fn h_set_index_int(vm: &mut Vm, pc: usize, i: Insn, bf: u64) -> Step {
    let (obj, key, src) = i.r_set_index_int();
    let w = win!(vm, pc, i, bf);
    if let &Value::Table(t) = &w[obj] {
        if let IntStore::Array =
            by_parts!(w, src, |v| table::store_int(&mut vm.heap, t, key as i64, v))
        {
            return next(vm, pc, bf);
        }
    }
    set_index_int_slow(vm, pc, i, bf)
}

#[inline(never)]
fn set_index_int_slow(vm: &mut Vm, pc: usize, i: Insn, bf: u64) -> Step {
    let (obj, key, src) = i.r_set_index_int();
    let w = win!(vm, pc, i, bf);
    let &Value::Table(t) = &w[obj] else {
        throw!(vm, pc, i, bf; Exc::new_index(w[obj], Value::Int(key as i64), w[src]))
    };
    match table::store_int(&mut vm.heap, t, key as i64, w[src]) {
        IntStore::Array => {}
        IntStore::Other => safepoint!(vm, pc, bf),
        IntStore::Slow => {
            throw!(vm, pc, i, bf; Exc::new_index(w[obj], Value::Int(key as i64), w[src]))
        }
    }
    next(vm, pc, bf)
}

#[inline(never)]

fn h_self_call(vm: &mut Vm, pc: usize, i: Insn, bf: u64) -> Step {
    let (dst, obj, key, ic) = i.r_self_call();
    let mut w = win!(vm, pc, i, bf);
    let cur = &vm.rt.cur;
    let Value::Str(k) = cur.consts[key as usize] else {
        unreachable!("method names are strings")
    };
    let t = match w[obj] {
        Value::Table(t) => t,
        o => {
            w[dst + 1] = o;
            throw!(vm, pc, i, bf; Exc::index(o, Value::Str(k)))
        }
    };
    w[dst + 1] = Value::Table(t);
    match method_lookup(&vm.heap, &vm.rt.metas, cur, ic, k, t) {
        Some((tag, p)) => Value::write_unpacked(&mut w[dst], tag, p),
        None => throw!(vm, pc, i, bf; Exc::index(Value::Table(t), Value::Str(k))),
    }
    next(vm, pc, bf)
}

// Arithmetic. The handler takes the two same-type cases; everything else
// (mixed numbers, coercions, metamethods, errors) is the slow function.

macro_rules! arith_rr {
    ($name:ident, $slow:ident, $read:ident, $op:expr, |$x:ident, $y:ident| $int:expr, $flt:expr) => {
        #[inline(never)]
        fn $name(vm: &mut Vm, pc: usize, i: Insn, bf: u64) -> Step {
            let (dst, a, b) = i.$read();
            let mut w = win!(vm, pc, i, bf);
            match (&w[a], &w[b]) {
                (&Value::Int($x), &Value::Int($y)) => w[dst] = $int,
                (&Value::Float($x), &Value::Float($y)) => w[dst] = Value::Float($flt),
                _ => return $slow(vm, pc, i, bf),
            }
            next(vm, pc, bf)
        }
        #[inline(never)]
        fn $slow(vm: &mut Vm, pc: usize, i: Insn, bf: u64) -> Step {
            let (dst, a, b) = i.$read();
            let mut w = win!(vm, pc, i, bf);
            tri!(vm, pc, i, bf; fast::arith(&mut w, dst, R(a), R(b), $op, &vm.heap, &vm.rt.metas));
            next(vm, pc, bf)
        }
    };
}

arith_rr!(h_add, add_slow, r_add, Arith::Add, |x, y| Value::Int(x.wrapping_add(y)), x + y);
arith_rr!(h_sub, sub_slow, r_sub, Arith::Sub, |x, y| Value::Int(x.wrapping_sub(y)), x - y);
arith_rr!(h_mul, mul_slow, r_mul, Arith::Mul, |x, y| Value::Int(x.wrapping_mul(y)), x * y);

/// Arithmetic whose integer case isn't a single operation (division, a
/// remainder, a constant operand): one function, as the old loop's arm.
macro_rules! arith_gen {
    ($name:ident, $read:ident, |$w:ident, $cur:ident, $p:ident| $x:expr, $y:expr, $op:expr) => {
        #[inline(never)]
        fn $name(vm: &mut Vm, pc: usize, i: Insn, bf: u64) -> Step {
            let $p = i.$read();
            let mut $w = win!(vm, pc, i, bf);
            let $cur = &vm.rt.cur;
            tri!(vm, pc, i, bf; fast::arith(&mut $w, $p.0, $x, $y, $op, &vm.heap, &vm.rt.metas));
            next(vm, pc, bf)
        }
    };
}

arith_gen!(h_div, r_div, |w, _cur, p| R(p.1), R(p.2), Arith::Div);
arith_gen!(h_mod, r_mod, |w, _cur, p| R(p.1), R(p.2), Arith::Mod);
arith_gen!(h_mul_k, r_mul_k, |w, cur, p| R(p.1), K(&cur.consts[p.2 as usize]), Arith::Mul);
arith_gen!(h_mod_k, r_mod_k, |w, cur, p| R(p.1), K(&cur.consts[p.2 as usize]), Arith::Mod);
arith_gen!(h_k_add, r_k_add, |w, cur, p| K(&cur.consts[p.1 as usize]), R(p.2), Arith::Add);
arith_gen!(h_k_mul, r_k_mul, |w, cur, p| K(&cur.consts[p.1 as usize]), R(p.2), Arith::Mul);
arith_gen!(h_k_div, r_k_div, |w, cur, p| K(&cur.consts[p.1 as usize]), R(p.2), Arith::Div);

#[inline(never)]

fn h_add_int(vm: &mut Vm, pc: usize, i: Insn, bf: u64) -> Step {
    let (dst, a, imm) = i.r_add_int();
    let mut w = win!(vm, pc, i, bf);
    match w[a] {
        Value::Int(x) => w[dst] = Value::Int(x.wrapping_add(imm as i64)),
        Value::Float(f) => w[dst] = Value::Float(f + imm as f64),
        _ => return add_int_slow(vm, pc, i, bf),
    }
    next(vm, pc, bf)
}

#[inline(never)]
fn add_int_slow(vm: &mut Vm, pc: usize, i: Insn, bf: u64) -> Step {
    let (dst, a, imm) = i.r_add_int();
    let mut w = win!(vm, pc, i, bf);
    let x = w[a];
    w[dst] = tri!(vm, pc, i, bf; ops::arith_slow(&vm.heap, &vm.rt.metas, Arith::Add, x, Value::Int(imm as i64)));
    next(vm, pc, bf)
}

#[inline(never)]

fn h_sub_int(vm: &mut Vm, pc: usize, i: Insn, bf: u64) -> Step {
    let (dst, a, imm) = i.r_sub_int();
    let mut w = win!(vm, pc, i, bf);
    match w[a] {
        Value::Int(x) => w[dst] = Value::Int(x.wrapping_sub(imm as i64)),
        // Adding the negated integer, as lua5.5 does: `-0.0 - 0` is `0.0` there.
        Value::Float(f) => w[dst] = Value::Float(f + (-(imm as i64)) as f64),
        _ => return sub_int_slow(vm, pc, i, bf),
    }
    next(vm, pc, bf)
}

#[inline(never)]
fn sub_int_slow(vm: &mut Vm, pc: usize, i: Insn, bf: u64) -> Step {
    let (dst, a, imm) = i.r_sub_int();
    let mut w = win!(vm, pc, i, bf);
    let x = w[a];
    w[dst] = tri!(vm, pc, i, bf; ops::arith_slow(&vm.heap, &vm.rt.metas, Arith::Sub, x, Value::Int(imm as i64)));
    next(vm, pc, bf)
}

macro_rules! bitwise {
    ($name:ident, $read:ident, $op:expr) => {
        #[inline(never)]
        fn $name(vm: &mut Vm, pc: usize, i: Insn, bf: u64) -> Step {
            let (dst, a, b) = i.$read();
            let mut w = win!(vm, pc, i, bf);
            tri!(vm, pc, i, bf; fast::bitwise(&mut w, dst, a, b, $op, &vm.heap, &vm.rt.metas));
            next(vm, pc, bf)
        }
    };
}

bitwise!(h_b_and, r_b_and, BitOp::And);
bitwise!(h_b_or, r_b_or, BitOp::Or);
bitwise!(h_b_xor, r_b_xor, BitOp::Xor);
bitwise!(h_shl, r_shl, BitOp::Shl);
bitwise!(h_shr, r_shr, BitOp::Shr);

#[inline(never)]

fn h_not(vm: &mut Vm, pc: usize, i: Insn, bf: u64) -> Step {
    let (dst, src) = i.r_not();
    let mut w = win!(vm, pc, i, bf);
    w[dst] = Value::Bool(w[src].is_falsy());
    next(vm, pc, bf)
}

#[inline(never)]

fn h_len(vm: &mut Vm, pc: usize, i: Insn, bf: u64) -> Step {
    let (dst, src) = i.r_len();
    let mut w = win!(vm, pc, i, bf);
    let x = w[src];
    w[dst] = tri!(vm, pc, i, bf; op_len(&vm.heap, &vm.rt.metas, x));
    next(vm, pc, bf)
}

// Branches.

#[inline(never)]

fn h_jump(vm: &mut Vm, pc: usize, i: Insn, bf: u64) -> Step {
    next(vm, jump(pc, i.r_jump()), bf)
}

#[inline(never)]

fn h_jump_if_true(vm: &mut Vm, pc: usize, i: Insn, bf: u64) -> Step {
    let (src, off) = i.r_jump_if_true();
    let w = win!(vm, pc, i, bf);
    let pc = if !w[src].is_falsy() { jump(pc, off) } else { pc };
    next(vm, pc, bf)
}

#[inline(never)]

fn h_jump_if_false(vm: &mut Vm, pc: usize, i: Insn, bf: u64) -> Step {
    let (src, off) = i.r_jump_if_false();
    let w = win!(vm, pc, i, bf);
    let pc = if w[src].is_falsy() { jump(pc, off) } else { pc };
    next(vm, pc, bf)
}

/// A compare-and-jump: the same-type numeric cases in the handler, the
/// generic comparison in the slow function. `$want` is the outcome that
/// jumps.
macro_rules! cmp_jump {
    ($name:ident, $slow:ident, $read:ident, $cmp:expr, $want:expr, |$x:ident, $y:ident| $test:expr) => {
        #[inline(never)]
        fn $name(vm: &mut Vm, pc: usize, i: Insn, bf: u64) -> Step {
            let (a, b, off) = i.$read();
            let w = win!(vm, pc, i, bf);
            let c = match (&w[a], &w[b]) {
                (&Value::Int($x), &Value::Int($y)) => $test,
                (&Value::Float($x), &Value::Float($y)) => $test,
                _ => return $slow(vm, pc, i, bf),
            };
            let pc = if c == $want { jump(pc, off) } else { pc };
            next(vm, pc, bf)
        }
        #[inline(never)]
        fn $slow(vm: &mut Vm, pc: usize, i: Insn, bf: u64) -> Step {
            let (a, b, off) = i.$read();
            let w = win!(vm, pc, i, bf);
            let c = tri!(vm, pc, i, bf; fast::compare(&w, a, b, $cmp, &vm.heap, &vm.rt.metas));
            let pc = if c == $want { jump(pc, off) } else { pc };
            next(vm, pc, bf)
        }
    };
}

cmp_jump!(h_jump_lt, jump_lt_slow, r_jump_lt, Cmp::Lt, true, |x, y| x < y);
cmp_jump!(h_jump_not_lt, jump_not_lt_slow, r_jump_not_lt, Cmp::Lt, false, |x, y| x < y);
cmp_jump!(h_jump_le, jump_le_slow, r_jump_le, Cmp::Le, true, |x, y| x <= y);
cmp_jump!(h_jump_not_le, jump_not_le_slow, r_jump_not_le, Cmp::Le, false, |x, y| x <= y);

macro_rules! eq_jump {
    ($name:ident, $slow:ident, $read:ident, $want:expr) => {
        #[inline(never)]
        fn $name(vm: &mut Vm, pc: usize, i: Insn, bf: u64) -> Step {
            let (a, b, off) = i.$read();
            let w = win!(vm, pc, i, bf);
            let c = match (&w[a], &w[b]) {
                (&Value::Int(x), &Value::Int(y)) => x == y,
                (&Value::Float(x), &Value::Float(y)) => x == y,
                (&Value::Nil, &Value::Nil) => true,
                (&Value::Nil, _) | (_, &Value::Nil) => false,
                _ => return $slow(vm, pc, i, bf),
            };
            let pc = if c == $want { jump(pc, off) } else { pc };
            next(vm, pc, bf)
        }
        #[inline(never)]
        fn $slow(vm: &mut Vm, pc: usize, i: Insn, bf: u64) -> Step {
            let (a, b, off) = i.$read();
            let w = win!(vm, pc, i, bf);
            let c = tri!(vm, pc, i, bf; fast::equal(&w, a, b, &vm.heap, &vm.rt.metas));
            let pc = if c == $want { jump(pc, off) } else { pc };
            next(vm, pc, bf)
        }
    };
}

eq_jump!(h_jump_eq, jump_eq_slow, r_jump_eq, true);
eq_jump!(h_jump_ne, jump_ne_slow, r_jump_ne, false);

#[inline(never)]

fn h_jump_eq_int(vm: &mut Vm, pc: usize, i: Insn, bf: u64) -> Step {
    let (a, imm, want, off) = i.r_jump_eq_int();
    let w = win!(vm, pc, i, bf);
    let c = match w[a] {
        Value::Int(x) => x == imm as i64,
        Value::Float(f) => f == imm as f64,
        _ => false,
    };
    let pc = if c == want { jump(pc, off) } else { pc };
    next(vm, pc, bf)
}

macro_rules! cmp_int_jump {
    ($name:ident, $slow:ident, $read:ident, $cmp:expr, |$x:ident, $y:ident| $test:expr) => {
        #[inline(never)]
        fn $name(vm: &mut Vm, pc: usize, i: Insn, bf: u64) -> Step {
            let (a, imm, want, off) = i.$read();
            let w = win!(vm, pc, i, bf);
            let c = match w[a] {
                Value::Int($x) => {
                    let $y = imm as i64;
                    $test
                }
                Value::Float($x) => {
                    let $y = imm as f64;
                    $test
                }
                _ => return $slow(vm, pc, i, bf),
            };
            let pc = if c == want { jump(pc, off) } else { pc };
            next(vm, pc, bf)
        }
        #[inline(never)]
        fn $slow(vm: &mut Vm, pc: usize, i: Insn, bf: u64) -> Step {
            let (a, imm, want, off) = i.$read();
            let w = win!(vm, pc, i, bf);
            let c = tri!(vm, pc, i, bf; fast::compare_int(&w, a, imm, $cmp, &vm.heap, &vm.rt.metas));
            let pc = if c == want { jump(pc, off) } else { pc };
            next(vm, pc, bf)
        }
    };
}

cmp_int_jump!(h_jump_lt_int, jump_lt_int_slow, r_jump_lt_int, Cmp::Lt, |x, y| x < y);
cmp_int_jump!(h_jump_le_int, jump_le_int_slow, r_jump_le_int, Cmp::Le, |x, y| x <= y);
cmp_int_jump!(h_jump_gt_int, jump_gt_int_slow, r_jump_gt_int, Cmp::Gt, |x, y| x > y);
cmp_int_jump!(h_jump_ge_int, jump_ge_int_slow, r_jump_ge_int, Cmp::Ge, |x, y| x >= y);

// Calls and returns.

#[inline(never)]

fn h_call(vm: &mut Vm, pc: usize, i: Insn, bf: u64) -> Step {
    let (func, nargs, nresults) = i.r_call();
    let base = base_of(bf);
    let n = if nargs == MULTI {
        vm.rt.top - (base + func as usize) - 1
    } else {
        nargs as usize
    };
    let w = win!(vm, pc, i, bf);
    let callee = w[func];
    let r = vm.call_threaded(callee, base, pc, func as usize, n, nresults);
    frames_step(vm, pc, i, bf, r)
}

#[inline(never)]

fn h_generic_call(vm: &mut Vm, pc: usize, i: Insn, bf: u64) -> Step {
    let (b, nvars) = i.r_generic_call();
    let base = base_of(bf);
    let mut w = win!(vm, pc, i, bf);
    let at = b as usize;
    w.copy_within(at..at + 3, at + 4);
    let callee = w[at + 4];
    let r = vm.call_threaded(callee, base, pc, at + 4, 2, nvars);
    frames_step(vm, pc, i, bf, r)
}

#[inline(never)]

fn h_tail_call(vm: &mut Vm, pc: usize, i: Insn, bf: u64) -> Step {
    let (func, nargs) = i.r_tail_call();
    let base = base_of(bf);
    let fa = base + func as usize;
    let n = if nargs == MULTI {
        vm.rt.top - fa - 1
    } else {
        nargs as usize
    };
    let w = win!(vm, pc, i, bf);
    let callee = w[func];
    if let Value::Closure(c) = callee {
        let code = &vm.heap.get(c).code;
        if !code.builtin && !matches!(code.code.first(), Some(Op::NativeEntry { .. })) {
            let f = vm.rt.stack.frames.pop().expect("frame");
            vm.rt.stack.cells.truncate(f.cell_base as usize);
            let dst = f.func as usize;
            vm.rt.stack.regs.copy_within(fa..fa + 1 + n, dst);
            if let Err(x) = vm.push_lua_frame(c, dst, n, f.want) {
                vm.rt.stack.frames.push(f);
                throw!(vm, pc, i, bf; x);
            }
            vm.rt.stack.frames.last_mut().expect("frame").tail = true;
            return Step::Reload;
        }
    }
    let r = vm.call_threaded(callee, base, pc, func as usize, n, MULTI);
    frames_step(vm, pc, i, bf, r)
}

#[inline(never)]

fn h_return(vm: &mut Vm, pc: usize, i: Insn, bf: u64) -> Step {
    let (first, count) = i.r_return();
    let src = base_of(bf) + first as usize;
    let n = if count == MULTI {
        vm.rt.top - src
    } else {
        count as usize
    };
    let r = vm.return_from_loop(src, n);
    frames_step(vm, pc, i, bf, r)
}

#[inline(never)]

fn h_return0(vm: &mut Vm, pc: usize, i: Insn, bf: u64) -> Step {
    let r = vm.return_from_loop(base_of(bf), 0);
    frames_step(vm, pc, i, bf, r)
}

#[inline(never)]

fn h_return1(vm: &mut Vm, pc: usize, i: Insn, bf: u64) -> Step {
    let src = i.r_return1();
    let from = base_of(bf) + src as usize;
    if let Some((caller, f)) = vm.lua_caller().filter(|(_, f)| f.want == 1) {
        let func = f.func as usize;
        vm.pop_frame(&f);
        fast::move_by_parts(&mut vm.rt.stack.regs, from, func);
        vm.rt.top = func + 1;
        frames_step(vm, pc, i, bf, Ok(Frames::Lua(caller)))
    } else {
        let r = vm.return_from_loop(from, 1);
        frames_step(vm, pc, i, bf, r)
    }
}

// Allocating instructions.

#[inline(never)]

fn h_new_table(vm: &mut Vm, pc: usize, i: Insn, bf: u64) -> Step {
    let (dst, array_hint, hash_hint) = i.r_new_table();
    let t = vm.op_new_table(array_hint as usize, hash_hint as usize);
    let mut w = rewin!(vm, bf);
    w[dst] = Value::Table(t);
    safepoint!(vm, pc, bf);
    next(vm, pc, bf)
}

#[inline(never)]

fn h_set_list(vm: &mut Vm, pc: usize, i: Insn, bf: u64) -> Step {
    let (tr, first, count, start) = i.r_set_list();
    let base = base_of(bf);
    let w = win!(vm, pc, i, bf);
    let Value::Table(t) = w[tr] else {
        unreachable!("SetList on a table")
    };
    let from = base + first as usize;
    let n = if count == MULTI {
        vm.rt.top - from
    } else {
        count as usize
    };
    if let Err(e) = vm.op_set_list(t, from, n, start as i64) {
        throw!(vm, pc, i, bf; e.into());
    }
    safepoint!(vm, pc, bf);
    next(vm, pc, bf)
}

#[inline(never)]

fn h_concat(vm: &mut Vm, pc: usize, i: Insn, bf: u64) -> Step {
    let (dst, first, count) = i.r_concat();
    let v = tri!(vm, pc, i, bf; vm.op_concat(base_of(bf) + first as usize, count as usize));
    let mut w = rewin!(vm, bf);
    w[dst] = v;
    safepoint!(vm, pc, bf);
    next(vm, pc, bf)
}

#[inline(never)]

fn h_closure(vm: &mut Vm, pc: usize, i: Insn, bf: u64) -> Step {
    let (dst, proto) = i.r_closure();
    let cb = cell_base(vm);
    let c = vm.op_closure(vm.rt.closure, cb, proto as usize);
    let mut w = rewin!(vm, bf);
    w[dst] = c;
    safepoint!(vm, pc, bf);
    next(vm, pc, bf)
}

#[inline(never)]

fn h_vararg_len(vm: &mut Vm, pc: usize, i: Insn, bf: u64) -> Step {
    let dst = i.r_vararg_len();
    let nvarargs = vm.rt.stack.running().nvarargs;
    let mut w = win!(vm, pc, i, bf);
    w[dst] = Value::Int(nvarargs as i64);
    next(vm, pc, bf)
}

// Loops.

#[inline(never)]

fn h_for_prep(vm: &mut Vm, pc: usize, i: Insn, bf: u64) -> Step {
    let (b, exit) = i.r_for_prep();
    let mut w = win!(vm, pc, i, bf);
    match (&w[b], &w[b + 1], &w[b + 2]) {
        (&Value::Int(x), &Value::Int(lim), &Value::Int(s)) if s > 0 => {
            if x <= lim {
                let count = lim.abs_diff(x) / s as u64;
                w[b + 1] = Value::Int(count as i64);
                w[b + 3] = Value::Int(x);
                next(vm, pc, bf)
            } else {
                next(vm, jump(pc, exit), bf)
            }
        }
        _ => for_prep_slow(vm, pc, i, bf),
    }
}

#[inline(never)]
fn for_prep_slow(vm: &mut Vm, pc: usize, i: Insn, bf: u64) -> Step {
    let (b, exit) = i.r_for_prep();
    let runs = tri!(vm, pc, i, bf; vm.op_for_prep(base_of(bf) + b as usize, false));
    let pc = if runs { pc } else { jump(pc, exit) };
    next(vm, pc, bf)
}

#[inline(never)]

fn h_for_loop(vm: &mut Vm, pc: usize, i: Insn, bf: u64) -> Step {
    let (b, back) = i.r_for_loop();
    let mut w = win!(vm, pc, i, bf);
    let at = b as usize;
    let mut to = pc;
    match &mut w[at..at + 4] {
        [Value::Int(cur), Value::Int(count), Value::Int(s), var] => {
            if *count > 0 {
                *count -= 1;
                *cur = cur.wrapping_add(*s);
                *var = Value::Int(*cur);
                to = jump(pc, back);
            }
        }
        [Value::Float(cur), Value::Float(lim), Value::Float(s), var] => {
            let n = *cur + *s;
            if if *s > 0.0 { n <= *lim } else { *lim <= n } {
                *cur = n;
                *var = Value::Float(n);
                to = jump(pc, back);
            }
        }
        _ => unreachable!("loop state"),
    }
    next(vm, to, bf)
}

#[inline(never)]

fn h_generic_loop(vm: &mut Vm, pc: usize, i: Insn, bf: u64) -> Step {
    let (b, back) = i.r_generic_loop();
    let mut w = win!(vm, pc, i, bf);
    let v = w[b + 4];
    let mut to = pc;
    if !v.is_nil() {
        w[b + 2] = v;
        to = jump(pc, back);
    }
    next(vm, to, bf)
}

/// The rare instructions, through the old loop's out-of-line function.
#[inline(never)]
fn h_cold(vm: &mut Vm, pc: usize, i: Insn, bf: u64) -> Step {
    let cur = vm.rt.cur.clone();
    let op = cur.code[pc - 1];
    match vm.cold_op(op, &cur, vm.rt.closure, base_of(bf), pc) {
        Ok(Cold::Next {
            pc: to,
            safepoint,
        }) => {
            if safepoint {
                safepoint!(vm, to, bf);
            }
            next(vm, to, bf)
        }
        Ok(Cold::Flow(flow)) => flow_step(flow),
        Err(x) => raise_step(vm, pc, i, bf, x),
    }
}

fn h_invalid(_vm: &mut Vm, _pc: usize, _i: Insn, _bf: u64) -> Step {
    unreachable!("not an instruction")
}

static HANDLERS: [Handler; 256] = {
    let mut t: [Handler; 256] = [h_invalid; 256];
    t[Opcode::Move as usize] = h_move;
    t[Opcode::LoadNil as usize] = h_load_nil;
    t[Opcode::LoadBool as usize] = h_load_bool;
    t[Opcode::LoadInt as usize] = h_load_int;
    t[Opcode::LoadConst as usize] = h_load_const;
    t[Opcode::LoadConstX as usize] = h_cold;
    t[Opcode::CellNew as usize] = h_cell_new;
    t[Opcode::CellGet as usize] = h_cell_get;
    t[Opcode::CellSet as usize] = h_cell_set;
    t[Opcode::CellClear as usize] = h_cold;
    t[Opcode::CapGet as usize] = h_cap_get;
    t[Opcode::CapSet as usize] = h_cap_set;
    t[Opcode::CapSelf as usize] = h_cap_self;
    t[Opcode::GetGlobal as usize] = h_get_global;
    t[Opcode::SetGlobal as usize] = h_cold;
    t[Opcode::CheckUndefined as usize] = h_cold;
    t[Opcode::NewTable as usize] = h_new_table;
    t[Opcode::GetField as usize] = h_get_field;
    t[Opcode::SetField as usize] = h_set_field;
    t[Opcode::GetIndex as usize] = h_get_index;
    t[Opcode::SetIndex as usize] = h_set_index;
    t[Opcode::GetIndexInt as usize] = h_get_index_int;
    t[Opcode::SetIndexInt as usize] = h_set_index_int;
    t[Opcode::SetList as usize] = h_set_list;
    t[Opcode::SelfCall as usize] = h_self_call;
    t[Opcode::Add as usize] = h_add;
    t[Opcode::Sub as usize] = h_sub;
    t[Opcode::Mul as usize] = h_mul;
    t[Opcode::Div as usize] = h_div;
    t[Opcode::IDiv as usize] = h_cold;
    t[Opcode::Mod as usize] = h_mod;
    t[Opcode::Pow as usize] = h_cold;
    t[Opcode::AddInt as usize] = h_add_int;
    t[Opcode::SubInt as usize] = h_sub_int;
    t[Opcode::AddK as usize] = h_cold;
    t[Opcode::SubK as usize] = h_cold;
    t[Opcode::MulK as usize] = h_mul_k;
    t[Opcode::DivK as usize] = h_cold;
    t[Opcode::IDivK as usize] = h_cold;
    t[Opcode::ModK as usize] = h_mod_k;
    t[Opcode::PowK as usize] = h_cold;
    t[Opcode::KAdd as usize] = h_k_add;
    t[Opcode::KSub as usize] = h_cold;
    t[Opcode::KMul as usize] = h_k_mul;
    t[Opcode::KDiv as usize] = h_k_div;
    t[Opcode::BAnd as usize] = h_b_and;
    t[Opcode::BOr as usize] = h_b_or;
    t[Opcode::BXor as usize] = h_b_xor;
    t[Opcode::Shl as usize] = h_shl;
    t[Opcode::Shr as usize] = h_shr;
    t[Opcode::Neg as usize] = h_cold;
    t[Opcode::BNot as usize] = h_cold;
    t[Opcode::Not as usize] = h_not;
    t[Opcode::Len as usize] = h_len;
    t[Opcode::Cast as usize] = h_cold;
    t[Opcode::Concat as usize] = h_concat;
    t[Opcode::Jump as usize] = h_jump;
    t[Opcode::JumpIfTrue as usize] = h_jump_if_true;
    t[Opcode::JumpIfFalse as usize] = h_jump_if_false;
    t[Opcode::JumpLt as usize] = h_jump_lt;
    t[Opcode::JumpNotLt as usize] = h_jump_not_lt;
    t[Opcode::JumpLe as usize] = h_jump_le;
    t[Opcode::JumpNotLe as usize] = h_jump_not_le;
    t[Opcode::JumpEq as usize] = h_jump_eq;
    t[Opcode::JumpNe as usize] = h_jump_ne;
    t[Opcode::JumpEqInt as usize] = h_jump_eq_int;
    t[Opcode::JumpLtInt as usize] = h_jump_lt_int;
    t[Opcode::JumpLeInt as usize] = h_jump_le_int;
    t[Opcode::JumpGtInt as usize] = h_jump_gt_int;
    t[Opcode::JumpGeInt as usize] = h_jump_ge_int;
    t[Opcode::Call as usize] = h_call;
    t[Opcode::TailCall as usize] = h_tail_call;
    t[Opcode::Return as usize] = h_return;
    t[Opcode::Return0 as usize] = h_return0;
    t[Opcode::Return1 as usize] = h_return1;
    t[Opcode::Vararg as usize] = h_cold;
    t[Opcode::VarargTable as usize] = h_cold;
    t[Opcode::VarargGet as usize] = h_cold;
    t[Opcode::VarargLen as usize] = h_vararg_len;
    t[Opcode::VarargGetInt as usize] = h_cold;
    t[Opcode::VarargUnpack as usize] = h_cold;
    t[Opcode::ForPrep as usize] = h_for_prep;
    t[Opcode::ForPrepExclusive as usize] = h_cold;
    t[Opcode::ForLoop as usize] = h_for_loop;
    t[Opcode::GenericCall as usize] = h_generic_call;
    t[Opcode::GenericLoop as usize] = h_generic_loop;
    t[Opcode::Closure as usize] = h_closure;
    t[Opcode::Tbc as usize] = h_cold;
    t[Opcode::Close as usize] = h_cold;
    t[Opcode::NativeEntry as usize] = h_cold;
    t
};

mod quick;
mod raw;
