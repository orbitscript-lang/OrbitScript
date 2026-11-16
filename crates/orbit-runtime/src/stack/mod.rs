//! The stack API: Lua-C-API-shaped operations on a call's window, in safe
//! Rust (ADR-11, phase e).
//!
//! A [`Stack`] is the window of values a native function works in, the
//! same window an `orbit::Scope` wraps. Values are addressed by index with Lua's
//! convention: 1 is the window's first slot, -1 its top, and two kinds of
//! pseudo-index reach the registry ([`REGISTRY_INDEX`]) and the running
//! native closure's upvalues ([`upvalue_index`]). An index outside the
//! window reads as "none"; writing to one, or a wrong type where an
//! operation needs a table, is an error, never undefined behaviour.
//!
//! The typed handles stay the API for Rust hosts, and are written over
//! this layer: an `orbit::Scope` holds a stack and each handle is a slot in it.
//! This layer is also for bindings that need Lua's exact semantics, the
//! Lua C API above all (`orbit-capi`).
//!
//! Natives written against it implement [`Native`]; one that waits for a
//! call or a yield continues in a [`Continuation`]. Both are types rather
//! than function values, so each compiles to its own trampoline with no
//! indirect call.
//!
//! The window itself, over the runtime, is in `window`. The `orbit` crate
//! re-exports this module as `orbit::stack`, adding what needs its own
//! types (scopes and chunks) through `orbit::stack::StackExt`.

mod exit;
mod typed;
mod window;

use crate::object::OrbitString;
use crate::stdlib::Lib;

#[doc(hidden)]
pub use exit::exit_hooks;

pub(crate) use typed::Buffer;
pub use typed::{
    Any, Arg, Either, Fail, FunctionArg, IntoRaised, NativeFunction, Nil, Number, Push, Rest,
    Results, State, TableArg, Ud, UserType, View, field_name, function,
};
#[doc(hidden)]
pub use typed::{PureArg, PureFn, PureResult, PureValue, catch_panic, panic_message};
pub(crate) use window::{ContRef, cont_ref};
pub use window::{Control, NativeRef, Raised, Stack, ThreadRef, ThreadWindow};
pub(crate) use window::{LibraryRow, library_row};

/// The pseudo-index of the registry (Lua's `LUA_REGISTRYINDEX`).
pub const REGISTRY_INDEX: i32 = -(i32::MAX / 2 + 1000);

/// The pseudo-index of upvalue `i` (1-based) of the running native
/// closure (Lua's `lua_upvalueindex`).
pub const fn upvalue_index(i: i32) -> i32 {
    REGISTRY_INDEX - i
}

/// As a result count: keep every result (Lua's `LUA_MULTRET`).
pub const MULTRET: i32 = -1;

/// The registry entry that holds the main thread (Lua's
/// `LUA_RIDX_MAINTHREAD`): 3, or 1 in a Lua 5.4 build (ADR-14).
pub const RIDX_MAINTHREAD: i64 = if orbit_lua_version::LUA.is_5_4() {
    1
} else {
    3
};

/// The registry index of the global table (`LUA_RIDX_GLOBALS`).
pub const RIDX_GLOBALS: i64 = 2;
/// A value's type, with Lua's type codes as discriminants.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
pub enum Type {
    /// No value: an index outside the window.
    None = -1,
    Nil = 0,
    Boolean = 1,
    LightUserdata = 2,
    Number = 3,
    String = 4,
    Table = 5,
    Function = 6,
    Userdata = 7,
    Thread = 8,
}

impl Type {
    /// The type with Lua's code `code`.
    pub fn from_code(code: i32) -> Option<Type> {
        Some(match code {
            -1 => Type::None,
            0 => Type::Nil,
            1 => Type::Boolean,
            2 => Type::LightUserdata,
            3 => Type::Number,
            4 => Type::String,
            5 => Type::Table,
            6 => Type::Function,
            7 => Type::Userdata,
            8 => Type::Thread,
            _ => return None,
        })
    }

