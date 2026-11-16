//! The window: the stack API's operations over the runtime.

use std::any::{Any, TypeId};
use std::ffi::c_void;

use crate::locale::{Category, LocaleMode, LocaleScope, TimeParts};
use crate::object::HashSeed;
use crate::object::number::float_to_int;
use crate::object::table as tbl;
use crate::object::value::raw_eq;
use crate::object::{self as raw, Gc, LightUserData, LuaStr, NativeId, NoMemory, Table, gc};
use crate::object::{OrbitStr, OrbitString};
use crate::stdlib;
use crate::vm::{
    self, CallOutcome, Cause, CoStatus, LuaError, Meta, MetaField, NativeCtx, NativeReturn,
    Operator, Vm,
};
use orbit_lua_version::LUA;

use super::{
    ArithOp, CompareOp, Continuation, FrameInfo, GcMode, GcOp, GcParam, HostValue,
    InterruptHandler, Library, MULTRET, Native, Outcome, REGISTRY_INDEX, RIDX_MAINTHREAD, Resumed,
    Ret, Return, StackResult, Status, ThreadStatus, Type,
};

type RawValue = raw::Value;

/// The most values a native's window may hold, as `LUAI_MAXSTACK`.
const MAX_STACK: usize = 1_000_000;

/// The type of `v`.
fn type_of(v: RawValue) -> Type {
    match v {
        RawValue::Nil => Type::Nil,
        RawValue::Bool(_) => Type::Boolean,
        RawValue::Int(_) | RawValue::Float(_) => Type::Number,
        RawValue::Str(_) => Type::String,
        RawValue::Table(_) => Type::Table,
        RawValue::Closure(_) | RawValue::Native(_) => Type::Function,
        RawValue::UserData(_) => Type::Userdata,
        RawValue::Thread(_) => Type::Thread,
        RawValue::LightUserData(_) => Type::LightUserdata,
    }
}

/// The runtime's operator for `op`.
fn operator(op: ArithOp) -> Operator {
    match op {
        ArithOp::Add => Operator::Add,
        ArithOp::Sub => Operator::Sub,
        ArithOp::Mul => Operator::Mul,
        ArithOp::Mod => Operator::Mod,
        ArithOp::Pow => Operator::Pow,
        ArithOp::Div => Operator::Div,
        ArithOp::IDiv => Operator::IDiv,
        ArithOp::BAnd => Operator::BAnd,
        ArithOp::BOr => Operator::BOr,
        ArithOp::BXor => Operator::BXor,
        ArithOp::Shl => Operator::Shl,
        ArithOp::Shr => Operator::Shr,
        ArithOp::Unm => Operator::Unm,
        ArithOp::BNot => Operator::BNot,
    }
}

/// An error raised in the isolate, to return from a native (an `Err`
/// unwinds to the nearest protected call). Holds the error value.
#[derive(Debug)]
pub struct Raised(pub(crate) Box<LuaError>);

impl From<LuaError> for Raised {
    // Errors are rare: keep the boxing out of the callers' fast paths.
    #[cold]
    #[inline(never)]
    fn from(e: LuaError) -> Raised {
        Raised(Box::new(e))
    }
}

/// A function made from a Rust closure ([`Stack::push_function`]).
type FnBox = std::rc::Rc<dyn for<'a, 'b> Fn(&'a mut Stack<'b>) -> StackResult<Return>>;

/// What a function's hidden upvalue holds: its closure.
struct FunctionBox(FnBox);

/// The name functions made from closures are registered under. Messages
/// name them by their call site, or by where a loaded module keeps them.
const CLOSURE_NAME: &str = "?";

/// The runtime native behind every [`Stack::push_function`] function.
fn function_entry(ctx: &mut NativeCtx<'_>) -> Result<NativeReturn, LuaError> {
    let RawValue::UserData(u) = ctx.hidden_upvalue(0) else {
        unreachable!("a function's hidden upvalue is its closure")
    };
    let f = ctx
        .userdata_ref::<FunctionBox>(u)
        .expect("a function's hidden upvalue holds its closure")
        .0
        .clone();
    let mut s = Stack {
        ctx: ctx.reborrow(),
    };
    match f(&mut s) {
        Ok(r) => s.finish(r),
        Err(Raised(e)) => Err(*e),
    }
}

/// The runtime native behind every [`Native`] `N`.
fn native_entry<N: Native>(ctx: &mut NativeCtx<'_>) -> Result<NativeReturn, LuaError> {
    let mut s = Stack {
        ctx: ctx.reborrow(),
    };
    match N::call(&mut s) {
        Ok(r) => s.finish(r),
        Err(Raised(e)) => Err(*e),
    }
}

/// A row of a built-in library's table: the function's qualified name,
/// its entry, and the fast path calls wanting one result try first.
#[derive(Clone, Copy)]
pub(crate) struct LibraryRow {
    pub(crate) name: &'static str,
    pub(crate) entry: vm::NativeFn,
    pub(crate) fast: Option<vm::FastFn>,
}

impl LibraryRow {
    /// A native written on the raw interface.
    pub(crate) const fn raw(name: &'static str, entry: vm::NativeFn) -> LibraryRow {
        LibraryRow {
            name,
            entry,
            fast: None,
        }
    }

    /// A hand-written fast path, for a raw native.
    pub(crate) const fn with_fast(self, fast: vm::FastFn) -> LibraryRow {
        LibraryRow {
            fast: Some(fast),
            ..self
        }
    }
}

/// A library table's row for native `N`: its name, its entry, and the
/// fast path `#[native(pure)]` gave it.
pub(crate) const fn library_row<N: Native>() -> LibraryRow {
    LibraryRow {
        name: N::NAME,
        entry: native_entry::<N>,
        fast: pure_fast::<N>(),
    }
}

/// The fast path `#[native(pure)]` gave native `N`, if any.
const fn pure_fast<N: Native>() -> Option<vm::FastFn> {
    match N::PURE {
        Some(_) => Some(pure_entry::<N>),
        None => None,
    }
}

/// The interpreter's fast path for a `pure` native `N`.
fn pure_entry<N: Native>(cx: vm::FastCtx<'_>, args: &[RawValue]) -> Option<RawValue> {
    N::PURE.and_then(|f| f(cx.view(), args))
}

/// How a [`Return`] names the continuation to run: the runtime's.
pub(crate) type ContRef = vm::Continuation;

/// The continuation behind `K`.
pub(crate) fn cont_ref<K: Continuation>() -> ContRef {
    continue_in::<K>
}

/// The runtime continuation behind every [`Continuation`] `K`.
fn continue_in<K: Continuation>(
    ctx: &mut NativeCtx<'_>,
    o: CallOutcome,
) -> Result<NativeReturn, LuaError> {
    let mut s = Stack {
        ctx: ctx.reborrow(),
    };
    let o = match o {
        CallOutcome::Ok(n) => Outcome::Ok(n),
        CallOutcome::Err(v) => Outcome::Err(Raised::from(s.ctx.error_value(v))),
    };
    match K::resume(&mut s, o) {
        Ok(r) => s.finish(r),
        Err(Raised(e)) => Err(*e),
    }
}

/// A thread (coroutine), for keeping between calls. It does not keep the
/// thread alive: check [`Stack::thread_is_live`] before using one that may
/// have been collected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ThreadRef(Gc<raw::Thread>);

impl ThreadRef {
    /// The heap slot the thread occupies. A collected thread's slot goes to
    /// the next thread made, so this tells live threads apart only.
    pub fn slot(self) -> u32 {
        self.0.index()
    }
}

/// Another thread's stack in place of the window, between
/// [`Stack::enter_thread`] and [`Stack::leave_thread`].
#[derive(Debug)]
#[must_use]
pub struct ThreadWindow(pub(crate) vm::WindowSave);

/// A native with data words, from [`Control::register_native`]: a plain
/// value with no collector state, to keep as long as the isolate.
#[derive(Debug, Clone, Copy)]
pub struct NativeRef(pub(crate) RawValue);

/// Memory a binding asked for with [`Stack::new_userdata`], aligned to 16
/// bytes. The cells never move while the userdata lives, so the binding can
/// keep the pointer.
#[derive(Debug)]
struct Block {
    cells: Box<[std::cell::Cell<u128>]>,
    size: usize,
    /// How many user values it has. One lives in the userdata's own slot;
    /// two or more live in a table there.
    nuv: u16,
}

impl Block {
    fn new(size: usize, nuv: u16) -> Block {
        let n = size.div_ceil(16).max(1);
        Block {
            cells: (0..n).map(|_| std::cell::Cell::new(0)).collect(),
            size,
            nuv,
        }
    }

    fn as_ptr(&self) -> *mut c_void {
        self.cells.as_ptr() as *mut c_void
    }
}

/// Where an index points.
#[derive(Clone, Copy)]
enum Slot {
    Stack(usize),
    Registry,
    /// One of the running native closure's upvalues (0-based).
    Upvalue(usize),
    None,
}

/// Where user value `n` of a userdata is kept.
enum UserValue {
    /// The userdata's own slot (one user value, or a host userdata's).
    Slot(Gc<raw::UserData>),
    /// Entry `n` of the table in that slot.
    InTable(Gc<Table>, i64),
}

/// A warning whose pieces are still arriving ([`Stack::warn`]).
#[derive(Debug, Default)]
struct WarnBuffer {
    text: Vec<u8>,
    /// A piece came with `more` set, so the next one continues it.
    continued: bool,
}

/// The window of a native function, or the host's window between calls.
pub struct Stack<'s> {
    pub(crate) ctx: NativeCtx<'s>,
}

impl std::fmt::Debug for Stack<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Stack")
            .field("top", &self.ctx.len())
            .finish_non_exhaustive()
    }
}

impl<'s> Stack<'s> {
    #[doc(hidden)]
    pub fn new(ctx: NativeCtx<'s>) -> Stack<'s> {
        Stack { ctx }
    }

    /// The same window, borrowed for a shorter time: for code that keeps a
    /// stack in a value of its own while the native runs.
    pub fn reborrow(&mut self) -> Stack<'_> {
        Stack {
            ctx: self.ctx.reborrow(),
        }
    }

    /// Turn what a native ended with into the runtime's terms.
    #[inline(always)]
    pub(crate) fn finish(&mut self, r: Return) -> Result<NativeReturn, LuaError> {
        match r.0 {
            Ret::Values(n) if n <= self.ctx.len() => Ok(NativeReturn::Return(n)),
            _ => self.finish_other(r),
        }
    }

    #[inline(never)]
    fn finish_other(&mut self, r: Return) -> Result<NativeReturn, LuaError> {
        let c = &mut self.ctx;
        Ok(match r.0 {
            Ret::Values(n) => {
                if n > c.len() {
                    return Err(c.error("native returned more results than its stack holds"));
                }
                NativeReturn::Return(n)
            }
            Ret::Yield(n) => NativeReturn::Yield { nvals: n },
            Ret::YieldThen(n, k) => NativeReturn::YieldThen { nvals: n, k },
            Ret::CallThen {
                nargs,
                protected,
                k,
            } => {
                if nargs + 1 > c.len() {
                    return Err(c.error("not enough elements for the call"));
                }
                // The function comes out from below its arguments.
                let at = c.len() - nargs - 1;
                let f = c.arg(at);
                for i in at..c.len() - 1 {
                    let v = c.arg(i + 1);
                    c.set(i, v);
                }
                let len = c.len();
                c.truncate(len - 1);
                NativeReturn::CallThenYieldable {
                    f,
                    nargs,
                    k,
                    protected,
                    ok: crate::vm::ResumeOk::Continue,
                    in_place: false,
                }
            }
            Ret::Exit { code, close } => {
                return super::exit::exit_process(c, code, close);
            }
        })
    }

    // ---- indices -----------------------------------------------------------

    #[inline]
    fn slot(&self, idx: i32) -> Slot {
        let c = &self.ctx;
        // A positive index (every argument) first: it is the usual case.
        if idx > 0 {
            let at = idx as usize - 1;
            return if at < c.len() {
                Slot::Stack(at)
            } else {
                Slot::None
            };
        }
        let len = c.len() as i64;
        if idx > REGISTRY_INDEX {
            // A negative stack index (0 is invalid).
            let at = len + idx as i64;
            if idx < 0 && at >= 0 {
                Slot::Stack(at as usize)
            } else {
                Slot::None
            }
        } else if idx == REGISTRY_INDEX {
            Slot::Registry
        } else {
            // upvalue_index(i), 1-based. An index past the function's
            // upvalues is acceptable but reads as none.
            let i = (REGISTRY_INDEX as i64 - idx as i64) as usize;
            if i <= c.upvalue_count() {
                Slot::Upvalue(i - 1)
            } else {
                Slot::None
            }
        }
    }

    #[inline]
    fn get(&self, idx: i32) -> RawValue {
        self.given(idx).unwrap_or(RawValue::Nil)
    }

    /// The value at `idx`, or `None` when the index is outside the window.
    /// An argument's index (positive) is resolved in place, as every
    /// native's argument checks and [`Stack::compare`] ask for one.
    #[inline(always)]
    pub(crate) fn given(&self, idx: i32) -> Option<RawValue> {
        if idx > 0 {
            // One check against the top, rather than `len` and then `arg`.
            let at = self.ctx.base + idx as usize - 1;
            let rt = &self.ctx.vm.rt;
            return if at < rt.top {
                Some(rt.stack.regs[at])
            } else {
                None
            };
        }
        if idx < 0 && idx > REGISTRY_INDEX {
            // A negative index counts down from the top.
            let rt = &self.ctx.vm.rt;
            let back = idx.unsigned_abs() as usize;
            return if back <= rt.top - self.ctx.base {
                Some(rt.stack.regs[rt.top - back])
            } else {
                None
            };
        }
        self.given_slot(idx)
    }

