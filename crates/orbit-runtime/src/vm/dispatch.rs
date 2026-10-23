//! The dispatch loop ([`Vm::run`]): fetch an instruction, run it, repeat.
//!
//! The loop keeps the running frame's state in locals (its base, pc, code
//! and a fixed-size view of its registers) and runs the common
//! instructions in place. Everything rare runs out of line:
//!
//! - [`Vm::cold_op`] runs the instructions that are seldom executed or
//!   that call out of the loop anyway (allocation, varargs, casts, cells).
//! - [`Vm::call_native_in_loop`] runs a native called from Lua code.
//! - [`method_lookup`] walks the method caches for `obj:m()`.
//! - Errors and metamethod requests leave through one exit at the bottom
//!   of the loop, which hands them to [`Vm::raise`].
//!
//! Lua-to-Lua calls and returns switch the frame state in place (one call
//! path, one return path, shared by the instructions that call or return);
//! everything else that changes the frame stack comes back through
//! `'reload`, which reads the state from the frame on top.

use std::rc::Rc;

use crate::object::number::{ArithOp, Number};
use crate::object::table::{self, IntStore};
use crate::object::{
    Closure, FieldCache, Gc, LuaHeap, LuaSpace, LuaStr, Payload, ProtoCode, Table, Tag, Value,
};
use orbit_bytecode::number::BitOp;
use orbit_bytecode::{MULTI, Op};

use crate::vm::cold::{
    already_defined, env_error, get_str_miss, method_miss, op_cast, op_len, set_str_miss,
};
use crate::vm::fast::{
    self, Cmp,
    Src::{K, R},
};
use crate::vm::interp::{
    Flow, Ran, Shape, WINDOW, Window, const_str, enter_lua_frame_fast, enter_lua_frame_general,
};
use crate::vm::meta::Metas;
use crate::vm::native::{NativeCtx, NativeReturn};
use crate::vm::ops::{self, Arith, Exc};
use crate::vm::{Frame, FrameKind, LuaError, Vm};

/// The write barrier for storing `v` into `obj`. A store that refers to no
/// object needs none: with the value's variant known (`by_parts!`) the test
/// folds away. In generational mode this keeps number stores into old
/// objects from remembering them.
#[inline(always)]
fn barrier_for<T>(heap: &mut LuaHeap, obj: Gc<T>, v: Value)
where
    LuaSpace: orbit_gc::Holds<T>,
{
    if !matches!(
        v,
        Value::Nil | Value::Bool(_) | Value::Int(_) | Value::Float(_)
    ) {
        heap.barrier_back(obj);
    }
}

/// What [`Vm::cold_op`] tells the loop to do next.
enum Cold {
    /// Go on at `pc`; `safepoint` says the instruction allocated.
    Next { pc: usize, safepoint: bool },
    /// The frame stack changed.
    Flow(Flow),
}

/// What [`Vm::call_native_in_loop`] tells the loop to do next.
enum NativeStep {
    /// The native returned into the running frame with no hook armed:
    /// take the window again.
    Returned,
    /// The native returned into the running frame through the general
    /// path: it may have set or cleared a hook.
    Resumed,
    /// The frame stack changed.
    Flow(Flow),
}

/// Where the frame stack stands after a call or a return made from the
/// loop ([`Vm::call_from_loop`], [`Vm::return_from_loop`]): the loop
/// picks its state up from here.
enum Frames {
    /// A Lua frame runs now, the callee or the caller: the loop switches
    /// to it in place.
    Lua(LuaFrame),
    /// Still this frame, after a native's fast path: only the window has
    /// to be taken again.
    Same,
    /// Still this frame, after a native ran in place with no hook armed:
    /// it may have allocated.
    SameAllocated,
    /// Still this frame, back through the general call path: the native
    /// may have set or cleared a hook.
    SameResumed,
    /// The run goes on elsewhere: reload the frame, or finish.
    Flow(Flow),
}

/// The loop's state for a Lua frame, as a call or return hands it over.
#[derive(Clone, Copy)]
struct LuaFrame {
    closure: Gc<Closure>,
    base: usize,
    pc: usize,
    code_id: usize,
}

impl Vm {
    /// Run the top frame (a Lua frame) and everything it calls until it
    /// returns into a boundary, in the plain or the hooked
    /// dispatch loop as the stack's hook requires.
    pub(crate) fn run(&mut self) -> Result<(), LuaError> {
        loop {
            #[cfg(feature = "hooks")]
            let ran = if self.top_hooked() {
                self.run_loop::<true>()?
            } else {
                self.run_loop::<false>()?
            };
            #[cfg(not(feature = "hooks"))]
            let ran = self.run_loop::<false>()?;
            match ran {
                Ran::Done => return Ok(()),
                Ran::Switch if std::mem::take(&mut self.rt.no_memory) => {
                    // A safe point found the heap over its limit; the frame's
                    // pc is saved.
                    match self.raise(crate::object::NoMemory.into())? {
                        Flow::Done => return Ok(()),
                        Flow::Reload | Flow::Switched => self.rt.switched = false,
                    }
                }
                Ran::Switch => {}
            }
        }
    }

