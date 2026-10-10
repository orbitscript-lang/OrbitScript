//! The native-function interface (ADR-3's continuation style).

use orbit_bytecode::{Op, Origin};
use orbit_core::number::{self, float_to_int};
use orbit_core::{Gc, LuaHeap, LuaStr, NativeId, Table, Value};

use crate::meta::{self, Event};
use crate::ops::Exc;
use crate::{FrameKind, LuaError, Runtime, ops};

/// The outcome of an operation that may need a metamethod: either the
/// answer, or a function to call. For [`Meta::Call`] the arguments are
/// already pushed on the window; return
/// `NativeReturn::CallThen { f, nargs, k, .. }` and take the result in `k`.
#[derive(Debug, Clone, Copy)]
pub enum Meta<T> {
    Done(T),
    Call { f: Value, nargs: usize },
}

/// A native function.
pub type NativeFn = fn(&mut NativeCtx<'_>) -> Result<NativeReturn, LuaError>;

/// An optional fast path for a native that returns one value and needs
/// nothing but its arguments (most of `math`). It gets the arguments and
/// returns the result, or `None` to run the full native instead (wrong
/// types, errors, anything unusual), so it never changes behavior. It cannot
/// allocate, call back into Lua, or fail.
pub type FastFn = fn(&[Value]) -> Option<Value>;

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
    Resume {
        co: Gc<orbit_core::Thread>,
        nargs: usize,
        k: Continuation,
    },
    /// Close coroutine `co` (suspended, or dead): run its pending `__close`
    /// handlers on its stack, newest first, with its error (nil if it is
    /// suspended), then run `k`: `Ok` with no values, or `Err` with the error
    /// it died with or the last error a handler raised.
    Close {
        co: Gc<orbit_core::Thread>,
        k: Continuation,
    },
    /// Yield the last `nvals` values of the window to the resumer. The
    /// values the next resume passes become this native's results.
    Yield { nvals: usize },
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
    pub heap: &'a mut LuaHeap,
    pub rt: &'a mut Runtime,
    pub(crate) base: usize,
    pub(crate) nargs: usize,
    pub(crate) id: NativeId,
    /// The native closure being called, for its upvalues.
    pub(crate) upvals: Option<Gc<orbit_core::Closure>>,
}