    /// The name `type` gives (`"no value"` for [`Type::None`]).
    pub fn name(self) -> &'static str {
        match self {
            Type::None => crate::arg_errors::NO_VALUE,
            Type::Nil => "nil",
            Type::Boolean => "boolean",
            Type::LightUserdata | Type::Userdata => "userdata",
            Type::Number => "number",
            Type::String => "string",
            Type::Table => "table",
            Type::Function => "function",
            Type::Thread => "thread",
        }
    }
}

/// How a protected call, a resume or a load ended, with Lua's status
/// codes as discriminants.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
pub enum Status {
    Ok = 0,
    Yield = 1,
    /// A runtime error.
    Runtime = 2,
    Syntax = 3,
    Memory = 4,
    /// An error in the message handler.
    Handler = 5,
}

impl Status {
    /// The status with Lua's code `code`.
    pub fn from_code(code: i32) -> Option<Status> {
        Some(match code {
            0 => Status::Ok,
            1 => Status::Yield,
            2 => Status::Runtime,
            3 => Status::Syntax,
            4 => Status::Memory,
            5 => Status::Handler,
            _ => return None,
        })
    }
}

/// A comparison for [`Stack::compare`], with Lua's `LUA_OP*` codes as
/// discriminants.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum CompareOp {
    Eq = 0,
    Lt = 1,
    Le = 2,
}

impl CompareOp {
    /// All of them, by Lua's code.
    pub const ALL: [CompareOp; 3] = [CompareOp::Eq, CompareOp::Lt, CompareOp::Le];

    /// The comparison with Lua's code `code`.
    pub fn from_code(code: u8) -> Option<CompareOp> {
        CompareOp::ALL.get(usize::from(code)).copied()
    }
}

/// An operator for [`Stack::arith`], in Lua's `LUA_OP*` order, so its
/// discriminant is Lua's code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ArithOp {
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

impl ArithOp {
    /// All of them, by Lua's code.
    pub const ALL: [ArithOp; 14] = [
        ArithOp::Add,
        ArithOp::Sub,
        ArithOp::Mul,
        ArithOp::Mod,
        ArithOp::Pow,
        ArithOp::Div,
        ArithOp::IDiv,
        ArithOp::BAnd,
        ArithOp::BOr,
        ArithOp::BXor,
        ArithOp::Shl,
        ArithOp::Shr,
        ArithOp::Unm,
        ArithOp::BNot,
    ];

    /// The operator with Lua's code `code`.
    pub fn from_code(code: u8) -> Option<ArithOp> {
        ArithOp::ALL.get(usize::from(code)).copied()
    }

    fn unary(self) -> bool {
        matches!(self, ArithOp::Unm | ArithOp::BNot)
    }
}

/// A collector parameter for [`GcOp::Param`], in Lua's `LUA_GCP*` order,
/// so its discriminant is Lua's code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum GcParam {
    MinorMul,
    MajorMinor,
    MinorMajor,
    Pause,
    StepMul,
    StepSize,
}

impl GcParam {
    /// All of them, by Lua's code.
    pub const ALL: [GcParam; 6] = [
        GcParam::MinorMul,
        GcParam::MajorMinor,
        GcParam::MinorMajor,
        GcParam::Pause,
        GcParam::StepMul,
        GcParam::StepSize,
    ];

    /// The parameter with Lua's code `code`.
    pub fn from_code(code: u8) -> Option<GcParam> {
        GcParam::ALL.get(usize::from(code)).copied()
    }
}

/// What [`Stack::gc`] does (Lua's `lua_gc` options).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GcOp {
    Stop,
    Restart,
    Collect,
    /// Kilobytes in use.
    Count,
    /// Bytes in use past the last whole kilobyte.
    CountRemainder,
    /// An incremental step of about this many kilobytes.
    Step(i32),
    IsRunning,
    /// Read a parameter, and set it when the value is `Some`.
    Param(GcParam, Option<i64>),
}

/// The result of a stack operation that can raise.
pub type StackResult<T> = Result<T, Raised>;

/// A native function written against the stack API. Its window starts
/// with its arguments; it ends with a [`Return`].
pub trait Native: 'static {
    /// The native's name in messages, when the call site doesn't name it.
    /// Natives are told apart by type, so two may share a name.
    const NAME: &'static str;

    /// A fast path for calls that want one result, which `#[native(pure)]`
    /// generates: no window, no frame. The interpreter tries it first, and
    /// runs [`Native::call`] when it gives up.
    #[doc(hidden)]
    const PURE: Option<PureFn> = None;

    fn call(s: &mut Stack<'_>) -> StackResult<Return>;
}

