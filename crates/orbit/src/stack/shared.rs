//! The stack API over `liborbit` (ADR-10): each operation crosses the
//! boundary declared in [`crate::abi`].
//!
//! What only means something in the program stays here, in the isolate's
//! [`HostState`]: host values in userdata (the library holds an id), the
//! extensions, and which natives and continuations the program has
//! registered.

use std::any::{Any, TypeId};
use std::cell::RefCell;
use std::collections::HashMap;
use std::marker::PhantomData;
use std::mem::ManuallyDrop;
use std::rc::Rc;

use stabby::abi::vtable::{CompoundVt, VtDrop};
use stabby::abi::{AnonymRefMut, Dyn, IDeterminantProvider};
use stabby::boxed::Box as SBox;
use stabby::option::Option as SOption;
use stabby::slice::Slice;

use crate::abi::*;
use crate::isolate::Library;
use crate::value::ThreadStatus;
use crate::{OrbitStr, OrbitString};

use super::{
    ArithOp, CompareOp, Continuation, FrameInfo, GcOp, InterruptHandler, Native, Outcome, Resumed,
    Ret, Return, StackResult, Status, Type,
};

/// An error raised in the isolate, to return from a native (an `Err`
/// unwinds to the nearest protected call). The library holds the error
/// until the native call or host entry that raised it ends.
#[derive(Debug)]
pub struct Raised(pub(crate) RaisedId);

/// A thread (coroutine), for keeping between calls. It does not keep the
/// thread alive: check [`Stack::thread_is_live`] before using one that may
/// have been collected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ThreadRef {
    bits: u64,
    slot: u32,
}

impl ThreadRef {
    /// The heap slot the thread occupies. A collected thread's slot goes to
    /// the next thread made, so this tells live threads apart only.
    pub fn slot(self) -> u32 {
        self.slot
    }

    fn abi(self) -> ThreadAbi {
        ThreadAbi {
            bits: self.bits,
            slot: self.slot,
        }
    }
}

impl From<ThreadAbi> for ThreadRef {
    fn from(t: ThreadAbi) -> ThreadRef {
        ThreadRef {
            bits: t.bits,
            slot: t.slot,
        }
    }
}

/// Another thread's stack in place of the window, between
/// [`Stack::enter_thread`] and [`Stack::leave_thread`].
#[derive(Debug)]
#[must_use]
pub struct ThreadWindow(u64);

/// A native with data words, from [`Control::register_native`]: a plain
/// value with no collector state, to keep as long as the isolate.
#[derive(Debug, Clone, Copy)]
pub struct NativeRef(u32);

/// How a [`Return`] names the continuation to run: the type, and how to
/// make the object the library calls, the first time the isolate sees it.
#[derive(Clone, Copy)]
pub(crate) struct ContRef {
    id: fn() -> TypeId,
    make: fn(Rc<HostState>) -> ContinuationBox,
}

impl std::fmt::Debug for ContRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ContRef")
    }
}

/// The continuation behind `K`.
pub(crate) fn cont_ref<K: Continuation>() -> ContRef {
    ContRef {
        id: TypeId::of::<K>,
        make: |host| SBox::new(ContinuationObj::<K>(host, PhantomData)).into(),
    }
}

// ---- the program's objects -------------------------------------------------------

/// What one isolate keeps on the program's side of the boundary.
#[derive(Default)]
pub(crate) struct HostState {
    ext: RefCell<HashMap<TypeId, Box<dyn Any>>>,
    pub(crate) values: RefCell<HostValues>,
    /// Host values whose userdata was collected while `values` was
    /// borrowed, to drop at the next chance.
    dropped: RefCell<Vec<u64>>,
    /// The natives registered so far, by type: their index.
    natives: RefCell<HashMap<TypeId, u32>>,
    /// The continuations registered so far: their slot.
    continuations: RefCell<HashMap<TypeId, u32>>,
}

impl std::fmt::Debug for HostState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HostState").finish_non_exhaustive()
    }
}

/// The host values in an isolate's userdata, each an `Option<T>` (`None`
/// while a running method has the value), by id.
#[derive(Default)]
pub(crate) struct HostValues {
    slots: Vec<Option<Box<dyn Any>>>,
    free: Vec<u64>,
}

impl HostValues {
    /// The cell of the host value `id`, if it is a `T`.
    pub(crate) fn get<T: Any>(&self, id: u64) -> Option<&Option<T>> {
        self.slots.get(id as usize)?.as_ref()?.downcast_ref()
    }

    pub(crate) fn get_mut<T: Any>(&mut self, id: u64) -> Option<&mut Option<T>> {
        self.slots.get_mut(id as usize)?.as_mut()?.downcast_mut()
    }
}

impl HostState {
    pub(crate) fn extension<T: Any + Default, R>(&self, f: impl FnOnce(&mut T) -> R) -> R {
        let mut ext = self.ext.borrow_mut();
        let v = ext
            .entry(TypeId::of::<T>())
            .or_insert_with(|| Box::new(T::default()));
        f(v.downcast_mut().expect("extensions are keyed by type"))
    }

    pub(crate) fn find_extension<T: Any, R>(&self, f: impl FnOnce(&T) -> R) -> Option<R> {
        let ext = self.ext.borrow();
        ext.get(&TypeId::of::<T>())
            .and_then(|v| v.downcast_ref())
            .map(f)
    }

