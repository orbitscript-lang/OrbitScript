//! The plain interpreter's loop: handlers chained by jumps, with the
//! registers and the code as raw pointers (ADR-15).
//!
//! A handler is `fn(&mut Vm, w, ip, insn) -> Step`. It does its
//! instruction's common case and ends by calling the next instruction's
//! handler in tail position, which the compiler turns into a jump. `w`
//! points at the register below the running frame's base (the slot its
//! function sits in, and where a single result goes), `ip` at the next
//! instruction. There is no window test and no bounds test on the code:
//! the trampoline and the call handler make sure the stack covers a
//! frame's window before running it, and verified code never runs off
//! its end.
//!
//! What a handler does not cover it leaves to [`slow`], which runs the
//! instruction on the whole-VM handler of the parent module and comes
//! back here.
//!
//! Three rules keep a handler's hot path free of saved registers, and
//! `tools/check-handlers.py` lists the handlers of a build that have a
//! prologue or a real call, from its disassembly:
//! - a handler returns one scalar, and never a constant from a callee
//!   the compiler can see through ([`slow`]'s results come from a call);
//! - anything that needs a real call (growing a vector, a write barrier,
//!   a lookup that misses) lives in an `#[inline(never)]` function the
//!   handler calls in tail position;
//! - no value built on the handler's own stack is passed on by address.
//!
//! Nothing depends on the jumps for correctness. A chain spends fuel on
//! every backward jump and call (on every instruction in a debug build),
//! and returns to the trampoline when it runs out.
#![allow(unsafe_code)]

use super::*;
use crate::object::ProtoCode;
use crate::vm::Frame;
use std::rc::Rc;

type Quick = fn(&mut Vm, *mut Value, *const Insn, Insn) -> Step;

/// `*dst = *src`, as the tag byte, the byte a boolean lives in and the
/// payload word: no branch on the tag, and each load matches the size of
/// the store that last wrote it.
#[inline(always)]
fn copy_value(dst: &mut Value, src: &Value) {
    use std::mem::MaybeUninit;
    let s = src as *const Value as *const MaybeUninit<u8>;
    let d = dst as *mut Value as *mut MaybeUninit<u8>;
    // SAFETY: `Value` is `repr(C, u8)`, 16 bytes: the tag at 0, a `bool`
    // at 1, every other payload at 8. Bytes are copied as possibly
    // uninitialised, so padding is never read as a value.
    unsafe {
        *d = *s;
        *d.add(1) = *s.add(1);
        *(d.add(8) as *mut MaybeUninit<u64>) = *(s.add(8) as *const MaybeUninit<u64>);
    }
}

/// Write the value with this tag and payload, without a branch on the tag.
/// `*dst` = the value with tag byte `tag` and payload `p`.
#[inline(always)]
fn write_raw(dst: &mut Value, tag: u8, p: crate::object::value::Payload) {
    let d = dst as *mut Value as *mut u8;
    // SAFETY: as `write_packed`; `tag` is the tag byte of a stored value.
    unsafe {
        *d = tag;
        *d.add(1) = (p.bits() != 0) as u8;
        *(d.add(8) as *mut u64) = p.bits();
    }
}

#[inline(always)]
fn write_packed(dst: &mut Value, tag: Tag, p: crate::object::value::Payload) {
    let d = dst as *mut Value as *mut u8;
    // SAFETY: as `copy_value`; a payload's bits are the variant's field,
    // and a boolean's are 0 or 1.
    unsafe {
        *d = tag as u8;
        *d.add(1) = (p.bits() != 0) as u8;
        *(d.add(8) as *mut u64) = p.bits();
    }
}

/// The index of the instruction `ip` points at.
#[inline(always)]
fn pc_of(vm: &Vm, ip: *const Insn) -> usize {
    // SAFETY: `ip` points into the running code, which starts at `wp`.
    unsafe { ip.offset_from(vm.rt.wp) as usize }
}

