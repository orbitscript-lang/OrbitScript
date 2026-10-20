//! The boundary between a program and `liborbit` (ADR-10, ADR-11 phase e),
//! declared with `stabby`: the stack API, the isolate and the objects a
//! program hands the library, as trait objects whose layout both sides
//! agree on without `unsafe` in our code.
//!
//! Both sides compile this file: `liborbit` implements the library's traits
//! over the runtime (`export`), and a program built with the `shared`
//! feature implements the program's traits and calls the library's (the
//! `shared` backends of `stack` and `isolate`). stabby checks method names
//! and order when the library hands over its first object; the build
//! script's hash of this file covers the signatures, and [`Runtime::new`]
//! compares it with the library's.
//!
//! Values that need the runtime to mean anything cross as numbers:
//! errors as ids into the library's table of raised errors, threads as
//! heap handles, natives as their registry index, enums as their codes.
//!
//! [`Runtime::new`]: crate::Runtime::new

use stabby::abi::StableLike;
use stabby::boxed::Box as SBox;
use stabby::dynptr;
use stabby::option::Option as SOption;
use stabby::result::Result as SResult;
use stabby::slice::Slice;
use stabby::str::Str;
use stabby::string::String as SString;
use stabby::tuple::Tuple2;
use stabby::vec::Vec as SVec;

include!(concat!(env!("OUT_DIR"), "/abi_hash.rs"));

/// An error raised in the library: an id in its table of raised errors,
/// valid until the native call or host entry that raised it ends.
pub type RaisedId = u64;

/// The result of a stack operation that can raise.
pub type Res<T> = SResult<T, RaisedId>;

/// How a native ends ([`crate::stack::Return`], or an error).
#[stabby::stabby]
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct RetAbi {
    /// [`RetAbi::VALUES`] and the other kinds below.
    pub kind: u8,
    /// `protected` for a call, `close` for an exit.
    pub flag: bool,
    /// The continuation's slot ([`StackAbi::register_continuation`]).
    pub k: u32,
    /// How many values (or arguments), or the exit status.
    pub n: u64,
    /// The error, for [`RetAbi::RAISE`].
    pub raised: RaisedId,
}

impl RetAbi {
    pub const VALUES: u8 = 0;
    pub const YIELD: u8 = 1;
    pub const YIELD_THEN: u8 = 2;
    pub const CALL_THEN: u8 = 3;
    pub const EXIT: u8 = 4;
    pub const RAISE: u8 = 5;
}

/// A thread: its heap handle, and the heap slot it occupies.
#[stabby::stabby]
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct ThreadAbi {
    pub bits: u64,
    pub slot: u32,
}

/// Two words: a native's data, or the outcome of a resume here.
#[stabby::stabby]
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct Pair {
    pub a: u64,
    pub b: u64,
}

/// [`crate::stack::Resumed`].
#[stabby::stabby]
#[repr(C)]
pub struct ResumedAbi {
    pub status: i32,
    pub nresults: usize,
    pub failed_top: SOption<usize>,
}

/// An error's message and traceback ([`crate::Stack::take_raised`]).
#[stabby::stabby]
#[repr(C)]
pub struct TakenAbi {
    pub message: SString,
    pub traceback: SOption<SString>,
}

/// [`crate::stack::FrameInfo`].
#[stabby::stabby]
#[repr(C)]
pub struct FrameInfoAbi {
    pub name: SOption<SVec<u8>>,
    pub namewhat: Str<'static>,
    pub what: Str<'static>,
    pub source: SVec<u8>,
    pub short_src: SString,
    pub currentline: i32,
    pub linedefined: i32,
    pub lastlinedefined: i32,
    pub nups: u8,
    pub nparams: u8,
    pub isvararg: bool,
    pub extraargs: u8,
    pub istailcall: bool,
    pub defined_name: SOption<SVec<u8>>,
}

