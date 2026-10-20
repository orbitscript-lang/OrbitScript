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
use crate::object::{Closure, Gc, LuaHeap, LuaStr, ProtoCode, Table, Value};
use orbit_bytecode::insn::Opcode;
use orbit_bytecode::number::BitOp;
use orbit_bytecode::{MULTI, Op};

use crate::vm::cold::{
    already_defined, env_error, get_str_miss, method_miss, op_cast, op_len, set_str_miss,
};
use crate::vm::interp::{
    Flow, Ran, Shape, WINDOW, Window, const_str, enter_lua_frame_fast, enter_lua_frame_general,
};
use crate::vm::meta::Metas;
use crate::vm::native::{NativeCtx, NativeReturn};
use crate::vm::ops::{self, Arith, Exc};
use crate::vm::{FrameKind, LuaError, Vm};

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
            // The running frame's state. Lua-to-Lua calls and returns switch
            // it in place; everything else comes back through `'reload`.
            #[cfg(not(feature = "debugger"))]
            if self.rt.stack.hook.armed() != HOOKED {
                return Ok(Ran::Switch);
            }
            let frame = self.rt.stack.frames.last().expect("the running frame");
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
            if self.hooked_here(&cur, closure, self.rt.stack.frames.len()) != HOOKED {
                return Ok(Ran::Switch);
            }
            #[cfg(feature = "debugger")]
            if HOOKED {
                self.debug_reloaded(pc, self.rt.stack.frames.len());
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
                            && self.debug_switches(&cur, $closure, self.rt.stack.frames.len(), HOOKED)
                        {
                            self.rt.stack.frames.last_mut().expect("the running frame").pc = pc as u32;
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
            macro_rules! safepoint {
                () => {
                    if self.heap.should_step() {
                        self.rt.stack.frames.last_mut().expect("the running frame").pc = pc as u32;
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
            // Run `$body` with `$v` bound to `w[$src]`, reading a number
            // by parts. Arithmetic writes its result as a tag store and a
            // payload store, and a whole 16-byte load of that value soon
            // after would stall on store forwarding; with the variant known,
            // `$body` stores it by parts too.
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
            macro_rules! set_int_parts {
                ($t:expr, $i:expr, $src:expr) => {
                    by_parts!($src, |v| table::store_int(&mut self.heap, $t, $i, v))
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
                        self.rt.stack.frames.last_mut().expect("the running frame").pc = pc as u32;
                        self.rt.hooks_touched = true;
                        self.rt.stack.hook.mask |= crate::vm::hook::INTERRUPT;
                        return Ok(Ran::Switch);
                    }
                }};
            }
            macro_rules! arith {
                ($dst:expr, $a:expr, $b:expr, $op:expr, $nop:expr) => {
                    arith_v!($dst, w[$a], w[$b], $op, $nop)
                };
            }
            // `arith!` over two operand places, registers or constants.
            macro_rules! arith_v {
                ($dst:expr, $x:expr, $y:expr, $op:expr, $nop:expr) => {{
                    // `Number::arith` owns the integer/float rules; with a
                    // constant operator it inlines to the one case. Each arm
                    // stores its own result, so the common cases write a
                    // known tag and payload instead of copying a merged
                    // `Value` through the stack.
                    macro_rules! store {
                        ($p:expr, $q:expr) => {
                            match Number::arith($p, $nop, $q) {
                                Ok(Number::Int(i)) => w[$dst] = Value::Int(i),
                                Ok(Number::Float(f)) => w[$dst] = Value::Float(f),
                                Err(e) => throw!(Exc::Rt(e.message().into(), None)),
                            }
                        };
                    }
                    // One arm per operand pair, so each inlines with both
                    // subtypes known.
                    match (&$x, &$y) {
                        (&Value::Int(i), &Value::Int(j)) => store!(Number::Int(i), Number::Int(j)),
                        (&Value::Float(f), &Value::Float(g)) => {
                            store!(Number::Float(f), Number::Float(g))
                        }
                        (&Value::Int(i), &Value::Float(g)) => {
                            store!(Number::Int(i), Number::Float(g))
                        }
                        (&Value::Float(f), &Value::Int(j)) => {
                            store!(Number::Float(f), Number::Int(j))
                        }
                        _ => {
                            let (x, y) = ($x, $y);
                            w[$dst] = tri!(ops::arith_slow(&self.heap, &self.rt.metas, $op, x, y))
                        }
                    }
                }};
            }
            macro_rules! bitwise {
                ($dst:expr, $a:expr, $b:expr, $op:expr) => {{
                    match (&w[$a], &w[$b]) {
                        (&Value::Int(i), &Value::Int(j)) => w[$dst] = Value::Int($op.apply(i, j)),
                        _ => {
                            let (x, y) = (w[$a], w[$b]);
                            w[$dst] = tri!(ops::bitwise_slow(&self.heap, &self.rt.metas, $op, x, y))
                        }
                    }
                }};
            }
            macro_rules! compare_jump {
                ($a:expr, $b:expr, $off:expr, $want:expr, $slow:path, |$i:ident, $j:ident| $cmp:expr) => {{
                    let c = match (&w[$a], &w[$b]) {
                        (&Value::Int($i), &Value::Int($j)) => $cmp,
                        (&Value::Float($i), &Value::Float($j)) => $cmp,
                        _ => {
                            let (x, y) = (w[$a], w[$b]);
                            tri!($slow(&self.heap, &self.rt.metas, x, y))
                        }
                    };
                    if c == $want {
                        jump!($off);
                    }
                }};
            }
            // `R[a] == R[b]`, reading the operands by parts: a register
            // `LoadNil` just wrote holds only its tag, and a whole 16-byte
            // load of it would stall on store forwarding.
            macro_rules! eq_parts {
                ($a:expr, $b:expr) => {
                    match (&w[$a], &w[$b]) {
                        (&Value::Int(i), &Value::Int(j)) => i == j,
                        (&Value::Float(f), &Value::Float(g)) => f == g,
                        (&Value::Nil, &Value::Nil) => true,
                        (&Value::Nil, _) | (_, &Value::Nil) => false,
                        _ => {
                            let (x, y) = (w[$a], w[$b]);
                            tri!(values_equal(&self.heap, &self.rt.metas, x, y))
                        }
                    }
                };
            }
            macro_rules! compare_int_jump {
                // `$slow` names an `ops` comparison and its operand order,
                // with `x` the register and `k` the immediate as values.
                ($a:expr, $imm:expr, $want:expr, $off:expr, |$x:ident, $k:ident| $cmp:expr,
                 $slow:ident($s1:ident, $s2:ident)) => {{
                    let c = match w[$a] {
                        Value::Int($x) => {
                            let $k = $imm as i64;
                            $cmp
                        }
                        Value::Float($x) => {
                            let $k = $imm as f64;
                            $cmp
                        }
                        v => {
                            let ($x, $k) = (v, Value::Int($imm as i64));
                            tri!(ops::$slow(&self.heap, &self.rt.metas, $s1, $s2))
                        }
                    };
                    if c == $want {
                        jump!($off);
                    }
                }};
            }
            // `R[dst] = obj[key]` through the metatable, when the inline
            // path can't finish (a raw miss on a table with a metatable, or
            // a value that isn't a table). `raise` finishes the instruction,
            // so the dispatch loop carries no code for it.
            macro_rules! get_slow {
                ($obj:expr, $key:expr) => {{
                    throw!(Exc::Index {
                        obj: $obj,
                        key: $key
                    })
                }};
            }
            macro_rules! set_slow {
                ($obj:expr, $key:expr, $v:expr) => {{
                    throw!(Exc::NewIndex {
                        obj: $obj,
                        key: $key,
                        v: $v
                    })
                }};
            }
            // Something returned into the running frame from outside the
            // loop (a native, a metamethod): the native may have set or
            // cleared a hook, so check which loop runs on; then take the
            // window again and let the collector catch up.
            macro_rules! resumed_here {
                () => {{
                    if self.hooked_here(&cur, closure, self.rt.stack.frames.len()) != HOOKED {
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

                    // Enter the callee at register `f` with `n` arguments,
                    // `want` results back to the caller. A Lua callee's frame
                    // entry saves the pc; a native is called in place.
                    macro_rules! call {
                        ($f:expr, $n:expr, $want:expr) => {{
                            let (f, n, want): (usize, usize, u8) = ($f, $n, $want);
                            let fa = base + f;
                            match w[f] {
                                Value::Closure(c) => {
                                    // A function calling itself: the running
                                    // frame keeps `closure` alive, so an equal
                                    // handle is the same closure, and its code
                                    // is `cur`.
                                    let shape = if c == closure {
                                        Shape::of(&cur)
                                    } else {
                                        Shape::of(&self.heap.get(c).code)
                                    };
                                    let new_base = match enter_lua_frame_fast(
                                        &mut self.rt,
                                        &shape,
                                        c,
                                        fa,
                                        n,
                                        want,
                                        pc as u32,
                                    ) {
                                        Some(b) => b,
                                        None => {
                                            self.rt.stack.frames.last_mut().expect("the running frame").pc = pc as u32;
                                            tri!(enter_lua_frame_general(
                                                &mut self.rt,
                                                Shape::of(&self.heap.get(c).code),
                                                c,
                                                fa,
                                                n,
                                                want
                                            ))
                                            .base
                                        }
                                    };
                                    if HOOKED {
                                        self.rt.stack.hook.call_pending = Some(false);
                                    }
                                    closure = c;
                                    base = new_base;
                                    pc = 0;
                                    switch_code!(c, shape.code_id);
                                    w = Window::take(&mut self.rt.stack.regs, base);
                                }
                                // A native's fast path, when it has one and takes
                                // these arguments: no window, no frame.
                                Value::Native(id)
                                    if !HOOKED
                                        && (want <= 1 || want == MULTI)
                                        && let Some(fast) = self.rt.natives[id.0 as usize].fast
                                        && let Some(v) = w
                                            .get(f + 1..f + 1 + n)
                                            .and_then(|a| fast(&self.heap, a)) =>
                                {
                                    if want != 0 {
                                        w[f] = v;
                                    }
                                    self.rt.top = fa + want.min(1) as usize;
                                }
                                // Any other native, called in place: no frame, and
                                // a plain return comes straight back to this loop.
                                Value::Native(id) if !HOOKED => {
                                    self.rt.stack.frames.last_mut().expect("the running frame").pc = pc as u32;
                                    match self.call_native_in_loop(
                                        fa,
                                        n,
                                        want,
                                        id,
                                        self.rt.stack.frames.len(),
                                    ) {
                                        Ok(NativeStep::Returned) => {
                                            w = Window::take(&mut self.rt.stack.regs, base);
                                            safepoint!();
                                        }
                                        Ok(NativeStep::Resumed) => resumed_here!(),
                                        Ok(NativeStep::Flow(flow)) => flow!(Ok(flow)),
                                        Err(x) => throw!(x),
                                    }
                                }
                                // Anything else (a hooked native, a `__call`
                                // handler) goes through the general entry.
                                _ => {
                                    self.rt.stack.frames.last_mut().expect("the running frame").pc = pc as u32;
                                    let d = self.rt.stack.frames.len();
                                    match self.call_value(fa, n, want) {
                                        Ok(Flow::Reload) if self.rt.stack.frames.len() == d => {
                                            resumed_here!()
                                        }
                                        other => flow!(other),
                                    }
                                }
                            }
                        }};
                    }
                    // Return `n` values from absolute register `src`.
                    macro_rules! lua_return {
                        ($src:expr, $n:expr) => {{
                            let (src, n): (usize, usize) = ($src, $n);
                            if let [.., caller, f] = &self.rt.stack.frames[..]
                                && let FrameKind::Lua {
                                    closure: c,
                                    code_id,
                                } = caller.kind
                            {
                                let (func, own_cells, want) =
                                    (f.func as usize, f.cell_base as usize, f.want);
                                base = caller.base as usize;
                                pc = caller.pc as usize;
                                self.rt.stack.frames.pop();
                                if self.rt.stack.cells.len() > own_cells {
                                    self.rt.stack.cells.truncate(own_cells);
                                }
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
                                closure = c;
                                switch_code!(c, code_id);
                                w = Window::take(&mut self.rt.stack.regs, base);
                                continue;
                            }
                            flow!(self.do_return(src, n))
                        }};
                    }
                    let insn = code[pc];
                    pc += 1;
                    if HOOKED {
                        self.rt.stack.frames.last_mut().expect("the running frame").pc = pc as u32;
                        if let Err(e) = self.hook_point(pc - 1, insn.decode()) {
                            throw!(Exc::Lua(e));
                        }
                        #[cfg(feature = "debugger")]
                        if self.rt.debug.watch
                            && let Err(e) = self.debug_point(pc - 1)
                        {
                            throw!(Exc::Lua(e));
                        }
                        if !self.hooked_here(&cur, closure, self.rt.stack.frames.len()) {
                            // The hook was turned off: run this instruction in
                            // the plain loop.
                            self.rt.stack.frames.last_mut().expect("the running frame").pc = (pc - 1) as u32;
                            return Ok(Ran::Switch);
                        }
                        w = Window::take(&mut self.rt.stack.regs, base);
                    }
                    match insn.op() {
                        // Numbers are copied by parts: the source was usually
                        // just written that way, and a whole 16-byte load would
                        // stall on store forwarding.
                        Opcode::Move => {
                            let (dst, src) = (insn.a(), insn.b());
                            match w[src] {
                                Value::Int(x) => w[dst] = Value::Int(x),
                                Value::Float(f) => w[dst] = Value::Float(f),
                                Value::Table(t) => w[dst] = Value::Table(t),
                                Value::Str(x) => w[dst] = Value::Str(x),
                                Value::Closure(c) => w[dst] = Value::Closure(c),
                                Value::Bool(b) => w[dst] = Value::Bool(b),
                                v => w[dst] = v,
                            }
                        }
                        Opcode::LoadNil => {
                            let (dst, count) = (insn.a(), insn.b());
                            let at = dst as usize;
                            for r in &mut w[at..at + count as usize] {
                                *r = Value::Nil;
                            }
                        }
                        Opcode::LoadBool => {
                            let (dst, value) = (insn.a(), insn.b() != 0);
                            w[dst] = Value::Bool(value)
                        }
                        Opcode::LoadInt => {
                            let (dst, value) = (insn.a(), insn.sd());
                            w[dst] = Value::Int(value as i64)
                        }
                        Opcode::LoadConst => {
                            let (dst, k) = (insn.a(), insn.dlo());
                            w[dst] = cur.consts[k as usize]
                        }

                        Opcode::CellGet => {
                            let (dst, cell) = (insn.a(), insn.b());
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
                        Opcode::CapGet => {
                            let (dst, cap) = (insn.a(), insn.b());
                            let c = self.heap.get(closure).captures[cap as usize];
                            w[dst] = self.heap.get(c).value;
                        }
                        Opcode::CapSelf => {
                            let dst = insn.a();
                            w[dst] = Value::Closure(closure)
                        }
                        Opcode::CapSet => {
                            let (cap, src) = (insn.a(), insn.b());
                            let c = self.heap.get(closure).captures[cap as usize];
                            self.heap.barrier_back(c);
                            let cell = self.heap.get_mut(c);
                            by_parts!(src, |v| cell.value = v);
                        }

                        Opcode::GetGlobal => {
                            let (dst, name, ic) = (insn.a(), insn.dlo(), insn.dhi());
                            let env_cell = self.heap.get(closure).captures[0];
                            let v = match self.heap.get(env_cell).value {
                                Value::Table(t) => {
                                    let cache = &cur.caches[ic as usize];
                                    let tab = self.heap.get(t);
                                    match tab.cached_str(cache.slot.get() as usize, cache.key) {
                                        // A dead entry is a miss when `__index`
                                        // could apply.
                                        Some(v) if !v.is_nil() || tab.meta.is_none() => v,
                                        _ => {
                                            let k = const_str(&cur.consts, name);
                                            tri!(get_str_miss(
                                                &self.heap,
                                                &self.rt.metas,
                                                t,
                                                k,
                                                cache
                                            ))
                                        }
                                    }
                                }
                                other => throw!(env_error(&self.heap, &self.rt.metas, other)),
                            };
                            w[dst] = v;
                        }

                        Opcode::GetField => {
                            let (dst, obj, key, ic) = (insn.a(), insn.b(), insn.dlo(), insn.dhi());
                            let &Value::Table(t) = &w[obj] else {
                                get_slow!(w[obj], cur.consts[key as usize])
                            };
                            let cache = &cur.caches[ic as usize];
                            let tab = self.heap.get(t);
                            match tab.cached_str(cache.slot.get() as usize, cache.key) {
                                // A dead entry is a miss when `__index` could
                                // apply.
                                Some(v) if !v.is_nil() || tab.meta.is_none() => w[dst] = v,
                                _ => {
                                    let k = const_str(&cur.consts, key);
                                    w[dst] =
                                        tri!(get_str_miss(&self.heap, &self.rt.metas, t, k, cache));
                                }
                            }
                        }
                        Opcode::SetField => {
                            let (obj, key, src, ic) = (insn.a(), insn.dlo(), insn.b(), insn.dhi());
                            let &Value::Table(t) = &w[obj] else {
                                set_slow!(w[obj], cur.consts[key as usize], w[src])
                            };
                            let cache = &cur.caches[ic as usize];
                            let slot = cache.slot.get() as usize;
                            let tab = self.heap.get(t);
                            if matches!(tab.cached_str(slot, cache.key),
                            Some(v) if tab.meta.is_none() || !v.is_nil())
                            {
                                // The key is there: overwrite its value. No
                                // capacity changes, so no byte accounting.
                                self.heap.barrier_back(t);
                                let tab = self.heap.get_mut(t);
                                by_parts!(src, |v| tab.set_slot(slot, v));
                            } else {
                                let k = const_str(&cur.consts, key);
                                let v = w[src];
                                tri!(set_str_miss(&mut self.heap, &self.rt.metas, t, k, v, cache));
                                safepoint!();
                            }
                        }
                        Opcode::GetIndex => {
                            let (dst, obj, key) = (insn.a(), insn.b(), insn.c());
                            let &Value::Table(t) = &w[obj] else {
                                get_slow!(w[obj], w[key])
                            };
                            let tab = self.heap.get(t);
                            // A nil result in a table with a metatable may
                            // come from `__index`. `dst` may be `obj` or `key`.
                            match w[key] {
                                Value::Int(i) => {
                                    if !tab.read_int_present(i, &mut w[dst]) {
                                        let v = tab.get_int(i);
                                        if v.is_nil() && tab.meta.is_some() {
                                            get_slow!(Value::Table(t), Value::Int(i))
                                        }
                                        w[dst] = v;
                                    }
                                }
                                k => {
                                    w[dst] = tab.get(&self.heap, k);
                                    if tab.meta.is_some() && w[dst].is_nil() {
                                        get_slow!(Value::Table(t), k)
                                    }
                                }
                            }
                        }
                        Opcode::SetIndex => {
                            let (obj, key, src) = (insn.a(), insn.b(), insn.c());
                            let &Value::Table(t) = &w[obj] else {
                                set_slow!(w[obj], w[key], w[src])
                            };
                            match w[key] {
                                // A store into the array part allocates
                                // nothing, so it needs no safe point.
                                Value::Int(i) => match set_int_parts!(t, i, src) {
                                    IntStore::Array => {}
                                    IntStore::Other => safepoint!(),
                                    IntStore::Slow => {
                                        set_slow!(w[obj], w[key], w[src])
                                    }
                                },
                                k => {
                                    let v = w[src];
                                    if self.heap.get(t).meta.is_some() {
                                        set_slow!(w[obj], k, v)
                                    }
                                    if let Err(e) = table::set(&mut self.heap, t, k, v) {
                                        throw!(ops::store_failed(e, t, k, v));
                                    }
                                    safepoint!();
                                }
                            }
                        }
                        Opcode::GetIndexInt => {
                            let (dst, obj, key) = (insn.a(), insn.b(), insn.dlo());
                            let &Value::Table(t) = &w[obj] else {
                                get_slow!(w[obj], Value::Int(key as i64))
                            };
                            let tab = self.heap.get(t);
                            if !tab.read_int_present(key as i64, &mut w[dst]) {
                                let v = tab.get_int(key as i64);
                                if v.is_nil() && tab.meta.is_some() {
                                    get_slow!(Value::Table(t), Value::Int(key as i64))
                                }
                                w[dst] = v;
                            }
                        }
                        Opcode::SetIndexInt => {
                            let (obj, key, src) = (insn.a(), insn.dlo(), insn.b());
                            let &Value::Table(t) = &w[obj] else {
                                set_slow!(w[obj], Value::Int(key as i64), w[src])
                            };
                            match set_int_parts!(t, key as i64, src) {
                                IntStore::Array => {}
                                IntStore::Other => safepoint!(),
                                IntStore::Slow => {
                                    set_slow!(w[obj], Value::Int(key as i64), w[src])
                                }
                            }
                        }
                        Opcode::SelfCall => {
                            let (dst, obj, key, ic) = (insn.a(), insn.b(), insn.dlo(), insn.dhi());
                            let Value::Str(k) = cur.consts[key as usize] else {
                                unreachable!("method names are strings")
                            };
                            // Copied by parts: a whole-value copy also moves the
                            // padding bytes, through overlapping stack slots.
                            let t = match w[obj] {
                                Value::Table(t) => t,
                                o => {
                                    w[dst + 1] = o;
                                    get_slow!(o, Value::Str(k))
                                }
                            };
                            w[dst + 1] = Value::Table(t);
                            match method_lookup(&self.heap, &self.rt.metas, &cur, ic, k, t) {
                                Some(Value::Closure(c)) => w[dst] = Value::Closure(c),
                                Some(m) => w[dst] = m,
                                None => get_slow!(Value::Table(t), Value::Str(k)),
                            }
                        }

                        Opcode::Add => {
                            let (dst, a, b) = (insn.a(), insn.b(), insn.c());
                            arith!(dst, a, b, Arith::Add, ArithOp::Add)
                        }
                        Opcode::Sub => {
                            let (dst, a, b) = (insn.a(), insn.b(), insn.c());
                            arith!(dst, a, b, Arith::Sub, ArithOp::Sub)
                        }
                        Opcode::Mul => {
                            let (dst, a, b) = (insn.a(), insn.b(), insn.c());
                            arith!(dst, a, b, Arith::Mul, ArithOp::Mul)
                        }
                        Opcode::Div => {
                            let (dst, a, b) = (insn.a(), insn.b(), insn.c());
                            arith!(dst, a, b, Arith::Div, ArithOp::Div)
                        }
                        Opcode::Mod => {
                            let (dst, a, b) = (insn.a(), insn.b(), insn.c());
                            arith!(dst, a, b, Arith::Mod, ArithOp::Mod)
                        }
                        Opcode::MulK => {
                            let (dst, a, k) = (insn.a(), insn.b(), insn.dlo());
                            arith_v!(dst, w[a], cur.consts[k as usize], Arith::Mul, ArithOp::Mul)
                        }
                        Opcode::ModK => {
                            let (dst, a, k) = (insn.a(), insn.b(), insn.dlo());
                            arith_v!(dst, w[a], cur.consts[k as usize], Arith::Mod, ArithOp::Mod)
                        }
                        Opcode::KAdd => {
                            let (dst, k, b) = (insn.a(), insn.dlo(), insn.b());
                            arith_v!(dst, cur.consts[k as usize], w[b], Arith::Add, ArithOp::Add)
                        }
                        Opcode::KMul => {
                            let (dst, k, b) = (insn.a(), insn.dlo(), insn.b());
                            arith_v!(dst, cur.consts[k as usize], w[b], Arith::Mul, ArithOp::Mul)
                        }
                        Opcode::KDiv => {
                            let (dst, k, b) = (insn.a(), insn.dlo(), insn.b());
                            arith_v!(dst, cur.consts[k as usize], w[b], Arith::Div, ArithOp::Div)
                        }
                        Opcode::AddInt => {
                            let (dst, a, imm) = (insn.a(), insn.b(), insn.dlo() as i16);
                            match w[a] {
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
                            }
                        }
                        Opcode::SubInt => {
                            let (dst, a, imm) = (insn.a(), insn.b(), insn.dlo() as i16);
                            match w[a] {
                                Value::Int(i) => w[dst] = Value::Int(i.wrapping_sub(imm as i64)),
                                // Adding the negated integer, as lua5.5 does: `-0.0 - 0` is `0.0` there.
                                Value::Float(f) => {
                                    w[dst] = Value::Float(f + (-(imm as i64)) as f64)
                                }
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
                            }
                        }
                        Opcode::BAnd => {
                            let (dst, a, b) = (insn.a(), insn.b(), insn.c());
                            bitwise!(dst, a, b, BitOp::And)
                        }
                        Opcode::BOr => {
                            let (dst, a, b) = (insn.a(), insn.b(), insn.c());
                            bitwise!(dst, a, b, BitOp::Or)
                        }
                        Opcode::BXor => {
                            let (dst, a, b) = (insn.a(), insn.b(), insn.c());
                            bitwise!(dst, a, b, BitOp::Xor)
                        }
                        Opcode::Shl => {
                            let (dst, a, b) = (insn.a(), insn.b(), insn.c());
                            bitwise!(dst, a, b, BitOp::Shl)
                        }
                        Opcode::Shr => {
                            let (dst, a, b) = (insn.a(), insn.b(), insn.c());
                            bitwise!(dst, a, b, BitOp::Shr)
                        }
                        Opcode::Not => {
                            let (dst, src) = (insn.a(), insn.b());
                            w[dst] = Value::Bool(w[src].is_falsy())
                        }
                        Opcode::Len => {
                            let (dst, src) = (insn.a(), insn.b());
                            let x = w[src];
                            w[dst] = tri!(op_len(&self.heap, &self.rt.metas, x));
                        }

                        Opcode::Jump => {
                            let off = insn.sd();
                            jump!(off)
                        }
                        Opcode::JumpIfTrue => {
                            let (src, off) = (insn.a(), insn.sd());
                            if !w[src].is_falsy() {
                                jump!(off);
                            }
                        }
                        Opcode::JumpIfFalse => {
                            let (src, off) = (insn.a(), insn.sd());
                            if w[src].is_falsy() {
                                jump!(off);
                            }
                        }
                        Opcode::JumpLt => {
                            let (a, b, off) = (insn.a(), insn.b(), insn.sd());
                            compare_jump!(a, b, off, true, ops::less_than, |x, y| x < y)
                        }
                        Opcode::JumpNotLt => {
                            let (a, b, off) = (insn.a(), insn.b(), insn.sd());
                            compare_jump!(a, b, off, false, ops::less_than, |x, y| x < y)
                        }
                        Opcode::JumpLe => {
                            let (a, b, off) = (insn.a(), insn.b(), insn.sd());
                            compare_jump!(a, b, off, true, ops::less_equal, |x, y| x <= y)
                        }
                        Opcode::JumpNotLe => {
                            let (a, b, off) = (insn.a(), insn.b(), insn.sd());
                            compare_jump!(a, b, off, false, ops::less_equal, |x, y| x <= y)
                        }
                        Opcode::JumpEq => {
                            let (a, b, off) = (insn.a(), insn.b(), insn.sd());
                            if eq_parts!(a, b) {
                                jump!(off);
                            }
                        }
                        Opcode::JumpNe => {
                            let (a, b, off) = (insn.a(), insn.b(), insn.sd());
                            if !eq_parts!(a, b) {
                                jump!(off);
                            }
                        }
                        Opcode::JumpEqInt => {
                            let (a, imm, want, off) =
                                (insn.a(), insn.b() as i8, insn.c() != 0, insn.sd());
                            let c = match w[a] {
                                Value::Int(i) => i == imm as i64,
                                Value::Float(f) => f == imm as f64,
                                _ => false,
                            };
                            if c == want {
                                jump!(off);
                            }
                        }
                        Opcode::JumpLtInt => {
                            let (a, imm, want, off) =
                                (insn.a(), insn.b() as i8, insn.c() != 0, insn.sd());
                            compare_int_jump!(a, imm, want, off, |x, k| x < k, less_than(x, k))
                        }
                        Opcode::JumpLeInt => {
                            let (a, imm, want, off) =
                                (insn.a(), insn.b() as i8, insn.c() != 0, insn.sd());
                            compare_int_jump!(a, imm, want, off, |x, k| x <= k, less_equal(x, k))
                        }
                        Opcode::JumpGtInt => {
                            let (a, imm, want, off) =
                                (insn.a(), insn.b() as i8, insn.c() != 0, insn.sd());
                            compare_int_jump!(a, imm, want, off, |x, k| x > k, less_than(k, x))
                        }
                        Opcode::JumpGeInt => {
                            let (a, imm, want, off) =
                                (insn.a(), insn.b() as i8, insn.c() != 0, insn.sd());
                            compare_int_jump!(a, imm, want, off, |x, k| x >= k, less_equal(k, x))
                        }

                        // Calls. Each arm works out the callee's register, its
                        // argument count and the results the caller takes;
                        // `call!` enters the callee.
                        Opcode::Call => {
                            let (func, nargs, nresults) = (insn.a(), insn.b(), insn.c());
                            let n = if nargs == MULTI {
                                self.rt.top - (base + func as usize) - 1
                            } else {
                                nargs as usize
                            };
                            call!(func as usize, n, nresults);
                        }
                        Opcode::GenericCall => {
                            let (b, nvars) = (insn.a(), insn.b());
                            let at = b as usize;
                            w.copy_within(at..at + 3, at + 4);
                            call!(at + 4, 2, nvars);
                        }
                        Opcode::TailCall => {
                            let (func, nargs) = (insn.a(), insn.b());
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
                                && !matches!(
                                    code.code.first().map(|i| i.decode()),
                                    Some(Op::NativeEntry { .. })
                                )
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
                            call!(func as usize, n, MULTI);
                        }

                        // Returns. Back into a Lua caller `lua_return!` switches
                        // the frame state in place; the caller's window lies
                        // below this frame's, so the stack is already long
                        // enough for its results and its window.
                        Opcode::Return => {
                            let (first, count) = (insn.a(), insn.b());
                            let src = base + first as usize;
                            let n = if count == MULTI {
                                self.rt.top - src
                            } else {
                                count as usize
                            };
                            lua_return!(src, n);
                        }
                        Opcode::Return0 => {
                            lua_return!(base, 0)
                        }
                        Opcode::Return1 => {
                            let src = insn.a();
                            // The common case, one value back to a Lua caller
                            // that wants one, without the general return path.
                            if let [.., caller, f] = &self.rt.stack.frames[..]
                                && f.want == 1
                                && let FrameKind::Lua {
                                    closure: c,
                                    code_id,
                                } = caller.kind
                            {
                                let (func, own_cells) = (f.func as usize, f.cell_base as usize);
                                base = caller.base as usize;
                                pc = caller.pc as usize;
                                self.rt.stack.frames.pop();
                                if self.rt.stack.cells.len() > own_cells {
                                    self.rt.stack.cells.truncate(own_cells);
                                }
                                // Read from the window and copied by parts: the
                                // value was usually just written that way, and
                                // a whole 16-byte load would stall on store
                                // forwarding.
                                by_parts!(src, |v| self.rt.stack.regs[func] = v);
                                self.rt.top = func + 1;
                                closure = c;
                                switch_code!(c, code_id);
                                w = Window::take(&mut self.rt.stack.regs, base);
                                continue;
                            }
                            lua_return!(base + src as usize, 1)
                        }

                        // Allocating instructions call out anyway; they stay
                        // in the loop so the call is all they pay.
                        Opcode::NewTable => {
                            let (dst, array_hint, hash_hint) = (insn.a(), insn.dlo(), insn.dhi());
                            let t = self.op_new_table(array_hint as usize, hash_hint as usize);
                            w = Window::take(&mut self.rt.stack.regs, base);
                            w[dst] = Value::Table(t);
                            safepoint!();
                        }
                        Opcode::SetList => {
                            let (tr, first, count, start) =
                                (insn.a(), insn.b(), insn.c(), insn.d());
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
                        Opcode::Concat => {
                            let (dst, first, count) = (insn.a(), insn.b(), insn.c());
                            let v = tri!(self.op_concat(base + first as usize, count as usize));
                            w = Window::take(&mut self.rt.stack.regs, base);
                            w[dst] = v;
                            safepoint!();
                        }
                        Opcode::Closure => {
                            let (dst, proto) = (insn.a(), insn.dlo());
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
                        Opcode::CellNew => {
                            let (cell, src) = (insn.a(), insn.b());
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
                        Opcode::CellSet => {
                            let (cell, src) = (insn.a(), insn.b());
                            let cell_base = self
                                .rt
                                .stack
                                .frames
                                .last()
                                .expect("the running frame")
                                .cell_base as usize;
                            let c = self.rt.stack.cells[cell_base + cell as usize];
                            let v = w[src];
                            self.heap.barrier_back(c);
                            self.heap.get_mut(c).value = v;
                        }

                        Opcode::VarargLen => {
                            let dst = insn.a();
                            let nvarargs = self
                                .rt
                                .stack
                                .frames
                                .last()
                                .expect("the running frame")
                                .nvarargs;
                            w[dst] = Value::Int(nvarargs as i64);
                        }

                        Opcode::ForPrep => {
                            let (b, exit) = (insn.a(), insn.sd());
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
                        Opcode::ForLoop => {
                            let (b, back) = (insn.a(), insn.sd());
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
                        Opcode::GenericLoop => {
                            let (b, back) = (insn.a(), insn.sd());
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
                        // The verifier admits no opcode byte past the last
                        // opcode, so a linked chunk never holds one.
                        Opcode::Invalid => unreachable!("an invalid opcode"),
                        Opcode::LoadConstX
                        | Opcode::CellClear
                        | Opcode::CheckUndefined
                        | Opcode::SetGlobal
                        | Opcode::Pow
                        | Opcode::IDiv
                        | Opcode::AddK
                        | Opcode::SubK
                        | Opcode::DivK
                        | Opcode::IDivK
                        | Opcode::PowK
                        | Opcode::KSub
                        | Opcode::BNot
                        | Opcode::Neg
                        | Opcode::Cast
                        | Opcode::Vararg
                        | Opcode::VarargTable
                        | Opcode::VarargGet
                        | Opcode::VarargGetInt
                        | Opcode::VarargUnpack
                        | Opcode::ForPrepExclusive
                        | Opcode::NativeEntry
                        | Opcode::Tbc
                        | Opcode::Close => {
                            match self.cold_op(insn.decode(), &cur, closure, base, pc) {
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
                            }
                        }
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
                            let dst = match code[pc - 1].decode() {
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
                self.rt.stack.frames.last_mut().expect("the running frame").pc = pc as u32;
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
        let frame = *self.rt.stack.frames.last().expect("the running frame");
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
                        if matches!(tab.cached_str(slot, cache.key),
                            Some(v) if tab.meta.is_none() || !v.is_nil())
                        {
                            self.heap.barrier_back(t);
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
) -> Option<Value> {
    let tab = heap.get(t);
    let cm = &cur.caches[ic as usize];
    // A method in the object itself (a module or a singleton object) hits
    // the cache; a class instance lacks the key, so the probe proves that.
    let m = match tab.cached_str(cm.slot.get() as usize, cm.key) {
        Some(v) => v,
        None => match tab.find_str_key(heap, &cm.key_for(heap, k)) {
            Some(slot) => {
                cm.fill(slot);
                tab.slot_value(slot)
            }
            None => Value::Nil,
        },
    };
    if !m.is_nil() {
        return Some(m);
    }
    let mt = tab.meta?;
    // The usual class layout: the metatable's `__index` is a table holding
    // the method.
    let ci = &cur.caches[ic as usize + 1];
    if let Some(Value::Table(c)) = heap.get(mt).cached_str(ci.slot.get() as usize, ci.key)
        && let Some(v) = heap
            .get(c)
            .cached_str(cm.slot.get() as usize, cm.key)
            .filter(|v| !v.is_nil())
    {
        return Some(v);
    }
    method_lookup_up(heap, metas, cur, ic, k, t, mt)
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
) -> Option<Value> {
    let cm = &cur.caches[ic as usize];
    let ci = &cur.caches[ic as usize + 1];
    let up = cur
        .caches
        .get(ic as usize + 2)
        .zip(cur.caches.get(ic as usize + 3));
    if let Some(Value::Table(c)) = heap.get(mt).cached_str(ci.slot.get() as usize, ci.key)
        && let ctab = heap.get(c)
        && let (Some((ci2, cm2)), Some(mt2)) = (up, ctab.meta)
        && ctab
            .find_str_key(heap, &cm.key_for(heap, k))
            .is_none_or(|s| ctab.slot_value(s).is_nil())
        && let Some(Value::Table(p)) = heap.get(mt2).cached_str(ci2.slot.get() as usize, ci2.key)
        && let Some(v) = heap
            .get(p)
            .cached_str(cm2.slot.get() as usize, cm2.key)
            .filter(|v| !v.is_nil())
    {
        return Some(v);
    }
    method_miss(heap, metas, Value::Table(t), mt, k, cm, ci, up).ok()
}

#[inline(always)]
fn values_equal(heap: &LuaHeap, rt: &Metas, x: Value, y: Value) -> Result<bool, Exc> {
    match (x, y) {
        (Value::Int(i), Value::Int(j)) => Ok(i == j),
        (Value::Float(f), Value::Float(g)) => Ok(f == g),
        _ => ops::equal(heap, rt, x, y),
    }
}
