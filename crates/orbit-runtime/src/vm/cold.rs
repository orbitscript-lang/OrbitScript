//! The slow and rare halves of instructions.
//!
//! The dispatch loop (`run_loop` in interp.rs) is one large function, and
//! every line in it competes for the same machine registers. So the loop
//! keeps only each instruction's common case: decoding, a type check and a
//! few operations on registers. Everything bigger or rarer (allocation,
//! cache misses, varargs, concatenation, error messages) is a method here,
//! marked `#[inline(never)]` so it costs the loop one call instead of its
//! body. New instructions follow the same split: their fast path goes in
//! the loop, the rest here. That keeps the loop's size, and the hot
//! instructions' register allocation, independent of how many instructions
//! there are.

use std::borrow::Cow;

use crate::object::number::float_to_int;

use crate::object::table::{self, Key};
use crate::object::{Cell, Closure, FieldCache, Gc, LuaHeap, LuaStr, NoMemory, Table, Value};
use orbit_bytecode::{CaptureSource, MULTI, Op};

use crate::vm::interp::Flow;
use crate::vm::meta::{self, MetaAction, MetaCall, MetaField, Metas};
use crate::vm::ops::{self, Exc};
use crate::vm::{Frame, FrameKind, LuaError, Vm};

impl Vm {
    /// Raise an instruction's error: add its position, then resume the
    /// nearest protected call or return the error.
    #[cold]
    #[inline(never)]
    pub(crate) fn raise(&mut self, x: Exc) -> Result<Flow, LuaError> {
        let x = match x {
            Exc::Meta(call) => match self.call_meta(call) {
                Ok(flow) => return Ok(flow),
                Err(x) => x,
            },
            Exc::Index { obj, key } => {
                match crate::vm::meta::index(&self.heap, &self.rt.metas, obj, key) {
                    Ok(v) => {
                        let MetaAction::Store { dst } = self.pending_action(0) else {
                            unreachable!("an indexing instruction")
                        };
                        let base = self.rt.stack.frames.last().expect("a frame").base as usize;
                        self.rt.stack.regs[base + dst as usize] = v;
                        return Ok(Flow::Reload);
                    }
                    Err(Exc::Meta(call)) => match self.call_meta(call) {
                        Ok(flow) => return Ok(flow),
                        Err(x) => x,
                    },
                    Err(x) => x,
                }
            }
            Exc::NewIndex { obj, key, v } => {
                let mut r = crate::vm::meta::new_index(&mut self.heap, &self.rt.metas, obj, key, v);
                if matches!(r, Err(Exc::NoMemory)) && !self.rt.gc.finalizing() {
                    // Like lua5.5's emergency collection: collect, then try
                    // once more. The operands are still in registers.
                    self.heap.full_collect(&mut self.rt);
                    r = crate::vm::meta::new_index(&mut self.heap, &self.rt.metas, obj, key, v);
                }
                match r {
                    Ok(()) => {
                        if self.heap.should_step() {
                            let base = self.rt.stack.frames.last().expect("a frame").base as usize;
                            let live = (base + 256).max(self.rt.top);
                            self.trim_dead_regs(live);
                            self.heap.step(&mut self.rt);
                        }
                        return Ok(Flow::Reload);
                    }
                    Err(Exc::Meta(call)) => match self.call_meta(call) {
                        Ok(flow) => return Ok(flow),
                        Err(x) => x,
                    },
                    Err(x) => x,
                }
            }
            x => x,
        };
        let e = self.make_error(x);
        self.unwind(e)
    }

    /// Native `id`, called at stack index `func`, raised `e`: give it a
    /// frame, so tracebacks and message handlers see it as the reference
    /// shows it (`[C]: in global 'error'`). Unwinding pops it.
    #[cold]
    #[inline(never)]
    pub(crate) fn native_raised(
        &mut self,
        id: crate::object::NativeId,
        func: usize,
        e: LuaError,
    ) -> Exc {
        let s = &mut self.rt.stack;
        if !matches!(s.frames.last(), Some(f) if f.func as usize == func
            && matches!(f.kind, FrameKind::NativeCall { .. }))
        {
            let cdepth = s.same_depth();
            s.frames.push(Frame {
                kind: FrameKind::NativeCall { id },
                func: func as u32,
                base: func as u32 + 1,
                pc: 0,
                cell_base: s.cells.len() as u32,
                nvarargs: 0,
                want: 0,
                tail: false,
                extra: 0,
                cdepth,
            });
        }
        Exc::Lua(e)
    }

