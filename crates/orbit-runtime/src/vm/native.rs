//! The native-function interface (ADR-3's continuation style).

use std::rc::Rc;

use crate::locale::Locale;
use crate::object::number::float_to_int;
use crate::object::{
    Cell, Closure, Gc, LuaStr, NativeId, OrbitStr, OrbitString, Proto, ProtoCode, Table, Value,
};
use orbit_bytecode::{FunctionName, Op, Origin};

use crate::vm::interp::Flow;
use crate::vm::meta::{self, MetaField};
use crate::vm::ops::Exc;
use crate::vm::{FrameKind, LuaError, Vm, ops};

/// The outcome of an operation that may need a metamethod: either the
/// answer, or a function to call. For [`Meta::Call`] the arguments are
/// already pushed on the window; return
/// `NativeReturn::CallThen { f, nargs, k, .. }` and take the result in `k`.
#[derive(Debug, Clone, Copy)]
pub enum Meta<T> {
    Done(T),
    Call { f: Value, nargs: usize },
}

/// An operator for [`NativeCtx::arith`], in the C API's order
/// (`LUA_OPADD` is 0 .. `LUA_OPBNOT` is 13).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Operator {
    Add,
    Sub,
    Mul,
    Mod,
    Pow,
    Div,
    IDiv,
    BAnd,
    BOr,
    BXor,
    Shl,
    Shr,
    Unm,
    BNot,
}

/// A native function.
pub type NativeFn = fn(&mut NativeCtx<'_>) -> Result<NativeReturn, LuaError>;

/// An optional fast path for a native that returns one value and needs
/// nothing but its arguments and a look at the heap (most of `math`, and
/// the intrinsics built-ins call most). It gets the arguments and returns
/// the result, or `None` to run the full native instead (wrong types,
/// errors, anything unusual), so it never changes behavior. It cannot
/// allocate, call back into Lua, or fail.
pub type FastFn = fn(&crate::object::LuaHeap, &[Value]) -> Option<Value>;

/// What a native does after a call it requested with
/// [`NativeReturn::CallThen`] finishes.
pub type Continuation = fn(&mut NativeCtx<'_>, CallOutcome) -> Result<NativeReturn, LuaError>;

/// How a requested call ended.
#[derive(Debug, Clone, Copy)]
pub enum CallOutcome {
    /// It returned this many values, now the last values of the window.
    Ok(usize),
    /// It raised this error (only for `protected` calls). The window is as
    /// it was when the call was requested, minus the call's arguments.
    Err(Value),
}

/// What a [`NativeReturn::Resume`] does when the coroutine yields or
/// returns values. Anything but [`ResumeOk::Continue`] promises that `k`
/// would do exactly that with `Ok`, so the VM may skip calling it (it still
/// does when a return hook needs the native's frame).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResumeOk {
    /// Run the continuation.
    Continue,
    /// The values are the native's results (`coroutine.wrap`).
    Return,
    /// `true`, then the values, are the native's results
    /// (`coroutine.resume`).
    ReturnTrue,
}

/// What a native function returns.
#[derive(Debug, Clone, Copy)]
pub enum NativeReturn {
    /// The last `n` values of the window are the results.
    Return(usize),
    /// Call `f` with the last `nargs` values of the window, then run `k`
    /// with the outcome. The call's results replace its arguments at the end
    /// of the window. With `protected`, an error in the call goes to `k`
    /// instead of propagating. Code the call runs can't yield across it
    /// (`attempt to yield across a C-call boundary`), as for most lua5.5
    /// library callbacks; see [`NativeReturn::CallThenYieldable`].
    CallThen {
        f: Value,
        nargs: usize,
        k: Continuation,
        protected: bool,
    },
    /// [`NativeReturn::CallThen`], but code the call runs may yield across
    /// it: for the few library functions lua5.5 lets callbacks yield
    /// through (`pcall`, `pairs` calling `__pairs`, `dofile`).
    CallThenYieldable {
        f: Value,
        nargs: usize,
        k: Continuation,
        protected: bool,
    },
    /// [`NativeReturn::CallThen`] with `protected`, yieldable (only the
    /// call itself, not the handler), but an error first goes
    /// to the message handler in the window's first slot, which runs on top
    /// of the frames that raised (so it can see them); its result is the
    /// error `k` gets (`xpcall`).
    CallHandled {
        f: Value,
        nargs: usize,
        k: Continuation,
    },
    /// Resume coroutine `co` with the last `nargs` values of the window,
    /// then run `k` when it yields (`Ok` with the yielded values), returns
    /// (`Ok` with its results) or fails (`Err`). A coroutine that isn't
    /// suspended, or one nested too deeply, gets `k` an `Err` at once.
    /// `ok` lets the VM finish the `Ok` case itself, without calling `k`.
    Resume {
        co: Gc<crate::object::Thread>,
        nargs: usize,
        k: Continuation,
        ok: ResumeOk,
    },
    /// Close coroutine `co` (suspended, or dead): run its pending `__close`
    /// handlers on its stack, newest first, with its error (nil if it is
    /// suspended), then run `k`: `Ok` with no values, or `Err` with the error
    /// it died with or the last error a handler raised.
    Close {
        co: Gc<crate::object::Thread>,
        k: Continuation,
    },
    /// Yield the last `nvals` values of the window to the resumer. The
    /// values the next resume passes become this native's results.
    Yield { nvals: usize },
    /// [`NativeReturn::Yield`], then run `k` with the values the next
    /// resume passes (the C API's `lua_yieldk` with a continuation).
    YieldThen { nvals: usize, k: Continuation },
    /// End the running coroutine as if its body had returned nothing
    /// (`coroutine.close` of itself).
    Exit,
}

/// Largest scratch buffer ([`NativeCtx::take_buf`]) kept between calls.
const SCRATCH_KEEP: usize = 64 * 1024;

/// A native function's view of the VM: its window of the stack (arguments,
/// then whatever it pushes), the heap and the runtime.
#[derive(Debug)]
pub struct NativeCtx<'a> {
    /// The VM: its heap and runtime.
    pub vm: &'a mut crate::vm::Vm,
    pub(crate) base: usize,
    pub(crate) nargs: usize,
    pub(crate) id: NativeId,
    /// The native closure being called, for its upvalues.
    pub(crate) upvals: Option<Gc<crate::object::Closure>>,
}

