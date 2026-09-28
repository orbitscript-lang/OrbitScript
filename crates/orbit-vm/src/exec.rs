//! The interpreter: calls and returns (`ldo.c`), the dispatch loop (`lvm.c`), and error
//! recovery at protected calls.
//!
//! The loop is stackless (ADR-3): a Lua function calling a Lua function, or an
//! instruction calling a Lua metamethod, pushes a frame and keeps going in the same
//! Rust frame. Native functions run on the Rust stack; they can still call back into Lua
//! without blocking yields by returning [`Action::Call`] or [`Action::PCall`] instead of
//! calling [`Vm::call`].

use crate::debug::Operand;
use crate::error::{Error, LuaResult, Status};
use crate::meta::{Get, Set};
use crate::number::{self, Arith, F2I};
use crate::state::{Action, Frame, KStatus, MIN_STACK, Vm, flag};
use orbit_bytecode::*;
use orbit_core::object::LuaClosure;
use orbit_core::{Tm, Value};

/// What the frame stack needs next.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Flow {
    /// The top frame is a Lua frame ready to run.
    RunLua,
    /// The top frame returned; its caller continues.
    Returned,
    /// The running coroutine yields.
    Yield,
}

/// How a run of the loop ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Exit {
    /// The frame the run started with returned.
    Returned,
    /// The coroutine yielded.
    Yielded,
}

const MAX_CALL_TM: usize = 15;

impl Vm {
    // ---- calls ----------------------------------------------------------------

    /// Replaces a non-function at `func` by its `__call` metamethod, shifting the
    /// arguments up (`tryfuncTM`). Returns the callable.
    fn resolve_callable(&mut self, func: usize) -> LuaResult<Value> {
        let mut f = self.st.stack[func];
        let mut n = 0;
        while !f.is_function() {
            let tm = self.heap.tm_of(f, Tm::Call);
            if tm.is_nil() {
                return Err(self.call_error(func));
            }
            n += 1;
            if n > MAX_CALL_TM {
                return Err(self.rt_error("'__call' chain too long"));
            }
            let top = self.st.top;
            self.ensure_stack(top + 1);
            self.st.stack.copy_within(func..top, func + 1);
            self.st.top = top + 1;
            self.st.stack[func] = tm;
            f = tm;
        }
        Ok(f)
    }

    /// Starts a call of the function at `func` with the arguments above it up to the
    /// top (`luaD_precall`). A Lua function gets a frame and `Flow::RunLua`; a native
    /// function runs now.
    pub(crate) fn precall(&mut self, func: usize, nresults: i32) -> LuaResult<Flow> {
        match self.resolve_callable(func)? {
            Value::LuaFn(cl) => {
                let p = self.heap.get(self.heap.get(cl).proto);
                let nparams = usize::from(p.info.num_params);
                let fsize = usize::from(p.info.max_stack);
                let nargs = self.st.top - func - 1;
                self.grow_stack(func + 1 + fsize + 1)?;
                for i in nargs..nparams {
                    self.st.stack[func + 1 + i] = Value::Nil;
                }
                self.st.top = func + 1 + nargs.max(nparams);
                self.st.frames.push(Frame {
                    func,
                    base: func + 1,
                    top: func + 1 + fsize,
                    pc: 0,
                    nresults,
                    nextraargs: 0,
                    flags: flag::LUA,
                    k: None,
                    ctx: 0,
                    handler: 0,
                    pcall_func: 0,
                    pcall_nresults: 0,
                });
                Ok(Flow::RunLua)
            }
            f => self.call_native(f, func, nresults),
        }
    }

    fn call_native(&mut self, f: Value, func: usize, nresults: i32) -> LuaResult<Flow> {
        let entry = self.native_entry(f).expect("native function");
        self.grow_stack(self.st.top + MIN_STACK)?;
        self.st.frames.push(Frame {
            func,
            base: func + 1,
            top: self.st.top + MIN_STACK,
            pc: 0,
            nresults,
            nextraargs: 0,
            flags: 0,
            k: None,
            ctx: 0,
            handler: 0,
            pcall_func: 0,
            pcall_nresults: 0,
        });
        let action = (entry.f)(self)?;
        self.native_action(action)
    }

    /// Carries out what a native function returned.
    pub(crate) fn native_action(&mut self, mut action: Action) -> LuaResult<Flow> {
        loop {
            let fi = self.st.frames.len() - 1;
            match action {
                Action::Return(n) => {
                    let top = self.st.top;
                    assert!(
                        n <= top - self.st.frames[fi].base,
                        "native returned too many values"
                    );
                    self.check_gc(top)?;
                    self.poscall(fi, top - n, n);
                    return Ok(Flow::Returned);
                }
                Action::Call {
                    nargs,
                    nresults,
                    k,
                    ctx,
                } => {
                    let fr = &mut self.st.frames[fi];
                    fr.flags |= flag::KCALL;
                    fr.k = Some(k);
                    fr.ctx = ctx;
                    let func = self.st.top - nargs - 1;
                    match self.precall(func, nresults.map_or(-1, |n| n as i32))? {
                        Flow::Returned => action = self.continue_native(KStatus::Ok)?,
                        flow => return Ok(flow),
                    }
                }
                Action::PCall {
                    nargs,
                    nresults,
                    handler,
                    k,
                    ctx,
                } => {
                    let func = self.st.top - nargs - 1;
                    let base = self.st.frames[fi].base;
                    let fr = &mut self.st.frames[fi];
                    fr.flags |= flag::PCALL;
                    fr.k = Some(k);
                    fr.ctx = ctx;
                    fr.handler = if handler == 0 { 0 } else { base + handler - 1 };
                    fr.pcall_func = func;
                    fr.pcall_nresults = nresults.map_or(-1, |n| n as i32);
                    // Errors raised while starting the call are caught by `execute`,
                    // which finds this frame.
                    match self.precall(func, nresults.map_or(-1, |n| n as i32))? {
                        Flow::Returned => action = self.continue_native(KStatus::Ok)?,
                        flow => return Ok(flow),
                    }
                }
                Action::Yield { n, k } => {
                    if self.st.nny > 0 {
                        let msg = if self.cur == self.main_thread {
                            "attempt to yield from outside a coroutine"
                        } else {
                            "attempt to yield across a C-call boundary"
                        };
                        return Err(self.error_msg(msg));
                    }
                    let fr = &mut self.st.frames[fi];
                    fr.flags |= flag::YIELDED;
                    fr.k = k.map(|(k, _)| k);
                    fr.ctx = k.map_or(0, |(_, c)| c);
                    self.st.nyield = n;
                    return Ok(Flow::Yield);
                }
            }
        }
    }

    /// Runs the continuation of the native function on top of the frame stack.
    pub(crate) fn continue_native(&mut self, status: KStatus) -> LuaResult<Action> {
        let fi = self.st.frames.len() - 1;
        let fr = &mut self.st.frames[fi];
        fr.flags &= !(flag::KCALL | flag::PCALL | flag::YIELDED);
        let k = fr.k.take().expect("native frame has a continuation");
        let ctx = fr.ctx;
        k(self, status, ctx)
    }

