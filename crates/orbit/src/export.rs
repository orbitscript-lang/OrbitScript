//! `liborbit`'s half of the boundary (ADR-10): the library's traits in
//! [`crate::abi`] implemented over the runtime, for `orbit-lib` to export.
//!
//! A program's natives and continuations run through trampolines here.
//! Each call hands the program a fresh [`Win`] over the native's window;
//! errors raised during the call live in the `Win` and cross as ids, so
//! they go when the call ends.

use std::rc::Rc;

use orbit_runtime::vm::{self, CallOutcome, LuaError, NativeCtx, NativeReturn};
use stabby::abi::{IDeterminantProvider, StableLike};
use stabby::boxed::Box as SBox;
use stabby::option::Option as SOption;
use stabby::result::Result as SResult;
use stabby::slice::Slice;
use stabby::str::Str;
use stabby::string::String as SString;
use stabby::vec::Vec as SVec;

use crate::abi::*;
use crate::abi::{
    ConfigAbi, ContinuationBox, ControlAbi, ControlMut, EntryMut, FrameInfoAbi, HookBox,
    HostValueBox, InterruptAbi, InterruptArc, InterruptHandlerBox, IsolateAbi, IsolateBox,
    NativeBox, Pair, RaisedId, Res, ResumedAbi, RetAbi, StackAbi, StdoutUserMut, TakenAbi,
    ThreadAbi, WarnHookBox, Writer, WriterBox,
};
use crate::stack::{
    ArithOp, CompareOp, Control, FrameInfo, GcOp, GcParam, Raised, Ret, Return, Stack, StackResult,
    ThreadRef, ThreadWindow,
};
use crate::{
    Chunk, Config, Error, GcMode, Isolate, Language, Library, Mode, Runtime, ThreadStatus,
};

type NativeResult = Result<NativeReturn, LuaError>;

/// The program objects an isolate holds, by the index the program knows
/// them by.
#[derive(Default)]
struct Programs {
    /// A program's natives, by native index.
    natives: Vec<Option<Rc<NativeBox>>>,
    /// Their names, by native index.
    names: Vec<&'static str>,
    /// A program's continuations, by slot.
    continuations: Vec<Rc<ContinuationBox>>,
    interrupt: Option<Rc<InterruptHandlerBox>>,
}

impl Programs {
    fn native(&self, id: u32) -> Option<(Rc<NativeBox>, &'static str)> {
        let f = self.natives.get(id as usize)?.clone()?;
        Some((f, self.names[id as usize]))
    }

    fn set_native(&mut self, id: u32, f: Rc<NativeBox>, name: &'static str) {
        let i = id as usize;
        if self.natives.len() <= i {
            self.natives.resize(i + 1, None);
            self.names.resize(i + 1, "");
        }
        self.natives[i] = Some(f);
        self.names[i] = name;
    }
}

// ---- trampolines -------------------------------------------------------------

/// The runtime native behind every native a program registered.
fn program_native(ctx: &mut NativeCtx<'_>) -> NativeResult {
    let mut s = Stack::new(ctx.reborrow());
    let id = s.running_native();
    let f = s
        .find_extension(|p: &Programs| p.native(id))
        .flatten()
        .expect("program natives are registered through the boundary")
        .0;
    match run_native(&mut s, &f) {
        Ok(r) => s.finish(r),
        Err(Raised(e)) => Err(e),
    }
}

/// Run a program's native in `s`'s window.
fn run_native(s: &mut Stack<'_>, f: &NativeBox) -> StackResult<Return> {
    let mut w = Win::new(s);
    let r = f.call(StableLike::new((&mut w).into()));
    w.ret(r)
}

/// The runtime continuation for slot `I`: a program's continuations get
/// slots in the order they are registered, since continuations are plain
/// functions with no data to tell them apart.
fn continuation_slot<const I: usize>(ctx: &mut NativeCtx<'_>, o: CallOutcome) -> NativeResult {
    let mut s = Stack::new(ctx.reborrow());
    let k = s
        .find_extension(|p: &Programs| p.continuations.get(I).cloned())
        .flatten()
        .expect("program continuations are registered through the boundary");
    let r = {
        let mut w = Win::new(&mut s);
        let (ok, n, raised) = match o {
            CallOutcome::Ok(n) => (true, n, 0),
            CallOutcome::Err(v) => {
                let e = w.s.ctx.error_value(v);
                (false, 0, w.raise(Raised(e)))
            }
        };
        let r = k.resume(StableLike::new((&mut w).into()), ok, n, raised);
        w.ret(r)
    };
    match r {
        Ok(r) => s.finish(r),
        Err(Raised(e)) => Err(e),
    }
}

macro_rules! slots {
    ($($i:literal)*) => {
        /// How many continuations one isolate's programs can register.
        const SLOTS: usize = [$($i),*].len();
        const CONTINUATIONS: [vm::Continuation; SLOTS] = [$(continuation_slot::<$i>),*];
    };
}

slots!(
    0 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20 21 22 23 24 25 26 27 28 29 30 31
    32 33 34 35 36 37 38 39 40 41 42 43 44 45 46 47 48 49 50 51 52 53 54 55 56 57 58 59 60 61
    62 63
);

fn interrupt_trampoline(vm: &mut vm::Vm) -> bool {
    let mut c = Control::new(vm);
    let h = c.extension(|p: &mut Programs| p.interrupt.clone());
    match h {
        Some(h) => h.handle(StableLike::new((&mut LibControl(c)).into())),
        None => false,
    }
}

// ---- conversions -----------------------------------------------------------------

fn type_code(t: crate::stack::Type) -> i32 {
    t as i32
}