/// How a native continues after the call or yield it ended with
/// ([`Return::call_then`], [`Return::yield_then`]). It runs in the same
/// window, with the same upvalues.
pub trait Continuation: 'static {
    fn resume(s: &mut Stack<'_>, outcome: Outcome) -> StackResult<Return>;
}

/// What a [`Continuation`] continues from.
#[derive(Debug)]
pub enum Outcome {
    /// The call returned this many results, now the window's last values;
    /// or the coroutine was resumed with this many values.
    Ok(usize),
    /// The protected call failed, or the coroutine was closed while
    /// suspended.
    Err(Raised),
}

/// How a [`Native`] ends.
#[derive(Debug, Clone, Copy)]
#[must_use]
pub struct Return(pub(crate) Ret);

#[derive(Debug, Clone, Copy)]
pub(crate) enum Ret {
    Values(usize),
    Yield(usize),
    YieldThen(usize, ContRef),
    CallThen {
        nargs: usize,
        protected: bool,
        k: ContRef,
    },
    Exit {
        code: i32,
        close: bool,
    },
}

impl Return {
    /// Return the window's top `n` values.
    pub fn values(n: usize) -> Return {
        Return(Ret::Values(n))
    }

    /// Yield the window's top `n` values; when resumed, return the values
    /// the resume passed.
    pub fn yield_values(n: usize) -> Return {
        Return(Ret::Yield(n))
    }

    /// Yield the window's top `n` values; when resumed, continue in `K`.
    pub fn yield_then<K: Continuation>(n: usize) -> Return {
        Return(Ret::YieldThen(n, cont_ref::<K>()))
    }

    /// Call the function below the window's top `nargs` values, which code
    /// may yield across, then continue in `K` with its results (or, when
    /// `protected`, its error).
    pub fn call_then<K: Continuation>(nargs: usize, protected: bool) -> Return {
        Return(Ret::CallThen {
            nargs,
            protected,
            k: cont_ref::<K>(),
        })
    }

    /// Call the function below the window's top `nargs` values as the
    /// native's last act: its results, or its error, are the native's, and
    /// code it runs may yield across it (as across `dofile`).
    pub fn tail_call(nargs: usize) -> Return {
        Return::call_then::<TailCalled>(nargs, false)
    }

    /// End the process with exit status `code`, as `os.exit` does: first,
    /// when `close`, closing the isolate (the main thread's variables still
    /// to close, then every pending finalizer); then running the
    /// [`Stack::before_exit`] hooks and flushing standard output.
    pub fn exit(code: i32, close: bool) -> Return {
        Return(Ret::Exit { code, close })
    }
}

/// What [`Stack::resume`] ended with. The results, or the error, are on
/// the resumed thread's stack.
#[derive(Debug, Clone, Copy)]
pub struct Resumed {
    pub status: Status,
    /// How many values it yielded or returned (0 after an error).
    pub nresults: usize,
    /// After an error that killed the thread: where its stack ended, which
    /// is where a view of its stack should start from now on.
    pub failed_top: Option<usize>,
}

/// What [`Stack::level_info`] and [`Stack::function_info`] report
/// (`lua_getinfo`).
#[derive(Debug, Default)]
pub struct FrameInfo {
    pub name: Option<OrbitString>,
    pub namewhat: &'static str,
    pub what: &'static str,
    pub source: OrbitString,
    pub short_src: String,
    pub currentline: i32,
    pub linedefined: i32,
    pub lastlinedefined: i32,
    pub nups: u8,
    pub nparams: u8,
    pub isvararg: bool,
    pub extraargs: u8,
    pub istailcall: bool,
    /// The name the function was defined with (`local function f`,
    /// `function t.f`, `fn f`), from its source rather than the call:
    /// for a debugger to show where `name` is unknown, such as a tail
    /// call. Not part of `debug.getinfo`.
    pub defined_name: Option<OrbitString>,
}

