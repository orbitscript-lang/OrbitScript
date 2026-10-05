//! Full userdata: a host Rust value with a metatable ([VM semantics]).
//!
//! [VM semantics]: https://github.com/tymlipari/OrbitScript/blob/main/docs/design/vm-semantics.md#runtime-objects

use std::any::Any;
use std::fmt;

use orbit_gc::{Gc, Marker, Trace};

use crate::table::mark;
use crate::{LuaSpace, Table, Value};

/// A userdata value: any `'static` Rust value, opaque to the collector,
/// plus a metatable and one traced user value. A host that wants the
/// userdata to keep Lua values alive stores them in `user_value`.
pub struct UserData {
    pub meta: Option<Gc<Table>>,
    pub user_value: Value,
    data: Box<dyn Any>,
}

impl UserData {
    pub fn new<T: Any>(data: T) -> Self {
        UserData {
            meta: None,
            user_value: Value::Nil,
            data: Box::new(data),
        }
    }

    /// The host value, if it is a `T`.
    pub fn downcast_ref<T: Any>(&self) -> Option<&T> {
        self.data.downcast_ref()
    }

    /// The host value, mutably, if it is a `T`.
    pub fn downcast_mut<T: Any>(&mut self) -> Option<&mut T> {
        self.data.downcast_mut()
    }

    /// Whether the host value is a `T`.
    pub fn is<T: Any>(&self) -> bool {
        self.data.is::<T>()
    }
}

impl fmt::Debug for UserData {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UserData")
            .field("meta", &self.meta)
            .field("user_value", &self.user_value)
            .finish_non_exhaustive()
    }
}

impl Trace<LuaSpace> for UserData {
    fn trace(&self, m: &mut Marker<'_, LuaSpace>) {
        if let Some(t) = self.meta {
            m.mark(t);
        }
        mark(m, self.user_value);
    }

    fn extra_bytes(&self) -> usize {
        std::mem::size_of_val(&*self.data)
    }
}