fn thread(t: ThreadRef) -> ThreadAbi {
    ThreadAbi {
        bits: t.to_bits(),
        slot: t.slot(),
    }
}

fn opt<T: IDeterminantProvider<()>>(o: Option<T>) -> SOption<T> {
    o.into()
}

fn bytes(v: Option<Vec<u8>>) -> SOption<SVec<u8>> {
    opt(v.map(|v| v.as_slice().into()))
}

fn frame_info(f: FrameInfo) -> FrameInfoAbi {
    FrameInfoAbi {
        name: bytes(f.name),
        namewhat: f.namewhat.into(),
        what: f.what.into(),
        source: f.source.as_slice().into(),
        short_src: f.short_src.as_str().into(),
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

fn gc_op(op: u8, param: u8, value: Option<i64>) -> GcOp {
    match op {
        0 => GcOp::Stop,
        1 => GcOp::Restart,
        2 => GcOp::Collect,
        3 => GcOp::Count,
        4 => GcOp::CountRemainder,
        5 => GcOp::Step(value.unwrap_or(0) as i32),
        6 => GcOp::IsRunning,
        _ => GcOp::Param(
            GcParam::from_code(param).unwrap_or(GcParam::StepSize),
            value,
        ),
    }
}

fn gc_mode(mode: u8) -> GcMode {
    if mode == 1 {
        GcMode::Generational
    } else {
        GcMode::Incremental
    }
}

// ---- a window ----------------------------------------------------------------------

/// A native's window, or the host's, as a program sees it for one call,
/// with the errors raised during the call.
struct Win<'a, 's> {
    s: &'a mut Stack<'s>,
    raised: Vec<Option<LuaError>>,
    windows: Vec<Option<ThreadWindow>>,
}

impl<'a, 's> Win<'a, 's> {
    fn new(s: &'a mut Stack<'s>) -> Win<'a, 's> {
        Win {
            s,
            raised: Vec::new(),
            windows: Vec::new(),
        }
    }

    fn raise(&mut self, e: Raised) -> RaisedId {
        self.raised.push(Some(e.0));
        self.raised.len() as RaisedId
    }

    fn take(&mut self, id: RaisedId) -> Raised {
        let e = (id as usize)
            .checked_sub(1)
            .and_then(|i| self.raised.get_mut(i))
            .and_then(Option::take)
            .expect("an error raised during this call, not yet used");
        Raised(e)
    }

    fn peek(&self, id: RaisedId) -> &LuaError {
        (id as usize)
            .checked_sub(1)
            .and_then(|i| self.raised.get(i))
            .and_then(Option::as_ref)
            .expect("an error raised during this call, not yet used")
    }

    fn res<T: IDeterminantProvider<RaisedId>>(&mut self, r: StackResult<T>) -> Res<T> {
        match r {
            Ok(v) => SResult::Ok(v),
            Err(e) => SResult::Err(self.raise(e)),
        }
    }

    /// What a program's native ended with, in the runtime's terms.
    fn ret(&mut self, r: RetAbi) -> StackResult<Return> {
        let k = || {
            *CONTINUATIONS
                .get(r.k as usize)
                .expect("continuation slots come from the library")
        };
        Ok(Return(match r.kind {
            RetAbi::VALUES => Ret::Values(r.n as usize),
            RetAbi::YIELD => Ret::Yield(r.n as usize),
            RetAbi::YIELD_THEN => Ret::YieldThen(r.n as usize, k()),
            RetAbi::CALL_THEN => Ret::CallThen {
                nargs: r.n as usize,
                protected: r.flag,
                k: k(),
            },
            RetAbi::EXIT => Ret::Exit {
                code: r.n as i32,
                close: r.flag,
            },
            _ => return Err(self.take(r.raised)),
        }))
    }
}

/// Converts a `Slice`-returning result without holding the window past the
/// error branch.
macro_rules! res_slice {
    ($self:ident, $e:expr) => {
        match $e {
            Ok(v) => SResult::Ok(v),
            Err(e) => {
                $self.raised.push(Some(e.0));
                SResult::Err($self.raised.len() as RaisedId)
            }
        }
    };
}

impl StackAbi for Win<'_, '_> {
    extern "C" fn abs_index(&self, idx: i32) -> i32 {
        self.s.abs_index(idx)
    }
    extern "C" fn top(&self) -> i32 {
        self.s.top()
    }
    extern "C" fn window_start(&self) -> usize {
        self.s.window_start()
    }
    extern "C" fn set_top(&mut self, idx: i32) -> Res<()> {
        let r = self.s.set_top(idx);
        self.res(r)
    }
    extern "C" fn to_close(&mut self, idx: i32) -> Res<()> {
        let r = self.s.to_close(idx);
        self.res(r)
    }
    extern "C" fn close_slot(&mut self, idx: i32) -> Res<()> {
        let r = self.s.close_slot(idx);
        self.res(r)
    }
    extern "C" fn close_all(&mut self) -> Res<()> {
        let r = self.s.close_all();
        self.res(r)
    }
    extern "C" fn push_copy(&mut self, idx: i32) {
        self.s.push_copy(idx)
    }
    extern "C" fn rotate(&mut self, idx: i32, n: i32) -> Res<()> {
        let r = self.s.rotate(idx, n);
        self.res(r)
    }
    extern "C" fn copy(&mut self, from: i32, to: i32) -> Res<()> {
        let r = self.s.copy(from, to);
        self.res(r)
    }
    extern "C" fn check_room(&self, n: i32) -> bool {
        self.s.check_room(n)
    }

    extern "C" fn type_at(&self, idx: i32) -> i32 {
        type_code(self.s.type_at(idx))
    }
    extern "C" fn to_number(&self, idx: i32) -> SOption<f64> {
        opt(self.s.to_number(idx))
    }
    extern "C" fn to_integer(&self, idx: i32) -> SOption<i64> {
        opt(self.s.to_integer(idx))
    }
    extern "C" fn is_string(&self, idx: i32) -> bool {
        self.s.is_string(idx)
    }
    extern "C" fn is_integer(&self, idx: i32) -> bool {
        self.s.is_integer(idx)
    }
    extern "C" fn is_userdata(&self, idx: i32) -> bool {
        self.s.is_userdata(idx)
    }
    extern "C" fn is_native_function(&self, idx: i32) -> bool {
        self.s.is_native_function(idx)
    }
    extern "C" fn to_boolean(&self, idx: i32) -> bool {
        self.s.to_boolean(idx)
    }
    extern "C" fn to_str<'a>(&'a mut self, idx: i32) -> Res<SOption<Slice<'a, u8>>> {
        res_slice!(self, self.s.to_str(idx).map(|b| opt(b.map(Slice::from))))
    }
    extern "C" fn string_at<'a>(&'a self, idx: i32) -> SOption<Slice<'a, u8>> {
        opt(self.s.string_at(idx).map(Slice::from))
    }
    extern "C" fn number_text(&self, idx: i32) -> SOption<SString> {
        opt(self.s.number_text(idx).map(|t| t.as_str().into()))
    }
    extern "C" fn raw_len(&self, idx: i32) -> u64 {
        self.s.raw_len(idx)
    }
    extern "C" fn to_userdata(&self, idx: i32) -> usize {
        self.s.to_userdata(idx) as usize
    }
    extern "C" fn host_value_id(&mut self, idx: i32) -> SOption<u64> {
        let id = self
            .s
            .with_userdata::<HeldValue, _>(idx, |h| h.as_ref().map(|h| h.0.id()));
        opt(id.flatten())
    }
    extern "C" fn is_host_userdata(&self, idx: i32) -> bool {
        self.s.is_host_userdata(idx)
    }
    extern "C" fn identity(&self, idx: i32) -> SOption<u64> {
        opt(self.s.identity(idx))
    }
    extern "C" fn to_pointer(&self, idx: i32) -> usize {
        self.s.to_pointer(idx)
    }
    extern "C" fn closure_data_of(&mut self, idx: i32, native: u32) -> SOption<u64> {
        opt(self.s.closure_data_of_index(idx, native))
    }
    extern "C" fn raw_equal(&self, a: i32, b: i32) -> bool {
        self.s.raw_equal(a, b)
    }
    extern "C" fn compare(&mut self, a: i32, b: i32, op: u8) -> Res<bool> {
        let op = CompareOp::from_code(op).unwrap_or(CompareOp::Le);
        let r = self.s.compare(a, b, op);
        self.res(r)
    }

    extern "C" fn arith(&mut self, op: u8) -> Res<()> {
        let op = ArithOp::from_code(op).unwrap_or(ArithOp::BNot);
        let r = self.s.arith(op);
        self.res(r)
    }
    extern "C" fn concat(&mut self, n: i32) -> Res<()> {
        let r = self.s.concat(n);
        self.res(r)
    }
    extern "C" fn len(&mut self, idx: i32) -> Res<()> {
        let r = self.s.len(idx);
        self.res(r)
    }
    extern "C" fn push_numeral<'b>(&mut self, text: Slice<'b, u8>) -> usize {
        self.s.push_numeral(&text)
    }

    extern "C" fn push_nil(&mut self) {
        self.s.push_nil()
    }
    extern "C" fn push_boolean(&mut self, b: bool) {
        self.s.push_boolean(b)
    }
    extern "C" fn push_integer(&mut self, n: i64) {
        self.s.push_integer(n)
    }
    extern "C" fn push_number(&mut self, n: f64) {
        self.s.push_number(n)
    }
    extern "C" fn push_bytes<'a, 'b>(&'a mut self, bytes: Slice<'b, u8>) -> Slice<'a, u8> {
        self.s.push_bytes(&bytes).into()
    }
    extern "C" fn push_light_userdata(&mut self, p: usize) {
        self.s.push_light_userdata(p)
    }
    extern "C" fn push_host_value(&mut self, v: HostValueBox) {
        self.s.push_userdata(HeldValue(v))
    }
    extern "C" fn reserve_memory(&mut self, bytes: usize) -> Res<()> {
        let r = self.s.reserve_memory(bytes);
        self.res(r)
    }
    extern "C" fn push_function(&mut self, f: NativeBox) {
        self.s.push_function(move |s| run_native(s, &f))
    }
    extern "C" fn register_native(&mut self, name: Str<'static>, f: NativeBox) -> u32 {
        let name = name.as_str();
        let id = self.s.add_native(name, program_native);
        let f = Rc::new(f);
        self.s
            .extension(|p: &mut Programs| p.set_native(id, f, name));
        id
    }
    extern "C" fn push_native(&mut self, native: u32) {
        self.s.push_native_index(native)
    }
    extern "C" fn push_native_closure(&mut self, native: u32, nups: usize, data: u64) -> Res<()> {
        let r = self.s.push_closure_of(native, nups, data);
        self.res(r)
    }
    extern "C" fn register_continuation(&mut self, k: ContinuationBox) -> u32 {
        let k = Rc::new(k);
        self.s.extension(|p: &mut Programs| {
            assert!(
                p.continuations.len() < SLOTS,
                "a program registered more than {SLOTS} continuation types in one isolate"
            );
            p.continuations.push(k);
            p.continuations.len() as u32 - 1
        })
    }
    extern "C" fn closure_data(&self) -> u64 {
        self.s.closure_data()
    }
    extern "C" fn native_data(&self) -> Pair {
        let [a, b] = self.s.native_data();
        Pair { a, b }
    }
    extern "C" fn push_registered(&mut self, native: u32) {
        self.s.push_native_index(native)
    }

    extern "C" fn get_table(&mut self, idx: i32) -> Res<i32> {
        let r = self.s.get_table(idx).map(type_code);
        self.res(r)
    }
    extern "C" fn get_field<'b>(&mut self, idx: i32, k: Slice<'b, u8>) -> Res<i32> {
        let r = self.s.get_field(idx, &k).map(type_code);
        self.res(r)
    }
    extern "C" fn get_index(&mut self, idx: i32, n: i64) -> Res<i32> {
        let r = self.s.get_index(idx, n).map(type_code);
        self.res(r)
    }
    extern "C" fn get_global<'b>(&mut self, name: Slice<'b, u8>) -> Res<i32> {
        let r = self.s.get_global(&name).map(type_code);
        self.res(r)
    }
    extern "C" fn raw_get(&mut self, idx: i32) -> Res<i32> {
        let r = self.s.raw_get(idx).map(type_code);
        self.res(r)
    }
    extern "C" fn raw_get_index(&mut self, idx: i32, n: i64) -> Res<i32> {
        let r = self.s.raw_get_index(idx, n).map(type_code);
        self.res(r)
    }
    extern "C" fn raw_get_light(&mut self, idx: i32, p: usize) -> Res<i32> {
        let r = self.s.raw_get_light(idx, p).map(type_code);
        self.res(r)
    }
    extern "C" fn raw_get_field<'b>(&mut self, idx: i32, k: Slice<'b, u8>) -> Res<i32> {
        let r = self.s.raw_get_field(idx, &k).map(type_code);
        self.res(r)
    }
    extern "C" fn create_table(&mut self, narr: i32, nrec: i32) {
        self.s.create_table(narr, nrec)
    }
    extern "C" fn new_userdata(&mut self, size: usize, nuv: i32) -> Res<usize> {
        let r = self.s.new_userdata(size, nuv).map(|p| p as usize);
        self.res(r)
    }
    extern "C" fn get_user_value(&mut self, idx: i32, n: i32) -> i32 {
        type_code(self.s.get_user_value(idx, n))
    }
    extern "C" fn set_user_value(&mut self, idx: i32, n: i32) -> Res<bool> {
        let r = self.s.set_user_value(idx, n);
        self.res(r)
    }
    extern "C" fn get_metatable(&mut self, idx: i32) -> bool {
        self.s.get_metatable(idx)
    }
    extern "C" fn set_table(&mut self, idx: i32) -> Res<()> {
        let r = self.s.set_table(idx);
        self.res(r)
    }
    extern "C" fn set_field<'b>(&mut self, idx: i32, k: Slice<'b, u8>) -> Res<()> {
        let r = self.s.set_field(idx, &k);
        self.res(r)
    }
    extern "C" fn set_index_int(&mut self, idx: i32, n: i64) -> Res<()> {
        let r = self.s.set_index_int(idx, n);
        self.res(r)
    }
    extern "C" fn set_global<'b>(&mut self, name: Slice<'b, u8>) -> Res<()> {
        let r = self.s.set_global(&name);
        self.res(r)
    }
    extern "C" fn raw_set(&mut self, idx: i32) -> Res<()> {
        let r = self.s.raw_set(idx);
        self.res(r)
    }
    extern "C" fn raw_set_index(&mut self, idx: i32, n: i64) -> Res<()> {
        let r = self.s.raw_set_index(idx, n);
        self.res(r)
    }
    extern "C" fn raw_set_light(&mut self, idx: i32, p: usize) -> Res<()> {
        let r = self.s.raw_set_light(idx, p);
        self.res(r)
    }
    extern "C" fn raw_set_field<'b>(&mut self, idx: i32, k: Slice<'b, u8>) -> Res<()> {
        let r = self.s.raw_set_field(idx, &k);
        self.res(r)
    }
    extern "C" fn set_metatable(&mut self, idx: i32) -> Res<()> {
        let r = self.s.set_metatable(idx);
        self.res(r)
    }
    extern "C" fn next(&mut self, idx: i32) -> Res<bool> {
        let r = self.s.next(idx);
        self.res(r)
    }

    extern "C" fn check_call(&mut self, nargs: i32) -> Res<usize> {
        let r = self.s.check_call(nargs);
        self.res(r)
    }
    extern "C" fn adjust_results(&mut self, n: usize, nresults: i32) {
        self.s.adjust_results(n, nresults)
    }
    extern "C" fn call(&mut self, nargs: i32, nresults: i32) -> Res<()> {
        let r = self.s.call(nargs, nresults);
        self.res(r)
    }
    extern "C" fn call_handled(&mut self, nargs: i32, nresults: i32, handler: i32) -> Res<()> {
        let r = self.s.call_handled(nargs, nresults, handler);
        self.res(r)
    }
    extern "C" fn pcall(&mut self, nargs: i32, nresults: i32, handler: i32) -> Res<i32> {
        let r = self.s.pcall(nargs, nresults, handler).map(|s| s as i32);
        self.res(r)
    }
    extern "C" fn status_of(&self, e: RaisedId) -> i32 {
        self.s.status_of(&Raised(self.peek(e).clone())) as i32
    }
    extern "C" fn push_raised(&mut self, e: RaisedId) {
        // Pushing leaves the error usable.
        let e = Raised(self.peek(e).clone());
        self.s.push_raised(&e);
    }
    extern "C" fn take_raised(&mut self, e: RaisedId) -> TakenAbi {
        let e = self.take(e);
        let (message, traceback) = self.s.take_raised(e);
        TakenAbi {
            message: message.as_str().into(),
            traceback: opt(traceback.map(|t| t.as_str().into())),
        }
    }
    extern "C" fn memory_error(&mut self) -> RaisedId {
        let e = self.s.memory_error();
        self.raise(e)
    }
    extern "C" fn load<'b>(
        &mut self,
        bytes: Slice<'b, u8>,
        name: Slice<'b, u8>,
        mode: Slice<'b, u8>,
    ) -> i32 {
        self.s.load(&bytes, &name, &mode) as i32
    }
    extern "C" fn load_chunk<'b>(
        &mut self,
        bytes: Slice<'b, u8>,
        name: SOption<Slice<'b, u8>>,
        language: u8,
        mode: u8,
        env: i32,
    ) -> i32 {
        let mut chunk = Chunk::new(&*bytes)
            .language(match language {
                1 => Language::Lua,
                2 => Language::OrbitScript,
                _ => Language::Auto,
            })
            .mode(match mode {
                0 => Mode::Text,
                1 => Mode::Binary,
                _ => Mode::Both,
            });
        if let Some(name) = Option::<Slice<'b, u8>>::from(name) {
            chunk = chunk.name(&*name);
        }
        self.s.load_chunk(&chunk, env) as i32
    }

    extern "C" fn error_from_top(&mut self) -> RaisedId {
        let e = self.s.error_from_top();
        self.raise(e)
    }
    extern "C" fn error<'b>(&mut self, message: Str<'b>) -> RaisedId {
        let e = self.s.error(message.as_str());
        self.raise(e)
    }
    extern "C" fn arg_error<'b>(&mut self, arg: i32, message: Str<'b>) -> RaisedId {
        let e = self.s.arg_error(arg, message.as_str());
        self.raise(e)
    }
    extern "C" fn type_error<'b>(&mut self, arg: i32, expected: Str<'b>) -> RaisedId {
        let e = self.s.type_error(arg, expected.as_str());
        self.raise(e)
    }
    extern "C" fn check_string<'a>(&'a mut self, arg: i32) -> Res<Slice<'a, u8>> {
        res_slice!(self, self.s.check_string(arg).map(Slice::from))
    }
    extern "C" fn check_integer(&mut self, arg: i32) -> Res<i64> {
        let r = self.s.check_integer(arg);
        self.res(r)
    }
    extern "C" fn opt_integer(&mut self, arg: i32, default: i64) -> Res<i64> {
        let r = self.s.opt_integer(arg, default);
        self.res(r)
    }
    extern "C" fn check_number(&mut self, arg: i32) -> Res<f64> {
        let r = self.s.check_number(arg);
        self.res(r)
    }
    extern "C" fn display(&self, idx: i32) -> SVec<u8> {
        self.s.display(idx).as_slice().into()
    }
    extern "C" fn push_where(&mut self, level: i32) {
        self.s.push_where(level)
    }
    extern "C" fn push_traceback<'b>(&mut self, message: SOption<Slice<'b, u8>>, level: i32) {
        let message = Option::<Slice<'b, u8>>::from(message);
        self.s.push_traceback(message.as_deref(), level)
    }
    extern "C" fn to_display_string<'a>(&'a mut self, idx: i32) -> Res<Slice<'a, u8>> {
        res_slice!(self, self.s.to_display_string(idx).map(Slice::from))
    }

    extern "C" fn gc(&mut self, op: u8, param: u8, value: SOption<i64>) -> SOption<i64> {
        opt(self.s.gc(gc_op(op, param, value.into())))
    }
    extern "C" fn set_gc_mode(&mut self, mode: u8) -> SOption<u8> {
        opt(self.s.set_gc_mode(gc_mode(mode)).map(|m| m as u8))
    }
    extern "C" fn warn<'b>(&mut self, message: Slice<'b, u8>, more: bool) {
        self.s.warn(&message, more)
    }
    extern "C" fn set_warn_hook(&mut self, mut hook: WarnHookBox) {
        self.s
            .set_warn_hook(move |m: &[u8], more| hook.warn(m.into(), more))
    }
    extern "C" fn open_library(&mut self, lib: u8, ignore_env: bool) -> bool {
        match Library::ALL.get(lib as usize) {
            Some(&lib) => self.s.open_library(lib, ignore_env),
            None => false,
        }
    }
    extern "C" fn pin(&mut self, idx: i32) -> u32 {
        self.s.pin(idx)
    }
    extern "C" fn push_pinned(&mut self, slot: u32) {
        self.s.push_pinned(slot)
    }
    extern "C" fn unpin(&mut self, slot: u32) {
        self.s.unpin(slot)
    }
    extern "C" fn with_stdout<'u>(&mut self, mut user: StdoutUserMut<'u>) {
        self.s.with_stdout(|out| {
            let mut w = LibWriter(out);
            user.run((&mut w).into())
        })
    }
    extern "C" fn before_exit(&mut self, hook: HookBox) {
        self.s.before_exit(move || hook.run())
    }
    extern "C" fn safepoint(&mut self) {
        self.s.safepoint()
    }
    extern "C" fn control<'a>(&'a mut self) -> ControlMut<'a> {
        self.into()
    }

    extern "C" fn running_thread(&self) -> ThreadAbi {
        thread(self.s.running_thread())
    }
    extern "C" fn main_thread(&self) -> ThreadAbi {
        thread(self.s.main_thread())
    }
    extern "C" fn push_thread(&mut self, t: ThreadAbi) {
        self.s.push_thread(ThreadRef::from_bits(t.bits))
    }
    extern "C" fn push_new_thread(&mut self) -> ThreadAbi {
        thread(self.s.push_new_thread())
    }
    extern "C" fn to_thread(&self, idx: i32) -> SOption<ThreadAbi> {
        opt(self.s.to_thread(idx).map(thread))
    }
    extern "C" fn thread_is_live(&self, t: u64) -> bool {
        self.s.thread_is_live(ThreadRef::from_bits(t))
    }
    extern "C" fn thread_status(&self, t: u64) -> u8 {
        match self.s.thread_status(ThreadRef::from_bits(t)) {
            ThreadStatus::Suspended => 0,
            ThreadStatus::Running => 1,
            ThreadStatus::Normal => 2,
            ThreadStatus::Dead => 3,
        }
    }
    extern "C" fn thread_yielded(&self, t: u64) -> bool {
        self.s.thread_yielded(ThreadRef::from_bits(t))
    }
    extern "C" fn thread_failed(&self, t: u64) -> bool {
        self.s.thread_failed(ThreadRef::from_bits(t))
    }
    extern "C" fn thread_yieldable(&self, t: u64) -> bool {
        self.s.thread_yieldable(ThreadRef::from_bits(t))
    }
    extern "C" fn enter_thread(&mut self, t: u64, base: SOption<usize>) -> u64 {
        let w = self.s.enter_thread(ThreadRef::from_bits(t), base.into());
        self.windows.push(Some(w));
        self.windows.len() as u64
    }
    extern "C" fn leave_thread(&mut self, t: u64, window: u64) {
        let w = (window as usize)
            .checked_sub(1)
            .and_then(|i| self.windows.get_mut(i))
            .and_then(Option::take)
            .expect("a thread window entered during this call");
        self.s.leave_thread(ThreadRef::from_bits(t), w)
    }
    extern "C" fn move_values(
        &mut self,
        from: u64,
        from_base: SOption<usize>,
        to: u64,
        to_base: SOption<usize>,
        n: usize,
    ) {
        self.s.move_values(
            (ThreadRef::from_bits(from), from_base.into()),
            (ThreadRef::from_bits(to), to_base.into()),
            n,
        )
    }
    extern "C" fn resume(&mut self, t: u64, base: SOption<usize>, nargs: usize) -> Res<ResumedAbi> {
        let r = self
            .s
            .resume(ThreadRef::from_bits(t), base.into(), nargs)
            .map(|r| ResumedAbi {
                status: r.status as i32,
                nresults: r.nresults,
                failed_top: opt(r.failed_top),
            });
        self.res(r)
    }
    extern "C" fn push_coroutine(&mut self, f: i32) {
        self.s.push_coroutine(f)
    }
    extern "C" fn resume_here(&mut self, idx: i32, nargs: usize) -> Res<Pair> {
        let r = self.s.resume_here(idx, nargs).map(|(done, n)| Pair {
            a: done as u64,
            b: n as u64,
        });
        self.res(r)
    }
    extern "C" fn close_thread(&mut self, t: u64) -> i32 {
        self.s.close_thread(ThreadRef::from_bits(t)) as i32
    }

    extern "C" fn has_level(&self, level: i32) -> bool {
        self.s.has_level(level)
    }
    extern "C" fn level_info(&self, level: usize) -> SOption<FrameInfoAbi> {
        opt(self.s.level_info(level).map(frame_info))
    }
    extern "C" fn function_info(&self, idx: i32) -> FrameInfoAbi {
        frame_info(self.s.function_info(idx))
    }
    extern "C" fn push_level_function(&mut self, level: usize) {
        self.s.push_level_function(level)
    }
    extern "C" fn get_local(&mut self, level: usize, n: i32) -> SOption<SVec<u8>> {
        bytes(self.s.get_local(level, n))
    }
    extern "C" fn param_name(&mut self, n: i32) -> SOption<SVec<u8>> {
        bytes(self.s.param_name(n))
    }
    extern "C" fn set_local(&mut self, level: usize, n: i32) -> Res<SOption<SVec<u8>>> {
        let r = self.s.set_local(level, n).map(bytes);
        self.res(r)
    }
    extern "C" fn get_upvalue(&mut self, func: i32, n: i32) -> SOption<SVec<u8>> {
        bytes(self.s.get_upvalue(func, n))
    }
    extern "C" fn set_upvalue(&mut self, func: i32, n: i32) -> Res<SOption<SVec<u8>>> {
        let r = self.s.set_upvalue(func, n).map(bytes);
        self.res(r)
    }
    extern "C" fn upvalue_id(&self, func: i32, n: i32) -> usize {
        self.s.upvalue_id(func, n)
    }
    extern "C" fn join_upvalues(&mut self, f1: i32, n1: i32, f2: i32, n2: i32) -> Res<()> {
        let r = self.s.join_upvalues(f1, n1, f2, n2);
        self.res(r)
    }
}

