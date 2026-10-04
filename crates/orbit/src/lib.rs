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

pub use orbit_core::Value;
pub use orbit_vm::{LuaError, Vm};

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
    orbit_compiler::compile(&hir).map_err(|e| Error::Syntax(e.to_string()))
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
        let mut vm = Vm::new();
        orbit_stdlib::open_libs(&mut vm);
        Lua { vm }
    }

    /// The underlying VM, for registering natives and building values.
    pub fn vm(&mut self) -> &mut Vm {
        &mut self.vm
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
