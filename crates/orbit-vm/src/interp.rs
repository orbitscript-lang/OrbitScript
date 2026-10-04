//! The dispatch loop, calls, returns and error unwinding.

use std::rc::Rc;

use orbit_bytecode::{MULTI, Op, Origin};
use orbit_core::number::{ArithOp, Number};
use orbit_core::table;
use orbit_core::{Closure, Gc, ProtoCode, Value};

use crate::cold::{env_error, index_error};
use crate::native::{CallOutcome, NativeCtx, NativeReturn};
use crate::ops::{self, Arith, Exc};
use crate::{Frame, FrameKind, LuaError, Vm};

/// Registers a frame can address: register numbers are `u8`.
const WINDOW: usize = 256;

/// Where a new Lua frame's state lives, so the dispatch loop can enter it
/// without reading the frame back.
struct Entered {
    base: usize,
    cell_base: usize,
    nvarargs: usize,
    code_id: usize,
}

/// What the dispatch loop does next.
pub(crate) enum Flow {
    /// The top frame changed (or its registers were touched by a call):
    /// reload it.
    Reload,
    /// The frame stack is back at the level the run started from.
    Done,
}

impl Vm {
    /// Call `regs[func]` with the `nargs` values above it and run until it
    /// returns. Its results end up at `regs[func..top]`.
    pub(crate) fn call_at(
        &mut self,
        func: usize,
        nargs: usize,
        stop: usize,
    ) -> Result<(), LuaError> {
        let flow = match self.call_value(func, nargs, MULTI, stop) {
            Ok(f) => f,
            Err(x) => {
                let e = self.make_error(x, stop);
                self.unwind(e, stop)?
            }
        };
        match flow {
            Flow::Done => Ok(()),
            Flow::Reload => self.run(stop),
        }
    }

    /// Start a call of `regs[func]`. Lua functions get a frame (the loop runs
    /// them); natives run now.
    pub(crate) fn call_value(
        &mut self,
        func: usize,
        nargs: usize,
        want: u8,
        stop: usize,
    ) -> Result<Flow, Exc> {
        match self.rt.stack.regs[func] {
            Value::Closure(c) => {
                self.push_lua_frame(c, func, nargs, want)?;
                Ok(Flow::Reload)
            }
            Value::Native(id) => {
                self.rt.stack.regs.truncate(func + 1 + nargs);
                let f = self.rt.natives[id.0 as usize].f;
                let mut ctx = NativeCtx {
                    heap: &mut self.heap,
                    rt: &mut self.rt,
                    base: func + 1,
                    nargs,
                    id,
                };
                let r = f(&mut ctx)?;
                self.native_returned(r, None, func, want, id, stop)
            }
            v => Err(Exc::Rt(
                format!("attempt to call a {} value", v.type_name()),
                Some(0),
            )),
        }
    }

    /// Push a frame for Lua closure `c`, called at stack index `func` with
    /// `nargs` arguments, and size the stack for its register window.
    #[inline(always)]
    fn push_lua_frame(
        &mut self,
        c: Gc<Closure>,
        func: usize,
        nargs: usize,
        want: u8,
    ) -> Result<Entered, Exc> {
        let proto = &*self.heap.get(c).code;
        enter_lua_frame(&mut self.rt, proto, c, func, nargs, want)
    }
}

/// Push a frame for Lua closure `c` running `proto`: the body of
/// [`Vm::push_lua_frame`], for callers that already hold the code.
#[inline(always)]
fn enter_lua_frame(
    rt: &mut crate::Runtime,
    proto: &ProtoCode,
    c: Gc<Closure>,
    func: usize,
    nargs: usize,
    want: u8,
) -> Result<Entered, Exc> {
    let code_id = proto as *const ProtoCode as usize;
    let np = proto.nparams as usize;
    let nregs = proto.nregs as usize;
    let ncells = proto.ncells as usize;
    let (base, nvar) = if proto.is_vararg && nargs > np {
        (func + 1 + nargs, nargs - np)
    } else {
        (func + 1, 0)
    };
    let end = base + nregs.max(np);
    if end > rt.limits.max_stack {
        return Err(Exc::Rt("stack overflow".into(), None));
    }
    let regs = &mut rt.stack.regs;
    if regs.len() < base + WINDOW {
        regs.resize(base + WINDOW, Value::Nil);
    }
    if nvar > 0 {
        regs.copy_within(func + 1..func + 1 + np, base);
    } else if nargs < np {
        for r in &mut regs[base + nargs..base + np] {
            *r = Value::Nil;
        }
    }
    let cells = &mut rt.stack.cells;
    let cell_base = cells.len();
    if ncells > 0 {
        cells.resize(cell_base + ncells, rt.dead_cell);
    }
    rt.stack.frames.push(Frame {
        kind: FrameKind::Lua {
            closure: c,
            code_id,
        },
        func: func as u32,
        base: base as u32,
        pc: 0,
        cell_base: cell_base as u32,
        nvarargs: nvar as u32,
        want,
    });
    Ok(Entered {
        base,
        cell_base,
        nvarargs: nvar,
        code_id,
    })
}