/// [`crate::Config`]: libraries as a bit per [`crate::Library::ALL`]
/// entry.
#[stabby::stabby]
#[repr(C)]
pub struct ConfigAbi {
    pub memory_limit: SOption<usize>,
    pub libraries: u16,
    pub gc_mode: u8,
    pub warnings: bool,
    pub ignore_env: bool,
    /// Whether a debugger may attach ([`crate::Debugger::Allowed`]).
    pub debugger: bool,
}

/// A breakpoint ([`crate::debugger::Breakpoint`]); `line` 0 while it
/// isn't resolved, since lines count from 1.
#[stabby::stabby]
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct BreakpointAbi {
    pub requested: u32,
    pub line: u32,
}

/// Why the program stopped ([`crate::debugger::StopReason`]): `kind` is
/// [`StopAbi::BREAKPOINT`] (at `line`) or one of the others below. For an
/// error, the window's slot 1 holds the error.
#[stabby::stabby]
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct StopAbi {
    pub kind: u8,
    pub caught: bool,
    pub line: u32,
}

impl StopAbi {
    pub const BREAKPOINT: u8 = 0;
    pub const STEP: u8 = 1;
    pub const PAUSE: u8 = 2;
    pub const ERROR: u8 = 3;
}

/// A native's window, or the host's ([`crate::Stack`]). Each method is the
/// stack method of the same name unless its comment says otherwise.
#[stabby::stabby(checked)]
pub trait StackAbi {
    extern "C" fn abs_index(&self, idx: i32) -> i32;
    extern "C" fn top(&self) -> i32;
    extern "C" fn window_start(&self) -> usize;
    extern "C" fn set_top(&mut self, idx: i32) -> Res<()>;
    extern "C" fn to_close(&mut self, idx: i32) -> Res<()>;
    extern "C" fn close_slot(&mut self, idx: i32) -> Res<()>;
    extern "C" fn close_all(&mut self) -> Res<()>;
    extern "C" fn push_copy(&mut self, idx: i32);
    extern "C" fn rotate(&mut self, idx: i32, n: i32) -> Res<()>;
    extern "C" fn copy(&mut self, from: i32, to: i32) -> Res<()>;
    extern "C" fn check_room(&self, n: i32) -> bool;