    /// Moves `n` results starting at slot `first` to the function slot of frame `fi`,
    /// adjusted to the number its caller wants, and pops the frame (`luaD_poscall`).
    pub(crate) fn poscall(&mut self, fi: usize, first: usize, n: usize) {
        let fr = self.st.frames[fi];
        let res = fr.func;
        let wanted = if fr.nresults < 0 {
            n
        } else {
            fr.nresults as usize
        };
        self.ensure_stack(res + wanted);
        let copy = n.min(wanted);
        self.st.stack.copy_within(first..first + copy, res);
        for v in &mut self.st.stack[res + copy..res + wanted] {
            *v = Value::Nil;
        }
        self.st.top = res + wanted;
        self.st.frames.truncate(fi);
    }

    // ---- running ----------------------------------------------------------------

    /// Runs frames above depth `stop` until the one at depth `stop` returns or the
    /// coroutine yields, recovering from errors at protected calls among those frames.
    /// `start` sets things in motion (for example by calling a function), and errors it
    /// raises are handled the same way.
    pub(crate) fn execute(
        &mut self,
        stop: usize,
        start: impl FnOnce(&mut Vm) -> LuaResult<Flow>,
    ) -> LuaResult<Exit> {
        let mut result = start(self).and_then(|flow| self.run(stop, flow));
        loop {
            match result {
                Ok(exit) => return Ok(exit),
                Err(e) => match self.find_pcall(stop) {
                    Some(fi) => {
                        result = self.recover(fi, e).and_then(|flow| self.run(stop, flow));
                    }
                    None => return Err(e),
                },
            }
        }
    }

    /// The innermost native frame at or above `stop` waiting on a protected call.
    fn find_pcall(&self, stop: usize) -> Option<usize> {
        (stop..self.st.frames.len())
            .rev()
            .find(|&i| self.st.frames[i].flags & flag::PCALL != 0)
    }

    /// Recovers from error `e` at the protected call of native frame `fi`: runs the
    /// message handler, closes variables, and resumes the native with the error.
    fn recover(&mut self, fi: usize, e: Error) -> LuaResult<Flow> {
        let fr = self.st.frames[fi];
        let mut err = e;
        if fr.handler != 0 && err.status == Status::Runtime {
            self.st.top = self.st.top.max(self.frames_top());
            err = self.call_handler(fr.handler, err);
        }
        if err.value.is_nil() && err.status == Status::Runtime {
            err.value = self.heap.str_value("<no error object>");
        }
        self.st.frames.truncate(fi + 1);
        let func = fr.pcall_func;
        let err = self.close_with_error(func, err);
        self.st.frames.truncate(fi + 1);
        self.st.top = func;
        self.push(err.value);
        let action = self.continue_native(KStatus::Error(err.status))?;
        self.native_action(action)
    }

    /// The highest slot any frame uses.
    fn frames_top(&self) -> usize {
        self.st.frames.iter().map(|f| f.top).max().unwrap_or(0)
    }

    /// Calls the message handler in slot `h` on error `err` (`luaG_errormsg`). An error
    /// in the handler is handled by the handler again, until the nesting limit turns
    /// it into "error in error handling".
    pub(crate) fn call_handler(&mut self, h: usize, err: Error) -> Error {
        self.c_calls += 1;
        let r = if self.c_calls >= crate::state::MAX_C_CALLS / 10 * 11 {
            self.err_in_handler()
        } else if self.c_calls == crate::state::MAX_C_CALLS {
            let e = self.rt_error("C stack overflow");
            self.call_handler(h, e)
        } else {
            let func = self.st.top;
            self.ensure_stack(func + 2);
            self.st.stack[func] = self.st.stack[h];
            self.st.stack[func + 1] = err.value;
            self.st.top = func + 2;
            match self.call_raw(func, Some(1), true) {
                Ok(()) => {
                    let v = self.st.stack[func];
                    self.st.top = func;
                    Error::new(v)
                }
                Err(e2) if e2.status == Status::Handler => e2,
                Err(e2) => {
                    self.st.top = self.st.top.max(self.frames_top());
                    self.call_handler(h, e2)
                }
            }
        };
        self.c_calls -= 1;
        r
    }

    /// The state machine over the frame stack.
    fn run(&mut self, stop: usize, mut flow: Flow) -> LuaResult<Exit> {
        loop {
            match flow {
                Flow::Yield => return Ok(Exit::Yielded),
                Flow::RunLua => flow = self.run_lua()?,
                Flow::Returned => {
                    if self.st.frames.len() <= stop {
                        return Ok(Exit::Returned);
                    }
                    let fi = self.st.frames.len() - 1;
                    let fr = self.st.frames[fi];
                    if fr.is_lua() {
                        flow = Flow::RunLua;
                        if fr.flags & flag::FINISH_OP != 0 {
                            self.st.frames[fi].flags &= !flag::FINISH_OP;
                            if let Some(f) = self.finish_op(fi)? {
                                flow = f;
                            }
                        }
                    } else {
                        let action = self.continue_native(KStatus::Ok)?;
                        flow = self.native_action(action)?;
                    }
                }
            }
        }
    }

    /// Calls the function at `func` with the arguments above it, running it to
    /// completion on the Rust stack. Results are adjusted to `nresults` (`None` for all)
    /// and left at `func`. `noyield` marks the call non-yieldable, as every re-entrant
    /// call is. On error the frames are left in place for the caller to unwind.
    pub(crate) fn call_raw(
        &mut self,
        func: usize,
        nresults: Option<usize>,
        _noyield: bool,
    ) -> LuaResult<()> {
        if let Err(e) = self.inc_c_calls() {
            self.c_calls -= 1;
            return Err(e);
        }
        self.st.nny += 1;
        let stop = self.st.frames.len();
        let nres = nresults.map_or(-1, |n| n as i32);
        let r = self.execute(stop, |vm| vm.precall(func, nres));
        self.st.nny -= 1;
        self.c_calls -= 1;
        r.map(|_| ())
    }

    /// Calls the function below the top `nargs` values (`lua_call`). Results replace
    /// the function and arguments; `nresults` is the number wanted, or `None` for all.
    ///
    /// The call cannot yield. From a native function, prefer returning
    /// [`Action::Call`] so the callee may yield.
    pub fn call(&mut self, nargs: usize, nresults: Option<usize>) -> LuaResult<()> {
        let func = self.st.top - nargs - 1;
        self.call_raw(func, nresults, true)
    }

