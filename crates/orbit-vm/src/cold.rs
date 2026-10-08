//! The slow and rare halves of instructions.
//!
//! The dispatch loop in `run()` is one large function, and every line in it
//! competes for the same machine registers. So `run()` keeps only each
//! instruction's common case: decoding, a type check and a few operations
//! on registers. Everything bigger or rarer (allocation, cache misses,
//! varargs, concatenation, error messages) is a method here, marked
//! `#[inline(never)]` so it costs the loop one call instead of its body.
//! New instructions follow the same split: their fast path goes in `run()`,
//! the rest here. That keeps the loop's size, and the hot instructions'
//! register allocation, independent of how many instructions there are.

use orbit_bytecode::{CaptureSource, MULTI, Op};
use orbit_core::table::{self, Key};
use orbit_core::{Cell, Closure, FieldCache, Gc, LuaHeap, LuaStr, Table, Value};

use crate::interp::Flow;
use crate::meta::{self, Event, MetaAction, MetaCall, Metas};
use crate::ops::{self, Exc};
use crate::{Frame, FrameKind, LuaError, Vm};

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
                match crate::meta::index(&self.heap, &self.rt.metas, obj, key) {
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
                match crate::meta::new_index(&mut self.heap, &self.rt.metas, obj, key, v) {
                    Ok(()) => {
                        if self.heap.should_step() {
                            let base = self.rt.stack.frames.last().expect("a frame").base as usize;
                            let live = (base + 256).max(self.rt.top);
                            self.rt.stack.regs.truncate(live);
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
        id: orbit_core::NativeId,
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
        if !call.f.is_function() && self.rt.metamethod(&self.heap, call.f, Event::Call).is_nil() {
            return Err(Exc::Rt(
                format!(
                    "attempt to call a {} value (metamethod '{}')",
                    self.rt.metas.type_name(&self.heap, call.f),
                    call.event.short_name()
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
                event: call.event,
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
            return Err(Exc::Lua(crate::LuaError::new(res)));
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
        if self.rt.metamethod(&self.heap, v, Event::Close).is_nil() {
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
        self.rt.stack.tbc.push(r as u32);
        Ok(())
    }

    /// `Close`, with a variable to close at or above register `from`
    /// (absolute): call the newest one's `__close(value)`. The meta
    /// frame runs `Close` again when it returns.
    #[inline(never)]
    pub(crate) fn op_close(&mut self) -> Exc {
        let t = self.rt.stack.tbc.pop().expect("a variable to close") as usize;
        let v = self.rt.stack.regs[t];
        let h = self.rt.metamethod(&self.heap, v, Event::Close);
        Exc::Meta(MetaCall::new(h, Event::Close, &[v]))
    }

    /// `NewTable`.
    #[inline(never)]
    pub(crate) fn op_new_table(&mut self, array_hint: usize, hash_hint: usize) -> Value {
        Value::Table(self.heap.alloc(Table::with_capacity(array_hint, hash_hint)))
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
                let mut buf = Vec::new();
                for &v in &regs[at..from + left] {
                    ops::concat_piece(&self.heap, v, &mut buf);
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
            let h = meta::binary_handler(&self.heap, &self.rt.metas, Event::Concat, x, y);
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
            let mut call = MetaCall::new(h, Event::Concat, &[x, y]);
            call.aux = left as u8;
            return Err(Exc::Meta(call));
        }
    }

    /// `SetList`: store the `n` values at stack index `from` into `t`,
    /// starting at index `start`.
    #[inline(never)]
    pub(crate) fn op_set_list(&mut self, t: Gc<Table>, from: usize, n: usize, start: i64) {
        let regs = &self.rt.stack.regs;
        self.heap.update(t, |tab| {
            for (i, &v) in regs[from..from + n].iter().enumerate() {
                tab.set_int(start + i as i64, v);
            }
        });
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
        for i in 0..nvarargs {
            let v = self.rt.stack.regs[base - nvarargs + i];
            table::set_int(&mut self.heap, t, i as i64 + 1, v);
        }
        let n = self.rt.strings.new_str(&mut self.heap, b"n");
        let key = Key::str(&self.heap, n);
        table::set_key(&mut self.heap, t, &key, Value::Int(nvarargs as i64));
        Value::Table(t)
    }

    /// `VarargGet`: what the vararg table would hold at `key`.
    #[inline(never)]
    pub(crate) fn op_vararg_get(&self, base: usize, nvarargs: usize, key: Value) -> Value {
        let i = match key {
            Value::Int(i) => i,
            Value::Float(x) => match orbit_core::number::float_to_int(x) {
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

    /// `ForPrep` on the loop state at stack index `at`. Returns whether the
    /// loop runs at all.
    #[inline(never)]
    pub(crate) fn op_for_prep(&mut self, at: usize) -> Result<bool, Exc> {
        let r = &mut self.rt.stack.regs[at..at + 4];
        // Numeric strings convert, always to floats, as in the reference.
        for v in &mut r[..3] {
            if let Value::Str(s) = *v
                && let Some(n) = orbit_core::number::str_to_number(self.heap.get(s).as_bytes())
            {
                *v = Value::Float(n.as_f64().expect("a number"));
            }
        }
        let (init, limit, step) = (r[0], r[1], r[2]);
        if let (Value::Int(i), Value::Int(s)) = (init, step) {
            if s == 0 {
                return Err(Exc::Rt("'for' step is zero".into(), None));
            }
            return match ops::for_limit(i, limit, s) {
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
                    Some(_) => match metas.metamethod(heap, v, Event::Len) {
                        Value::Nil => return Ok(Value::Int(tab.border())),
                        h => h,
                    },
                }
            }
            _ => metas.metamethod(heap, v, Event::Len),
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
        Err(Exc::Meta(MetaCall::new(h, Event::Len, &[v, v])))
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
    {
        let tab = heap.get(t);
        let v = match tab.find_str(heap, k) {
            Some(slot) => {
                cache.fill(slot);
                tab.slot_value(slot)
            }
            None => Value::Nil,
        };
        if v.is_nil() && tab.meta.is_some() {
            return meta::index(heap, metas, Value::Table(t), Value::Str(k));
        }
        Ok(v)
    }
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
        if heap.get(t).meta.is_some() {
            return meta::new_index(heap, metas, Value::Table(t), Value::Str(k), v);
        }
        let key = Key::str(heap, k);
        if let Some(slot) = table::set_key(heap, t, &key, v) {
            cache.fill(slot);
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
