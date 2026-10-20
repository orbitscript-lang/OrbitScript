//! The stack API over the runtime linked into this program.

use std::any::Any;
use std::ffi::c_void;

use orbit_runtime::object::number::{float_to_int, fmt_number, str_to_number};
use orbit_runtime::object::table as tbl;
use orbit_runtime::object::value::raw_eq;
use orbit_runtime::object::{
    self as raw, Gc, LightUserData, LuaStr, NativeId, NoMemory, Table, gc,
};
use orbit_runtime::stdlib;
use orbit_runtime::vm::{
    self, CallOutcome, CoStatus, LuaError, Meta, MetaField, NativeCtx, NativeReturn, Operator, Vm,
};

use crate::isolate::Library;
use crate::scope::Scope;
use crate::value::{ThreadStatus, Value};

use super::{
    ArithOp, CompareOp, Continuation, FrameInfo, GcOp, GcParam, InterruptHandler, MULTRET, Native,
    Outcome, REGISTRY_INDEX, RIDX_MAINTHREAD, Resumed, Ret, Return, StackResult, Status, Type,
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
pub struct Raised(pub(crate) LuaError);

impl From<LuaError> for Raised {
    fn from(e: LuaError) -> Raised {
        Raised(e)
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
        Err(Raised(e)) => Err(e),
    }
}

/// The runtime native behind every [`Native`] `N`.
fn native_entry<N: Native>(ctx: &mut NativeCtx<'_>) -> Result<NativeReturn, LuaError> {
    let mut s = Stack {
        ctx: ctx.reborrow(),
    };
    match N::call(&mut s) {
        Ok(r) => s.finish(r),
        Err(Raised(e)) => Err(e),
    }
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
        CallOutcome::Err(v) => Outcome::Err(Raised(s.ctx.error_value(v))),
    };
    match K::resume(&mut s, o) {
        Ok(r) => s.finish(r),
        Err(Raised(e)) => Err(e),
    }
}

/// A thread (coroutine), for keeping between calls. It does not keep the
/// thread alive: check [`Stack::thread_is_live`] before using one that may
/// have been collected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ThreadRef(Gc<raw::Thread>);

impl ThreadRef {
    /// The thread's heap handle as plain bits, and back.
    #[cfg(feature = "abi")]
    pub(crate) fn to_bits(self) -> u64 {
        self.0.to_bits()
    }