    /// The dispatch loop. `HOOKED` adds a hook point before each
    /// instruction ([`crate::vm::hook`]); either loop returns
    /// [`Ran::Switch`] when the hook's state no longer matches it.
    fn run_loop<const HOOKED: bool>(&mut self) -> Result<Ran, LuaError> {
        // The running function's code, constants and caches. Returning to or
        // calling the same function (recursion) keeps the `Rc` instead of
        // cloning it again; holding it keeps the allocation alive, so pointer
        // equality identifies the prototype.
        let mut cur: Rc<ProtoCode> = match self.rt.stack.frames.last().map(|f| f.kind) {
            Some(FrameKind::Lua { closure, .. }) => self.heap.get(closure).code.clone(),
            _ => unreachable!("run starts on a Lua frame"),
        };
        'reload: loop {
            // What comes back through `'reload` (a native closure's return,
            // an error caught by `pcall`, a metamethod's result) passes no
            // instruction's safe point, so a loop of such calls would
            // allocate without ever collecting. Take the step here; the
            // frame's pc is saved.
            if self.heap.should_step() {
                let base = self.rt.stack.frames.last().expect("a frame").base as usize;
                if self.gc_safepoint((base + WINDOW).max(self.rt.top)) {
                    // Over the memory limit: `run` raises the error.
                    return Ok(Ran::Switch);
                }
            }
            // The running frame's state. Lua-to-Lua calls and returns switch
            // it in place; everything else comes back through `'reload`.
            #[cfg(not(feature = "debugger"))]
            if self.rt.stack.hook.armed() != HOOKED {
                return Ok(Ran::Switch);
            }
            let frame = self.rt.stack.running();
            let FrameKind::Lua {
                mut closure,
                code_id,
            } = frame.kind
            else {
                unreachable!("run starts on a Lua frame")
            };
            let mut base = frame.base as usize;
            let mut pc = frame.pc as usize;
            // Both the frame's code and `cur` are alive, so equal addresses
            // mean the same code.
            if code_id != Rc::as_ptr(&cur) as usize {
                cur = self.heap.get(closure).code.clone();
            }
            // The debugger decides by function and depth, so it needs the
            // frame's code first.
            #[cfg(feature = "debugger")]
            if self.hooked_here(&cur, closure, self.rt.stack.depth()) != HOOKED {
                return Ok(Ran::Switch);
            }
            #[cfg(feature = "debugger")]
            if HOOKED {
                self.debug_reloaded(pc, self.rt.stack.depth());
            }
            let mut code = &cur.code[..];
            macro_rules! switch_code {
                ($closure:expr, $code_id:expr) => {
                    // A recursive call or its return keeps the same code.
                    if $code_id != Rc::as_ptr(&cur) as usize {
                        match &mut self.rt.prev_code {
                            // `prev_code` keeps its code alive, so an equal
                            // address is the same code.
                            Some(p) if Rc::as_ptr(p) as usize == $code_id => {
                                std::mem::swap(&mut cur, p)
                            }
                            _ => {
                                let new = self.heap.get($closure).code.clone();
                                self.rt.prev_code = Some(std::mem::replace(&mut cur, new));
                            }
                        }
                        code = &cur.code[..];
                        // While a breakpoint or step is set, entering other
                        // code may need the other loop. Recursion keeps the
                        // loop: a step keeps its own function hooked at
                        // every depth.
                        #[cfg(feature = "debugger")]
                        if self.rt.debug.watch
                            && self.debug_switches(&cur, $closure, self.rt.stack.depth(), HOOKED)
                        {
                            save_pc!();
                            return Ok(Ran::Switch);
                        }
                    }
                };
            }

            // The frame's registers as a fixed-size window, so indexing it
            // with a `u8` register number needs no bounds check, and the
            // window's address stays in a machine register. Anything that may
            // resize the stack (calls, collection steps, errors) ends the
            // borrow; take the window again afterwards.
            let mut w = Window::take(&mut self.rt.stack.regs, base);
            // Record the running frame's `pc` before anything that may read
            // the frame back (a call out of the loop, an error). A macro, not
            // a `Stack` method: as a function, even inlined, it moves the
            // loop's register allocation (+3-9% instructions, measured).
            macro_rules! save_pc {
                () => {
                    save_pc!(pc)
                };
                ($pc:expr) => {
                    self.rt
                        .stack
                        .frames
                        .last_mut()
                        .expect("the running frame")
                        .pc = $pc as u32;
                };
            }
            // Run `$body` with `$v` bound to `w[$src]`, reading a number
            // by parts: a register just written as a tag store and a
            // payload store would stall a whole 16-byte load on store
            // forwarding, and with the variant known `$body` stores it by
            // parts too. A macro, not a function taking `$body` as a
            // closure: the four arms then call one closure, the optimizer
            // merges the calls back into one with the value in memory, and
            // the store is a 16-byte copy again (measured, Sieve).
            macro_rules! by_parts {
                ($src:expr, |$v:ident| $body:expr) => {
                    match w[$src] {
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
            macro_rules! safepoint {
                () => {
                    if self.heap.should_step() {
                        save_pc!();
                        // Nothing above this frame's window and the last
                        // open results is live; don't let it keep garbage.
                        let live = (base + WINDOW).max(self.rt.top);
                        if self.gc_safepoint(live) {
                            // Over the memory limit: `run` raises the error.
                            return Ok(Ran::Switch);
                        }
                        w = Window::take(&mut self.rt.stack.regs, base);
                    }
                };
            }
            macro_rules! jump {
                ($off:expr) => {{
                    pc = (pc as isize + $off as isize) as usize;
                    #[cfg(feature = "hooks")]
                    if !HOOKED
                        && $off < 0
                        && self.rt.interrupt.load(std::sync::atomic::Ordering::Relaxed)
                    {
                        // An interrupt was requested ([`crate::vm::Interrupt`]): the
                        // hooked loop raises it before the jump's target.
                        save_pc!();
                        self.rt.hooks_touched = true;
                        self.rt.stack.hook.mask |= crate::vm::hook::INTERRUPT;
                        return Ok(Ran::Switch);
                    }
                }};
            }
            // Something returned into the running frame from outside the
            // loop (a native, a metamethod): the native may have set or
            // cleared a hook, so check which loop runs on; then take the
            // window again and let the collector catch up.
            macro_rules! resumed_here {
                () => {{
                    if self.hooked_here(&cur, closure, self.rt.stack.depth()) != HOOKED {
                        return Ok(Ran::Switch);
                    }
                    w = Window::take(&mut self.rt.stack.regs, base);
                    safepoint!();
                }};
            }

            // The instruction loop. It ends with an instruction's error,
            // which is raised below; calls and returns into other frames
            // `continue 'reload`, and a finished run returns.
            let exc: Exc = 'run: loop {
                // The instruction loop proper: an instruction's error, or a
                // metamethod request, ends it.
                let exc: Exc = 'insn: loop {
                    // An instruction's error (or metamethod request) ends the
                    // instruction loop; the code after it raises.
                    macro_rules! throw {
                        ($x:expr) => {
                            break 'insn $x
                        };
                    }
                    macro_rules! tri {
                        ($e:expr) => {
                            match $e {
                                Ok(v) => v,
                                Err(x) => throw!(x),
                            }
                        };
                    }
                    macro_rules! flow {
                        ($e:expr) => {
                            match $e {
                                Ok(Flow::Reload) => continue 'reload,
                                Ok(Flow::Switched) => {
                                    self.rt.switched = false;
                                    continue 'reload;
                                }
                                Ok(Flow::Done) => return Ok(Ran::Done),
                                Err(x) => throw!(x),
                            }
                        };
                    }

                    // Pick the loop's state up after a call or a return
                    // ([`Frames`]): the frame to run, and whether the
                    // window, the collector or the hook check come first.
                    // Switch the loop's state to a Lua frame, the callee of
                    // a call or the caller of a return.
                    macro_rules! enter {
                        ($f:expr) => {{
                            let f: LuaFrame = $f;
                            closure = f.closure;
                            base = f.base;
                            pc = f.pc;
                            switch_code!(f.closure, f.code_id);
                            w = Window::take(&mut self.rt.stack.regs, base);
                        }};
                    }
                    macro_rules! frames {
                        ($e:expr) => {
                            match $e {
                                Ok(Frames::Lua(f)) => enter!(f),
                                Ok(Frames::Same) => {
                                    w = Window::take(&mut self.rt.stack.regs, base);
                                }
                                Ok(Frames::SameAllocated) => {
                                    w = Window::take(&mut self.rt.stack.regs, base);
                                    safepoint!();
                                }
                                Ok(Frames::SameResumed) => resumed_here!(),
                                Ok(Frames::Flow(f)) => flow!(Ok(f)),
                                Err(x) => throw!(x),
                            }
                        };
                    }
                    let op = code[pc];
                    pc += 1;
                    if HOOKED {
                        save_pc!();
                        if let Err(e) = self.hook_point(pc - 1, op) {
                            throw!(Exc::Lua(e));
                        }
                        #[cfg(feature = "debugger")]
                        if self.rt.debug.watch
                            && let Err(e) = self.debug_point(pc - 1)
                        {
                            throw!(Exc::Lua(e));
                        }
                        if !self.hooked_here(&cur, closure, self.rt.stack.depth()) {
                            // The hook was turned off: run this instruction in
                            // the plain loop.
                            save_pc!(pc - 1);
                            return Ok(Ran::Switch);
                        }
                        w = Window::take(&mut self.rt.stack.regs, base);
                    }
                    match op {
                        // Numbers are copied by parts: the source was usually
                        // just written that way, and a whole 16-byte load would
                        // stall on store forwarding.
                        Op::Move { dst, src } => match w[src] {
                            Value::Int(x) => w[dst] = Value::Int(x),
                            Value::Float(f) => w[dst] = Value::Float(f),
                            Value::Table(t) => w[dst] = Value::Table(t),
                            Value::Str(x) => w[dst] = Value::Str(x),
                            Value::Closure(c) => w[dst] = Value::Closure(c),
                            Value::Bool(b) => w[dst] = Value::Bool(b),
                            v => w[dst] = v,
                        },
                        Op::LoadNil { dst, count } => {
                            let at = dst as usize;
                            for r in &mut w[at..at + count as usize] {
                                *r = Value::Nil;
                            }
                        }
                        Op::LoadBool { dst, value } => w[dst] = Value::Bool(value),
                        Op::LoadInt { dst, value } => w[dst] = Value::Int(value as i64),
                        Op::LoadConst { dst, k } => w[dst] = cur.consts[k as usize],

                        Op::CellGet { dst, cell } => {
                            let cell_base = self
                                .rt
                                .stack
                                .frames
                                .last()
                                .expect("the running frame")
                                .cell_base as usize;
                            let c = self.rt.stack.cells[cell_base + cell as usize];
                            w[dst] = self.heap.get(c).value;
                        }
                        Op::CapGet { dst, cap } => {
                            let c = self.heap.get(closure).captures[cap as usize];
                            w[dst] = self.heap.get(c).value;
                        }
                        Op::CapSelf { dst } => w[dst] = Value::Closure(closure),
                        Op::CapSet { cap, src } => {
                            let c = self.heap.get(closure).captures[cap as usize];
                            by_parts!(src, |v| {
                                barrier_for(&mut self.heap, c, v);
                                self.heap.get_mut(c).value = v
                            });
                        }

                        Op::GetGlobal { dst, name, ic } => {
                            let env_cell = self.heap.get(closure).captures[0];
                            match self.heap.get(env_cell).value {
                                Value::Table(t) => {
                                    let cache = &cur.caches[ic as usize];
                                    let tab = self.heap.get(t);
                                    match tab
                                        .cached_str_packed(cache.slot.get() as usize, cache.key)
                                    {
                                        // A dead entry is a miss when `__index`
                                        // could apply.
                                        Some((tag, p)) if tag != Tag::Nil || tab.meta.is_none() => {
                                            Value::write_unpacked(&mut w[dst], tag, p)
                                        }
                                        _ => {
                                            let k = const_str(&cur.consts, name);
                                            w[dst] = tri!(get_str_miss(
                                                &self.heap,
                                                &self.rt.metas,
                                                t,
                                                k,
                                                cache
                                            ));
                                        }
                                    }
                                }
                                other => throw!(env_error(&self.heap, &self.rt.metas, other)),
                            }
                        }

                        Op::GetField { dst, obj, key, ic } => {
                            let &Value::Table(t) = &w[obj] else {
                                throw!(Exc::index(w[obj], cur.consts[key as usize]))
                            };
                            let cache = &cur.caches[ic as usize];
                            let tab = self.heap.get(t);
                            match tab.cached_str_packed(cache.slot.get() as usize, cache.key) {
                                // A dead entry is a miss when `__index` could
                                // apply.
                                Some((tag, p)) if tag != Tag::Nil || tab.meta.is_none() => {
                                    Value::write_unpacked(&mut w[dst], tag, p)
                                }
                                _ => {
                                    let k = const_str(&cur.consts, key);
                                    w[dst] =
                                        tri!(get_str_miss(&self.heap, &self.rt.metas, t, k, cache));
                                }
                            }
                        }
                        Op::SetField { obj, key, src, ic } => {
                            let &Value::Table(t) = &w[obj] else {
                                throw!(Exc::new_index(w[obj], cur.consts[key as usize], w[src]))
                            };
                            let cache = &cur.caches[ic as usize];
                            let slot = cache.slot.get() as usize;
                            let tab = self.heap.get(t);
                            if matches!(tab.cached_str_packed(slot, cache.key),
                            Some((tag, _)) if tab.meta.is_none() || tag != Tag::Nil)
                            {
                                // The key is there: overwrite its value. No
                                // capacity changes, so no byte accounting.
                                by_parts!(src, |v| {
                                    barrier_for(&mut self.heap, t, v);
                                    self.heap.get_mut(t).set_slot(slot, v)
                                });
                            } else {
                                let k = const_str(&cur.consts, key);
                                let v = w[src];
                                tri!(set_str_miss(&mut self.heap, &self.rt.metas, t, k, v, cache));
                                safepoint!();
                            }
                        }
                        Op::GetIndex { dst, obj, key } => {
                            let &Value::Table(t) = &w[obj] else {
                                throw!(Exc::index(w[obj], w[key]))
                            };
                            let tab = self.heap.get(t);
                            // A nil result in a table with a metatable may
                            // come from `__index`. `dst` may be `obj` or `key`.
                            match w[key] {
                                Value::Int(i) => {
                                    if !tab.read_int_present(i, &mut w[dst]) {
                                        let v = tab.get_int(i);
                                        if v.is_nil() && tab.meta.is_some() {
                                            throw!(Exc::index(Value::Table(t), Value::Int(i)))
                                        }
                                        w[dst] = v;
                                    }
                                }
                                k => {
                                    w[dst] = tab.get(&self.heap, k);
                                    if tab.meta.is_some() && w[dst].is_nil() {
                                        throw!(Exc::index(Value::Table(t), k))
                                    }
                                }
                            }
                        }
                        Op::SetIndex { obj, key, src } => {
                            let &Value::Table(t) = &w[obj] else {
                                throw!(Exc::new_index(w[obj], w[key], w[src]))
                            };
                            match w[key] {
                                // A store into the array part allocates
                                // nothing, so it needs no safe point.
                                Value::Int(i) => match by_parts!(src, |v| table::store_int(
                                    &mut self.heap,
                                    t,
                                    i,
                                    v
                                )) {
                                    IntStore::Array => {}
                                    IntStore::Other => safepoint!(),
                                    IntStore::Slow => {
                                        throw!(Exc::new_index(w[obj], w[key], w[src]))
                                    }
                                },
                                k => {
                                    let v = w[src];
                                    if self.heap.get(t).meta.is_some() {
                                        throw!(Exc::new_index(w[obj], k, v))
                                    }
                                    if let Err(e) = table::set(&mut self.heap, t, k, v) {
                                        throw!(ops::store_failed(e, t, k, v));
                                    }
                                    safepoint!();
                                }
                            }
                        }
                        Op::GetIndexInt { dst, obj, key } => {
                            let &Value::Table(t) = &w[obj] else {
                                throw!(Exc::index(w[obj], Value::Int(key as i64)))
                            };
                            let tab = self.heap.get(t);
                            if !tab.read_int_present(key as i64, &mut w[dst]) {
                                let v = tab.get_int(key as i64);
                                if v.is_nil() && tab.meta.is_some() {
                                    throw!(Exc::index(Value::Table(t), Value::Int(key as i64)))
                                }
                                w[dst] = v;
                            }
                        }
                        Op::SetIndexInt { obj, key, src } => {
                            let &Value::Table(t) = &w[obj] else {
                                throw!(Exc::new_index(w[obj], Value::Int(key as i64), w[src]))
                            };
                            match by_parts!(src, |v| table::store_int(
                                &mut self.heap,
                                t,
                                key as i64,
                                v
                            )) {
                                IntStore::Array => {}
                                IntStore::Other => safepoint!(),
                                IntStore::Slow => {
                                    throw!(Exc::new_index(w[obj], Value::Int(key as i64), w[src]))
                                }
                            }
                        }
                        Op::SelfCall { dst, obj, key, ic } => {
                            let Value::Str(k) = cur.consts[key as usize] else {
                                unreachable!("method names are strings")
                            };
                            // Copied by parts: a whole-value copy also moves the
                            // padding bytes, through overlapping stack slots.
                            let t = match w[obj] {
                                Value::Table(t) => t,
                                o => {
                                    w[dst + 1] = o;
                                    throw!(Exc::index(o, Value::Str(k)))
                                }
                            };
                            w[dst + 1] = Value::Table(t);
                            match method_lookup(&self.heap, &self.rt.metas, &cur, ic, k, t) {
                                Some((tag, p)) => Value::write_unpacked(&mut w[dst], tag, p),
                                None => throw!(Exc::index(Value::Table(t), Value::Str(k))),
                            }
                        }

                        Op::Add { dst, a, b } => tri!(fast::arith(
                            &mut w,
                            dst,
                            R(a),
                            R(b),
                            Arith::Add,
                            &self.heap,
                            &self.rt.metas
                        )),
                        Op::Sub { dst, a, b } => tri!(fast::arith(
                            &mut w,
                            dst,
                            R(a),
                            R(b),
                            Arith::Sub,
                            &self.heap,
                            &self.rt.metas
                        )),
                        Op::Mul { dst, a, b } => tri!(fast::arith(
                            &mut w,
                            dst,
                            R(a),
                            R(b),
                            Arith::Mul,
                            &self.heap,
                            &self.rt.metas
                        )),
                        Op::Div { dst, a, b } => tri!(fast::arith(
                            &mut w,
                            dst,
                            R(a),
                            R(b),
                            Arith::Div,
                            &self.heap,
                            &self.rt.metas
                        )),
                        Op::Mod { dst, a, b } => tri!(fast::arith(
                            &mut w,
                            dst,
                            R(a),
                            R(b),
                            Arith::Mod,
                            &self.heap,
                            &self.rt.metas
                        )),
                        Op::MulK { dst, a, k } => {
                            tri!(fast::arith(
                                &mut w,
                                dst,
                                R(a),
                                K(&cur.consts[k as usize]),
                                Arith::Mul,
                                &self.heap,
                                &self.rt.metas
                            ))
                        }
                        Op::ModK { dst, a, k } => {
                            tri!(fast::arith(
                                &mut w,
                                dst,
                                R(a),
                                K(&cur.consts[k as usize]),
                                Arith::Mod,
                                &self.heap,
                                &self.rt.metas
                            ))
                        }
                        Op::KAdd { dst, k, b } => {
                            tri!(fast::arith(
                                &mut w,
                                dst,
                                K(&cur.consts[k as usize]),
                                R(b),
                                Arith::Add,
                                &self.heap,
                                &self.rt.metas
                            ))
                        }
                        Op::KMul { dst, k, b } => {
                            tri!(fast::arith(
                                &mut w,
                                dst,
                                K(&cur.consts[k as usize]),
                                R(b),
                                Arith::Mul,
                                &self.heap,
                                &self.rt.metas
                            ))
                        }
                        Op::KDiv { dst, k, b } => {
                            tri!(fast::arith(
                                &mut w,
                                dst,
                                K(&cur.consts[k as usize]),
                                R(b),
                                Arith::Div,
                                &self.heap,
                                &self.rt.metas
                            ))
                        }
                        Op::AddInt { dst, a, imm } => match w[a] {
                            Value::Int(i) => w[dst] = Value::Int(i.wrapping_add(imm as i64)),
                            Value::Float(f) => w[dst] = Value::Float(f + imm as f64),
                            _ => {
                                let x = w[a];
                                w[dst] = tri!(ops::arith_slow(
                                    &self.heap,
                                    &self.rt.metas,
                                    Arith::Add,
                                    x,
                                    Value::Int(imm as i64)
                                ))
                            }
                        },
                        Op::SubInt { dst, a, imm } => match w[a] {
                            Value::Int(i) => w[dst] = Value::Int(i.wrapping_sub(imm as i64)),
                            // Adding the negated integer, as lua5.5 does: `-0.0 - 0` is `0.0` there.
                            Value::Float(f) => w[dst] = Value::Float(f + (-(imm as i64)) as f64),
                            _ => {
                                let x = w[a];
                                w[dst] = tri!(ops::arith_slow(
                                    &self.heap,
                                    &self.rt.metas,
                                    Arith::Sub,
                                    x,
                                    Value::Int(imm as i64)
                                ))
                            }
                        },
                        Op::BAnd { dst, a, b } => tri!(fast::bitwise(
                            &mut w,
                            dst,
                            a,
                            b,
                            BitOp::And,
                            &self.heap,
                            &self.rt.metas
                        )),
                        Op::BOr { dst, a, b } => tri!(fast::bitwise(
                            &mut w,
                            dst,
                            a,
                            b,
                            BitOp::Or,
                            &self.heap,
                            &self.rt.metas
                        )),
                        Op::BXor { dst, a, b } => tri!(fast::bitwise(
                            &mut w,
                            dst,
                            a,
                            b,
                            BitOp::Xor,
                            &self.heap,
                            &self.rt.metas
                        )),
                        Op::Shl { dst, a, b } => tri!(fast::bitwise(
                            &mut w,
                            dst,
                            a,
                            b,
                            BitOp::Shl,
                            &self.heap,
                            &self.rt.metas
                        )),
                        Op::Shr { dst, a, b } => tri!(fast::bitwise(
                            &mut w,
                            dst,
                            a,
                            b,
                            BitOp::Shr,
                            &self.heap,
                            &self.rt.metas
                        )),
                        Op::Not { dst, src } => w[dst] = Value::Bool(w[src].is_falsy()),
                        Op::Len { dst, src } => {
                            let x = w[src];
                            w[dst] = tri!(op_len(&self.heap, &self.rt.metas, x));
                        }

                        Op::Jump { off } => jump!(off),
                        Op::JumpIfTrue { src, off } => {
                            if !w[src].is_falsy() {
                                jump!(off);
                            }
                        }
                        Op::JumpIfFalse { src, off } => {
                            if w[src].is_falsy() {
                                jump!(off);
                            }
                        }
                        Op::JumpLt { a, b, off } => {
                            if tri!(fast::compare(&w, a, b, Cmp::Lt, &self.heap, &self.rt.metas)) {
                                jump!(off);
                            }
                        }
                        Op::JumpNotLt { a, b, off } => {
                            if !tri!(fast::compare(&w, a, b, Cmp::Lt, &self.heap, &self.rt.metas)) {
                                jump!(off);
                            }
                        }
                        Op::JumpLe { a, b, off } => {
                            if tri!(fast::compare(&w, a, b, Cmp::Le, &self.heap, &self.rt.metas)) {
                                jump!(off);
                            }
                        }
                        Op::JumpNotLe { a, b, off } => {
                            if !tri!(fast::compare(&w, a, b, Cmp::Le, &self.heap, &self.rt.metas)) {
                                jump!(off);
                            }
                        }
                        Op::JumpEq { a, b, off } => {
                            if tri!(fast::equal(&w, a, b, &self.heap, &self.rt.metas)) {
                                jump!(off);
                            }
                        }
                        Op::JumpNe { a, b, off } => {
                            if !tri!(fast::equal(&w, a, b, &self.heap, &self.rt.metas)) {
                                jump!(off);
                            }
                        }
                        Op::JumpEqInt { a, imm, want, off } => {
                            let c = match w[a] {
                                Value::Int(i) => i == imm as i64,
                                Value::Float(f) => f == imm as f64,
                                _ => false,
                            };
                            if c == want {
                                jump!(off);
                            }
                        }
                        Op::JumpLtInt { a, imm, want, off } => {
                            if tri!(fast::compare_int(
                                &w,
                                a,
                                imm,
                                Cmp::Lt,
                                &self.heap,
                                &self.rt.metas
                            )) == want
                            {
                                jump!(off);
                            }
                        }
                        Op::JumpLeInt { a, imm, want, off } => {
                            if tri!(fast::compare_int(
                                &w,
                                a,
                                imm,
                                Cmp::Le,
                                &self.heap,
                                &self.rt.metas
                            )) == want
                            {
                                jump!(off);
                            }
                        }
                        Op::JumpGtInt { a, imm, want, off } => {
                            if tri!(fast::compare_int(
                                &w,
                                a,
                                imm,
                                Cmp::Gt,
                                &self.heap,
                                &self.rt.metas
                            )) == want
                            {
                                jump!(off);
                            }
                        }
                        Op::JumpGeInt { a, imm, want, off } => {
                            if tri!(fast::compare_int(
                                &w,
                                a,
                                imm,
                                Cmp::Ge,
                                &self.heap,
                                &self.rt.metas
                            )) == want
                            {
                                jump!(off);
                            }
                        }

                        // Calls. Each arm works out the callee's register, its
                        // argument count and the results the caller takes;
                        // `call_from_loop` enters the callee.
                        Op::Call {
                            func,
                            nargs,
                            nresults,
                        } => {
                            let n = if nargs == MULTI {
                                self.rt.top - (base + func as usize) - 1
                            } else {
                                nargs as usize
                            };
                            let fa = base + func as usize;
                            match w[func] {
                                Value::Closure(c) => enter!(tri!(self.enter_lua_call::<HOOKED>(
                                    c, closure, &cur, pc, fa, n, nresults
                                ))),
                                callee => {
                                    frames!(self.call_other_from_loop::<HOOKED>(
                                        callee, pc, fa, n, nresults
                                    ))
                                }
                            }
                        }
                        Op::GenericCall { base: b, nvars } => {
                            let at = b as usize;
                            w.copy_within(at..at + 3, at + 4);
                            let (fa, n) = (base + at + 4, 2);
                            match w[at + 4] {
                                Value::Closure(c) => {
                                    enter!(tri!(self.enter_lua_call::<HOOKED>(
                                        c, closure, &cur, pc, fa, n, nvars
                                    )))
                                }
                                callee => frames!(
                                    self.call_other_from_loop::<HOOKED>(callee, pc, fa, n, nvars)
                                ),
                            }
                        }
                        Op::TailCall { func, nargs } => {
                            let fa = base + func as usize;
                            let n = if nargs == MULTI {
                                self.rt.top - fa - 1
                            } else {
                                nargs as usize
                            };
                            // A native closure or a built-in in tail position
                            // is called like a native, keeping this frame, as
                            // natives are.
                            if let Value::Closure(c) = w[func as usize]
                                && let code = &self.heap.get(c).code
                                && !code.builtin
                                && !matches!(code.code.first(), Some(Op::NativeEntry { .. }))
                            {
                                let f = self.rt.stack.frames.pop().expect("frame");
                                self.rt.stack.cells.truncate(f.cell_base as usize);
                                let dst = f.func as usize;
                                self.rt.stack.regs.copy_within(fa..fa + 1 + n, dst);
                                if let Err(x) = self.push_lua_frame(c, dst, n, f.want) {
                                    self.rt.stack.frames.push(f);
                                    throw!(x);
                                }
                                self.rt.stack.frames.last_mut().expect("frame").tail = true;
                                if HOOKED {
                                    self.rt.stack.hook.call_pending = Some(true);
                                }
                                continue 'reload;
                            }
                            // A native in tail position: an ordinary call whose
                            // results the following `Return` delivers.
                            match w[func] {
                                Value::Closure(c) => {
                                    enter!(tri!(self.enter_lua_call::<HOOKED>(
                                        c, closure, &cur, pc, fa, n, MULTI
                                    )))
                                }
                                callee => frames!(
                                    self.call_other_from_loop::<HOOKED>(callee, pc, fa, n, MULTI)
                                ),
                            }
                        }

                        // Returns. Back into a Lua caller the loop switches
                        // its frame state in place; the caller's window lies
                        // below this frame's, so the stack is already long
                        // enough for its results and its window.
                        Op::Return { first, count } => {
                            let src = base + first as usize;
                            let n = if count == MULTI {
                                self.rt.top - src
                            } else {
                                count as usize
                            };
                            frames!(self.return_from_loop(src, n));
                        }
                        Op::Return0 => frames!(self.return_from_loop(base, 0)),
                        Op::Return1 { src } => {
                            // The common case, one value back to a Lua caller
                            // that wants one, without the general return path.
                            let from = base + src as usize;
                            if let Some((caller, f)) = self.lua_caller()
                                && f.want == 1
                            {
                                let func = f.func as usize;
                                self.pop_frame(&f);
                                // Read from the callee's window and copied by
                                // parts: the value was usually just written
                                // that way, and a whole 16-byte load would
                                // stall on store forwarding.
                                fast::move_by_parts(&mut self.rt.stack.regs, from, func);
                                self.rt.top = func + 1;
                                enter!(caller);
                            } else {
                                frames!(self.return_from_loop(from, 1));
                            }
                        }

                        // Allocating instructions call out anyway; they stay
                        // in the loop so the call is all they pay.
                        Op::NewTable {
                            dst,
                            array_hint,
                            hash_hint,
                        } => {
                            let t = self.op_new_table(array_hint as usize, hash_hint as usize);
                            w = Window::take(&mut self.rt.stack.regs, base);
                            w[dst] = Value::Table(t);
                            safepoint!();
                        }
                        Op::SetList {
                            table: tr,
                            first,
                            count,
                            start,
                        } => {
                            let Value::Table(t) = w[tr] else {
                                unreachable!("SetList on a table")
                            };
                            let from = base + first as usize;
                            let n = if count == MULTI {
                                self.rt.top - from
                            } else {
                                count as usize
                            };
                            let stored = self.op_set_list(t, from, n, start as i64);
                            w = Window::take(&mut self.rt.stack.regs, base);
                            if let Err(e) = stored {
                                throw!(e.into());
                            }
                            safepoint!();
                        }
                        Op::Concat { dst, first, count } => {
                            let v = tri!(self.op_concat(base + first as usize, count as usize));
                            w = Window::take(&mut self.rt.stack.regs, base);
                            w[dst] = v;
                            safepoint!();
                        }
                        Op::Closure { dst, proto } => {
                            let cell_base = self
                                .rt
                                .stack
                                .frames
                                .last()
                                .expect("the running frame")
                                .cell_base as usize;
                            let c = self.op_closure(closure, cell_base, proto as usize);
                            w = Window::take(&mut self.rt.stack.regs, base);
                            w[dst] = c;
                            safepoint!();
                        }
                        Op::CellNew { cell, src } => {
                            let cell_base = self
                                .rt
                                .stack
                                .frames
                                .last()
                                .expect("the running frame")
                                .cell_base as usize;
                            let v = w[src];
                            self.op_cell_new(cell_base + cell as usize, v);
                            w = Window::take(&mut self.rt.stack.regs, base);
                            safepoint!();
                        }
                        Op::CellSet { cell, src } => {
                            let cell_base = self
                                .rt
                                .stack
                                .frames
                                .last()
                                .expect("the running frame")
                                .cell_base as usize;
                            let c = self.rt.stack.cells[cell_base + cell as usize];
                            let v = w[src];
                            barrier_for(&mut self.heap, c, v);
                            self.heap.get_mut(c).value = v;
                        }

                        Op::VarargLen { dst } => {
                            let nvarargs = self
                                .rt
                                .stack
                                .frames
                                .last()
                                .expect("the running frame")
                                .nvarargs;
                            w[dst] = Value::Int(nvarargs as i64);
                        }

                        Op::ForPrep { base: b, exit } => {
                            // The common integer loop counting up; the rest
                            // (floats, negative steps, errors) is cold.
                            if let (&Value::Int(i), &Value::Int(lim), &Value::Int(s)) =
                                (&w[b], &w[b + 1], &w[b + 2])
                                && s > 0
                            {
                                if i <= lim {
                                    // The iterations left replace the limit.
                                    let count = (lim as u64).wrapping_sub(i as u64) / s as u64;
                                    w[b + 1] = Value::Int(count as i64);
                                    w[b + 3] = Value::Int(i);
                                } else {
                                    jump!(exit);
                                }
                            } else {
                                let runs = tri!(self.op_for_prep(base + b as usize, false));
                                w = Window::take(&mut self.rt.stack.regs, base);
                                if !runs {
                                    jump!(exit);
                                }
                            }
                        }
                        Op::ForLoop { base: b, back } => {
                            // One match over the four loop registers: the
                            // integer arm tests each tag once. Update the
                            // counters' payloads in place: their tags are
                            // already right, so only the payloads need stores.
                            let at = b as usize;
                            match &mut w[at..at + 4] {
                                [Value::Int(cur), Value::Int(count), Value::Int(s), var] => {
                                    if *count > 0 {
                                        *count -= 1;
                                        *cur = cur.wrapping_add(*s);
                                        *var = Value::Int(*cur);
                                        jump!(back);
                                    }
                                }
                                [Value::Float(cur), Value::Float(lim), Value::Float(s), var] => {
                                    let next = *cur + *s;
                                    if if *s > 0.0 { next <= *lim } else { *lim <= next } {
                                        *cur = next;
                                        *var = Value::Float(next);
                                        jump!(back);
                                    }
                                }
                                _ => unreachable!("loop state"),
                            }
                        }
                        Op::GenericLoop { base: b, back } => {
                            let v = w[b + 4];
                            if !v.is_nil() {
                                w[b + 2] = v;
                                jump!(back);
                            }
                        }

                        // Everything else is rare, or calls out of the loop
                        // anyway: run it out of line. The arms are listed so
                        // the match stays exhaustive without a default, which
                        // would put a range check in front of the jump table.
                        Op::LoadConstX { .. }
                        | Op::CellClear { .. }
                        | Op::CheckUndefined { .. }
                        | Op::SetGlobal { .. }
                        | Op::Pow { .. }
                        | Op::IDiv { .. }
                        | Op::AddK { .. }
                        | Op::SubK { .. }
                        | Op::DivK { .. }
                        | Op::IDivK { .. }
                        | Op::PowK { .. }
                        | Op::KSub { .. }
                        | Op::BNot { .. }
                        | Op::Neg { .. }
                        | Op::Cast { .. }
                        | Op::Vararg { .. }
                        | Op::VarargTable { .. }
                        | Op::VarargGet { .. }
                        | Op::VarargGetInt { .. }
                        | Op::VarargUnpack { .. }
                        | Op::ForPrepExclusive { .. }
                        | Op::NativeEntry { .. }
                        | Op::Tbc { .. }
                        | Op::Close { .. } => match self.cold_op(op, &cur, closure, base, pc) {
                            Ok(Cold::Next {
                                pc: next,
                                safepoint,
                            }) => {
                                pc = next;
                                w = Window::take(&mut self.rt.stack.regs, base);
                                if safepoint {
                                    safepoint!();
                                }
                            }
                            Ok(Cold::Flow(flow)) => flow!(Ok(flow)),
                            Err(x) => throw!(x),
                        },
                    }
                };
                // An `__index` request (a raw miss on a table with a
                // metatable, or indexing a value that isn't a table):
                // follow the metatable chain here and finish the
                // instruction without leaving the loop. A metamethod
                // call, or an error, still goes out through `raise`.
                if let Exc::Index { obj, key } = exc {
                    match crate::vm::meta::index(&self.heap, &self.rt.metas, obj, key) {
                        Ok(v) => {
                            let dst = match code[pc - 1] {
                                Op::GetField { dst, .. }
                                | Op::GetIndex { dst, .. }
                                | Op::GetIndexInt { dst, .. }
                                | Op::SelfCall { dst, .. } => dst,
                                _ => unreachable!("an indexing instruction"),
                            };
                            // The window is retaken: the paths that end the
                            // instruction loop don't all hold one.
                            w = Window::take(&mut self.rt.stack.regs, base);
                            w[dst] = v;
                            continue 'run;
                        }
                        Err(e) => break 'run e,
                    }
                }
                break 'run exc;
            };
            // The instruction at `pc - 1` raised `exc`. After a coroutine
            // switch, the running stack isn't the one this loop's frame
            // state describes, so its pc isn't saved.
            if !std::mem::take(&mut self.rt.switched) {
                save_pc!();
            }
            match self.raise(exc) {
                Ok(Flow::Reload | Flow::Switched) => {
                    self.rt.switched = false;
                    continue 'reload;
                }
                Ok(Flow::Done) => return Ok(Ran::Done),
                Err(e) => return Err(e),
            }
        }
    }