    /// Like [`Vm::call`], but catches errors (`lua_pcall`): on error the stack is back
    /// to where it was without the function and arguments, and the error is returned.
    /// `handler` is the window index of a message handler, or 0.
    pub fn pcall(
        &mut self,
        nargs: usize,
        nresults: Option<usize>,
        handler: usize,
    ) -> LuaResult<()> {
        let func = self.st.top - nargs - 1;
        let depth = self.st.frames.len();
        let tbc = self.st.tbc.len();
        let h = (handler != 0).then(|| self.st.frames.last().map_or(0, |f| f.base) + handler - 1);
        match self.call_raw(func, nresults, true) {
            Ok(()) => Ok(()),
            Err(mut e) => {
                if let Some(h) = h
                    && e.status == Status::Runtime
                {
                    self.st.top = self.st.top.max(self.frames_top());
                    e = self.call_handler(h, e);
                }
                if e.value.is_nil() && e.status == Status::Runtime {
                    e.value = self.heap.str_value("<no error object>");
                }
                self.st.frames.truncate(depth);
                let e = self.close_with_error(func, e);
                self.st.frames.truncate(depth);
                debug_assert!(
                    self.st.tbc.len() <= tbc || self.st.tbc[tbc..].iter().all(|&s| s < func)
                );
                self.st.top = func;
                Err(e)
            }
        }
    }

    // ---- finishing interrupted instructions ------------------------------------

    /// Calls metamethod `tm` with `args` from the running Lua frame, placing the call
    /// at the current top. The instruction is completed by `finish_op`, now if the
    /// metamethod is native, or when the metamethod's frame returns.
    pub(crate) fn call_tm(
        &mut self,
        tm: Value,
        args: &[Value],
        nresults: i32,
    ) -> LuaResult<Option<Flow>> {
        let fi = self.st.frames.len() - 1;
        let func = self.st.top;
        self.ensure_stack(func + args.len() + 1);
        self.st.stack[func] = tm;
        self.st.stack[func + 1..func + 1 + args.len()].copy_from_slice(args);
        self.st.top = func + 1 + args.len();
        self.st.frames[fi].flags |= flag::FINISH_OP;
        match self.precall(func, nresults)? {
            Flow::Returned => {
                self.st.frames[fi].flags &= !flag::FINISH_OP;
                self.finish_op(fi)
            }
            flow => Ok(Some(flow)),
        }
    }

    /// Completes the instruction a metamethod call interrupted (`luaV_finishOp`).
    /// `None` means the frame can go on running.
    fn finish_op(&mut self, fi: usize) -> LuaResult<Option<Flow>> {
        let fr = self.st.frames[fi];
        let base = fr.base;
        let code = self.frame_code(&fr);
        let inst = code.code[fr.pc - 1];
        match OpCode::of(inst) {
            OpCode::MmBin | OpCode::MmBinI | OpCode::MmBinK => {
                self.st.top -= 1;
                let dest = arg_a(code.code[fr.pc - 2]) as usize;
                self.st.stack[base + dest] = self.st.stack[self.st.top];
            }
            OpCode::Unm
            | OpCode::BNot
            | OpCode::Len
            | OpCode::GetTabUp
            | OpCode::GetTable
            | OpCode::GetI
            | OpCode::GetField
            | OpCode::SelfOp => {
                self.st.top -= 1;
                self.st.stack[base + arg_a(inst) as usize] = self.st.stack[self.st.top];
            }
            OpCode::Lt
            | OpCode::Le
            | OpCode::LtI
            | OpCode::LeI
            | OpCode::GtI
            | OpCode::GeI
            | OpCode::Eq => {
                self.st.top -= 1;
                let res = self.st.stack[self.st.top].is_truthy();
                if res != arg_k(inst) {
                    self.st.frames[fi].pc += 1;
                }
            }
            OpCode::Concat => {
                let top = self.st.top - 1;
                let a = arg_a(inst) as usize;
                let total = top - 1 - (base + a);
                self.st.stack[top - 2] = self.st.stack[top];
                self.st.top = top - 1;
                return self.concat(total);
            }
            _ => {}
        }
        Ok(None)
    }

    fn frame_code(&self, fr: &Frame) -> std::rc::Rc<FnInfo> {
        let Value::LuaFn(cl) = self.st.stack[fr.func] else {
            unreachable!("Lua frame without a Lua function")
        };
        self.heap.get(self.heap.get(cl).proto).info.clone()
    }

    // ---- slow paths ----------------------------------------------------------------

    /// `luaV_concat` over the top `total` values; the result ends in the first slot.
    pub(crate) fn concat(&mut self, mut total: usize) -> LuaResult<Option<Flow>> {
        while total > 1 {
            let top = self.st.top;
            let (p1, p2) = (top - 2, top - 1);
            let a = self.st.stack[p1];
            let a_ok = matches!(a, Value::Str(_) | Value::Int(_) | Value::Float(_));
            if !a_ok || !self.tostring_slot(p2) {
                let b = self.st.stack[p2];
                let tm = self.bin_tm(a, b, Tm::Concat);
                if tm.is_nil() {
                    let (v, o) = if a_ok {
                        (b, Operand::Slot(p2))
                    } else {
                        (a, Operand::Slot(p1))
                    };
                    return Err(self.type_error(v, o, "concatenate"));
                }
                return self.call_tm(tm, &[a, b], 1);
            }
            let n;
            if self.is_empty_str(self.st.stack[p2]) {
                self.tostring_slot(p1);
                n = 2;
            } else if self.is_empty_str(self.st.stack[p1]) {
                self.st.stack[p1] = self.st.stack[p2];
                n = 2;
            } else {
                let mut len = self.str_len(self.st.stack[p2]);
                let mut k = 1;
                while k < total && self.tostring_slot(top - k - 1) {
                    len += self.str_len(self.st.stack[top - k - 1]);
                    k += 1;
                }
                n = k;
                if len >= (i32::MAX as usize) {
                    self.st.top = top - total;
                    return Err(self.rt_error("string length overflow"));
                }
                let mut buf = Vec::with_capacity(len);
                for slot in top - n..top {
                    if let Value::Str(s) = self.st.stack[slot] {
                        buf.extend_from_slice(self.heap.string(s).as_bytes());
                    }
                }
                self.st.stack[top - n] = Value::Str(self.heap.intern_vec(buf));
            }
            total -= n - 1;
            self.st.top -= n - 1;
        }
        Ok(None)
    }

    fn is_empty_str(&self, v: Value) -> bool {
        matches!(v, Value::Str(s) if self.heap.string(s).is_empty())
    }

    fn str_len(&self, v: Value) -> usize {
        match v {
            Value::Str(s) => self.heap.string(s).len(),
            _ => 0,
        }
    }

    /// Arithmetic metamethod for `a op b` (`luaT_trybinTM`), result to the instruction's
    /// destination via `finish_op`.
    fn try_bin_tm(
        &mut self,
        a: Value,
        b: Value,
        oa: Operand,
        ob: Operand,
        event: Tm,
    ) -> LuaResult<Option<Flow>> {
        let tm = self.bin_tm(a, b, event);
        if tm.is_nil() {
            return Err(self.arith_error(a, b, oa, ob, event));
        }
        self.call_tm(tm, &[a, b], 1)
    }

    /// Order comparison with metamethods (`luaT_callorderTM`).
    fn order_tm(&mut self, a: Value, b: Value, event: Tm) -> LuaResult<Option<Flow>> {
        let tm = self.bin_tm(a, b, event);
        if tm.is_nil() {
            return Err(self.order_error(a, b));
        }
        self.call_tm(tm, &[a, b], 1)
    }