/// A program's host value, as a userdata holds it.
struct HeldValue(HostValueBox);

// ---- the isolate itself ---------------------------------------------------------

/// Control over the isolate, from a window, an isolate or an interrupt.
macro_rules! control_abi {
    ([$($g:tt)*] $ty:ty, |$this:ident| $ctl:expr, |$p:ident| $peek:expr) => {
        impl<$($g)*> ControlAbi for $ty {
            extern "C" fn interrupt_flag(&self) -> usize {
                let $p = self;
                $peek.interrupt_flag() as usize
            }
            extern "C" fn clear_interrupt(&mut self) {
                let $this = self;
                $ctl.clear_interrupt()
            }
            extern "C" fn set_interrupt_handler(&mut self, h: InterruptHandlerBox) {
                let $this = self;
                let mut c = $ctl;
                c.extension(|p: &mut Programs| p.interrupt = Some(Rc::new(h)));
                c.vm.rt.interrupt_handler = Some(interrupt_trampoline);
            }
            extern "C" fn register_native(&mut self, name: Str<'static>, f: NativeBox) -> u32 {
                let $this = self;
                let mut c = $ctl;
                let name = name.as_str();
                let id = c.register_native_fn(name, program_native, [0; 2]);
                let f = Rc::new(f);
                c.extension(|p: &mut Programs| p.set_native(id, f, name));
                id
            }
            extern "C" fn register_native_data(&mut self, native: u32, data: Pair) -> u32 {
                let $this = self;
                let mut c = $ctl;
                let (f, name) = c
                    .extension(|p: &mut Programs| p.native(native))
                    .expect("program natives are registered through the boundary");
                let id = c.register_native_fn(name, program_native, [data.a, data.b]);
                c.extension(|p: &mut Programs| p.set_native(id, f, name));
                id
            }
            extern "C" fn set_hook(&mut self, f: SOption<u32>, mask: u8, count: i64) {
                let $this = self;
                let f = Option::<u32>::from(f).map(crate::stack::NativeRef::from_index);
                $ctl.set_hook(f, mask, count)
            }
            extern "C" fn publish_main_thread(&mut self) {
                let $this = self;
                $ctl.publish_main_thread()
            }
            extern "C" fn main_thread(&self) -> ThreadAbi {
                let $p = self;
                thread($peek.main_thread())
            }
            extern "C" fn set_breakpoints<'b>(
                &mut self,
                source: Slice<'b, u8>,
                lines: Slice<'b, u32>,
            ) -> SVec<BreakpointAbi> {
                let $this = self;
                #[cfg(feature = "debugger")]
                let bps = $ctl
                    .set_breakpoints(&source, &lines)
                    .into_iter()
                    .map(crate::debugger::abi::breakpoint_to)
                    .collect();
                #[cfg(not(feature = "debugger"))]
                let bps = {
                    let _ = (&$this, source);
                    lines
                        .iter()
                        .map(|&requested| BreakpointAbi { requested, line: 0 })
                        .collect()
                };
                bps
            }
            extern "C" fn set_error_break(&mut self, on: u8) {
                let $this = self;
                #[cfg(feature = "debugger")]
                $ctl.set_error_break(crate::debugger::abi::error_break_from(on));
                #[cfg(not(feature = "debugger"))]
                let _ = (&$this, on);
            }
            extern "C" fn stop_on_next_line(&mut self) {
                let $this = self;
                #[cfg(feature = "debugger")]
                $ctl.stop_on_next_line();
                #[cfg(not(feature = "debugger"))]
                let _ = &$this;
            }
        }
    };
}