    fn given_slot(&self, idx: i32) -> Option<RawValue> {
        Some(match self.slot(idx) {
            Slot::Stack(i) => self.ctx.arg(i),
            Slot::Registry => RawValue::Table(self.ctx.vm.rt.registry),
            Slot::Upvalue(i) => self.ctx.upvalue(i),
            Slot::None => return None,
        })
    }

    /// A read-only look at the isolate, for a `pure` native's
    /// [`View`](super::View) parameter.
    #[doc(hidden)]
    #[inline(always)]
    pub fn view(&self) -> super::View<'_> {
        super::View::new(&*self.ctx.vm)
    }

    fn is_none(&self, idx: i32) -> bool {
        matches!(self.slot(idx), Slot::None)
    }

    fn put(&mut self, idx: i32, v: RawValue) -> StackResult<()> {
        match self.slot(idx) {
            Slot::Stack(i) => self.ctx.set(i, v),
            Slot::Upvalue(i) => self.ctx.set_upvalue(i, v),
            Slot::Registry | Slot::None => return Err(self.error("invalid index in stack call")),
        }
        Ok(())
    }

    fn pop_raw(&mut self) -> StackResult<RawValue> {
        let len = self.ctx.len();
        if len == 0 {
            return Err(self.error("not enough elements in the stack"));
        }
        let v = self.ctx.arg(len - 1);
        self.ctx.truncate(len - 1);
        Ok(v)
    }

    fn need(&mut self, n: usize) -> StackResult<()> {
        if self.ctx.len() < n {
            return Err(self.error("not enough elements in the stack"));
        }
        Ok(())
    }

    fn table_at(&mut self, idx: i32) -> StackResult<Gc<Table>> {
        match self.get(idx) {
            RawValue::Table(t) => Ok(t),
            _ => Err(self.error("table expected")),
        }
    }

    /// The window slot a stack index names, or an error.
    fn stack_slot(&mut self, idx: i32) -> StackResult<usize> {
        match self.slot(idx) {
            Slot::Stack(i) => Ok(i),
            _ => Err(self.error("invalid stack index")),
        }
    }

    fn store_error(&mut self, e: tbl::StoreError) -> Raised {
        Raised::from(match e {
            tbl::StoreError::NoMemory => self.ctx.no_memory(),
            e => self.ctx.error(e.message()),
        })
    }

    fn top_value(&self) -> RawValue {
        let len = self.ctx.len();
        if len > 0 {
            self.ctx.arg(len - 1)
        } else {
            RawValue::Nil
        }
    }

    fn bytes_of(&self, v: RawValue) -> Option<&[u8]> {
        match v {
            RawValue::Str(s) => Some(self.ctx.bytes(s)),
            _ => None,
        }
    }

    /// Argument `arg` (1-based) as a string (a number converts in place),
    /// as a handle the typed arguments read the bytes through once.
    #[inline]
    pub(crate) fn check_string_handle(&mut self, arg: i32) -> StackResult<Gc<LuaStr>> {
        Ok(self.ctx.check_str((arg.max(1) - 1) as usize)?)
    }

    /// The bytes of a string the window holds.
    #[inline(always)]
    pub(crate) fn handle_bytes(&self, s: Gc<LuaStr>) -> &[u8] {
        self.ctx.bytes(s)
    }

    /// The string at `idx` (an argument or an upvalue) as a handle, for
    /// results built from a string argument after it was checked.
    #[inline]
    pub(crate) fn string_handle(&self, idx: i32) -> Option<Gc<LuaStr>> {
        match self.get(idx) {
            RawValue::Str(s) => Some(s),
            _ => None,
        }
    }

    /// Push bytes `from..to` of `s`, a string the window or an upvalue
    /// holds, copied straight from it.
    #[inline]
    pub(crate) fn push_substring(&mut self, s: Gc<LuaStr>, from: usize, to: usize) {
        let v = self.ctx.new_substr(s, from, to);
        self.ctx.push(v);
    }

    /// An empty buffer to build a string result in, reusing the isolate's
    /// scratch buffer: push it with [`Buffer`](super::Buffer) as the
    /// result, which hands the buffer back. One that is dropped instead
    /// (on an error) only costs an allocation.
    #[inline]
    pub(crate) fn take_buffer(&mut self) -> Vec<u8> {
        self.ctx.take_buf()
    }

    /// Give a buffer from [`Stack::take_buffer`] back unused.
    #[inline]
    pub(crate) fn give_buffer(&mut self, b: Vec<u8>) {
        self.ctx.give_buf(b);
    }

    /// Room for a pattern's captures, kept on the heap: see
    /// [`NativeCtx::take_captures`].
    #[inline]
    pub(crate) fn take_captures(&mut self) -> Box<crate::stdlib::string::pattern::Captures> {
        self.ctx.take_captures()
    }

    /// Give captures from [`Stack::take_captures`] back for reuse.
    #[inline]
    pub(crate) fn give_captures(&mut self, c: Box<crate::stdlib::string::pattern::Captures>) {
        self.ctx.give_captures(c);
    }

    /// Append the value at `idx` to `out` as `tostring` shows it without
    /// metamethods.
    #[inline(always)]
    pub(crate) fn display_into(&self, idx: i32, out: &mut Vec<u8>) {
        self.ctx.display(self.get(idx), out);
    }

    /// Replace argument `arg` with what its `__tostring` gives, if it is
    /// not a string and has one (`string.format`'s `%s`); called nested,
    /// so nothing may yield across it.
    pub(crate) fn tostring_in_place(&mut self, arg: i32) -> StackResult<()> {
        let v = self.get(arg);
        if matches!(v, RawValue::Str(_)) {
            return Ok(());
        }
        let h = self.ctx.metamethod(v, MetaField::ToString);
        if !h.is_nil() {
            let r = crate::stdlib::tostring_now(&mut self.ctx, h, v)?;
            self.ctx.set((arg - 1) as usize, r);
        }
        Ok(())
    }

    /// Push a string with the bytes of a buffer from
    /// [`Stack::take_buffer`], and keep the buffer for reuse.
    #[inline(always)]
    pub(crate) fn push_buffer(&mut self, b: Vec<u8>) {
        let v = self.ctx.new_str_buf(b);
        self.ctx.push(v);
    }

    /// Room for `n` more values on the stack, or the error `stack
    /// overflow (<what>)`, for a native that returns as many values as its
    /// arguments ask for.
    #[inline]
    pub(crate) fn need_room(&mut self, n: usize, what: &str) -> StackResult<()> {
        Ok(self.ctx.check_room(n, what)?)
    }

    /// The isolate's locale (ADR-13), for the libraries that follow it.
    #[inline]
    pub(crate) fn locale(&self) -> &crate::locale::Locale {
        self.ctx.locale()
    }

    /// Upvalue `i` (1-based) of the running native closure if it is a
    /// string, as a handle.
    #[inline]
    pub(crate) fn upvalue_string(&self, i: usize) -> Option<Gc<LuaStr>> {
        match self.ctx.upvalue(i - 1) {
            RawValue::Str(s) => Some(s),
            _ => None,
        }
    }

    /// Upvalue `i` (1-based) of the running native closure if it is an
    /// integer.
    #[inline]
    pub(crate) fn upvalue_integer(&self, i: usize) -> Option<i64> {
        match self.ctx.upvalue(i - 1) {
            RawValue::Int(n) => Some(n),
            _ => None,
        }
    }

    /// Set upvalue `i` (1-based) of the running native closure to an
    /// integer.
    #[inline]
    pub(crate) fn set_upvalue_integer(&mut self, i: usize, n: i64) {
        self.ctx.set_upvalue(i - 1, RawValue::Int(n));
    }

    /// Push a string with `bytes`, for results ([`Push`](super::Push)):
    /// [`Stack::push_bytes`] without reading the string back.
    #[inline]
    pub(crate) fn push_string_result(&mut self, bytes: &[u8]) {
        let v = self.ctx.new_str(bytes);
        self.ctx.push(v);
    }

    fn push_str_value(&mut self, bytes: &[u8]) -> Gc<LuaStr> {
        let v = self.ctx.new_str(bytes);
        self.ctx.push(v);
        let RawValue::Str(s) = v else {
            unreachable!("new_str makes a string")
        };
        s
    }

    // ---- calls back in -------------------------------------------------------

    /// Finish an operation that may need a metamethod: run it now. For a
    /// call, its arguments are the last `nargs` values of the window.
    fn finish_meta<T>(&mut self, m: Meta<T>, conv: impl FnOnce(RawValue) -> T) -> StackResult<T> {
        match m {
            Meta::Done(v) => Ok(v),
            Meta::Call { nargs } => Ok(conv(self.ctx.meta_value(nargs)?)),
        }
    }

    // A table's `__index` or `__newindex` function is called straight
    // away, as the runtime's own natives call it.
    #[inline]
    fn index(&mut self, obj: RawValue, key: RawValue) -> StackResult<RawValue> {
        // A `match` rather than `?`: the value comes back without being
        // copied through a second result.
        match self.ctx.index_now(obj, key) {
            Ok(v) => Ok(v),
            Err(e) => Err(e.into()),
        }
    }

    #[inline]
    fn set_index(&mut self, obj: RawValue, key: RawValue, v: RawValue) -> StackResult<()> {
        match self.ctx.set_index_now(obj, key, v) {
            Ok(()) => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    // ---- slots -----------------------------------------------------------------

    /// `idx` as an index that stays valid as values are pushed and popped.
    pub fn abs_index(&self, idx: i32) -> i32 {
        if idx > 0 || idx <= REGISTRY_INDEX {
            idx
        } else {
            self.ctx.len() as i32 + idx + 1
        }
    }

    /// How many values are in the window (the top's index).
    pub fn top(&self) -> i32 {
        self.ctx.len() as i32
    }

    /// Where the window starts in its thread's stack.
    pub fn window_start(&self) -> usize {
        self.ctx.window_start()
    }

    /// Grow the window with nils or cut it back to `idx` values (negative:
    /// relative to the top). Variables marked with [`Stack::to_close`]
    /// above the new top close first.
    #[inline]
    pub fn set_top(&mut self, idx: i32) -> StackResult<()> {
        // Dropping values none of which is to be closed, the common case.
        if let Ok(new) = usize::try_from(idx) {
            if new <= self.ctx.len() && !self.ctx.closes_from(new) {
                self.ctx.truncate(new);
                return Ok(());
            }
        }
        self.set_top_general(idx)
    }

    #[inline(never)]
    fn set_top_general(&mut self, idx: i32) -> StackResult<()> {
        let len = self.ctx.len() as i64;
        let new = if idx >= 0 {
            idx as i64
        } else {
            len + idx as i64 + 1
        };
        if new < 0 {
            return Err(self.error("invalid new top"));
        }
        let new = new as usize;
        if new > self.ctx.len() {
            self.ctx.set(new - 1, RawValue::Nil);
        }
        self.ctx.close_from(new)?;
        self.ctx.truncate(new);
        Ok(())
    }

    /// Mark the value at `idx` to be closed (its `__close` called) when it
    /// leaves the window (`lua_toclose`).
    pub fn to_close(&mut self, idx: i32) -> StackResult<()> {
        let i = self.stack_slot(idx)?;
        Ok(self.ctx.mark_to_be_closed(i)?)
    }

    /// Close the variable at `idx`, if it is the newest marked one, and set
    /// the slot to nil (`lua_closeslot`).
    pub fn close_slot(&mut self, idx: i32) -> StackResult<()> {
        let i = self.stack_slot(idx)?;
        if self.ctx.is_newest_to_be_closed(i) {
            self.ctx.close_from(i)?;
        }
        self.ctx.set(i, RawValue::Nil);
        Ok(())
    }

    /// Close every variable in the window marked with [`Stack::to_close`],
    /// as a native does when it returns.
    pub fn close_all(&mut self) -> StackResult<()> {
        Ok(self.ctx.close_from(0)?)
    }

    /// Push a copy of the value at `idx`.
    #[inline]
    pub fn push_copy(&mut self, idx: i32) {
        let v = self.get(idx);
        self.ctx.push(v);
    }

    /// Rotate the values from `idx` to the top `n` places towards the top
    /// (negative: towards `idx`).
    pub fn rotate(&mut self, idx: i32, n: i32) -> StackResult<()> {
        let Slot::Stack(start) = self.slot(idx) else {
            return Err(self.error("invalid index to rotate"));
        };
        let mut vals = self.ctx.window()[start..].to_vec();
        let len = vals.len() as i64;
        if len > 0 {
            let k = (n as i64).rem_euclid(len) as usize;
            vals.rotate_right(k);
            for (i, v) in vals.into_iter().enumerate() {
                self.ctx.set(start + i, v);
            }
        }
        Ok(())
    }

    /// Copy the value at `from` into `to`.
    pub fn copy(&mut self, from: i32, to: i32) -> StackResult<()> {
        let v = self.get(from);
        self.put(to, v)
    }

    /// Whether `n` more values fit in the window.
    pub fn check_room(&self, n: i32) -> bool {
        n >= 0 && self.ctx.len() + n as usize <= MAX_STACK
    }

    // ---- reading -----------------------------------------------------------------

    /// [`Stack::type_at`] for argument `i` (1-based), without the
    /// pseudo-index cases: the typed parameters' test.
    #[inline(always)]
    pub(crate) fn arg_type(&self, i: i32) -> Type {
        let at = (i as usize).wrapping_sub(1);
        if i >= 1 && at < self.ctx.len() {
            type_of(self.ctx.arg(at))
        } else {
            Type::None
        }
    }

    #[inline]
    pub fn type_at(&self, idx: i32) -> Type {
        match self.given(idx) {
            Some(v) => type_of(v),
            None => Type::None,
        }
    }

    /// The number at `idx`, converting a numeric string.
    pub fn to_number(&self, idx: i32) -> Option<f64> {
        match self.get(idx) {
            RawValue::Int(i) => Some(i as f64),
            RawValue::Float(f) => Some(f),
            RawValue::Str(s) => match self.ctx.locale().str_to_number(self.ctx.bytes(s)) {
                Some(RawValue::Int(i)) => Some(i as f64),
                Some(RawValue::Float(f)) => Some(f),
                _ => None,
            },
            _ => None,
        }
    }

    /// The integer at `idx`, converting a numeric string or an integral
    /// float.
    pub fn to_integer(&self, idx: i32) -> Option<i64> {
        let v = match self.get(idx) {
            RawValue::Str(s) => self.ctx.locale().str_to_number(self.ctx.bytes(s))?,
            v => v,
        };
        match v {
            RawValue::Int(i) => Some(i),
            RawValue::Float(f) => float_to_int(f),
            _ => None,
        }
    }

    /// Whether `idx` holds a string or a number.
    pub fn is_string(&self, idx: i32) -> bool {
        matches!(
            self.get(idx),
            RawValue::Str(_) | RawValue::Int(_) | RawValue::Float(_)
        )
    }

    /// The number at `idx`, if it holds one (no conversion from strings).
    #[inline]
    pub(crate) fn number_at(&self, idx: i32) -> Option<crate::object::number::Number> {
        use crate::object::number::Number;
        // An argument's index (the usual case) needs no pseudo-index checks.
        let v = if idx > 0 {
            let at = idx as usize - 1;
            if at < self.ctx.len() {
                self.ctx.arg(at)
            } else {
                return None;
            }
        } else {
            self.get(idx)
        };
        match v {
            RawValue::Int(i) => Some(Number::Int(i)),
            RawValue::Float(f) => Some(Number::Float(f)),
            _ => None,
        }
    }

    pub fn is_integer(&self, idx: i32) -> bool {
        matches!(self.get(idx), RawValue::Int(_))
    }

    /// Whether `idx` holds a userdata or a light userdata.
    pub fn is_userdata(&self, idx: i32) -> bool {
        matches!(
            self.get(idx),
            RawValue::UserData(_) | RawValue::LightUserData(_)
        )
    }

    /// Whether `idx` holds a function that isn't a script function: a
    /// native, in Rust or C.
    pub fn is_native_function(&self, idx: i32) -> bool {
        self.ctx.native_of(self.get(idx)).is_some()
    }

    pub fn to_boolean(&self, idx: i32) -> bool {
        !self.get(idx).is_falsy()
    }

    /// The string at `idx`, converting a number in place (`lua_tolstring`);
    /// `None` for other types.
    pub fn to_str(&mut self, idx: i32) -> StackResult<Option<OrbitStr<'_>>> {
        Ok(self.converted_str(idx)?.map(|s| self.ctx.bytes(s).into()))
    }

    /// [`Stack::to_str`] for C (`lua_tolstring`): the address of a
    /// NUL-terminated copy of the string, and its length in bytes. The copy
    /// belongs to the string: it is made once, never changes, and stays at
    /// that address for as long as the string is alive.
    pub fn to_c_str(&mut self, idx: i32) -> StackResult<Option<(*const u8, usize)>> {
        Ok(self
            .converted_str(idx)?
            .map(|s| (self.ctx.c_string(s), self.ctx.bytes(s).len())))
    }

    /// The string at `idx`, converting a number in place.
    fn converted_str(&mut self, idx: i32) -> StackResult<Option<Gc<LuaStr>>> {
        let s = match self.get(idx) {
            RawValue::Str(s) => s,
            v @ (RawValue::Int(_) | RawValue::Float(_)) => {
                let mut out = Vec::new();
                self.ctx.locale().fmt_number(&mut out, v);
                let RawValue::Str(s) = self.ctx.new_str(&out) else {
                    unreachable!("new_str makes a string")
                };
                if let Slot::Stack(_) | Slot::Upvalue(_) = self.slot(idx) {
                    self.put(idx, RawValue::Str(s))?;
                }
                s
            }
            _ => return Ok(None),
        };
        Ok(Some(s))
    }

    /// The string at `idx`, without converting numbers.
    pub fn string_at(&self, idx: i32) -> Option<OrbitStr<'_>> {
        self.bytes_of(self.get(idx)).map(OrbitStr::from)
    }

    /// The number at `idx` as Lua prints it, or `None` if it isn't one.
    pub fn number_text(&self, idx: i32) -> Option<String> {
        match self.get(idx) {
            v @ (RawValue::Int(_) | RawValue::Float(_)) => {
                let mut out = Vec::new();
                self.ctx.locale().fmt_number(&mut out, v);
                // Decimal points are text in every locale.
                Some(String::from_utf8_lossy(&out).into_owned())
            }
            _ => None,
        }
    }

    /// The length of the value at `idx` with no metamethod: a string's
    /// bytes, a table's border, a [`Stack::new_userdata`] block's size.
    pub fn raw_len(&self, idx: i32) -> u64 {
        match self.get(idx) {
            RawValue::Str(s) => self.ctx.bytes(s).len() as u64,
            RawValue::Table(t) => {
                let heap = &self.ctx.vm.heap;
                heap.get(t).border(HashSeed::of(heap)) as u64
            }
            RawValue::UserData(u) => self
                .ctx
                .vm
                .heap
                .get(u)
                .downcast_ref::<Block>()
                .map_or(0, |b| b.size as u64),
            _ => 0,
        }
    }

    /// The memory of a userdata [`Stack::new_userdata`] made, or a light
    /// userdata's pointer. Null for anything else, including userdata a
    /// host made with typed values (an `orbit::Class` instance).
    pub fn to_userdata(&self, idx: i32) -> *mut c_void {
        match self.get(idx) {
            RawValue::UserData(u) => self
                .ctx
                .vm
                .heap
                .get(u)
                .downcast_ref::<Block>()
                .map_or(std::ptr::null_mut(), Block::as_ptr),
            RawValue::LightUserData(p) => p.get() as usize as *mut c_void,
            _ => std::ptr::null_mut(),
        }
    }

    /// Run `f` on the cell holding the host value of type `T` in the
    /// userdata at `idx` ([`Stack::push_userdata`]). The cell reads `None`
    /// while the value is taken out (a running method has it). `None`,
    /// without calling `f`, if the value there holds no `T`.
    pub fn with_userdata<T: Any, R>(
        &mut self,
        idx: i32,
        f: impl FnOnce(&mut Option<T>) -> R,
    ) -> Option<R> {
        match self.get(idx) {
            RawValue::UserData(u) => self
                .ctx
                .userdata_mut::<HostValue<T>>(u)
                .map(|s| f(&mut s.0)),
            _ => None,
        }
    }

    /// The host value of type `T` in the userdata at `idx`, in place: for
    /// the typed handles' borrows.
    #[doc(hidden)]
    pub fn userdata_in_place<T: Any>(&self, idx: i32) -> Option<&T> {
        match self.get(idx) {
            RawValue::UserData(u) => self
                .ctx
                .userdata_ref::<HostValue<T>>(u)
                .and_then(|s| s.0.as_ref()),
            _ => None,
        }
    }

    /// [`Stack::userdata_in_place`], mutably.
    #[doc(hidden)]
    pub fn userdata_in_place_mut<T: Any>(&mut self, idx: i32) -> Option<&mut T> {
        match self.get(idx) {
            RawValue::UserData(u) => self
                .ctx
                .userdata_mut::<HostValue<T>>(u)
                .and_then(|s| s.0.as_mut()),
            _ => None,
        }
    }

    /// Whether `idx` is a userdata holding a host value rather than a
    /// [`Stack::new_userdata`] block.
    pub fn is_host_userdata(&self, idx: i32) -> bool {
        match self.get(idx) {
            RawValue::UserData(u) => !self.ctx.vm.heap.get(u).is::<Block>(),
            _ => false,
        }
    }

    /// The identity of a table, function, thread or userdata, as `%p`
    /// shows it; `None` for other values. The same while the object lives.
    pub fn identity(&self, idx: i32) -> Option<u64> {
        self.get(idx).identity()
    }

    /// A value's identity as a number (`lua_topointer`), for hashing and
    /// printing only; 0 for values without one.
    pub fn to_pointer(&self, idx: i32) -> usize {
        match self.get(idx) {
            RawValue::UserData(_) | RawValue::LightUserData(_) => self.to_userdata(idx) as usize,
            v => v.identity().unwrap_or(0) as usize,
        }
    }

    /// The data word of the closure of native `N` at `idx`
    /// ([`Stack::push_native_closure`]); `None` if it isn't one.
    pub fn closure_data_of<N: Native>(&mut self, idx: i32) -> Option<u64> {
        let id = self.native_index::<N>();
        self.closure_data_of_index(idx, id)
    }

    /// The index native `N` is registered under, registering it first if it
    /// isn't. Natives are told apart by type, not by `NAME`, so an
    /// embedder's `tostring` is not the built-in one.
    #[inline]
    fn native_index<N: Native>(&mut self) -> u32 {
        self.native_id(
            TypeId::of::<N>(),
            N::NAME,
            native_entry::<N>,
            pure_fast::<N>(),
        )
        .0
    }

    /// The non-generic half of registering a native: its index, with the
    /// fast path set when it has one.
    #[inline(never)]
    fn native_id(
        &mut self,
        ty: TypeId,
        name: &'static str,
        f: vm::NativeFn,
        fast: Option<vm::FastFn>,
    ) -> NativeId {
        let id = self.ctx.vm.native_for_type(ty, name, f);
        if fast.is_some() {
            self.ctx.vm.rt.natives.get_mut(id).fast = fast;
        }
        id
    }

    /// [`Stack::closure_data_of`] for the native with index `id`.
    pub(crate) fn closure_data_of_index(&mut self, idx: i32, id: u32) -> Option<u64> {
        let v = self.get(idx);
        if self.ctx.native_of(v) != Some(NativeId(id)) {
            return None;
        }
        match self.ctx.hidden_upvalue_of(v, 0) {
            RawValue::LightUserData(p) => Some(p.get()),
            _ => None,
        }
    }

    /// Whether the values at `a` and `b` are equal without metamethods.
    pub fn raw_equal(&self, a: i32, b: i32) -> bool {
        !self.is_none(a) && !self.is_none(b) && raw_eq(&self.ctx.vm.heap, self.get(a), self.get(b))
    }

    /// Compare the values at `a` and `b` as the operator would, metamethods
    /// included; false if either index is outside the window.
    pub fn compare(&mut self, a: i32, b: i32, op: CompareOp) -> StackResult<bool> {
        let (Some(x), Some(y)) = (self.given(a), self.given(b)) else {
            return Ok(false);
        };
        if op == CompareOp::Lt {
            // A `__lt` function is called straight away, as `math.max`,
            // `math.min` and the like call it once per argument.
            return Ok(self.ctx.less_than_now(x, y)?);
        }
        // A Lua 5.4 build answers a missing `__le` with `__lt` (ADR-14).
        let negate = op == CompareOp::Le && self.ctx.le_via_lt(x, y);
        let m = match op {
            CompareOp::Eq => self.ctx.equal(x, y)?,
            CompareOp::Lt => self.ctx.less_than(x, y)?,
            CompareOp::Le => self.ctx.less_equal(x, y)?,
        };
        self.finish_meta(m, |v| v.is_falsy() == negate)
    }

    // ---- operators ---------------------------------------------------------------

    /// Apply `op` to the top two values (one for a unary operator),
    /// replacing them with the result.
    pub fn arith(&mut self, op: ArithOp) -> StackResult<()> {
        let unary = op.unary();
        self.need(if unary { 1 } else { 2 })?;
        let len = self.ctx.len();
        let (a, b) = if unary {
            (self.ctx.arg(len - 1), self.ctx.arg(len - 1))
        } else {
            (self.ctx.arg(len - 2), self.ctx.arg(len - 1))
        };
        let m = self.ctx.arith(operator(op), a, b)?;
        let r = self.finish_meta(m, |v| v)?;
        // The operands stay on the stack (rooted) until the result replaces them.
        let len = self.ctx.len();
        self.ctx.truncate(len - if unary { 1 } else { 2 });
        self.ctx.push(r);
        Ok(())
    }

    /// Join the top `n` values as `..` does, replacing them with the result
    /// (an empty string when `n` is 0).
    pub fn concat(&mut self, n: i32) -> StackResult<()> {
        let n = n.max(0) as usize;
        self.need(n)?;
        if n == 0 {
            self.push_str_value(b"");
            return Ok(());
        }
        // Right to left, as `..` associates: each step joins the last two.
        for _ in 1..n {
            let len = self.ctx.len();
            let (a, b) = (self.ctx.arg(len - 2), self.ctx.arg(len - 1));
            let m = self.ctx.concat(a, b)?;
            let r = self.finish_meta(m, |v| v)?;
            let len = self.ctx.len();
            self.ctx.truncate(len - 2);
            self.ctx.push(r);
        }
        Ok(())
    }

    /// The length of the value at `idx`, as `#` gives it, if it is an
    /// integer or converts to one (a float with an integral value or a
    /// numeric string): the table functions' lengths.
    #[inline]
    pub(crate) fn length_integer(&mut self, idx: i32) -> StackResult<Option<i64>> {
        let v = self.get(idx);
        Ok(match self.ctx.length_now(v)? {
            RawValue::Int(n) => Some(n),
            n => self.ctx.to_integer(n),
        })
    }

    /// Push the length of the value at `idx`, as `#` gives it.
    pub fn len(&mut self, idx: i32) -> StackResult<()> {
        let v = self.get(idx);
        let r = self.ctx.length_now(v)?;
        self.ctx.push(r);
        Ok(())
    }

    /// Push the number `text` spells, as `tonumber` reads it, and give its
    /// length plus one; 0, pushing nothing, if it isn't a numeral.
    pub fn push_numeral<'t>(&mut self, text: impl Into<OrbitStr<'t>>) -> usize {
        self.push_numeral_str(text.into())
    }

    fn push_numeral_str(&mut self, text: OrbitStr<'_>) -> usize {
        let text = text.as_bytes();
        match self.ctx.locale().str_to_number(text) {
            Some(v) => {
                self.ctx.push(v);
                text.len() + 1
            }
            None => 0,
        }
    }

    // ---- pushing -------------------------------------------------------------------

    pub fn push_nil(&mut self) {
        self.ctx.push(RawValue::Nil);
    }

    pub fn push_boolean(&mut self, b: bool) {
        self.ctx.push(RawValue::Bool(b));
    }

    pub fn push_integer(&mut self, n: i64) {
        self.ctx.push(RawValue::Int(n));
    }

    pub fn push_number(&mut self, n: f64) {
        self.ctx.push(RawValue::Float(n));
    }

    /// Push a string with these bytes, and give its bytes as stored.
    pub fn push_bytes<'b>(&mut self, bytes: impl Into<OrbitStr<'b>>) -> OrbitStr<'_> {
        self.push_bytes_str(bytes.into())
    }

    fn push_bytes_str(&mut self, bytes: OrbitStr<'_>) -> OrbitStr<'_> {
        let bytes = bytes.as_bytes();
        let s = self.push_str_value(bytes);
        self.ctx.bytes(s).into()
    }

    pub fn push_light_userdata(&mut self, p: usize) {
        self.ctx.push(light(p));
    }

    /// Push a userdata holding host value `value`, which
    /// [`Stack::with_userdata`] reads back.
    pub fn push_userdata<T: Any>(&mut self, value: T) {
        let u = self.ctx.new_userdata(HostValue(Some(value)));
        self.ctx.push(RawValue::UserData(u));
    }

    /// Account for `bytes` of memory the caller is about to allocate
    /// outside the heap, raising Lua's memory error past the heap limit.
    pub fn reserve_memory(&mut self, bytes: usize) -> StackResult<()> {
        Ok(self.ctx.reserve(bytes)?)
    }

    /// Push a function that runs `f`, which reads its arguments and
    /// returns as a [`Native`] does. The closure lives as long as the
    /// function value.
    pub fn push_function(
        &mut self,
        f: impl for<'a, 'b> Fn(&'a mut Stack<'b>) -> StackResult<Return> + 'static,
    ) {
        let f: FnBox = std::rc::Rc::new(f);
        let u = RawValue::UserData(self.ctx.new_userdata(FunctionBox(f)));
        let at = self.ctx.len();
        self.ctx.push(u);
        let v = self
            .ctx
            .new_closure_hidden(CLOSURE_NAME, function_entry, &[], &[u]);
        self.ctx.truncate(at);
        self.ctx.push(v);
    }

    /// Push native `N` as a function value.
    #[inline]
    pub fn push_native<N: Native>(&mut self) {
        let id = self.native_id(
            TypeId::of::<N>(),
            N::NAME,
            native_entry::<N>,
            pure_fast::<N>(),
        );
        self.ctx.push(RawValue::Native(id));
    }

    /// Push a function the runtime keeps for a library written above it
    /// (ADR-11), by its qualified name: the `table` and `string` libraries'
    /// functions (`table.concat`, `string.gsub`, ...; `table.create` only
    /// in Lua 5.5 builds) and the string metatable's arithmetic
    /// metamethods (`string.__add`, `string.__sub`, `string.__mul`,
    /// `string.__mod`, `string.__pow`, `string.__div`, `string.__idiv`,
    /// `string.__unm`). False, pushing nothing, for any other name.
    pub fn push_library_function(&mut self, name: &str) -> bool {
        let Some(row) = crate::stdlib::library_function(name) else {
            return false;
        };
        let v = match self.ctx.vm.rt.natives.named(row.name) {
            Some(id) => RawValue::Native(id),
            None => crate::stdlib::register_row(self.ctx.vm, row),
        };
        self.ctx.push(v);
        true
    }

    /// Push a closure of native `N` over the top `nups` values (popped),
    /// which it reads at [`upvalue_index`](super::upvalue_index), and over `data`, a word scripts
    /// can neither see nor change ([`Stack::closure_data`]).
    pub fn push_native_closure<N: Native>(&mut self, nups: usize, data: u64) -> StackResult<()> {
        let id = self.native_index::<N>();
        self.push_closure_of(id, nups, data)
    }

    /// [`Stack::push_native_closure`] for the native with index `id`.
    pub(crate) fn push_closure_of(&mut self, id: u32, nups: usize, data: u64) -> StackResult<()> {
        self.need(nups)?;
        let len = self.ctx.len();
        let hidden = [RawValue::LightUserData(LightUserData::new(data))];
        // A few upvalues (the usual case) are copied without allocating.
        let mut few = [RawValue::Nil; 8];
        let many;
        let ups = if nups <= few.len() {
            few[..nups].copy_from_slice(&self.ctx.window()[len - nups..]);
            &few[..nups]
        } else {
            many = self.ctx.window()[len - nups..].to_vec();
            &many[..]
        };
        let v = self.ctx.new_native_closure(NativeId(id), ups, &hidden);
        self.ctx.truncate(len - nups);
        self.ctx.push(v);
        Ok(())
    }

    /// The running native closure's data word ([`Stack::push_native_closure`]).
    pub fn closure_data(&self) -> u64 {
        match self.ctx.hidden_upvalue(0) {
            RawValue::LightUserData(p) => p.get(),
            _ => 0,
        }
    }

    /// The running native's data words ([`Control::register_native`]).
    pub fn native_data(&self) -> [u64; 2] {
        self.ctx.native_data()
    }

    /// Push a native registered with [`Control::register_native`].
    pub fn push_registered(&mut self, f: NativeRef) {
        self.ctx.push(f.0);
    }

    // ---- tables ------------------------------------------------------------------

    fn get_with(&mut self, obj: RawValue, key: RawValue) -> StackResult<Type> {
        // Keep the key rooted while metamethods run.
        self.ctx.push(key);
        let v = self.index(obj, key);
        let len = self.ctx.len();
        self.ctx.truncate(len - 1);
        let v = v?;
        self.ctx.push(v);
        Ok(type_of(v))
    }

    /// Push `t[k]` for the table at `idx` and the key on top (popped),
    /// metamethods included.
    pub fn get_table(&mut self, idx: i32) -> StackResult<Type> {
        let obj = self.get(idx);
        let key = self.pop_raw()?;
        self.get_with(obj, key)
    }

    /// Push `t[k]` for the value at `idx` and string key `k`.
    pub fn get_field<'k>(&mut self, idx: i32, k: impl Into<OrbitStr<'k>>) -> StackResult<Type> {
        self.get_field_str(idx, k.into())
    }

    fn get_field_str(&mut self, idx: i32, k: OrbitStr<'_>) -> StackResult<Type> {
        let k = k.as_bytes();
        let obj = self.get(idx);
        let key = self.ctx.new_str(k);
        self.get_with(obj, key)
    }

    /// Push `t[n]` for the value at `idx`.
    #[inline(always)]
    pub fn get_index(&mut self, idx: i32, n: i64) -> StackResult<Type> {
        // An integer key needs no rooting while metamethods run.
        let obj = self.get(idx);
        // Pushing in the `Ok` arm saves copying the value out first.
        match self.ctx.index_now(obj, RawValue::Int(n)) {
            Ok(v) => {
                self.ctx.push(v);
                Ok(type_of(v))
            }
            Err(e) => Err(e.into()),
        }
    }

    /// Push the global `name`.
    pub fn get_global<'n>(&mut self, name: impl Into<OrbitStr<'n>>) -> StackResult<Type> {
        self.get_global_str(name.into())
    }

    fn get_global_str(&mut self, name: OrbitStr<'_>) -> StackResult<Type> {
        let name = name.as_bytes();
        let g = RawValue::Table(self.ctx.vm.rt.globals);
        let k = self.ctx.new_str(name);
        self.get_with(g, k)
    }

    fn raw_get_with(&mut self, idx: i32, key: RawValue) -> StackResult<Type> {
        let t = self.table_at(idx)?;
        let v = tbl::get(&self.ctx.vm.heap, t, key);
        self.ctx.push(v);
        Ok(type_of(v))
    }

    /// Push `t[k]` for the table at `idx` and the key on top (popped), with
    /// no metamethods.
    pub fn raw_get(&mut self, idx: i32) -> StackResult<Type> {
        let t = self.table_at(idx)?;
        let key = self.pop_raw()?;
        let v = tbl::get(&self.ctx.vm.heap, t, key);
        self.ctx.push(v);
        Ok(type_of(v))
    }

    pub fn raw_get_index(&mut self, idx: i32, n: i64) -> StackResult<Type> {
        self.raw_get_with(idx, RawValue::Int(n))
    }

    /// Push `t[k]` for a light userdata key `p`, with no metamethods.
    pub fn raw_get_light(&mut self, idx: i32, p: usize) -> StackResult<Type> {
        self.raw_get_with(idx, light(p))
    }

    /// Push `t[k]` for string key `k`, with no metamethods.
    pub fn raw_get_field<'k>(&mut self, idx: i32, k: impl Into<OrbitStr<'k>>) -> StackResult<Type> {
        self.raw_get_field_str(idx, k.into())
    }

    fn raw_get_field_str(&mut self, idx: i32, k: OrbitStr<'_>) -> StackResult<Type> {
        let k = k.as_bytes();
        let key = self.ctx.new_str(k);
        self.raw_get_with(idx, key)
    }

    /// Push a new table with room for `narr` array and `nrec` other entries.
    pub fn create_table(&mut self, narr: i32, nrec: i32) {
        let t = self.ctx.new_table(Table::with_capacity(
            narr.max(0) as usize,
            nrec.max(0) as usize,
        ));
        self.ctx.push(RawValue::Table(t));
    }

    /// [`Stack::create_table`] with room for `narr` array and `nrec` hash
    /// entries, or `not enough memory` if they can't be allocated.
    pub(crate) fn try_create_table(&mut self, narr: usize, nrec: usize) -> StackResult<()> {
        let Ok(t) = Table::try_with_capacity(narr, nrec) else {
            return Err(Raised::from(self.ctx.no_memory()));
        };
        let t = self.ctx.new_table(t);
        self.ctx.push(RawValue::Table(t));
        Ok(())
    }

    /// Push a table with the `count` values from index `first` at keys
    /// `1..=count`, and room for `nrec` more entries.
    pub(crate) fn push_array_of(
        &mut self,
        first: i32,
        count: usize,
        nrec: usize,
    ) -> StackResult<()> {
        let t = self.ctx.new_table(Table::with_capacity(count, nrec));
        self.ctx.push(RawValue::Table(t));
        if LUA.is_5_4() {
            // As a 5.4 constructor does: all `count` positions in the
            // array part, nils included (`Table::lengthen_array`).
            let seed = HashSeed::of(&self.ctx.vm.heap);
            self.ctx
                .vm
                .heap
                .update(t, |tab| tab.lengthen_array(seed, 1, count))
                .map_err(|_| self.memory_error())?;
        }
        let from = (first.max(1) - 1) as usize;
        for i in 0..count {
            let v = self.ctx.arg(from + i);
            crate::object::table::set_int(&mut self.ctx.vm.heap, t, i as i64 + 1, v)
                .map_err(|_| Raised::from(self.ctx.no_memory()))?;
        }
        Ok(())
    }

    /// Push a userdata of `size` bytes, zeroed and aligned to 16, with
    /// `nuv` user values, and give its memory, which stays put as long as
    /// the userdata lives.
    pub fn new_userdata(&mut self, size: usize, nuv: i32) -> StackResult<*mut c_void> {
        if nuv < 0 || nuv > u16::MAX as i32 {
            return Err(self.error("invalid number of user values"));
        }
        self.ctx.reserve(size)?;
        let block = Block::new(size, nuv as u16);
        let p = block.as_ptr();
        let u = self.ctx.new_userdata(block);
        self.ctx.push(RawValue::UserData(u));
        if nuv >= 2 {
            // More than one user value: they live in a table in the
            // userdata's one traced slot.
            let t = self.ctx.new_table(Table::with_capacity(nuv as usize, 0));
            self.ctx
                .vm
                .heap
                .update(u, |u| u.user_value = RawValue::Table(t));
        }
        Ok(p)
    }

    fn user_value(&self, idx: i32, n: i32) -> Option<UserValue> {
        let RawValue::UserData(u) = self.get(idx) else {
            return None;
        };
        let ud = self.ctx.vm.heap.get(u);
        let count = ud.downcast_ref::<Block>().map_or(1, |b| b.nuv as i32);
        if n < 1 || n > count {
            return None;
        }
        if count == 1 {
            return Some(UserValue::Slot(u));
        }
        match ud.user_value {
            RawValue::Table(t) => Some(UserValue::InTable(t, n as i64)),
            _ => None,
        }
    }

    /// Push user value `n` (1-based) of the userdata at `idx`; nil and
    /// [`Type::None`] if it has none.
    pub fn get_user_value(&mut self, idx: i32, n: i32) -> Type {
        let v = match self.user_value(idx, n) {
            Some(UserValue::Slot(u)) => self.ctx.vm.heap.get(u).user_value,
            Some(UserValue::InTable(t, i)) => {
                let heap = &self.ctx.vm.heap;
                heap.get(t).get_int(HashSeed::of(heap), i)
            }
            None => {
                self.ctx.push(RawValue::Nil);
                return Type::None;
            }
        };
        self.ctx.push(v);
        type_of(v)
    }

    /// Pop the top value into user value `n` of the userdata at `idx`;
    /// false if it has no such value.
    pub fn set_user_value(&mut self, idx: i32, n: i32) -> StackResult<bool> {
        self.need(1)?;
        let len = self.ctx.len();
        let v = self.ctx.arg(len - 1);
        let r = match self.user_value(idx, n) {
            Some(UserValue::Slot(u)) => {
                self.ctx.vm.heap.update(u, |u| u.user_value = v);
                true
            }
            Some(UserValue::InTable(t, i)) => {
                if tbl::set_int(&mut self.ctx.vm.heap, t, i, v).is_err() {
                    return Err(Raised::from(self.ctx.no_memory()));
                }
                true
            }
            None => false,
        };
        self.ctx.truncate(len - 1);
        Ok(r)
    }

    /// Whether the metatable of the value at `idx` has field `e` (raw),
    /// without interning its name: `__metatable` for the base library.
    pub(crate) fn has_meta_field(&self, idx: i32, e: crate::vm::MetaField) -> bool {
        !self.ctx.metamethod(self.get(idx), e).is_nil()
    }

    /// The prototype of the Lua function at `idx`; `None` for anything
    /// else, a native closure (a host or C function) included.
    pub(crate) fn lua_function_proto(&self, idx: i32) -> Option<Gc<crate::object::Proto>> {
        let f = self.get(idx);
        match (f, self.ctx.native_of(f)) {
            (RawValue::Closure(c), None) => Some(self.ctx.vm.heap.get(c).proto),
            _ => None,
        }
    }

    /// The heap, for the runtime's own natives that read objects directly.
    pub(crate) fn heap(&self) -> &crate::object::LuaHeap {
        &self.ctx.vm.heap
    }

    /// The number at `idx`, or the number a string there converts to
    /// (with the locale's decimal point), as arithmetic coerces it.
    pub(crate) fn coerced_number(&self, idx: i32) -> Option<crate::object::number::Number> {
        match self.get(idx) {
            RawValue::Str(s) => self
                .ctx
                .locale()
                .str_to_number(self.ctx.bytes(s))
                .and_then(RawValue::as_number),
            v => v.as_number(),
        }
    }

    /// Push the value at `idx`'s metamethod for `event`; false, pushing
    /// nothing, when it has none.
    pub(crate) fn push_metamethod(&mut self, idx: i32, event: crate::vm::MetaField) -> bool {
        let h = self.ctx.metamethod(self.get(idx), event);
        if h.is_nil() {
            return false;
        }
        self.ctx.push(h);
        true
    }

    /// The table at `idx` if it has no metatable, for the table
    /// library's raw loops.
    #[inline]
    pub(crate) fn plain_table(&self, idx: i32) -> Option<Gc<crate::object::Table>> {
        match self.get(idx) {
            RawValue::Table(t) if self.ctx.vm.heap.get(t).meta.is_none() => Some(t),
            _ => None,
        }
    }

    /// `t[i]` without metamethods, for a table the window holds.
    #[inline]
    pub(crate) fn push_raw_index(&mut self, t: Gc<crate::object::Table>, i: i64) {
        let heap = &self.ctx.vm.heap;
        let v = heap.get(t).get_int(crate::object::HashSeed::of(heap), i);
        self.ctx.push(v);
    }

    /// Push a new table with room for `n` array elements (up to a cap;
    /// bigger arrays grow as they fill), and give it.
    pub(crate) fn push_array_table(&mut self, n: i64) -> StackResult<Gc<Table>> {
        /// Most slots reserved up front.
        const MAX_RESERVE: usize = 1 << 24;
        let n = (n.max(0) as usize).min(MAX_RESERVE);
        self.ctx.reserve(n.saturating_mul(9))?;
        let Ok(t) = Table::try_with_capacity(n, 0) else {
            return Err(self.memory_error());
        };
        let t = self.ctx.new_table(t);
        self.ctx.push(RawValue::Table(t));
        Ok(t)
    }

    /// `table.sort`'s fast path for a table the window holds; see
    /// [`stdlib::table::sort_primitive`].
    pub(crate) fn sort_primitive(&mut self, t: Gc<Table>, n: i64) -> StackResult<bool> {
        match stdlib::table::sort_primitive(self.ctx.vm, t, n) {
            Ok(sorted) => Ok(sorted),
            Err(NoMemory) => Err(self.memory_error()),
        }
    }

    /// The border of a table the window holds (`#t` without `__len`).
    #[inline]
    pub(crate) fn raw_border(&self, t: Gc<crate::object::Table>) -> i64 {
        let heap = &self.ctx.vm.heap;
        heap.get(t).border(crate::object::HashSeed::of(heap))
    }

    /// Room left on the stack, in values.
    #[inline]
    pub(crate) fn room(&self) -> usize {
        self.ctx
            .vm
            .rt
            .host
            .max_stack()
            .saturating_sub(self.ctx.len())
    }

    /// Copy `src[from]` to `dst[to]` without metamethods, for tables the
    /// window holds.
    #[inline]
    pub(crate) fn raw_copy_int(
        &mut self,
        src: Gc<crate::object::Table>,
        from: i64,
        dst: Gc<crate::object::Table>,
        to: i64,
    ) -> StackResult<()> {
        let heap = &self.ctx.vm.heap;
        let v = heap
            .get(src)
            .get_int(crate::object::HashSeed::of(heap), from);
        self.raw_put_int(dst, to, v)
    }

    /// `t[i] = ` the value at `idx`, without metamethods, for a table the
    /// window holds.
    #[inline]
    pub(crate) fn raw_set_int_from(
        &mut self,
        t: Gc<crate::object::Table>,
        i: i64,
        idx: i32,
    ) -> StackResult<()> {
        let v = self.get(idx);
        self.raw_put_int(t, i, v)
    }

    #[inline]
    fn raw_put_int(&mut self, t: Gc<crate::object::Table>, i: i64, v: RawValue) -> StackResult<()> {
        match crate::object::table::set_int(&mut self.ctx.vm.heap, t, i, v) {
            Ok(_) => Ok(()),
            Err(_) => Err(self.memory_error()),
        }
    }

    /// Append `t[i]` (no metamethods) to `out` as `concat` writes it, with
    /// room for `extra` bytes more; `Err` with the value's type name if it
    /// is neither a string nor a number.
    #[inline(always)]
    pub(crate) fn append_raw_index(
        &mut self,
        t: Gc<crate::object::Table>,
        i: i64,
        extra: usize,
        out: &mut Vec<u8>,
    ) -> StackResult<Result<(), &'static str>> {
        let heap = &self.ctx.vm.heap;
        let v = heap.get(t).get_int(crate::object::HashSeed::of(heap), i);
        self.append_text(v, extra, out)
    }

    /// Append `t[i]` for the value at `idx` (metamethods included) to `out`
    /// as `concat` writes it; see [`Stack::append_raw_index`]. The value is
    /// written before anything else runs, so it needs no slot.
    #[inline(always)]
    pub(crate) fn append_index(
        &mut self,
        idx: i32,
        i: i64,
        extra: usize,
        out: &mut Vec<u8>,
    ) -> StackResult<Result<(), &'static str>> {
        let obj = self.get(idx);
        let v = self.index(obj, RawValue::Int(i))?;
        self.append_text(v, extra, out)
    }

    /// Append the value at `idx` to `out` as `concat` writes it; see
    /// [`Stack::append_raw_index`].
    #[inline(always)]
    pub(crate) fn append_text_at(
        &mut self,
        idx: i32,
        extra: usize,
        out: &mut Vec<u8>,
    ) -> StackResult<Result<(), &'static str>> {
        let v = self.get(idx);
        self.append_text(v, extra, out)
    }

    #[inline(always)]
    fn append_text(
        &mut self,
        v: RawValue,
        extra: usize,
        out: &mut Vec<u8>,
    ) -> StackResult<Result<(), &'static str>> {
        // Long pieces grow the buffer by a lot: reserve first, so running
        // out of memory is an error rather than an abort.
        let piece = match v {
            RawValue::Str(s) => self.ctx.bytes(s).len(),
            RawValue::Int(_) | RawValue::Float(_) => 0,
            _ => return Ok(Err(v.type_name())),
        };
        if out.try_reserve(piece + extra).is_err() {
            return Err(self.memory_error());
        }
        self.ctx.display(v, out);
        Ok(Ok(()))
    }

    /// The type name of the value at `idx`, as operator errors give it
    /// (no `__name`).
    pub(crate) fn type_name_at(&self, idx: i32) -> &'static str {
        self.get(idx).type_name()
    }

    /// Push the metatable of the value at `idx`, if it has one.
    pub fn get_metatable(&mut self, idx: i32) -> bool {
        match self.ctx.metatable(self.get(idx)) {
            Some(mt) => {
                self.ctx.push(RawValue::Table(mt));
                true
            }
            None => false,
        }
    }

    /// `t[k] = v` for the value at `idx` and the key and value on top (both
    /// popped), metamethods included.
    pub fn set_table(&mut self, idx: i32) -> StackResult<()> {
        self.need(2)?;
        let obj = self.get(idx);
        let len = self.ctx.len();
        let (key, v) = (self.ctx.arg(len - 2), self.ctx.arg(len - 1));
        // Both stay rooted until the store is done.
        self.set_index(obj, key, v)?;
        self.ctx.truncate(len - 2);
        Ok(())
    }

    /// `obj[key] = top`, popping it.
    fn set_popping(&mut self, obj: RawValue, key: RawValue) -> StackResult<()> {
        self.need(1)?;
        let len = self.ctx.len();
        let v = self.ctx.arg(len - 1);
        self.ctx.push(key);
        self.set_index(obj, key, v)?;
        self.ctx.truncate(len - 1);
        Ok(())
    }

    /// `t[k] = top` (popped) for the value at `idx` and string key `k`.
    pub fn set_field<'k>(&mut self, idx: i32, k: impl Into<OrbitStr<'k>>) -> StackResult<()> {
        self.set_field_str(idx, k.into())
    }

    fn set_field_str(&mut self, idx: i32, k: OrbitStr<'_>) -> StackResult<()> {
        let k = k.as_bytes();
        let obj = self.get(idx);
        let key = self.ctx.new_str(k);
        self.set_popping(obj, key)
    }

    /// `t[n] = top` (popped) for the value at `idx`.
    #[inline]
    pub fn set_index_int(&mut self, idx: i32, n: i64) -> StackResult<()> {
        // An integer key needs no rooting while metamethods run; the value
        // stays on the window until the store is done.
        self.need(1)?;
        let obj = self.get(idx);
        let len = self.ctx.len();
        let v = self.ctx.arg(len - 1);
        self.set_index(obj, RawValue::Int(n), v)?;
        self.ctx.truncate(len - 1);
        Ok(())
    }

    /// `t[n] = v` for the value at `idx` and the value at `from`, which
    /// stays where it is: [`Stack::set_index_int`] without the push.
    #[inline]
    pub(crate) fn set_index_int_from(&mut self, idx: i32, n: i64, from: i32) -> StackResult<()> {
        let obj = self.get(idx);
        let v = self.get(from);
        self.set_index(obj, RawValue::Int(n), v)
    }

    /// Pop the top value into global `name`.
    pub fn set_global<'n>(&mut self, name: impl Into<OrbitStr<'n>>) -> StackResult<()> {
        self.set_global_str(name.into())
    }

    fn set_global_str(&mut self, name: OrbitStr<'_>) -> StackResult<()> {
        let name = name.as_bytes();
        let g = RawValue::Table(self.ctx.vm.rt.globals);
        let key = self.ctx.new_str(name);
        self.set_popping(g, key)
    }

    fn raw_set_with(&mut self, idx: i32, key: RawValue) -> StackResult<()> {
        let t = self.table_at(idx)?;
        self.need(1)?;
        let len = self.ctx.len();
        let v = self.ctx.arg(len - 1);
        self.ctx.push(key);
        let r = tbl::set(&mut self.ctx.vm.heap, t, key, v);
        self.ctx.truncate(len - 1);
        r.map_err(|e| self.store_error(e))
    }

    /// `t[k] = v` for the table at `idx` and the key and value on top (both
    /// popped), with no metamethods.
    pub fn raw_set(&mut self, idx: i32) -> StackResult<()> {
        let t = self.table_at(idx)?;
        self.need(2)?;
        let len = self.ctx.len();
        let (key, v) = (self.ctx.arg(len - 2), self.ctx.arg(len - 1));
        let r = tbl::set(&mut self.ctx.vm.heap, t, key, v);
        self.ctx.truncate(len - 2);
        r.map_err(|e| self.store_error(e))
    }

    pub fn raw_set_index(&mut self, idx: i32, n: i64) -> StackResult<()> {
        self.raw_set_with(idx, RawValue::Int(n))
    }

    /// `t[p] = top` (popped) for a light userdata key `p`, with no
    /// metamethods.
    pub fn raw_set_light(&mut self, idx: i32, p: usize) -> StackResult<()> {
        self.raw_set_with(idx, light(p))
    }

    /// `t[k] = top` (popped) for string key `k`, with no metamethods.
    pub fn raw_set_field<'k>(&mut self, idx: i32, k: impl Into<OrbitStr<'k>>) -> StackResult<()> {
        self.raw_set_field_str(idx, k.into())
    }

    fn raw_set_field_str(&mut self, idx: i32, k: OrbitStr<'_>) -> StackResult<()> {
        let k = k.as_bytes();
        let key = self.ctx.new_str(k);
        self.raw_set_with(idx, key)
    }

    /// Pop a table (or nil) and make it the metatable of the value at
    /// `idx`; for types without their own metatables, of every value of
    /// that type.
    pub fn set_metatable(&mut self, idx: i32) -> StackResult<()> {
        let target = self.get(idx);
        let mt = match self.pop_raw()? {
            RawValue::Table(t) => Some(t),
            RawValue::Nil => None,
            _ => return Err(self.error("table expected for metatable")),
        };
        if !gc::set_metatable(&mut self.ctx.vm.heap, target, mt) {
            let tag = target.tag();
            self.ctx.vm.rt.set_type_meta(tag, mt);
        }
        Ok(())
    }

    /// Pop a key and push the next key and value of the table at `idx`
    /// after it; false, pushing nothing, after the last.
    pub fn next(&mut self, idx: i32) -> StackResult<bool> {
        let t = self.table_at(idx)?;
        let k = self.pop_raw()?;
        let c = &mut self.ctx;
        match c.vm.heap.get(t).next(&c.vm.heap, k) {
            Ok(Some((k, v))) => {
                c.push(k);
                c.push(v);
                Ok(true)
            }
            Ok(None) => Ok(false),
            Err(_) => {
                let msg = c.new_str(b"invalid key to 'next'");
                Err(Raised::from(c.error_value(msg)))
            }
        }
    }

    // ---- calls ---------------------------------------------------------------------

    /// Check that there is a function below `nargs` arguments, and give
    /// `nargs` as a count.
    pub fn check_call(&mut self, nargs: i32) -> StackResult<usize> {
        let nargs = nargs.max(0) as usize;
        if nargs + 1 > self.ctx.len() {
            return Err(self.error("not enough elements for the call"));
        }
        Ok(nargs)
    }

    /// Keep `nresults` of the `n` results at the top, padding with nils
    /// (all of them for [`MULTRET`]).
    pub fn adjust_results(&mut self, n: usize, nresults: i32) {
        if nresults == MULTRET {
            return;
        }
        let want = nresults.max(0) as usize;
        let len = self.ctx.len();
        if n > want {
            self.ctx.truncate(len - (n - want));
        } else {
            for _ in n..want {
                self.ctx.push(RawValue::Nil);
            }
        }
    }

    /// Call the function below the top `nargs` values, replacing them with
    /// `nresults` results. Nothing can yield across the call.
    pub fn call(&mut self, nargs: i32, nresults: i32) -> StackResult<()> {
        let nargs = self.check_call(nargs)?;
        // A fixed count the callee's return can leave in place.
        match u8::try_from(nresults) {
            Ok(want) if want != crate::vm::op::MULTI => {
                self.ctx.call_want(nargs, want)?;
            }
            _ => {
                let n = self.ctx.call(nargs, None)?;
                self.adjust_results(n, nresults);
            }
        }
        Ok(())
    }

    /// [`Stack::call`] with a message handler at `handler`, which runs on
    /// an error with the error value, on top of the frames that raised it,
    /// and gives the error's value.
    pub fn call_handled(&mut self, nargs: i32, nresults: i32, handler: i32) -> StackResult<()> {
        let nargs = self.check_call(nargs)?;
        let Slot::Stack(h) = self.slot(handler) else {
            return Err(self.error("invalid message handler index"));
        };
        let n = self.ctx.call(nargs, Some(h))?;
        self.adjust_results(n, nresults);
        Ok(())
    }

    /// [`Stack::call`], catching an error: the error value replaces the
    /// function and arguments. `handler` is the stack index of a message
    /// handler, or 0 for none.
    pub fn pcall(&mut self, nargs: i32, nresults: i32, handler: i32) -> StackResult<Status> {
        let nargs = self.check_call(nargs)?;
        let handler = if handler == 0 {
            None
        } else {
            match self.slot(handler) {
                Slot::Stack(i) => Some(i),
                _ => return Err(self.error("invalid message handler index")),
            }
        };
        match self.ctx.call(nargs, handler) {
            Ok(n) => {
                self.adjust_results(n, nresults);
                Ok(Status::Ok)
            }
            // A coroutine closing itself is not caught.
            Err(e) if e.is_exit() => Err(e.into()),
            Err(e) => {
                let status = self.value_status(e.value);
                self.ctx.push(e.value);
                Ok(status)
            }
        }
    }

    fn value_status(&self, v: RawValue) -> Status {
        match self.bytes_of(v) {
            Some(b) if b == NoMemory::MESSAGE.as_bytes() => Status::Memory,
            Some(b"error in error handling") => Status::Handler,
            _ => Status::Runtime,
        }
    }

    /// The status a protected call would report for this error.
    pub fn status_of(&self, e: &Raised) -> Status {
        self.value_status(e.0.value)
    }

    /// Whether the host's interrupt raised this error (`interrupted!`),
    /// rather than a script raising that text.
    pub fn is_interrupt(&self, e: &Raised) -> bool {
        e.0.cause == Cause::Interrupt
    }

    /// Push an error's value.
    pub fn push_raised(&mut self, e: &Raised) {
        self.ctx.push(e.0.value);
    }

    /// Push an error's value, and give its message as the standalone
    /// interpreter prints it and the traceback it carries, if any.
    pub fn take_raised(&mut self, e: Raised) -> (String, Option<String>) {
        let message = self.ctx.vm.error_message(&e.0);
        self.ctx.push(e.0.value);
        (message, e.0.traceback.map(String::from))
    }

    /// Lua's `not enough memory` error, to return.
    pub fn memory_error(&mut self) -> Raised {
        Raised::from(self.ctx.no_memory())
    }

    /// Compile (or decode) `bytes` as a chunk named `name`, with the global
    /// table as its `_ENV`, and push its function, or the message.
    /// `mode` is `load`'s: `b"t"`, `b"b"` or `b"bt"`.
    pub fn load<'n, 'm>(
        &mut self,
        bytes: &[u8],
        name: impl Into<OrbitStr<'n>>,
        mode: impl Into<OrbitStr<'m>>,
    ) -> Status {
        self.load_str(bytes, name.into(), mode.into())
    }

    fn load_str(&mut self, bytes: &[u8], name: OrbitStr<'_>, mode: OrbitStr<'_>) -> Status {
        let name = name.as_bytes();
        let mode = mode.as_bytes();
        let env = RawValue::Table(self.ctx.vm.rt.globals);
        match stdlib::load_chunk_in(&mut self.ctx, bytes, name, mode, env) {
            Ok(f) => {
                self.ctx.push(f);
                Status::Ok
            }
            Err(msg) => {
                let status = if msg.contains(NoMemory::MESSAGE) {
                    Status::Memory
                } else {
                    Status::Syntax
                };
                self.push_str_value(msg.as_bytes());
                status
            }
        }
    }

    /// Load a chunk with `load`, which compiles or decodes it and links it
    /// with the `_ENV` it is given: the value at `env` (0: the global
    /// table). Push its function, or push the message and give
    /// [`Status::Syntax`] (or [`Status::Memory`]). `orbit`'s
    /// `StackExt::load_chunk` passes its chunk loader here.
    #[doc(hidden)]
    pub fn load_with(
        &mut self,
        env: i32,
        load: impl FnOnce(&mut NativeCtx<'s>, RawValue) -> Result<RawValue, String>,
    ) -> Status {
        let env = if env == 0 {
            RawValue::Table(self.ctx.vm.rt.globals)
        } else {
            self.get(env)
        };
        match load(&mut self.ctx, env) {
            Ok(f) => {
                self.ctx.push(f);
                Status::Ok
            }
            Err(msg) => {
                let status = if msg.contains(NoMemory::MESSAGE) {
                    Status::Memory
                } else {
                    Status::Syntax
                };
                self.push_str_value(msg.as_bytes());
                status
            }
        }
    }

    /// The runtime's context under the window, for the `orbit` crate's
    /// own operations.
    #[doc(hidden)]
    pub fn native_ctx(&mut self) -> &mut NativeCtx<'s> {
        &mut self.ctx
    }

    /// Push the main function of a chunk `orbit::Compiler`
    /// compiled, linked here with the global table as its `_ENV`.
    pub fn push_compiled(&mut self, chunk: &orbit_bytecode::UnlinkedChunk) {
        let env = RawValue::Table(self.ctx.vm.rt.globals);
        let f = self.ctx.link(chunk, env);
        self.ctx.push(f);
    }

    // ---- errors --------------------------------------------------------------------

    /// An error with the top value as its value (nil if the window is
    /// empty), to return.
    pub fn error_from_top(&mut self) -> Raised {
        let v = self.top_value();
        Raised::from(self.ctx.error_value(v))
    }

    /// A runtime error with `message`, positioned at the caller as Lua's
    /// `error` does at level 1.
    pub fn error<'m>(&mut self, message: impl Into<OrbitStr<'m>>) -> Raised {
        Raised::from(self.ctx.error(message))
    }

    /// Lua's argument error for argument `arg` (1-based): `bad argument #1
    /// to 'f' (message)`.
    pub fn arg_error<'m>(&mut self, arg: i32, message: impl Into<OrbitStr<'m>>) -> Raised {
        Raised::from(self.ctx.arg_error((arg.max(1) - 1) as usize, message))
    }

    /// Lua's type error for argument `arg` (1-based): `bad argument #1 to
    /// 'f' (number expected, got nil)`.
    pub fn type_error(&mut self, arg: i32, expected: &str) -> Raised {
        Raised::from(self.ctx.type_error((arg.max(1) - 1) as usize, expected))
    }

    /// Argument `arg` (1-based) as a string (numbers convert), or Lua's
    /// argument error.
    pub fn check_string(&mut self, arg: i32) -> StackResult<OrbitStr<'_>> {
        let s = self.ctx.check_str((arg.max(1) - 1) as usize)?;
        Ok(self.ctx.bytes(s).into())
    }

    /// Argument `arg` (1-based) as an integer (floats with an exact integer
    /// value and numeric strings convert), or Lua's argument error.
    #[inline(always)]
    pub fn check_integer(&mut self, arg: i32) -> StackResult<i64> {
        match self.get(arg) {
            RawValue::Int(n) => Ok(n),
            _ => self.check_integer_slow(arg),
        }
    }

    #[inline(never)]
    fn check_integer_slow(&mut self, arg: i32) -> StackResult<i64> {
        Ok(self.ctx.check_int((arg.max(1) - 1) as usize)?)
    }

    /// Like [`Stack::check_integer`], but `default` for an absent or nil
    /// argument.
    pub fn opt_integer(&mut self, arg: i32, default: i64) -> StackResult<i64> {
        Ok(self.ctx.opt_int((arg.max(1) - 1) as usize, default)?)
    }

    /// Argument `arg` (1-based) as a float (numeric strings convert), or
    /// Lua's argument error.
    #[inline(always)]
    pub fn check_number(&mut self, arg: i32) -> StackResult<f64> {
        match self.get(arg) {
            RawValue::Float(x) => Ok(x),
            _ => self.check_number_slow(arg),
        }
    }

    #[inline(never)]
    fn check_number_slow(&mut self, arg: i32) -> StackResult<f64> {
        Ok(self.ctx.check_f64((arg.max(1) - 1) as usize)?)
    }

    /// The value at `idx` as `tostring` shows it without metamethods.
    pub fn display(&self, idx: i32) -> OrbitString {
        let mut out = Vec::new();
        self.ctx.display(self.get(idx), &mut out);
        out.into()
    }

    /// Push `chunk:line:` for the function at `level` (1 is the caller),
    /// or an empty string where that isn't known.
    pub fn push_where(&mut self, level: i32) {
        let w = self.ctx.where_(level.max(0) as usize);
        self.push_str_value(w.as_bytes());
    }

    /// Push `message` (if any), a newline, and the traceback from `level`
    /// on, as one string (`luaL_traceback` on the running thread).
    pub fn push_traceback(&mut self, message: Option<OrbitStr<'_>>, level: i32) {
        let mut out = Vec::new();
        if let Some(m) = message {
            out.extend_from_slice(&m);
            out.push(b'\n');
        }
        out.extend_from_slice(self.ctx.traceback(level.max(0) as usize).as_bytes());
        self.push_str_value(&out);
    }

    /// [`push_traceback`](Self::push_traceback) for the thread at index
    /// `thread`, whose levels start at its top activation when it is not
    /// the running one (`luaL_traceback` of another thread).
    pub fn push_traceback_of(
        &mut self,
        thread: i32,
        message: Option<OrbitStr<'_>>,
        level: i32,
    ) -> StackResult<()> {
        let RawValue::Thread(t) = self.get(thread) else {
            return Err(self.error("thread expected"));
        };
        let mut out = Vec::new();
        if let Some(m) = message {
            out.extend_from_slice(&m);
            out.push(b'\n');
        }
        let level = level.max(0) as usize;
        let trace = self
            .ctx
            .on_thread(t, |ctx, foreign| ctx.traceback_in(foreign, level));
        out.extend_from_slice(trace.as_bytes());
        self.push_str_value(&out);
        Ok(())
    }

    /// Push the value at `idx` as `tostring` converts it (`__tostring`,
    /// then `__name`), and give the string's bytes.
    pub fn to_display_string(&mut self, idx: i32) -> StackResult<OrbitStr<'_>> {
        let v = self.get(idx);
        let mm = self.ctx.metamethod(v, MetaField::ToString);
        if !mm.is_nil() {
            self.ctx.push(mm);
            self.ctx.push(v);
            let n = self.ctx.call(1, None)?;
            self.adjust_results(n, 1);
            // A number will do, as for Lua's `luaL_tolstring`.
            if !self.is_string(-1) {
                return Err(self.error("'__tostring' must return a string"));
            }
            return Ok(self.to_str(-1)?.expect("a string or a number"));
        }
        let mut out = Vec::new();
        self.ctx.display(v, &mut out);
        Ok(self.push_bytes(&out))
    }

    // ---- the isolate -------------------------------------------------------------------

    /// The collector, as `lua_gc` drives it. Inside a finalizer every
    /// option does nothing and gives `None`, as in Lua.
    pub fn gc(&mut self, op: GcOp) -> Option<i64> {
        use crate::vm::{GcMode, GcParam as P};
        let c = &mut self.ctx;
        if c.vm.rt.gc.finalizing() {
            return None;
        }
        Some(match op {
            GcOp::Collect => {
                c.full_collect();
                c.run_finalizers_now();
                0
            }
            GcOp::Step(kb) => match c.vm.rt.gc.mode() {
                GcMode::Generational => c.gen_step() as i64,
                GcMode::Incremental => {
                    let bytes = usize::try_from(kb).unwrap_or(0).saturating_mul(1024);
                    c.vm.heap.step_with(&mut c.vm.rt, bytes) as i64
                }
            },
            GcOp::Count => (c.vm.heap.bytes() / 1024) as i64,
            GcOp::CountRemainder => (c.vm.heap.bytes() % 1024) as i64,
            GcOp::Stop => {
                c.vm.heap.stop();
                0
            }
            GcOp::Restart => {
                c.vm.heap.restart();
                0
            }
            GcOp::IsRunning => !c.vm.heap.is_stopped() as i64,
            GcOp::Param(p, set) => {
                let p = match p {
                    GcParam::MinorMul => P::MinorMul,
                    GcParam::MajorMinor => P::MajorMinor,
                    GcParam::MinorMajor => P::MinorMajor,
                    GcParam::Pause => P::Pause,
                    GcParam::StepMul => P::StepMul,
                    GcParam::StepSize => P::StepSize,
                };
                match set {
                    Some(v) => c.vm.rt.gc.set_param(&mut c.vm.heap, p, v),
                    None => c.vm.rt.gc.param(p),
                }
            }
        })
    }

    /// Switch the collector to `mode` and give the previous one; `None`,
    /// changing nothing, inside a finalizer.
    pub fn set_gc_mode(&mut self, mode: GcMode) -> Option<GcMode> {
        let vm = &mut *self.ctx.vm;
        if vm.rt.gc.finalizing() {
            return None;
        }
        Some(match vm.set_gc_mode(mode.raw()) {
            vm::GcMode::Generational => GcMode::Generational,
            vm::GcMode::Incremental => GcMode::Incremental,
        })
    }

    /// Emit a warning piece (`lua_warning`): pieces join until one with
    /// `more` false. A one-piece `@on` or `@off` switches warnings.
    pub fn warn<'m>(&mut self, msg: impl Into<OrbitStr<'m>>, more: bool) {
        self.warn_str(msg.into(), more)
    }

    fn warn_str(&mut self, msg: OrbitStr<'_>, more: bool) {
        let msg = msg.as_bytes();
        let host = &mut self.ctx.vm.rt.host;
        if let Some(hook) = host.warn_hook() {
            (hook.0)(msg, more);
            return;
        }
        let pending = host.ext_mut().get_or_insert_with(WarnBuffer::default);
        let first = !pending.continued;
        pending.text.extend_from_slice(msg);
        pending.continued = more;
        if more {
            return;
        }
        let text = std::mem::take(&mut pending.text);
        // A one-piece message starting with '@' is a control message.
        match (first, text.as_slice()) {
            (true, b"@on") => host.set_warnings(true),
            (true, b"@off") => host.set_warnings(false),
            (true, [b'@', ..]) => {}
            _ => host.warn(&text),
        }
    }

    /// Send warning pieces to `hook` (the piece, and whether more follow)
    /// instead of showing them (`lua_setwarnf`).
    pub fn set_warn_hook(&mut self, hook: impl FnMut(OrbitStr<'_>, bool) + 'static) {
        let mut hook = hook;
        let hook = move |msg: &[u8], more| hook(msg.into(), more);
        self.ctx
            .vm
            .rt
            .host
            .set_warn_hook(vm::WarnHook(Box::new(hook)));
    }

    /// Open standard library `lib` as `orbit::Config::libraries` would
    /// (setting its global), and push its table; false, pushing nothing, if
    /// this build leaves it out.
    pub fn open_library(&mut self, lib: Library) -> bool {
        let top = self.ctx.len();
        let opened = stdlib::open_one(self.ctx.vm, lib.lib());
        // Opening a library can leave the VM's top above the window's;
        // what lies there is no value of ours.
        self.ctx.truncate(top);
        match opened {
            Some(t) => {
                self.ctx.push(t);
                true
            }
            None => false,
        }
    }

    /// Keep the value at `idx` alive in a pin slot of its own until
    /// [`Stack::unpin`], and give the slot: for values a host keeps
    /// between calls.
    pub fn pin(&mut self, idx: i32) -> u32 {
        let v = self.get(idx);
        self.ctx.vm.pin_slot(v)
    }

    /// Push the value in pin slot `slot`.
    pub fn push_pinned(&mut self, slot: u32) {
        let v = self.ctx.vm.pin_slot_value(slot);
        self.ctx.push(v);
    }

    /// Free pin slot `slot`.
    pub fn unpin(&mut self, slot: u32) {
        self.ctx.vm.free_pin_slot(slot);
    }

    /// How the isolate's text streams treat line endings
    /// (`orbit::Config::line_endings`). Standard output
    /// ([`Stack::with_stdout`]) already does what this says.
    pub fn line_endings(&self) -> crate::text::LineEndings {
        self.ctx.vm.rt.host.line_endings()
    }

    /// Write to the isolate's standard output, the stream `print` and
    /// `io.stdout` share. `f` must not reach the isolate.
    pub fn with_stdout<R>(&mut self, f: impl FnOnce(&mut dyn std::io::Write) -> R) -> R {
        f(self.ctx.vm.rt.host.out().as_write())
    }

    /// Run `hook` when a script ends the process ([`Return::exit`],
    /// `os.exit`), after the isolate closes (if it does) and before
    /// standard output is flushed.
    pub fn before_exit(&mut self, hook: impl Fn() + 'static) {
        super::exit::exit_hooks(self.ctx.vm).0.push(Box::new(hook));
    }

    /// Let the collector run if it is due, as between statements: for a
    /// host that makes many calls from outside any native.
    pub fn safepoint(&mut self) {
        self.ctx.vm.host_safepoint();
    }

    /// Run `f` on the isolate's extension value of type `T`, made with
    /// `Default` on first use: per-isolate state for a binding. `f` can't
    /// reach the stack.
    #[inline]
    pub fn extension<T: Any + Default, R>(&mut self, f: impl FnOnce(&mut T) -> R) -> R {
        f(self.ctx.vm.rt.host.ext_mut().get_or_insert_with(T::default))
    }

    /// Run `f` on the isolate's extension value of type `T` if one was
    /// made, without making one: the cheaper lookup for a native's hot
    /// path.
    #[inline]
    pub fn find_extension<T: Any, R>(&self, f: impl FnOnce(&T) -> R) -> Option<R> {
        self.ctx.vm.rt.host.ext().get::<T>().map(f)
    }

    // ---- the locale (ADR-13) ----------------------------------------------

    /// `os.setlocale(name, category, scope)`: switch `category` to locale
    /// `name`, or with no name, ask. The locale's name, or `None` if it
    /// isn't available.
    pub fn set_locale(
        &mut self,
        category: Category,
        name: Option<OrbitStr<'_>>,
        scope: LocaleScope,
    ) -> Option<OrbitString> {
        let name = name.as_ref().map(|n| n.as_bytes());
        self.ctx
            .vm
            .set_locale(category, name, scope)
            .map(OrbitString::from)
    }

    /// The decimal point numbers are written and read with: `.` in the C
    /// locale.
    pub fn decimal_point(&self) -> OrbitString {
        self.ctx.locale().decimal_point().into()
    }

    /// Whether `os.setlocale` changes this isolate's locale or the
    /// process's.
    pub fn locale_mode(&self) -> LocaleMode {
        self.ctx.vm.locale_mode()
    }

    /// See `orbit::Isolate::set_locale_mode`.
    pub fn set_locale_mode(&mut self, mode: LocaleMode) {
        self.ctx.vm.set_locale_mode(mode)
    }

    /// The text of a locale-dependent `os.date` conversion (`spec` without
    /// its `%`) in the time category's locale, or `None` for the C
    /// locale's.
    pub fn locale_time_text(&self, spec: &[u8], tm: &TimeParts) -> Option<OrbitString> {
        self.ctx
            .vm
            .locale_time_text(spec, tm)
            .map(OrbitString::from)
    }

    /// What reaches the isolate itself: hooks, the interrupt handler.
    pub fn control(&mut self) -> Control<'_> {
        Control {
            vm: &mut *self.ctx.vm,
        }
    }

    // ---- threads ---------------------------------------------------------------------

    /// The thread this window belongs to.
    pub fn running_thread(&self) -> ThreadRef {
        ThreadRef(self.ctx.running().0)
    }

    pub fn main_thread(&self) -> ThreadRef {
        ThreadRef(self.ctx.running_main_thread())
    }

    pub fn push_thread(&mut self, t: ThreadRef) {
        self.ctx.push(RawValue::Thread(t.0));
    }

    /// Push a thread with an empty stack, as `lua_newthread` makes: it
    /// reads as dead until a function is put on its stack and it is
    /// resumed.
    pub fn push_new_thread(&mut self) -> ThreadRef {
        let t = self.ctx.new_empty_thread();
        self.ctx.push(RawValue::Thread(t));
        ThreadRef(t)
    }

    pub fn to_thread(&self, idx: i32) -> Option<ThreadRef> {
        match self.get(idx) {
            RawValue::Thread(t) => Some(ThreadRef(t)),
            _ => None,
        }
    }

    /// Push a new coroutine that runs the function at `idx`
    /// (`coroutine.create`).
    pub(crate) fn push_new_coroutine(&mut self, idx: i32) -> ThreadRef {
        let f = self.get(idx);
        let t = self.ctx.new_thread(f);
        self.ctx.push(RawValue::Thread(t));
        ThreadRef(t)
    }

    /// Whether `t` is still alive (not collected).
    pub fn thread_is_live(&self, t: ThreadRef) -> bool {
        self.ctx.vm.heap.is_live(t.0)
    }

    pub fn thread_status(&self, t: ThreadRef) -> ThreadStatus {
        match self.ctx.thread_status(t.0) {
            CoStatus::Suspended => ThreadStatus::Suspended,
            CoStatus::Running => ThreadStatus::Running,
            CoStatus::Normal => ThreadStatus::Normal,
            CoStatus::Dead => ThreadStatus::Dead,
        }
    }

    /// Whether `t` is suspended in a yield (not one that hasn't started).
    pub fn thread_yielded(&self, t: ThreadRef) -> bool {
        self.ctx.thread_yielded(t.0)
    }

    /// Whether `t` died with an error no close has cleared.
    pub fn thread_failed(&self, t: ThreadRef) -> bool {
        self.ctx.thread_failed(t.0)
    }

    /// Whether `t` could yield where it is.
    pub fn thread_yieldable(&self, t: ThreadRef) -> bool {
        self.ctx.thread_yieldable(t.0)
    }

    /// Make `t`'s stack this window, from `base` (by default where its
    /// view for the C API starts) to its top, until [`Stack::leave_thread`].
    /// Pushing and popping work there; running code does not.
    pub fn enter_thread(&mut self, t: ThreadRef, base: Option<usize>) -> ThreadWindow {
        ThreadWindow(self.ctx.enter_thread_window(t.0, base))
    }

    /// Back to this window's own stack after [`Stack::enter_thread`].
    pub fn leave_thread(&mut self, t: ThreadRef, w: ThreadWindow) {
        self.ctx.leave_thread_window(t.0, w.0);
    }

    /// Move the top `n` values of thread `from`'s stack to `to`'s (each
    /// viewed from its `base`, as [`Stack::enter_thread`]).
    pub fn move_values(
        &mut self,
        from: (ThreadRef, Option<usize>),
        to: (ThreadRef, Option<usize>),
        n: usize,
    ) {
        let vals = self.on_thread(from.0, from.1, |s| {
            let len = s.ctx.len();
            let n = n.min(len);
            let vals: Vec<RawValue> = (len - n..len).map(|i| s.ctx.arg(i)).collect();
            s.ctx.truncate(len - n);
            vals
        });
        // Unrooted between the two windows; nothing allocates there.
        self.on_thread(to.0, to.1, |s| vals.iter().for_each(|&v| s.ctx.push(v)));
    }

    /// Resume `t` with the top `nargs` values of its stack (viewed from
    /// `base`), running it until it yields, returns or fails, as
    /// `lua_resume` does; what it yields or returns, or its error, goes on
    /// its stack. Nothing can yield across this call.
    pub fn resume(
        &mut self,
        t: ThreadRef,
        base: Option<usize>,
        nargs: usize,
    ) -> StackResult<Resumed> {
        let args = self.on_thread(t, base, |s| {
            let len = s.ctx.len();
            if nargs > len {
                return None;
            }
            let args: Vec<RawValue> = (len - nargs..len).map(|i| s.ctx.arg(i)).collect();
            s.ctx.truncate(len - nargs);
            Some(args)
        });
        let Some(args) = args else {
            return Err(self.error("not enough values to resume with"));
        };
        let (out, vals) = match self.ctx.resume_nested(t.0, &args) {
            Ok((done, vals)) => (
                Resumed {
                    status: if done { Status::Ok } else { Status::Yield },
                    nresults: vals.len(),
                    failed_top: None,
                },
                vals,
            ),
            Err(v) => {
                let status = self.value_status(v);
                let failed_top = self
                    .ctx
                    .thread_failed(t.0)
                    .then(|| self.ctx.thread_top(t.0));
                (
                    Resumed {
                        status,
                        nresults: 0,
                        failed_top,
                    },
                    vec![v],
                )
            }
        };
        // A thread that ran is viewed from its default base again; one
        // that died, from its top as it died.
        let base = match out.status {
            Status::Ok | Status::Yield => None,
            _ => out.failed_top.or(base),
        };
        self.on_thread(t, base, |s| vals.iter().for_each(|&v| s.ctx.push(v)));
        Ok(out)
    }

    /// Push a coroutine that runs the function at `f` when first resumed.
    pub fn push_coroutine(&mut self, f: i32) {
        let f = self.get(f);
        let t = self.ctx.new_thread(f);
        self.ctx.push(RawValue::Thread(t));
    }

    /// Resume the coroutine at `idx` with the window's top `nargs` values
    /// (popped) until it yields, returns or fails, and push what it yielded
    /// or returned: whether it returned, and how many values. Nothing can
    /// yield across this call.
    pub fn resume_here(&mut self, idx: i32, nargs: usize) -> StackResult<(bool, usize)> {
        let RawValue::Thread(t) = self.get(idx) else {
            return Err(self.error("thread expected"));
        };
        self.need(nargs)?;
        let at = self.ctx.len() - nargs;
        let args: Vec<RawValue> = (at..at + nargs).map(|i| self.ctx.arg(i)).collect();
        let r = self.ctx.resume_nested(t, &args);
        self.ctx.truncate(at);
        match r {
            Ok((returned, vals)) => {
                vals.iter().for_each(|&v| self.ctx.push(v));
                Ok((returned, vals.len()))
            }
            Err(v) => {
                let cause = self.ctx.thread_error_cause(t, v);
                Err(Raised::from(LuaError::with_cause(v, cause)))
            }
        }
    }

    /// Close `t` (suspended or dead), as `lua_closethread` does: its
    /// pending variables close, and it is left empty for reuse. After an
    /// error, the error goes on its stack.
    pub fn close_thread(&mut self, t: ThreadRef) -> Status {
        match self.ctx.close_nested(t.0) {
            Ok(()) => Status::Ok,
            Err(v) => {
                let status = self.value_status(v);
                self.on_thread(t, None, |s| s.ctx.push(v));
                status
            }
        }
    }

    // ---- debug interface -----------------------------------------------------------------

    /// Whether there is an activation at `level` (0 is the running native).
    pub fn has_level(&self, level: i32) -> bool {
        level >= 0 && self.ctx.level(level as usize).is_some()
    }

    fn info_of(&self, func: RawValue, at: Option<vm::debug::Level>) -> FrameInfo {
        let c = &self.ctx;
        let fi = c.func_info(func);
        let mut out = FrameInfo {
            what: fi.what,
            source: fi.source.into(),
            short_src: fi.short_src,
            linedefined: fi.line_defined as i32,
            lastlinedefined: fi.last_line as i32,
            nups: fi.nups.min(255) as u8,
            nparams: fi.nparams,
            isvararg: fi.is_vararg,
            currentline: -1,
            namewhat: "",
            defined_name: fi.defined_name.map(OrbitString::from),
            ..FrameInfo::default()
        };
        if let Some(l) = at {
            out.currentline = c.current_line(l) as i32;
            out.istailcall = c.is_tail_call(l);
            out.extraargs = c.extra_args(l);
            if let Some(n) = c.level_name(l) {
                out.namewhat = n.namewhat;
                out.name = Some(n.name.into());
            }
        }
        out
    }

    /// What the activation at `level` runs, and where.
    pub fn level_info(&self, level: usize) -> Option<FrameInfo> {
        let l = self.ctx.level(level)?;
        Some(self.info_of(self.ctx.level_func(l), Some(l)))
    }

    /// What the function at `idx` is.
    pub fn function_info(&self, idx: i32) -> FrameInfo {
        self.info_of(self.get(idx), None)
    }

    /// Push the function at activation `level` (nil if there is none).
    pub fn push_level_function(&mut self, level: usize) {
        let f = self
            .ctx
            .level(level)
            .map_or(RawValue::Nil, |l| self.ctx.level_func(l));
        self.ctx.push(f);
    }

    /// Local `n` of activation `level`: its name, its value pushed; `None`,
    /// pushing nothing, if there is no such local.
    pub fn get_local(&mut self, level: usize, n: i32) -> Option<OrbitString> {
        let l = self.ctx.level(level)?;
        let (name, v) = self.ctx.get_local(l, n.into())?;
        self.ctx.push(v);
        Some(name.into())
    }

    /// The name of parameter `n` of the script function on top, pushing
    /// nothing.
    pub fn param_name(&mut self, n: i32) -> Option<OrbitString> {
        let f = self.ctx.arg(self.ctx.len().checked_sub(1)?);
        self.ctx.param_name(f, n.into()).map(OrbitString::from)
    }

    /// Pop the top value into local `n` of activation `level` and give its
    /// name; with no such local, pop nothing.
    pub fn set_local(&mut self, level: usize, n: i32) -> StackResult<Option<OrbitString>> {
        self.need(1)?;
        let len = self.ctx.len();
        let v = self.ctx.arg(len - 1);
        let Some(l) = self.ctx.level(level) else {
            return Ok(None);
        };
        let name = self.ctx.set_local(l, n.into(), v);
        if name.is_some() {
            self.ctx.truncate(len - 1);
        }
        Ok(name.map(OrbitString::from))
    }

    /// Upvalue `n` (1-based) of the function at `func`: its name, its value
    /// pushed.
    pub fn get_upvalue(&mut self, func: i32, n: i32) -> Option<OrbitString> {
        let f = self.get(func);
        let (name, v) = self.ctx.upvalue_of(f, n as i64)?;
        self.ctx.push(v);
        Some(name.into())
    }

    /// Pop the top value into upvalue `n` of the function at `func` and
    /// give its name; with no upvalue `n`, pop nothing and give `None`.
    pub fn set_upvalue(&mut self, func: i32, n: i32) -> StackResult<Option<OrbitString>> {
        self.need(1)?;
        let f = self.get(func);
        let len = self.ctx.len();
        let v = self.ctx.arg(len - 1);
        let r = self.ctx.set_upvalue_of(f, n as i64, v);
        // As `lua_setupvalue`: the value is popped only when it was set.
        if r.is_some() {
            self.ctx.truncate(len - 1);
        }
        Ok(r.map(OrbitString::from))
    }

    /// An identity for upvalue `n` of the function at `func` (shared
    /// upvalues have the same one), or 0.
    pub fn upvalue_id(&self, func: i32, n: i32) -> usize {
        match self.ctx.upvalue_cell(self.get(func), n as i64) {
            Some(cell) => cell.to_bits() as usize | 1 << 63,
            None => 0,
        }
    }

    /// Make upvalue `n1` of the script function at `f1` share upvalue `n2`
    /// of the one at `f2`.
    pub fn join_upvalues(&mut self, f1: i32, n1: i32, f2: i32, n2: i32) -> StackResult<()> {
        let (a, b) = (self.get(f1), self.get(f2));
        let (RawValue::Closure(x), RawValue::Closure(y)) = (a, b) else {
            return Err(self.error("Lua function expected"));
        };
        if self.ctx.upvalue_cell(a, n1 as i64).is_none()
            || self.ctx.upvalue_cell(b, n2 as i64).is_none()
        {
            return Err(self.error("invalid upvalue index"));
        }
        self.ctx.join_upvalues(x, n1 as usize, y, n2 as usize);
        Ok(())
    }
}

