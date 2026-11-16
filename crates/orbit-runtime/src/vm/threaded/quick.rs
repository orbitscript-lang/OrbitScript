//! SPIKE, second cut: handlers that get the frame's registers as an
//! argument.
//!
//! A handler here is `fn(&mut Vm, w, pc, insn) -> Step`, where `w` is the
//! register stack from the frame's base on. The stack is taken out of the
//! VM while a chain runs ([`Vm::run_quick`] holds it), so the window is a
//! length test on a slice already in registers rather than three loads and
//! a bounds check, and a call hands the callee the tail of the same slice.
//!
//! What a handler can't do with that (a return to the caller's window,
//! anything that needs the whole VM: natives, metamethods, errors,
//! collection, growth of the stack) it doesn't try: it leaves the
//! instruction untouched and returns [`Step::Slow`], and the trampoline runs
//! that one instruction on the first cut's handler, which has the whole VM.
//! So a handler here has no error path at all.

use super::*;
use crate::vm::Frame;
use std::rc::Rc;

type Quick = fn(&mut Vm, &mut [Value], usize, Insn) -> Step;

/// Run the instruction at `pc`.
#[inline(always)]
fn next(vm: &mut Vm, w: &mut [Value], pc: usize) -> Step {
    // Without guaranteed sibling calls (a debug build) every dispatch is a
    // call, so every dispatch uses fuel.
    if cfg!(debug_assertions) {
        vm.rt.fuel = vm.rt.fuel.wrapping_sub(1);
        if vm.rt.fuel == 0 {
            return out_of_fuel(vm, pc);
        }
    }
    match vm.rt.words.get(pc) {
        Some(&i) => QUICK[i.code()](vm, w, pc + 1, i),
        None => bad_pc(),
    }
}

/// [`next`] after a backward jump or into a callee: the points that bound
/// how long a chain runs before the trampoline sees it again.
#[inline(always)]
fn next_counted(vm: &mut Vm, w: &mut [Value], pc: usize) -> Step {
    if !cfg!(debug_assertions) {
        vm.rt.fuel = vm.rt.fuel.wrapping_sub(1);
        if vm.rt.fuel == 0 {
            return out_of_fuel(vm, pc);
        }
    }
    next(vm, w, pc)
}

/// Leave the instruction before `pc` to the handler with the whole VM.
#[inline(never)]
fn slow(vm: &mut Vm, pc: usize) -> Step {
    save_pc(vm, pc - 1);
    // Not a constant: a call whose result the caller replaces with a
    // constant is no longer a call in tail position.
    std::hint::black_box(Step::Slow)
}

macro_rules! qwin {
    ($vm:ident, $w:ident, $pc:ident) => {
        match Window::frame($w) {
            Some(r) => r,
            None => return slow($vm, $pc),
        }
    };
}