/// Runs when the isolate's interrupt flag is raised
/// ([`Control::set_interrupt_handler`]): returns true if it handled the
/// interrupt, so running code goes on, or false to stop it with
/// `interrupted!`.
pub trait InterruptHandler: 'static {
    fn handle(c: &mut Control<'_>) -> bool;
}

// ---- built on the window's operations ---------------------------------------

impl<'s> Stack<'s> {
    /// Run `f` on argument `arg` (1-based) as the host value of type `T`,
    /// or raise Lua's type error naming `expected` (also while the value is
    /// taken out).
    pub fn check_userdata<T: std::any::Any, R>(
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
}

/// The continuation of [`Return::tail_call`]: the call's results, or its
/// error, are the native's.
struct TailCalled;

impl Continuation for TailCalled {
    fn resume(_: &mut Stack<'_>, outcome: Outcome) -> StackResult<Return> {
        match outcome {
            Outcome::Ok(n) => Ok(Return::values(n)),
            Outcome::Err(e) => Err(e),
        }
    }
}

// ---- types the window's operations name ---------------------------------------

/// How the API stores a host value in a userdata (`Stack::push_userdata`,
/// `orbit::Class`): taken out while a method runs, so the method can use
/// the whole window at the same time.
pub(crate) struct HostValue<T>(pub(crate) Option<T>);

impl<T> std::fmt::Debug for HostValue<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(if self.0.is_some() {
            "HostValue(..)"
        } else {
            "HostValue(taken)"
        })
    }
}

/// What [`Stack::thread_status`] reports, as `coroutine.status` does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThreadStatus {
    Suspended,
    Running,
    /// It resumed another coroutine and waits for it.
    Normal,
    Dead,
}

impl ThreadStatus {
    /// The status as `coroutine.status` names it.
    pub fn name(self) -> &'static str {
        match self {
            ThreadStatus::Suspended => "suspended",
            ThreadStatus::Running => "running",
            ThreadStatus::Normal => "normal",
            ThreadStatus::Dead => "dead",
        }
    }
}

/// A standard library an isolate can open ([`Stack::open_library`],
/// `orbit::Config::libraries`). The libraries that touch the system (`io`,
/// `os`, `print`, `loadfile`, `dofile`) and the ones written on the public
/// API (`math`, `package`, `table`, `utf8`) are `orbit-std`'s, which hosts
/// install on top.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Library {
    /// The base functions (`print`, `pairs`, `pcall`, ...), as globals.
    Base,
    Coroutine,
    Debug,
    String,
}

impl Library {
    pub const ALL: [Library; 4] = [
        Library::Base,
        Library::Coroutine,
        Library::Debug,
        Library::String,
    ];

    /// The runtime's name for the library.
    #[doc(hidden)]
    pub fn lib(self) -> Lib {
        match self {
            Library::Base => Lib::Base,
            Library::Coroutine => Lib::Coroutine,
            Library::Debug => Lib::Debug,
            Library::String => Lib::String,
        }
    }
}

/// The collector's mode ([`Stack::set_gc_mode`], `orbit::Config::gc_mode`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum GcMode {
    /// Every cycle marks the whole heap, in small steps paced by
    /// allocation: short pauses, more total work.
    Incremental,
    /// Frequent minor collections of recently made objects, and major ones
    /// of everything when the heap has grown: less total work for programs
    /// that make many short-lived objects. Each collection runs to the end
    /// once started, so a major one is a longer pause.
    #[default]
    Generational,
}

impl GcMode {
    /// The VM's own value for the mode.
    #[doc(hidden)]
    pub fn raw(self) -> crate::vm::GcMode {
        match self {
            GcMode::Incremental => crate::vm::GcMode::Incremental,
            GcMode::Generational => crate::vm::GcMode::Generational,
        }
    }
}

/// Which errors stop the program (`orbit::Isolate::set_error_break`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ErrorBreak {
    #[default]
    Never,
    /// Errors nothing in the script catches.
    Uncaught,
    /// Every error.
    All,
}

/// A breakpoint as set, and where it ended up: the first line at or after
/// the requested one that has code, or `None` while no loaded function of
/// its source has code there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Breakpoint {
    pub requested: u32,
    pub line: Option<u32>,
}