fn light(p: usize) -> RawValue {
    RawValue::LightUserData(LightUserData::new(p as u64))
}

/// Reaches the isolate itself rather than a window: from
/// [`Stack::control`], `orbit::Isolate::control`, or an
/// [`InterruptHandler`].
pub struct Control<'a> {
    pub(crate) vm: &'a mut Vm,
}

impl std::fmt::Debug for Control<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Control").finish_non_exhaustive()
    }
}

fn interrupt_entry<H: InterruptHandler>(vm: &mut Vm) -> bool {
    H::handle(&mut Control { vm })
}

impl<'a> Control<'a> {
    #[doc(hidden)]
    pub fn new(vm: &'a mut Vm) -> Control<'a> {
        Control { vm }
    }

    /// See [`Stack::extension`].
    pub fn extension<T: Any + Default, R>(&mut self, f: impl FnOnce(&mut T) -> R) -> R {
        f(self.vm.rt.host.ext_mut().get_or_insert_with(T::default))
    }

    /// The interrupt flag, for code that can only store to memory (a C
    /// signal handler): storing a nonzero byte interrupts running code, as
    /// `orbit::Interrupt::request` does. Valid as long as the
    /// isolate.
    pub fn interrupt_flag(&self) -> *mut bool {
        self.vm.interrupt_flag()
    }

    /// Lower the interrupt flag, from a handler that dealt with it.
    pub fn clear_interrupt(&mut self) {
        self.vm.clear_interrupt();
    }

    /// Call `H` when the interrupt flag is raised.
    pub fn set_interrupt_handler<H: InterruptHandler>(&mut self) {
        self.vm.rt.host.set_interrupt_handler(interrupt_entry::<H>);
    }

    /// Register another native `N` that reads `data` on every call
    /// ([`Stack::native_data`]). Each call adds one, which lives as long as
    /// the isolate, so callers keep one per distinct `data`.
    pub fn register_native<N: Native>(&mut self, data: [u64; 2]) -> NativeRef {
        NativeRef(RawValue::Native(NativeId(self.register_native_fn(
            N::NAME,
            native_entry::<N>,
            data,
        ))))
    }

    /// [`Control::register_native`] for the native `f`, as `name`: its
    /// index.
    pub(crate) fn register_native_fn(
        &mut self,
        name: &'static str,
        f: vm::NativeFn,
        data: [u64; 2],
    ) -> u32 {
        let RawValue::Native(id) = self.vm.register_native_with_data(name, f, data) else {
            unreachable!("natives are registered as natives")
        };
        id.0
    }

    /// Set the running thread's hook, as `debug.sethook` does: `f` is
    /// called as a script hook is, `(event, line)`; `mask` has bits for
    /// call (1), return (2), line (4) and count (8) events, `count` is the
    /// count interval. `None`, or no events, turns it off.
    pub fn set_hook(&mut self, f: Option<NativeRef>, mask: u8, count: i64) {
        let f = f.map_or(RawValue::Nil, |f| f.0);
        self.vm.set_hook(f, mask & 0xf, count);
    }

    /// Store the main thread in registry entry [`RIDX_MAINTHREAD`], where
    /// C modules look for it.
    pub fn publish_main_thread(&mut self) {
        let (reg, main) = (self.vm.rt.registry, self.vm.main_thread());
        self.vm
            .table_set(reg, RawValue::Int(RIDX_MAINTHREAD), RawValue::Thread(main));
    }

    pub fn main_thread(&self) -> ThreadRef {
        ThreadRef(self.vm.main_thread())
    }

    /// `orbit::Isolate::set_breakpoints`, also from inside a stop.
    #[cfg(feature = "debugger")]
    pub fn set_breakpoints(&mut self, source: &[u8], lines: &[u32]) -> Vec<super::Breakpoint> {
        self.vm
            .set_breakpoints(source, lines)
            .into_iter()
            .map(|b| super::Breakpoint {
                requested: b.requested,
                line: b.line,
            })
            .collect()
    }

    /// `orbit::Isolate::stop_on_next_line`.
    #[cfg(feature = "debugger")]
    pub fn stop_on_next_line(&mut self) {
        self.vm.stop_on_next_line()
    }

    /// `orbit::Isolate::set_error_break`, also from inside a stop.
    #[cfg(feature = "debugger")]
    pub fn set_error_break(&mut self, on: super::ErrorBreak) {
        use crate::vm::debugger as engine;
        self.vm.set_error_break(match on {
            super::ErrorBreak::Never => engine::ErrorBreak::Never,
            super::ErrorBreak::Uncaught => engine::ErrorBreak::Uncaught,
            super::ErrorBreak::All => engine::ErrorBreak::All,
        })
    }
}