/// [`Control`], for an interrupt handler.
struct LibControl<'a>(Control<'a>);

control_abi!([] Win<'_, '_>, |w| w.s.control(), |w| w.s);
control_abi!([] LibControl<'_>, |c| c.0.reborrow(), |c| c.0);

/// Writes to the isolate's standard output for a program.
struct LibWriter<'a>(&'a mut dyn std::io::Write);

impl Writer for LibWriter<'_> {
    extern "C" fn write<'b>(&mut self, bytes: Slice<'b, u8>) -> SResult<usize, SString> {
        match self.0.write(&bytes) {
            Ok(n) => SResult::Ok(n),
            Err(e) => SResult::Err(e.to_string().as_str().into()),
        }
    }
    extern "C" fn flush(&mut self) -> SResult<(), SString> {
        match self.0.flush() {
            Ok(()) => SResult::Ok(()),
            Err(e) => SResult::Err(e.to_string().as_str().into()),
        }
    }
}

/// A program's stream as the isolate's standard output.
struct ProgramOut(WriterBox);

impl std::fmt::Debug for ProgramOut {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ProgramOut")
    }
}

impl std::io::Write for ProgramOut {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        Result::from(self.0.write(buf.into()))
            .map_err(|m: SString| std::io::Error::other(m.to_string()))
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Result::from(self.0.flush()).map_err(|m: SString| std::io::Error::other(m.to_string()))
    }
}

