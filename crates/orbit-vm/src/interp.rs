//! The dispatch loop, calls, returns and error unwinding.

use std::rc::Rc;

use orbit_bytecode::number::BitOp;
use orbit_bytecode::{MULTI, Op, Origin};
use orbit_core::number::{ArithOp, Number};
use orbit_core::table::{self, IntStore};
use orbit_core::{Closure, Gc, ProtoCode, Value};

use crate::cold::{already_defined, env_error, get_str_miss, op_len, set_str_miss};
use crate::meta::Event;
use crate::native::{CallOutcome, NativeCtx, NativeReturn};
use crate::ops::{self, Arith, Exc};
use crate::{Frame, FrameKind, LuaError, Vm};

/// Registers a frame can address: register numbers are `u8`.
const WINDOW: usize = 256;

/// Where a new Lua frame's state lives, so the dispatch loop can enter it
/// without reading the frame back.
struct Entered {
    base: usize,
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
        mut nargs: usize,
        want: u8,
        stop: usize,
    ) -> Result<Flow, Exc> {
        let mut chain = 0;
        loop {
            match self.rt.stack.regs[func] {
                Value::Closure(_) | Value::Native(_) => break,
                v => {
                    // `__call`: the handler is called with the value first.
                    let h = self.rt.metamethod(&self.heap, v, Event::Call);
                    if h.is_nil() {
                        return Err(Exc::Rt(
                            format!("attempt to call a {} value", v.type_name()),
                            Some(0),
                        ));
                    }
                    if chain == crate::meta::MAX_CHAIN {
                        return Err(Exc::Plain("'__call' chain too long".into()));
                    }
                    let regs = &mut self.rt.stack.regs;
                    regs.truncate(func + 1 + nargs);
                    regs.insert(func, h);
                    nargs += 1;
                    chain += 1;
                }
            }
        }
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
            _ => unreachable!("a function"),
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
        let shape = Shape::of(&self.heap.get(c).code);
        enter_lua_frame(&mut self.rt, shape, c, func, nargs, want)
    }
}

/// String constant `k`, for the miss paths of field and global accesses.
#[inline(always)]
fn const_str(consts: &[Value], k: u16) -> Gc<orbit_core::LuaStr> {
    match consts[k as usize] {
        Value::Str(s) => s,
        _ => unreachable!("field keys are strings"),
    }
}

impl Vm {
    /// The shape of Lua closure `c`, from the one-entry callee cache.
    #[inline(always)]
    fn callee_shape(&mut self, c: Gc<Closure>) -> Shape {
        match self.rt.stack.callee {
            // The cache roots its closure, so an equal handle is the same
            // closure.
            Some((cached, shape)) if cached == c => shape,
            _ => self.callee_miss(c),
        }
    }

    #[cold]
    #[inline(never)]
    fn callee_miss(&mut self, c: Gc<Closure>) -> Shape {
        let shape = Shape::of(&self.heap.get(c).code);
        self.rt.stack.callee = Some((c, shape));
        shape
    }
}

/// What entering a frame needs from its code, copied out so a cache can
/// hold it without borrowing the code.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Shape {
    code_id: usize,
    nparams: u8,
    is_vararg: bool,
    nregs: u8,
    ncells: u8,
}

impl Shape {
    #[inline(always)]
    pub(crate) fn of(proto: &ProtoCode) -> Shape {
        Shape {
            code_id: proto as *const ProtoCode as usize,
            nparams: proto.nparams,
            is_vararg: proto.is_vararg,
            nregs: proto.nregs,
            ncells: proto.ncells,
        }
    }
}