    #[cfg(feature = "abi")]
    pub(crate) fn from_bits(bits: u64) -> ThreadRef {
        ThreadRef(Gc::from_bits(bits))
    }

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

#[cfg(feature = "abi")]
impl NativeRef {
    /// The native with index `id`.
    pub(crate) fn from_index(id: u32) -> NativeRef {
        NativeRef(RawValue::Native(NativeId(id)))
    }
}

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
struct WarnBuffer(Vec<u8>);

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
    pub(crate) fn new(ctx: NativeCtx<'s>) -> Stack<'s> {
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
                }
            }
            Ret::Exit { code, close } => {
                return crate::isolate::backend::exit_process(c, code, close);
            }
        })
    }

    // ---- indices -----------------------------------------------------------

    #[inline]
    fn slot(&self, idx: i32) -> Slot {
        let c = &self.ctx;
        let len = c.len() as i64;
        if idx > REGISTRY_INDEX && idx <= 0 {
            // Negative stack index (0 is invalid).
            let at = len + idx as i64;
            if idx < 0 && at >= 0 {
                Slot::Stack(at as usize)
            } else {
                Slot::None
            }
        } else if idx == REGISTRY_INDEX {
            Slot::Registry
        } else if idx < REGISTRY_INDEX {
            // upvalue_index(i), 1-based. An index past the function's
            // upvalues is acceptable but reads as none.
            let i = (REGISTRY_INDEX as i64 - idx as i64) as usize;
            if i <= c.upvalue_count() {
                Slot::Upvalue(i - 1)
            } else {
                Slot::None
            }
        } else {
            let at = idx as i64 - 1;
            if at < len {
                Slot::Stack(at as usize)
            } else {
                Slot::None
            }
        }
    }

    #[inline]
    fn get(&self, idx: i32) -> RawValue {
        match self.slot(idx) {
            Slot::Stack(i) => self.ctx.arg(i),
            Slot::Registry => RawValue::Table(self.ctx.vm.rt.registry),
            Slot::Upvalue(i) => self.ctx.upvalue(i),
            Slot::None => RawValue::Nil,
        }
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
        Raised(match e {
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
            Meta::Call { f, nargs } => {
                let c = &mut self.ctx;
                let at = c.len() - nargs;
                c.insert(at, f);
                let n = c.call(nargs, None)?;
                let r = if n > 0 { c.arg(at) } else { RawValue::Nil };
                c.truncate(at);
                Ok(conv(r))
            }
        }
    }

    fn index(&mut self, obj: RawValue, key: RawValue) -> StackResult<RawValue> {
        let m = self.ctx.index(obj, key)?;
        self.finish_meta(m, |v| v)
    }

    fn set_index(&mut self, obj: RawValue, key: RawValue, v: RawValue) -> StackResult<()> {
        let m = self.ctx.set_index(obj, key, v)?;
        self.finish_meta(m, |_| ())
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
    pub fn set_top(&mut self, idx: i32) -> StackResult<()> {
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
        Ok(self.ctx.to_be_closed(i)?)
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

    pub fn type_at(&self, idx: i32) -> Type {
        if self.is_none(idx) {
            Type::None
        } else {
            type_of(self.get(idx))
        }
    }

    /// The number at `idx`, converting a numeric string.
    pub fn to_number(&self, idx: i32) -> Option<f64> {
        match self.get(idx) {
            RawValue::Int(i) => Some(i as f64),
            RawValue::Float(f) => Some(f),
            RawValue::Str(s) => match str_to_number(self.ctx.bytes(s)) {
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
            RawValue::Str(s) => str_to_number(self.ctx.bytes(s))?,
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
    pub fn to_str(&mut self, idx: i32) -> StackResult<Option<&[u8]>> {
        let s = match self.get(idx) {
            RawValue::Str(s) => s,
            v @ (RawValue::Int(_) | RawValue::Float(_)) => {
                let mut out = String::new();
                fmt_number(&mut out, v);
                let RawValue::Str(s) = self.ctx.new_str(out.as_bytes()) else {
                    unreachable!("new_str makes a string")
                };
                if let Slot::Stack(_) | Slot::Upvalue(_) = self.slot(idx) {
                    self.put(idx, RawValue::Str(s))?;
                }
                s
            }
            _ => return Ok(None),
        };
        Ok(Some(self.ctx.bytes(s)))
    }

    /// The string at `idx`, without converting numbers.
    pub fn string_at(&self, idx: i32) -> Option<&[u8]> {
        self.bytes_of(self.get(idx))
    }

    /// The number at `idx` as Lua prints it, or `None` if it isn't one.
    pub fn number_text(&self, idx: i32) -> Option<String> {
        match self.get(idx) {
            v @ (RawValue::Int(_) | RawValue::Float(_)) => {
                let mut out = String::new();
                fmt_number(&mut out, v);
                Some(out)
            }
            _ => None,
        }
    }

    /// The length of the value at `idx` with no metamethod: a string's
    /// bytes, a table's border, a [`Stack::new_userdata`] block's size.
    pub fn raw_len(&self, idx: i32) -> u64 {
        match self.get(idx) {
            RawValue::Str(s) => self.ctx.bytes(s).len() as u64,
            RawValue::Table(t) => self.ctx.vm.heap.get(t).border() as u64,
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
    /// host made with typed values (a [`Class`](crate::Class) instance).
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
                .userdata_mut::<crate::class::Slot<T>>(u)
                .map(|s| f(&mut s.0)),
            _ => None,
        }
    }

    /// Run `f` on argument `arg` (1-based) as the host value of type `T`,
    /// or raise Lua's type error naming `expected` (also while the value is
    /// taken out).
    pub fn check_userdata<T: Any, R>(
        &mut self,
        arg: i32,
        expected: &str,
        f: impl FnOnce(&mut T) -> R,
    ) -> StackResult<R> {
        match self.with_userdata::<T, _>(arg, |cell| cell.as_mut().map(f)) {
            Some(Some(r)) => Ok(r),
            _ => Err(self.type_error(arg, expected)),
        }
    }

    /// The host value of type `T` in the userdata at `idx`, in place: for
    /// the typed handles' borrows.
    pub(crate) fn userdata_in_place<T: Any>(&self, idx: i32) -> Option<&T> {
        match self.get(idx) {
            RawValue::UserData(u) => self
                .ctx
                .userdata_ref::<crate::class::Slot<T>>(u)
                .and_then(|s| s.0.as_ref()),
            _ => None,
        }
    }

    /// [`Stack::userdata_in_place`], mutably.
    pub(crate) fn userdata_in_place_mut<T: Any>(&mut self, idx: i32) -> Option<&mut T> {
        match self.get(idx) {
            RawValue::UserData(u) => self
                .ctx
                .userdata_mut::<crate::class::Slot<T>>(u)
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
        let id = self.native_index_of(N::NAME, native_entry::<N>);
        self.closure_data_of_index(idx, id)
    }

    /// The index the native `f` is registered under as `name`, registering
    /// it first if it isn't.
    pub(crate) fn native_index_of(&mut self, name: &'static str, f: vm::NativeFn) -> u32 {
        let RawValue::Native(id) = self.ctx.native(name, f) else {
            unreachable!("natives are registered as natives")
        };
        id.0
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

    /// See [`Control::interrupt_flag`].
    #[cfg(feature = "abi")]
    pub(crate) fn interrupt_flag(&self) -> *mut bool {
        self.ctx.vm.interrupt_flag()
    }

    /// The running native's index.
    #[cfg(feature = "abi")]
    pub(crate) fn running_native(&self) -> u32 {
        self.ctx.native_id().0
    }

    /// Push the native with index `id`.
    #[cfg(feature = "abi")]
    pub(crate) fn push_native_index(&mut self, id: u32) {
        self.ctx.push(RawValue::Native(NativeId(id)));
    }

    /// Whether the values at `a` and `b` are equal without metamethods.
    pub fn raw_equal(&self, a: i32, b: i32) -> bool {
        !self.is_none(a) && !self.is_none(b) && raw_eq(&self.ctx.vm.heap, self.get(a), self.get(b))
    }

    /// Compare the values at `a` and `b` as the operator would, metamethods
    /// included; false if either index is outside the window.
    pub fn compare(&mut self, a: i32, b: i32, op: CompareOp) -> StackResult<bool> {
        if self.is_none(a) || self.is_none(b) {
            return Ok(false);
        }
        let (x, y) = (self.get(a), self.get(b));
        let m = match op {
            CompareOp::Eq => self.ctx.equal(x, y)?,
            CompareOp::Lt => self.ctx.less_than(x, y)?,
            CompareOp::Le => self.ctx.less_equal(x, y)?,
        };
        self.finish_meta(m, |v| !v.is_falsy())
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

    /// Push the length of the value at `idx`, as `#` gives it.
    pub fn len(&mut self, idx: i32) -> StackResult<()> {
        let v = self.get(idx);
        let m = self.ctx.length(v)?;
        let r = self.finish_meta(m, |v| v)?;
        self.ctx.push(r);
        Ok(())
    }

    /// Push the number `text` spells, as `tonumber` reads it, and give its
    /// length plus one; 0, pushing nothing, if it isn't a numeral.
    pub fn push_numeral(&mut self, text: &[u8]) -> usize {
        match str_to_number(text) {
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
    pub fn push_bytes(&mut self, bytes: &[u8]) -> &[u8] {
        let s = self.push_str_value(bytes);
        self.ctx.bytes(s)
    }

    pub fn push_light_userdata(&mut self, p: usize) {
        self.ctx.push(light(p));
    }

    /// Push a userdata holding host value `value`, which
    /// [`Stack::userdata`] reads back.
    pub fn push_userdata<T: Any>(&mut self, value: T) {
        let u = self.ctx.new_userdata(crate::class::Slot(Some(value)));
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
        self.push_native_fn(N::NAME, native_entry::<N>);
    }

    /// The non-generic half of [`Stack::push_native`], so each native
    /// doesn't carry its own copy of the registry lookup.
    #[inline(never)]
    fn push_native_fn(&mut self, name: &'static str, f: vm::NativeFn) {
        let f = self.ctx.native(name, f);
        self.ctx.push(f);
    }

    /// Push a closure of native `N` over the top `nups` values (popped),
    /// which it reads at [`upvalue_index`], and over `data`, a word scripts
    /// can neither see nor change ([`Stack::closure_data`]).
    pub fn push_native_closure<N: Native>(&mut self, nups: usize, data: u64) -> StackResult<()> {
        self.push_closure_of(N::NAME, native_entry::<N>, nups, data)
    }

    /// [`Stack::push_native_closure`] for the native `f`, registered as
    /// `name`.
    pub(crate) fn push_closure_of(
        &mut self,
        name: &'static str,
        f: vm::NativeFn,
        nups: usize,
        data: u64,
    ) -> StackResult<()> {
        self.need(nups)?;
        let len = self.ctx.len();
        let ups = self.ctx.window()[len - nups..].to_vec();
        let hidden = [RawValue::LightUserData(LightUserData::new(data))];
        let v = self.ctx.new_closure_hidden(name, f, &ups, &hidden);
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
    pub fn get_field(&mut self, idx: i32, k: &[u8]) -> StackResult<Type> {
        let obj = self.get(idx);
        let key = self.ctx.new_str(k);
        self.get_with(obj, key)
    }

    /// Push `t[n]` for the value at `idx`.
    pub fn get_index(&mut self, idx: i32, n: i64) -> StackResult<Type> {
        let obj = self.get(idx);
        self.get_with(obj, RawValue::Int(n))
    }

    /// Push the global `name`.
    pub fn get_global(&mut self, name: &[u8]) -> StackResult<Type> {
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
    pub fn raw_get_field(&mut self, idx: i32, k: &[u8]) -> StackResult<Type> {
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
            Some(UserValue::InTable(t, i)) => self.ctx.vm.heap.get(t).get_int(i),
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
                    return Err(Raised(self.ctx.no_memory()));
                }
                true
            }
            None => false,
        };
        self.ctx.truncate(len - 1);
        Ok(r)
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
    pub fn set_field(&mut self, idx: i32, k: &[u8]) -> StackResult<()> {
        let obj = self.get(idx);
        let key = self.ctx.new_str(k);
        self.set_popping(obj, key)
    }

    /// `t[n] = top` (popped) for the value at `idx`.
    pub fn set_index_int(&mut self, idx: i32, n: i64) -> StackResult<()> {
        let obj = self.get(idx);
        self.set_popping(obj, RawValue::Int(n))
    }

    /// Pop the top value into global `name`.
    pub fn set_global(&mut self, name: &[u8]) -> StackResult<()> {
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
    pub fn raw_set_field(&mut self, idx: i32, k: &[u8]) -> StackResult<()> {
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
                Err(Raised(c.error_value(msg)))
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
        let n = self.ctx.call(nargs, None)?;
        self.adjust_results(n, nresults);
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

    /// Push an error's value.
    pub fn push_raised(&mut self, e: &Raised) {
        self.ctx.push(e.0.value);
    }

    /// Push an error's value, and give its message as the standalone
    /// interpreter prints it and the traceback it carries, if any.
    pub fn take_raised(&mut self, e: Raised) -> (String, Option<String>) {
        let message = self.ctx.vm.error_message(&e.0);
        self.ctx.push(e.0.value);
        (message, e.0.traceback)
    }

    /// Lua's `not enough memory` error, to return.
    pub fn memory_error(&mut self) -> Raised {
        Raised(self.ctx.no_memory())
    }

    /// Compile (or decode) `bytes` as a chunk named `name`, with the global
    /// table as its `_ENV`, and push its function, or the message.
    /// `mode` is `load`'s: `b"t"`, `b"b"` or `b"bt"`.
    pub fn load(&mut self, bytes: &[u8], name: &[u8], mode: &[u8]) -> Status {
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

    /// Compile (or decode) `chunk` and push its function, with the value at
    /// `env` as its `_ENV` (0: the global table); or push the message and
    /// give [`Status::Syntax`] (or [`Status::Memory`]).
    pub fn load_chunk(&mut self, chunk: &crate::Chunk<'_>, env: i32) -> Status {
        let env = if env == 0 {
            RawValue::Table(self.ctx.vm.rt.globals)
        } else {
            self.get(env)
        };
        match crate::chunk::load(&mut self.ctx, chunk, env) {
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

    /// Push the main function of a chunk [`Compiler`](crate::Compiler)
    /// compiled, linked here with the global table as its `_ENV`.
    pub fn push_compiled(&mut self, chunk: &crate::UnlinkedChunk) {
        let env = RawValue::Table(self.ctx.vm.rt.globals);
        let f = self.ctx.link(chunk, env);
        self.ctx.push(f);
    }

    // ---- errors --------------------------------------------------------------------

    /// An error with the top value as its value (nil if the window is
    /// empty), to return.
    pub fn error_from_top(&mut self) -> Raised {
        let v = self.top_value();
        Raised(self.ctx.error_value(v))
    }

    /// A runtime error with `message`, positioned at the caller as Lua's
    /// `error` does at level 1.
    pub fn error(&mut self, message: impl AsRef<str>) -> Raised {
        Raised(self.ctx.error(message))
    }

    /// Lua's argument error for argument `arg` (1-based): `bad argument #1
    /// to 'f' (message)`.
    pub fn arg_error(&mut self, arg: i32, message: &str) -> Raised {
        Raised(self.ctx.arg_error((arg.max(1) - 1) as usize, message))
    }

    /// Lua's type error for argument `arg` (1-based): `bad argument #1 to
    /// 'f' (number expected, got nil)`.
    pub fn type_error(&mut self, arg: i32, expected: &str) -> Raised {
        Raised(self.ctx.type_error((arg.max(1) - 1) as usize, expected))
    }

    /// Argument `arg` (1-based) as a string (numbers convert), or Lua's
    /// argument error.
    pub fn check_string(&mut self, arg: i32) -> StackResult<&[u8]> {
        let s = self.ctx.check_str((arg.max(1) - 1) as usize)?;
        Ok(self.ctx.bytes(s))
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
    pub fn display(&self, idx: i32) -> Vec<u8> {
        let mut out = Vec::new();
        self.ctx.display(self.get(idx), &mut out);
        out
    }

    /// Push `chunk:line:` for the function at `level` (1 is the caller),
    /// or an empty string where that isn't known.
    pub fn push_where(&mut self, level: i32) {
        let w = self.ctx.where_(level.max(0) as usize);
        self.push_str_value(w.as_bytes());
    }

    /// Push `message` (if any), a newline, and the traceback from `level`
    /// on, as one string (`luaL_traceback` on the running thread).
    pub fn push_traceback(&mut self, message: Option<&[u8]>, level: i32) {
        let mut out = Vec::new();
        if let Some(m) = message {
            out.extend_from_slice(m);
            out.push(b'\n');
        }
        out.extend_from_slice(self.ctx.traceback(level.max(0) as usize).as_bytes());
        self.push_str_value(&out);
    }

    /// Push the value at `idx` as `tostring` converts it (`__tostring`,
    /// then `__name`), and give the string's bytes.
    pub fn to_display_string(&mut self, idx: i32) -> StackResult<&[u8]> {
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
        use orbit_runtime::vm::{GcMode, GcParam as P};
        let c = &mut self.ctx;
        if c.vm.rt.gc.finalizing() {
            return None;
        }
        Some(match op {
            GcOp::Collect => {
                c.full_collect();
                0
            }
            GcOp::Step(kb) => match c.vm.rt.gc.mode {
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
    pub fn set_gc_mode(&mut self, mode: crate::GcMode) -> Option<crate::GcMode> {
        let vm = &mut *self.ctx.vm;
        if vm.rt.gc.finalizing() {
            return None;
        }
        Some(match vm.set_gc_mode(mode.raw()) {
            vm::GcMode::Generational => crate::GcMode::Generational,
            vm::GcMode::Incremental => crate::GcMode::Incremental,
        })
    }

    /// Emit a warning piece (`lua_warning`): pieces join until one with
    /// `more` false. A one-piece `@on` or `@off` switches warnings.
    pub fn warn(&mut self, msg: &[u8], more: bool) {
        let rt = &mut self.ctx.vm.rt;
        if let Some(hook) = &mut rt.warn_hook {
            (hook.0)(msg, more);
            return;
        }
        let pending = rt.ext.get_or_insert_with(WarnBuffer::default);
        let first = pending.0.is_empty();
        pending.0.extend_from_slice(msg);
        if more {
            return;
        }
        let text = std::mem::take(&mut pending.0);
        // A one-piece message starting with '@' is a control message.
        match (first, text.as_slice()) {
            (true, b"@on") => rt.warnings = true,
            (true, b"@off") => rt.warnings = false,
            (true, [b'@', ..]) => {}
            _ => rt.warn(&text),
        }
    }

    /// Send warning pieces to `hook` (the piece, and whether more follow)
    /// instead of showing them (`lua_setwarnf`).
    pub fn set_warn_hook(&mut self, hook: impl FnMut(&[u8], bool) + 'static) {
        self.ctx.vm.rt.warn_hook = Some(vm::WarnHook(Box::new(hook)));
    }

    /// Open standard library `lib` as [`Config::libraries`] would (setting
    /// its global), and push its table; false, pushing nothing, if this
    /// build leaves it out. `ignore_env` as [`Config::ignore_env`].
    ///
    /// [`Config::libraries`]: crate::Config::libraries
    /// [`Config::ignore_env`]: crate::Config::ignore_env
    pub fn open_library(&mut self, lib: Library, ignore_env: bool) -> bool {
        let options = stdlib::Options { ignore_env };
        let top = self.ctx.len();
        let opened = stdlib::open_one(self.ctx.vm, lib.lib(), options);
        // Running the built-in modules can leave the VM's top above the
        // window's; what lies there is no value of ours.
        self.ctx.truncate(top);
        match opened {
            Some(t) => {
                self.ctx.push(t);
                true
            }
            None => false,
        }
    }

    /// Run `f` with a scope over this window: the typed API, for code that
    /// builds on it (`orbit-std`'s libraries). What `f` made is dropped
    /// after.
    pub fn with_scope<R>(&mut self, f: impl for<'a> FnOnce(&'a Scope<'a>) -> R) -> R {
        let start = self.top();
        let scope = Scope::new(self.reborrow());
        let r = f(&scope);
        let _ = scope.stack().set_top(start);
        r
    }

    /// [`Stack::with_scope`], pushing the value `f` gives, if any.
    pub fn push_from_scope(
        &mut self,
        f: impl for<'a> FnOnce(&'a Scope<'a>) -> Option<Value<'a>>,
    ) -> bool {
        let start = self.top();
        let scope = Scope::new(self.reborrow());
        let v = f(&scope);
        let mut stack = scope.stack();
        let end = match v {
            Some(v) => {
                v.push(&mut stack);
                let r = stack.copy(-1, start + 1);
                debug_assert!(r.is_ok(), "the slot after the start exists");
                start + 1
            }
            None => start,
        };
        let _ = stack.set_top(end);
        v.is_some()
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

    /// Write to the isolate's standard output, the stream `print` and
    /// `io.stdout` share. `f` must not reach the isolate.
    pub fn with_stdout<R>(&mut self, f: impl FnOnce(&mut dyn std::io::Write) -> R) -> R {
        f(&mut *self.ctx.vm.rt.out)
    }

    /// Run `hook` when a script ends the process ([`Return::exit`],
    /// `os.exit`), after the isolate closes (if it does) and before
    /// standard output is flushed.
    pub fn before_exit(&mut self, hook: impl Fn() + 'static) {
        crate::isolate::backend::exit_hooks(self.ctx.vm)
            .0
            .push(Box::new(hook));
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
        f(self.ctx.vm.rt.ext.get_or_insert_with(T::default))
    }

    /// Run `f` on the isolate's extension value of type `T` if one was
    /// made, without making one: the cheaper lookup for a native's hot
    /// path.
    #[inline]
    pub fn find_extension<T: Any, R>(&self, f: impl FnOnce(&T) -> R) -> Option<R> {
        self.ctx.vm.rt.ext.get::<T>().map(f)
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

    /// Run `f` with `t`'s stack as the window ([`Stack::enter_thread`]).
    pub fn on_thread<R>(
        &mut self,
        t: ThreadRef,
        base: Option<usize>,
        f: impl FnOnce(&mut Stack<'s>) -> R,
    ) -> R {
        let w = self.enter_thread(t, base);
        let r = f(self);
        self.leave_thread(t, w);
        r
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
            Err(v) => Err(Raised(LuaError::new(v))),
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
            source: fi.source,
            short_src: fi.short_src,
            linedefined: fi.line_defined as i32,
            lastlinedefined: fi.last_line as i32,
            nups: fi.nups.min(255) as u8,
            nparams: fi.nparams,
            isvararg: fi.is_vararg,
            currentline: -1,
            namewhat: "",
            defined_name: fi.defined_name,
            ..FrameInfo::default()
        };
        if let Some(l) = at {
            out.currentline = c.current_line(l) as i32;
            out.istailcall = c.is_tail_call(l);
            out.extraargs = c.extra_args(l);
            if let Some(n) = c.level_name(l) {
                out.namewhat = n.namewhat;
                out.name = Some(n.name);
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
    pub fn get_local(&mut self, level: usize, n: i32) -> Option<Vec<u8>> {
        let l = self.ctx.level(level)?;
        let (name, v) = self.ctx.get_local(l, n.into())?;
        self.ctx.push(v);
        Some(name)
    }

    /// The name of parameter `n` of the script function on top, pushing
    /// nothing.
    pub fn param_name(&mut self, n: i32) -> Option<Vec<u8>> {
        let f = self.ctx.arg(self.ctx.len().checked_sub(1)?);
        self.ctx.param_name(f, n.into())
    }

    /// Pop the top value into local `n` of activation `level` and give its
    /// name; with no such local, pop nothing.
    pub fn set_local(&mut self, level: usize, n: i32) -> StackResult<Option<Vec<u8>>> {
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
        Ok(name)
    }

    /// Upvalue `n` (1-based) of the function at `func`: its name, its value
    /// pushed.
    pub fn get_upvalue(&mut self, func: i32, n: i32) -> Option<Vec<u8>> {
        let f = self.get(func);
        let (name, v) = self.ctx.upvalue_of(f, n as i64)?;
        self.ctx.push(v);
        Some(name)
    }

    /// Pop the top value into upvalue `n` of the function at `func` and
    /// give its name.
    pub fn set_upvalue(&mut self, func: i32, n: i32) -> StackResult<Option<Vec<u8>>> {
        self.need(1)?;
        let f = self.get(func);
        let len = self.ctx.len();
        let v = self.ctx.arg(len - 1);
        let r = self.ctx.set_upvalue_of(f, n as i64, v);
        self.ctx.truncate(len - 1);
        Ok(r)
    }

    /// An identity for upvalue `n` of the function at `func` (shared
    /// upvalues have the same one), or 0.
    pub fn upvalue_id(&self, func: i32, n: i32) -> usize {
        match self.ctx.upvalue_cell(self.get(func), n as i64) {
            Some((_, cell)) => cell.to_bits() as usize | 1 << 63,
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
/// [`Stack::control`], [`Isolate::control`](crate::Isolate::control), or an
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
    pub(crate) fn new(vm: &'a mut Vm) -> Control<'a> {
        Control { vm }
    }

    #[cfg(feature = "abi")]
    pub(crate) fn reborrow(&mut self) -> Control<'_> {
        Control { vm: &mut *self.vm }
    }

    /// See [`Stack::extension`].
    pub fn extension<T: Any + Default, R>(&mut self, f: impl FnOnce(&mut T) -> R) -> R {
        f(self.vm.rt.ext.get_or_insert_with(T::default))
    }

    /// The interrupt flag, for code that can only store to memory (a C
    /// signal handler): storing a nonzero byte interrupts running code, as
    /// [`Interrupt::request`](crate::Interrupt) does. Valid as long as the
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
        self.vm.rt.interrupt_handler = Some(interrupt_entry::<H>);
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

    /// [`crate::Isolate::set_breakpoints`], also from inside a stop.
    #[cfg(feature = "debugger")]
    pub fn set_breakpoints(
        &mut self,
        source: &[u8],
        lines: &[u32],
    ) -> Vec<crate::debugger::Breakpoint> {
        self.vm
            .set_breakpoints(source, lines)
            .into_iter()
            .map(|b| crate::debugger::Breakpoint {
                requested: b.requested,
                line: b.line,
            })
            .collect()
    }

    /// [`crate::Isolate::stop_on_next_line`].
    #[cfg(feature = "debugger")]
    pub fn stop_on_next_line(&mut self) {
        self.vm.stop_on_next_line()
    }

    /// [`crate::Isolate::set_error_break`], also from inside a stop.
    #[cfg(feature = "debugger")]
    pub fn set_error_break(&mut self, on: crate::debugger::ErrorBreak) {
        use crate::debugger::ErrorBreak;
        use orbit_runtime::vm::debugger as engine;
        self.vm.set_error_break(match on {
            ErrorBreak::Never => engine::ErrorBreak::Never,
            ErrorBreak::Uncaught => engine::ErrorBreak::Uncaught,
            ErrorBreak::All => engine::ErrorBreak::All,
        })
    }
}