/// [`crate::Interrupt`], for any thread.
struct LibInterrupt(crate::Interrupt);

impl InterruptAbi for LibInterrupt {
    extern "C" fn request(&self) {
        self.0.request()
    }
    extern "C" fn pending(&self) -> bool {
        self.0.pending()
    }
}

/// The libraries `liborbit` carries above the API (`orbit-std`, the C API,
/// `ffi`), which `orbit-lib` installs for a program on the shared backend.
pub trait Bundled: 'static {
    fn install_std(
        iso: &mut Isolate,
        base: bool,
        io: bool,
        os: bool,
        ignore_env: bool,
    ) -> Result<(), Error>;
    fn install_c_modules(iso: &mut Isolate, lock_cpath: bool);
    /// False if the library was built without `ffi`.
    fn install_ffi(iso: &mut Isolate) -> bool;
}

/// An isolate, as a program on the shared backend holds it.
struct IsoObj<B> {
    iso: Isolate,
    bundled: std::marker::PhantomData<B>,
}

control_abi!([B: Bundled] IsoObj<B>, |o| o.iso.control(), |o| o.iso);

impl<B: Bundled> IsolateAbi for IsoObj<B> {
    extern "C" fn enter<'e>(&mut self, mut e: EntryMut<'e>) {
        self.iso.stack(|s| {
            let mut w = Win::new(s);
            e.run(StableLike::new((&mut w).into()))
        })
    }
    extern "C" fn control<'a>(&'a mut self) -> ControlMut<'a> {
        self.into()
    }
    extern "C" fn set_stdout(&mut self, out: WriterBox) {
        self.iso.set_stdout(ProgramOut(out))
    }
    extern "C" fn gc_collect(&mut self) {
        self.iso.gc().collect()
    }
    extern "C" fn gc_used_bytes(&self) -> usize {
        self.iso.used_bytes()
    }
    extern "C" fn gc_set_mode(&mut self, mode: u8) {
        self.iso.gc().set_mode(gc_mode(mode))
    }
    extern "C" fn gc_set_limit(&mut self, bytes: SOption<usize>) {
        self.iso.gc().set_limit(bytes.into())
    }
    extern "C" fn gc_limit(&self) -> SOption<usize> {
        opt(self.iso.memory_limit())
    }
    extern "C" fn gc_set_stress(&mut self, on: bool) {
        self.iso.gc().set_stress(on)
    }
    extern "C" fn interrupt(&self) -> InterruptArc {
        stabby::sync::Arc::new(LibInterrupt(self.iso.interrupt())).into()
    }
    extern "C" fn flush(&mut self) {
        self.iso.flush()
    }
    extern "C" fn set_binary_checks(&mut self, level: u8) {
        #[cfg(feature = "verify")]
        self.iso.set_binary_checks(match level {
            0 => None,
            1 => Some(crate::BinaryChecks::Structure),
            _ => Some(crate::BinaryChecks::Full),
        });
        #[cfg(not(feature = "verify"))]
        let _ = level;
    }
    extern "C" fn before_exit(&mut self, hook: HookBox) {
        self.iso.before_exit(move || hook.run())
    }
    extern "C" fn install_std(
        &mut self,
        base: bool,
        io: bool,
        os: bool,
        ignore_env: bool,
    ) -> SResult<(), SString> {
        match B::install_std(&mut self.iso, base, io, os, ignore_env) {
            Ok(()) => SResult::Ok(()),
            Err(e) => SResult::Err(e.message().into()),
        }
    }
    extern "C" fn install_c_modules(&mut self, lock_cpath: bool) {
        B::install_c_modules(&mut self.iso, lock_cpath)
    }
    extern "C" fn install_ffi(&mut self) -> bool {
        B::install_ffi(&mut self.iso)
    }
    #[cfg(feature = "debugger")]
    extern "C" fn set_debug_events(&mut self, events: SOption<DebugEventsBox>) -> bool {
        let events = Option::<DebugEventsBox>::from(events)
            .map(|e| Box::new(ProgramEvents(e)) as Box<dyn crate::debugger::DebugEvents>);
        self.iso.set_debug_events(events).is_ok()
    }
    #[cfg(not(feature = "debugger"))]
    extern "C" fn set_debug_events(&mut self, _events: SOption<DebugEventsBox>) -> bool {
        false
    }
    extern "C" fn pause_handle(&self) -> SOption<PauseArc> {
        #[cfg(feature = "debugger")]
        if self.iso.debugger_allowed() {
            let arc: PauseArc = stabby::sync::Arc::new(LibPause(self.iso.pause_handle())).into();
            return SOption::Some(arc);
        }
        SOption::None()
    }
}