    /// Start metamethod call `call` for the instruction the top (Lua) frame
    /// is running: push a meta frame recording how to finish it, then call
    /// the handler above the frame's registers.
    pub(crate) fn call_meta(&mut self, call: MetaCall) -> Result<Flow, Exc> {
        let action = self.pending_action(call.aux);
        if !call.f.is_function()
            && self
                .rt
                .metamethod(&self.heap, call.f, MetaField::Call)
                .is_nil()
        {
            return Err(Exc::Rt(
                format!(
                    "attempt to call a {} value (metamethod '{}')",
                    self.rt.metas.type_name(&self.heap, call.f),
                    call.field.short_name()
                ),
                None,
            ));
        }
        let f = *self.rt.stack.frames.last().expect("a frame");
        let FrameKind::Lua { closure, .. } = f.kind else {
            unreachable!("metamethods are requested by Lua frames")
        };
        let mut at = f.base as usize + self.heap.get(closure).code.nregs as usize;
        if let MetaAction::Close { top } = action {
            // Keep the open-ended results a `return` is about to pass on.
            at = at.max(top as usize);
        }
        let n = call.nargs as usize;
        let regs = &mut self.rt.stack.regs;
        if regs.len() < at + 1 + n {
            regs.resize(at + 1 + n, Value::Nil);
        }
        regs[at] = call.f;
        regs[at + 1..at + 1 + n].copy_from_slice(&call.args[..n]);
        let cdepth = self.rt.stack.deeper();
        if self.c_overflow(cdepth) {
            return Err(Exc::Rt("C stack overflow".into(), None));
        }
        self.rt.stack.frames.push(Frame {
            kind: FrameKind::Meta {
                action,
                field: call.field,
            },
            func: at as u32,
            base: at as u32,
            pc: 0,
            cell_base: self.rt.stack.cells.len() as u32,
            nvarargs: 0,
            want: 1,
            tail: false,
            extra: 0,
            cdepth,
        });
        self.call_value(at, n, 1)
    }

    /// How to finish the instruction the top (Lua) frame is running once
    /// its metamethod returns; `aux` as in [`MetaCall`].
    fn pending_action(&self, aux: u8) -> MetaAction {
        let f = self.rt.stack.frames.last().expect("a frame");
        let FrameKind::Lua { closure, .. } = f.kind else {
            unreachable!("metamethods are requested by Lua frames")
        };
        let op = self.heap.get(closure).code.code[f.pc as usize - 1];
        match op {
            Op::GetGlobal { dst, .. }
            | Op::GetField { dst, .. }
            | Op::GetIndex { dst, .. }
            | Op::GetIndexInt { dst, .. }
            | Op::SelfCall { dst, .. }
            | Op::Add { dst, .. }
            | Op::Sub { dst, .. }
            | Op::Mul { dst, .. }
            | Op::Div { dst, .. }
            | Op::IDiv { dst, .. }
            | Op::Mod { dst, .. }
            | Op::Pow { dst, .. }
            | Op::AddInt { dst, .. }
            | Op::AddK { dst, .. }
            | Op::SubK { dst, .. }
            | Op::MulK { dst, .. }
            | Op::DivK { dst, .. }
            | Op::IDivK { dst, .. }
            | Op::ModK { dst, .. }
            | Op::PowK { dst, .. }
            | Op::KAdd { dst, .. }
            | Op::KSub { dst, .. }
            | Op::KMul { dst, .. }
            | Op::KDiv { dst, .. }
            | Op::SubInt { dst, .. }
            | Op::Neg { dst, .. }
            | Op::BAnd { dst, .. }
            | Op::BOr { dst, .. }
            | Op::BXor { dst, .. }
            | Op::Shl { dst, .. }
            | Op::Shr { dst, .. }
            | Op::BNot { dst, .. }
            | Op::Len { dst, .. } => MetaAction::Store { dst },
            Op::SetGlobal { .. }
            | Op::SetField { .. }
            | Op::SetIndex { .. }
            | Op::SetIndexInt { .. } => MetaAction::Discard,
            Op::JumpLt { off, .. } | Op::JumpLe { off, .. } | Op::JumpEq { off, .. } => {
                MetaAction::Branch { want: true, off }
            }
            Op::JumpNotLt { off, .. } | Op::JumpNotLe { off, .. } | Op::JumpNe { off, .. } => {
                MetaAction::Branch { want: false, off }
            }
            Op::JumpLtInt { want, off, .. }
            | Op::JumpLeInt { want, off, .. }
            | Op::JumpGtInt { want, off, .. }
            | Op::JumpGeInt { want, off, .. } => MetaAction::Branch { want, off },
            Op::Concat { dst, first, .. } => MetaAction::Concat {
                dst,
                first,
                left: aux,
            },
            Op::Close { .. } => MetaAction::Close {
                top: self.rt.top as u32,
            },
            Op::Cast { dst, .. } => MetaAction::ToStr { dst },
            other => unreachable!("{other:?} doesn't call metamethods"),
        }
    }