    /// Enter the Lua closure `c`, called from absolute register `fa` of
    /// the running frame (`closure`, `cur`, at `pc`) with `n` arguments
    /// and `want` results: its frame is pushed (the entry saves the pc)
    /// and handed to the loop.
    #[inline(always)]
    #[allow(clippy::too_many_arguments)]
    fn enter_lua_call<const HOOKED: bool>(
        &mut self,
        c: Gc<Closure>,
        closure: Gc<Closure>,
        cur: &Rc<ProtoCode>,
        pc: usize,
        fa: usize,
        n: usize,
        want: u8,
    ) -> Result<LuaFrame, Exc> {
        // A function calling itself: the running frame keeps `closure`
        // alive, so an equal handle is the same closure, and its code is
        // `cur`.
        let shape = if c == closure {
            Shape::of(cur)
        } else {
            Shape::of(&self.heap.get(c).code)
        };
        let new_base = match enter_lua_frame_fast(&mut self.rt, &shape, c, fa, n, want, pc as u32) {
            Some(b) => b,
            None => {
                self.rt
                    .stack
                    .frames
                    .last_mut()
                    .expect("the running frame")
                    .pc = pc as u32;
                enter_lua_frame_general(
                    &mut self.rt,
                    Shape::of(&self.heap.get(c).code),
                    c,
                    fa,
                    n,
                    want,
                )?
                .base
            }
        };
        if HOOKED {
            self.rt.stack.hook.call_pending = Some(false);
        }
        Ok(LuaFrame {
            closure: c,
            base: new_base,
            pc: 0,
            code_id: shape.code_id,
        })
    }