/// [`crate::debugger::PauseHandle`], for any thread.
#[cfg(feature = "debugger")]
struct LibPause(crate::debugger::PauseHandle);

#[cfg(feature = "debugger")]
impl PauseAbi for LibPause {
    extern "C" fn request(&self) {
        self.0.request()
    }
}

/// A program's debug events, as the isolate reports to them.
#[cfg(feature = "debugger")]
struct ProgramEvents(DebugEventsBox);

#[cfg(feature = "debugger")]
impl crate::debugger::DebugEvents for ProgramEvents {
    fn stopped(
        &mut self,
        paused: &mut crate::debugger::Paused<'_, '_>,
        reason: crate::debugger::StopReason,
    ) -> crate::debugger::Resume {
        use crate::debugger::abi::{resume_from, stop_to};
        // The error, for an error stop, is already in slot 1.
        let mut w = Win::new(paused.stack());
        let code = self
            .0
            .stopped(StableLike::new((&mut w).into()), stop_to(reason));
        resume_from(code)
    }

    fn breakpoints_resolved(&mut self, source: &[u8], bps: &[crate::debugger::Breakpoint]) {
        let bps: Vec<BreakpointAbi> = bps
            .iter()
            .map(|&b| crate::debugger::abi::breakpoint_to(b))
            .collect();
        self.0.breakpoints_resolved(source.into(), bps[..].into())
    }
}