impl Vm {
    /// Copy `n` results from `src` to `dst`, adjusted to `want`.
    #[inline]
    fn place_results(&mut self, src: usize, n: usize, dst: usize, want: u8) {
        let regs = &mut self.rt.stack.regs;
        if want == 1 && n >= 1 && dst < regs.len() {
            regs[dst] = regs[src];
            self.rt.top = dst + 1;
            return;
        }
        if want == MULTI {
            regs.copy_within(src..src + n, dst);
            self.rt.top = dst + n;
        } else {
            let want = want as usize;
            if regs.len() < dst + want {
                regs.resize(dst + want, Value::Nil);
            }
            let k = n.min(want);
            regs.copy_within(src..src + k, dst);
            for r in &mut regs[dst + k..dst + want] {
                *r = Value::Nil;
            }
            self.rt.top = dst + want;
        }
    }

    /// Pop the running Lua frame, returning `n` values from `src`.
    #[inline]
    fn do_return(&mut self, src: usize, n: usize, stop: usize) -> Result<Flow, Exc> {
        let f = self.rt.stack.frames.pop().expect("a frame to return from");
        if self.rt.stack.cells.len() > f.cell_base as usize {
            self.rt.stack.cells.truncate(f.cell_base as usize);
        }
        self.place_results(src, n, f.func as usize, f.want);
        self.after_return(stop)
    }

    /// Results have been placed for the frame now on top: resume it.
    fn after_return(&mut self, stop: usize) -> Result<Flow, Exc> {
        if self.rt.stack.frames.len() <= stop {
            return Ok(Flow::Done);
        }
        let f = *self.rt.stack.frames.last().expect("frame");
        match f.kind {
            // The frame's window lies below the callee's, so the stack is
            // long enough for it.
            FrameKind::Lua { .. } => Ok(Flow::Reload),
            FrameKind::Native { id, k, call_at, .. } => {
                let top = self.rt.top;
                self.rt.stack.regs.truncate(top);
                let mut ctx = NativeCtx {
                    heap: &mut self.heap,
                    rt: &mut self.rt,
                    base: f.base as usize,
                    nargs: f.nvarargs as usize,
                    id,
                };
                let r = k(&mut ctx, CallOutcome::Ok(top - call_at as usize))?;
                self.native_returned(r, Some(f), f.func as usize, f.want, id, stop)
            }
        }
    }

    /// Act on what a native returned. `frame` is its continuation frame, if
    /// it has one on top of the stack.
    fn native_returned(
        &mut self,
        r: NativeReturn,
        frame: Option<Frame>,
        func: usize,
        want: u8,
        id: orbit_core::NativeId,
        stop: usize,
    ) -> Result<Flow, Exc> {
        match r {
            NativeReturn::Return(n) => {
                if frame.is_some() {
                    self.rt.stack.frames.pop();
                }
                let src = self.rt.stack.regs.len() - n;
                self.place_results(src, n, func, want);
                if want == MULTI {
                    self.rt.stack.regs.truncate(self.rt.top);
                }
                self.after_return(stop)
            }
            NativeReturn::CallThen {
                f,
                nargs,
                k,
                protected,
            } => {
                let regs = &mut self.rt.stack.regs;
                let at = regs.len() - nargs;
                regs.insert(at, f);
                let kind = FrameKind::Native {
                    id,
                    k,
                    protected,
                    call_at: at as u32,
                };
                match frame {
                    Some(_) => self.rt.stack.frames.last_mut().expect("frame").kind = kind,
                    None => {
                        let nargs = at - (func + 1);
                        self.rt.stack.frames.push(Frame {
                            kind,
                            func: func as u32,
                            base: func as u32 + 1,
                            pc: 0,
                            cell_base: self.rt.stack.cells.len() as u32,
                            // For native frames: how many arguments it had.
                            nvarargs: nargs as u32,
                            want,
                        })
                    }
                }
                self.call_value(at, nargs, MULTI, stop)
            }
        }
    }