    /// `luaV_objlen` into register slot `dest` (via `finish_op` when a metamethod runs).
    fn obj_len(&mut self, v: Value, o: Operand) -> LuaResult<Result<Value, Value>> {
        let tm = match v {
            Value::Table(h) => {
                let tm = match self.heap.table(h).metatable() {
                    Some(mt) => self.heap.tm(mt, Tm::Len),
                    None => Value::Nil,
                };
                if tm.is_nil() {
                    return Ok(Ok(Value::Int(self.heap.raw_len(h) as i64)));
                }
                tm
            }
            Value::Str(s) => return Ok(Ok(Value::Int(self.heap.string(s).len() as i64))),
            _ => {
                let tm = self.heap.tm_of(v, Tm::Len);
                if tm.is_nil() {
                    return Err(self.type_error(v, o, "get length of"));
                }
                tm
            }
        };
        Ok(Err(tm))
    }

    /// `forprep`: prepares a numeric loop at `ra`; true to skip it.
    fn for_prep(&mut self, ra: usize) -> LuaResult<bool> {
        let init = self.st.stack[ra];
        let limit = self.st.stack[ra + 1];
        let step = self.st.stack[ra + 2];
        if let (Value::Int(init), Value::Int(step)) = (init, step) {
            if step == 0 {
                return Err(self.rt_error("'for' step is zero"));
            }
            let Some(limit) = self.for_limit(init, limit, step)? else {
                return Ok(true);
            };
            let count = if step > 0 {
                let c = (limit as u64).wrapping_sub(init as u64);
                if step != 1 { c / step as u64 } else { c }
            } else {
                let c = (init as u64).wrapping_sub(limit as u64);
                c / ((-(step + 1)) as u64 + 1)
            };
            self.st.stack[ra] = Value::Int(count as i64);
            self.st.stack[ra + 1] = Value::Int(step);
            self.st.stack[ra + 2] = Value::Int(init);
        } else {
            let Some(flimit) = number::to_float(&self.heap, limit) else {
                return Err(self.for_error(limit, "limit"));
            };
            let Some(fstep) = number::to_float(&self.heap, step) else {
                return Err(self.for_error(step, "step"));
            };
            let Some(finit) = number::to_float(&self.heap, init) else {
                return Err(self.for_error(init, "initial value"));
            };
            if fstep == 0.0 {
                return Err(self.rt_error("'for' step is zero"));
            }
            let skip = if 0.0 < fstep {
                flimit < finit
            } else {
                finit < flimit
            };
            if skip {
                return Ok(true);
            }
            self.st.stack[ra] = Value::Float(flimit);
            self.st.stack[ra + 1] = Value::Float(fstep);
            self.st.stack[ra + 2] = Value::Float(finit);
        }
        Ok(false)
    }

    /// `forlimit`: the integer limit of a loop, or `None` when it must not run.
    fn for_limit(&mut self, init: i64, lim: Value, step: i64) -> LuaResult<Option<i64>> {
        let mode = if step < 0 { F2I::Ceil } else { F2I::Floor };
        let l = match number::to_integer(&self.heap, lim, mode) {
            Some(l) => l,
            None => {
                let Some(flim) = number::to_float(&self.heap, lim) else {
                    return Err(self.for_error(lim, "limit"));
                };
                if 0.0 < flim {
                    if step < 0 {
                        return Ok(None);
                    }
                    i64::MAX
                } else {
                    if step > 0 {
                        return Ok(None);
                    }
                    i64::MIN
                }
            }
        };
        let skip = if step > 0 { init > l } else { init < l };
        Ok(if skip { None } else { Some(l) })
    }

    fn for_error(&mut self, v: Value, what: &str) -> Error {
        let t = self.type_name_of(v);
        self.rt_error(format!("bad 'for' {what} (number expected, got {t})"))
    }

    /// `luaT_adjustvarargs` for frame `fi`.
    fn adjust_varargs(
        &mut self,
        fi: usize,
        nfix: usize,
        fsize: usize,
        pflags: u8,
    ) -> LuaResult<()> {
        let fr = self.st.frames[fi];
        let func = fr.func;
        let totalargs = self.st.top - func - 1;
        let nextra = totalargs - nfix;
        if pflags & PF_VATAB != 0 {
            let t = match self.heap.new_table_with(nextra as u32, 1) {
                Ok(t) => t,
                Err(e) => return Err(self.set_error(e)),
            };
            let _ = self
                .heap
                .raw_set(t, Value::Str(self.str_n), Value::Int(nextra as i64));
            for i in 0..nextra {
                let v = self.st.stack[func + nfix + 1 + i];
                let _ = self.heap.raw_set_int(t, i as i64 + 1, v);
            }
            self.st.stack[func + nfix + 1] = Value::Table(t);
            self.check_gc(func + nfix + 2)?;
        } else {
            self.grow_stack(self.st.top + fsize + 1)?;
            let top = self.st.top;
            self.st.stack[top] = self.st.stack[func];
            for i in 1..=nfix {
                self.st.stack[top + i] = self.st.stack[func + i];
                self.st.stack[func + i] = Value::Nil;
            }
            let newfunc = func + totalargs + 1;
            self.st.top = top + 1 + nfix;
            let fr = &mut self.st.frames[fi];
            fr.nextraargs = nextra;
            fr.func = newfunc;
            fr.base = newfunc + 1;
            fr.top = newfunc + 1 + fsize;
            self.ensure_stack(newfunc + 2 + nfix);
            self.st.stack[newfunc + nfix + 1] = Value::Nil;
        }
        Ok(())
    }

    /// `luaT_getvarargs`: `wanted` (or all, when negative) extra arguments to `ra`.
    fn get_varargs(
        &mut self,
        fi: usize,
        ra: usize,
        wanted: i32,
        vatab: Option<usize>,
    ) -> LuaResult<()> {
        let fr = self.st.frames[fi];
        let table = vatab.map(|r| self.st.stack[fr.base + r]);
        let nargs = match table {
            None => fr.nextraargs,
            Some(Value::Table(h)) => match self.heap.raw_get(h, Value::Str(self.str_n)) {
                Value::Int(n) if (n as u64) <= (i32::MAX / 2) as u64 => n as usize,
                _ => return Err(self.rt_error("vararg table has no proper 'n'")),
            },
            Some(_) => return Err(self.rt_error("vararg table has no proper 'n'")),
        };
        let (touse, wanted) = if wanted < 0 {
            self.grow_stack(ra + nargs + 1)?;
            self.st.top = ra + nargs;
            (nargs, nargs)
        } else {
            let w = wanted as usize;
            self.ensure_stack(ra + w);
            (nargs.min(w), w)
        };
        match table {
            Some(Value::Table(h)) => {
                for i in 0..touse {
                    self.st.stack[ra + i] = self.heap.raw_get_int(h, i as i64 + 1);
                }
            }
            _ => {
                let from = fr.func - nargs;
                self.st.stack.copy_within(from..from + touse, ra);
            }
        }
        for v in &mut self.st.stack[ra + touse..ra + wanted] {
            *v = Value::Nil;
        }
        Ok(())
    }