/// A new isolate set up as `config` says, for a program on the shared
/// backend; `orbit-lib` exports this.
pub fn new_isolate<B: Bundled>(config: ConfigAbi) -> IsolateBox {
    let libraries: Vec<Library> = Library::ALL
        .iter()
        .enumerate()
        .filter(|(i, _)| config.libraries & (1 << i) != 0)
        .map(|(_, &l)| l)
        .collect();
    let config = Config::default()
        .libraries(&libraries)
        .memory_limit(config.memory_limit.into())
        .gc_mode(gc_mode(config.gc_mode))
        .warnings(config.warnings)
        .ignore_env(config.ignore_env)
        .debugger(if config.debugger {
            crate::Debugger::Allowed
        } else {
            crate::Debugger::Disabled
        });
    let iso = Runtime::new().new_isolate(config);
    SBox::new(IsoObj::<B> {
        iso,
        bundled: std::marker::PhantomData,
    })
    .into()
}

/// Whether a program built against the boundary with hash `hash` can use
/// this library; `orbit-lib` exports this, and [`Runtime::new`] calls it.
pub fn check(hash: u64) -> SResult<(), SString> {
    if hash == INTERFACE_HASH {
        return SResult::Ok(());
    }
    let m = format!(
        "this program was built for a different liborbit (interface {hash:016x}, library {INTERFACE_HASH:016x})"
    );
    SResult::Err(m.as_str().into())
}