    /// A metamethod call returned into its meta frame (on top): finish the
    /// instruction that asked for it.
    pub(crate) fn meta_returned(&mut self) -> Result<Flow, Exc> {
        let m = self.rt.stack.frames.pop().expect("meta frame");
        let FrameKind::Meta { action, .. } = m.kind else {
            unreachable!("meta frame")
        };
        let res = self.rt.stack.regs[m.func as usize];
        if let MetaAction::Unwind { handled } = action {
            // A `__close` handler ran while `res` unwound: carry on.
            self.rt.handled = handled;
            return Err(Exc::Lua(crate::vm::LuaError::new(res)));
        }
        let caller = self.rt.stack.frames.last_mut().expect("caller frame");
        let base = caller.base as usize;
        match action {
            MetaAction::Store { dst } => self.rt.stack.regs[base + dst as usize] = res,
            MetaAction::Branch { want, off } => {
                if !res.is_falsy() == want {
                    caller.pc = (caller.pc as i64 + off as i64) as u32;
                }
            }
            MetaAction::Discard => {}
            MetaAction::Close { top } => {
                // Run `Close` again for the next variable.
                self.rt.top = top as usize;
                caller.pc -= 1;
            }
            MetaAction::Unwind { .. } => unreachable!("handled above"),
            MetaAction::ToStr { dst } => {
                let v = match res {
                    Value::Str(_) => res,
                    Value::Int(_) | Value::Float(_) => self.number_string(res)?,
                    _ => return Err(Exc::Rt("'__tostring' must return a string".into(), None)),
                };
                let base = self.rt.stack.frames.last().expect("caller frame").base as usize;
                self.rt.stack.regs[base + dst as usize] = v;
            }
            MetaAction::Concat { dst, first, left } => {
                let from = base + first as usize;
                let left = left as usize;
                self.rt.stack.regs[from + left - 2] = res;
                match self.op_concat(from, left - 1) {
                    Ok(v) => self.rt.stack.regs[base + dst as usize] = v,
                    Err(Exc::Meta(call)) => return self.call_meta(call),
                    Err(x) => return Err(x),
                }
            }
        }
        Ok(Flow::Reload)
    }

    /// `Tbc`: mark register `r` (absolute) to be closed. Its value is not
    /// `nil` or `false`. `pc` is the instruction's, for the local's name.
    #[inline(never)]
    pub(crate) fn op_tbc(&mut self, r: usize, pc: usize) -> Result<(), Exc> {
        let v = self.rt.stack.regs[r];
        if self.rt.metamethod(&self.heap, v, MetaField::Close).is_nil() {
            let f = self.rt.stack.frames.last().expect("a frame");
            let FrameKind::Lua { closure, .. } = f.kind else {
                unreachable!("Tbc runs in a Lua frame")
            };
            let proto = self.heap.get(self.heap.get(closure).proto);
            let name = match proto.debug.origin(pc as u32, 0) {
                Some(orbit_bytecode::Origin::Local(n)) => String::from_utf8_lossy(n).into_owned(),
                _ => "?".into(),
            };
            return Err(Exc::Rt(
                format!("variable '{name}' got a non-closable value"),
                None,
            ));
        }
        self.rt.stack.aux.tbc.push(r as u32);
        Ok(())
    }

    /// `Close`, with a variable to close at or above register `from`
    /// (absolute): call the newest one's `__close(value)`. The meta
    /// frame runs `Close` again when it returns.
    #[inline(never)]
    pub(crate) fn op_close(&mut self) -> Exc {
        let t = self.rt.stack.aux.tbc.pop().expect("a variable to close") as usize;
        let v = self.rt.stack.regs[t];
        let h = self.rt.metamethod(&self.heap, v, MetaField::Close);
        Exc::Meta(MetaCall::new(h, MetaField::Close, &[v]))
    }

    /// `NewTable`.
    #[inline(never)]
    pub(crate) fn op_new_table(&mut self, array_hint: usize, hash_hint: usize) -> Gc<Table> {
        // Sized in place: see `Table::presize`.
        let t = self.heap.alloc(Table::new());
        if array_hint | hash_hint != 0 {
            self.heap
                .update(t, |tab| tab.presize(array_hint, hash_hint));
        }
        t
    }

    /// `CellNew`: a fresh cell holding `v` in cell slot `slot`.
    #[inline(never)]
    pub(crate) fn op_cell_new(&mut self, slot: usize, v: Value) {
        let c = self.heap.alloc(Cell { value: v });
        self.rt.stack.cells[slot] = c;
    }

    /// `Closure`: instantiate child prototype `proto` of `parent`.
    #[inline(never)]
    pub(crate) fn op_closure(
        &mut self,
        parent: Gc<Closure>,
        cell_base: usize,
        proto: usize,
    ) -> Value {
        let parent = self.heap.get(parent);
        let child = self.heap.get(parent.proto).children[proto];
        let captures: Box<[Gc<Cell>]> = self
            .heap
            .get(child)
            .captures
            .iter()
            .map(|c| match *c {
                CaptureSource::Cell(i) => self.rt.stack.cells[cell_base + i as usize],
                CaptureSource::Capture(i) => parent.captures[i as usize],
                CaptureSource::Env => parent.captures[0],
            })
            .collect();
        let code = self.heap.get(child).code.clone();
        Value::Closure(self.heap.alloc(Closure {
            proto: child,
            code,
            captures,
        }))
    }

    /// At the end of a collection cycle, give back register space that a
    /// deep moment left behind (a native that pushed a large table's
    /// elements, say, or deep recursion): when at most a quarter of a
    /// large register vector is in use, keep twice what is.
    fn shrink_stack(&mut self) {
        const SMALL: usize = 1 << 16;
        let regs = &mut self.rt.stack.regs;
        if regs.capacity() > SMALL && regs.len() * 4 <= regs.capacity() {
            regs.shrink_to((regs.len() * 2).max(SMALL));
        }
    }