    /// The [`crate::stack::Type`] code.
    extern "C" fn type_at(&self, idx: i32) -> i32;
    extern "C" fn to_number(&self, idx: i32) -> SOption<f64>;
    extern "C" fn to_integer(&self, idx: i32) -> SOption<i64>;
    extern "C" fn is_string(&self, idx: i32) -> bool;
    extern "C" fn is_integer(&self, idx: i32) -> bool;
    extern "C" fn is_userdata(&self, idx: i32) -> bool;
    extern "C" fn is_native_function(&self, idx: i32) -> bool;
    extern "C" fn to_boolean(&self, idx: i32) -> bool;
    extern "C" fn to_str<'a>(&'a mut self, idx: i32) -> Res<SOption<Slice<'a, u8>>>;
    extern "C" fn string_at<'a>(&'a self, idx: i32) -> SOption<Slice<'a, u8>>;
    extern "C" fn number_text(&self, idx: i32) -> SOption<SString>;
    extern "C" fn raw_len(&self, idx: i32) -> u64;
    extern "C" fn to_userdata(&self, idx: i32) -> usize;
    /// The id of the program's host value in the userdata at `idx`
    /// ([`StackAbi::push_host_value`]).
    extern "C" fn host_value_id(&mut self, idx: i32) -> SOption<u64>;
    extern "C" fn is_host_userdata(&self, idx: i32) -> bool;
    extern "C" fn identity(&self, idx: i32) -> SOption<u64>;
    extern "C" fn to_pointer(&self, idx: i32) -> usize;
    /// The data word of a closure of the native registered as `native`.
    extern "C" fn closure_data_of(&mut self, idx: i32, native: u32) -> SOption<u64>;
    extern "C" fn raw_equal(&self, a: i32, b: i32) -> bool;
    /// `op`: 0 `==`, 1 `<`, 2 `<=`.
    extern "C" fn compare(&mut self, a: i32, b: i32, op: u8) -> Res<bool>;

    /// `op`: the [`crate::stack::ArithOp`] code.
    extern "C" fn arith(&mut self, op: u8) -> Res<()>;
    extern "C" fn concat(&mut self, n: i32) -> Res<()>;
    extern "C" fn len(&mut self, idx: i32) -> Res<()>;
    extern "C" fn push_numeral<'b>(&mut self, text: Slice<'b, u8>) -> usize;

    extern "C" fn push_nil(&mut self);
    extern "C" fn push_boolean(&mut self, b: bool);
    extern "C" fn push_integer(&mut self, n: i64);
    extern "C" fn push_number(&mut self, n: f64);
    extern "C" fn push_bytes<'a, 'b>(&'a mut self, bytes: Slice<'b, u8>) -> Slice<'a, u8>;
    extern "C" fn push_light_userdata(&mut self, p: usize);
    /// Push a userdata holding a program's host value, which the library
    /// keeps (and drops when the userdata is collected).
    extern "C" fn push_host_value(&mut self, v: HostValueBox);
    extern "C" fn reserve_memory(&mut self, bytes: usize) -> Res<()>;
    /// Push a function that runs `f`.
    extern "C" fn push_function(&mut self, f: NativeBox);
    /// Register `f` as a new native, named `name` in messages, giving its
    /// index for [`StackAbi::push_native`] and friends. The program keeps
    /// the index, registering each native once per isolate.
    extern "C" fn register_native(&mut self, name: Str<'static>, f: NativeBox) -> u32;
    extern "C" fn push_native(&mut self, native: u32);
    extern "C" fn push_native_closure(&mut self, native: u32, nups: usize, data: u64) -> Res<()>;
    /// Register `k` as a continuation (once per isolate), giving its slot.
    extern "C" fn register_continuation(&mut self, k: ContinuationBox) -> u32;
    extern "C" fn closure_data(&self) -> u64;
    extern "C" fn native_data(&self) -> Pair;
    extern "C" fn push_registered(&mut self, native: u32);

    /// The table operations give the pushed value's [`crate::stack::Type`]
    /// code.
    extern "C" fn get_table(&mut self, idx: i32) -> Res<i32>;
    extern "C" fn get_field<'b>(&mut self, idx: i32, k: Slice<'b, u8>) -> Res<i32>;
    extern "C" fn get_index(&mut self, idx: i32, n: i64) -> Res<i32>;
    extern "C" fn get_global<'b>(&mut self, name: Slice<'b, u8>) -> Res<i32>;
    extern "C" fn raw_get(&mut self, idx: i32) -> Res<i32>;
    extern "C" fn raw_get_index(&mut self, idx: i32, n: i64) -> Res<i32>;
    extern "C" fn raw_get_light(&mut self, idx: i32, p: usize) -> Res<i32>;
    extern "C" fn raw_get_field<'b>(&mut self, idx: i32, k: Slice<'b, u8>) -> Res<i32>;
    extern "C" fn create_table(&mut self, narr: i32, nrec: i32);
    extern "C" fn new_userdata(&mut self, size: usize, nuv: i32) -> Res<usize>;
    extern "C" fn get_user_value(&mut self, idx: i32, n: i32) -> i32;
    extern "C" fn set_user_value(&mut self, idx: i32, n: i32) -> Res<bool>;
    extern "C" fn get_metatable(&mut self, idx: i32) -> bool;
    extern "C" fn set_table(&mut self, idx: i32) -> Res<()>;
    extern "C" fn set_field<'b>(&mut self, idx: i32, k: Slice<'b, u8>) -> Res<()>;
    extern "C" fn set_index_int(&mut self, idx: i32, n: i64) -> Res<()>;
    extern "C" fn set_global<'b>(&mut self, name: Slice<'b, u8>) -> Res<()>;
    extern "C" fn raw_set(&mut self, idx: i32) -> Res<()>;
    extern "C" fn raw_set_index(&mut self, idx: i32, n: i64) -> Res<()>;
    extern "C" fn raw_set_light(&mut self, idx: i32, p: usize) -> Res<()>;
    extern "C" fn raw_set_field<'b>(&mut self, idx: i32, k: Slice<'b, u8>) -> Res<()>;
    extern "C" fn set_metatable(&mut self, idx: i32) -> Res<()>;
    extern "C" fn next(&mut self, idx: i32) -> Res<bool>;

    extern "C" fn check_call(&mut self, nargs: i32) -> Res<usize>;
    extern "C" fn adjust_results(&mut self, n: usize, nresults: i32);
    extern "C" fn call(&mut self, nargs: i32, nresults: i32) -> Res<()>;
    extern "C" fn call_handled(&mut self, nargs: i32, nresults: i32, handler: i32) -> Res<()>;
    /// Gives the [`crate::stack::Status`] code.
    extern "C" fn pcall(&mut self, nargs: i32, nresults: i32, handler: i32) -> Res<i32>;
    extern "C" fn status_of(&self, e: RaisedId) -> i32;
    extern "C" fn push_raised(&mut self, e: RaisedId);
    extern "C" fn take_raised(&mut self, e: RaisedId) -> TakenAbi;
    extern "C" fn memory_error(&mut self) -> RaisedId;
    extern "C" fn load<'b>(
        &mut self,
        bytes: Slice<'b, u8>,
        name: Slice<'b, u8>,
        mode: Slice<'b, u8>,
    ) -> i32;
    /// [`crate::Stack::load_chunk`], with the chunk's parts: `language`
    /// and `mode` as [`crate::Language`] and [`crate::Mode`] codes.
    extern "C" fn load_chunk<'b>(
        &mut self,
        bytes: Slice<'b, u8>,
        name: SOption<Slice<'b, u8>>,
        language: u8,
        mode: u8,
        env: i32,
    ) -> i32;

    extern "C" fn error_from_top(&mut self) -> RaisedId;
    extern "C" fn error<'b>(&mut self, message: Str<'b>) -> RaisedId;
    extern "C" fn arg_error<'b>(&mut self, arg: i32, message: Str<'b>) -> RaisedId;
    extern "C" fn type_error<'b>(&mut self, arg: i32, expected: Str<'b>) -> RaisedId;
    extern "C" fn check_string<'a>(&'a mut self, arg: i32) -> Res<Slice<'a, u8>>;
    extern "C" fn check_integer(&mut self, arg: i32) -> Res<i64>;
    extern "C" fn opt_integer(&mut self, arg: i32, default: i64) -> Res<i64>;
    extern "C" fn check_number(&mut self, arg: i32) -> Res<f64>;
    extern "C" fn display(&self, idx: i32) -> SVec<u8>;
    extern "C" fn push_where(&mut self, level: i32);
    extern "C" fn push_traceback<'b>(&mut self, message: SOption<Slice<'b, u8>>, level: i32);
    extern "C" fn to_display_string<'a>(&'a mut self, idx: i32) -> Res<Slice<'a, u8>>;

    /// `op`: the [`crate::stack::GcOp`] code (`Stop` is 0), `param` and
    /// `value` for `Step` and `Param`.
    extern "C" fn gc(&mut self, op: u8, param: u8, value: SOption<i64>) -> SOption<i64>;
    /// `mode`: 0 incremental, 1 generational.
    extern "C" fn set_gc_mode(&mut self, mode: u8) -> SOption<u8>;
    extern "C" fn warn<'b>(&mut self, message: Slice<'b, u8>, more: bool);
    extern "C" fn set_warn_hook(&mut self, hook: WarnHookBox);
    /// `lib`: the index in [`crate::Library::ALL`].
    extern "C" fn open_library(&mut self, lib: u8, ignore_env: bool) -> bool;
    extern "C" fn pin(&mut self, idx: i32) -> u32;
    extern "C" fn push_pinned(&mut self, slot: u32);
    extern "C" fn unpin(&mut self, slot: u32);
    extern "C" fn with_stdout<'u>(&mut self, user: StdoutUserMut<'u>);
    extern "C" fn before_exit(&mut self, hook: HookBox);
    extern "C" fn safepoint(&mut self);
    extern "C" fn control<'a>(&'a mut self) -> ControlMut<'a>;

    extern "C" fn running_thread(&self) -> ThreadAbi;
    extern "C" fn main_thread(&self) -> ThreadAbi;
    extern "C" fn push_thread(&mut self, t: ThreadAbi);
    extern "C" fn push_new_thread(&mut self) -> ThreadAbi;
    extern "C" fn to_thread(&self, idx: i32) -> SOption<ThreadAbi>;
    extern "C" fn thread_is_live(&self, t: u64) -> bool;
    /// The [`crate::ThreadStatus`] code: suspended, running, normal, dead.
    extern "C" fn thread_status(&self, t: u64) -> u8;
    extern "C" fn thread_yielded(&self, t: u64) -> bool;
    extern "C" fn thread_failed(&self, t: u64) -> bool;
    extern "C" fn thread_yieldable(&self, t: u64) -> bool;
    /// Gives an id for [`StackAbi::leave_thread`].
    extern "C" fn enter_thread(&mut self, t: u64, base: SOption<usize>) -> u64;
    extern "C" fn leave_thread(&mut self, t: u64, window: u64);
    extern "C" fn move_values(
        &mut self,
        from: u64,
        from_base: SOption<usize>,
        to: u64,
        to_base: SOption<usize>,
        n: usize,
    );
    extern "C" fn resume(&mut self, t: u64, base: SOption<usize>, nargs: usize) -> Res<ResumedAbi>;
    extern "C" fn push_coroutine(&mut self, f: i32);
    /// Whether it returned (`a` is 1), and how many values (`b`).
    extern "C" fn resume_here(&mut self, idx: i32, nargs: usize) -> Res<Pair>;
    extern "C" fn close_thread(&mut self, t: u64) -> i32;

    extern "C" fn has_level(&self, level: i32) -> bool;
    extern "C" fn level_info(&self, level: usize) -> SOption<FrameInfoAbi>;
    extern "C" fn function_info(&self, idx: i32) -> FrameInfoAbi;
    extern "C" fn push_level_function(&mut self, level: usize);
    extern "C" fn get_local(&mut self, level: usize, n: i32) -> SOption<SVec<u8>>;
    extern "C" fn param_name(&mut self, n: i32) -> SOption<SVec<u8>>;
    extern "C" fn set_local(&mut self, level: usize, n: i32) -> Res<SOption<SVec<u8>>>;
    extern "C" fn get_upvalue(&mut self, func: i32, n: i32) -> SOption<SVec<u8>>;
    extern "C" fn set_upvalue(&mut self, func: i32, n: i32) -> Res<SOption<SVec<u8>>>;
    extern "C" fn upvalue_id(&self, func: i32, n: i32) -> usize;
    extern "C" fn join_upvalues(&mut self, f1: i32, n1: i32, f2: i32, n2: i32) -> Res<()>;
}