    /// Call `callee`, anything but a Lua closure, from absolute register
    /// `fa` of the running frame (at `pc`) with `n` arguments and `want`
    /// results. A native runs here, in place.
    #[inline(always)]
    fn call_other_from_loop<const HOOKED: bool>(
        &mut self,
        callee: Value,
        pc: usize,
        fa: usize,
        n: usize,
        want: u8,
    ) -> Result<Frames, Exc> {
        match callee {
            // A native's fast path, when it has one and takes these
            // arguments: no window, no frame.
            Value::Native(id)
                if !HOOKED
                    && (want <= 1 || want == MULTI)
                    && let Some(fast) = self.rt.natives[id.0 as usize].fast
                    && let Some(v) = self
                        .rt
                        .stack
                        .regs
                        .get(fa + 1..fa + 1 + n)
                        .and_then(|a| fast(&self.heap, a)) =>
            {
                if want != 0 {
                    self.rt.stack.regs[fa] = v;
                }
                self.rt.top = fa + want.min(1) as usize;
                Ok(Frames::Same)
            }
            // Any other native, called in place: no frame, and a plain
            // return comes straight back to the loop.
            Value::Native(id) if !HOOKED => {
                self.rt
                    .stack
                    .frames
                    .last_mut()
                    .expect("the running frame")
                    .pc = pc as u32;
                let depth = self.rt.stack.depth();
                Ok(match self.call_native_in_loop(fa, n, want, id, depth)? {
                    NativeStep::Returned => Frames::SameAllocated,
                    NativeStep::Resumed => Frames::SameResumed,
                    NativeStep::Flow(flow) => Frames::Flow(flow),
                })
            }
            // Anything else (a hooked native, a `__call` handler) goes
            // through the general entry.
            _ => {
                self.rt
                    .stack
                    .frames
                    .last_mut()
                    .expect("the running frame")
                    .pc = pc as u32;
                let d = self.rt.stack.depth();
                Ok(match self.call_value(fa, n, want)? {
                    Flow::Reload if self.rt.stack.frames.len() == d => Frames::SameResumed,
                    flow => Frames::Flow(flow),
                })
            }
        }
    }