    /// Before a collection step from the dispatch loop: drop the registers at
    /// and above `live`, and clear those between the newest Lua frame's
    /// registers (and the last open results) and `live`. They are dead, but
    /// may still hold what returned callees or frames an error unwound left
    /// there, which would otherwise stay alive until overwritten.
    fn trim_dead_regs(&mut self, live: usize) {
        let s = &mut self.rt.stack;
        s.regs.truncate(live);
        if let Some(f) = s.frames.last() {
            if let FrameKind::Lua { closure, .. } = f.kind {
                let end =
                    (f.base as usize + self.heap.get(closure).code.nregs as usize).max(self.rt.top);
                if end < s.regs.len() {
                    s.regs[end..].fill(Value::Nil);
                }
            }
        }
    }

    /// Collector work for a host that uses the VM between calls through
    /// [`Vm::host_ctx`] (the C API's `lua_newstate`): when allocation has
    /// earned a step, take it and run the finalizers it made due, as the
    /// interpreter does at its safe points. Everything above the host's top
    /// is dead.
    pub fn host_safepoint(&mut self) {
        if self.heap.should_step() && self.rt.stack.frames.is_empty() {
            let live = self.rt.top;
            self.gc_safepoint(live);
        }
    }

    /// A safe point's collector work: one step, then the finalizers it made
    /// due. Returns true if the heap is over its limit even after a full
    /// collection, so the caller raises `not enough memory`.
    #[inline(never)]
    pub(crate) fn gc_safepoint(&mut self, live: usize) -> bool {
        self.trim_dead_regs(live);
        if self.heap.step(&mut self.rt) {
            self.shrink_stack();
        }
        if self.heap.pending_finalizers() > 0 {
            self.run_finalizers();
        }
        if !self.heap.over_limit() {
            self.rt.limit_grace = false;
            return false;
        }
        self.rt.no_memory = self.over_limit_after_collecting();
        self.rt.no_memory
    }

    /// `Concat` of the `n` values at stack index `from`. Strings and numbers
    /// are joined in one go; with any other value the list is reduced from
    /// the right, a pair at a time, through `__concat`. Partial results are
    /// written back into the operand registers, which are temporaries.
    #[inline(never)]
    pub(crate) fn op_concat(&mut self, from: usize, n: usize) -> Result<Value, Exc> {
        let strnum = |v: Value| matches!(v, Value::Str(_) | Value::Int(_) | Value::Float(_));
        let mut left = n;
        loop {
            let regs = &self.rt.stack.regs;
            // The longest run of strings and numbers at the end.
            let run = regs[from..from + left]
                .iter()
                .rev()
                .take_while(|&&v| strnum(v))
                .count();
            if run >= 2 || (run == left && left == 1) {
                let at = from + left - run;
                // Numbers take at most this many bytes.
                const NUM: usize = 64;
                let len = regs[at..from + left]
                    .iter()
                    .map(|&v| match v {
                        Value::Str(s) => self.heap.get(s).as_bytes().len(),
                        _ => NUM,
                    })
                    .fold(0usize, usize::saturating_add);
                self.reserve(len)?;
                let regs = &self.rt.stack.regs;
                let mut buf = Vec::new();
                buf.try_reserve_exact(len).map_err(NoMemory::from)?;
                for &v in &regs[at..from + left] {
                    ops::concat_piece(&self.heap, &self.rt.metas.locale, v, &mut buf);
                }
                let s = Value::Str(self.rt.strings.new_str_owned(&mut self.heap, buf));
                if at == from {
                    return Ok(s);
                }
                self.rt.stack.regs[at] = s;
                left = at - from + 1;
                continue;
            }
            if left == 1 {
                return Ok(regs[from]);
            }
            let (x, y) = (regs[from + left - 2], regs[from + left - 1]);
            let h = meta::binary_handler(&self.heap, &self.rt.metas, MetaField::Concat, x, y);
            if h.is_nil() {
                let j = if strnum(x) { left - 1 } else { left - 2 };
                let v = regs[from + j];
                return Err(Exc::Rt(
                    format!(
                        "attempt to concatenate a {} value",
                        self.rt.metas.type_name(&self.heap, v)
                    ),
                    Some(j as u8),
                ));
            }
            let mut call = MetaCall::new(h, MetaField::Concat, &[x, y]);
            call.aux = left as u8;
            return Err(Exc::Meta(call));
        }
    }