/// A window, borrowed for one native call or host entry.
pub type StackMut<'a> = dynptr!(&'a mut dyn StackAbi);

/// A window as a program object receives it. stabby proves a trait object
/// stable through every type its methods name, and the window's methods
/// take the program objects that receive windows, so the program's traits
/// name the window by its layout alone, which keeps the proof from going
/// round in a loop. [`StableLike::into_inner`] gives the window back.
pub type StackArg<'a> = StableLike<StackMut<'a>, Tuple2<usize, usize>>;

/// The isolate itself ([`crate::stack::Control`]).
#[stabby::stabby(checked)]
pub trait ControlAbi {
    extern "C" fn interrupt_flag(&self) -> usize;
    extern "C" fn clear_interrupt(&mut self);
    extern "C" fn set_interrupt_handler(&mut self, h: InterruptHandlerBox);
    /// [`StackAbi::register_native`].
    extern "C" fn register_native(&mut self, name: Str<'static>, f: NativeBox) -> u32;
    /// Register another native running what `native` runs, with `data`.
    extern "C" fn register_native_data(&mut self, native: u32, data: Pair) -> u32;
    extern "C" fn set_hook(&mut self, f: SOption<u32>, mask: u8, count: i64);
    extern "C" fn publish_main_thread(&mut self);
    extern "C" fn main_thread(&self) -> ThreadAbi;
    /// [`crate::Isolate::set_breakpoints`]; all unresolved when the
    /// library has no engine.
    extern "C" fn set_breakpoints<'b>(
        &mut self,
        source: Slice<'b, u8>,
        lines: Slice<'b, u32>,
    ) -> SVec<BreakpointAbi>;
    /// `on`: 0 never, 1 uncaught, 2 all.
    extern "C" fn set_error_break(&mut self, on: u8);
    /// [`crate::Isolate::stop_on_next_line`].
    extern "C" fn stop_on_next_line(&mut self);
}