    /// Turn an instruction's error into a Lua error, adding the position of
    /// the running function and the operand's name.
    pub(crate) fn make_error(&mut self, x: Exc, _stop: usize) -> LuaError {
        match x {
            Exc::Lua(e) => e,
            Exc::Plain(m) => LuaError::new(self.new_str(m.as_bytes())),
            Exc::Rt(msg, operand) => {
                let mut text = ops::where_(&self.heap, &self.rt, 1);
                text.push_str(&msg);
                if let Some(op) = operand
                    && let Some(f) = self.rt.stack.frames.last()
                    && let FrameKind::Lua { closure, .. } = f.kind
                {
                    let proto = self.heap.get(self.heap.get(closure).proto);
                    let pc = f.pc.saturating_sub(1);
                    if op == 0
                        && matches!(
                            proto.code.code.get(pc as usize),
                            Some(Op::GenericCall { .. })
                        )
                    {
                        text.push_str(" (for iterator 'for iterator')");
                    } else if let Some(origin) = proto.debug.origin(pc, op) {
                        let (kind, name) = match origin {
                            Origin::Local(n) => ("local", n),
                            Origin::Global(n) => ("global", n),
                            Origin::Field(n) => ("field", n),
                            Origin::Upvalue(n) => ("upvalue", n),
                            Origin::Method(n) => ("method", n),
                            Origin::Constant(n) => ("constant", n),
                        };
                        text.push_str(&format!(" ({kind} '{}')", String::from_utf8_lossy(name)));
                    }
                }
                if msg == "attempt to index a string value" {
                    text.push_str(" (string methods are not supported yet)");
                }
                LuaError::new(self.new_str(text.as_bytes()))
            }
        }
    }

    /// Unwind to the nearest protected native frame above `stop` and resume
    /// it with the error, or return the error if there is none.
    pub(crate) fn unwind(&mut self, mut e: LuaError, stop: usize) -> Result<Flow, LuaError> {
        loop {
            let frames = &self.rt.stack.frames;
            let found = frames
                .iter()
                .enumerate()
                .skip(stop)
                .rev()
                .find(|(_, f)| {
                    matches!(
                        f.kind,
                        FrameKind::Native {
                            protected: true,
                            ..
                        }
                    )
                })
                .map(|(i, _)| i);
            let Some(idx) = found else {
                if e.traceback.is_none() {
                    e.traceback = Some(self.traceback(stop));
                }
                if let Some(f) = self.rt.stack.frames.get(stop) {
                    self.rt.stack.cells.truncate(f.cell_base as usize);
                }
                self.rt.stack.frames.truncate(stop);
                return Err(e);
            };
            if let Some(above) = self.rt.stack.frames.get(idx + 1) {
                self.rt.stack.cells.truncate(above.cell_base as usize);
            }
            self.rt.stack.frames.truncate(idx + 1);
            let f = self.rt.stack.frames[idx];
            let FrameKind::Native { id, k, call_at, .. } = f.kind else {
                unreachable!()
            };
            self.rt.stack.regs.truncate(call_at as usize);
            let mut ctx = NativeCtx {
                heap: &mut self.heap,
                rt: &mut self.rt,
                base: f.base as usize,
                nargs: f.nvarargs as usize,
                id,
            };
            match k(&mut ctx, CallOutcome::Err(e.value)) {
                Err(e2) => {
                    // The continuation itself failed: its frame is done.
                    self.rt.stack.frames.truncate(idx);
                    e = e2;
                }
                Ok(r) => {
                    match self.native_returned(r, Some(f), f.func as usize, f.want, id, stop) {
                        Ok(flow) => return Ok(flow),
                        Err(x) => e = self.make_error(x, stop),
                    }
                }
            }
        }
    }

    fn traceback(&self, stop: usize) -> String {
        let mut s = String::from("stack traceback:");
        for f in self.rt.stack.frames[stop..].iter().rev() {
            match f.kind {
                FrameKind::Lua { closure, .. } => {
                    let proto = self.heap.get(self.heap.get(closure).proto);
                    let line = proto.debug.line(f.pc.saturating_sub(1));
                    let what = match &proto.name {
                        orbit_bytecode::FunctionName::Main => "in main chunk".to_string(),
                        orbit_bytecode::FunctionName::Anonymous => {
                            format!("in function <{}:{}>", proto.chunk, proto.line_defined)
                        }
                        orbit_bytecode::FunctionName::Global(n)
                        | orbit_bytecode::FunctionName::Local(n)
                        | orbit_bytecode::FunctionName::Field(n)
                        | orbit_bytecode::FunctionName::Method(n) => {
                            format!("in function '{}'", String::from_utf8_lossy(n))
                        }
                    };
                    s.push_str(&format!("\n\t{}:{}: {}", proto.chunk, line, what));
                }
                FrameKind::Native { id, .. } => {
                    let name = self.rt.natives[id.0 as usize].name;
                    s.push_str(&format!("\n\t[C]: in function '{name}'"));
                }
            }
        }
        s.push_str("\n\t[C]: in ?");
        s
    }