    fn insert_value(&self, v: Box<dyn Any>) -> u64 {
        self.drop_pending();
        let mut values = self.values.borrow_mut();
        match values.free.pop() {
            Some(id) => {
                values.slots[id as usize] = Some(v);
                id
            }
            None => {
                values.slots.push(Some(v));
                values.slots.len() as u64 - 1
            }
        }
    }

    fn remove_value(&self, id: u64) {
        let v = match self.values.try_borrow_mut() {
            Ok(mut values) => {
                values.free.push(id);
                values.slots.get_mut(id as usize).and_then(Option::take)
            }
            Err(_) => {
                self.dropped.borrow_mut().push(id);
                None
            }
        };
        // Dropped with the table released: its destructor is the host's.
        drop(v);
    }

    fn drop_pending(&self) {
        let ids = std::mem::take(&mut *self.dropped.borrow_mut());
        for id in ids {
            self.remove_value(id);
        }
    }
}

/// A host value in a userdata, as the library holds it.
struct HeldId(u64, Rc<HostState>);

impl HostValue for HeldId {
    extern "C" fn id(&self) -> u64 {
        self.0
    }
}

impl Drop for HeldId {
    fn drop(&mut self) {
        self.1.remove_value(self.0);
    }
}

/// Run `f` on the window the library hands a program object.
pub(crate) fn on_window<R>(
    host: &Rc<HostState>,
    s: StackArg<'_>,
    f: impl FnOnce(&mut Stack<'_>) -> R,
) -> R {
    let mut s = s.into_inner();
    let mut stack = Stack::new(&mut s, Rc::clone(host));
    f(&mut stack)
}

struct NativeObj<N>(Rc<HostState>, PhantomData<fn() -> N>);

impl<N: Native> ProgramNative for NativeObj<N> {
    extern "C" fn call<'a>(&self, s: StackArg<'a>) -> RetAbi {
        on_window(&self.0, s, |s| {
            let r = N::call(s);
            s.ret(r)
        })
    }
}

type FnBox = Box<dyn for<'a, 'b> Fn(&'a mut Stack<'b>) -> StackResult<Return>>;

struct FunctionObj(Rc<HostState>, FnBox);

impl ProgramNative for FunctionObj {
    extern "C" fn call<'a>(&self, s: StackArg<'a>) -> RetAbi {
        on_window(&self.0, s, |s| {
            let r = (self.1)(s);
            s.ret(r)
        })
    }
}

struct ContinuationObj<K>(Rc<HostState>, PhantomData<fn() -> K>);

impl<K: Continuation> ProgramContinuation for ContinuationObj<K> {
    extern "C" fn resume<'a>(
        &self,
        s: StackArg<'a>,
        ok: bool,
        n: usize,
        raised: RaisedId,
    ) -> RetAbi {
        on_window(&self.0, s, |s| {
            let o = if ok {
                Outcome::Ok(n)
            } else {
                Outcome::Err(Raised(raised))
            };
            let r = K::resume(s, o);
            s.ret(r)
        })
    }
}

struct InterruptObj<H>(Rc<HostState>, PhantomData<fn() -> H>);

impl<H: InterruptHandler> ProgramInterruptHandler for InterruptObj<H> {
    extern "C" fn handle<'c>(&self, c: ControlArg<'c>) -> bool {
        H::handle(&mut Control::new(c.into_inner(), Rc::clone(&self.0)))
    }
}

struct HookObj<F>(F);

impl<F: Fn() + 'static> Hook for HookObj<F> {
    extern "C" fn run(&self) {
        (self.0)()
    }
}

pub(crate) fn hook(f: impl Fn() + 'static) -> HookBox {
    SBox::new(HookObj(f)).into()
}

struct WarnObj<F>(F);

impl<F: FnMut(OrbitStr<'_>, bool) + 'static> WarnHook for WarnObj<F> {
    extern "C" fn warn<'b>(&mut self, message: Slice<'b, u8>, more: bool) {
        (self.0)(str_of(message), more)
    }
}

/// Runs a program's closure on the isolate's standard output.
struct StdoutObj<F, R>(Option<F>, Option<R>);

impl<F: FnOnce(&mut dyn std::io::Write) -> R, R> StdoutUser for StdoutObj<F, R> {
    extern "C" fn run<'w>(&mut self, out: WriterMut<'w>) {
        if let Some(f) = self.0.take() {
            self.1 = Some(f(&mut LibOut(out)));
        }
    }
}

/// The library's stream, as a program writes to it.
struct LibOut<'w>(WriterMut<'w>);

impl std::io::Write for LibOut<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        Result::from(self.0.write(buf.into()))
            .map_err(|m: stabby::string::String| std::io::Error::other(m.to_string()))
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Result::from(self.0.flush())
            .map_err(|m: stabby::string::String| std::io::Error::other(m.to_string()))
    }
}

// ---- conversions -----------------------------------------------------------------

fn res<T: IDeterminantProvider<RaisedId>>(r: Res<T>) -> StackResult<T> {
    Result::from(r).map_err(Raised)
}

fn type_of(code: i32) -> Type {
    Type::from_code(code).unwrap_or(Type::None)
}

fn status_of(code: i32) -> Status {
    Status::from_code(code).unwrap_or(Status::Runtime)
}

fn bytes(v: SOption<stabby::vec::Vec<u8>>) -> Option<OrbitString> {
    Option::from(v).map(|v: stabby::vec::Vec<u8>| OrbitString::from(&v[..]))
}