    /// `Cast` to a string (not a test): `v` itself, a number or boolean
    /// written as `tostring` writes it, or a call to its `__tostring`.
    #[inline(never)]
    pub(crate) fn op_cast_string(&mut self, v: Value, mode: u8) -> Result<Value, Exc> {
        use orbit_bytecode::cast;
        match v {
            Value::Str(_) => Ok(v),
            Value::Int(_) | Value::Float(_) => self.number_string(v),
            Value::Bool(b) => {
                let s: &[u8] = if b { b"true" } else { b"false" };
                Ok(Value::Str(self.rt.strings.new_str(&mut self.heap, s)))
            }
            Value::Nil if mode & cast::NILABLE != 0 => Ok(v),
            _ => {
                let h = self.rt.metamethod(&self.heap, v, MetaField::ToString);
                if !h.is_nil() {
                    Err(Exc::Meta(MetaCall::new(h, MetaField::ToString, &[v])))
                } else if mode & cast::SOFT != 0 {
                    Ok(Value::Nil)
                } else {
                    Err(cast_error(&self.heap, &self.rt.metas, v, Value::Nil, mode))
                }
            }
        }
    }

    /// A number as a string, as `tostring` writes it.
    fn number_string(&mut self, v: Value) -> Result<Value, Exc> {
        self.reserve(64)?;
        let mut buf = Vec::new();
        ops::concat_piece(&self.heap, &self.rt.metas.locale, v, &mut buf);
        Ok(Value::Str(
            self.rt.strings.new_str_owned(&mut self.heap, buf),
        ))
    }

    /// `SetList`: store the `n` values at stack index `from` into `t`,
    /// starting at index `start`.
    #[inline(never)]
    pub(crate) fn op_set_list(
        &mut self,
        t: Gc<Table>,
        from: usize,
        n: usize,
        start: i64,
    ) -> Result<(), NoMemory> {
        let regs = &self.rt.stack.regs;
        self.heap.update(t, |tab| {
            for (i, &v) in regs[from..from + n].iter().enumerate() {
                tab.set_int(start + i as i64, v)?;
            }
            Ok(())
        })
    }

    /// `Vararg`: copy the frame's extra arguments (the `nvarargs` values
    /// below `base`) to register `dst`, `count` of them or all with `MULTI`.
    #[inline(never)]
    pub(crate) fn op_vararg(&mut self, base: usize, nvarargs: usize, dst: usize, count: u8) {
        let from = base - nvarargs;
        let to = base + dst;
        let regs = &mut self.rt.stack.regs;
        if count == MULTI {
            if regs.len() < to + nvarargs {
                regs.resize(to + nvarargs, Value::Nil);
            }
            regs.copy_within(from..from + nvarargs, to);
            self.rt.top = to + nvarargs;
        } else {
            for i in 0..count as usize {
                regs[to + i] = if i < nvarargs {
                    regs[from + i]
                } else {
                    Value::Nil
                };
            }
        }
    }

    /// `VarargTable`: `{n = count, ...}` of the frame's extra arguments.
    #[inline(never)]
    pub(crate) fn op_vararg_table(&mut self, base: usize, nvarargs: usize) -> Value {
        let t = self.heap.alloc(Table::with_capacity(nvarargs, 1));
        // The table is presized, so these stores allocate nothing.
        for i in 0..nvarargs {
            let v = self.rt.stack.regs[base - nvarargs + i];
            let _ = table::set_int(&mut self.heap, t, i as i64 + 1, v);
        }
        let n = self.rt.strings.new_str(&mut self.heap, b"n");
        let key = Key::str(&self.heap, n);
        let _ = table::set_key(&mut self.heap, t, &key, Value::Int(nvarargs as i64));
        Value::Table(t)
    }

    /// `VarargGet`: what the vararg table would hold at `key`.
    #[inline(never)]
    pub(crate) fn op_vararg_get(&self, base: usize, nvarargs: usize, key: Value) -> Value {
        let i = match key {
            Value::Int(i) => i,
            Value::Float(x) => match crate::object::number::float_to_int(x) {
                Some(i) => i,
                None => return Value::Nil,
            },
            Value::Str(s) if self.heap.get(s).as_bytes() == b"n" => {
                return Value::Int(nvarargs as i64);
            }
            _ => return Value::Nil,
        };
        if (1..=nvarargs as i64).contains(&i) {
            self.rt.stack.regs[base - nvarargs + i as usize - 1]
        } else {
            Value::Nil
        }
    }

    /// `VarargUnpack`: like `Vararg`, from the vararg table `table`.
    #[inline(never)]
    pub(crate) fn op_vararg_unpack(
        &mut self,
        base: usize,
        table: Value,
        dst: usize,
        count: u8,
    ) -> Result<(), Exc> {
        let Value::Table(t) = table else {
            unreachable!("the vararg table is a read-only local")
        };
        let key = self.rt.strings.new_str(&mut self.heap, b"n");
        let n = match self.heap.get(t).get_str(&self.heap, key) {
            Value::Int(n) if (0..1 << 30).contains(&n) => n as usize,
            _ => return Err(Exc::Rt("vararg table has no proper 'n'".into(), None)),
        };
        let to = base + dst;
        let count = if count == MULTI {
            if to + n > self.rt.limits.max_stack {
                return Err(Exc::Rt("stack overflow".into(), None));
            }
            let regs = &mut self.rt.stack.regs;
            if regs.len() < to + n {
                regs.resize(to + n, Value::Nil);
            }
            self.rt.top = to + n;
            n
        } else {
            count as usize
        };
        let tab = self.heap.get(t);
        for i in 0..count {
            self.rt.stack.regs[to + i] = if i < n {
                tab.get_int(i as i64 + 1)
            } else {
                Value::Nil
            };
        }
        Ok(())
    }

