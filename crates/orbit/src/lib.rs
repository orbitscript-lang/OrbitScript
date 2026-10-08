//! Rust embedding API for the OrbitScript runtime ([VM design, "Embedding and
//! CLI"](https://github.com/tymlipari/OrbitScript/blob/main/docs/design/vm.md#embedding-and-cli)).
//!
//! ```
//! let mut lua = orbit::Lua::new();
//! lua.exec(b"x = 6 * 7", "example").unwrap();
//! ```

use std::fmt;

use orbit_compiler::bytecode::UnlinkedChunk;
use orbit_lua_syntax::ParseOptions;

pub use orbit_core;
pub use orbit_core::Value;
pub use orbit_stdlib::Options;
/// How much `load` checks binary chunks (features `verify`, `verify-flow`;
/// `ORBIT_VERIFY`).
#[cfg(feature = "dump")]
pub use orbit_stdlib::checks;
#[cfg(feature = "load")]
pub use orbit_stdlib::{chunk_id, read_chunk_file};
#[cfg(feature = "debug")]
pub use orbit_vm::Interrupt;
pub use orbit_vm::{
    CallOutcome, Event, GcMode, LIMIT_GRACE, LuaError, NativeCtx, NativeReturn, Vm,
};

/// The runtime version, taken from the workspace version.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Why loading or running a chunk failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// A syntax or compile error: `chunk:line: message`.
    Syntax(String),
    /// An uncaught runtime error: the message as the standalone interpreter
    /// prints it, and a traceback.
    Runtime {
        message: String,
        traceback: Option<String>,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Syntax(m) => f.write_str(m),
            Error::Runtime { message, .. } => f.write_str(message),
        }
    }
}

impl std::error::Error for Error {}

/// Parse, lower, analyse and compile Lua source into plain data. Needs no
/// VM, so it can run on any thread.
pub fn compile(
    src: &[u8],
    chunk_name: &str,
    options: &ParseOptions,
) -> Result<UnlinkedChunk, Error> {
    let hir = orbit_lua_syntax::to_hir(chunk_name, src, options)
        .map_err(|d| Error::Syntax(d.to_lua_message(chunk_name)))?;
    let chunk = orbit_compiler::compile(&hir).map_err(|e| Error::Syntax(e.to_string()))?;
    // Debug builds check the compiler against the binary-chunk verifier, so
    // every test that compiles Lua keeps the two in agreement.
    #[cfg(debug_assertions)]
    if let Err(e) = orbit_compiler::bytecode::verify::verify(
        &chunk,
        orbit_compiler::bytecode::verify::Level::STRONGEST,
    ) {
        panic!("compiler output fails verification: {e}");
    }
    Ok(chunk)
}

/// The compiler `load` and friends use: the Lua front end with default
/// options ([`orbit_vm::CompileFn`]).
fn compile_lua(src: &[u8], chunk_name: &str) -> Result<UnlinkedChunk, String> {
    compile(src, chunk_name, &ParseOptions::default()).map_err(|e| e.to_string())
}

/// A Lua state with the first-light libraries open.
#[derive(Debug)]
pub struct Lua {
    vm: Vm,
}

impl Default for Lua {
    fn default() -> Self {
        Lua::new()
    }
}

impl Lua {
    pub fn new() -> Self {
        Lua::with_options(Options::default())
    }

    /// [`Lua::new`] with library [`Options`].
    pub fn with_options(options: Options) -> Self {
        let mut vm = Vm::new();
        vm.rt.compile = Some(compile_lua);
        orbit_stdlib::open_libs_with(&mut vm, options);
        Lua { vm }
    }

    /// The underlying VM, for registering natives and building values.
    pub fn vm(&mut self) -> &mut Vm {
        &mut self.vm
    }