/// A byte string the boundary lends.
fn str_of(s: Slice<'_, u8>) -> OrbitStr<'_> {
    OrbitStr::from(s.as_slice())
}

fn frame_info(f: FrameInfoAbi) -> FrameInfo {
    FrameInfo {
        name: bytes(f.name),
        namewhat: f.namewhat.as_str(),
        what: f.what.as_str(),
        source: f.source.to_vec().into(),
        short_src: f.short_src.to_string(),
        currentline: f.currentline,
        linedefined: f.linedefined,
        lastlinedefined: f.lastlinedefined,
        nups: f.nups,
        nparams: f.nparams,
        isvararg: f.isvararg,
        extraargs: f.extraargs,
        istailcall: f.istailcall,
        defined_name: bytes(f.defined_name),
    }
}

/// [`GcOp`] as the boundary's codes: the op, the parameter, the value.
fn gc_op(op: GcOp) -> (u8, u8, Option<i64>) {
    match op {
        GcOp::Stop => (0, 0, None),
        GcOp::Restart => (1, 0, None),
        GcOp::Collect => (2, 0, None),
        GcOp::Count => (3, 0, None),
        GcOp::CountRemainder => (4, 0, None),
        GcOp::Step(n) => (5, 0, Some(i64::from(n))),
        GcOp::IsRunning => (6, 0, None),
        GcOp::Param(p, v) => (7, p as u8, v),
    }
}

// ---- the stack -------------------------------------------------------------------

type StackVt = <dyn StackAbi as CompoundVt<'static>>::Vt<VtDrop>;

/// The window of a native function, or the host's window between calls.
pub struct Stack<'s> {
    /// Kept from dropping, which does nothing for a borrow but would make
    /// the borrow checker require the window to outlive every scope over it.
    w: ManuallyDrop<Dyn<'s, AnonymRefMut<'s>, StackVt>>,
    pub(crate) host: Rc<HostState>,
}

impl std::fmt::Debug for Stack<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Stack")
            .field("top", &self.top())
            .finish_non_exhaustive()
    }
}