    /// The dispatch loop. Runs the top frame (a Lua frame) and everything it
    /// calls until the frame stack is back to `stop` frames.
    pub(crate) fn run(&mut self, stop: usize) -> Result<(), LuaError> {
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
            let mut depth = self.rt.stack.frames.len();
            let frame = &self.rt.stack.frames[depth - 1];
            let FrameKind::Lua {
                mut closure,
                code_id,
            } = frame.kind
            else {
                unreachable!("run starts on a Lua frame")
            };
            let mut base = frame.base as usize;
            let mut cell_base = frame.cell_base as usize;
            let mut nvarargs = frame.nvarargs as usize;
            let mut pc = frame.pc as usize;
            // Both the frame's code and `cur` are alive, so equal addresses
            // mean the same code.
            if code_id != Rc::as_ptr(&cur) as usize {
                cur = self.heap.get(closure).code.clone();
            }
            let mut code = &cur.code[..];
            let mut consts = &cur.consts[..];
            let mut caches = &cur.caches[..];
            macro_rules! switch_code {
                ($closure:expr, $code_id:expr) => {
                    // A recursive call or its return keeps the same code.
                    if $code_id != Rc::as_ptr(&cur) as usize {
                        cur = self.heap.get($closure).code.clone();
                        code = &cur.code[..];
                        consts = &cur.consts[..];
                        caches = &cur.caches[..];
                    }
                };
            }

            // The frame's registers as a fixed-size window, so indexing it
            // with a `u8` register number needs no bounds check, and the
            // window's address stays in a machine register. Anything that may
            // resize the stack (calls, collection steps, errors) ends the
            // borrow; take the window again afterwards.
            macro_rules! window {
                () => {{
                    let regs = &mut self.rt.stack.regs;
                    if regs.len() < base + WINDOW {
                        regs.resize(base + WINDOW, Value::Nil);
                    }
                    <&mut [Value; WINDOW]>::try_from(&mut regs[base..base + WINDOW])
                        .expect("a full window")
                }};
            }
            // The window when the stack is known to be long enough: after
            // entering a frame (`push_lua_frame` sized it) or returning to
            // one (its window lies below the callee's).
            macro_rules! window_sized {
                () => {
                    (&mut self.rt.stack.regs[base..])
                        .first_chunk_mut::<WINDOW>()
                        .expect("a full window")
                };
            }
            let mut w = window!();
            macro_rules! reg {
                ($r:expr) => {
                    w[$r as usize]
                };
            }
            macro_rules! save_pc {
                () => {
                    if self.rt.stack.frames.len() == depth {
                        self.rt.stack.frames[depth - 1].pc = pc as u32;
                    }
                };
            }
            macro_rules! throw {
                ($x:expr) => {{
                    let x = $x;
                    save_pc!();
                    match self.raise(x, stop) {
                        Ok(Flow::Reload) => continue 'reload,
                        Ok(Flow::Done) => return Ok(()),
                        Err(e) => return Err(e),
                    }
                }};
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
                        Ok(Flow::Done) => return Ok(()),
                        Err(x) => throw!(x),
                    }
                };
            }
            macro_rules! safepoint {
                () => {
                    if self.heap.should_step() {
                        save_pc!();
                        self.heap.step(&mut self.rt);
                        w = window!();
                    }
                };
            }
            macro_rules! jump {
                ($off:expr) => {
                    pc = (pc as isize + $off as isize) as usize
                };
            }
            macro_rules! arith {
                ($dst:expr, $a:expr, $b:expr, $op:expr, $nop:expr) => {{
                    // `Number::arith` owns the integer/float rules; with a
                    // constant operator it inlines to the one case. Each arm
                    // stores its own result, so the common cases write a
                    // known tag and payload instead of copying a merged
                    // `Value` through the stack.
                    macro_rules! store {
                        ($p:expr, $q:expr) => {
                            match Number::arith($p, $nop, $q) {
                                Ok(Number::Int(i)) => reg!($dst) = Value::Int(i),
                                Ok(Number::Float(f)) => reg!($dst) = Value::Float(f),
                                Err(e) => throw!(Exc::Rt(e.message().into(), None)),
                            }
                        };
                    }
                    // One arm per operand pair, so each inlines with both
                    // subtypes known.
                    match (&reg!($a), &reg!($b)) {
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
                            let (x, y) = (reg!($a), reg!($b));
                            reg!($dst) = tri!(ops::arith_slow(&self.heap, $op, x, y))
                        }
                    }
                }};
            }
            macro_rules! compare_jump {
                ($a:expr, $b:expr, $off:expr, $want:expr, $slow:path, |$i:ident, $j:ident| $cmp:expr) => {{
                    let c = match (&reg!($a), &reg!($b)) {
                        (&Value::Int($i), &Value::Int($j)) => $cmp,
                        (&Value::Float($i), &Value::Float($j)) => $cmp,
                        _ => {
                            let (x, y) = (reg!($a), reg!($b));
                            tri!($slow(&self.heap, x, y))
                        }
                    };
                    if c == $want {
                        jump!($off);
                    }
                }};
            }
            macro_rules! index_error {
                ($v:expr) => {
                    throw!(index_error($v))
                };
            }
            macro_rules! call {
                ($f:expr, $nargs:expr, $want:expr) => {{
                    // `f` is the function's register, `fa` its stack index.
                    let f: usize = $f;
                    let fa = base + f;
                    let n = $nargs;
                    save_pc!();
                    let want: u8 = $want;
                    match w[f] {
                        Value::Closure(c) => {
                            // A function calling itself: the running frame
                            // keeps `closure` alive, so an equal handle is
                            // the same closure, and its code is `cur`.
                            let e = if c == closure {
                                tri!(enter_lua_frame(&mut self.rt, &cur, c, fa, n, want))
                            } else {
                                tri!(self.push_lua_frame(c, fa, n, want))
                            };
                            depth += 1;
                            closure = c;
                            base = e.base;
                            cell_base = e.cell_base;
                            nvarargs = e.nvarargs;
                            pc = 0;
                            switch_code!(c, e.code_id);
                            w = window_sized!();
                        }
                        // A native's fast path, when it has one and takes
                        // these arguments: no window, no frame.
                        Value::Native(id)
                            if (want <= 1 || want == MULTI)
                                && let Some(fast) = self.rt.natives[id.0 as usize].fast
                                && let Some(v) = w.get(f + 1..f + 1 + n).and_then(fast) =>
                        {
                            if want != 0 {
                                w[f] = v;
                            }
                            self.rt.top = fa + want.min(1) as usize;
                        }
                        _ => match self.call_value(fa, n, want, stop) {
                            Ok(Flow::Reload) if self.rt.stack.frames.len() == depth => {
                                w = window!();
                                safepoint!();
                            }
                            other => flow!(other),
                        },
                    }
                }};
            }

            loop {
                let op = code[pc];
                pc += 1;
                match op {
                    Op::Move { dst, src } => reg!(dst) = reg!(src),
                    Op::LoadNil { dst, count } => {
                        let at = dst as usize;
                        for r in &mut w[at..at + count as usize] {
                            *r = Value::Nil;
                        }
                    }
                    Op::LoadBool { dst, value } => reg!(dst) = Value::Bool(value),
                    Op::LoadInt { dst, value } => reg!(dst) = Value::Int(value as i64),
                    Op::LoadConst { dst, k } => reg!(dst) = consts[k as usize],

                    Op::CellNew { cell, src } => {
                        let v = reg!(src);
                        self.op_cell_new(cell_base + cell as usize, v);
                        w = window!();
                        safepoint!();
                    }
                    Op::CellGet { dst, cell } => {
                        let c = self.rt.stack.cells[cell_base + cell as usize];
                        reg!(dst) = self.heap.get(c).value;
                    }
                    Op::CellSet { cell, src } => {
                        let c = self.rt.stack.cells[cell_base + cell as usize];
                        let v = reg!(src);
                        self.heap.update(c, |c| c.value = v);
                    }
                    Op::CapGet { dst, cap } => {
                        let c = self.heap.get(closure).captures[cap as usize];
                        reg!(dst) = self.heap.get(c).value;
                    }
                    Op::CapSet { cap, src } => {
                        let c = self.heap.get(closure).captures[cap as usize];
                        let v = reg!(src);
                        self.heap.update(c, |c| c.value = v);
                    }

                    Op::GetGlobal { dst, name, ic } => {
                        let env_cell = self.heap.get(closure).captures[0];
                        let Value::Str(k) = consts[name as usize] else {
                            unreachable!("global names are strings")
                        };
                        let v = match self.heap.get(env_cell).value {
                            Value::Table(t) => {
                                let cache = &caches[ic as usize];
                                match self.heap.get(t).cached_str(cache.get() as usize, k) {
                                    Some(v) => v,
                                    None => {
                                        let v = self.get_str_miss(t, k, cache);
                                        w = window!();
                                        v
                                    }
                                }
                            }
                            other => throw!(env_error(other)),
                        };
                        reg!(dst) = v;
                    }
                    Op::SetGlobal { src, name, ic } => {
                        let env_cell = self.heap.get(closure).captures[0];
                        let Value::Str(k) = consts[name as usize] else {
                            unreachable!("global names are strings")
                        };
                        let v = reg!(src);
                        match self.heap.get(env_cell).value {
                            Value::Table(t) => {
                                let cache = &caches[ic as usize];
                                let slot = cache.get() as usize;
                                if self.heap.get(t).cached_str(slot, k).is_some() {
                                    self.heap.barrier_back(t);
                                    self.heap.get_mut(t).set_slot(slot, v);
                                } else {
                                    self.set_str_miss(t, k, v, cache);
                                    w = window!();
                                    safepoint!();
                                }
                            }
                            other => throw!(env_error(other)),
                        }
                    }

                    Op::NewTable {
                        dst,
                        array_hint,
                        hash_hint,
                    } => {
                        let t = self.op_new_table(array_hint as usize, hash_hint as usize);
                        w = window!();
                        reg!(dst) = t;
                        safepoint!();
                    }
                    Op::GetField { dst, obj, key, ic } => {
                        let Value::Str(k) = consts[key as usize] else {
                            unreachable!("field keys are strings")
                        };
                        let &Value::Table(t) = &reg!(obj) else {
                            let o = reg!(obj);
                            index_error!(o)
                        };
                        let cache = &caches[ic as usize];
                        match self.heap.get(t).cached_str(cache.get() as usize, k) {
                            Some(v) => reg!(dst) = v,
                            None => {
                                let v = self.get_str_miss(t, k, cache);
                                w = window!();
                                reg!(dst) = v;
                            }
                        }
                    }
                    Op::SetField { obj, key, src, ic } => {
                        let Value::Str(k) = consts[key as usize] else {
                            unreachable!("field keys are strings")
                        };
                        let &Value::Table(t) = &reg!(obj) else {
                            let o = reg!(obj);
                            index_error!(o)
                        };
                        let v = reg!(src);
                        let cache = &caches[ic as usize];
                        let slot = cache.get() as usize;
                        if self.heap.get(t).cached_str(slot, k).is_some() {
                            // The key is there: overwrite its value. No
                            // capacity changes, so no byte accounting.
                            self.heap.barrier_back(t);
                            self.heap.get_mut(t).set_slot(slot, v);
                        } else {
                            self.set_str_miss(t, k, v, cache);
                            w = window!();
                            safepoint!();
                        }
                    }
                    Op::GetIndex { dst, obj, key } => {
                        let &Value::Table(t) = &reg!(obj) else {
                            let o = reg!(obj);
                            index_error!(o)
                        };
                        let tab = self.heap.get(t);
                        match reg!(key) {
                            Value::Int(i) => tab.read_int(i, &mut reg!(dst)),
                            k => reg!(dst) = tab.get(&self.heap, k),
                        }
                    }
                    Op::SetIndex { obj, key, src } => {
                        let &Value::Table(t) = &reg!(obj) else {
                            let o = reg!(obj);
                            index_error!(o)
                        };
                        let v = reg!(src);
                        match reg!(key) {
                            Value::Int(i) => table::set_int(&mut self.heap, t, i, v),
                            k => {
                                if let Err(e) = table::set(&mut self.heap, t, k, v) {
                                    throw!(Exc::Rt(e.message().into(), None));
                                }
                            }
                        }
                        safepoint!();
                    }
                    Op::SetList {
                        table: tr,
                        first,
                        count,
                        start,
                    } => {
                        let Value::Table(t) = reg!(tr) else {
                            unreachable!("SetList on a table")
                        };
                        let from = base + first as usize;
                        let n = if count == MULTI {
                            self.rt.top - from
                        } else {
                            count as usize
                        };
                        // Open-ended results can run past the window.
                        self.op_set_list(t, from, n, start as i64);
                        w = window!();
                        safepoint!();
                    }
                    Op::SelfCall { dst, obj, key } => {
                        let o = reg!(obj);
                        let Value::Str(k) = consts[key as usize] else {
                            unreachable!("method names are strings")
                        };
                        let Value::Table(t) = o else { index_error!(o) };
                        let m = self.heap.get(t).get_str(&self.heap, k);
                        reg!(dst + 1) = o;
                        reg!(dst) = m;
                    }

                    Op::Add { dst, a, b } => arith!(dst, a, b, Arith::Add, ArithOp::Add),
                    Op::Sub { dst, a, b } => arith!(dst, a, b, Arith::Sub, ArithOp::Sub),
                    Op::Mul { dst, a, b } => arith!(dst, a, b, Arith::Mul, ArithOp::Mul),
                    Op::Div { dst, a, b } => arith!(dst, a, b, Arith::Div, ArithOp::Div),
                    Op::Pow { dst, a, b } => arith!(dst, a, b, Arith::Pow, ArithOp::Pow),
                    Op::IDiv { dst, a, b } => arith!(dst, a, b, Arith::IDiv, ArithOp::IDiv),
                    Op::Mod { dst, a, b } => arith!(dst, a, b, Arith::Mod, ArithOp::Mod),
                    Op::AddInt { dst, a, imm } => match reg!(a) {
                        Value::Int(i) => reg!(dst) = Value::Int(i.wrapping_add(imm as i64)),
                        Value::Float(f) => reg!(dst) = Value::Float(f + imm as f64),
                        _ => {
                            let x = reg!(a);
                            reg!(dst) = tri!(ops::arith_slow(
                                &self.heap,
                                Arith::Add,
                                x,
                                Value::Int(imm as i64)
                            ))
                        }
                    },
                    Op::Neg { dst, src } => match reg!(src) {
                        Value::Int(i) => reg!(dst) = Value::Int(i.wrapping_neg()),
                        Value::Float(f) => reg!(dst) = Value::Float(-f),
                        _ => {
                            let x = reg!(src);
                            reg!(dst) = tri!(ops::arith_slow(&self.heap, Arith::Neg, x, x))
                        }
                    },
                    Op::Not { dst, src } => reg!(dst) = Value::Bool(reg!(src).is_falsy()),
                    Op::Len { dst, src } => {
                        let x = reg!(src);
                        let v = tri!(self.op_len(x));
                        w = window!();
                        reg!(dst) = v;
                    }
                    Op::Concat { dst, first, count } => {
                        let v = tri!(self.op_concat(base + first as usize, count as usize));
                        w = window!();
                        reg!(dst) = v;
                        safepoint!();
                    }

                    Op::Jump { off } => jump!(off),
                    Op::JumpIfTrue { src, off } => {
                        if !reg!(src).is_falsy() {
                            jump!(off);
                        }
                    }
                    Op::JumpIfFalse { src, off } => {
                        if reg!(src).is_falsy() {
                            jump!(off);
                        }
                    }
                    Op::JumpLt { a, b, off } => {
                        compare_jump!(a, b, off, true, ops::less_than, |x, y| x < y)
                    }
                    Op::JumpNotLt { a, b, off } => {
                        compare_jump!(a, b, off, false, ops::less_than, |x, y| x < y)
                    }
                    Op::JumpLe { a, b, off } => {
                        compare_jump!(a, b, off, true, ops::less_equal, |x, y| x <= y)
                    }
                    Op::JumpNotLe { a, b, off } => {
                        compare_jump!(a, b, off, false, ops::less_equal, |x, y| x <= y)
                    }
                    Op::JumpEq { a, b, off } => {
                        let (x, y) = (reg!(a), reg!(b));
                        if values_equal(&self.heap, x, y) {
                            jump!(off);
                        }
                    }
                    Op::JumpNe { a, b, off } => {
                        let (x, y) = (reg!(a), reg!(b));
                        if !values_equal(&self.heap, x, y) {
                            jump!(off);
                        }
                    }

                    Op::Call {
                        func,
                        nargs,
                        nresults,
                    } => {
                        let fa = base + func as usize;
                        let n = if nargs == MULTI {
                            self.rt.top - fa - 1
                        } else {
                            nargs as usize
                        };
                        call!(func as usize, n, nresults);
                    }
                    Op::TailCall { func, nargs } => {
                        let fa = base + func as usize;
                        let n = if nargs == MULTI {
                            self.rt.top - fa - 1
                        } else {
                            nargs as usize
                        };
                        if let Value::Closure(c) = w[func as usize] {
                            let f = self.rt.stack.frames.pop().expect("frame");
                            self.rt.stack.cells.truncate(f.cell_base as usize);
                            let dst = f.func as usize;
                            self.rt.stack.regs.copy_within(fa..fa + 1 + n, dst);
                            if let Err(x) = self.push_lua_frame(c, dst, n, f.want) {
                                self.rt.stack.frames.push(f);
                                throw!(x);
                            }
                            continue 'reload;
                        }
                        // A native in tail position: an ordinary call whose
                        // results the following `Return` delivers.
                        call!(func as usize, n, MULTI);
                    }
                    Op::Return { first, count } => {
                        let src = base + first as usize;
                        let n = if count == MULTI {
                            self.rt.top - src
                        } else {
                            count as usize
                        };
                        flow!(self.do_return(src, n, stop));
                    }
                    Op::Return0 => flow!(self.do_return(base, 0, stop)),
                    Op::Return1 { src } => {
                        // The common case, one value back to a Lua caller
                        // that wants one, without the general return path.
                        // The caller's window lies below this one, so the
                        // stack is already long enough for it.
                        if let [.., caller, f] = &self.rt.stack.frames[..]
                            && f.want == 1
                            && depth > stop + 1
                            && let FrameKind::Lua {
                                closure: c,
                                code_id,
                            } = caller.kind
                        {
                            let (func, own_cells) = (f.func as usize, f.cell_base as usize);
                            let v = w[src as usize];
                            base = caller.base as usize;
                            cell_base = caller.cell_base as usize;
                            nvarargs = caller.nvarargs as usize;
                            pc = caller.pc as usize;
                            self.rt.stack.frames.pop();
                            if self.rt.stack.cells.len() > own_cells {
                                self.rt.stack.cells.truncate(own_cells);
                            }
                            self.rt.stack.regs[func] = v;
                            self.rt.top = func + 1;
                            depth -= 1;
                            closure = c;
                            switch_code!(c, code_id);
                            w = window_sized!();
                            continue;
                        }
                        flow!(self.do_return(base + src as usize, 1, stop))
                    }
                    Op::Vararg { dst, count } => {
                        self.op_vararg(base, nvarargs, dst as usize, count);
                        w = window!();
                    }

                    Op::ForPrep { base: b, exit } => {
                        let runs = tri!(self.op_for_prep(base + b as usize));
                        w = window!();
                        if !runs {
                            jump!(exit);
                        }
                    }
                    Op::ForLoop { base: b, back } => match reg!(b + 1) {
                        Value::Int(count) => {
                            if count > 0 {
                                let (&Value::Int(cur), &Value::Int(s)) = (&reg!(b), &reg!(b + 2))
                                else {
                                    unreachable!("integer loop state")
                                };
                                let next = Value::Int(cur.wrapping_add(s));
                                reg!(b) = next;
                                reg!(b + 1) = Value::Int(count - 1);
                                reg!(b + 3) = next;
                                jump!(back);
                            }
                        }
                        Value::Float(lim) => {
                            let (&Value::Float(cur), &Value::Float(s)) = (&reg!(b), &reg!(b + 2))
                            else {
                                unreachable!("float loop state")
                            };
                            let next = cur + s;
                            if if s > 0.0 { next <= lim } else { lim <= next } {
                                reg!(b) = Value::Float(next);
                                reg!(b + 3) = Value::Float(next);
                                jump!(back);
                            }
                        }
                        _ => unreachable!("loop state"),
                    },
                    Op::GenericCall { base: b, nvars } => {
                        let at = b as usize;
                        w.copy_within(at..at + 3, at + 4);
                        call!(at + 4, 2, nvars);
                    }
                    Op::GenericLoop { base: b, back } => {
                        let v = reg!(b + 4);
                        if !v.is_nil() {
                            reg!(b + 2) = v;
                            jump!(back);
                        }
                    }

                    Op::Closure { dst, proto } => {
                        let c = self.op_closure(closure, cell_base, proto as usize);
                        w = window!();
                        reg!(dst) = c;
                        safepoint!();
                    }
                }
            }
        }
    }
}

#[inline(always)]
fn values_equal(heap: &orbit_core::LuaHeap, x: Value, y: Value) -> bool {
    match (x, y) {
        (Value::Int(i), Value::Int(j)) => i == j,
        (Value::Float(f), Value::Float(g)) => f == g,
        _ => ops::equal(heap, x, y),
    }
}
