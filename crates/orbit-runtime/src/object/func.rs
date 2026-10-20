//! Functions: closures, the cells captured locals live in, and linked
//! prototypes.

use std::cell::Cell as StdCell;
use std::rc::Rc;

use orbit_bytecode::{CaptureSource, DebugInfo, FunctionName, Insn, Op};
use orbit_gc::{Gc, Marker, Trace};

use crate::object::table::mark;
use crate::object::{LuaSpace, LuaStr, Value};

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
    /// The prototype's [`Proto::code`], so a call reaches the code and frame
    /// sizes without a second heap lookup.
    pub code: Rc<ProtoCode>,
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

/// What the interpreter needs to run a prototype: its code, constants,
/// caches and frame sizes. Shared through an `Rc` so the dispatch loop can
/// hold it while it runs without going through the heap, and one clone covers
/// all of it.
#[derive(Debug)]
pub struct ProtoCode {
    pub code: Box<[Insn]>,
    /// Kept alive for the collector by the owning [`Proto`], which traces
    /// them.
    pub consts: Box<[Value]>,
    /// Inline caches, indexed by the `ic` operands.
    pub caches: Box<[FieldCache]>,
    /// Per-instruction type feedback (`type-feedback` feature): bits for the
    /// operand types each instruction has seen.
    #[cfg(feature = "type-feedback")]
    pub feedback: Box<[StdCell<u8>]>,
    pub nparams: u8,
    pub is_vararg: bool,
    pub nregs: u8,
    pub ncells: u8,
    /// Part of a built-in library written in OrbitScript: the debug
    /// interface shows its activations as C functions (ADR-11).
    pub builtin: bool,
    /// Nothing may yield across its activations (`attempt to yield across
    /// a C-call boundary`): set for every built-in but the few whose C
    /// counterparts in the reference allow it.
    pub refuses_yield: bool,
    /// The debugger's mark: the breakpoint generation this code was last
    /// checked against, shifted left one, and whether it has a breakpoint
    /// in bit 0 (0 when never checked).
    #[cfg(feature = "debugger")]
    pub debug_mark: StdCell<u32>,
}

impl ProtoCode {
    /// A copy in which [`Op::CapSelf`] reads capture `cap` instead, for a
    /// closure whose self capture no longer holds it (a `debug` write).
    pub fn without_cap_self(&self, cap: u8) -> ProtoCode {
        let code = self
            .code
            .iter()
            .map(|&i| match i.decode() {
                Op::CapSelf { dst } => Op::CapGet { dst, cap }.encode(),
                _ => i,
            })
            .collect();
        self.with_code(code)
    }

    /// A copy that code it calls may yield across.
    pub fn allowing_yields(&self) -> ProtoCode {
        ProtoCode {
            refuses_yield: false,
            ..self.with_code(self.code.clone())
        }
    }

    fn with_code(&self, code: Box<[Insn]>) -> ProtoCode {
        ProtoCode {
            code,
            consts: self.consts.clone(),
            caches: self.caches.clone(),
            #[cfg(feature = "type-feedback")]
            feedback: self.feedback.clone(),
            nparams: self.nparams,
            is_vararg: self.is_vararg,
            nregs: self.nregs,
            ncells: self.ncells,
            builtin: self.builtin,
            refuses_yield: self.refuses_yield,
            #[cfg(feature = "debugger")]
            debug_mark: StdCell::new(0),
        }
    }
}

/// An inline cache for a field or global access: the instruction's string
/// key, so the hit path needs no constant lookup, and the hash slot where
/// the key was last found.
#[derive(Debug, Clone)]
pub struct FieldCache {
    /// Kept alive by the prototype's constants, which hold the same string.
    pub key: Gc<LuaStr>,
    /// `key`'s hash, so a miss can probe without reading the string.
    pub hash: u32,
    /// The hash slot, `u32::MAX` before the first fill.
    pub slot: StdCell<u32>,
    /// Several instructions share this cache (a function with more sites
    /// than cache indexes), so `key` isn't theirs: it never fills, and every
    /// access takes the miss path with the instruction's own key.
    pub shared: bool,
}

impl FieldCache {
    pub fn new(heap: &crate::object::LuaHeap, key: Gc<LuaStr>) -> FieldCache {
        FieldCache {
            key,
            hash: heap.get(key).hash(),
            slot: StdCell::new(u32::MAX),
            shared: false,
        }
    }

    /// The table key for string `k`, the instruction's own key: from the
    /// cache when the cache is the instruction's alone (no string read).
    #[inline]
    pub fn key_for(
        &self,
        heap: &crate::object::LuaHeap,
        k: Gc<LuaStr>,
    ) -> crate::object::table::Key {
        if self.shared {
            crate::object::table::Key::str(heap, k)
        } else {
            crate::object::table::Key::str_hashed(self.key, self.hash)
        }
    }

    /// Remember slot `slot` for the next access.
    #[inline]
    pub fn fill(&self, slot: usize) {
        if !self.shared {
            self.slot.set(slot as u32);
        }
    }
}

/// A linked function prototype.
#[derive(Debug)]
pub struct Proto {
    pub code: Rc<ProtoCode>,
    pub children: Box<[Gc<Proto>]>,
    pub captures: Box<[CaptureSource]>,
    pub name: FunctionName,
    pub line_defined: u32,
    /// Line of the function's `end` (0 for a main chunk).
    pub last_line: u32,
    /// Chunk name for messages (`script.lua` in `script.lua:3:`).
    pub chunk: Rc<str>,
    /// The chunk's source as given to the loader (`@script.lua`), shared
    /// by all its prototypes.
    pub source: Rc<[u8]>,
    /// The capture [`Op::CapSelf`] stands for, if the function has one.
    pub self_capture: Option<u8>,
    pub debug: Rc<DebugInfo>,
}

impl Trace<LuaSpace> for Proto {
    fn trace(&self, m: &mut Marker<'_, LuaSpace>) {
        for &k in self.code.consts.iter() {
            mark(m, k);
        }
        for &c in &self.children {
            m.mark(c);
        }
    }

    fn extra_bytes(&self) -> usize {
        let c = &self.code;
        c.code.len() * std::mem::size_of::<Insn>()
            + c.consts.len() * std::mem::size_of::<Value>()
            + c.caches.len() * std::mem::size_of::<FieldCache>()
    }
}