/// Push a frame for Lua closure `c` running `proto`: the body of
/// [`Vm::push_lua_frame`], for callers that already hold the code.
#[inline(always)]
fn enter_lua_frame(
    rt: &mut crate::Runtime,
    proto: Shape,
    c: Gc<Closure>,
    func: usize,
    nargs: usize,
    want: u8,
) -> Result<Entered, Exc> {
    let code_id = proto.code_id;
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
    Ok(Entered { base, code_id })
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
            FrameKind::Meta { .. } => self.meta_returned(stop),
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
            Exc::Meta(_) | Exc::Index { .. } | Exc::NewIndex { .. } => {
                unreachable!("metamethod requests are finished by `raise`")
            }
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
                        let described = format!(" ({kind} '{}')", String::from_utf8_lossy(name));
                        // `number (local 'x') has no integer representation`: the name goes
                        // after the subject, not at the end.
                        match msg.strip_prefix("number ") {
                            Some(rest) if msg == ops::NO_INTEGER_REP => {
                                text.truncate(text.len() - msg.len());
                                text.push_str("number");
                                text.push_str(&described);
                                text.push(' ');
                                text.push_str(rest);
                            }
                            _ => text.push_str(&described),
                        }
                    }
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
        let frames = &self.rt.stack.frames[stop..];
        for (i, f) in frames.iter().enumerate().rev() {
            // A function called as a metamethod sits on a meta frame.
            let as_meta = match i.checked_sub(1).map(|j| frames[j].kind) {
                Some(FrameKind::Meta { event, .. }) => Some(event),
                _ => None,
            };
            match f.kind {
                FrameKind::Meta { .. } => {}
                FrameKind::Lua { .. } | FrameKind::Native { .. } if as_meta.is_some() => {
                    let line = match f.kind {
                        FrameKind::Lua { closure, .. } => {
                            let proto = self.heap.get(self.heap.get(closure).proto);
                            format!(
                                "{}:{}",
                                proto.chunk,
                                proto.debug.line(f.pc.saturating_sub(1))
                            )
                        }
                        _ => "[C]".into(),
                    };
                    let e = as_meta.expect("meta").short_name();
                    s.push_str(&format!("\n\t{line}: in metamethod '{e}'"));
                }
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
            let mut pc = frame.pc as usize;
            // Both the frame's code and `cur` are alive, so equal addresses
            // mean the same code.
            if code_id != Rc::as_ptr(&cur) as usize {
                cur = self.heap.get(closure).code.clone();
            }
            let mut code = &cur.code[..];
            macro_rules! switch_code {
                ($closure:expr, $code_id:expr) => {
                    // A recursive call or its return keeps the same code.
                    if $code_id != Rc::as_ptr(&cur) as usize {
                        match &mut self.rt.stack.prev_code {
                            // `prev_code` keeps its code alive, so an equal
                            // address is the same code.
                            Some(p) if Rc::as_ptr(p) as usize == $code_id => {
                                std::mem::swap(&mut cur, p)
                            }
                            _ => {
                                let new = self.heap.get($closure).code.clone();
                                self.rt.stack.prev_code = Some(std::mem::replace(&mut cur, new));
                            }
                        }
                        code = &cur.code[..];
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
            // The running frame's cell base and vararg count, for the few
            // instructions that need them; kept in the frame rather than in
            // locals so the hot loop has more machine registers.
            macro_rules! cell_base {
                () => {
                    self.rt.stack.frames[depth - 1].cell_base as usize
                };
            }
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
            // Run `$body` with `$v` bound to `reg!($src)`, reading a number
            // by parts. Arithmetic writes its result as a tag store and a
            // payload store, and a whole 16-byte load of that value soon
            // after would stall on store forwarding; with the variant known,
            // `$body` stores it by parts too.
            macro_rules! by_parts {
                ($src:expr, |$v:ident| $body:expr) => {
                    match reg!($src) {
                        Value::Int(x) => {
                            let $v = Value::Int(x);
                            $body
                        }
                        Value::Float(f) => {
                            let $v = Value::Float(f);
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
                            reg!($dst) =
                                tri!(ops::arith_slow(&self.heap, &self.rt.metas, $op, x, y))
                        }
                    }
                }};
            }
            macro_rules! bitwise {
                ($dst:expr, $a:expr, $b:expr, $op:expr) => {{
                    match (&reg!($a), &reg!($b)) {
                        (&Value::Int(i), &Value::Int(j)) => {
                            reg!($dst) = Value::Int($op.apply(i, j))
                        }
                        _ => {
                            let (x, y) = (reg!($a), reg!($b));
                            reg!($dst) = tri!(ops::bitwise_slow(&self.heap, $op, x, y))
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
                            tri!($slow(&self.heap, &self.rt.metas, x, y))
                        }
                    };
                    if c == $want {
                        jump!($off);
                    }
                }};
            }
            macro_rules! compare_int_jump {
                // `$slow` names an `ops` comparison and its operand order,
                // with `x` the register and `k` the immediate as values.
                ($a:expr, $imm:expr, $want:expr, $off:expr, |$x:ident, $k:ident| $cmp:expr,
                 $slow:ident($s1:ident, $s2:ident)) => {{
                    let c = match reg!($a) {
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
                ($dst:expr, $obj:expr, $key:expr) => {{
                    let _ = $dst;
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
                            let shape = if c == closure {
                                Shape::of(&cur)
                            } else {
                                self.callee_shape(c)
                            };
                            let e = tri!(enter_lua_frame(&mut self.rt, shape, c, fa, n, want));
                            depth += 1;
                            closure = c;
                            base = e.base;
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
                    // Numbers are copied by parts: the source was usually
                    // just written that way, and a whole 16-byte load would
                    // stall on store forwarding.
                    Op::Move { dst, src } => by_parts!(src, |v| reg!(dst) = v),
                    Op::LoadNil { dst, count } => {
                        let at = dst as usize;
                        for r in &mut w[at..at + count as usize] {
                            *r = Value::Nil;
                        }
                    }
                    Op::LoadBool { dst, value } => reg!(dst) = Value::Bool(value),
                    Op::LoadInt { dst, value } => reg!(dst) = Value::Int(value as i64),
                    Op::LoadConst { dst, k } => reg!(dst) = cur.consts[k as usize],

                    Op::CellNew { cell, src } => {
                        let v = reg!(src);
                        self.op_cell_new(cell_base!() + cell as usize, v);
                        w = window!();
                        safepoint!();
                    }
                    Op::CellGet { dst, cell } => {
                        let c = self.rt.stack.cells[cell_base!() + cell as usize];
                        reg!(dst) = self.heap.get(c).value;
                    }
                    Op::CellSet { cell, src } => {
                        let c = self.rt.stack.cells[cell_base!() + cell as usize];
                        self.heap.barrier_back(c);
                        let cell = self.heap.get_mut(c);
                        by_parts!(src, |v| cell.value = v);
                    }
                    Op::CapGet { dst, cap } => {
                        let c = self.heap.get(closure).captures[cap as usize];
                        reg!(dst) = self.heap.get(c).value;
                    }
                    Op::CapSelf { dst } => reg!(dst) = Value::Closure(closure),
                    Op::CapSet { cap, src } => {
                        let c = self.heap.get(closure).captures[cap as usize];
                        self.heap.barrier_back(c);
                        let cell = self.heap.get_mut(c);
                        by_parts!(src, |v| cell.value = v);
                    }

                    Op::GetGlobal { dst, name, ic } => {
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
                                        tri!(get_str_miss(&self.heap, &self.rt.metas, t, k, cache))
                                    }
                                }
                            }
                            other => throw!(env_error(other)),
                        };
                        reg!(dst) = v;
                    }
                    Op::CheckUndefined { src, name } => {
                        if !matches!(reg!(src), Value::Nil) {
                            let k = const_str(&cur.consts, name);
                            throw!(already_defined(self.heap.get(k).as_bytes()));
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
                                    tri!(set_str_miss(
                                        &mut self.heap,
                                        &self.rt.metas,
                                        t,
                                        k,
                                        v,
                                        cache
                                    ));
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
                        let &Value::Table(t) = &reg!(obj) else {
                            get_slow!(dst, reg!(obj), cur.consts[key as usize])
                        };
                        let cache = &cur.caches[ic as usize];
                        let tab = self.heap.get(t);
                        match tab.cached_str(cache.slot.get() as usize, cache.key) {
                            // A dead entry is a miss when `__index` could
                            // apply.
                            Some(v) if !v.is_nil() || tab.meta.is_none() => reg!(dst) = v,
                            _ => {
                                let k = const_str(&cur.consts, key);
                                reg!(dst) =
                                    tri!(get_str_miss(&self.heap, &self.rt.metas, t, k, cache));
                            }
                        }
                    }
                    Op::SetField { obj, key, src, ic } => {
                        let &Value::Table(t) = &reg!(obj) else {
                            set_slow!(reg!(obj), cur.consts[key as usize], reg!(src))
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
                            let v = reg!(src);
                            tri!(set_str_miss(&mut self.heap, &self.rt.metas, t, k, v, cache));
                            safepoint!();
                        }
                    }
                    Op::GetIndex { dst, obj, key } => {
                        let &Value::Table(t) = &reg!(obj) else {
                            get_slow!(dst, reg!(obj), reg!(key))
                        };
                        let tab = self.heap.get(t);
                        // A nil result in a table with a metatable may
                        // come from `__index`. `dst` may be `obj` or `key`.
                        match reg!(key) {
                            Value::Int(i) => {
                                tab.read_int(i, &mut reg!(dst));
                                if tab.meta.is_some() && reg!(dst).is_nil() {
                                    get_slow!(dst, Value::Table(t), Value::Int(i))
                                }
                            }
                            k => {
                                reg!(dst) = tab.get(&self.heap, k);
                                if tab.meta.is_some() && reg!(dst).is_nil() {
                                    get_slow!(dst, Value::Table(t), k)
                                }
                            }
                        }
                    }
                    Op::SetIndex { obj, key, src } => {
                        let &Value::Table(t) = &reg!(obj) else {
                            set_slow!(reg!(obj), reg!(key), reg!(src))
                        };
                        match reg!(key) {
                            // A store into the array part allocates
                            // nothing, so it needs no safe point.
                            Value::Int(i) => match set_int_parts!(t, i, src) {
                                IntStore::Array => {}
                                IntStore::Other => safepoint!(),
                                IntStore::Meta => {
                                    set_slow!(reg!(obj), reg!(key), reg!(src))
                                }
                            },
                            k => {
                                let v = reg!(src);
                                if self.heap.get(t).meta.is_some() {
                                    set_slow!(reg!(obj), k, v)
                                }
                                if let Err(e) = table::set(&mut self.heap, t, k, v) {
                                    throw!(Exc::Rt(e.message().into(), None));
                                }
                                safepoint!();
                            }
                        }
                    }
                    Op::GetIndexInt { dst, obj, key } => {
                        let &Value::Table(t) = &reg!(obj) else {
                            get_slow!(dst, reg!(obj), Value::Int(key as i64))
                        };
                        let tab = self.heap.get(t);
                        tab.read_int(key as i64, &mut reg!(dst));
                        if tab.meta.is_some() && reg!(dst).is_nil() {
                            get_slow!(dst, Value::Table(t), Value::Int(key as i64))
                        }
                    }
                    Op::SetIndexInt { obj, key, src } => {
                        let &Value::Table(t) = &reg!(obj) else {
                            set_slow!(reg!(obj), Value::Int(key as i64), reg!(src))
                        };
                        match set_int_parts!(t, key as i64, src) {
                            IntStore::Array => {}
                            IntStore::Other => safepoint!(),
                            IntStore::Meta => {
                                set_slow!(reg!(obj), Value::Int(key as i64), reg!(src))
                            }
                        }
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
                        let Value::Str(k) = cur.consts[key as usize] else {
                            unreachable!("method names are strings")
                        };
                        reg!(dst + 1) = o;
                        let Value::Table(t) = o else {
                            get_slow!(dst, o, Value::Str(k))
                        };
                        let tab = self.heap.get(t);
                        let m = tab.get_str(&self.heap, k);
                        if m.is_nil() && tab.meta.is_some() {
                            get_slow!(dst, o, Value::Str(k))
                        }
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
                                &self.rt.metas,
                                Arith::Add,
                                x,
                                Value::Int(imm as i64)
                            ))
                        }
                    },
                    Op::BAnd { dst, a, b } => bitwise!(dst, a, b, BitOp::And),
                    Op::BOr { dst, a, b } => bitwise!(dst, a, b, BitOp::Or),
                    Op::BXor { dst, a, b } => bitwise!(dst, a, b, BitOp::Xor),
                    Op::Shl { dst, a, b } => bitwise!(dst, a, b, BitOp::Shl),
                    Op::Shr { dst, a, b } => bitwise!(dst, a, b, BitOp::Shr),
                    Op::BNot { dst, src } => match reg!(src) {
                        Value::Int(i) => reg!(dst) = Value::Int(!i),
                        _ => {
                            let x = reg!(src);
                            reg!(dst) = tri!(ops::bnot_slow(&self.heap, x))
                        }
                    },
                    Op::Neg { dst, src } => match reg!(src) {
                        Value::Int(i) => reg!(dst) = Value::Int(i.wrapping_neg()),
                        Value::Float(f) => reg!(dst) = Value::Float(-f),
                        _ => {
                            let x = reg!(src);
                            reg!(dst) = tri!(ops::arith_slow(
                                &self.heap,
                                &self.rt.metas,
                                Arith::Neg,
                                x,
                                x
                            ))
                        }
                    },
                    Op::Not { dst, src } => reg!(dst) = Value::Bool(reg!(src).is_falsy()),
                    Op::Len { dst, src } => {
                        let x = reg!(src);
                        reg!(dst) = tri!(op_len(&self.heap, &self.rt.metas, x));
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
                        if tri!(values_equal(&self.heap, &self.rt.metas, x, y)) {
                            jump!(off);
                        }
                    }
                    Op::JumpNe { a, b, off } => {
                        let (x, y) = (reg!(a), reg!(b));
                        if !tri!(values_equal(&self.heap, &self.rt.metas, x, y)) {
                            jump!(off);
                        }
                    }
                    Op::JumpEqInt { a, imm, want, off } => {
                        let c = match reg!(a) {
                            Value::Int(i) => i == imm as i64,
                            Value::Float(f) => f == imm as f64,
                            _ => false,
                        };
                        if c == want {
                            jump!(off);
                        }
                    }
                    Op::JumpLtInt { a, imm, want, off } => {
                        compare_int_jump!(a, imm, want, off, |x, k| x < k, less_than(x, k))
                    }
                    Op::JumpLeInt { a, imm, want, off } => {
                        compare_int_jump!(a, imm, want, off, |x, k| x <= k, less_equal(x, k))
                    }
                    Op::JumpGtInt { a, imm, want, off } => {
                        compare_int_jump!(a, imm, want, off, |x, k| x > k, less_than(k, x))
                    }
                    Op::JumpGeInt { a, imm, want, off } => {
                        compare_int_jump!(a, imm, want, off, |x, k| x >= k, less_equal(k, x))
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
                            // Numbers are copied by parts: the value was
                            // usually just written that way, and a whole
                            // 16-byte load would stall on store forwarding.
                            let v = match w[src as usize] {
                                Value::Float(f) => Ok(f),
                                Value::Int(i) => Err(Ok(i)),
                                v => Err(Err(v)),
                            };
                            base = caller.base as usize;
                            pc = caller.pc as usize;
                            self.rt.stack.frames.pop();
                            if self.rt.stack.cells.len() > own_cells {
                                self.rt.stack.cells.truncate(own_cells);
                            }
                            let out = &mut self.rt.stack.regs[func];
                            match v {
                                Ok(f) => *out = Value::Float(f),
                                Err(Ok(i)) => *out = Value::Int(i),
                                Err(Err(v)) => *out = v,
                            }
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
                        let nvarargs = self.rt.stack.frames[depth - 1].nvarargs as usize;
                        self.op_vararg(base, nvarargs, dst as usize, count);
                        w = window!();
                    }
                    Op::VarargTable { dst } => {
                        let nvarargs = self.rt.stack.frames[depth - 1].nvarargs as usize;
                        let t = self.op_vararg_table(base, nvarargs);
                        w = window!();
                        reg!(dst) = t;
                        safepoint!();
                    }
                    Op::VarargGet { dst, key } => {
                        let nvarargs = self.rt.stack.frames[depth - 1].nvarargs as usize;
                        let k = reg!(key);
                        let v = self.op_vararg_get(base, nvarargs, k);
                        w = window!();
                        reg!(dst) = v;
                    }
                    Op::VarargUnpack { dst, table, count } => {
                        let t = reg!(table);
                        tri!(self.op_vararg_unpack(base, t, dst as usize, count));
                        w = window!();
                    }

                    Op::ForPrep { base: b, exit } => {
                        // The common integer loop counting up; the rest
                        // (floats, negative steps, errors) is cold.
                        if let (&Value::Int(i), &Value::Int(lim), &Value::Int(s)) =
                            (&reg!(b), &reg!(b + 1), &reg!(b + 2))
                            && s > 0
                        {
                            if i <= lim {
                                // The iterations left replace the limit.
                                let count = (lim as u64).wrapping_sub(i as u64) / s as u64;
                                reg!(b + 1) = Value::Int(count as i64);
                                reg!(b + 3) = Value::Int(i);
                            } else {
                                jump!(exit);
                            }
                        } else {
                            let runs = tri!(self.op_for_prep(base + b as usize));
                            w = window!();
                            if !runs {
                                jump!(exit);
                            }
                        }
                    }
                    Op::ForLoop { base: b, back } => match reg!(b + 1) {
                        Value::Int(_) => {
                            // Update the counters' payloads in place: their
                            // tags are already right, so only the payloads
                            // need stores.
                            let at = b as usize;
                            let [Value::Int(cur), Value::Int(count), Value::Int(s), var] =
                                &mut w[at..at + 4]
                            else {
                                unreachable!("integer loop state")
                            };
                            if *count > 0 {
                                *count -= 1;
                                *cur = cur.wrapping_add(*s);
                                *var = Value::Int(*cur);
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
                        let c = self.op_closure(closure, cell_base!(), proto as usize);
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
fn values_equal(
    heap: &orbit_core::LuaHeap,
    rt: &crate::meta::Metas,
    x: Value,
    y: Value,
) -> Result<bool, Exc> {
    match (x, y) {
        (Value::Int(i), Value::Int(j)) => Ok(i == j),
        (Value::Float(f), Value::Float(g)) => Ok(f == g),
        _ => ops::equal(heap, rt, x, y),
    }
}
