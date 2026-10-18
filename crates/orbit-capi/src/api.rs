//! What the C API adds to [`orbit::stack`]: the ABI's codes, the state C
//! needs between calls, and the few operations whose meaning is C's own
//! (light C functions, `lua_gc`'s options, `luaopen_*`).
//!
//! Indices follow the manual and pass through unchanged: the stack API
//! uses Lua's convention, pseudo-indices included.

use std::cell::RefCell;

use orbit::Library;
use orbit::stack::{ArithOp, CompareOp, GcOp, GcParam, Raised, Stack, StackResult, Type};

pub(crate) type Res<T> = StackResult<T>;

/// Constants from the C ABI (`lua.h`).
pub(crate) mod abi {
    /// Hook events (`lua_Debug.event`).
    pub const HOOKCALL: i32 = 0;
    pub const HOOKRET: i32 = 1;
    pub const HOOKLINE: i32 = 2;
    pub const HOOKCOUNT: i32 = 3;
    pub const HOOKTAILCALL: i32 = 4;
    pub const REGISTRYINDEX: i32 = orbit::stack::REGISTRY_INDEX;
    pub const TNONE: i32 = -1;
    pub const OK: i32 = 0;
    pub const YIELD: i32 = 1;
    pub const ERRRUN: i32 = 2;
    pub const ERRSYNTAX: i32 = 3;
    pub const OPEQ: i32 = 0;
    pub const OPLT: i32 = 1;
    pub const OPLE: i32 = 2;
    pub const GCSTOP: i32 = 0;
    pub const GCRESTART: i32 = 1;
    pub const GCCOLLECT: i32 = 2;
    pub const GCCOUNT: i32 = 3;
    pub const GCCOUNTB: i32 = 4;
    pub const GCSTEP: i32 = 5;
    pub const GCISRUNNING: i32 = 6;
    pub const GCGEN: i32 = 7;
    pub const GCINC: i32 = 8;
    pub const GCPARAM: i32 = 9;
    /// `LUA_RIDX_GLOBALS`: where the registry keeps the global table.
    pub const RIDX_GLOBALS: i64 = orbit::stack::RIDX_GLOBALS;
}

/// Per-VM state the API keeps between calls. Borrowed only for short
/// stretches, never across a call back into Lua, because that call can run
/// C functions that borrow it again.
#[derive(Debug, Default)]
pub(crate) struct State {
    /// The error a Rust entry point raised, for the trampoline's caller.
    pub pending: Option<Raised>,
    /// NUL-terminated copies of strings handed to C. Each C call drops the
    /// copies made during it when it returns. Between calls, in a state C
    /// created (ADR-10), copies of string values (`lua_tolstring`) last
    /// while an equal string is on the host's stack; see
    /// [`State::prune`].
    pub strs: Vec<CStrCopy>,
    /// How many copies the last [`State::prune`] kept.
    pub kept: usize,
}

/// A string handed to C.
#[derive(Debug)]
pub(crate) struct CStrCopy {
    /// The bytes and a trailing NUL.
    pub bytes: Box<[u8]>,
    /// A copy of a string value, which may go once no equal string is on
    /// the host's stack. Other copies (`lua_getinfo`'s source) stay until
    /// the state closes.
    pub value: bool,
}

impl State {
    /// Drop the copies of string values that no string on `stack` equals.
    /// Runs when the copies have doubled since the last pruning, so each
    /// costs O(1) amortized.
    pub fn prune(&mut self, stack: &[&[u8]]) {
        if self.strs.len() < 2 * self.kept + 16 {
            return;
        }
        self.strs.retain(|s| {
            !s.value || {
                let text = &s.bytes[..s.bytes.len() - 1];
                stack.contains(&text)
            }
        });
        self.kept = self.strs.len();
    }
}

pub(crate) type Shared = RefCell<State>;

/// `lua_type`'s code.
pub(crate) fn type_code(t: Type) -> i32 {
    t as i32
}

pub(crate) fn compare(c: &mut Stack<'_>, a: i32, b: i32, op: i32) -> Res<bool> {
    let op = match op {
        abi::OPEQ => CompareOp::Eq,
        abi::OPLT => CompareOp::Lt,
        abi::OPLE => CompareOp::Le,
        _ => return Err(c.error("C API: invalid comparison option")),
    };
    c.compare(a, b, op)
}

pub(crate) fn arith(c: &mut Stack<'_>, op: i32) -> Res<()> {
    let Some(&op) = usize::try_from(op).ok().and_then(|i| ArithOp::ALL.get(i)) else {
        return Err(c.error("C API: invalid arithmetic operator"));
    };
    c.arith(op)
}

/// A registry key no module can make: the address of a static of ours.
fn private_key(key: &'static u8) -> usize {
    std::ptr::from_ref(key) as usize
}

static C_FUNCTIONS: u8 = 0;

/// `lua_pushcclosure`.
pub(crate) fn pushcclosure(c: &mut Stack<'_>, f: u64, n: i32) -> Res<()> {
    let n = n.max(0) as usize;
    if n > 0 {
        return crate::push_c_function(c, f, n);
    }
    // One function value per C function, so pushing it twice gives equal
    // values, as in Lua (light C functions).
    let key = private_key(&C_FUNCTIONS);
    if c.raw_get_light(abi::REGISTRYINDEX, key)? != Type::Table {
        c.set_top(-2)?;
        c.create_table(0, 0);
        c.push_copy(-1);
        c.raw_set_light(abi::REGISTRYINDEX, key)?;
    }
    // [cache]
    if c.raw_get_light(-1, f as usize)? != Type::Function {
        c.set_top(-2)?;
        crate::push_c_function(c, f, 0)?;
        c.push_copy(-1);
        c.raw_set_light(-3, f as usize)?;
    }
    // [cache, f]
    c.rotate(-2, 1)?;
    c.set_top(-2)
}