impl<'s> Stack<'s> {
    pub(crate) fn new(w: &'s mut StackMut<'_>, host: Rc<HostState>) -> Stack<'s> {
        Stack {
            w: ManuallyDrop::new(w.as_mut()),
            host,
        }
    }

    /// The same window, borrowed for a shorter time: for code that keeps a
    /// stack in a value of its own while the native runs.
    pub fn reborrow(&mut self) -> Stack<'_> {
        Stack {
            w: ManuallyDrop::new(self.w.as_mut()),
            host: Rc::clone(&self.host),
        }
    }

    /// What a native ended with, for the library.
    fn ret(&mut self, r: StackResult<Return>) -> RetAbi {
        let mut out = RetAbi {
            kind: RetAbi::VALUES,
            flag: false,
            k: 0,
            n: 0,
            raised: 0,
        };
        match r {
            Ok(Return(Ret::Values(n))) => out.n = n as u64,
            Ok(Return(Ret::Yield(n))) => {
                out.kind = RetAbi::YIELD;
                out.n = n as u64;
            }
            Ok(Return(Ret::YieldThen(n, k))) => {
                out.kind = RetAbi::YIELD_THEN;
                out.n = n as u64;
                out.k = self.continuation(k);
            }
            Ok(Return(Ret::CallThen {
                nargs,
                protected,
                k,
            })) => {
                out.kind = RetAbi::CALL_THEN;
                out.n = nargs as u64;
                out.flag = protected;
                out.k = self.continuation(k);
            }
            Ok(Return(Ret::Exit { code, close })) => {
                out.kind = RetAbi::EXIT;
                out.n = code as u64;
                out.flag = close;
            }
            Err(Raised(e)) => {
                out.kind = RetAbi::RAISE;
                out.raised = e;
            }
        }
        out
    }

    /// The slot of continuation `k`, registering it first if it isn't.
    fn continuation(&mut self, k: ContRef) -> u32 {
        let id = (k.id)();
        if let Some(&slot) = self.host.continuations.borrow().get(&id) {
            return slot;
        }
        let slot = self
            .w
            .register_continuation((k.make)(Rc::clone(&self.host)));
        self.host.continuations.borrow_mut().insert(id, slot);
        slot
    }

    /// The index of native `N`, registering it first if it isn't.
    fn native<N: Native>(&mut self) -> u32 {
        if let Some(&id) = self.host.natives.borrow().get(&TypeId::of::<N>()) {
            return id;
        }
        let f = SBox::new(NativeObj::<N>(Rc::clone(&self.host), PhantomData)).into();
        let id = self.w.register_native(N::NAME.into(), f);
        self.host.natives.borrow_mut().insert(TypeId::of::<N>(), id);
        id
    }

    // ---- the window ------------------------------------------------------------------

    pub fn abs_index(&self, idx: i32) -> i32 {
        self.w.abs_index(idx)
    }

    pub fn top(&self) -> i32 {
        self.w.top()
    }

    pub fn window_start(&self) -> usize {
        self.w.window_start()
    }

    pub fn set_top(&mut self, idx: i32) -> StackResult<()> {
        res(self.w.set_top(idx))
    }

    pub fn to_close(&mut self, idx: i32) -> StackResult<()> {
        res(self.w.to_close(idx))
    }

    pub fn close_slot(&mut self, idx: i32) -> StackResult<()> {
        res(self.w.close_slot(idx))
    }

    pub fn close_all(&mut self) -> StackResult<()> {
        res(self.w.close_all())
    }

    pub fn push_copy(&mut self, idx: i32) {
        self.w.push_copy(idx)
    }

    pub fn rotate(&mut self, idx: i32, n: i32) -> StackResult<()> {
        res(self.w.rotate(idx, n))
    }

    pub fn copy(&mut self, from: i32, to: i32) -> StackResult<()> {
        res(self.w.copy(from, to))
    }

    pub fn check_room(&self, n: i32) -> bool {
        self.w.check_room(n)
    }

    // ---- reading values --------------------------------------------------------------

    /// [`Stack::type_at`] for argument `i` (1-based).
    #[inline(always)]
    pub(crate) fn arg_type(&self, i: i32) -> Type {
        self.type_at(i)
    }

    pub fn type_at(&self, idx: i32) -> Type {
        type_of(self.w.type_at(idx))
    }

    pub fn to_number(&self, idx: i32) -> Option<f64> {
        self.w.to_number(idx).into()
    }

    pub fn to_integer(&self, idx: i32) -> Option<i64> {
        self.w.to_integer(idx).into()
    }

    pub fn is_string(&self, idx: i32) -> bool {
        self.w.is_string(idx)
    }

    pub fn is_integer(&self, idx: i32) -> bool {
        self.w.is_integer(idx)
    }

    pub fn is_userdata(&self, idx: i32) -> bool {
        self.w.is_userdata(idx)
    }

    pub fn is_native_function(&self, idx: i32) -> bool {
        self.w.is_native_function(idx)
    }

    pub fn to_boolean(&self, idx: i32) -> bool {
        self.w.to_boolean(idx)
    }

    pub fn to_str(&mut self, idx: i32) -> StackResult<Option<OrbitStr<'_>>> {
        res(self.w.to_str(idx)).map(|s| Option::from(s).map(str_of))
    }

    pub fn string_at(&self, idx: i32) -> Option<OrbitStr<'_>> {
        Option::from(self.w.string_at(idx)).map(str_of)
    }

    pub fn number_text(&self, idx: i32) -> Option<String> {
        Option::from(self.w.number_text(idx)).map(|t: stabby::string::String| t.to_string())
    }

    pub fn raw_len(&self, idx: i32) -> u64 {
        self.w.raw_len(idx)
    }

    pub fn to_userdata(&self, idx: i32) -> *mut std::ffi::c_void {
        self.w.to_userdata(idx) as *mut std::ffi::c_void
    }

    /// The id of the program's host value in the userdata at `idx`.
    pub(crate) fn host_value_id(&mut self, idx: i32) -> Option<u64> {
        self.w.host_value_id(idx).into()
    }

    pub fn with_userdata<T: Any, R>(
        &mut self,
        idx: i32,
        f: impl FnOnce(&mut Option<T>) -> R,
    ) -> Option<R> {
        let id = self.host_value_id(idx)?;
        let mut values = self.host.values.borrow_mut();
        values.get_mut::<T>(id).map(f)
    }

    pub fn is_host_userdata(&self, idx: i32) -> bool {
        self.w.is_host_userdata(idx)
    }

    pub fn identity(&self, idx: i32) -> Option<u64> {
        self.w.identity(idx).into()
    }

    pub fn to_pointer(&self, idx: i32) -> usize {
        self.w.to_pointer(idx)
    }

    pub fn closure_data_of<N: Native>(&mut self, idx: i32) -> Option<u64> {
        let id = self.native::<N>();
        self.w.closure_data_of(idx, id).into()
    }

    pub fn raw_equal(&self, a: i32, b: i32) -> bool {
        self.w.raw_equal(a, b)
    }

    pub fn compare(&mut self, a: i32, b: i32, op: CompareOp) -> StackResult<bool> {
        res(self.w.compare(a, b, op as u8))
    }

    // ---- operators -------------------------------------------------------------------

    pub fn arith(&mut self, op: ArithOp) -> StackResult<()> {
        res(self.w.arith(op as u8))
    }

    pub fn concat(&mut self, n: i32) -> StackResult<()> {
        res(self.w.concat(n))
    }

    pub fn len(&mut self, idx: i32) -> StackResult<()> {
        res(self.w.len(idx))
    }

    pub fn push_numeral<'t>(&mut self, text: impl Into<OrbitStr<'t>>) -> usize {
        let text = text.into().as_bytes();
        self.w.push_numeral(text.into())
    }

    // ---- pushing values --------------------------------------------------------------

    pub fn push_nil(&mut self) {
        self.w.push_nil()
    }

    pub fn push_boolean(&mut self, b: bool) {
        self.w.push_boolean(b)
    }

    pub fn push_integer(&mut self, n: i64) {
        self.w.push_integer(n)
    }

    pub fn push_number(&mut self, n: f64) {
        self.w.push_number(n)
    }

    pub fn push_bytes<'b>(&mut self, bytes: impl Into<OrbitStr<'b>>) -> OrbitStr<'_> {
        let bytes = bytes.into().as_bytes();
        str_of(self.w.push_bytes(bytes.into()))
    }

    pub fn push_light_userdata(&mut self, p: usize) {
        self.w.push_light_userdata(p)
    }

    pub fn push_userdata<T: Any>(&mut self, value: T) {
        let id = self.host.insert_value(Box::new(Some(value)));
        let held = SBox::new(HeldId(id, Rc::clone(&self.host))).into();
        self.w.push_host_value(held)
    }

    pub fn reserve_memory(&mut self, bytes: usize) -> StackResult<()> {
        res(self.w.reserve_memory(bytes))
    }

    pub fn push_function(
        &mut self,
        f: impl for<'a, 'b> Fn(&'a mut Stack<'b>) -> StackResult<Return> + 'static,
    ) {
        let f = SBox::new(FunctionObj(Rc::clone(&self.host), Box::new(f))).into();
        self.w.push_function(f)
    }

    pub fn push_native<N: Native>(&mut self) {
        let id = self.native::<N>();
        self.w.push_native(id)
    }

    pub fn push_native_closure<N: Native>(&mut self, nups: usize, data: u64) -> StackResult<()> {
        let id = self.native::<N>();
        res(self.w.push_native_closure(id, nups, data))
    }

    pub fn closure_data(&self) -> u64 {
        self.w.closure_data()
    }

    pub fn native_data(&self) -> [u64; 2] {
        let Pair { a, b } = self.w.native_data();
        [a, b]
    }

    pub fn push_registered(&mut self, f: NativeRef) {
        self.w.push_registered(f.0)
    }

    // ---- tables ----------------------------------------------------------------------

    pub fn get_table(&mut self, idx: i32) -> StackResult<Type> {
        res(self.w.get_table(idx)).map(type_of)
    }

    pub fn get_field<'k>(&mut self, idx: i32, k: impl Into<OrbitStr<'k>>) -> StackResult<Type> {
        let k = k.into().as_bytes();
        res(self.w.get_field(idx, k.into())).map(type_of)
    }

    pub fn get_index(&mut self, idx: i32, n: i64) -> StackResult<Type> {
        res(self.w.get_index(idx, n)).map(type_of)
    }

    pub fn get_global<'n>(&mut self, name: impl Into<OrbitStr<'n>>) -> StackResult<Type> {
        let name = name.into().as_bytes();
        res(self.w.get_global(name.into())).map(type_of)
    }

    pub fn raw_get(&mut self, idx: i32) -> StackResult<Type> {
        res(self.w.raw_get(idx)).map(type_of)
    }

    pub fn raw_get_index(&mut self, idx: i32, n: i64) -> StackResult<Type> {
        res(self.w.raw_get_index(idx, n)).map(type_of)
    }

    pub fn raw_get_light(&mut self, idx: i32, p: usize) -> StackResult<Type> {
        res(self.w.raw_get_light(idx, p)).map(type_of)
    }

    pub fn raw_get_field<'k>(&mut self, idx: i32, k: impl Into<OrbitStr<'k>>) -> StackResult<Type> {
        let k = k.into().as_bytes();
        res(self.w.raw_get_field(idx, k.into())).map(type_of)
    }

    pub fn create_table(&mut self, narr: i32, nrec: i32) {
        self.w.create_table(narr, nrec)
    }

    pub fn new_userdata(&mut self, size: usize, nuv: i32) -> StackResult<*mut std::ffi::c_void> {
        res(self.w.new_userdata(size, nuv)).map(|p| p as *mut std::ffi::c_void)
    }

    pub fn get_user_value(&mut self, idx: i32, n: i32) -> Type {
        type_of(self.w.get_user_value(idx, n))
    }

    pub fn set_user_value(&mut self, idx: i32, n: i32) -> StackResult<bool> {
        res(self.w.set_user_value(idx, n))
    }

    pub fn get_metatable(&mut self, idx: i32) -> bool {
        self.w.get_metatable(idx)
    }

    pub fn set_table(&mut self, idx: i32) -> StackResult<()> {
        res(self.w.set_table(idx))
    }

    pub fn set_field<'k>(&mut self, idx: i32, k: impl Into<OrbitStr<'k>>) -> StackResult<()> {
        let k = k.into().as_bytes();
        res(self.w.set_field(idx, k.into()))
    }

    pub fn set_index_int(&mut self, idx: i32, n: i64) -> StackResult<()> {
        res(self.w.set_index_int(idx, n))
    }

    pub fn set_global<'n>(&mut self, name: impl Into<OrbitStr<'n>>) -> StackResult<()> {
        let name = name.into().as_bytes();
        res(self.w.set_global(name.into()))
    }

    pub fn raw_set(&mut self, idx: i32) -> StackResult<()> {
        res(self.w.raw_set(idx))
    }

    pub fn raw_set_index(&mut self, idx: i32, n: i64) -> StackResult<()> {
        res(self.w.raw_set_index(idx, n))
    }

    pub fn raw_set_light(&mut self, idx: i32, p: usize) -> StackResult<()> {
        res(self.w.raw_set_light(idx, p))
    }

    pub fn raw_set_field<'k>(&mut self, idx: i32, k: impl Into<OrbitStr<'k>>) -> StackResult<()> {
        let k = k.into().as_bytes();
        res(self.w.raw_set_field(idx, k.into()))
    }

    pub fn set_metatable(&mut self, idx: i32) -> StackResult<()> {
        res(self.w.set_metatable(idx))
    }

    pub fn next(&mut self, idx: i32) -> StackResult<bool> {
        res(self.w.next(idx))
    }

    // ---- calls -----------------------------------------------------------------------

    pub fn check_call(&mut self, nargs: i32) -> StackResult<usize> {
        res(self.w.check_call(nargs))
    }

    pub fn adjust_results(&mut self, n: usize, nresults: i32) {
        self.w.adjust_results(n, nresults)
    }

    pub fn call(&mut self, nargs: i32, nresults: i32) -> StackResult<()> {
        res(self.w.call(nargs, nresults))
    }

    pub fn call_handled(&mut self, nargs: i32, nresults: i32, handler: i32) -> StackResult<()> {
        res(self.w.call_handled(nargs, nresults, handler))
    }

    pub fn pcall(&mut self, nargs: i32, nresults: i32, handler: i32) -> StackResult<Status> {
        res(self.w.pcall(nargs, nresults, handler)).map(status_of)
    }

    pub fn status_of(&self, e: &Raised) -> Status {
        status_of(self.w.status_of(e.0))
    }

    pub fn push_raised(&mut self, e: &Raised) {
        self.w.push_raised(e.0)
    }

    pub fn take_raised(&mut self, e: Raised) -> (String, Option<String>) {
        let t = self.w.take_raised(e.0);
        let traceback: Option<stabby::string::String> = t.traceback.into();
        (t.message.to_string(), traceback.map(|t| t.to_string()))
    }

    pub fn memory_error(&mut self) -> Raised {
        Raised(self.w.memory_error())
    }

    pub fn load<'n, 'm>(
        &mut self,
        bytes: &[u8],
        name: impl Into<OrbitStr<'n>>,
        mode: impl Into<OrbitStr<'m>>,
    ) -> Status {
        let name = name.into().as_bytes();
        let mode = mode.into().as_bytes();
        status_of(self.w.load(bytes.into(), name.into(), mode.into()))
    }

    pub fn load_chunk(&mut self, chunk: &crate::Chunk<'_>, env: i32) -> Status {
        let language = match chunk.language {
            crate::Language::Auto => 0,
            crate::Language::Lua => 1,
            crate::Language::OrbitScript => 2,
        };
        let mode = match chunk.mode {
            crate::Mode::Text => 0,
            crate::Mode::Binary => 1,
            crate::Mode::Both => 2,
        };
        let name = chunk.name.as_deref().map(Slice::from);
        status_of(
            self.w
                .load_chunk(chunk.bytes.into(), name.into(), language, mode, env),
        )
    }

    /// Push the main function of a chunk [`Compiler`](crate::Compiler)
    /// compiled, linked here with the global table as its `_ENV`. It
    /// crosses to the library as a binary chunk.
    pub fn push_compiled(&mut self, chunk: &crate::UnlinkedChunk) {
        let bytes = orbit_compiler::bytecode::format::encode(chunk, false);
        let loaded = self.load_chunk(&crate::Chunk::new(&bytes).mode(crate::Mode::Binary), 0);
        debug_assert_eq!(loaded, Status::Ok, "the compiler's chunks decode");
    }

    // ---- errors ----------------------------------------------------------------------

    pub fn error_from_top(&mut self) -> Raised {
        Raised(self.w.error_from_top())
    }

    pub fn error<'m>(&mut self, message: impl Into<OrbitStr<'m>>) -> Raised {
        Raised(self.w.error(message.into().as_bytes().into()))
    }

    pub fn arg_error<'m>(&mut self, arg: i32, message: impl Into<OrbitStr<'m>>) -> Raised {
        Raised(self.w.arg_error(arg, message.into().as_bytes().into()))
    }

    pub fn type_error(&mut self, arg: i32, expected: &str) -> Raised {
        Raised(self.w.type_error(arg, expected.into()))
    }

    pub fn check_string(&mut self, arg: i32) -> StackResult<OrbitStr<'_>> {
        res(self.w.check_string(arg)).map(str_of)
    }

    pub fn check_integer(&mut self, arg: i32) -> StackResult<i64> {
        res(self.w.check_integer(arg))
    }

    pub fn opt_integer(&mut self, arg: i32, default: i64) -> StackResult<i64> {
        res(self.w.opt_integer(arg, default))
    }

    pub fn check_number(&mut self, arg: i32) -> StackResult<f64> {
        res(self.w.check_number(arg))
    }

    pub fn display(&self, idx: i32) -> OrbitString {
        OrbitString::from(&self.w.display(idx)[..])
    }

    pub fn push_where(&mut self, level: i32) {
        self.w.push_where(level)
    }

    pub fn push_traceback(&mut self, message: Option<OrbitStr<'_>>, level: i32) {
        self.w
            .push_traceback(message.map(|m| Slice::from(m.as_bytes())).into(), level)
    }

    pub fn to_display_string(&mut self, idx: i32) -> StackResult<OrbitStr<'_>> {
        res(self.w.to_display_string(idx)).map(str_of)
    }

    // ---- the isolate -----------------------------------------------------------------

    pub fn gc(&mut self, op: GcOp) -> Option<i64> {
        let (op, param, value) = gc_op(op);
        self.w.gc(op, param, value.into()).into()
    }

    pub fn set_gc_mode(&mut self, mode: crate::GcMode) -> Option<crate::GcMode> {
        let old: Option<u8> = self.w.set_gc_mode(mode as u8).into();
        old.map(|m| {
            if m == 1 {
                crate::GcMode::Generational
            } else {
                crate::GcMode::Incremental
            }
        })
    }

    pub fn warn<'m>(&mut self, msg: impl Into<OrbitStr<'m>>, more: bool) {
        let msg = msg.into().as_bytes();
        self.w.warn(msg.into(), more)
    }

    pub fn set_warn_hook(&mut self, hook: impl FnMut(OrbitStr<'_>, bool) + 'static) {
        self.w.set_warn_hook(SBox::new(WarnObj(hook)).into())
    }

    pub fn open_library(&mut self, lib: Library, ignore_env: bool) -> bool {
        let code = Library::ALL
            .iter()
            .position(|&l| l == lib)
            .expect("every library is in ALL");
        self.w.open_library(code as u8, ignore_env)
    }

    pub fn pin(&mut self, idx: i32) -> u32 {
        self.w.pin(idx)
    }

    pub fn push_pinned(&mut self, slot: u32) {
        self.w.push_pinned(slot)
    }

    pub fn unpin(&mut self, slot: u32) {
        self.w.unpin(slot)
    }

    pub fn with_stdout<R>(&mut self, f: impl FnOnce(&mut dyn std::io::Write) -> R) -> R {
        let mut user = StdoutObj(Some(f), None);
        self.w.with_stdout((&mut user).into());
        user.1.expect("the library runs the closure")
    }

    pub fn before_exit(&mut self, f: impl Fn() + 'static) {
        self.w.before_exit(hook(f))
    }

    pub fn safepoint(&mut self) {
        self.w.safepoint()
    }

    pub fn extension<T: Any + Default, R>(&mut self, f: impl FnOnce(&mut T) -> R) -> R {
        self.host.extension(f)
    }

    pub fn find_extension<T: Any, R>(&self, f: impl FnOnce(&T) -> R) -> Option<R> {
        self.host.find_extension(f)
    }

    pub fn control(&mut self) -> Control<'_> {
        Control::new(self.w.control(), Rc::clone(&self.host))
    }

    // ---- the locale (ADR-13) --------------------------------------------------------

    pub fn set_locale(
        &mut self,
        category: crate::LocaleCategory,
        name: Option<OrbitStr<'_>>,
    ) -> Option<OrbitString> {
        let name: SOption<Slice<'_, u8>> = name.as_ref().map(|n| Slice::from(n.as_bytes())).into();
        bytes(self.w.set_locale(category_code(category), name))
    }

    pub fn decimal_point(&self) -> OrbitString {
        OrbitString::from(&self.w.decimal_point()[..])
    }

    pub fn locale_mode(&self) -> crate::LocaleMode {
        locale_mode_of(self.w.locale_mode())
    }

    pub fn set_locale_mode(&mut self, mode: crate::LocaleMode) {
        self.w.set_locale_mode(locale_mode_code(mode))
    }

    pub fn sync_locale(&mut self) {
        self.w.sync_locale()
    }

    pub fn locale_time_pattern(&self, spec: &[u8]) -> Option<OrbitString> {
        bytes(self.w.locale_time_pattern(spec.into()))
    }

    pub fn locale_time_text(&self, spec: &[u8], tm: &crate::TimeParts) -> Option<OrbitString> {
        bytes(self.w.locale_time_text(spec.into(), tm.into()))
    }

    pub fn enter_native_locale(&mut self) {
        self.w.enter_native_locale()
    }

    pub fn leave_native_locale(&mut self) {
        self.w.leave_native_locale()
    }

    // ---- threads ---------------------------------------------------------------------

    pub fn running_thread(&self) -> ThreadRef {
        self.w.running_thread().into()
    }

    pub fn main_thread(&self) -> ThreadRef {
        self.w.main_thread().into()
    }

    pub fn push_thread(&mut self, t: ThreadRef) {
        self.w.push_thread(t.abi())
    }

    pub fn push_new_thread(&mut self) -> ThreadRef {
        self.w.push_new_thread().into()
    }

    pub fn to_thread(&self, idx: i32) -> Option<ThreadRef> {
        Option::<ThreadAbi>::from(self.w.to_thread(idx)).map(ThreadRef::from)
    }

    pub fn thread_is_live(&self, t: ThreadRef) -> bool {
        self.w.thread_is_live(t.bits)
    }

    pub fn thread_status(&self, t: ThreadRef) -> ThreadStatus {
        match self.w.thread_status(t.bits) {
            0 => ThreadStatus::Suspended,
            1 => ThreadStatus::Running,
            2 => ThreadStatus::Normal,
            _ => ThreadStatus::Dead,
        }
    }

    pub fn thread_yielded(&self, t: ThreadRef) -> bool {
        self.w.thread_yielded(t.bits)
    }

    pub fn thread_failed(&self, t: ThreadRef) -> bool {
        self.w.thread_failed(t.bits)
    }

    pub fn thread_yieldable(&self, t: ThreadRef) -> bool {
        self.w.thread_yieldable(t.bits)
    }

    pub fn enter_thread(&mut self, t: ThreadRef, base: Option<usize>) -> ThreadWindow {
        ThreadWindow(self.w.enter_thread(t.bits, base.into()))
    }

    pub fn leave_thread(&mut self, t: ThreadRef, w: ThreadWindow) {
        self.w.leave_thread(t.bits, w.0)
    }

    pub fn move_values(
        &mut self,
        from: (ThreadRef, Option<usize>),
        to: (ThreadRef, Option<usize>),
        n: usize,
    ) {
        self.w
            .move_values(from.0.bits, from.1.into(), to.0.bits, to.1.into(), n)
    }

    pub fn resume(
        &mut self,
        t: ThreadRef,
        base: Option<usize>,
        nargs: usize,
    ) -> StackResult<Resumed> {
        res(self.w.resume(t.bits, base.into(), nargs)).map(|r| Resumed {
            status: status_of(r.status),
            nresults: r.nresults,
            failed_top: r.failed_top.into(),
        })
    }

    pub fn push_coroutine(&mut self, f: i32) {
        self.w.push_coroutine(f)
    }

    pub fn resume_here(&mut self, idx: i32, nargs: usize) -> StackResult<(bool, usize)> {
        res(self.w.resume_here(idx, nargs)).map(|p| (p.a != 0, p.b as usize))
    }

    pub fn close_thread(&mut self, t: ThreadRef) -> Status {
        status_of(self.w.close_thread(t.bits))
    }

    // ---- debugging -------------------------------------------------------------------

    pub fn has_level(&self, level: i32) -> bool {
        self.w.has_level(level)
    }

    pub fn level_info(&self, level: usize) -> Option<FrameInfo> {
        Option::from(self.w.level_info(level)).map(frame_info)
    }

    pub fn function_info(&self, idx: i32) -> FrameInfo {
        frame_info(self.w.function_info(idx))
    }

    pub fn push_level_function(&mut self, level: usize) {
        self.w.push_level_function(level)
    }

    pub fn get_local(&mut self, level: usize, n: i32) -> Option<OrbitString> {
        bytes(self.w.get_local(level, n))
    }

    pub fn param_name(&mut self, n: i32) -> Option<OrbitString> {
        bytes(self.w.param_name(n))
    }

    pub fn set_local(&mut self, level: usize, n: i32) -> StackResult<Option<OrbitString>> {
        res(self.w.set_local(level, n)).map(bytes)
    }

    pub fn get_upvalue(&mut self, func: i32, n: i32) -> Option<OrbitString> {
        bytes(self.w.get_upvalue(func, n))
    }

    pub fn set_upvalue(&mut self, func: i32, n: i32) -> StackResult<Option<OrbitString>> {
        res(self.w.set_upvalue(func, n)).map(bytes)
    }

    pub fn upvalue_id(&self, func: i32, n: i32) -> usize {
        self.w.upvalue_id(func, n)
    }

    pub fn join_upvalues(&mut self, f1: i32, n1: i32, f2: i32, n2: i32) -> StackResult<()> {
        res(self.w.join_upvalues(f1, n1, f2, n2))
    }
}