impl NativeCtx<'_> {
    /// The running native's index in the registry.
    pub fn native_id(&self) -> NativeId {
        self.id
    }

    /// The data the running native was registered with
    /// ([`Vm::register_native_with_data`](crate::vm::Vm::register_native_with_data)).
    #[inline]
    pub fn native_data(&self) -> [u64; 2] {
        self.vm.rt.natives[self.id.0 as usize].data
    }

    /// The same window, borrowed for a shorter time: for host code that
    /// keeps a context in a value of its own while the native runs.
    pub fn reborrow(&mut self) -> NativeCtx<'_> {
        NativeCtx {
            vm: self.vm,
            base: self.base,
            nargs: self.nargs,
            id: self.id,
            upvals: self.upvals,
        }
    }

    /// Number of arguments the native was called with.
    #[inline]
    pub fn nargs(&self) -> usize {
        self.nargs
    }

    /// Where the window starts on its stack: tells this activation from
    /// others on the same thread, for state kept across continuations.
    #[inline]
    pub fn window_start(&self) -> usize {
        self.base
    }

    /// Values currently in the window.
    #[inline]
    pub fn len(&self) -> usize {
        self.vm.rt.top - self.base
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Window slot `i` (argument `i` until the native changes it), or nil.
    #[inline]
    pub fn arg(&self, i: usize) -> Value {
        let at = self.base + i;
        if at < self.vm.rt.top {
            self.vm.rt.stack.regs[at]
        } else {
            Value::Nil
        }
    }

    /// A count or position a native saved in window slot `i` as an
    /// integer, for its continuation to pick up; 0 if the slot holds
    /// anything else.
    pub fn saved_usize(&self, i: usize) -> usize {
        match self.arg(i) {
            Value::Int(n) => n as usize,
            _ => 0,
        }
    }

    /// Overwrite window slot `i`, growing the window with nils if needed.
    #[inline]
    pub fn set(&mut self, i: usize, v: Value) {
        let at = self.base + i;
        self.vm.rt.stack.dirty_to = self.vm.rt.stack.dirty_to.max(at + 1);
        if at >= self.vm.rt.top {
            self.grow_to(at);
        }
        self.vm.rt.stack.regs[at] = v;
    }

    /// Grow the window with nils up to absolute register `at`, inclusive.
    #[cold]
    #[inline(never)]
    fn grow_to(&mut self, at: usize) {
        let top = self.vm.rt.top;
        let regs = &mut self.vm.rt.stack.regs;
        crate::vm::interp::extend_nil(regs, at + 1);
        regs[top..at].fill(Value::Nil);
        self.vm.rt.top = at + 1;
    }

    /// Insert `v` at window slot `i`, shifting the rest up.
    pub fn insert(&mut self, i: usize, v: Value) {
        let at = self.base + i;
        self.vm.rt.top = self.vm.rt.stack.insert(at, self.vm.rt.top, v);
    }

    /// Push a value onto the end of the window.
    #[inline]
    pub fn push(&mut self, v: Value) {
        self.vm.rt.stack.put(self.vm.rt.top, v);
        self.vm.rt.top += 1;
    }

    /// Shrink the window to `len` values.
    pub fn truncate(&mut self, len: usize) {
        self.vm.rt.top = self.vm.rt.top.min(self.base + len);
    }

    /// Mark window slot `i` to be closed when the window shrinks below it
    /// or the native returns or fails, as the C API's `lua_toclose` does.
    /// Nil and false need no closing; anything else needs a `__close`.
    pub fn to_be_closed(&mut self, i: usize) -> Result<(), LuaError> {
        let v = self.arg(i);
        if matches!(v, Value::Nil | Value::Bool(false)) {
            return Ok(());
        }
        if self.metamethod(v, MetaField::Close).is_nil() {
            return Err(self.error("variable '(C temporary)' got a non-closable value"));
        }
        self.vm.rt.stack.aux.tbc.push((self.base + i) as u32);
        Ok(())
    }

    /// Whether window slot `i` is the newest variable marked to be closed.
    pub fn is_newest_to_be_closed(&self, i: usize) -> bool {
        self.vm.rt.stack.aux.tbc.last() == Some(&((self.base + i) as u32))
    }

    /// Close the window's variables marked to be closed at slot `from` and
    /// above, newest first: each one's `__close(value, nil)`. An error a
    /// handler raises stops it; the rest close as that error unwinds.
    pub fn close_from(&mut self, from: usize) -> Result<(), LuaError> {
        let at = self.base + from;
        while let Some(&t) = self.vm.rt.stack.aux.tbc.last() {
            if (t as usize) < at {
                break;
            }
            self.vm.rt.stack.aux.tbc.pop();
            let v = self.vm.rt.stack.regs[t as usize];
            let h = self.metamethod(v, MetaField::Close);
            let mark = self.len();
            self.push(h);
            self.push(v);
            self.push(Value::Nil);
            let r = self.call(2, None);
            self.truncate(mark);
            r?;
        }
        Ok(())
    }

    /// The window's values.
    pub fn window(&self) -> &[Value] {
        &self.vm.rt.stack.regs[self.base..self.vm.rt.top]
    }

    /// Call the value below the last `nargs` values of the window with
    /// them and run it to the end now. Its results replace it and its
    /// arguments at the end of the window; returns how many there are. On
    /// error the function and arguments are gone and the error is returned.
    /// With `handler`, an error first goes to the message handler in window
    /// slot `handler`, which runs on top of the frames that raised; its
    /// result is the error returned.
    ///
    /// For natives that can't be written in continuation style (the C API's
    /// `lua_call` and `lua_pcall`); prefer [`NativeReturn::CallThen`]. This
    /// nests the interpreter on the native stack, and code the call runs
    /// can't yield across it. While the call runs the native has a frame,
    /// so tracebacks, `debug.getinfo` and error positions see it.
    pub fn call(&mut self, nargs: usize, handler: Option<usize>) -> Result<usize, LuaError> {
        debug_assert!(self.len() > nargs, "a function below the arguments");
        let func = self.vm.rt.top - nargs - 1;
        let me = self.base - 1;
        let s = &mut self.vm.rt.stack;
        // With hooks on, the native already has its frame. The host's own
        // context ([`Vm::host_ctx`]) gets none: its caller is outside Lua.
        let own = !matches!(s.frames.last(), Some(f) if f.func as usize == me
            && matches!(f.kind, FrameKind::NativeCall { .. }))
            && self.vm.rt.host_native != Some(self.id);
        if own {
            let cdepth = s.same_depth();
            s.frames.push(crate::vm::Frame {
                kind: FrameKind::NativeCall { id: self.id },
                func: me as u32,
                base: self.base as u32,
                pc: 0,
                cell_base: s.cells.len() as u32,
                nvarargs: 0,
                want: 0,
                tail: false,
                extra: 0,
                cdepth,
            });
        }
        let below = s.frames.len();
        // As in `Vm::call`, the host's outermost call is the one
        // `c_overflow` already counts, so a host that calls through a
        // function of its own first (as `lua`'s driver does) keeps lua5.5's
        // depths.
        let cdepth = if below == 0 && self.vm.rt.host_native == Some(self.id) {
            s.same_depth()
        } else {
            s.deeper()
        };
        let r = if self.vm.c_overflow(cdepth) {
            Err(self.error("C stack overflow"))
        } else {
            let s = &mut self.vm.rt.stack;
            s.frames.push(crate::vm::Frame {
                kind: FrameKind::Boundary {
                    native: true,
                    handler: handler.is_some(),
                },
                func: func as u32,
                base: handler.map_or(func, |h| self.base + h) as u32,
                pc: 0,
                cell_base: s.cells.len() as u32,
                nvarargs: 0,
                want: crate::vm::op::MULTI,
                tail: false,
                extra: 0,
                cdepth,
            });
            self.vm.call_at(func, nargs)
        };
        let s = &mut self.vm.rt.stack;
        debug_assert_eq!(s.frames.len(), below, "the boundary is gone");
        if own {
            s.frames.truncate(below - 1);
        }
        match r {
            Ok(()) => Ok(self.vm.rt.top - func),
            Err(e) => {
                self.vm.rt.top = func;
                Err(e)
            }
        }
    }

    /// Return `N` values, as in `ctx.ret([v])` or, for failing library
    /// functions, `ctx.ret([Value::Nil, msg])`.
    #[inline]
    pub fn ret<const N: usize>(&mut self, vals: [Value; N]) -> Result<NativeReturn, LuaError> {
        for v in &vals {
            self.push(*v);
        }
        Ok(NativeReturn::Return(N))
    }

    /// The function value of a native registered under `name`, registering
    /// `f` under that name the first time. For natives that return other
    /// natives (`pairs` returns `next`).
    pub fn native(&mut self, name: &'static str, f: NativeFn) -> Value {
        match self.vm.rt.native_ids.get(name) {
            Some(&id) => Value::Native(id),
            None => self.vm.register_native(name, f),
        }
    }

    /// A string value (allocated, unrooted until stored in the window).
    pub fn new_str(&mut self, bytes: &[u8]) -> Value {
        Value::Str(self.vm.rt.strings.new_str(&mut self.vm.heap, bytes))
    }

    /// A string value from owned bytes.
    pub fn new_str_owned(&mut self, bytes: Vec<u8>) -> Value {
        Value::Str(self.vm.rt.strings.new_str_owned(&mut self.vm.heap, bytes))
    }

    /// A string value with bytes `a..b` of `s`, copied straight from it.
    pub fn new_substr(&mut self, s: Gc<LuaStr>, a: usize, b: usize) -> Value {
        Value::Str(self.vm.rt.strings.new_substr(&mut self.vm.heap, s, a, b))
    }

    /// An empty byte buffer to build a result in. It reuses the VM's scratch
    /// buffer, so hand it back with [`NativeCtx::new_str_buf`] or
    /// [`NativeCtx::give_buf`]; one that is dropped instead (on an error,
    /// or by a nested native that took it first) only costs an allocation.
    #[inline]
    pub fn take_buf(&mut self) -> Vec<u8> {
        let mut b = std::mem::take(&mut self.vm.rt.scratch);
        b.clear();
        b
    }

    /// Give a buffer from [`NativeCtx::take_buf`] back for reuse. Very
    /// large buffers are freed instead, so one huge result doesn't stay
    /// allocated.
    #[inline]
    pub fn give_buf(&mut self, b: Vec<u8>) {
        if b.capacity() <= SCRATCH_KEEP && b.capacity() >= self.vm.rt.scratch.capacity() {
            self.vm.rt.scratch = b;
        }
    }

    /// A string value from a buffer taken with [`NativeCtx::take_buf`],
    /// which goes back for reuse (or, if large, becomes the string's bytes).
    pub fn new_str_buf(&mut self, b: Vec<u8>) -> Value {
        if b.len() > SCRATCH_KEEP {
            return self.new_str_owned(b);
        }
        let v = self.new_str(&b);
        self.give_buf(b);
        v
    }

    /// Link a compiled chunk, as [`Vm::load_with_env`](crate::vm::Vm::load_with_env)
    /// does. The function is unrooted until stored in the window.
    pub fn link(&mut self, chunk: &orbit_bytecode::UnlinkedChunk, env: Value) -> Value {
        crate::vm::link::link(&mut self.vm.heap, &mut self.vm.rt, chunk, env, false)
    }

    /// A new table (unrooted until stored in the window).
    pub fn new_table(&mut self, t: Table) -> Gc<Table> {
        self.vm.heap.alloc(t)
    }

    /// Bytes of a string.
    pub fn bytes(&self, s: Gc<LuaStr>) -> &[u8] {
        self.vm.heap.get(s).as_bytes()
    }

    /// The isolate's locale.
    pub fn locale(&self) -> &Locale {
        &self.vm.rt.metas.locale
    }

    /// `tostring` without metamethods, appended to `out`.
    pub fn display(&self, v: Value, out: &mut Vec<u8>) {
        ops::display(&self.vm.heap, &self.vm.rt, v, out);
    }

    // ---- native closures -------------------------------------------------

    /// A function value that runs native `f` (registered as `name`) with
    /// `upvals` as its upvalues ([`NativeCtx::upvalue`]). It is a closure
    /// whose prototype is a one-instruction stub, so calling it is an
    /// ordinary Lua call.
    pub fn new_closure(
        &mut self,
        name: &'static str,
        f: crate::vm::NativeFn,
        upvals: &[Value],
    ) -> Value {
        self.new_closure_hidden(name, f, upvals, &[])
    }

    /// [`NativeCtx::new_closure`], plus `hidden` upvalues that scripts can't
    /// see or change through the debug library, for state a native must be
    /// able to trust ([`NativeCtx::hidden_upvalue`]). Every closure of `f`
    /// has the same number of hidden upvalues.
    pub fn new_closure_hidden(
        &mut self,
        name: &'static str,
        f: crate::vm::NativeFn,
        upvals: &[Value],
        hidden: &[Value],
    ) -> Value {
        let Value::Native(id) = self.native(name, f) else {
            unreachable!("natives are registered as natives")
        };
        self.new_native_closure(id, upvals, hidden)
    }

    /// [`NativeCtx::new_closure_hidden`] for the native already registered
    /// as `id`, for natives not looked up by name (an embedder's).
    pub fn new_native_closure(
        &mut self,
        id: NativeId,
        upvals: &[Value],
        hidden: &[Value],
    ) -> Value {
        let n = u8::try_from(hidden.len()).expect("few hidden upvalues");
        let native = &mut self.vm.rt.natives[id.0 as usize];
        debug_assert!(
            native.hidden == n || !self.vm.rt.native_protos.contains_key(&id),
            "every closure of a native has the same hidden upvalues"
        );
        native.hidden = n;
        let proto = match self.vm.rt.native_protos.get(&id) {
            Some(&p) => p,
            None => {
                let p = self.vm.heap.alloc(stub_proto(id));
                self.vm.rt.native_protos.insert(id, p);
                p
            }
        };
        // Hidden upvalues go last, so visible ones keep their indices.
        let captures: Box<[Gc<Cell>]> = upvals
            .iter()
            .chain(hidden)
            .map(|&value| self.vm.heap.alloc(Cell { value }))
            .collect();
        let code = self.vm.heap.get(proto).code.clone();
        Value::Closure(self.vm.heap.alloc(Closure {
            proto,
            code,
            captures,
        }))
    }

    /// Hidden upvalue `i` of the native closure being called (nil outside
    /// one); see [`NativeCtx::new_closure_hidden`].
    #[inline]
    pub fn hidden_upvalue(&self, i: usize) -> Value {
        let Some(c) = self.upvals else {
            return Value::Nil;
        };
        let caps = &self.vm.heap.get(c).captures;
        let hidden = self.vm.rt.natives[self.id.0 as usize].hidden as usize;
        match (i < hidden).then(|| caps[caps.len() - hidden + i]) {
            Some(cell) => self.vm.heap.get(cell).value,
            None => Value::Nil,
        }
    }

    /// The native behind function `f`: `f` itself, or the native a native
    /// closure runs. `None` for Lua functions and other values.
    pub fn native_of(&self, f: Value) -> Option<NativeId> {
        match f {
            Value::Native(id) => Some(id),
            Value::Closure(c) => {
                let p = self.vm.heap.get(self.vm.heap.get(c).proto);
                match p.code.code.first() {
                    Some(Op::NativeEntry { id }) => Some(NativeId(*id)),
                    _ => None,
                }
            }
            _ => None,
        }
    }

    /// Hidden upvalue `i` of native closure `f` (nil if it has none).
    pub fn hidden_upvalue_of(&self, f: Value, i: usize) -> Value {
        let Value::Closure(c) = f else {
            return Value::Nil;
        };
        let closure = self.vm.heap.get(c);
        let hidden = self.vm.rt.hidden_upvalues(&self.vm.heap, closure);
        let caps = &closure.captures;
        match (i < hidden).then(|| caps[caps.len() - hidden + i]) {
            Some(cell) => self.vm.heap.get(cell).value,
            None => Value::Nil,
        }
    }

    /// How many visible upvalues the native closure being called has.
    pub fn upvalue_count(&self) -> usize {
        match self.upvals {
            Some(c) => {
                self.vm.heap.get(c).captures.len()
                    - self.vm.rt.natives[self.id.0 as usize].hidden as usize
            }
            None => 0,
        }
    }

    /// Upvalue `i` of the native closure being called, or whose
    /// continuation is running (nil outside one).
    pub fn upvalue(&self, i: usize) -> Value {
        match self.upvals {
            Some(c) => match self.vm.heap.get(c).captures.get(i) {
                Some(&cell) => self.vm.heap.get(cell).value,
                None => Value::Nil,
            },
            None => Value::Nil,
        }
    }

    /// Set upvalue `i` of the native closure being called.
    pub fn set_upvalue(&mut self, i: usize, v: Value) {
        if let Some(c) = self.upvals {
            if let Some(&cell) = self.vm.heap.get(c).captures.get(i) {
                self.vm.heap.update(cell, |cell| cell.value = v);
            }
        }
    }

    // ---- errors ---------------------------------------------------------

    /// `chunk:line: ` for the function at `level` (1 is the caller of this
    /// native), or empty if it isn't a Lua function.
    pub fn where_(&self, level: usize) -> String {
        self.vm
            .rt
            .where_(&self.vm.heap, Some(self.running_native()), level)
    }

    /// This native, as the debug walk sees it (level 0).
    pub(crate) fn running_native(&self) -> crate::vm::debug::Running {
        crate::vm::debug::Running {
            id: self.id,
            func: self.base - 1,
        }
    }

    /// An error with a message, positioned at the caller (like
    /// `luaL_error`). The message is a byte string, so names a script gave
    /// keep bytes that aren't UTF-8.
    pub fn error<'m>(&mut self, msg: impl Into<OrbitStr<'m>>) -> LuaError {
        let m = self.positioned(msg.into().as_bytes());
        LuaError::new(m)
    }

    /// `msg` as a string with the caller's position in front, as
    /// [`Self::error`] raises it. The bytes are kept as they are.
    // Out of line: inlined into each `error` instantiation it adds about
    // 12 KB of code.
    #[inline(never)]
    pub fn positioned(&mut self, msg: &[u8]) -> Value {
        let mut text = self.where_(1).into_bytes();
        text.extend_from_slice(msg);
        self.new_str_owned(text)
    }

    /// Lua's "not enough memory" error, which has no position.
    #[cold]
    pub fn no_memory(&mut self) -> LuaError {
        LuaError::new(self.new_str(crate::object::NoMemory::MESSAGE.as_bytes()))
    }

    /// An error whose value is `v`, unchanged.
    pub fn error_value(&self, v: Value) -> LuaError {
        LuaError::new(v)
    }

    /// How the call site names this function, and whether it was a method
    /// call. Falls back to the name a loaded module gives it, then to the
    /// registered name.
    pub fn call_name(&self) -> (Vec<u8>, bool) {
        let me = match self.upvals {
            Some(c) => Value::Closure(c),
            None => Value::Native(self.id),
        };
        let caller = self.vm.rt.stack.frames.last().copied();
        self.name_at(caller, me).unwrap_or_else(|| {
            (
                self.vm.rt.natives[self.id.0 as usize]
                    .name
                    .as_bytes()
                    .to_vec(),
                false,
            )
        })
    }

    /// How `caller`'s call instruction names `me`, or else the name a loaded
    /// module gives it.
    fn name_at(&self, caller: Option<crate::vm::Frame>, me: Value) -> Option<(Vec<u8>, bool)> {
        if let Some(f) = caller {
            if let FrameKind::Lua { closure, .. } = f.kind {
                if f.pc > 0 && !crate::vm::debug::is_builtin(&self.vm.heap, closure) {
                    let proto = self.vm.heap.get(self.vm.heap.get(closure).proto);
                    let pc = f.pc - 1;
                    if matches!(
                        proto.code.code.get(pc as usize),
                        Some(Op::GenericCall { .. })
                    ) {
                        return Some((b"for iterator".to_vec(), false));
                    }
                    if matches!(
                        proto.code.code.get(pc as usize),
                        Some(Op::Call { .. } | Op::TailCall { .. })
                    ) {
                        if let Some(origin) = proto.debug.origin(pc, 0) {
                            let (name, method) = match origin {
                                Origin::Global(n)
                                | Origin::Field(n)
                                | Origin::Local(n)
                                | Origin::Upvalue(n) => (n, false),
                                Origin::Method(n) => (n, true),
                                Origin::Constant(n) => (n, false),
                            };
                            return Some((name.to_vec(), method));
                        }
                    }
                }
            }
        }
        // As the reference does, a function with no name at its call site is
        // named by where a loaded module keeps it.
        self.vm
            .rt
            .global_name(&self.vm.heap, me)
            .map(|name| (name.to_vec(), false))
    }

    // ---- intrinsics' errors ----------------------------------------------

    /// For an intrinsic: the frame of the built-in library function calling
    /// it, the newest Lua frame (an intrinsic has none of its own).
    fn builtin_frame(&self) -> Option<usize> {
        self.vm
            .rt
            .stack
            .frames
            .iter()
            .rposition(|f| matches!(f.kind, FrameKind::Lua { .. }))
    }

    /// The environment (`_ENV`) of the built-in calling this intrinsic: its
    /// first capture.
    pub(crate) fn builtin_env(&self) -> Value {
        let Some(i) = self.builtin_frame() else {
            return Value::Nil;
        };
        let FrameKind::Lua { closure, .. } = self.vm.rt.stack.frames[i].kind else {
            unreachable!("builtin_frame finds a Lua frame")
        };
        match self.vm.heap.get(closure).captures.first() {
            Some(&cell) => self.vm.heap.get(cell).value,
            None => Value::Nil,
        }
    }

    /// An error with a message, positioned at the caller of the built-in
    /// calling this intrinsic: `luaL_error` from that built-in.
    pub(crate) fn builtin_error<'m>(&mut self, msg: impl Into<OrbitStr<'m>>) -> LuaError {
        self.builtin_error_str(msg.into())
    }

    // One out-of-line body for every message type.
    #[inline(never)]
    fn builtin_error_str(&mut self, msg: OrbitStr<'_>) -> LuaError {
        let mut text = self.where_(2).into_bytes();
        text.extend_from_slice(&msg);
        LuaError::new(self.new_str_owned(text))
    }

    /// `bad argument #n to 'name' (msg)` for argument `n` (1-based) of the
    /// built-in calling this intrinsic, named as [`NativeCtx::call_name`]
    /// names a native.
    pub(crate) fn builtin_arg_error<'m>(
        &mut self,
        n: usize,
        msg: impl Into<OrbitStr<'m>>,
    ) -> LuaError {
        self.builtin_arg_error_str(n, msg.into())
    }

    // One out-of-line body for every message type.
    #[inline(never)]
    fn builtin_arg_error_str(&mut self, n: usize, msg: OrbitStr<'_>) -> LuaError {
        let frames = &self.vm.rt.stack.frames;
        let named = self.builtin_frame().and_then(|i| {
            let f = frames[i];
            let FrameKind::Lua { closure, .. } = f.kind else {
                return None;
            };
            // A built-in entered by a tail call has no call site.
            let caller = if f.tail {
                None
            } else {
                i.checked_sub(1).map(|c| frames[c])
            };
            self.name_at(caller, Value::Closure(closure))
        });
        let (name, method) = named.unwrap_or_else(|| (b"?".to_vec(), false));
        let text = bad_argument(&name, method, n, msg);
        self.builtin_error_str(text.as_orbit_str())
    }

    /// `bad argument` for a wrong type: `v` is the argument, `None` when it
    /// is absent.
    pub(crate) fn builtin_type_error(
        &mut self,
        n: usize,
        expected: &str,
        v: Option<Value>,
    ) -> LuaError {
        let got = match v {
            None => "no value".into(),
            Some(Value::LightUserData(_)) => "light userdata".into(),
            Some(v) => self.vm.rt.metas.type_name(&self.vm.heap, v),
        };
        self.builtin_arg_error(n, &format!("{expected} expected, got {got}"))
    }

    /// `bad argument #i to 'name' (msg)`, for 0-based argument `i`.
    pub fn arg_error<'m>(&mut self, i: usize, msg: impl Into<OrbitStr<'m>>) -> LuaError {
        self.arg_error_str(i, msg.into())
    }

    // One out-of-line body for every message type.
    #[inline(never)]
    fn arg_error_str(&mut self, i: usize, msg: OrbitStr<'_>) -> LuaError {
        let (name, method) = self.call_name();
        let text = bad_argument(&name, method, i + 1, msg);
        LuaError::new(self.positioned(&text))
    }

    /// `bad argument` for a wrong type.
    pub fn type_error(&mut self, i: usize, expected: &str) -> LuaError {
        let got = if i >= self.nargs {
            "no value".into()
        } else {
            match self.arg(i) {
                Value::LightUserData(_) => "light userdata".into(),
                v => self.vm.rt.metas.type_name(&self.vm.heap, v),
            }
        };
        self.arg_error(i, &format!("{expected} expected, got {got}"))
    }

    /// Any value (`value expected` if absent).
    pub fn check_any(&mut self, i: usize) -> Result<Value, LuaError> {
        if i >= self.nargs {
            return Err(self.arg_error(i, "value expected"));
        }
        Ok(self.arg(i))
    }

    /// A string argument; numbers are converted (in place, as Lua does).
    pub fn check_str(&mut self, i: usize) -> Result<Gc<LuaStr>, LuaError> {
        match self.arg(i) {
            Value::Str(s) => Ok(s),
            v @ (Value::Int(_) | Value::Float(_)) if i < self.nargs => {
                let mut text = Vec::new();
                self.vm.rt.metas.locale.fmt_number(&mut text, v);
                let s = self.vm.rt.strings.new_str(&mut self.vm.heap, &text);
                self.set(i, Value::Str(s));
                Ok(s)
            }
            _ => Err(self.type_error(i, "string")),
        }
    }

    /// A number argument (strings convertible to numbers are accepted).
    pub fn check_number(&mut self, i: usize) -> Result<Value, LuaError> {
        match self.arg(i) {
            v @ (Value::Int(_) | Value::Float(_)) => Ok(v),
            Value::Str(s) => match self
                .vm
                .rt
                .metas
                .locale
                .str_to_number(self.vm.heap.get(s).as_bytes())
            {
                Some(v) => Ok(v),
                None => Err(self.type_error(i, "number")),
            },
            _ => Err(self.type_error(i, "number")),
        }
    }

    /// The integer `v` converts to, as Lua's integer arguments convert: an
    /// integer, a float with an integral value, or a string that reads as
    /// either.
    pub fn to_integer(&self, v: Value) -> Option<i64> {
        let n = match v {
            Value::Str(s) => self
                .vm
                .rt
                .metas
                .locale
                .str_to_number(self.vm.heap.get(s).as_bytes())?,
            _ => v,
        };
        match n {
            Value::Int(i) => Some(i),
            Value::Float(f) => float_to_int(f),
            _ => None,
        }
    }

    /// A float argument.
    pub fn check_f64(&mut self, i: usize) -> Result<f64, LuaError> {
        Ok(self.check_number(i)?.as_f64().unwrap_or(0.0))
    }

    /// An integer argument (floats with an integral value are accepted).
    pub fn check_int(&mut self, i: usize) -> Result<i64, LuaError> {
        match self.check_number(i)? {
            Value::Int(n) => Ok(n),
            Value::Float(f) => match float_to_int(f) {
                Some(n) => Ok(n),
                None => Err(self.arg_error(i, "number has no integer representation")),
            },
            _ => unreachable!(),
        }
    }

    /// An optional integer argument.
    pub fn opt_int(&mut self, i: usize, default: i64) -> Result<i64, LuaError> {
        if self.arg(i).is_nil() {
            Ok(default)
        } else {
            self.check_int(i)
        }
    }

    /// A function argument.
    pub fn check_function(&mut self, i: usize) -> Result<Value, LuaError> {
        match self.arg(i) {
            f @ (Value::Closure(_) | Value::Native(_)) => Ok(f),
            _ => Err(self.type_error(i, "function")),
        }
    }

    /// A table argument.
    pub fn check_table(&mut self, i: usize) -> Result<Gc<Table>, LuaError> {
        match self.arg(i) {
            Value::Table(t) => Ok(t),
            _ => Err(self.type_error(i, "table")),
        }
    }

    // ---- metatables ------------------------------------------------------

    /// The metatable of `v`, if any.
    pub fn metatable(&self, v: Value) -> Option<Gc<Table>> {
        self.vm.rt.metatable(&self.vm.heap, v)
    }

    /// Field `e` of `v`'s metatable (raw), or nil.
    pub fn metamethod(&self, v: Value, e: MetaField) -> Value {
        self.vm.rt.metamethod(&self.vm.heap, v, e)
    }

    /// Turn a slow path's result into a [`Meta`]: a metamethod call gets
    /// its arguments pushed, an error gets positioned at the caller.
    fn meta_result<T>(&mut self, r: Result<T, Exc>) -> Result<Meta<T>, LuaError> {
        match r {
            Ok(v) => Ok(Meta::Done(v)),
            Err(Exc::Meta(call)) => {
                let n = call.nargs as usize;
                for &a in &call.args[..n] {
                    self.push(a);
                }
                Ok(Meta::Call {
                    f: call.f,
                    nargs: n,
                })
            }
            Err(Exc::Rt(msg, _)) => Err(self.error(&msg)),
            Err(Exc::Plain(msg)) => Err(LuaError::new(self.new_str(msg.as_bytes()))),
            Err(Exc::NoMemory) => Err(self.no_memory()),
            Err(Exc::Lua(e)) => Err(e),
            Err(Exc::Index { .. } | Exc::NewIndex { .. }) => {
                unreachable!("only the dispatch loop defers indexing")
            }
        }
    }

    /// `obj[key]` with metamethods.
    pub fn index(&mut self, obj: Value, key: Value) -> Result<Meta<Value>, LuaError> {
        let r = match obj {
            Value::Table(t) => {
                let tab = self.vm.heap.get(t);
                let v = tab.get(&self.vm.heap, key);
                if !v.is_nil() || tab.meta.is_none() {
                    Ok(v)
                } else {
                    meta::index(&self.vm.heap, &self.vm.rt.metas, obj, key)
                }
            }
            _ => meta::index(&self.vm.heap, &self.vm.rt.metas, obj, key),
        };
        self.meta_result(r)
    }

    /// `obj[key] = v` with metamethods.
    pub fn set_index(&mut self, obj: Value, key: Value, v: Value) -> Result<Meta<()>, LuaError> {
        let r = meta::new_index(&mut self.vm.heap, &self.vm.rt.metas, obj, key, v);
        self.meta_result(r)
    }

    /// `#v` with `__len`.
    pub fn length(&mut self, v: Value) -> Result<Meta<Value>, LuaError> {
        let r = crate::vm::cold::op_len(&self.vm.heap, &self.vm.rt.metas, v);
        self.meta_result(r)
    }

    /// `a < b` with `__lt`.
    pub fn less_than(&mut self, a: Value, b: Value) -> Result<Meta<bool>, LuaError> {
        let r = ops::less_than(&self.vm.heap, &self.vm.rt.metas, a, b);
        self.meta_result(r)
    }

    /// `a == b` with `__eq`.
    pub fn equal(&mut self, a: Value, b: Value) -> Result<Meta<bool>, LuaError> {
        let r = ops::equal(&self.vm.heap, &self.vm.rt.metas, a, b);
        self.meta_result(r)
    }

    /// `a <= b` with `__le`.
    pub fn less_equal(&mut self, a: Value, b: Value) -> Result<Meta<bool>, LuaError> {
        let r = ops::less_equal(&self.vm.heap, &self.vm.rt.metas, a, b);
        self.meta_result(r)
    }

    /// `a op b` (for the unary operators, `a` alone) with metamethods, as
    /// the C API's `lua_arith` does it.
    pub fn arith(&mut self, op: Operator, a: Value, b: Value) -> Result<Meta<Value>, LuaError> {
        use orbit_bytecode::number::BitOp;
        let (heap, metas) = (&self.vm.heap, &self.vm.rt.metas);
        let r = match op {
            Operator::Add => ops::arith_slow(heap, metas, ops::Arith::Add, a, b),
            Operator::Sub => ops::arith_slow(heap, metas, ops::Arith::Sub, a, b),
            Operator::Mul => ops::arith_slow(heap, metas, ops::Arith::Mul, a, b),
            Operator::Mod => ops::arith_slow(heap, metas, ops::Arith::Mod, a, b),
            Operator::Pow => ops::arith_slow(heap, metas, ops::Arith::Pow, a, b),
            Operator::Div => ops::arith_slow(heap, metas, ops::Arith::Div, a, b),
            Operator::IDiv => ops::arith_slow(heap, metas, ops::Arith::IDiv, a, b),
            Operator::Unm => ops::arith_slow(heap, metas, ops::Arith::Neg, a, a),
            Operator::BAnd => ops::bitwise_slow(heap, metas, BitOp::And, a, b),
            Operator::BOr => ops::bitwise_slow(heap, metas, BitOp::Or, a, b),
            Operator::BXor => ops::bitwise_slow(heap, metas, BitOp::Xor, a, b),
            Operator::Shl => ops::bitwise_slow(heap, metas, BitOp::Shl, a, b),
            Operator::Shr => ops::bitwise_slow(heap, metas, BitOp::Shr, a, b),
            Operator::BNot => ops::bnot_slow(heap, metas, a),
        };
        self.meta_result(r)
    }

    /// `a .. b` with `__concat`.
    pub fn concat(&mut self, a: Value, b: Value) -> Result<Meta<Value>, LuaError> {
        let mut out = Vec::new();
        let heap = &self.vm.heap;
        let locale = &self.vm.rt.metas.locale;
        if ops::concat_piece(heap, locale, a, &mut out)
            && ops::concat_piece(heap, locale, b, &mut out)
        {
            return Ok(Meta::Done(self.new_str_owned(out)));
        }
        let h = meta::binary_handler(heap, &self.vm.rt.metas, MetaField::Concat, a, b);
        if h.is_nil() {
            let bad = if ops::concat_piece(heap, locale, a, &mut Vec::new()) {
                b
            } else {
                a
            };
            let msg = format!(
                "attempt to concatenate a {} value",
                self.vm.rt.metas.type_name(heap, bad)
            );
            return Err(self.error(&msg));
        }
        self.meta_result(Err(Exc::Meta(meta::MetaCall::new(
            h,
            MetaField::Concat,
            &[a, b],
        ))))
    }

    // ---- userdata --------------------------------------------------------

    /// The host value of userdata `u`, if it is a `T`.
    pub fn userdata_ref<T: std::any::Any>(&self, u: Gc<crate::object::UserData>) -> Option<&T> {
        self.vm.heap.get(u).downcast_ref()
    }

    /// The host value of userdata `u`, mutably, if it is a `T`.
    pub fn userdata_mut<T: std::any::Any>(
        &mut self,
        u: Gc<crate::object::UserData>,
    ) -> Option<&mut T> {
        self.vm.heap.get_mut(u).downcast_mut()
    }

    /// A userdata argument holding a `T`; `name` is the type the message
    /// names (`FILE*`).
    pub fn check_userdata<T: std::any::Any>(
        &mut self,
        i: usize,
        name: &str,
    ) -> Result<Gc<crate::object::UserData>, LuaError> {
        match self.arg(i) {
            Value::UserData(u) if self.vm.heap.get(u).is::<T>() => Ok(u),
            _ => Err(self.type_error(i, name)),
        }
    }

    /// A new userdata (unrooted until stored in the window).
    pub fn new_userdata<T: std::any::Any>(&mut self, data: T) -> Gc<crate::object::UserData> {
        self.vm.heap.alloc(crate::object::UserData::new(data))
    }

    /// Room for `bytes` more under the heap's limit
    /// ([`Vm::set_memory_limit`](crate::vm::Vm::set_memory_limit)), collecting
    /// in full first if needed; `not enough memory` if there still isn't.
    /// Call it before a large allocation, with every live value in the
    /// window.
    pub fn reserve(&mut self, bytes: usize) -> Result<(), LuaError> {
        if self.vm.heap.would_exceed(bytes) {
            if !self.vm.rt.gc.finalizing() {
                self.full_collect();
            }
            if self.vm.heap.would_exceed(bytes) {
                return Err(self.no_memory());
            }
        }
        Ok(())
    }

    /// One generational collection (`collectgarbage("step")`): true if it
    /// was a major one.
    pub fn gen_step(&mut self) -> bool {
        let top = self.vm.rt.top;
        self.vm.rt.stack.regs.truncate(top);
        self.vm.heap.step_with(&mut self.vm.rt, 0)
    }

    /// Run a full collection (`collectgarbage("collect")`).
    pub fn full_collect(&mut self) {
        // Values above the window are dead.
        let top = self.vm.rt.top;
        self.vm.rt.stack.regs.truncate(top);
        self.vm.heap.full_collect(&mut self.vm.rt);
    }
}