pub type ControlMut<'a> = dynptr!(&'a mut dyn ControlAbi);

/// The isolate as an interrupt handler receives it, by layout as
/// [`StackArg`] is.
pub type ControlArg<'a> = StableLike<ControlMut<'a>, Tuple2<usize, usize>>;

/// One isolate ([`crate::Isolate`]); dropping it closes the isolate.
#[stabby::stabby(checked)]
pub trait IsolateAbi {
    /// Run `e` on the host's window.
    extern "C" fn enter<'e>(&mut self, e: EntryMut<'e>);
    extern "C" fn control<'a>(&'a mut self) -> ControlMut<'a>;
    extern "C" fn set_stdout(&mut self, out: WriterBox);
    extern "C" fn gc_collect(&mut self);
    extern "C" fn gc_used_bytes(&self) -> usize;
    extern "C" fn gc_set_mode(&mut self, mode: u8);
    extern "C" fn gc_set_limit(&mut self, bytes: SOption<usize>);
    extern "C" fn gc_limit(&self) -> SOption<usize>;
    extern "C" fn gc_set_stress(&mut self, on: bool);
    extern "C" fn interrupt(&self) -> InterruptArc;
    extern "C" fn flush(&mut self);
    /// `level`: 0 none, 1 structure, 2 full.
    extern "C" fn set_binary_checks(&mut self, level: u8);
    extern "C" fn before_exit(&mut self, hook: HookBox);
    /// Install the library's own `orbit-std` (`orbit_std::install`).
    extern "C" fn install_std(
        &mut self,
        base: bool,
        io: bool,
        os: bool,
        ignore_env: bool,
    ) -> SResult<(), SString>;
    /// Install the library's C API for C modules (`orbit_capi::install`).
    extern "C" fn install_c_modules(&mut self, lock_cpath: bool);
    /// Install the library's `ffi` (`orbit_std::install_ffi`); false if it
    /// was built without it.
    extern "C" fn install_ffi(&mut self) -> bool;
    /// [`crate::Isolate::set_debug_events`]; false if the isolate allows
    /// no debugger or the library was built without the engine.
    extern "C" fn set_debug_events(&mut self, events: SOption<DebugEventsBox>) -> bool;
    /// None when no debugger can attach.
    extern "C" fn pause_handle(&self) -> SOption<PauseArc>;
}