#[inline(always)]
fn jump(ip: *const Insn, off: i32) -> *const Insn {
    // SAFETY: verified code only jumps inside itself.
    unsafe { ip.offset(off as isize) }
}

/// Run the instruction at `ip`.
#[inline(always)]
fn next(vm: &mut Vm, w: *mut Value, ip: *const Insn) -> Step {
    if cfg!(debug_assertions) {
        vm.rt.fuel = vm.rt.fuel.wrapping_sub(1);
        if vm.rt.fuel == 0 {
            return pause(vm, ip);
        }
    }
    // SAFETY: `ip` is inside the running code, which ends in a return.
    let i = unsafe { *ip };
    QUICK[i.code()](vm, w, unsafe { ip.add(1) }, i)
}

#[inline(always)]
fn next_counted(vm: &mut Vm, w: *mut Value, ip: *const Insn) -> Step {
    if !cfg!(debug_assertions) {
        vm.rt.fuel = vm.rt.fuel.wrapping_sub(1);
        if vm.rt.fuel == 0 {
            return pause(vm, ip);
        }
    }
    next(vm, w, ip)
}

/// Run the instruction at `ip`, the target of a backward jump: the place
/// where a requested interrupt is noticed, so that it is raised in the
/// function that loops.
#[inline(always)]
fn next_back(vm: &mut Vm, w: *mut Value, ip: *const Insn) -> Step {
    #[cfg(feature = "hooks")]
    if vm.rt.host.interrupt_requested() {
        return interrupted(vm, ip);
    }
    next_counted(vm, w, ip)
}

/// Leave to the hooked loop, which raises the interrupt before the
/// instruction at `ip`.
#[cfg(feature = "hooks")]
#[inline(never)]
fn interrupted(vm: &mut Vm, ip: *const Insn) -> Step {
    let pc = pc_of(vm, ip);
    sync(vm);
    save_pc(vm, pc);
    arm_interrupt(vm)
}

/// The end of a conditional jump by `off`.
macro_rules! branch {
    ($vm:ident, $w:ident, $pc:ident, $off:ident, $taken:expr) => {
        if $taken {
            if $off < 0 {
                return next_back($vm, $w, jump($pc, $off));
            }
            next($vm, $w, jump($pc, $off))
        } else {
            next($vm, $w, $pc)
        }
    };
}

#[inline(never)]
fn pause(vm: &mut Vm, ip: *const Insn) -> Step {
    let pc = pc_of(vm, ip);
    sync(vm);
    out_of_fuel(vm, pc)
}

/// The running code as the handlers of this file see it.
macro_rules! code {
    ($vm:ident) => {
        // SAFETY: the code of the running closure, which its frame keeps
        // alive.
        unsafe { &*$vm.rt.code }
    };
}

/// Make the VM's own idea of the running code match this file's, which
/// calls and returns here change without it.
#[inline(always)]
fn sync(vm: &mut Vm) {
    if vm.rt.code != Rc::as_ptr(&vm.rt.cur) {
        sync_cold(vm);
    }
}

#[inline(never)]
fn sync_cold(vm: &mut Vm) {
    let (closure, code_id) = (vm.rt.closure, vm.rt.code as usize);
    let _ = switch_code(vm, closure, code_id);
}

/// Whether moving to other code has to go through the VM's own handlers.
#[inline(always)]
fn watched(_vm: &Vm) -> bool {
    #[cfg(feature = "debugger")]
    return _vm.rt.debug.watch;
    #[cfg(not(feature = "debugger"))]
    false
}

