//! Rust embedding API for the OrbitScript runtime (ADR-11).
//!
//! A host starts from a [`Runtime`], makes an [`Isolate`] (one VM) from a
//! [`Config`], and loads [`Chunk`]s into it: bytes the host supplies, since
//! the runtime reads no files. Script values are reached through handles
//! that live in a [`Scope`] ([`Isolate::scope`], or the [`Call`] a host
//! function runs in), or that the host keeps with [`Pinned`].
//!
//! ```
//! use orbit::{Chunk, Config, Runtime};
//!
//! let rt = Runtime::new();
//! let mut iso = rt.new_isolate(Config::default());
//! let answer: i64 = iso
//!     .scope(|s| {
//!         let f = s.load(Chunk::new("return 6 * 7").name("=example"))?;
//!         f.call(())?[0].get()
//!     })
//!     .unwrap();
//! assert_eq!(answer, 42);
//! ```

// stabby's trait macros nest deeply for the boundary's larger traits.
#![recursion_limit = "1024"]

#[cfg(not(any(runtime_backend, shared_backend)))]
compile_error!("orbit needs a backend: enable the `runtime` or the `shared` feature");

/// The `liborbit` boundary, for `orbit-lib` and the shared backend.
#[cfg(any(feature = "abi", shared_backend))]
#[doc(hidden)]
pub mod abi;
mod chunk;
mod class;
mod compiler;
#[cfg(feature = "debugger")]
pub mod debugger;
mod error;
/// liborbit's side of the boundary, for `orbit-lib`.
#[cfg(all(feature = "abi", runtime_backend))]
#[doc(hidden)]
pub mod export;
mod host;
mod isolate;
mod pin;
mod scope;
pub mod stack;
mod value;

pub use chunk::{Chunk, Mode};
pub use class::{Class, ClassBuilder};
pub use compiler::{Compiler, Language, SyntaxError};
pub use error::{Error, ErrorKind};
pub use host::{Call, HostFunction, Ret};
#[cfg(feature = "verify")]
pub use isolate::BinaryChecks;
pub use isolate::{Config, Debugger, Gc, GcMode, Interrupt, Isolate, Library, Runtime};
pub use orbit_macros::native;
pub use orbit_runtime::locale::{Category as LocaleCategory, LocaleMode, LocaleScope, TimeParts};
pub use pin::{PinKind, Pinned};
pub use scope::Scope;
pub use value::{
    FromValue, FromValueError, Function, IntoValue, IntoValues, Pairs, Resumed, Str, Table, Thread,
    ThreadStatus, Userdata, Value,
};

/// Byte strings for natives and embedders (ADR-5): Lua strings need not be
/// UTF-8.
pub use orbit_runtime::object::{OrbitStr, OrbitString};

/// Compiled chunks, as [`Compiler`] returns them: plain data to save
/// (`orbitc`) or inspect.
pub use orbit_compiler::bytecode::UnlinkedChunk;

/// The runtime version, taken from the workspace version.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[cfg(test)]
mod tests {
    use super::*;

    fn isolate() -> Isolate {
        Runtime::new().new_isolate(Config::default())
    }

    #[test]
    fn runs_a_chunk() {
        let mut iso = isolate();
        iso.scope(|s| {
            let r = s
                .load(Chunk::new("local a = 40 return a + 2, 'x' .. 1"))?
                .call(())?;
            assert_eq!(r[0].get::<i64>()?, 42);
            assert_eq!(r[1].get::<String>()?, "x1");
            Ok::<_, Error>(())
        })
        .unwrap();
    }

    #[test]
    #[cfg(feature = "load")]
    fn load_compiles_at_run_time() {
        let mut iso = isolate();
        let r: Vec<String> = iso
            .scope(|s| {
                let r = s
                    .load(Chunk::new(
                        "local f = load('return 1 + ...') \
                         local env = {} \
                         load('y = 5', 'c', 't', env)() \
                         local parts = {'return ', '4', '2'} \
                         local h = load(function() return table.remove(parts, 1) end) \
                         return f(41), select(2, load('x = = 1')), env.y, h()",
                    ))?
                    .call(())?;
                r.into_iter().map(|v| v.get()).collect::<Result<_, _>>()
            })
            .unwrap();
        assert_eq!(
            r,
            [
                "42",
                "[string \"x = = 1\"]:1: unexpected symbol near '='",
                "5",
                "42"
            ]
        );
    }

    #[test]
    fn reports_errors() {
        let mut iso = isolate();
        let e = iso
            .exec(Chunk::new("local t = nil\nreturn t.x").name("=t"))
            .unwrap_err();
        assert_eq!(e.kind(), ErrorKind::Runtime);
        assert_eq!(
            e.to_string(),
            "t:2: attempt to index a nil value (local 't')"
        );
        let e = iso.exec(Chunk::new("x = = 1").name("=t")).unwrap_err();
        assert_eq!(e.kind(), ErrorKind::Syntax);
        assert_eq!(e.to_string(), "t:1: unexpected symbol near '='");
    }