// ---- the isolate itself ----------------------------------------------------------

/// Reaches the isolate itself rather than a window: from
/// [`Stack::control`], [`Isolate::control`](crate::Isolate::control), or an
/// [`InterruptHandler`].
pub struct Control<'a> {
    /// Kept from dropping, as [`Stack`]'s window is.
    c: ManuallyDrop<ControlMut<'a>>,
    host: Rc<HostState>,
}

impl std::fmt::Debug for Control<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Control").finish_non_exhaustive()
    }
}

impl<'a> Control<'a> {
    pub(crate) fn new(c: ControlMut<'a>, host: Rc<HostState>) -> Control<'a> {
        Control {
            c: ManuallyDrop::new(c),
            host,
        }
    }

    pub fn extension<T: Any + Default, R>(&mut self, f: impl FnOnce(&mut T) -> R) -> R {
        self.host.extension(f)
    }

    pub fn interrupt_flag(&self) -> *mut bool {
        self.c.interrupt_flag() as *mut bool
    }

    pub fn clear_interrupt(&mut self) {
        self.c.clear_interrupt()
    }

    pub fn set_interrupt_handler<H: InterruptHandler>(&mut self) {
        let h = SBox::new(InterruptObj::<H>(Rc::clone(&self.host), PhantomData)).into();
        self.c.set_interrupt_handler(h)
    }

    pub fn register_native<N: Native>(&mut self, data: [u64; 2]) -> NativeRef {
        // The boundary registers a program native once, then copies it with
        // `data`, so reuse `N`'s first registration as `Stack::native` does.
        let cached = self.host.natives.borrow().get(&TypeId::of::<N>()).copied();
        let id = cached.unwrap_or_else(|| {
            let f = SBox::new(NativeObj::<N>(Rc::clone(&self.host), PhantomData)).into();
            let id = self.c.register_native(N::NAME.into(), f);
            self.host.natives.borrow_mut().insert(TypeId::of::<N>(), id);
            id
        });
        NativeRef(self.c.register_native_data(
            id,
            Pair {
                a: data[0],
                b: data[1],
            },
        ))
    }

    pub fn set_hook(&mut self, f: Option<NativeRef>, mask: u8, count: i64) {
        self.c.set_hook(f.map(|f| f.0).into(), mask, count)
    }

    pub fn publish_main_thread(&mut self) {
        self.c.publish_main_thread()
    }

    pub fn main_thread(&self) -> ThreadRef {
        self.c.main_thread().into()
    }

    /// [`crate::Isolate::set_breakpoints`], also from inside a stop.
    #[cfg(feature = "debugger")]
    pub fn set_breakpoints(
        &mut self,
        source: &[u8],
        lines: &[u32],
    ) -> Vec<crate::debugger::Breakpoint> {
        self.c
            .set_breakpoints(source.into(), lines.into())
            .iter()
            .map(|&b| crate::debugger::abi::breakpoint_from(b))
            .collect()
    }

    /// [`crate::Isolate::stop_on_next_line`].
    #[cfg(feature = "debugger")]
    pub fn stop_on_next_line(&mut self) {
        self.c.stop_on_next_line()
    }

    /// [`crate::Isolate::set_error_break`], also from inside a stop.
    #[cfg(feature = "debugger")]
    pub fn set_error_break(&mut self, on: crate::debugger::ErrorBreak) {
        self.c
            .set_error_break(crate::debugger::abi::error_break_to(on))
    }
}