/// Run the instruction before `ip` on the handler with the whole VM, then
/// go on from wherever that leaves the running frame.
#[inline(never)]
fn slow(vm: &mut Vm, ip: *const Insn) -> Step {
    let pc = pc_of(vm, ip) - 1;
    // SAFETY: the instruction whose handler called this.
    let i = unsafe { *ip.sub(1) };
    sync(vm);
    let base = vm.rt.stack.running().base as usize;
    // No fuel, so the handler comes back after its instruction.
    #[cfg(feature = "hooks")]
    let depth = vm.rt.stack.depth();
    let step = HANDLERS[i.code()](vm, pc + 1, i, with_base(base, 0));
    if step != Step::Continue {
        return step;
    }
    let f = vm.rt.stack.running();
    let (base, to) = (f.base as usize, f.pc as usize);
    // The instruction jumped backward in its frame: see `next_back`.
    #[cfg(feature = "hooks")]
    if to <= pc && vm.rt.stack.depth() == depth && vm.rt.host.interrupt_requested() {
        return arm_interrupt(vm);
    }
    let pc = to;
    vm.rt.fuel = vm.rt.fuel.wrapping_sub(1);
    if vm.rt.fuel == 0 {
        return Step::Continue;
    }
    quick_from(vm, base, pc)
}

macro_rules! qwin {
    ($vm:ident, $w:ident, $pc:ident) => {
        // SAFETY: the stack covers the window of a running frame, which
        // starts one register above `w`.
        Window::of(unsafe { &mut *($w.add(1) as *mut [Value; WINDOW]) })
    };
}