    /// `ForPrep` (or with `exclusive`, `ForPrepExclusive`) on the loop state
    /// at stack index `at`. Returns whether the loop runs at all.
    #[inline(never)]
    pub(crate) fn op_for_prep(&mut self, at: usize, exclusive: bool) -> Result<bool, Exc> {
        let r = &mut self.rt.stack.regs[at..at + 4];
        // Numeric strings convert, always to floats, as in the reference.
        for v in &mut r[..3] {
            if let Value::Str(s) = *v {
                if let Some(n) = self
                    .rt
                    .metas
                    .locale
                    .str_to_number(self.heap.get(s).as_bytes())
                {
                    *v = Value::Float(n.as_f64().expect("a number"));
                }
            }
        }
        let (init, limit, step) = (r[0], r[1], r[2]);
        if let (Value::Int(i), Value::Int(s)) = (init, step) {
            if s == 0 {
                return Err(Exc::Rt("'for' step is zero".into(), None));
            }
            return match ops::for_limit(i, limit, s, exclusive) {
                Err(_) => Err(for_error(&self.heap, &self.rt.metas, "limit", limit)),
                Ok(None) => Ok(false),
                Ok(Some(lim)) => {
                    // The number of iterations left replaces the limit.
                    let count = if s > 0 {
                        (lim as u64).wrapping_sub(i as u64) / s as u64
                    } else {
                        (i as u64).wrapping_sub(lim as u64) / ((-(s + 1)) as u64 + 1)
                    };
                    r[1] = Value::Int(count as i64);
                    r[3] = Value::Int(i);
                    Ok(true)
                }
            };
        }
        let Some(lim) = limit.as_f64() else {
            return Err(for_error(&self.heap, &self.rt.metas, "limit", limit));
        };
        let Some(st) = step.as_f64() else {
            return Err(for_error(&self.heap, &self.rt.metas, "step", step));
        };
        let Some(start) = init.as_f64() else {
            return Err(for_error(&self.heap, &self.rt.metas, "initial", init));
        };
        if st == 0.0 {
            return Err(Exc::Rt("'for' step is zero".into(), None));
        }
        // A float loop that stops short of the limit is the inclusive loop to
        // the nearest float inside it: no float lies between the two.
        let lim = match (exclusive, st > 0.0) {
            (false, _) => lim,
            (true, true) => next_down(lim),
            (true, false) => next_up(lim),
        };
        let runs = if st > 0.0 { start <= lim } else { lim <= start };
        if runs {
            r[0] = Value::Float(start);
            r[1] = Value::Float(lim);
            r[2] = Value::Float(st);
            r[3] = Value::Float(start);
        }
        Ok(runs)
    }
}

/// `Cast`, past the fast paths, except a conversion to a string
/// ([`Vm::op_cast_string`]): `v as` the target `mode` names, `ty` the named
/// type (or, for `FAIL`, the target's text). See [`orbit_bytecode::cast`].
#[inline(never)]
pub(crate) fn op_cast(
    heap: &LuaHeap,
    metas: &Metas,
    v: Value,
    ty: Value,
    mode: u8,
) -> Result<Value, Exc> {
    use orbit_bytecode::cast;
    let kind = mode & cast::KIND;
    if mode & cast::TEST != 0 {
        let ok = match kind {
            cast::INT => matches!(v, Value::Int(_)),
            cast::FLOAT => matches!(v, Value::Float(_)),
            cast::NUMBER => matches!(v, Value::Int(_) | Value::Float(_)),
            cast::STRING => matches!(v, Value::Str(_)),
            cast::BOOL => matches!(v, Value::Bool(_)),
            cast::NIL => v.is_nil(),
            cast::NAMED => is_named(heap, metas, v, ty),
            _ => false,
        };
        return Ok(Value::Bool(ok || (v.is_nil() && mode & cast::NILABLE != 0)));
    }
    if v.is_nil() && mode & cast::NILABLE != 0 {
        return Ok(v);
    }
    let done = match (kind, v) {
        (cast::INT, Value::Int(_)) => Some(v),
        (cast::INT, Value::Float(f)) => match float_to_int(f) {
            Some(i) => Some(Value::Int(i)),
            None if mode & cast::SOFT != 0 => return Ok(Value::Nil),
            None => {
                return Err(Exc::Rt("number has no integer representation".into(), None));
            }
        },
        (cast::FLOAT, Value::Int(i)) => Some(Value::Float(i as f64)),
        (cast::FLOAT | cast::NUMBER, Value::Float(_)) | (cast::NUMBER, Value::Int(_)) => Some(v),
        (cast::BOOL, _) => Some(Value::Bool(!v.is_falsy())),
        (cast::NIL, Value::Nil) => Some(v),
        (cast::NAMED, _) if is_named(heap, metas, v, ty) => Some(v),
        _ => None,
    };
    match done {
        Some(v) => Ok(v),
        None if mode & cast::SOFT != 0 => Ok(Value::Nil),
        None => Err(cast_error(heap, metas, v, ty, mode)),
    }
}