pub type IsolateBox = dynptr!(SBox<dyn IsolateAbi>);

/// Stops running code from any thread ([`crate::Interrupt`]).
#[stabby::stabby(checked)]
pub trait InterruptAbi {
    extern "C" fn request(&self);
    extern "C" fn pending(&self) -> bool;
}

pub type InterruptArc = dynptr!(stabby::sync::Arc<dyn InterruptAbi + Send + Sync>);

/// Pauses an isolate from any thread ([`crate::debugger::PauseHandle`]).
#[stabby::stabby(checked)]
pub trait PauseAbi {
    extern "C" fn request(&self);
}

pub type PauseArc = dynptr!(stabby::sync::Arc<dyn PauseAbi + Send + Sync>);

/// A program's [`crate::debugger::DebugEvents`]. `stopped` gets the window
/// of the native reporting the stop and answers with a
/// [`crate::debugger::Resume`]: 0 continue, 1 step in, 2 over, 3 out.
#[stabby::stabby(checked)]
pub trait DebugEventsAbi {
    extern "C" fn stopped<'a>(&mut self, s: StackArg<'a>, reason: StopAbi) -> u8;
    extern "C" fn breakpoints_resolved<'b>(
        &mut self,
        source: Slice<'b, u8>,
        breakpoints: Slice<'b, BreakpointAbi>,
    );
}