impl NativeCtx<'_> {
    /// Number of arguments the native was called with.
    #[inline]
    pub fn nargs(&self) -> usize {
        self.nargs
    }

    /// Values currently in the window.
    #[inline]
    pub fn len(&self) -> usize {
        self.rt.top - self.base
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Window slot `i` (argument `i` until the native changes it), or nil.
    #[inline]
    pub fn arg(&self, i: usize) -> Value {
        let at = self.base + i;
        if at < self.rt.top {
            self.rt.stack.regs[at]
        } else {
            Value::Nil
        }
    }

    /// Overwrite window slot `i`, growing the window with nils if needed.
    pub fn set(&mut self, i: usize, v: Value) {
        let at = self.base + i;
        let top = self.rt.top;
        self.rt.stack.dirty_to = self.rt.stack.dirty_to.max(at + 1);
        let regs = &mut self.rt.stack.regs;
        if at >= top {
            crate::interp::extend_nil(regs, at + 1);
            regs[top..at].fill(Value::Nil);
            self.rt.top = at + 1;
        }
        regs[at] = v;
    }

    /// Insert `v` at window slot `i`, shifting the rest up.
    pub fn insert(&mut self, i: usize, v: Value) {
        let at = self.base + i;
        self.rt.top = self.rt.stack.insert(at, self.rt.top, v);
    }

    /// Push a value onto the end of the window.
    #[inline]
    pub fn push(&mut self, v: Value) {
        self.rt.stack.put(self.rt.top, v);
        self.rt.top += 1;
    }

    /// Shrink the window to `len` values.
    pub fn truncate(&mut self, len: usize) {
        self.rt.top = self.rt.top.min(self.base + len);
    }

    /// The window's values.
    pub fn window(&self) -> &[Value] {
        &self.rt.stack.regs[self.base..self.rt.top]
    }

    /// Return one value.
    pub fn ret1(&mut self, v: Value) -> Result<NativeReturn, LuaError> {
        self.push(v);
        Ok(NativeReturn::Return(1))
    }

    /// The function value of a native registered under `name`, registering
    /// `f` under that name the first time. For natives that return other
    /// natives (`pairs` returns `next`).
    pub fn native(&mut self, name: &'static str, f: NativeFn) -> Value {
        if let Some(&id) = self.rt.native_ids.get(name) {
            return Value::Native(id);
        }
        let id = NativeId(self.rt.natives.len() as u32);
        self.rt.natives.push(crate::Native {
            name,
            f,
            fast: None,
        });
        self.rt.native_ids.insert(name, id);
        Value::Native(id)
    }

    /// A string value (allocated, unrooted until stored in the window).
    pub fn new_str(&mut self, bytes: &[u8]) -> Value {
        Value::Str(self.rt.strings.new_str(self.heap, bytes))
    }

    /// A string value from owned bytes.
    pub fn new_str_owned(&mut self, bytes: Vec<u8>) -> Value {
        Value::Str(self.rt.strings.new_str_owned(self.heap, bytes))
    }

    /// A string value with bytes `a..b` of `s`, copied straight from it.
    pub fn new_substr(&mut self, s: Gc<LuaStr>, a: usize, b: usize) -> Value {
        Value::Str(self.rt.strings.new_substr(self.heap, s, a, b))
    }

    /// An empty byte buffer to build a result in. It reuses the VM's scratch
    /// buffer, so hand it back with [`NativeCtx::new_str_buf`] or
    /// [`NativeCtx::give_buf`]; one that is dropped instead (on an error,
    /// or by a nested native that took it first) only costs an allocation.
    #[inline]
    pub fn take_buf(&mut self) -> Vec<u8> {
        let mut b = std::mem::take(&mut self.rt.scratch);
        b.clear();
        b
    }

    /// Give a buffer from [`NativeCtx::take_buf`] back for reuse. Very
    /// large buffers are freed instead, so one huge result doesn't stay
    /// allocated.
    #[inline]
    pub fn give_buf(&mut self, b: Vec<u8>) {
        if b.capacity() <= SCRATCH_KEEP && b.capacity() >= self.rt.scratch.capacity() {
            self.rt.scratch = b;
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

    /// Link a compiled chunk, as [`Vm::load_with_env`](crate::Vm::load_with_env)
    /// does. The function is unrooted until stored in the window.
    pub fn link(&mut self, chunk: &orbit_bytecode::UnlinkedChunk, env: Value) -> Value {
        crate::link::link(self.heap, self.rt, chunk, env)
    }

    /// A new table (unrooted until stored in the window).
    pub fn new_table(&mut self, t: Table) -> Gc<Table> {
        self.heap.alloc(t)
    }

    /// Bytes of a string.
    pub fn bytes(&self, s: Gc<LuaStr>) -> &[u8] {
        self.heap.get(s).as_bytes()
    }

    /// `tostring` without metamethods, appended to `out`.
    pub fn display(&self, v: Value, out: &mut Vec<u8>) {
        ops::display(self.heap, self.rt, v, out);
    }

    // ---- errors ---------------------------------------------------------

    /// `chunk:line: ` for the function at `level` (1 is the caller of this
    /// native), or empty if it isn't a Lua function.
    pub fn where_(&self, level: usize) -> String {
        self.rt
            .where_(self.heap, Some(self.running_native()), level)
    }

    /// This native, as the debug walk sees it (level 0).
    pub(crate) fn running_native(&self) -> crate::debug::Running {
        crate::debug::Running {
            id: self.id,
            func: self.base - 1,
        }
    }

    /// An error with a message, positioned at the caller (like
    /// `luaL_error`).
    pub fn error(&mut self, msg: impl AsRef<str>) -> LuaError {
        let text = format!("{}{}", self.where_(1), msg.as_ref());
        LuaError::new(self.new_str(text.as_bytes()))
    }

    /// Lua's "not enough memory" error, which has no position.
    #[cold]
    pub fn no_memory(&mut self) -> LuaError {
        LuaError::new(self.new_str(orbit_core::NoMemory::MESSAGE.as_bytes()))
    }

    /// An error whose value is `v`, unchanged.
    pub fn error_value(&self, v: Value) -> LuaError {
        LuaError::new(v)
    }

    /// How the call site names this function, and whether it was a method
    /// call. Falls back to the registered name.
    pub fn call_name(&self) -> (String, bool) {
        if let Some(f) = self.rt.stack.frames.last()
            && let FrameKind::Lua { closure, .. } = f.kind
            && f.pc > 0
        {
            let proto = self.heap.get(self.heap.get(closure).proto);
            let pc = f.pc - 1;
            if matches!(
                proto.code.code.get(pc as usize),
                Some(Op::GenericCall { .. })
            ) {
                return ("for iterator".to_owned(), false);
            }
            if matches!(
                proto.code.code.get(pc as usize),
                Some(Op::Call { .. } | Op::TailCall { .. })
            ) && let Some(origin) = proto.debug.origin(pc, 0)
            {
                let (name, method) = match origin {
                    Origin::Global(n)
                    | Origin::Field(n)
                    | Origin::Local(n)
                    | Origin::Upvalue(n) => (n, false),
                    Origin::Method(n) => (n, true),
                    Origin::Constant(n) => (n, false),
                };
                return (String::from_utf8_lossy(name).into_owned(), method);
            }
        }
        (self.rt.natives[self.id.0 as usize].name.to_owned(), false)
    }

    /// `bad argument #i to 'name' (msg)`, for 0-based argument `i`.
    pub fn arg_error(&mut self, i: usize, msg: impl AsRef<str>) -> LuaError {
        let (name, method) = self.call_name();
        let text = if method && i == 0 {
            format!("calling '{name}' on bad self ({})", msg.as_ref())
        } else {
            let n = if method { i } else { i + 1 };
            format!("bad argument #{n} to '{name}' ({})", msg.as_ref())
        };
        self.error(text)
    }

    /// `bad argument` for a wrong type.
    pub fn type_error(&mut self, i: usize, expected: &str) -> LuaError {
        let got = if i >= self.nargs {
            "no value".into()
        } else {
            match self.arg(i) {
                Value::LightUserData(_) => "light userdata".into(),
                v => self.rt.metas.type_name(self.heap, v),
            }
        };
        self.arg_error(i, format!("{expected} expected, got {got}"))
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
                let mut text = String::new();
                number::fmt_number(&mut text, v);
                let s = self.rt.strings.new_str(self.heap, text.as_bytes());
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
            Value::Str(s) => match number::str_to_number(self.heap.get(s).as_bytes()) {
                Some(v) => Ok(v),
                None => Err(self.type_error(i, "number")),
            },
            _ => Err(self.type_error(i, "number")),
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
        self.rt.metatable(self.heap, v)
    }

    /// Field `e` of `v`'s metatable (raw), or nil.
    pub fn metamethod(&self, v: Value, e: Event) -> Value {
        self.rt.metamethod(self.heap, v, e)
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
            Err(Exc::Rt(msg, _)) => Err(self.error(msg)),
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
                let tab = self.heap.get(t);
                let v = tab.get(self.heap, key);
                if !v.is_nil() || tab.meta.is_none() {
                    Ok(v)
                } else {
                    meta::index(self.heap, &self.rt.metas, obj, key)
                }
            }
            _ => meta::index(self.heap, &self.rt.metas, obj, key),
        };
        self.meta_result(r)
    }

    /// `obj[key] = v` with metamethods.
    pub fn set_index(&mut self, obj: Value, key: Value, v: Value) -> Result<Meta<()>, LuaError> {
        let r = meta::new_index(self.heap, &self.rt.metas, obj, key, v);
        self.meta_result(r)
    }

    /// `#v` with `__len`.
    pub fn length(&mut self, v: Value) -> Result<Meta<Value>, LuaError> {
        let r = crate::cold::op_len(self.heap, &self.rt.metas, v);
        self.meta_result(r)
    }

    /// `a < b` with `__lt`.
    pub fn less_than(&mut self, a: Value, b: Value) -> Result<Meta<bool>, LuaError> {
        let r = ops::less_than(self.heap, &self.rt.metas, a, b);
        self.meta_result(r)
    }

    /// `a == b` with `__eq`.
    pub fn equal(&mut self, a: Value, b: Value) -> Result<Meta<bool>, LuaError> {
        let r = ops::equal(self.heap, &self.rt.metas, a, b);
        self.meta_result(r)
    }

    // ---- userdata --------------------------------------------------------

    /// The host value of userdata `u`, if it is a `T`.
    pub fn userdata_ref<T: std::any::Any>(&self, u: Gc<orbit_core::UserData>) -> Option<&T> {
        self.heap.get(u).downcast_ref()
    }

    /// The host value of userdata `u`, mutably, if it is a `T`.
    pub fn userdata_mut<T: std::any::Any>(
        &mut self,
        u: Gc<orbit_core::UserData>,
    ) -> Option<&mut T> {
        self.heap.get_mut(u).downcast_mut()
    }

    /// A userdata argument holding a `T`; `name` is the type the message
    /// names (`FILE*`).
    pub fn check_userdata<T: std::any::Any>(
        &mut self,
        i: usize,
        name: &str,
    ) -> Result<Gc<orbit_core::UserData>, LuaError> {
        match self.arg(i) {
            Value::UserData(u) if self.heap.get(u).is::<T>() => Ok(u),
            _ => Err(self.type_error(i, name)),
        }
    }

    /// A new userdata (unrooted until stored in the window).
    pub fn new_userdata<T: std::any::Any>(&mut self, data: T) -> Gc<orbit_core::UserData> {
        self.heap.alloc(orbit_core::UserData::new(data))
    }

    /// Room for `bytes` more under the heap's limit
    /// ([`Vm::set_memory_limit`](crate::Vm::set_memory_limit)), collecting
    /// in full first if needed; `not enough memory` if there still isn't.
    /// Call it before a large allocation, with every live value in the
    /// window.
    pub fn reserve(&mut self, bytes: usize) -> Result<(), LuaError> {
        if self.heap.would_exceed(bytes) {
            if !self.rt.gc.finalizing() {
                self.full_collect();
            }
            if self.heap.would_exceed(bytes) {
                return Err(self.no_memory());
            }
        }
        Ok(())
    }

    /// Run a full collection (`collectgarbage("collect")`).
    pub fn full_collect(&mut self) {
        // Values above the window are dead.
        let top = self.rt.top;
        self.rt.stack.regs.truncate(top);
        self.heap.full_collect(self.rt);
    }
}