macro_rules! q {
    ($(#[$m:meta])* fn $name:ident($vm:ident, $w:ident, $pc:ident, $i:ident) $body:block) => {
        $(#[$m])*
        #[inline(never)]
        fn $name($vm: &mut Vm, $w: &mut [Value], $pc: usize, $i: Insn) -> Step $body
    };
}

impl Vm {
    /// The plain interpreter: run the frame on top until the run is
    /// finished or a hook wants the other loop.
    pub(crate) fn run_quick(&mut self) -> Result<Ran, LuaError> {
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
            loop {
                let f = self.rt.stack.running();
                let (base, pc) = (f.base as usize, f.pc as usize);
                self.rt.fuel = FUEL as u32;
                let mut regs = std::mem::take(&mut self.rt.stack.regs);
                let mut step = quick_from(self, &mut regs, base, pc);
                while step == Step::Returned {
                    // A handler returned to its caller's frame, which
                    // starts below the slice it had.
                    let f = self.rt.stack.running();
                    let (base, pc) = (f.base as usize, f.pc as usize);
                    step = quick_from(self, &mut regs, base, pc);
                }
                self.rt.stack.regs = regs;
                let step = if step == Step::Slow {
                    // One instruction on the handler with the whole VM: no
                    // fuel, so it comes back after it.
                    let f = self.rt.stack.running();
                    let (base, pc) = (f.base as usize, f.pc as usize);
                    let _ = Window::take(&mut self.rt.stack.regs, base);
                    let i = self.rt.cur.words[pc];
                    HANDLERS[i.code()](self, pc + 1, i, with_base(base, 0))
                } else {
                    step
                };
                match step {
                    Step::Continue => {}
                    Step::Slow | Step::Returned => {
                        unreachable!("the first cut's handlers finish their instruction")
                    }
                    Step::Reload => continue 'reload,
                    Step::Done => return Ok(Ran::Done),
                    Step::Switch => return Ok(Ran::Switch),
                    Step::NoMemory => return Ok(Ran::NoMemory),
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

/// Run the frame at `base` from `pc` on the handlers of this file.
#[inline(always)]
fn quick_from(vm: &mut Vm, regs: &mut [Value], base: usize, pc: usize) -> Step {
    if !Rc::ptr_eq(&vm.rt.words, &vm.rt.cur.words) {
        vm.rt.words = vm.rt.cur.words.clone();
    }
    // The slice starts at the register below the frame's: where the
    // function was, and where its first result goes.
    match (regs.get_mut(base.wrapping_sub(1)..), vm.rt.words.get(pc)) {
        (Some(w), Some(&i)) => QUICK[i.code()](vm, w, pc + 1, i),
        _ => Step::Slow,
    }
}

// Loads.

q! {fn q_move(vm, w, pc, i) {
    let (dst, src) = i.r_move();
    let mut r = qwin!(vm, w, pc);
    match r[src] {
        Value::Int(x) => r[dst] = Value::Int(x),
        Value::Float(f) => r[dst] = Value::Float(f),
        Value::Table(t) => r[dst] = Value::Table(t),
        Value::Str(x) => r[dst] = Value::Str(x),
        Value::Closure(c) => r[dst] = Value::Closure(c),
        Value::Bool(b) => r[dst] = Value::Bool(b),
        v => r[dst] = v,
    }
    next(vm, w, pc)
}}

q! {fn q_load_nil(vm, w, pc, i) {
    let (dst, count) = i.r_load_nil();
    let mut r = qwin!(vm, w, pc);
    let at = dst as usize;
    match r.get_mut(at..at + count as usize) {
        Some(s) => s.fill(Value::Nil),
        None => return slow(vm, pc),
    }
    next(vm, w, pc)
}}

q! {fn q_load_bool(vm, w, pc, i) {
    let (dst, value) = i.r_load_bool();
    let mut r = qwin!(vm, w, pc);
    r[dst] = Value::Bool(value);
    next(vm, w, pc)
}}

q! {fn q_load_int(vm, w, pc, i) {
    let (dst, value) = i.r_load_int();
    let mut r = qwin!(vm, w, pc);
    r[dst] = Value::Int(value as i64);
    next(vm, w, pc)
}}

q! {fn q_load_const(vm, w, pc, i) {
    let (dst, k) = i.r_load_const();
    let mut r = qwin!(vm, w, pc);
    match vm.rt.cur.consts.get(k as usize) {
        Some(&v) => r[dst] = v,
        None => return slow(vm, pc),
    }
    next(vm, w, pc)
}}

// Cells and captures.

q! {fn q_cell_get(vm, w, pc, i) {
    let (dst, cell) = i.r_cell_get();
    let Some(&c) = vm.rt.stack.cells.get(cell_base(vm) + cell as usize) else {
        return slow(vm, pc);
    };
    let mut r = qwin!(vm, w, pc);
    r[dst] = vm.heap.get(c).value;
    next(vm, w, pc)
}}

q! {fn q_cap_get(vm, w, pc, i) {
    let (dst, cap) = i.r_cap_get();
    let Some(&c) = vm.heap.get(vm.rt.closure).captures.get(cap as usize) else {
        return slow(vm, pc);
    };
    let mut r = qwin!(vm, w, pc);
    r[dst] = vm.heap.get(c).value;
    next(vm, w, pc)
}}

q! {fn q_cap_self(vm, w, pc, i) {
    let dst = i.r_cap_self();
    let mut r = qwin!(vm, w, pc);
    r[dst] = Value::Closure(vm.rt.closure);
    next(vm, w, pc)
}}

q! {fn q_cap_set(vm, w, pc, i) {
    let (cap, src) = i.r_cap_set();
    let Some(&c) = vm.heap.get(vm.rt.closure).captures.get(cap as usize) else {
        return slow(vm, pc);
    };
    let r = qwin!(vm, w, pc);
    by_parts!(r, src, |v| vm.heap.write_for(c, &v).value = v);
    next(vm, w, pc)
}}

q! {fn q_cell_set(vm, w, pc, i) {
    let (cell, src) = i.r_cell_set();
    let Some(&c) = vm.rt.stack.cells.get(cell_base(vm) + cell as usize) else {
        return slow(vm, pc);
    };
    let r = qwin!(vm, w, pc);
    let v = r[src];
    vm.heap.write_for(c, &v).value = v;
    next(vm, w, pc)
}}

// Globals and tables: the cache hit here, everything else slow.

q! {fn q_get_global(vm, w, pc, i) {
    let (dst, _name, ic) = i.r_get_global();
    let Some(&env_cell) = vm.heap.get(vm.rt.closure).captures.first() else {
        return slow(vm, pc);
    };
    let Value::Table(t) = vm.heap.get(env_cell).value else {
        return slow(vm, pc);
    };
    let Some(cache) = vm.rt.cur.caches.get(ic as usize) else {
        return slow(vm, pc);
    };
    let tab = vm.heap.get(t);
    let mut r = qwin!(vm, w, pc);
    match tab.cached_str_packed(cache.slot.get() as usize, cache.key) {
        Some((tag, p)) if tag != Tag::Nil || tab.meta.is_none() => {
            Value::write_unpacked(&mut r[dst], tag, p)
        }
        _ => return slow(vm, pc),
    }
    next(vm, w, pc)
}}

q! {fn q_get_field(vm, w, pc, i) {
    let (dst, obj, _key, ic) = i.r_get_field();
    let mut r = qwin!(vm, w, pc);
    let &Value::Table(t) = &r[obj] else {
        return slow(vm, pc);
    };
    let Some(cache) = vm.rt.cur.caches.get(ic as usize) else {
        return slow(vm, pc);
    };
    let tab = vm.heap.get(t);
    match tab.cached_str_packed(cache.slot.get() as usize, cache.key) {
        Some((tag, p)) if tag != Tag::Nil || tab.meta.is_none() => {
            Value::write_unpacked(&mut r[dst], tag, p)
        }
        _ => return slow(vm, pc),
    }
    next(vm, w, pc)
}}

q! {fn q_set_field(vm, w, pc, i) {
    let (obj, _key, src, ic) = i.r_set_field();
    let r = qwin!(vm, w, pc);
    let &Value::Table(t) = &r[obj] else {
        return slow(vm, pc);
    };
    let Some(cache) = vm.rt.cur.caches.get(ic as usize) else {
        return slow(vm, pc);
    };
    let slot = cache.slot.get() as usize;
    let tab = vm.heap.get(t);
    if matches!(tab.cached_str_packed(slot, cache.key),
        Some((tag, _)) if tab.meta.is_none() || tag != Tag::Nil)
    {
        by_parts!(r, src, |v| vm.heap.write_for(t, &v).set_slot(slot, v));
    } else {
        return slow(vm, pc);
    }
    next(vm, w, pc)
}}

/// Store a register into the array part of `t`; `false` when the store
/// is not that simple. Out of line and given the register by reference: a
/// value built on the handler's own stack and passed on would cost the
/// handler its jump to the next one.
#[inline(never)]
fn store_reg(heap: &mut crate::object::LuaHeap, t: Gc<crate::object::Table>, k: i64, v: &Value) -> bool {
    matches!(table::store_int(heap, t, k, *v), IntStore::Array)
}

q! {fn q_get_index(vm, w, pc, i) {
    let (dst, obj, key) = i.r_get_index();
    let mut r = qwin!(vm, w, pc);
    if let (&Value::Table(t), &Value::Int(k)) = (&r[obj], &r[key]) {
        if vm.heap.get(t).read_int_present(k, &mut r[dst]) {
            return next(vm, w, pc);
        }
    }
    slow(vm, pc)
}}

q! {fn q_set_index(vm, w, pc, i) {
    let (obj, key, src) = i.r_set_index();
    let r = qwin!(vm, w, pc);
    if let (&Value::Table(t), &Value::Int(k)) = (&r[obj], &r[key]) {
        if store_reg(&mut vm.heap, t, k, &r[src]) {
            return next(vm, w, pc);
        }
    }
    slow(vm, pc)
}}

q! {fn q_get_index_int(vm, w, pc, i) {
    let (dst, obj, key) = i.r_get_index_int();
    let mut r = qwin!(vm, w, pc);
    if let &Value::Table(t) = &r[obj] {
        if vm.heap.get(t).read_int_present(key as i64, &mut r[dst]) {
            return next(vm, w, pc);
        }
    }
    slow(vm, pc)
}}

q! {fn q_set_index_int(vm, w, pc, i) {
    let (obj, key, src) = i.r_set_index_int();
    let r = qwin!(vm, w, pc);
    if let &Value::Table(t) = &r[obj] {
        if store_reg(&mut vm.heap, t, key as i64, &r[src]) {
            return next(vm, w, pc);
        }
    }
    slow(vm, pc)
}}

q! {fn q_self_call(vm, w, pc, i) {
    let (dst, obj, key, ic) = i.r_self_call();
    let mut r = qwin!(vm, w, pc);
    let cur = &vm.rt.cur;
    let (Some(&Value::Str(k)), Value::Table(t)) = (cur.consts.get(key as usize), r[obj]) else {
        return slow(vm, pc);
    };
    match method_lookup(&vm.heap, &vm.rt.metas, cur, ic, k, t) {
        Some((tag, p)) => {
            r[dst + 1] = Value::Table(t);
            Value::write_unpacked(&mut r[dst], tag, p)
        }
        None => return slow(vm, pc),
    }
    next(vm, w, pc)
}}

// Arithmetic. The handlers take the number cases that need no call;
// anything else is slow.

/// `x op y` for two numbers: `$int` on two integers, `$flt` on two floats
/// after converting an integer operand.
macro_rules! num2 {
    ($vm:ident, $pc:ident, $x:expr, $y:expr, |$a:ident, $b:ident| $int:expr, $flt:expr) => {
        match ($x, $y) {
            (&Value::Int($a), &Value::Int($b)) => $int,
            (&Value::Float($a), &Value::Float($b)) => Value::Float($flt),
            (&Value::Int($a), &Value::Float($b)) => {
                let $a = $a as f64;
                Value::Float($flt)
            }
            (&Value::Float($a), &Value::Int($b)) => {
                let $b = $b as f64;
                Value::Float($flt)
            }
            _ => return slow($vm, $pc),
        }
    };
}

macro_rules! q_arith {
    ($name:ident, $read:ident, |$vm:ident, $pc:ident, $r:ident, $p:ident| $x:expr, $y:expr, |$a:ident, $b:ident| $int:expr, $flt:expr) => {
        q! {fn $name($vm, w, $pc, i) {
            let $p = i.$read();
            let mut $r = qwin!($vm, w, $pc);
            let v = num2!($vm, $pc, $x, $y, |$a, $b| $int, $flt);
            match v {
                Value::Int(n) => $r[$p.0] = Value::Int(n),
                Value::Float(f) => $r[$p.0] = Value::Float(f),
                _ => return slow($vm, $pc),
            }
            next($vm, w, $pc)
        }}
    };
}

macro_rules! kk {
    ($vm:ident, $pc:ident, $k:expr) => {
        match $vm.rt.cur.consts.get($k as usize) {
            Some(k) => k,
            None => return slow($vm, $pc),
        }
    };
}

/// Lua's integer `%`, where the result takes the divisor's sign; `Nil`
/// for the divisors the slow path handles.
#[inline(always)]
fn int_mod(a: i64, b: i64) -> Value {
    if b == 0 || b == -1 {
        return Value::Nil;
    }
    let m = a % b;
    Value::Int(if m != 0 && (m ^ b) < 0 { m + b } else { m })
}

q_arith!(q_add, r_add, |vm, pc, r, p| &r[p.1], &r[p.2], |x, y| Value::Int(x.wrapping_add(y)), x + y);
q_arith!(q_sub, r_sub, |vm, pc, r, p| &r[p.1], &r[p.2], |x, y| Value::Int(x.wrapping_sub(y)), x - y);
q_arith!(q_mul, r_mul, |vm, pc, r, p| &r[p.1], &r[p.2], |x, y| Value::Int(x.wrapping_mul(y)), x * y);
q_arith!(q_div, r_div, |vm, pc, r, p| &r[p.1], &r[p.2], |x, y| Value::Float(x as f64 / y as f64), x / y);
q_arith!(q_mod, r_mod, |vm, pc, r, p| &r[p.1], &r[p.2], |x, y| int_mod(x, y), {
    let _ = (x, y);
    return slow(vm, pc)
});
q_arith!(q_mul_k, r_mul_k, |vm, pc, r, p| &r[p.1], kk!(vm, pc, p.2), |x, y| Value::Int(x.wrapping_mul(y)), x * y);
q_arith!(q_mod_k, r_mod_k, |vm, pc, r, p| &r[p.1], kk!(vm, pc, p.2), |x, y| int_mod(x, y), {
    let _ = (x, y);
    return slow(vm, pc)
});
q_arith!(q_k_add, r_k_add, |vm, pc, r, p| kk!(vm, pc, p.1), &r[p.2], |x, y| Value::Int(x.wrapping_add(y)), x + y);
q_arith!(q_k_mul, r_k_mul, |vm, pc, r, p| kk!(vm, pc, p.1), &r[p.2], |x, y| Value::Int(x.wrapping_mul(y)), x * y);
q_arith!(q_k_div, r_k_div, |vm, pc, r, p| kk!(vm, pc, p.1), &r[p.2], |x, y| Value::Float(x as f64 / y as f64), x / y);

q! {fn q_add_int(vm, w, pc, i) {
    let (dst, a, imm) = i.r_add_int();
    let mut r = qwin!(vm, w, pc);
    match r[a] {
        Value::Int(x) => r[dst] = Value::Int(x.wrapping_add(imm as i64)),
        Value::Float(f) => r[dst] = Value::Float(f + imm as f64),
        _ => return slow(vm, pc),
    }
    next(vm, w, pc)
}}

q! {fn q_sub_int(vm, w, pc, i) {
    let (dst, a, imm) = i.r_sub_int();
    let mut r = qwin!(vm, w, pc);
    match r[a] {
        Value::Int(x) => r[dst] = Value::Int(x.wrapping_sub(imm as i64)),
        Value::Float(f) => r[dst] = Value::Float(f + (-(imm as i64)) as f64),
        _ => return slow(vm, pc),
    }
    next(vm, w, pc)
}}

macro_rules! q_bitwise {
    ($name:ident, $read:ident, $op:expr) => {
        q! {fn $name(vm, w, pc, i) {
            let (dst, a, b) = i.$read();
            let mut r = qwin!(vm, w, pc);
            match (&r[a], &r[b]) {
                (&Value::Int(x), &Value::Int(y)) => r[dst] = Value::Int($op.apply(x, y)),
                _ => return slow(vm, pc),
            }
            next(vm, w, pc)
        }}
    };
}

q_bitwise!(q_b_and, r_b_and, BitOp::And);
q_bitwise!(q_b_or, r_b_or, BitOp::Or);
q_bitwise!(q_b_xor, r_b_xor, BitOp::Xor);
q_bitwise!(q_shl, r_shl, BitOp::Shl);
q_bitwise!(q_shr, r_shr, BitOp::Shr);

q! {fn q_not(vm, w, pc, i) {
    let (dst, src) = i.r_not();
    let mut r = qwin!(vm, w, pc);
    r[dst] = Value::Bool(r[src].is_falsy());
    next(vm, w, pc)
}}

q! {fn q_len(vm, w, pc, i) {
    let (dst, src) = i.r_len();
    let mut r = qwin!(vm, w, pc);
    let x = r[src];
    match op_len(&vm.heap, &vm.rt.metas, x) {
        Ok(v) => r[dst] = v,
        Err(_) => return slow(vm, pc),
    }
    next(vm, w, pc)
}}

// Branches.

q! {fn q_jump(vm, w, pc, i) {
    let off = i.r_jump();
    if off < 0 {
        next_counted(vm, w, jump(pc, off))
    } else {
        next(vm, w, jump(pc, off))
    }
}}

q! {fn q_jump_if_true(vm, w, pc, i) {
    let (src, off) = i.r_jump_if_true();
    let r = qwin!(vm, w, pc);
    let pc = if !r[src].is_falsy() { jump(pc, off) } else { pc };
    next(vm, w, pc)
}}

q! {fn q_jump_if_false(vm, w, pc, i) {
    let (src, off) = i.r_jump_if_false();
    let r = qwin!(vm, w, pc);
    let pc = if r[src].is_falsy() { jump(pc, off) } else { pc };
    next(vm, w, pc)
}}

macro_rules! q_cmp_jump {
    ($name:ident, $read:ident, $want:expr, |$x:ident, $y:ident| $test:expr) => {
        q! {fn $name(vm, w, pc, i) {
            let (a, b, off) = i.$read();
            let r = qwin!(vm, w, pc);
            let c = match (&r[a], &r[b]) {
                (&Value::Int($x), &Value::Int($y)) => $test,
                (&Value::Float($x), &Value::Float($y)) => $test,
                _ => return slow(vm, pc),
            };
            let pc = if c == $want { jump(pc, off) } else { pc };
            next(vm, w, pc)
        }}
    };
}

q_cmp_jump!(q_jump_lt, r_jump_lt, true, |x, y| x < y);
q_cmp_jump!(q_jump_not_lt, r_jump_not_lt, false, |x, y| x < y);
q_cmp_jump!(q_jump_le, r_jump_le, true, |x, y| x <= y);
q_cmp_jump!(q_jump_not_le, r_jump_not_le, false, |x, y| x <= y);

macro_rules! q_eq_jump {
    ($name:ident, $read:ident, $want:expr) => {
        q! {fn $name(vm, w, pc, i) {
            let (a, b, off) = i.$read();
            let r = qwin!(vm, w, pc);
            let c = match (&r[a], &r[b]) {
                (&Value::Int(x), &Value::Int(y)) => x == y,
                (&Value::Float(x), &Value::Float(y)) => x == y,
                (&Value::Bool(x), &Value::Bool(y)) => x == y,
                (&Value::Nil, &Value::Nil) => true,
                (&Value::Nil, _) | (_, &Value::Nil) => false,
                _ => return slow(vm, pc),
            };
            let pc = if c == $want { jump(pc, off) } else { pc };
            next(vm, w, pc)
        }}
    };
}

q_eq_jump!(q_jump_eq, r_jump_eq, true);
q_eq_jump!(q_jump_ne, r_jump_ne, false);

q! {fn q_jump_eq_int(vm, w, pc, i) {
    let (a, imm, want, off) = i.r_jump_eq_int();
    let r = qwin!(vm, w, pc);
    let c = match r[a] {
        Value::Int(x) => x == imm as i64,
        Value::Float(f) => f == imm as f64,
        _ => false,
    };
    let pc = if c == want { jump(pc, off) } else { pc };
    next(vm, w, pc)
}}

macro_rules! q_cmp_int_jump {
    ($name:ident, $read:ident, |$x:ident, $y:ident| $test:expr) => {
        q! {fn $name(vm, w, pc, i) {
            let (a, imm, want, off) = i.$read();
            let r = qwin!(vm, w, pc);
            let c = match r[a] {
                Value::Int($x) => {
                    let $y = imm as i64;
                    $test
                }
                Value::Float($x) => {
                    let $y = imm as f64;
                    $test
                }
                _ => return slow(vm, pc),
            };
            let pc = if c == want { jump(pc, off) } else { pc };
            next(vm, w, pc)
        }}
    };
}

q_cmp_int_jump!(q_jump_lt_int, r_jump_lt_int, |x, y| x < y);
q_cmp_int_jump!(q_jump_le_int, r_jump_le_int, |x, y| x <= y);
q_cmp_int_jump!(q_jump_gt_int, r_jump_gt_int, |x, y| x > y);
q_cmp_int_jump!(q_jump_ge_int, r_jump_ge_int, |x, y| x >= y);

// Calls. A Lua function called with its parameters covered gets the tail
// of this frame's slice; a return of one value writes it below the frame
// and leaves the caller's frame to the trampoline. Anything else is slow.

q! {fn q_call(vm, w, pc, i) {
    let (func, nargs, nresults) = i.r_call();
    let r = qwin!(vm, w, pc);
    let Value::Closure(c) = r[func] else {
        return slow(vm, pc);
    };
    let same = c == vm.rt.closure;
    let shape = if same {
        Shape::of(&vm.rt.cur)
    } else {
        Shape::of(&vm.heap.get(c).code)
    };
    let frames = &mut vm.rt.stack.frames;
    let room = frames.len() < frames.capacity();
    let Some(caller) = frames.last_mut() else {
        return slow(vm, pc);
    };
    let fa = caller.base as usize + func as usize;
    // The cases with something to grow or fill are the slow handler's.
    if nargs == MULTI
        || nargs < shape.nparams
        || shape.is_vararg
        || shape.ncells > 0
        || !room
        || fa + 1 + (shape.nregs as usize).max(nargs as usize) > vm.rt.host.max_stack()
    {
        return slow(vm, pc);
    }
    caller.pc = pc as u32;
    let cell_base = vm.rt.stack.cells.len() as u32;
    frames.push(Frame {
        kind: FrameKind::Lua {
            closure: c,
            code_id: shape.code_id,
        },
        func: fa as u32,
        base: fa as u32 + 1,
        pc: 0,
        cell_base,
        nvarargs: 0,
        want: nresults,
        tail: false,
        extra: 0,
        cdepth: 0,
    });
    if shape.code_id != Rc::as_ptr(&vm.rt.cur) as usize {
        return enter_other(vm, w, pc, i);
    }
    vm.rt.closure = c;
    next_counted(vm, &mut w[func as usize + 1..], 0)
}}

/// The rest of a call of a function with other code than the caller's.
#[inline(never)]
fn enter_other(vm: &mut Vm, w: &mut [Value], _pc: usize, i: Insn) -> Step {
    let (func, _, _) = i.r_call();
    let FrameKind::Lua { closure, code_id } = vm.rt.stack.running().kind else {
        unreachable!("the frame just pushed")
    };
    if switch_code(vm, closure, code_id) {
        return Step::Reload;
    }
    vm.rt.words = vm.rt.cur.words.clone();
    next_counted(vm, &mut w[func as usize + 1..], 0)
}

q! {fn q_return1(vm, w, pc, i) {
    let src = i.r_return1();
    let [.., caller, f] = &vm.rt.stack.frames[..] else {
        return slow(vm, pc);
    };
    let FrameKind::Lua { closure, code_id } = caller.kind else {
        return slow(vm, pc);
    };
    // Only a frame whose function sits just below its registers, with
    // nothing of its own to release.
    if f.want != 1 || f.base != f.func + 1 || vm.rt.stack.cells.len() != f.cell_base as usize {
        return slow(vm, pc);
    }
    let top = f.base as usize;
    let Some((result, rest)) = w.split_first_mut() else {
        return slow(vm, pc);
    };
    let Some(r) = rest.get(..WINDOW) else {
        return slow(vm, pc);
    };
    match r[src as usize] {
        Value::Int(x) => *result = Value::Int(x),
        Value::Float(x) => *result = Value::Float(x),
        Value::Bool(x) => *result = Value::Bool(x),
        v => *result = v,
    }
    vm.rt.top = top;
    vm.rt.stack.frames.pop();
    if code_id != Rc::as_ptr(&vm.rt.cur) as usize {
        return return_other(vm, closure, code_id);
    }
    vm.rt.closure = closure;
    Step::Returned
}}

/// The rest of a return to a function with other code than the callee's.
#[inline(never)]
fn return_other(vm: &mut Vm, closure: Gc<Closure>, code_id: usize) -> Step {
    if switch_code(vm, closure, code_id) {
        return Step::Reload;
    }
    Step::Returned
}

// Loops.

q! {fn q_for_prep(vm, w, pc, i) {
    let (b, exit) = i.r_for_prep();
    let mut r = qwin!(vm, w, pc);
    match (&r[b], &r[b + 1], &r[b + 2]) {
        (&Value::Int(x), &Value::Int(lim), &Value::Int(s)) if s > 0 => {
            if x <= lim {
                let count = lim.abs_diff(x) / s as u64;
                r[b + 1] = Value::Int(count as i64);
                r[b + 3] = Value::Int(x);
                next(vm, w, pc)
            } else {
                next(vm, w, jump(pc, exit))
            }
        }
        _ => slow(vm, pc),
    }
}}

q! {fn q_for_loop(vm, w, pc, i) {
    let (b, back) = i.r_for_loop();
    let mut r = qwin!(vm, w, pc);
    let at = b as usize;
    let Some(s) = r.get_mut(at..at + 4) else {
        return slow(vm, pc);
    };
    match s {
        [Value::Int(cur), Value::Int(count), Value::Int(s), var] => {
            if *count > 0 {
                *count -= 1;
                *cur = cur.wrapping_add(*s);
                *var = Value::Int(*cur);
                return next_counted(vm, w, jump(pc, back));
            }
        }
        [Value::Float(cur), Value::Float(lim), Value::Float(s), var] => {
            let n = *cur + *s;
            if if *s > 0.0 { n <= *lim } else { *lim <= n } {
                *cur = n;
                *var = Value::Float(n);
                return next_counted(vm, w, jump(pc, back));
            }
        }
        _ => return slow(vm, pc),
    }
    next(vm, w, pc)
}}

q! {fn q_generic_loop(vm, w, pc, i) {
    let (b, back) = i.r_generic_loop();
    let mut r = qwin!(vm, w, pc);
    let v = r[b + 4];
    if !v.is_nil() {
        r[b + 2] = v;
        return next_counted(vm, w, jump(pc, back));
    }
    next(vm, w, pc)
}}

/// Every instruction without a handler here.
fn q_slow(vm: &mut Vm, _w: &mut [Value], pc: usize, _i: Insn) -> Step {
    slow(vm, pc)
}

static QUICK: [Quick; 256] = {
    let mut t: [Quick; 256] = [q_slow; 256];
    t[Opcode::Move as usize] = q_move;
    t[Opcode::LoadNil as usize] = q_load_nil;
    t[Opcode::LoadBool as usize] = q_load_bool;
    t[Opcode::LoadInt as usize] = q_load_int;
    t[Opcode::LoadConst as usize] = q_load_const;
    t[Opcode::CellGet as usize] = q_cell_get;
    t[Opcode::CellSet as usize] = q_cell_set;
    t[Opcode::CapGet as usize] = q_cap_get;
    t[Opcode::CapSet as usize] = q_cap_set;
    t[Opcode::CapSelf as usize] = q_cap_self;
    t[Opcode::GetGlobal as usize] = q_get_global;
    t[Opcode::GetField as usize] = q_get_field;
    t[Opcode::SetField as usize] = q_set_field;
    t[Opcode::GetIndex as usize] = q_get_index;
    t[Opcode::SetIndex as usize] = q_set_index;
    t[Opcode::GetIndexInt as usize] = q_get_index_int;
    t[Opcode::SetIndexInt as usize] = q_set_index_int;
    t[Opcode::SelfCall as usize] = q_self_call;
    t[Opcode::Add as usize] = q_add;
    t[Opcode::Sub as usize] = q_sub;
    t[Opcode::Mul as usize] = q_mul;
    t[Opcode::Div as usize] = q_div;
    t[Opcode::Mod as usize] = q_mod;
    t[Opcode::AddInt as usize] = q_add_int;
    t[Opcode::SubInt as usize] = q_sub_int;
    t[Opcode::MulK as usize] = q_mul_k;
    t[Opcode::ModK as usize] = q_mod_k;
    t[Opcode::KAdd as usize] = q_k_add;
    t[Opcode::KMul as usize] = q_k_mul;
    t[Opcode::KDiv as usize] = q_k_div;
    t[Opcode::BAnd as usize] = q_b_and;
    t[Opcode::BOr as usize] = q_b_or;
    t[Opcode::BXor as usize] = q_b_xor;
    t[Opcode::Shl as usize] = q_shl;
    t[Opcode::Shr as usize] = q_shr;
    t[Opcode::Not as usize] = q_not;
    t[Opcode::Len as usize] = q_len;
    t[Opcode::Jump as usize] = q_jump;
    t[Opcode::JumpIfTrue as usize] = q_jump_if_true;
    t[Opcode::JumpIfFalse as usize] = q_jump_if_false;
    t[Opcode::JumpLt as usize] = q_jump_lt;
    t[Opcode::JumpNotLt as usize] = q_jump_not_lt;
    t[Opcode::JumpLe as usize] = q_jump_le;
    t[Opcode::JumpNotLe as usize] = q_jump_not_le;
    t[Opcode::JumpEq as usize] = q_jump_eq;
    t[Opcode::JumpNe as usize] = q_jump_ne;
    t[Opcode::JumpEqInt as usize] = q_jump_eq_int;
    t[Opcode::JumpLtInt as usize] = q_jump_lt_int;
    t[Opcode::JumpLeInt as usize] = q_jump_le_int;
    t[Opcode::JumpGtInt as usize] = q_jump_gt_int;
    t[Opcode::JumpGeInt as usize] = q_jump_ge_int;
    t[Opcode::Call as usize] = q_call;
    t[Opcode::Return1 as usize] = q_return1;
    t[Opcode::ForPrep as usize] = q_for_prep;
    t[Opcode::ForLoop as usize] = q_for_loop;
    t[Opcode::GenericLoop as usize] = q_generic_loop;
    t
};