/// `lua_gc`. `a` and `b` are the option's arguments: the step size for
/// `LUA_GCSTEP`, the parameter and its new value (negative: unchanged) for
/// `LUA_GCPARAM`. Inside a finalizer every option does nothing and returns
/// -1, as in Lua.
pub(crate) fn gc(c: &mut Stack<'_>, what: i32, a: i32, b: i32) -> i32 {
    let op = match what {
        abi::GCSTOP => GcOp::Stop,
        abi::GCRESTART => GcOp::Restart,
        abi::GCCOLLECT => GcOp::Collect,
        abi::GCCOUNT => GcOp::Count,
        abi::GCCOUNTB => GcOp::CountRemainder,
        abi::GCSTEP => GcOp::Step(a),
        abi::GCISRUNNING => GcOp::IsRunning,
        abi::GCGEN | abi::GCINC => {
            let mode = if what == abi::GCGEN {
                orbit::GcMode::Generational
            } else {
                orbit::GcMode::Incremental
            };
            return match c.set_gc_mode(mode) {
                Some(orbit::GcMode::Generational) => abi::GCGEN,
                Some(orbit::GcMode::Incremental) => abi::GCINC,
                None => -1,
            };
        }
        abi::GCPARAM => {
            // lua.h's LUA_GCP* order.
            let p = match a {
                0 => GcParam::MinorMul,
                1 => GcParam::MajorMinor,
                2 => GcParam::MinorMajor,
                3 => GcParam::Pause,
                4 => GcParam::StepMul,
                5 => GcParam::StepSize,
                _ => return -1,
            };
            GcOp::Param(p, (b >= 0).then_some(i64::from(b)))
        }
        _ => return -1,
    };
    match c.gc(op) {
        Some(v) => i32::try_from(v).unwrap_or(i32::MAX),
        None => -1,
    }
}

/// A library `luaopen_*` opens.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Lib {
    Runtime(Library),
    /// `orbit-std`'s, which touch the system.
    Io,
    Os,
}

/// The libraries by the index `csrc/capi.c` passes (the order of
/// `lualib.h`'s masks), with their global names.
const LIBS: [(Lib, &[u8]); 10] = [
    (Lib::Runtime(Library::Base), b"_G"),
    (Lib::Runtime(Library::Package), b"package"),
    (Lib::Runtime(Library::Coroutine), b"coroutine"),
    (Lib::Runtime(Library::Debug), b"debug"),
    (Lib::Io, b"io"),
    (Lib::Runtime(Library::Math), b"math"),
    (Lib::Os, b"os"),
    (Lib::Runtime(Library::String), b"string"),
    (Lib::Runtime(Library::Table), b"table"),
    (Lib::Runtime(Library::Utf8), b"utf8"),
];

/// `luaopen_*`: open standard library `lib` and push its table, as
/// `luaL_requiref` expects. Like Lua's, it leaves the global to
/// `luaL_requiref`: a library opened through `package.preload` gets no
/// global. Returns 0, pushing nothing, for a library this build leaves out.
pub(crate) fn openlib(c: &mut Stack<'_>, lib: i32) -> Res<i32> {
    let Some(&(lib, name)) = usize::try_from(lib).ok().and_then(|i| LIBS.get(i)) else {
        return Ok(0);
    };
    // `registry.LUA_NOENV` (set by `lua -E`) makes the libraries ignore
    // environment variables.
    c.raw_get_field(abi::REGISTRYINDEX, b"LUA_NOENV")?;
    let ignore_env = c.to_boolean(-1);
    c.set_top(-2)?;
    let base = lib == Lib::Runtime(Library::Base);
    // [globals, the global's value before]
    c.raw_get_index(abi::REGISTRYINDEX, abi::RIDX_GLOBALS)?;
    if base {
        c.push_nil();
    } else {
        c.raw_get_field(-1, name)?;
    }
    let opened = match lib {
        Lib::Io => c.push_from_scope(|s| orbit_std::open_io(s).ok().map(|t| t.to_value())),
        Lib::Os => c.push_from_scope(|s| orbit_std::open_os(s).ok().map(|t| t.to_value())),
        Lib::Runtime(l) => {
            let opened = c.open_library(l, ignore_env);
            if opened {
                // What `orbit-std` adds to the runtime's own libraries.
                let added = c.with_scope(|s| match l {
                    Library::Base => orbit_std::open_base(s),
                    Library::Package => orbit_std::open_package(s, ignore_env),
                    Library::Debug => orbit_std::open_debug(s),
                    _ => Ok(()),
                });
                debug_assert!(added.is_ok(), "opening a library raised {added:?}");
                if l == Library::Package {
                    crate::searchers::install_searchers(c, false);
                }
            }
            opened
        }
    };
    if !opened {
        c.set_top(-3)?;
        return Ok(0);
    }
    // [globals, before, t]
    if !base {
        c.push_copy(-2);
        c.raw_set_field(-4, name)?;
    }
    c.rotate(-3, 1)?;
    c.set_top(-3)?;
    Ok(1)
}