/// `v` is a value of the struct, enum or variant `ty`: its metatable is
/// `ty`, it is `ty` (a unit variant), or its metatable's `__enum` is `ty`.
fn is_named(heap: &LuaHeap, metas: &Metas, v: Value, ty: Value) -> bool {
    let Value::Table(ty) = ty else { return false };
    if let Value::Table(t) = v {
        if t == ty {
            return true;
        }
    }
    let Some(mt) = metas.metatable(heap, v) else {
        return false;
    };
    if mt == ty {
        return true;
    }
    let name = metas.field_names.name(MetaField::Enum);
    let link = heap.get(mt).meta_field(heap, MetaField::Enum, name);
    matches!(link, Value::Table(e) if e == ty)
}

/// "cannot cast boolean to int": the error of a failed cast.
#[cold]
fn cast_error(heap: &LuaHeap, metas: &Metas, v: Value, ty: Value, mode: u8) -> Exc {
    use orbit_bytecode::cast;
    let target: Cow<'_, str> = match mode & cast::KIND {
        cast::INT => "int".into(),
        cast::FLOAT => "float".into(),
        cast::NUMBER => "number".into(),
        cast::STRING => "string".into(),
        cast::BOOL => "bool".into(),
        cast::NIL => "nil".into(),
        _ => type_text(heap, metas, ty),
    };
    let q = if mode & cast::NILABLE != 0 { "?" } else { "" };
    let from = metas.type_name(heap, v);
    Exc::Rt(format!("cannot cast {from} to {target}{q}"), None)
}

/// The name of a cast's target `ty`: the string itself, the table's own
/// `__name` (a struct, data variant or enum), or its metatable's (a unit
/// variant).
fn type_text<'a>(heap: &'a LuaHeap, metas: &Metas, ty: Value) -> Cow<'a, str> {
    let own = match ty {
        Value::Str(_) => ty,
        Value::Table(t) => {
            let name = metas.field_names.name(MetaField::Name);
            heap.get(t).meta_field(heap, MetaField::Name, name)
        }
        _ => Value::Nil,
    };
    match own {
        Value::Str(s) => String::from_utf8_lossy(heap.get(s).as_bytes()),
        _ => metas.type_name(heap, ty),
    }
}

/// `Len` of `v`.
#[inline(never)]
pub(crate) fn op_len(heap: &LuaHeap, metas: &Metas, v: Value) -> Result<Value, Exc> {
    {
        let h = match v {
            Value::Str(s) => return Ok(Value::Int(heap.get(s).len() as i64)),
            Value::Table(t) => {
                let tab = heap.get(t);
                match tab.meta {
                    None => return Ok(Value::Int(tab.border())),
                    Some(_) => match metas.metamethod(heap, v, MetaField::Len) {
                        Value::Nil => return Ok(Value::Int(tab.border())),
                        h => h,
                    },
                }
            }
            _ => metas.metamethod(heap, v, MetaField::Len),
        };
        if h.is_nil() {
            return Err(Exc::Rt(
                format!(
                    "attempt to get length of a {} value",
                    metas.type_name(heap, v)
                ),
                Some(0),
            ));
        }
        Err(Exc::Meta(MetaCall::new(h, MetaField::Len, &[v, v])))
    }
}

/// The value of string key `k` in `t` when the inline cache missed;
/// refills the cache when the key is there, and consults the metatable
/// when it isn't.
#[inline(never)]
pub(crate) fn get_str_miss(
    heap: &LuaHeap,
    metas: &Metas,
    t: Gc<Table>,
    k: Gc<LuaStr>,
    cache: &FieldCache,
) -> Result<Value, Exc> {
    let tab = heap.get(t);
    let key = cache.key_for(heap, k);
    let v = match tab.find_str_key(heap, &key) {
        Some(slot) => {
            cache.fill(slot);
            tab.slot_value(slot)
        }
        None => Value::Nil,
    };
    match tab.meta {
        Some(mt) if v.is_nil() => meta::index_str_miss(heap, metas, t, mt, &key),
        _ => Ok(v),
    }
}