    /// Return `n` values from absolute register `src`. Into a Lua caller
    /// the frame state is handed to the loop; the caller's window lies
    /// below this frame's, so the stack is already long enough for its
    /// results. Anything else is the general return.
    #[inline(always)]
    fn return_from_loop(&mut self, src: usize, n: usize) -> Result<Frames, Exc> {
        let Some((caller, f)) = self.lua_caller() else {
            return Ok(Frames::Flow(self.do_return(src, n)?));
        };
        let (func, want) = (f.func as usize, f.want);
        self.pop_frame(&f);
        let regs = &mut self.rt.stack.regs;
        if want == MULTI {
            regs.copy_within(src..src + n, func);
            self.rt.top = func + n;
        } else if n == 0 {
            let want = want as usize;
            regs[func..func + want].fill(Value::Nil);
            self.rt.top = func + want;
        } else {
            let want = want as usize;
            let k = n.min(want);
            regs.copy_within(src..src + k, func);
            regs[func + k..func + want].fill(Value::Nil);
            self.rt.top = func + want;
        }
        Ok(Frames::Lua(caller))
    }

    /// The running frame's caller when it is a Lua frame, as the loop's
    /// state for it, with the running frame.
    #[inline(always)]
    fn lua_caller(&self) -> Option<(LuaFrame, Frame)> {
        if let [.., caller, f] = &self.rt.stack.frames[..]
            && let FrameKind::Lua { closure, code_id } = caller.kind
        {
            Some((
                LuaFrame {
                    closure,
                    base: caller.base as usize,
                    pc: caller.pc as usize,
                    code_id,
                },
                *f,
            ))
        } else {
            None
        }
    }

