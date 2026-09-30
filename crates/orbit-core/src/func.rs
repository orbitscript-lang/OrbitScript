//! Functions: closures, the cells captured locals live in, and linked
//! prototypes.

use std::cell::Cell as StdCell;
use std::rc::Rc;

use orbit_bytecode::{CaptureSource, DebugInfo, FunctionName, Op};
use orbit_gc::{Gc, Marker, Trace};

use crate::table::mark;
use crate::{LuaSpace, Value};

/// A captured local. Every closure that captures the binding holds the same
/// cell ([VM design, "Locals, captures and cells"](https://github.com/tymlipari/OrbitScript/blob/main/docs/design/vm.md#locals-captures-and-cells)).
#[derive(Debug, Default)]
pub struct Cell {
    pub value: Value,
}

impl Trace<LuaSpace> for Cell {
    fn trace(&self, m: &mut Marker<'_, LuaSpace>) {
        mark(m, self.value);
    }
}

/// A Lua function value: a prototype plus its captured cells.
#[derive(Debug)]
pub struct Closure {
    pub proto: Gc<Proto>,
    pub captures: Box<[Gc<Cell>]>,
}

impl Trace<LuaSpace> for Closure {
    fn trace(&self, m: &mut Marker<'_, LuaSpace>) {
        m.mark(self.proto);
        for &c in &self.captures {
            m.mark(c);
        }
    }

    fn extra_bytes(&self) -> usize {
        self.captures.len() * std::mem::size_of::<Gc<Cell>>()
    }
}

/// A linked function prototype. The code, constants and caches are behind
/// `Rc` so the interpreter can hold them while it runs without going through
/// the heap.
#[derive(Debug)]
pub struct Proto {
    pub code: Rc<[Op]>,
    pub consts: Rc<[Value]>,
    /// Inline-cache slots, indexed by the `ic` operands: the hash slot where
    /// the key was last found.
    pub caches: Rc<[StdCell<u32>]>,
    /// Per-instruction type feedback (`type-feedback` feature): bits for the
    /// operand types each instruction has seen.
    #[cfg(feature = "type-feedback")]
    pub feedback: Rc<[StdCell<u8>]>,
    pub children: Box<[Gc<Proto>]>,
    pub captures: Box<[CaptureSource]>,
    pub nparams: u8,
    pub is_vararg: bool,
    pub nregs: u8,
    pub ncells: u8,
    pub name: FunctionName,
    pub line_defined: u32,
    /// Chunk name for messages (`script.lua` in `script.lua:3:`).
    pub chunk: Rc<str>,
    pub debug: Rc<DebugInfo>,
}

impl Trace<LuaSpace> for Proto {
    fn trace(&self, m: &mut Marker<'_, LuaSpace>) {
        for &k in self.consts.iter() {
            mark(m, k);
        }
        for &c in &self.children {
            m.mark(c);
        }
    }

    fn extra_bytes(&self) -> usize {
        self.code.len() * std::mem::size_of::<Op>()
            + self.consts.len() * std::mem::size_of::<Value>()
            + self.caches.len() * 4
    }
}