    // ---- the dispatch loop -----------------------------------------------------

    /// Runs the Lua frame on top of the frame stack until it returns, calls a Lua
    /// function, or yields.
    fn run_lua(&mut self) -> LuaResult<Flow> {
        let fi = self.st.frames.len() - 1;
        let fr = self.st.frames[fi];
        let Value::LuaFn(cl) = self.st.stack[fr.func] else {
            unreachable!("Lua frame without a Lua function")
        };
        let proto = self.heap.get(cl).proto;
        let info = self.heap.get(proto).info.clone();
        let code = &info.code[..];
        let mut base = fr.base;
        let mut pc = fr.pc;

        macro_rules! r {
            ($i:expr) => {
                self.st.stack[base + ($i) as usize]
            };
        }
        macro_rules! k {
            ($i:expr) => {
                self.heap.get(proto).constants[($i) as usize]
            };
        }
        // Prepare for code that may raise errors or call metamethods: correct top.
        macro_rules! protect {
            () => {
                self.st.top = self.st.frames[fi].top
            };
        }
        // Handle the outcome of a metamethod call.
        macro_rules! slow {
            ($e:expr) => {
                match $e? {
                    None => pc = self.st.frames[fi].pc,
                    Some(flow) => return Ok(flow),
                }
            };
        }
        macro_rules! cond_jump {
            ($cond:expr, $i:expr) => {
                if $cond != arg_k($i) {
                    pc += 1;
                } else {
                    let ni = code[pc];
                    pc = (pc as i64 + i64::from(arg_sj(ni)) + 1) as usize;
                }
            };
        }
        macro_rules! upval {
            ($n:expr) => {
                self.heap.get(cl).upvals[($n) as usize]
            };
        }

        loop {
            let i = code[pc];
            pc += 1;
            self.st.frames[fi].pc = pc;
            let a = arg_a(i) as usize;
            match OpCode::of(i) {
                OpCode::Move => r!(a) = r!(arg_b(i)),
                OpCode::LoadI => r!(a) = Value::Int(i64::from(arg_sbx(i))),
                OpCode::LoadF => r!(a) = Value::Float(f64::from(arg_sbx(i))),
                OpCode::LoadK => r!(a) = k!(arg_bx(i)),
                OpCode::LoadKX => {
                    r!(a) = k!(arg_ax(code[pc]));
                    pc += 1;
                }
                OpCode::LoadFalse => r!(a) = Value::Bool(false),
                OpCode::LFalseSkip => {
                    r!(a) = Value::Bool(false);
                    pc += 1;
                }
                OpCode::LoadTrue => r!(a) = Value::Bool(true),
                OpCode::LoadNil => {
                    let b = arg_b(i) as usize;
                    for v in &mut self.st.stack[base + a..=base + a + b] {
                        *v = Value::Nil;
                    }
                }
                OpCode::GetUpval => {
                    let uv = upval!(arg_b(i));
                    r!(a) = self.upval_get(uv);
                }
                OpCode::SetUpval => {
                    let uv = upval!(arg_b(i));
                    let v = r!(a);
                    self.upval_set(uv, v);
                }
                OpCode::GetTabUp => {
                    let t = self.upval_get(upval!(arg_b(i)));
                    let key = k!(arg_c(i));
                    if let (Value::Table(h), Value::Str(s)) = (t, key) {
                        let v = self.heap.table(h).get_str(s);
                        if !v.is_nil() {
                            r!(a) = v;
                            continue;
                        }
                    }
                    protect!();
                    match self.finish_get(t, key, Operand::Upval(arg_b(i) as usize))? {
                        Get::Value(v) => r!(a) = v,
                        Get::Call(tm, t) => slow!(self.call_tm(tm, &[t, key], 1)),
                    }
                }
                OpCode::GetTable => {
                    let t = r!(arg_b(i));
                    let key = r!(arg_c(i));
                    if let Value::Table(h) = t {
                        let v = match key {
                            Value::Int(n) => self.heap.raw_get_int(h, n),
                            _ => self.heap.raw_get(h, key),
                        };
                        if !v.is_nil() {
                            r!(a) = v;
                            continue;
                        }
                    }
                    protect!();
                    match self.finish_get(t, key, Operand::Slot(base + arg_b(i) as usize))? {
                        Get::Value(v) => r!(a) = v,
                        Get::Call(tm, t) => slow!(self.call_tm(tm, &[t, key], 1)),
                    }
                }
                OpCode::GetI => {
                    let t = r!(arg_b(i));
                    let n = i64::from(arg_c(i));
                    if let Value::Table(h) = t {
                        let v = self.heap.raw_get_int(h, n);
                        if !v.is_nil() {
                            r!(a) = v;
                            continue;
                        }
                    }
                    protect!();
                    let key = Value::Int(n);
                    match self.finish_get(t, key, Operand::Slot(base + arg_b(i) as usize))? {
                        Get::Value(v) => r!(a) = v,
                        Get::Call(tm, t) => slow!(self.call_tm(tm, &[t, key], 1)),
                    }
                }
                OpCode::GetField => {
                    let t = r!(arg_b(i));
                    let key = k!(arg_c(i));
                    if let (Value::Table(h), Value::Str(s)) = (t, key) {
                        let v = self.heap.table(h).get_str(s);
                        if !v.is_nil() {
                            r!(a) = v;
                            continue;
                        }
                    }
                    protect!();
                    match self.finish_get(t, key, Operand::Slot(base + arg_b(i) as usize))? {
                        Get::Value(v) => r!(a) = v,
                        Get::Call(tm, t) => slow!(self.call_tm(tm, &[t, key], 1)),
                    }
                }
                OpCode::SetTabUp => {
                    let t = self.upval_get(upval!(a));
                    let key = k!(arg_b(i));
                    let v = if arg_k(i) { k!(arg_c(i)) } else { r!(arg_c(i)) };
                    if let Value::Table(h) = t
                        && self.heap.update(h, |tab| tab.replace(key, v))
                    {
                        continue;
                    }
                    protect!();
                    match self.finish_set(t, key, v, Operand::Upval(a))? {
                        Set::Done => {}
                        Set::Call(tm, t) => slow!(self.call_tm(tm, &[t, key, v], 0)),
                    }
                }
                OpCode::SetTable => {
                    let t = r!(a);
                    let key = r!(arg_b(i));
                    let v = if arg_k(i) { k!(arg_c(i)) } else { r!(arg_c(i)) };
                    if let Value::Table(h) = t {
                        let key = self.heap.key(key);
                        if self.heap.update(h, |tab| tab.replace(key, v)) {
                            continue;
                        }
                    }
                    protect!();
                    match self.finish_set(t, key, v, Operand::Slot(base + a))? {
                        Set::Done => {}
                        Set::Call(tm, t) => slow!(self.call_tm(tm, &[t, key, v], 0)),
                    }
                }
                OpCode::SetI => {
                    let t = r!(a);
                    let key = Value::Int(i64::from(arg_b(i)));
                    let v = if arg_k(i) { k!(arg_c(i)) } else { r!(arg_c(i)) };
                    if let Value::Table(h) = t
                        && self.heap.update(h, |tab| tab.replace(key, v))
                    {
                        continue;
                    }
                    protect!();
                    match self.finish_set(t, key, v, Operand::Slot(base + a))? {
                        Set::Done => {}
                        Set::Call(tm, t) => slow!(self.call_tm(tm, &[t, key, v], 0)),
                    }
                }
                OpCode::SetField => {
                    let t = r!(a);
                    let key = k!(arg_b(i));
                    let v = if arg_k(i) { k!(arg_c(i)) } else { r!(arg_c(i)) };
                    if let Value::Table(h) = t
                        && self.heap.update(h, |tab| tab.replace(key, v))
                    {
                        continue;
                    }
                    protect!();
                    match self.finish_set(t, key, v, Operand::Slot(base + a))? {
                        Set::Done => {}
                        Set::Call(tm, t) => slow!(self.call_tm(tm, &[t, key, v], 0)),
                    }
                }
                OpCode::NewTable => {
                    let mut b = arg_vb(i) as u32;
                    let mut c = arg_vc(i) as u32;
                    if b > 0 {
                        b = 1 << (b - 1);
                    }
                    if arg_k(i) {
                        c += arg_ax(code[pc]) as u32 * (MAXARG_VC as u32 + 1);
                    }
                    pc += 1;
                    self.st.frames[fi].pc = pc;
                    let t = match self.heap.new_table_with(c, b) {
                        Ok(t) => t,
                        Err(e) => {
                            protect!();
                            return Err(self.set_error(e));
                        }
                    };
                    r!(a) = Value::Table(t);
                    self.check_gc(base + a + 1)?;
                }
                OpCode::SelfOp => {
                    let t = r!(arg_b(i));
                    let key = k!(arg_c(i));
                    r!(a + 1) = t;
                    if let (Value::Table(h), Value::Str(s)) = (t, key) {
                        let v = self.heap.table(h).get_str(s);
                        if !v.is_nil() {
                            r!(a) = v;
                            continue;
                        }
                    }
                    protect!();
                    match self.finish_get(t, key, Operand::Slot(base + arg_b(i) as usize))? {
                        Get::Value(v) => r!(a) = v,
                        Get::Call(tm, t) => slow!(self.call_tm(tm, &[t, key], 1)),
                    }
                }
                OpCode::AddI => {
                    let imm = i64::from(arg_sc(i));
                    match r!(arg_b(i)) {
                        Value::Int(x) => {
                            r!(a) = Value::Int(x.wrapping_add(imm));
                            pc += 1;
                        }
                        Value::Float(x) => {
                            r!(a) = Value::Float(x + imm as f64);
                            pc += 1;
                        }
                        _ => {}
                    }
                }
                OpCode::AddK
                | OpCode::SubK
                | OpCode::MulK
                | OpCode::ModK
                | OpCode::PowK
                | OpCode::DivK
                | OpCode::IDivK
                | OpCode::BAndK
                | OpCode::BOrK
                | OpCode::BXorK => {
                    let op = match OpCode::of(i) {
                        OpCode::AddK => Arith::Add,
                        OpCode::SubK => Arith::Sub,
                        OpCode::MulK => Arith::Mul,
                        OpCode::ModK => Arith::Mod,
                        OpCode::PowK => Arith::Pow,
                        OpCode::DivK => Arith::Div,
                        OpCode::IDivK => Arith::IDiv,
                        OpCode::BAndK => Arith::BAnd,
                        OpCode::BOrK => Arith::BOr,
                        _ => Arith::BXor,
                    };
                    let x = r!(arg_b(i));
                    let y = k!(arg_c(i));
                    let fast = if let (Arith::Add, Value::Int(p), Value::Int(q)) = (op, x, y) {
                        Some(Value::Int(p.wrapping_add(q)))
                    } else {
                        protect!();
                        self.try_arith(op, x, y)?
                    };
                    if let Some(v) = fast {
                        r!(a) = v;
                        pc += 1;
                    }
                }
                OpCode::ShlI => {
                    let ic = i64::from(arg_sc(i));
                    if let Some(ib) = number::to_integer_ns(r!(arg_b(i)), F2I::Exact) {
                        r!(a) = Value::Int(number::shl(ic, ib));
                        pc += 1;
                    }
                }
                OpCode::ShrI => {
                    let ic = i64::from(arg_sc(i));
                    if let Some(ib) = number::to_integer_ns(r!(arg_b(i)), F2I::Exact) {
                        r!(a) = Value::Int(number::shl(ib, -ic));
                        pc += 1;
                    }
                }
                OpCode::Add
                | OpCode::Sub
                | OpCode::Mul
                | OpCode::Mod
                | OpCode::Pow
                | OpCode::Div
                | OpCode::IDiv
                | OpCode::BAnd
                | OpCode::BOr
                | OpCode::BXor
                | OpCode::Shl
                | OpCode::Shr => {
                    let x = r!(arg_b(i));
                    let y = r!(arg_c(i));
                    let op = match OpCode::of(i) {
                        OpCode::Add => Arith::Add,
                        OpCode::Sub => Arith::Sub,
                        OpCode::Mul => Arith::Mul,
                        OpCode::Mod => Arith::Mod,
                        OpCode::Pow => Arith::Pow,
                        OpCode::Div => Arith::Div,
                        OpCode::IDiv => Arith::IDiv,
                        OpCode::BAnd => Arith::BAnd,
                        OpCode::BOr => Arith::BOr,
                        OpCode::BXor => Arith::BXor,
                        OpCode::Shl => Arith::Shl,
                        _ => Arith::Shr,
                    };
                    let v = match (op, x, y) {
                        (Arith::Add, Value::Int(p), Value::Int(q)) => {
                            Some(Value::Int(p.wrapping_add(q)))
                        }
                        (Arith::Sub, Value::Int(p), Value::Int(q)) => {
                            Some(Value::Int(p.wrapping_sub(q)))
                        }
                        (Arith::Add, Value::Float(p), Value::Float(q)) => Some(Value::Float(p + q)),
                        _ => {
                            protect!();
                            self.try_arith(op, x, y)?
                        }
                    };
                    if let Some(v) = v {
                        r!(a) = v;
                        pc += 1;
                    }
                }
                OpCode::MmBin => {
                    let x = r!(a);
                    let y = r!(arg_b(i));
                    let event = Tm::ALL[arg_c(i) as usize];
                    protect!();
                    slow!(self.try_bin_tm(
                        x,
                        y,
                        Operand::Slot(base + a),
                        Operand::Slot(base + arg_b(i) as usize),
                        event
                    ));
                }
                OpCode::MmBinI => {
                    let x = r!(a);
                    let imm = Value::Int(i64::from(arg_sb(i)));
                    let event = Tm::ALL[arg_c(i) as usize];
                    protect!();
                    let (p, q, op, oq) = if arg_k(i) {
                        (imm, x, Operand::None, Operand::Slot(base + a))
                    } else {
                        (x, imm, Operand::Slot(base + a), Operand::None)
                    };
                    slow!(self.try_bin_tm(p, q, op, oq, event));
                }
                OpCode::MmBinK => {
                    let x = r!(a);
                    let kv = k!(arg_b(i));
                    let event = Tm::ALL[arg_c(i) as usize];
                    protect!();
                    let (p, q, op, oq) = if arg_k(i) {
                        (kv, x, Operand::None, Operand::Slot(base + a))
                    } else {
                        (x, kv, Operand::Slot(base + a), Operand::None)
                    };
                    slow!(self.try_bin_tm(p, q, op, oq, event));
                }
                OpCode::Unm => {
                    let v = r!(arg_b(i));
                    match v {
                        Value::Int(x) => r!(a) = Value::Int(x.wrapping_neg()),
                        Value::Float(x) => r!(a) = Value::Float(-x),
                        _ => {
                            protect!();
                            let o = Operand::Slot(base + arg_b(i) as usize);
                            slow!(self.try_bin_tm(v, v, o, o, Tm::Unm));
                        }
                    }
                }
                OpCode::BNot => {
                    let v = r!(arg_b(i));
                    match number::to_integer_ns(v, F2I::Exact) {
                        Some(x) => r!(a) = Value::Int(!x),
                        None => {
                            protect!();
                            let o = Operand::Slot(base + arg_b(i) as usize);
                            slow!(self.try_bin_tm(v, v, o, o, Tm::BNot));
                        }
                    }
                }
                OpCode::Not => r!(a) = Value::Bool(r!(arg_b(i)).is_falsy()),
                OpCode::Len => {
                    let v = r!(arg_b(i));
                    protect!();
                    match self.obj_len(v, Operand::Slot(base + arg_b(i) as usize))? {
                        Ok(n) => r!(a) = n,
                        Err(tm) => slow!(self.call_tm(tm, &[v, v], 1)),
                    }
                }
                OpCode::Concat => {
                    let n = arg_b(i) as usize;
                    self.st.top = base + a + n;
                    slow!(self.concat(n));
                    self.check_gc(base + a + 1)?;
                }
                OpCode::Close => {
                    protect!();
                    self.close(base + a)?;
                }
                OpCode::Tbc => {
                    protect!();
                    self.new_tbc(base + a)?;
                }
                OpCode::Jmp => {
                    pc = (pc as i64 + i64::from(arg_sj(i))) as usize;
                }
                OpCode::Eq => {
                    let x = r!(a);
                    let y = r!(arg_b(i));
                    match self.eq_or_tm(x, y) {
                        Ok(c) => cond_jump!(c, i),
                        Err(tm) => {
                            protect!();
                            slow!(self.call_tm(tm, &[x, y], 1));
                        }
                    }
                }
                OpCode::Lt | OpCode::Le => {
                    let x = r!(a);
                    let y = r!(arg_b(i));
                    let is_lt = OpCode::of(i) == OpCode::Lt;
                    let c = if is_lt {
                        self.raw_lt(x, y)
                    } else {
                        self.raw_le(x, y)
                    };
                    match c {
                        Some(c) => cond_jump!(c, i),
                        None => {
                            protect!();
                            let ev = if is_lt { Tm::Lt } else { Tm::Le };
                            slow!(self.order_tm(x, y, ev));
                        }
                    }
                }
                OpCode::EqK => {
                    let c = self.heap.raw_eq(r!(a), k!(arg_b(i)));
                    cond_jump!(c, i);
                }
                OpCode::EqI => {
                    let im = arg_sb(i);
                    let c = match r!(a) {
                        Value::Int(x) => x == i64::from(im),
                        Value::Float(x) => x == f64::from(im),
                        _ => false,
                    };
                    cond_jump!(c, i);
                }
                OpCode::LtI | OpCode::LeI | OpCode::GtI | OpCode::GeI => {
                    let im = arg_sb(i);
                    let x = r!(a);
                    let op = OpCode::of(i);
                    let c = match x {
                        Value::Int(v) => Some(match op {
                            OpCode::LtI => v < i64::from(im),
                            OpCode::LeI => v <= i64::from(im),
                            OpCode::GtI => v > i64::from(im),
                            _ => v >= i64::from(im),
                        }),
                        Value::Float(v) => {
                            let f = f64::from(im);
                            Some(match op {
                                OpCode::LtI => v < f,
                                OpCode::LeI => v <= f,
                                OpCode::GtI => v > f,
                                _ => v >= f,
                            })
                        }
                        _ => None,
                    };
                    match c {
                        Some(c) => cond_jump!(c, i),
                        None => {
                            protect!();
                            let imm = if arg_c(i) != 0 {
                                Value::Float(f64::from(im))
                            } else {
                                Value::Int(i64::from(im))
                            };
                            let flip = matches!(op, OpCode::GtI | OpCode::GeI);
                            let ev = if matches!(op, OpCode::LtI | OpCode::GtI) {
                                Tm::Lt
                            } else {
                                Tm::Le
                            };
                            let (p, q) = if flip { (imm, x) } else { (x, imm) };
                            slow!(self.order_tm(p, q, ev));
                        }
                    }
                }
                OpCode::Test => {
                    let c = r!(a).is_truthy();
                    cond_jump!(c, i);
                }
                OpCode::TestSet => {
                    let v = r!(arg_b(i));
                    if v.is_falsy() == arg_k(i) {
                        pc += 1;
                    } else {
                        r!(a) = v;
                        let ni = code[pc];
                        pc = (pc as i64 + i64::from(arg_sj(ni)) + 1) as usize;
                    }
                }
                OpCode::Call => {
                    let func = base + a;
                    let b = arg_b(i) as usize;
                    if b != 0 {
                        self.st.top = func + b;
                    }
                    let nresults = arg_c(i) - 1;
                    match self.precall(func, nresults)? {
                        Flow::Returned => {}
                        flow => return Ok(flow),
                    }
                }
                OpCode::TailCall => {
                    let func = base + a;
                    let b = arg_b(i) as usize;
                    if b != 0 {
                        self.st.top = func + b;
                    }
                    if arg_k(i) {
                        self.close_upvals(base);
                    }
                    let f = self.resolve_callable(func)?;
                    let b = self.st.top - func;
                    match f {
                        Value::LuaFn(ncl) => {
                            let fr = self.st.frames[fi];
                            let nparams1 = arg_c(i) as usize;
                            let delta = if nparams1 != 0 {
                                fr.nextraargs + nparams1
                            } else {
                                0
                            };
                            let p = self.heap.get(self.heap.get(ncl).proto);
                            let fsize = usize::from(p.info.max_stack);
                            let nfix = usize::from(p.info.num_params);
                            let newfunc = fr.func - delta;
                            self.grow_stack(newfunc + 1 + fsize.max(b) + 1)?;
                            self.st.stack.copy_within(func..func + b, newfunc);
                            for n in b..=nfix {
                                self.st.stack[newfunc + n] = Value::Nil;
                            }
                            self.st.top = newfunc + b.max(nfix + 1);
                            let fr = &mut self.st.frames[fi];
                            fr.func = newfunc;
                            fr.base = newfunc + 1;
                            fr.top = newfunc + 1 + fsize;
                            fr.pc = 0;
                            fr.nextraargs = 0;
                            fr.flags |= flag::TAIL;
                            return Ok(Flow::RunLua);
                        }
                        _ => match self.precall(func, -1)? {
                            Flow::Returned => {}
                            flow => return Ok(flow),
                        },
                    }
                }
                OpCode::Return => {
                    let ra = base + a;
                    let mut n = arg_b(i) as i64 - 1;
                    if n < 0 {
                        n = (self.st.top - ra) as i64;
                    }
                    let n = n as usize;
                    if arg_k(i) {
                        let ftop = self.st.frames[fi].top;
                        if self.st.top < ftop {
                            self.st.top = ftop;
                        }
                        self.st.top = self.st.top.max(ra + n);
                        self.close(base)?;
                    }
                    let nparams1 = arg_c(i) as usize;
                    if nparams1 != 0 {
                        let fr = &mut self.st.frames[fi];
                        fr.func -= fr.nextraargs + nparams1;
                    }
                    self.poscall(fi, ra, n);
                    return Ok(Flow::Returned);
                }
                OpCode::Return0 => {
                    self.poscall(fi, base + a, 0);
                    return Ok(Flow::Returned);
                }
                OpCode::Return1 => {
                    self.poscall(fi, base + a, 1);
                    return Ok(Flow::Returned);
                }
                OpCode::ForLoop => {
                    if let Value::Int(step) = r!(a + 1) {
                        let Value::Int(count) = r!(a) else {
                            unreachable!()
                        };
                        let count = count as u64;
                        if count > 0 {
                            let Value::Int(idx) = r!(a + 2) else {
                                unreachable!()
                            };
                            r!(a) = Value::Int((count - 1) as i64);
                            r!(a + 2) = Value::Int(idx.wrapping_add(step));
                            pc -= arg_bx(i) as usize;
                        }
                    } else {
                        let (Value::Float(limit), Value::Float(step), Value::Float(idx)) =
                            (r!(a), r!(a + 1), r!(a + 2))
                        else {
                            unreachable!()
                        };
                        let idx = idx + step;
                        let go = if 0.0 < step {
                            idx <= limit
                        } else {
                            limit <= idx
                        };
                        if go {
                            r!(a + 2) = Value::Float(idx);
                            pc -= arg_bx(i) as usize;
                        }
                    }
                }
                OpCode::ForPrep => {
                    protect!();
                    if self.for_prep(base + a)? {
                        pc += arg_bx(i) as usize + 1;
                    }
                }
                OpCode::TForPrep => {
                    self.st.stack.swap(base + a + 2, base + a + 3);
                    protect!();
                    self.new_tbc(base + a + 2)?;
                    pc += arg_bx(i) as usize;
                }
                OpCode::TForCall => {
                    let ra = base + a;
                    self.ensure_stack(ra + 6);
                    self.st.stack[ra + 5] = self.st.stack[ra + 3];
                    self.st.stack[ra + 4] = self.st.stack[ra + 1];
                    self.st.stack[ra + 3] = self.st.stack[ra];
                    self.st.top = ra + 6;
                    match self.precall(ra + 3, arg_c(i))? {
                        Flow::Returned => {}
                        flow => return Ok(flow),
                    }
                }
                OpCode::TForLoop => {
                    if !r!(a + 3).is_nil() {
                        pc -= arg_bx(i) as usize;
                    }
                }
                OpCode::SetList => {
                    let ra = base + a;
                    let mut n = arg_vb(i) as usize;
                    let mut last = arg_vc(i) as usize;
                    if n == 0 {
                        n = self.st.top - ra - 1;
                    }
                    last += n;
                    if arg_k(i) {
                        last += arg_ax(code[pc]) as usize * (MAXARG_VC as usize + 1);
                        pc += 1;
                    }
                    let Value::Table(h) = self.st.stack[ra] else {
                        unreachable!("SETLIST on a non-table")
                    };
                    let values = &self.st.stack[ra + 1..=ra + n];
                    let first = (last - n) as i64 + 1;
                    let res = self.heap.update(h, |t| {
                        if t.array_size() < last as u32 {
                            t.reserve(last as u32, 0)?;
                        }
                        for (j, &v) in values.iter().enumerate() {
                            t.set_int(first + j as i64, v)?;
                        }
                        Ok::<(), orbit_core::table::SetError>(())
                    });
                    if let Err(e) = res {
                        protect!();
                        return Err(self.set_error(e));
                    }
                }
                OpCode::Closure => {
                    let p = self.heap.get(proto).protos[arg_bx(i) as usize];
                    let pinfo = self.heap.get(p).info.clone();
                    let mut upvals = Vec::with_capacity(pinfo.upvalues.len());
                    for d in pinfo.upvalues.iter() {
                        if d.in_stack {
                            upvals.push(self.find_upval(base + usize::from(d.index)));
                        } else {
                            upvals.push(upval!(d.index));
                        }
                    }
                    let c = self.heap.alloc(LuaClosure {
                        proto: p,
                        upvals: upvals.into_boxed_slice(),
                    });
                    r!(a) = Value::LuaFn(c);
                    self.check_gc(base + a + 1)?;
                }
                OpCode::VarArg => {
                    let n = arg_c(i) - 1;
                    let vatab = arg_k(i).then_some(arg_b(i) as usize);
                    protect!();
                    self.get_varargs(fi, base + a, n, vatab)?;
                }
                OpCode::GetVarg => {
                    let fr = self.st.frames[fi];
                    let nextra = fr.nextraargs;
                    let key = r!(arg_c(i));
                    let v = match key {
                        Value::Str(s) if self.heap.string(s).as_bytes() == b"n" => {
                            Value::Int(nextra as i64)
                        }
                        _ => match number::to_integer_ns(key, F2I::Exact) {
                            Some(n) if (n as u64).wrapping_sub(1) < nextra as u64 => {
                                self.st.stack[fr.func - nextra + n as usize - 1]
                            }
                            _ => Value::Nil,
                        },
                    };
                    r!(a) = v;
                }
                OpCode::ErrNNil => {
                    if !r!(a).is_nil() {
                        let bx = arg_bx(i) as usize;
                        let name = if bx > 0 {
                            match k!(bx - 1) {
                                Value::Str(s) => {
                                    String::from_utf8_lossy(self.heap.string(s).as_bytes())
                                        .into_owned()
                                }
                                _ => "?".to_string(),
                            }
                        } else {
                            "?".to_string()
                        };
                        protect!();
                        return Err(self.rt_error(format!("global '{name}' already defined")));
                    }
                }
                OpCode::VarArgPrep => {
                    let nfix = usize::from(info.num_params);
                    let fsize = usize::from(info.max_stack);
                    self.adjust_varargs(fi, nfix, fsize, info.flags)?;
                    base = self.st.frames[fi].base;
                }
                OpCode::ExtraArg => unreachable!("EXTRAARG executed"),
            }
        }
    }
}