    /// Pop the running frame `f` and its cells.
    #[inline(always)]
    fn pop_frame(&mut self, f: &Frame) {
        self.rt.stack.frames.pop();
        let own_cells = f.cell_base as usize;
        if self.rt.stack.cells.len() > own_cells {
            self.rt.stack.cells.truncate(own_cells);
        }
    }

    /// The instructions the loop runs out of line: those seldom executed
    /// (on the benchmark set, under 0.1% of all instructions), and those
    /// that leave the loop anyway to allocate or call. `pc` is the
    /// instruction after `op`; the result says where to go on.
    #[inline(never)]
    fn cold_op(
        &mut self,
        op: Op,
        cur: &ProtoCode,
        closure: Gc<Closure>,
        base: usize,
        pc: usize,
    ) -> Result<Cold, Exc> {
        let frame = *self.rt.stack.running();
        let cell_base = frame.cell_base as usize;
        let nvarargs = frame.nvarargs as usize;
        // A register of the running frame, by number.
        macro_rules! reg {
            ($r:expr) => {
                self.rt.stack.regs[base + $r as usize]
            };
        }
        macro_rules! arith_k {
            ($dst:expr, $x:expr, $y:expr, $op:expr, $nop:expr) => {{
                let (x, y): (Value, Value) = ($x, $y);
                reg!($dst) = match (x, y) {
                    (Value::Int(i), Value::Int(j)) => {
                        number_value(Number::arith(Number::Int(i), $nop, Number::Int(j)))?
                    }
                    (Value::Float(f), Value::Float(g)) => {
                        number_value(Number::arith(Number::Float(f), $nop, Number::Float(g)))?
                    }
                    (Value::Int(i), Value::Float(g)) => {
                        number_value(Number::arith(Number::Int(i), $nop, Number::Float(g)))?
                    }
                    (Value::Float(f), Value::Int(j)) => {
                        number_value(Number::arith(Number::Float(f), $nop, Number::Int(j)))?
                    }
                    _ => ops::arith_slow(&self.heap, &self.rt.metas, $op, x, y)?,
                };
            }};
        }
        let mut next = pc;
        let mut safepoint = false;
        match op {
            Op::LoadConstX { dst, k } => reg!(dst) = cur.consts[k as usize],
            Op::CellClear { from, count } => {
                let at = cell_base + from as usize;
                let dead = self.rt.dead_cell;
                self.rt.stack.cells[at..at + count as usize].fill(dead);
            }
            Op::CheckUndefined { src, name } => {
                if !matches!(reg!(src), Value::Nil) {
                    let k = const_str(&cur.consts, name);
                    return Err(already_defined(self.heap.get(k).as_bytes()));
                }
            }
            Op::SetGlobal { src, name, ic } => {
                let env_cell = self.heap.get(closure).captures[0];
                let v = reg!(src);
                match self.heap.get(env_cell).value {
                    Value::Table(t) => {
                        let cache = &cur.caches[ic as usize];
                        let slot = cache.slot.get() as usize;
                        let tab = self.heap.get(t);
                        if matches!(tab.cached_str_packed(slot, cache.key),
                            Some((tag, _)) if tab.meta.is_none() || tag != Tag::Nil)
                        {
                            barrier_for(&mut self.heap, t, v);
                            self.heap.get_mut(t).set_slot(slot, v);
                        } else {
                            let k = const_str(&cur.consts, name);
                            set_str_miss(&mut self.heap, &self.rt.metas, t, k, v, cache)?;
                            safepoint = true;
                        }
                    }
                    other => return Err(env_error(&self.heap, &self.rt.metas, other)),
                }
            }

            Op::Pow { dst, a, b } => arith_k!(dst, reg!(a), reg!(b), Arith::Pow, ArithOp::Pow),
            Op::IDiv { dst, a, b } => {
                arith_k!(dst, reg!(a), reg!(b), Arith::IDiv, ArithOp::IDiv)
            }
            Op::AddK { dst, a, k } => {
                arith_k!(
                    dst,
                    reg!(a),
                    cur.consts[k as usize],
                    Arith::Add,
                    ArithOp::Add
                )
            }
            Op::SubK { dst, a, k } => {
                arith_k!(
                    dst,
                    reg!(a),
                    cur.consts[k as usize],
                    Arith::Sub,
                    ArithOp::Sub
                )
            }
            Op::DivK { dst, a, k } => {
                arith_k!(
                    dst,
                    reg!(a),
                    cur.consts[k as usize],
                    Arith::Div,
                    ArithOp::Div
                )
            }
            Op::IDivK { dst, a, k } => {
                arith_k!(
                    dst,
                    reg!(a),
                    cur.consts[k as usize],
                    Arith::IDiv,
                    ArithOp::IDiv
                )
            }
            Op::PowK { dst, a, k } => {
                arith_k!(
                    dst,
                    reg!(a),
                    cur.consts[k as usize],
                    Arith::Pow,
                    ArithOp::Pow
                )
            }
            Op::KSub { dst, k, b } => {
                arith_k!(
                    dst,
                    cur.consts[k as usize],
                    reg!(b),
                    Arith::Sub,
                    ArithOp::Sub
                )
            }
            Op::BNot { dst, src } => {
                reg!(dst) = match reg!(src) {
                    Value::Int(i) => Value::Int(!i),
                    x => ops::bnot_slow(&self.heap, &self.rt.metas, x)?,
                }
            }
            Op::Neg { dst, src } => {
                reg!(dst) = match reg!(src) {
                    Value::Int(i) => Value::Int(i.wrapping_neg()),
                    Value::Float(f) => Value::Float(-f),
                    x => ops::arith_slow(&self.heap, &self.rt.metas, Arith::Neg, x, x)?,
                }
            }
            Op::Cast { dst, src, ty, mode } => {
                let x = reg!(src);
                reg!(dst) = match x {
                    Value::Int(_) if mode == orbit_bytecode::cast::INT => x,
                    // Converting to a string allocates or calls `__tostring`.
                    x if mode & !orbit_bytecode::cast::SOFT & !orbit_bytecode::cast::NILABLE
                        == orbit_bytecode::cast::STRING =>
                    {
                        safepoint = true;
                        self.op_cast_string(x, mode)?
                    }
                    x => {
                        let t = reg!(ty);
                        op_cast(&self.heap, &self.rt.metas, x, t, mode)?
                    }
                };
            }

            Op::Vararg { dst, count } => {
                self.op_vararg(base, nvarargs, dst as usize, count);
            }
            Op::VarargTable { dst } => {
                let t = self.op_vararg_table(base, nvarargs);
                reg!(dst) = t;
                safepoint = true;
            }
            Op::VarargGet { dst, key } => {
                let k = reg!(key);
                let v = self.op_vararg_get(base, nvarargs, k);
                reg!(dst) = v;
            }
            Op::VarargGetInt { dst, index } => {
                let i = index as usize;
                // The extra arguments sit just below the frame's base.
                let v = if (1..=nvarargs).contains(&i) {
                    self.rt.stack.regs[base - nvarargs + i - 1]
                } else {
                    Value::Nil
                };
                reg!(dst) = v;
            }
            Op::VarargUnpack { dst, table, count } => {
                let t = reg!(table);
                self.op_vararg_unpack(base, t, dst as usize, count)?;
            }

            Op::ForPrepExclusive { base: b, exit } => {
                // Rarer than `ForPrep` (OrbitScript only): no fast path.
                if !self.op_for_prep(base + b as usize, true)? {
                    next = (pc as isize + exit as isize) as usize;
                }
            }

            Op::NativeEntry { id } => {
                return Ok(Cold::Flow(
                    self.native_entry(closure, crate::object::NativeId(id))?,
                ));
            }
            Op::Tbc { reg } => {
                if !reg!(reg).is_falsy() {
                    self.op_tbc(base + reg as usize, pc - 1)?;
                }
            }
            Op::Close { from } => {
                if self
                    .rt
                    .stack
                    .aux
                    .tbc
                    .last()
                    .is_some_and(|&t| t as usize >= base + from as usize)
                {
                    return Err(self.op_close());
                }
            }
            _ => unreachable!("every other instruction runs in the loop"),
        }
        Ok(Cold::Next {
            pc: next,
            safepoint,
        })
    }