    /// Cap the Lua heap at `bytes` (`None`, the default: no cap). A script
    /// that needs more, even after a full collection, gets the catchable
    /// error `not enough memory`. The count is `collectgarbage("count")`'s:
    /// Lua objects and what they own, not the VM's other allocations. See
    /// [`Vm::set_memory_limit`].
    pub fn set_memory_limit(&mut self, bytes: Option<usize>) {
        self.vm.set_memory_limit(bytes);
    }

    /// Compile `src` and link it, returning the main function. The function
    /// is unrooted: call it (or pin it on the VM) before running other code.
    pub fn load(&mut self, src: &[u8], chunk_name: &str) -> Result<Value, Error> {
        self.load_with(src, chunk_name, &ParseOptions::default())
    }

    /// [`Lua::load`] with parse options.
    pub fn load_with(
        &mut self,
        src: &[u8],
        chunk_name: &str,
        options: &ParseOptions,
    ) -> Result<Value, Error> {
        let chunk = compile(src, chunk_name, options)?;
        Ok(self.vm.load(&chunk))
    }

    #[cfg(feature = "load")]
    /// Load `bytes`, source text or a binary chunk, as a chunk whose Lua
    /// source name is `source` (`@file`, `=name`), as `loadfile` would. The
    /// function is unrooted.
    pub fn load_buffer(&mut self, bytes: &[u8], source: &[u8]) -> Result<Value, Error> {
        orbit_stdlib::load_buffer(&mut self.vm, bytes, source).map_err(Error::Syntax)
    }

    /// Flush open files and `print` output, as closing the state does.
    pub fn flush_files(&mut self) {
        orbit_stdlib::flush_files(&mut self.vm);
    }

    /// Call a function.
    pub fn call(&mut self, f: Value, args: &[Value]) -> Result<Vec<Value>, Error> {
        self.vm.call(f, args).map_err(|e| Error::Runtime {
            message: self.vm.error_message(&e),
            traceback: e.traceback.clone(),
        })
    }

    /// Load and run a chunk.
    pub fn exec(&mut self, src: &[u8], chunk_name: &str) -> Result<Vec<Value>, Error> {
        let f = self.load(src, chunk_name)?;
        self.call(f, &[])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runs_a_chunk() {
        let mut lua = Lua::new();
        let r = lua
            .exec(b"local a = 40 return a + 2, 'x' .. 1", "t")
            .unwrap();
        assert!(matches!(r[0], Value::Int(42)));
        assert_eq!(lua.vm().to_display(r[1]), b"x1");
    }

    #[test]
    fn load_compiles_at_run_time() {
        let mut lua = Lua::new();
        let r = lua
            .exec(
                b"local f = load('return 1 + ...') \
                  local g = load('x = = 1') \
                  local env = {} \
                  load('y = 5', 'c', 't', env)() \
                  local parts = {'return ', '4', '2'} \
                  local h = load(function() return table.remove(parts, 1) end) \
                  return f(41), select(2, load('x = = 1')), env.y, h()",
                "t",
            )
            .unwrap();
        assert!(matches!(r[0], Value::Int(42)));
        assert_eq!(
            lua.vm().to_display(r[1]),
            b"[string \"x = = 1\"]:1: unexpected symbol near '='"
        );
        assert!(matches!(r[2], Value::Int(5)));
        assert!(matches!(r[3], Value::Int(42)));
    }

    #[test]
    fn reports_errors() {
        let mut lua = Lua::new();
        let e = lua.exec(b"local t = nil\nreturn t.x", "t").unwrap_err();
        assert_eq!(
            e.to_string(),
            "t:2: attempt to index a nil value (local 't')"
        );
        let e = lua.exec(b"x = = 1", "t").unwrap_err();
        assert!(matches!(e, Error::Syntax(_)));
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
        let mut lua = Lua::new();
        let r = lua.exec(src.as_bytes(), "t").unwrap();
        assert!(matches!(
            r[..],
            [Value::Int(99), Value::Int(1), Value::Int(5)]
        ));
    }
}