/// Method `k` of table `obj`, which lacks it and has metatable `mt`, when
/// `SelfCall`'s caches missed: refills them when `__index` is a table that
/// holds the method, else follows the whole `__index` chain. An error
/// (including an `__index` function to call) is left to the slow path.
#[inline(never)]
#[allow(clippy::too_many_arguments)]
pub(crate) fn method_miss(
    heap: &LuaHeap,
    metas: &Metas,
    obj: Value,
    mt: Gc<Table>,
    k: Gc<LuaStr>,
    method: &FieldCache,
    index: &FieldCache,
    up: Option<(&FieldCache, &FieldCache)>,
) -> Result<Value, Exc> {
    let index_key = index.key_for(heap, metas.field_names.name(MetaField::Index));
    let method_key = method.key_for(heap, k);
    let mut meta = mt;
    // Walk `__index` tables with the hashed keys; anything else (a function,
    // a userdata) goes through the general path from the start.
    for depth in 0..meta::MAX_CHAIN {
        let mtab = heap.get(meta);
        let Some(slot) = mtab.find_str_key(heap, &index_key) else {
            return Ok(Value::Nil);
        };
        if depth == 0 {
            index.fill(slot);
        } else if depth == 1 {
            if let Some((index2, _)) = up.filter(|(index2, _)| index2.key == index.key) {
                index2.fill(slot);
            }
        }
        let h = mtab.slot_value(slot);
        let Value::Table(c) = h else {
            return if h.is_nil() {
                Ok(Value::Nil)
            } else {
                meta::index(heap, metas, obj, Value::Str(k))
            };
        };
        let ctab = heap.get(c);
        if let Some(slot) = ctab.find_str_key(heap, &method_key) {
            if depth == 0 {
                method.fill(slot);
            } else if depth == 1 {
                if let Some((_, method2)) = up.filter(|(_, method2)| method2.key == method.key) {
                    method2.fill(slot);
                }
            }
            let v = ctab.slot_value(slot);
            if !v.is_nil() {
                return Ok(v);
            }
        }
        match ctab.meta {
            Some(next) => meta = next,
            None => return Ok(Value::Nil),
        }
    }
    meta::index(heap, metas, obj, Value::Str(k))
}

/// `t[k] = v` for string key `k` when the inline cache missed; refills
/// the cache. A table with a metatable whose key is absent goes through
/// `__newindex`.
#[inline(never)]
pub(crate) fn set_str_miss(
    heap: &mut LuaHeap,
    metas: &Metas,
    t: Gc<Table>,
    k: Gc<LuaStr>,
    v: Value,
    cache: &FieldCache,
) -> Result<(), Exc> {
    {
        let key = cache.key_for(heap, k);
        let tab = heap.get(t);
        if let Some(mt) = tab.meta {
            // A key that is present with a value takes a raw store, as
            // `__newindex` only applies to absent keys: fill the cache so
            // the next store hits inline.
            let slot = tab.find_str_key(heap, &key);
            // An absent key with a `__newindex` goes through it; without
            // one (an object whose class gives only `__index`), the store
            // is raw, into the key's dead entry if it has one.
            let e = MetaField::NewIndex;
            if slot.map_or(true, |s| tab.slot_value(s).is_nil())
                && !heap
                    .get(mt)
                    .meta_field(heap, e, metas.field_names.name(e))
                    .is_nil()
            {
                return meta::new_index(heap, metas, Value::Table(t), Value::Str(k), v);
            }
            if let Some(slot) = slot {
                cache.fill(slot);
                heap.barrier_back(t);
                heap.get_mut(t).set_slot(slot, v);
                return Ok(());
            }
        }
        match heap.get(t).find_str_store(heap, &key) {
            Ok(slot) => {
                cache.fill(slot);
                heap.barrier_back(t);
                heap.get_mut(t).set_slot(slot, v);
                return Ok(());
            }
            Err(Some(slot)) if !v.is_nil() => {
                cache.fill(slot);
                heap.barrier_back(t);
                heap.get_mut(t).insert_str_at(slot, &key, v);
                return Ok(());
            }
            Err(_) => {}
        }
        match table::set_key(heap, t, &key, v) {
            Ok(Some(slot)) => cache.fill(slot),
            Ok(None) => {}
            // The slow path collects and retries.
            Err(_) => {
                return Err(Exc::NewIndex {
                    obj: Value::Table(t),
                    key: Value::Str(k),
                    v,
                });
            }
        }
        Ok(())
    }
}

/// `global x = v` found `x` already set.
#[cold]
#[inline(never)]
pub(crate) fn already_defined(name: &[u8]) -> Exc {
    Exc::Rt(
        format!("global '{}' already defined", String::from_utf8_lossy(name)),
        None,
    )
}

/// `_ENV` isn't a table.
#[cold]
#[inline(never)]
pub(crate) fn env_error(heap: &LuaHeap, rt: &Metas, v: Value) -> Exc {
    Exc::Rt(
        format!(
            "attempt to index a {} value (upvalue '_ENV')",
            rt.type_name(heap, v)
        ),
        None,
    )
}

#[cold]
fn for_error(heap: &LuaHeap, rt: &Metas, what: &str, v: Value) -> Exc {
    let what = if what == "initial" {
        "initial value"
    } else {
        what
    };
    Exc::Rt(
        format!(
            "bad 'for' {what} (number expected, got {})",
            rt.type_name(heap, v)
        ),
        None,
    )
}

/// The least float greater than `x` (`f64::next_up`, stable only from Rust 1.86).
fn next_up(x: f64) -> f64 {
    if x.is_nan() || x == f64::INFINITY {
        return x;
    }
    if x == 0.0 {
        return f64::from_bits(1);
    }
    let bits = x.to_bits();
    f64::from_bits(if x > 0.0 { bits + 1 } else { bits - 1 })
}

/// The greatest float less than `x` (`f64::next_down`, stable only from Rust 1.86).
fn next_down(x: f64) -> f64 {
    -next_up(-x)
}