    #[test]
    fn error_values_survive_the_scope() {
        let mut iso = isolate();
        let e = iso.exec(Chunk::new("error({code = 7})")).unwrap_err();
        let code: i64 = iso
            .scope(|s| {
                let v = e.value().expect("a script error keeps its value").get(s)?;
                v.get::<Table>()?.get("code")
            })
            .unwrap();
        assert_eq!(code, 7);
    }

    #[test]
    fn host_functions_take_and_return_values() {
        let mut iso = isolate();
        iso.scope(|s| {
            let add = s.create_function(|c| {
                let a: i64 = c.check(0)?;
                let b: i64 = c.opt(1, 1)?;
                c.ret((a + b, "done"))
            });
            s.globals().set("add", add)?;
            let r = s
                .load(Chunk::new(
                    "local a, b = add(40, 2) \
                     local ok, e = pcall(add, 'x') \
                     return a, b, add(1), e",
                ))?
                .call(())?;
            assert_eq!(r[0].get::<i64>()?, 42);
            assert_eq!(r[1].get::<String>()?, "done");
            assert_eq!(r[2].get::<i64>()?, 2);
            // Called by `pcall`, `add` is named by its global, as in Lua.
            assert_eq!(
                r[3].get::<String>()?,
                "bad argument #1 to 'add' (number expected, got string)"
            );
            Ok::<_, Error>(())
        })
        .unwrap();
    }

    #[test]
    fn host_errors_and_panics_become_script_errors() {
        let mut iso = isolate();
        iso.scope(|s| {
            let g = s.globals();
            g.set("fail", s.create_function(|c| Err(c.error("no luck"))))?;
            g.set("boom", s.create_function(|_| panic!("kaboom")))?;
            let r = s
                .load(
                    Chunk::new(
                        "return select(2, pcall(function() fail() end)), select(2, pcall(boom))",
                    )
                    .name("=c"),
                )?
                .call(())?;
            assert_eq!(r[0].get::<String>()?, "c:1: no luck");
            assert_eq!(r[1].get::<String>()?, "host function panicked: kaboom");
            Ok::<_, Error>(())
        })
        .unwrap();
    }

    #[test]
    fn host_functions_call_back_into_scripts() {
        let mut iso = isolate();
        iso.scope(|s| {
            let twice = s.create_function(|c| {
                let f: Function = c.check(0)?;
                let x: i64 = f.call(1)?[0].get()?;
                let y: i64 = f.call(x)?[0].get()?;
                c.ret(y)
            });
            s.globals().set("twice", twice)?;
            let r = s
                .load(Chunk::new("return twice(function(n) return n * 10 end)"))?
                .call(())?;
            assert_eq!(r[0].get::<i64>()?, 100);
            Ok::<_, Error>(())
        })
        .unwrap();
    }

    #[test]
    fn tables_with_and_without_metamethods() {
        let mut iso = isolate();
        iso.scope(|s| {
            let t = s.create_table();
            t.set("a", 1)?;
            t.push("x")?;
            t.push("y")?;
            assert_eq!(t.raw_len(), 2);
            let mt: Table = s
                .load(Chunk::new(
                    "return {__index = function(_, k) return k .. '!' end, \
                             __len = function() return 99 end}",
                ))?
                .call(())?[0]
                .get()?;
            t.set_metatable(Some(mt));
            assert_eq!(t.get::<String>("b")?, "b!");
            assert_eq!(t.raw_get::<Option<String>>("b")?, None);
            assert_eq!(t.len()?, 99);
            let mut keys = 0;
            for kv in t.pairs() {
                kv?;
                keys += 1;
            }
            assert_eq!(keys, 3);
            Ok::<_, Error>(())
        })
        .unwrap();
    }

    #[test]
    fn pinned_handles_outlive_scopes_and_isolates() {
        let mut iso = isolate();
        let t = iso.scope(|s| {
            let t = s.create_table();
            t.set("n", 5).unwrap();
            t.pin()
        });
        iso.gc().collect();
        let n: i64 = iso.scope(|s| t.get(s)?.get("n")).unwrap();
        assert_eq!(n, 5);
        let mut other = isolate();
        assert!(other.scope(|s| t.get(s).is_err()));
        drop(iso);
        assert!(other.scope(|s| t.get(s).is_err()));
    }

