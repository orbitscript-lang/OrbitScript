//! Compiles a parsed Lua 5.5 chunk to register bytecode (stream C, ADR-4).
//!
//! The code generator is a port of the reference's `lcode.c` and the code-emitting half
//! of `lparser.c`, driven by the syntax tree from `orbit-lua-syntax` instead of by the
//! token stream. The tree already has every name resolved (locals, upvalues,
//! compile-time constants, globals) and every static error reported, so this pass only
//! assigns registers and emits instructions. Mirroring the reference instruction for
//! instruction keeps run-time behavior identical, including the variable names that
//! error messages recover from bytecode.
//!
//! The output ([`CompiledFn`]) holds no heap values: constants are plain Rust data, and
//! the VM moves them into its heap when it loads the chunk.
//!
//! ```
//! let chunk = orbit_lua_syntax::parse(b"local x = 1; return x + 2").unwrap();
//! let f = orbit_compiler::compile(&chunk).unwrap();
//! assert!(f.info.is_vararg());
//! assert_eq!(f.info.upvalues.len(), 1); // _ENV
//! ```

mod codegen;

pub use orbit_bytecode as bytecode;
use orbit_bytecode::FnInfo;
use orbit_lua_syntax::{Chunk, SyntaxError};

/// A constant in a compiled function.
#[derive(Clone, Debug, PartialEq)]
pub enum Constant {
    /// `nil`
    Nil,
    /// `true` or `false`
    Bool(bool),
    /// An integer.
    Int(i64),
    /// A float.
    Float(f64),
    /// A string.
    Str(Box<[u8]>),
}

/// A compiled function: code and debug information, constants and nested functions.
#[derive(Clone, Debug, PartialEq)]
pub struct CompiledFn {
    /// Code and debug information.
    pub info: FnInfo,
    /// The constant table.
    pub constants: Vec<Constant>,
    /// Nested functions, indexed by `CLOSURE`'s operand.
    pub protos: Vec<CompiledFn>,
}

/// Compiles a chunk's main function.
///
/// Errors are the few limits only code generation can detect, such as a function that
/// needs more than 255 registers. They use the reference's messages.
pub fn compile(chunk: &Chunk) -> Result<CompiledFn, SyntaxError> {
    codegen::compile(chunk)
}

/// A listing of a function's code, one instruction per line, for tests and debugging.
pub fn listing(f: &CompiledFn) -> String {
    let mut out = String::new();
    for (pc, &i) in f.info.code.iter().enumerate() {
        let line = f.info.lines.get(pc).copied().unwrap_or(0);
        out.push_str(&format!(
            "{:>4} [{line}] {}\n",
            pc + 1,
            orbit_bytecode::disassemble(i)
        ));
    }
    out
}