/// The prototype of native `id`'s closures: `NativeEntry` and nothing else,
/// taking any number of arguments.
fn stub_proto(id: NativeId) -> Proto {
    let code = [Op::NativeEntry { id: id.0 }];
    Proto {
        code: Rc::new(ProtoCode {
            code: code.as_slice().into(),
            consts: Box::new([]),
            caches: Box::new([]),
            nparams: 0,
            is_vararg: true,
            nregs: 0,
            ncells: 0,
            builtin: false,
            refuses_yield: false,
            #[cfg(feature = "debugger")]
            debug_mark: Default::default(),
        }),
        children: Box::new([]),
        captures: Box::new([]),
        name: FunctionName::Anonymous,
        line_defined: 0,
        last_line: 0,
        chunk: "[C]".into(),
        source: Rc::from(b"=[C]".as_slice()),
        debug: Rc::default(),
        self_capture: None,
    }
}

impl Vm {
    /// `NativeEntry`: the running frame is native closure `c`'s stub. Pop it
    /// and run the native in its place, with the frame's arguments.
    #[inline(never)]
    pub(crate) fn native_entry(&mut self, c: Gc<Closure>, id: NativeId) -> Result<Flow, Exc> {
        // Its fields one by one, as in `deliver`.
        let f = self.rt.stack.frames.last().expect("the stub's frame");
        let (func, nargs, want) = (f.func as usize, f.nvarargs as usize, f.want);
        let depth = self.rt.stack.frames.len() - 1;
        self.rt.stack.frames.truncate(depth);
        self.rt.top = func + 1 + nargs;
        let nf = self.rt.natives[id.0 as usize].f;
        let mut ctx = NativeCtx {
            vm: self,
            base: func + 1,
            nargs,
            id,
            upvals: Some(c),
        };
        let r = nf(&mut ctx);
        // A wrapped coroutine's function resumes it: switch at once, from
        // the fields alone (see `deliver`).
        if let Ok(NativeReturn::Resume { co, nargs, k, ok }) = r {
            return self.resume(co, nargs, k, ok, None, func, want, id);
        }
        let r = r?;
        if let NativeReturn::Return(n) = r {
            if self.rt.stack.hook.wants(crate::vm::hook::RETURN) {
                self.native_return_hook(func, id, n).map_err(Exc::Lua)?;
            }
        }
        self.native_returned(r, None, func, want, id)
    }
}

/// `bad argument #n to 'name' (msg)` for argument `n` (1-based, counting
/// `self`), or `calling 'name' on bad self (msg)` when a method call's
/// `self` is the bad one.
fn bad_argument(name: &[u8], method: bool, n: usize, msg: OrbitStr<'_>) -> OrbitString {
    let mut text = OrbitString::new();
    if method && n == 1 {
        text.push("calling '");
        text.push(name);
        text.push("' on bad self (");
    } else {
        let n = if method { n - 1 } else { n };
        text.push(format!("bad argument #{n} to '"));
        text.push(name);
        text.push("' (");
    }
    text.push(msg);
    text.push(")");
    text
}