    /// Call native `id` at stack index `fa` with `n` arguments from Lua code
    /// in the frame at `depth`, wanting `want` results, in place: no frame,
    /// so a plain return comes straight back to the loop. The caller saved
    /// its pc and tried the native's fast path.
    #[inline(always)]
    fn call_native_in_loop(
        &mut self,
        fa: usize,
        n: usize,
        want: u8,
        id: crate::object::NativeId,
        depth: usize,
    ) -> Result<NativeStep, Exc> {
        self.rt.top = fa + 1 + n;
        let nf = self.rt.natives[id.0 as usize].f;
        let mut ctx = NativeCtx {
            vm: self,
            base: fa + 1,
            nargs: n,
            id,
            upvals: None,
        };
        match nf(&mut ctx) {
            // Unless the native set a hook, which its return must already
            // report.
            Ok(NativeReturn::Return(k)) if !self.rt.stack.hook.armed() => {
                let top = self.rt.top;
                let src = top - k;
                let s = &mut self.rt.stack;
                let end = if want == 1 && k >= 1 {
                    s.regs[fa] = s.regs[src];
                    fa + 1
                } else if want == MULTI {
                    s.regs.copy_within(src..top, fa);
                    fa + k
                } else {
                    // The caller's window covers its results.
                    let want = want as usize;
                    let m = k.min(want);
                    s.regs.copy_within(src..src + m, fa);
                    s.regs[fa + m..fa + want].fill(Value::Nil);
                    fa + want
                };
                self.rt.top = end;
                // Clear the arguments and pushes, as `native_returned`
                // does.
                let stop = s.dirty_to.max(top).min(s.regs.len());
                if end < stop {
                    s.regs[end..stop].fill(Value::Nil);
                }
                s.dirty_to = end;
                Ok(NativeStep::Returned)
            }
            Ok(NativeReturn::Yield { nvals }) => {
                Ok(NativeStep::Flow(self.yield_(nvals, None, fa, want, id)?))
            }
            Ok(r) => {
                if let NativeReturn::Return(k) = r
                    && self.rt.stack.hook.wants(crate::vm::hook::RETURN)
                {
                    self.native_return_hook(fa, id, k).map_err(Exc::Lua)?;
                }
                match self.native_returned(r, None, fa, want, id)? {
                    Flow::Reload if self.rt.stack.frames.len() == depth => Ok(NativeStep::Resumed),
                    flow => Ok(NativeStep::Flow(flow)),
                }
            }
            Err(e) => Err(self.native_raised(id, fa, e)),
        }
    }
}