pub type DebugEventsBox = dynptr!(SBox<dyn DebugEventsAbi>);

/// A program's native: a [`crate::stack::Native`] or a function made from
/// a closure.
#[stabby::stabby(checked)]
pub trait ProgramNative {
    extern "C" fn call<'a>(&self, s: StackArg<'a>) -> RetAbi;
}

pub type NativeBox = dynptr!(SBox<dyn ProgramNative>);

/// A program's [`crate::stack::Continuation`]: `ok` with `n` values, or
/// the error `raised`.
#[stabby::stabby(checked)]
pub trait ProgramContinuation {
    extern "C" fn resume<'a>(
        &self,
        s: StackArg<'a>,
        ok: bool,
        n: usize,
        raised: RaisedId,
    ) -> RetAbi;
}

pub type ContinuationBox = dynptr!(SBox<dyn ProgramContinuation>);

/// A program's host value in a userdata, as the library holds it; the
/// value itself stays in the program, under this id.
#[stabby::stabby(checked)]
pub trait HostValue {
    extern "C" fn id(&self) -> u64;
}

pub type HostValueBox = dynptr!(SBox<dyn HostValue>);

/// A program's hook ([`crate::Isolate::before_exit`]).
#[stabby::stabby(checked)]
pub trait Hook {
    extern "C" fn run(&self);
}

pub type HookBox = dynptr!(SBox<dyn Hook>);

/// A program's warning hook ([`crate::Stack::set_warn_hook`]).
#[stabby::stabby(checked)]
pub trait WarnHook {
    extern "C" fn warn<'b>(&mut self, message: Slice<'b, u8>, more: bool);
}

pub type WarnHookBox = dynptr!(SBox<dyn WarnHook>);

/// An output stream, either side's: errors as their message.
#[stabby::stabby(checked)]
pub trait Writer {
    extern "C" fn write<'b>(&mut self, bytes: Slice<'b, u8>) -> SResult<usize, SString>;
    extern "C" fn flush(&mut self) -> SResult<(), SString>;
}

pub type WriterBox = dynptr!(SBox<dyn Writer>);
pub type WriterMut<'a> = dynptr!(&'a mut dyn Writer);

/// A program's [`crate::Stack::with_stdout`] closure.
#[stabby::stabby(checked)]
pub trait StdoutUser {
    extern "C" fn run<'w>(&mut self, out: WriterMut<'w>);
}

pub type StdoutUserMut<'a> = dynptr!(&'a mut dyn StdoutUser);

/// A program's closure over the host's window ([`IsolateAbi::enter`]).
#[stabby::stabby(checked)]
pub trait Entry {
    extern "C" fn run<'s>(&mut self, s: StackArg<'s>);
}

pub type EntryMut<'a> = dynptr!(&'a mut dyn Entry);

/// A program's [`crate::stack::InterruptHandler`].
#[stabby::stabby(checked)]
pub trait ProgramInterruptHandler {
    extern "C" fn handle<'c>(&self, c: ControlArg<'c>) -> bool;
}

pub type InterruptHandlerBox = dynptr!(SBox<dyn ProgramInterruptHandler>);