macro_rules! q {
    ($(#[$m:meta])* fn $name:ident($vm:ident, $w:ident, $pc:ident, $i:ident) $body:block) => {
        $(#[$m])*
        #[inline(never)]
        fn $name($vm: &mut Vm, $w: *mut Value, $pc: *const Insn, $i: Insn) -> Step $body
    };
}

impl Vm {
    /// The plain interpreter: run the frame on top until the run is
    /// finished or a hook wants the other loop.
    pub(crate) fn run_raw(&mut self) -> Result<Ran, LuaError> {
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
                let step = quick_from(self, base, pc);
                match step {
                    Step::Continue => {}
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
fn quick_from(vm: &mut Vm, base: usize, pc: usize) -> Step {
    vm.rt.code = Rc::as_ptr(&vm.rt.cur);
    vm.rt.wp = vm.rt.cur.words.as_ptr();
    let _ = Window::take(&mut vm.rt.stack.regs, base);
    let regs = &mut vm.rt.stack.regs;
    vm.rt.stack_len = regs.len();
    assert!(base >= 1 && pc < vm.rt.cur.words.len());
    // SAFETY: the stack covers the window at `base` (sized above), and
    // `pc` is inside the code (checked above). Nothing a handler of this
    // file calls moves the stack; `slow` comes back through here.
    let w = unsafe { regs.as_mut_ptr().add(base - 1) };
    let ip = unsafe { vm.rt.wp.add(pc) };
    next(vm, w, ip)
}

// Loads.

q! {fn q_move(vm, w, pc, i) {
    let (dst, src) = i.r_move();
    let mut r = qwin!(vm, w, pc);
    let v: *const Value = &r[src];
    // SAFETY: a register of the window; `dst` may be the same one.
    copy_value(&mut r[dst], unsafe { &*v });
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
    match code!(vm).consts.get(k as usize) {
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
    let Some(cache) = code!(vm).caches.get(ic as usize) else {
        return slow(vm, pc);
    };
    let tab = vm.heap.get(t);
    let mut r = qwin!(vm, w, pc);
    match tab.cached_str_packed(cache.slot.get() as usize, cache.key) {
        Some((tag, p)) if tag != Tag::Nil || tab.meta.is_none() => {
            write_packed(&mut r[dst], tag, p)
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
    let Some(cache) = code!(vm).caches.get(ic as usize) else {
        return slow(vm, pc);
    };
    let tab = vm.heap.get(t);
    match tab.cached_str_packed(cache.slot.get() as usize, cache.key) {
        Some((tag, p)) if tag != Tag::Nil || tab.meta.is_none() => {
            write_packed(&mut r[dst], tag, p)
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
    let Some(cache) = code!(vm).caches.get(ic as usize) else {
        return slow(vm, pc);
    };
    let slot = cache.slot.get() as usize;
    let tab = vm.heap.get(t);
    if !matches!(tab.cached_str_packed(slot, cache.key),
        Some((tag, _)) if tab.meta.is_none() || tag != Tag::Nil)
    {
        return slow(vm, pc);
    }
    // A number needs no barrier, and so no call; anything else goes on in
    // a function that can afford one.
    let v = match r[src] {
        Value::Int(n) => Value::Int(n),
        Value::Float(f) => Value::Float(f),
        _ => return set_field_handle(vm, w, pc, i),
    };
    vm.heap.write_for(t, &v).set_slot(slot, v);
    next(vm, w, pc)
}}

/// The store of [`q_set_field`], cache hit already known, for a value that
/// may need the write barrier.
#[inline(never)]
fn set_field_handle(vm: &mut Vm, w: *mut Value, pc: *const Insn, i: Insn) -> Step {
    let (obj, _key, src, ic) = i.r_set_field();
    let r = qwin!(vm, w, pc);
    let (&Value::Table(t), Some(cache)) = (&r[obj], code!(vm).caches.get(ic as usize)) else {
        return slow(vm, pc);
    };
    let slot = cache.slot.get() as usize;
    by_parts!(r, src, |v| vm.heap.write_for(t, &v).set_slot(slot, v));
    next(vm, w, pc)
}

/// Store a register into the array part of `t`; `false` when the store
/// is not that simple. Out of line and given the register by reference: a
/// value built on the handler's own stack and passed on would cost the
/// handler its jump to the next one.
#[inline(never)]
fn store_reg(
    heap: &mut crate::object::LuaHeap,
    t: Gc<crate::object::Table>,
    k: i64,
    v: &Value,
) -> bool {
    matches!(table::store_int(heap, t, k, *v), IntStore::Array)
}

q! {fn q_get_index(vm, w, pc, i) {
    let (dst, obj, key) = i.r_get_index();
    let mut r = qwin!(vm, w, pc);
    if let (&Value::Table(t), &Value::Int(k)) = (&r[obj], &r[key]) {
        if vm.heap.get(t).read_int_raw(k).map(|(b, p)| write_raw(&mut r[dst], b, p)).is_some() {
            return next(vm, w, pc);
        }
    }
    slow(vm, pc)
}}

q! {fn q_set_index(vm, w, pc, i) {
    let (obj, key, src) = i.r_set_index();
    let r = qwin!(vm, w, pc);
    if let (&Value::Table(t), &Value::Int(k)) = (&r[obj], &r[key]) {
        // A number or a boolean into the array part needs no barrier and
        // so no call.
        let v = match r[src] {
            Value::Int(n) => Value::Int(n),
            Value::Float(f) => Value::Float(f),
            Value::Bool(b) => Value::Bool(b),
            _ => return set_index_other(vm, w, pc, i),
        };
        if table::store_in_array(&mut vm.heap, t, k, v) {
            return next(vm, w, pc);
        }
    }
    set_index_other(vm, w, pc, i)
}}

#[inline(never)]
fn set_index_other(vm: &mut Vm, w: *mut Value, pc: *const Insn, i: Insn) -> Step {
    let (obj, key, src) = i.r_set_index();
    let r = qwin!(vm, w, pc);
    if let (&Value::Table(t), &Value::Int(k)) = (&r[obj], &r[key]) {
        if store_reg(&mut vm.heap, t, k, &r[src]) {
            return next(vm, w, pc);
        }
    }
    slow(vm, pc)
}

q! {fn q_get_index_int(vm, w, pc, i) {
    let (dst, obj, key) = i.r_get_index_int();
    let mut r = qwin!(vm, w, pc);
    if let &Value::Table(t) = &r[obj] {
        if vm.heap.get(t).read_int_raw(key as i64).map(|(b, p)| write_raw(&mut r[dst], b, p)).is_some() {
            return next(vm, w, pc);
        }
    }
    slow(vm, pc)
}}

q! {fn q_set_index_int(vm, w, pc, i) {
    let (obj, key, src) = i.r_set_index_int();
    let r = qwin!(vm, w, pc);
    if let (&Value::Table(t), k) = (&r[obj], key as i64) {
        // A number or a boolean into the array part needs no barrier and
        // so no call.
        let v = match r[src] {
            Value::Int(n) => Value::Int(n),
            Value::Float(f) => Value::Float(f),
            Value::Bool(b) => Value::Bool(b),
            _ => return set_index_int_other(vm, w, pc, i),
        };
        if table::store_in_array(&mut vm.heap, t, k, v) {
            return next(vm, w, pc);
        }
    }
    set_index_int_other(vm, w, pc, i)
}}

#[inline(never)]
fn set_index_int_other(vm: &mut Vm, w: *mut Value, pc: *const Insn, i: Insn) -> Step {
    let (obj, key, src) = i.r_set_index_int();
    let r = qwin!(vm, w, pc);
    if let (&Value::Table(t), k) = (&r[obj], key as i64) {
        if store_reg(&mut vm.heap, t, k, &r[src]) {
            return next(vm, w, pc);
        }
    }
    slow(vm, pc)
}

q! {fn q_self_call(vm, w, pc, i) {
    let (dst, obj, _key, ic) = i.r_self_call();
    let mut r = qwin!(vm, w, pc);
    let cur = code!(vm);
    let (Value::Table(t), Some([cm, ci])) = (r[obj], cur.caches.get(ic as usize..ic as usize + 2)) else {
        return self_call_miss(vm, w, pc, i);
    };
    // The two cache hits: the method in the object itself, or in the
    // table its metatable's `__index` names.
    let heap = &vm.heap;
    let tab = heap.get(t);
    let slot = cm.slot.get() as usize;
    let m = match tab.cached_str_packed(slot, cm.key) {
        Some(m) if m.0 != Tag::Nil => m,
        Some(_) => return self_call_miss(vm, w, pc, i),
        None => {
            let Some(mt) = tab.meta else {
                return self_call_miss(vm, w, pc, i);
            };
            let Some(c) = crate::vm::dispatch::cached_table(heap.get(mt), ci) else {
                return self_call_miss(vm, w, pc, i);
            };
            match heap.get(c).cached_str_packed(slot, cm.key) {
                Some(m) if m.0 != Tag::Nil => m,
                _ => return self_call_miss(vm, w, pc, i),
            }
        }
    };
    r[dst + 1] = Value::Table(t);
    write_packed(&mut r[dst], m.0, m.1);
    next(vm, w, pc)
}}

/// A method call whose caches miss: the whole lookup.
#[inline(never)]
fn self_call_miss(vm: &mut Vm, w: *mut Value, pc: *const Insn, i: Insn) -> Step {
    let (dst, obj, key, ic) = i.r_self_call();
    let mut r = qwin!(vm, w, pc);
    let cur = code!(vm);
    let (Some(&Value::Str(k)), Value::Table(t)) = (cur.consts.get(key as usize), r[obj]) else {
        return slow(vm, pc);
    };
    match method_lookup(&vm.heap, &vm.rt.metas, cur, ic, k, t) {
        Some((tag, p)) => {
            r[dst + 1] = Value::Table(t);
            write_packed(&mut r[dst], tag, p)
        }
        None => return slow(vm, pc),
    }
    next(vm, w, pc)
}

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
        match code!($vm).consts.get($k as usize) {
            Some(k) => k,
            None => return slow($vm, $pc),
        }
    };
}

/// Lua's integer `%`, where the result takes the divisor's sign; `None`
/// for the divisors the slow path handles.
#[inline(always)]
fn int_mod(a: i64, b: i64) -> Option<i64> {
    if b == 0 || b == -1 {
        return None;
    }
    let m = a % b;
    Some(if m != 0 && (m ^ b) < 0 { m + b } else { m })
}

/// `%` on two integers; floats are slow.
macro_rules! q_int_mod {
    ($name:ident, $read:ident, |$vm:ident, $pc:ident, $r:ident, $p:ident| $x:expr, $y:expr) => {
        q! {fn $name($vm, w, $pc, i) {
            let $p = i.$read();
            let mut $r = qwin!($vm, w, $pc);
            let (&Value::Int(a), &Value::Int(b)) = ($x, $y) else {
                return slow($vm, $pc);
            };
            match int_mod(a, b) {
                Some(n) => $r[$p.0] = Value::Int(n),
                None => return slow($vm, $pc),
            }
            next($vm, w, $pc)
        }}
    };
}

q_arith!(
    q_add,
    r_add,
    |vm, pc, r, p| &r[p.1],
    &r[p.2],
    |x, y| Value::Int(x.wrapping_add(y)),
    x + y
);
q_arith!(
    q_sub,
    r_sub,
    |vm, pc, r, p| &r[p.1],
    &r[p.2],
    |x, y| Value::Int(x.wrapping_sub(y)),
    x - y
);
q_arith!(
    q_mul,
    r_mul,
    |vm, pc, r, p| &r[p.1],
    &r[p.2],
    |x, y| Value::Int(x.wrapping_mul(y)),
    x * y
);
q_arith!(
    q_div,
    r_div,
    |vm, pc, r, p| &r[p.1],
    &r[p.2],
    |x, y| Value::Float(x as f64 / y as f64),
    x / y
);
q_int_mod!(q_mod, r_mod, |vm, pc, r, p| &r[p.1], &r[p.2]);
q_arith!(
    q_mul_k,
    r_mul_k,
    |vm, pc, r, p| &r[p.1],
    kk!(vm, pc, p.2),
    |x, y| Value::Int(x.wrapping_mul(y)),
    x * y
);
q_int_mod!(q_mod_k, r_mod_k, |vm, pc, r, p| &r[p.1], kk!(vm, pc, p.2));
q_arith!(
    q_k_add,
    r_k_add,
    |vm, pc, r, p| kk!(vm, pc, p.1),
    &r[p.2],
    |x, y| Value::Int(x.wrapping_add(y)),
    x + y
);
q_arith!(
    q_k_mul,
    r_k_mul,
    |vm, pc, r, p| kk!(vm, pc, p.1),
    &r[p.2],
    |x, y| Value::Int(x.wrapping_mul(y)),
    x * y
);
q_arith!(
    q_k_div,
    r_k_div,
    |vm, pc, r, p| kk!(vm, pc, p.1),
    &r[p.2],
    |x, y| Value::Float(x as f64 / y as f64),
    x / y
);

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
        next_back(vm, w, jump(pc, off))
    } else {
        next(vm, w, jump(pc, off))
    }
}}

q! {fn q_jump_if_true(vm, w, pc, i) {
    let (src, off) = i.r_jump_if_true();
    let r = qwin!(vm, w, pc);
    branch!(vm, w, pc, off, !r[src].is_falsy())
}}

q! {fn q_jump_if_false(vm, w, pc, i) {
    let (src, off) = i.r_jump_if_false();
    let r = qwin!(vm, w, pc);
    branch!(vm, w, pc, off, r[src].is_falsy())
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
            branch!(vm, w, pc, off, c == $want)
        }}
    };
}

q_cmp_jump!(q_jump_lt, r_jump_lt, true, |x, y| x < y);
q_cmp_jump!(q_jump_not_lt, r_jump_not_lt, false, |x, y| x < y);
q_cmp_jump!(q_jump_le, r_jump_le, true, |x, y| x <= y);
q_cmp_jump!(q_jump_not_le, r_jump_not_le, false, |x, y| x <= y);

macro_rules! q_eq_jump {
    ($name:ident, $other:ident, $read:ident, $want:expr) => {
        q! {fn $name(vm, w, pc, i) {
            let (a, b, off) = i.$read();
            let r = qwin!(vm, w, pc);
            let c = match (&r[a], &r[b]) {
                (&Value::Int(x), &Value::Int(y)) => x == y,
                (&Value::Float(x), &Value::Float(y)) => x == y,
                (&Value::Bool(x), &Value::Bool(y)) => x == y,
                (&Value::Nil, &Value::Nil) => true,
                (&Value::Nil, _) | (_, &Value::Nil) => false,
                // The same string, table or function.
                (&Value::Str(x), &Value::Str(y)) if x == y => true,
                (&Value::Table(x), &Value::Table(y)) if x == y => true,
                _ => return $other(vm, w, pc, i),
            };
            branch!(vm, w, pc, off, c == $want)
        }}

        /// The comparisons that may need a call: long strings, mixed
        /// numbers, `__eq`.
        #[inline(never)]
        fn $other(vm: &mut Vm, w: *mut Value, pc: *const Insn, i: Insn) -> Step {
            let (a, b, off) = i.$read();
            let r = qwin!(vm, w, pc);
            match fast::equal(&r, a, b, &vm.heap, &vm.rt.metas) {
                Ok(c) => branch!(vm, w, pc, off, c == $want),
                Err(_) => slow(vm, pc),
            }
        }
    };
}

q_eq_jump!(q_jump_eq, eq_other, r_jump_eq, true);
q_eq_jump!(q_jump_ne, ne_other, r_jump_ne, false);

q! {fn q_jump_eq_int(vm, w, pc, i) {
    let (a, imm, want, off) = i.r_jump_eq_int();
    let r = qwin!(vm, w, pc);
    let c = match r[a] {
        Value::Int(x) => x == imm as i64,
        Value::Float(f) => f == imm as f64,
        _ => false,
    };
    branch!(vm, w, pc, off, c == want)
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
            branch!(vm, w, pc, off, c == want)
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
    let c = match r[func] {
        Value::Closure(c) => c,
        Value::Native(_) => return call_native(vm, w, pc, i),
        _ => return slow(vm, pc),
    };
    let code: &ProtoCode = if c == vm.rt.closure {
        code!(vm)
    } else {
        // SAFETY: the callee's frame, pushed below, keeps it alive.
        unsafe { &*Rc::as_ptr(&vm.heap.get(c).code) }
    };
    let code_ptr = code as *const ProtoCode;
    let other_watched = code_ptr != vm.rt.code && watched(vm);
    let frames = &mut vm.rt.stack.frames;
    let depth = frames.len();
    let room = depth < frames.capacity();
    let Some(caller) = frames.last_mut() else {
        return slow(vm, pc);
    };
    let fa = caller.base as usize + func as usize;
    let covered = fa + 1 + WINDOW <= vm.rt.stack_len;
    // The cases with something to grow or fill are the slow handler's.
    if nargs == MULTI
        || nargs < code.nparams
        || code.is_vararg
        || code.ncells > 0
        || !room
        || !covered
        || fa + 1 + (code.nregs as usize).max(nargs as usize) > vm.rt.host.max_stack()
        || other_watched
    {
        return slow(vm, pc);
    }
    caller.pc = unsafe { pc.offset_from(vm.rt.wp) } as u32;
    let cell_base = vm.rt.stack.cells.len() as u32;
    // SAFETY: there is room for one more frame (checked above).
    unsafe {
        frames.as_mut_ptr().add(depth).write(Frame {
            kind: FrameKind::Lua {
                closure: c,
                code_id: code_ptr as usize,
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
        frames.set_len(depth + 1);
    }
    vm.rt.closure = c;
    vm.rt.code = code_ptr;
    let ip = code.words.as_ptr();
    vm.rt.wp = ip;
    // SAFETY: the callee's window is covered (checked above).
    next_counted(vm, unsafe { w.add(func as usize + 1) }, ip)
}}

/// A call of a native function that has a fast path and takes it: the
/// result goes where the function was, with no frame.
#[inline(never)]
fn call_native(vm: &mut Vm, w: *mut Value, pc: *const Insn, i: Insn) -> Step {
    let (func, nargs, want) = i.r_call();
    let mut r = qwin!(vm, w, pc);
    let Value::Native(id) = r[func] else {
        return slow(vm, pc);
    };
    let (at, n) = (func as usize, nargs as usize);
    if nargs == MULTI || (want > 1 && want != MULTI) {
        return slow(vm, pc);
    }
    let (Some(fast), Some(args)) = (vm.rt.natives.get(id).fast, r.get(at + 1..at + 1 + n)) else {
        return slow(vm, pc);
    };
    let Some(v) = fast(FastCtx::new(vm, want), args) else {
        return slow(vm, pc);
    };
    if want != 0 {
        r[func] = v;
    }
    let fa = vm.rt.stack.running().base as usize + at;
    vm.rt.top = fa + want.min(1) as usize;
    next(vm, w, pc)
}

/// Go on in the caller's frame, `up` registers below, at its instruction
/// `to`, after a return popped the frame above it.
#[inline(always)]
fn back(
    vm: &mut Vm,
    w: *mut Value,
    up: usize,
    to: usize,
    closure: Gc<Closure>,
    code_id: usize,
) -> Step {
    vm.rt.closure = closure;
    let code = code_id as *const ProtoCode;
    vm.rt.code = code;
    // SAFETY: the caller's code, which its closure keeps alive; `to` is
    // the instruction after its call, and its frame starts `up` below.
    let ip = unsafe { (*code).words.as_ptr() };
    vm.rt.wp = ip;
    next(vm, unsafe { w.sub(up) }, unsafe { ip.add(to) })
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
    if f.want != 1
        || f.base != f.func + 1
        || vm.rt.stack.cells.len() != f.cell_base as usize
        || (code_id != vm.rt.code as usize && watched(vm))
    {
        return slow(vm, pc);
    }
    let top = f.base as usize;
    let (up, to) = ((f.base - caller.base) as usize, caller.pc as usize);
    // SAFETY: `w` is the register below the frame's window.
    let result = unsafe { &mut *w };
    let r = unsafe { &*(w.add(1) as *const [Value; WINDOW]) };
    copy_value(result, &r[src as usize]);
    vm.rt.top = top;
    vm.rt.stack.frames.pop();
    back(vm, w, up, to, closure, code_id)
}}

q! {fn q_return0(vm, w, pc, _i) {
    let [.., caller, f] = &vm.rt.stack.frames[..] else {
        return slow(vm, pc);
    };
    let FrameKind::Lua { closure, code_id } = caller.kind else {
        return slow(vm, pc);
    };
    if f.want > 1
        || f.base != f.func + 1
        || vm.rt.stack.cells.len() != f.cell_base as usize
        || (code_id != vm.rt.code as usize && watched(vm))
    {
        return slow(vm, pc);
    }
    let (up, to) = ((f.base - caller.base) as usize, caller.pc as usize);
    // SAFETY: `w` is the register below the frame's window. Writing nil
    // there is right for one wanted result and harmless for none.
    unsafe { *w = Value::Nil };
    vm.rt.top = f.func as usize + f.want as usize;
    vm.rt.stack.frames.pop();
    back(vm, w, up, to, closure, code_id)
}}

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
                return next_back(vm, w, jump(pc, back));
            }
        }
        [Value::Float(cur), Value::Float(lim), Value::Float(s), var] => {
            let n = *cur + *s;
            if if *s > 0.0 { n <= *lim } else { *lim <= n } {
                *cur = n;
                *var = Value::Float(n);
                return next_back(vm, w, jump(pc, back));
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
        return next_back(vm, w, jump(pc, back));
    }
    next(vm, w, pc)
}}

/// Every instruction without a handler here.
fn q_slow(vm: &mut Vm, _w: *mut Value, pc: *const Insn, _i: Insn) -> Step {
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
    t[Opcode::Return0 as usize] = q_return0;
    t[Opcode::ForPrep as usize] = q_for_prep;
    t[Opcode::ForLoop as usize] = q_for_loop;
    t[Opcode::GenericLoop as usize] = q_generic_loop;
    t
};