    #[test]
    #[cfg(feature = "coroutine")]
    fn coroutines_resume_from_the_host() {
        let mut iso = isolate();
        iso.scope(|s| {
            let f = s
                .load(Chunk::new(
                    "return function(a) local b = coroutine.yield(a + 1) return b * 2 end",
                ))?
                .call(())?[0]
                .get::<Function>()?;
            let co = s.create_thread(f);
            match co.resume(1)? {
                Resumed::Yielded(v) => assert_eq!(v[0].get::<i64>()?, 2),
                r => panic!("{r:?}"),
            }
            match co.resume(5)? {
                Resumed::Returned(v) => assert_eq!(v[0].get::<i64>()?, 10),
                r => panic!("{r:?}"),
            }
            assert_eq!(co.status(), ThreadStatus::Dead);
            Ok::<_, Error>(())
        })
        .unwrap();
    }

    #[test]
    #[cfg(feature = "string")]
    fn classes_have_methods_and_finalizers() {
        use std::rc::Rc;

        struct Counter {
            n: i64,
            _drops: Rc<()>,
        }
        let drops = Rc::new(());
        let mut iso = isolate();
        let class = iso
            .scope(|s| {
                s.define_class::<Counter>("Counter", |c| {
                    c.method("inc", |me, call| {
                        me.n += call.opt(1, 1)?;
                        call.ret(me.n)
                    })?;
                    Ok(())
                })
            })
            .unwrap();
        iso.scope(|s| {
            let c = class.instance(
                s,
                Counter {
                    n: 40,
                    _drops: drops.clone(),
                },
            )?;
            s.globals().set("c", c)?;
            let r = s
                .load(Chunk::new(
                    "c:inc() local n = c:inc() \
                     return n, select(2, pcall(function() return c.inc({}) end)), tostring(c):match('^Counter')",
                ).name("=c"))?
                .call(())?;
            assert_eq!(r[0].get::<i64>()?, 42);
            assert_eq!(
                r[1].get::<String>()?,
                "c:1: bad argument #1 to 'inc' (Counter expected, got table)"
            );
            assert_eq!(r[2].get::<String>()?, "Counter");
            assert_eq!(c.borrow::<Counter>()?.n, 42);
            s.globals().set("c", Value::Nil)?;
            Ok::<_, Error>(())
        })
        .unwrap();
        assert_eq!(Rc::strong_count(&drops), 2);
        iso.gc().collect();
        assert_eq!(Rc::strong_count(&drops), 1);
    }

    #[test]
    fn tracebacks_on_request() {
        let mut iso = isolate();
        iso.scope(|s| {
            let f = s.load(Chunk::new("local function f() error('deep') end f()").name("=tb"))?;
            let e = f.call_traced(()).unwrap_err();
            assert_eq!(e.message(), "tb:1: deep");
            let tb = e.traceback().expect("a traceback");
            assert!(tb.starts_with("stack traceback:"), "{tb}");
            assert!(tb.contains("tb:1: in local 'f'"), "{tb}");
            assert!(f.call(()).unwrap_err().traceback().is_none());
            Ok::<_, Error>(())
        })
        .unwrap();
    }

    #[test]
    #[cfg(feature = "package")]
    fn modules_and_environments() {
        let mut iso = isolate();
        iso.scope(|s| {
            let m = s.create_table();
            m.set("answer", 42)?;
            s.register_module("deep", m);
            let env = s.create_table();
            env.set("require", s.globals().get::<Value>("require")?)?;
            let r = s
                .load_in(Chunk::new("x = require('deep').answer return x"), env)?
                .call(())?;
            assert_eq!(r[0].get::<i64>()?, 42);
            assert_eq!(env.get::<i64>("x")?, 42);
            assert!(s.globals().get::<Value>("x")?.is_nil());
            // Any value will do as an environment, as for `load`.
            let r = s.load_in(Chunk::new("return _ENV"), 7)?.call(())?;
            assert_eq!(r[0].get::<i64>()?, 7);
            Ok::<_, Error>(())
        })
        .unwrap();
    }

    #[test]
    fn field_sites_past_the_cache_limit_read_their_own_keys() {
        // Past 65534 sites in one function, the last inline cache is shared
        // by sites with different keys; each must still read its own field.
        let mut src = String::from("local t = {a = 1, b = 2, c = 3} local x\n");
        for _ in 0..65534 {
            src.push_str("x = t.a\n");
        }
        src.push_str("local s = 0\n");
        for _ in 0..3 {
            src.push_str("s = s + t.b * 10 + t.c\nt.b = t.b + 1\n");
        }
        src.push_str("return s, t.a, t.b");
        let mut iso = isolate();
        let r: Vec<i64> = iso
            .scope(|s| {
                let r = s.load(Chunk::new(&src))?.call(())?;
                r.into_iter().map(|v| v.get()).collect::<Result<_, _>>()
            })
            .unwrap();
        assert_eq!(r, [99, 1, 5]);
    }
}
