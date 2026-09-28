//! Function, upvalue, userdata, prototype and coroutine objects.
//!
//! These are the GC-managed objects other than strings and tables. Their layout is what
//! the collector traces; the compiler and VM (stream C) own what goes inside a prototype
//! and a coroutine, and will grow those types as they are written.

use crate::value::{NativeId, StrRef, TableRef, ThreadRef, Value};
use orbit_gc::Gc;
use std::any::Any;
use std::fmt;

/// Handle to a function prototype.
pub type ProtoRef = Gc<Proto>;
/// Handle to an upvalue.
pub type UpValRef = Gc<UpVal>;

/// A compiled Lua function body: bytecode, constants and nested prototypes.
///
/// Placeholder shape for stream C: it holds the fields the collector must trace
/// (constants, nested prototypes, source name). The compiler adds code and debug info.
#[derive(Debug, Default)]
pub struct Proto {
    /// Constants referenced by the bytecode.
    pub constants: Box<[Value]>,
    /// Prototypes of nested functions.
    pub protos: Box<[ProtoRef]>,
    /// Chunk name, for error messages and `debug.getinfo`.
    pub source: Option<StrRef>,
    /// Number of upvalues a closure of this prototype has.
    pub num_upvalues: u8,
}

/// A captured variable. It is *open* while the variable still lives in a coroutine's
/// stack slot, and *closed* (holding its own copy) after the enclosing block exits.
#[derive(Clone, Copy, Debug)]
pub enum UpVal {
    /// Refers to stack slot `slot` of `thread`.
    Open {
        /// The coroutine whose stack holds the variable.
        thread: ThreadRef,
        /// Absolute stack index.
        slot: u32,
    },
    /// The variable's value, after the stack slot went away.
    Closed(Value),
}

/// A Lua function: a prototype plus its upvalues.
#[derive(Debug)]
pub struct LuaClosure {
    /// The function body.
    pub proto: ProtoRef,
    /// Upvalues, in the prototype's order.
    pub upvals: Box<[UpValRef]>,
}

/// A native function with upvalues (Lua's C closure).
#[derive(Debug)]
pub struct NativeClosure {
    /// Index into the VM's native function registry.
    pub func: NativeId,
    /// Upvalues, readable and writable by the function.
    pub upvals: Box<[Value]>,
}

/// A full userdata: a Rust value owned by the GC heap, with a metatable and user values.
pub struct UserData {
    /// The metatable, which gives the userdata its behavior.
    pub metatable: Option<TableRef>,
    /// Lua values attached to the userdata (`debug.setuservalue`).
    pub user_values: Box<[Value]>,
    /// The Rust payload.
    pub data: Box<dyn Any>,
}

impl UserData {
    /// A userdata holding `data`, with no metatable and `nuv` user values.
    pub fn new<T: Any>(data: T, nuv: usize) -> Self {
        UserData {
            metatable: None,
            user_values: vec![Value::Nil; nuv].into_boxed_slice(),
            data: Box::new(data),
        }
    }

    /// The payload, if it has type `T`.
    pub fn downcast_ref<T: Any>(&self) -> Option<&T> {
        self.data.downcast_ref()
    }

    /// The payload, mutably, if it has type `T`.
    pub fn downcast_mut<T: Any>(&mut self) -> Option<&mut T> {
        self.data.downcast_mut()
    }
}

impl fmt::Debug for UserData {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UserData")
            .field("metatable", &self.metatable)
            .field("user_values", &self.user_values)
            .finish_non_exhaustive()
    }
}

/// A coroutine.
///
/// Placeholder shape for stream C (ADR-3): it holds the value stack the collector must
/// trace. The VM adds frames, status and the open-upvalue list.
#[derive(Debug, Default)]
pub struct Thread {
    /// The value stack.
    pub stack: Vec<Value>,
}