/// An arithmetic result as a value, or its error.
#[inline(always)]
fn number_value(r: Result<Number, crate::object::number::ArithError>) -> Result<Value, Exc> {
    match r {
        Ok(Number::Int(i)) => Ok(Value::Int(i)),
        Ok(Number::Float(f)) => Ok(Value::Float(f)),
        Err(e) => Err(Exc::Rt(e.message().into(), None)),
    }
}

/// Method `k` of table `t` for `obj:k(...)`, through the instruction's
/// caches at `ic` (see [`Op::SelfCall`]): in the table itself, or in the
/// `__index` table of its metatable (a class), or one level further up (a
/// superclass). `None` when the lookup has to go through `__index` the
/// general way.
#[inline(always)]
fn method_lookup(
    heap: &LuaHeap,
    metas: &Metas,
    cur: &ProtoCode,
    ic: u16,
    k: Gc<LuaStr>,
    t: Gc<Table>,
) -> Option<(Tag, Payload)> {
    let tab = heap.get(t);
    let cm = &cur.caches[ic as usize];
    // A method in the object itself (a module or a singleton object) hits
    // the cache; a class instance lacks the key, so the probe proves that.
    let m = match tab.cached_str_packed(cm.slot.get() as usize, cm.key) {
        Some(m) => m,
        None => match tab.find_str_key(heap, &cm.key_for(heap, k)) {
            Some(slot) => {
                cm.fill(slot);
                tab.slot_packed(slot)
            }
            None => (Tag::Nil, Payload::from_bits(0)),
        },
    };
    if m.0 != Tag::Nil {
        return Some(m);
    }
    let mt = tab.meta?;
    // The usual class layout: the metatable's `__index` is a table holding
    // the method.
    let ci = &cur.caches[ic as usize + 1];
    if let Some(c) = cached_table(heap.get(mt), ci)
        && let Some(m) = heap
            .get(c)
            .cached_str_packed(cm.slot.get() as usize, cm.key)
            .filter(|m| m.0 != Tag::Nil)
    {
        return Some(m);
    }
    method_lookup_up(heap, metas, cur, ic, k, t, mt)
}

/// The table in `cache`'s slot of `tab`, when the cache hits and the value
/// is a table. Read packed: a merged `Value` would leave the handle's upper
/// payload bytes to the compiler (see [`Table::cached_str_packed`]).
#[inline(always)]
fn cached_table(tab: &Table, cache: &FieldCache) -> Option<Gc<Table>> {
    match tab.cached_str_packed(cache.slot.get() as usize, cache.key) {
        Some((Tag::Table, p)) => Some(Gc::from_bits(p.bits())),
        _ => None,
    }
}

/// [`method_lookup`] past the first level: the class lacks the method and
/// its own metatable's `__index` table holds it, through the caches at
/// `ic + 2` and `ic + 3`; else fill the caches from a full lookup.
#[inline(always)]
fn method_lookup_up(
    heap: &LuaHeap,
    metas: &Metas,
    cur: &ProtoCode,
    ic: u16,
    k: Gc<LuaStr>,
    t: Gc<Table>,
    mt: Gc<Table>,
) -> Option<(Tag, Payload)> {
    let cm = &cur.caches[ic as usize];
    let ci = &cur.caches[ic as usize + 1];
    let up = cur
        .caches
        .get(ic as usize + 2)
        .zip(cur.caches.get(ic as usize + 3));
    if let Some(c) = cached_table(heap.get(mt), ci)
        && let ctab = heap.get(c)
        && let (Some((ci2, cm2)), Some(mt2)) = (up, ctab.meta)
        && ctab
            .find_str_key(heap, &cm.key_for(heap, k))
            .is_none_or(|s| ctab.slot_value(s).is_nil())
        && let Some(p) = cached_table(heap.get(mt2), ci2)
        && let Some(m) = heap
            .get(p)
            .cached_str_packed(cm2.slot.get() as usize, cm2.key)
            .filter(|m| m.0 != Tag::Nil)
    {
        return Some(m);
    }
    method_miss(heap, metas, Value::Table(t), mt, k, cm, ci, up)
        .ok()
        .map(Value::pack)
}
