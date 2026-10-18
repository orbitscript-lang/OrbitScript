//! [`Pinned`]: handles a host keeps outside any scope.

use std::fmt;
use std::marker::PhantomData;
use std::rc::{Rc, Weak};

use crate::error::Error;
use crate::scope::{Scope, Shared};
use crate::value::{Function, Str, Table, Thread, Userdata, Value};

/// The handle types that can be pinned. `Pinned<Table<'static>>` keeps a
/// table; [`Pinned::get`] gives it back as a `Table<'s>` in a scope.
pub trait PinKind: 'static {
    /// This handle type in scope `'s`.
    type At<'s>;
    #[doc(hidden)]
    fn at(v: Value<'_>) -> Option<Self::At<'_>>;
}

macro_rules! pin_kind {
    ($($t:ident => $as:ident),* $(,)?) => {$(
        impl PinKind for $t<'static> {
            type At<'s> = $t<'s>;
            fn at(v: Value<'_>) -> Option<$t<'_>> {
                v.$as()
            }
        }
    )*};
}

pin_kind!(
    Table => as_table,
    Function => as_function,
    Str => as_str,
    Thread => as_thread,
    Userdata => as_userdata,
);

impl PinKind for Value<'static> {
    type At<'s> = Value<'s>;
    fn at(v: Value<'_>) -> Option<Value<'_>> {
        Some(v)
    }
}

/// A handle that keeps its value alive until it is dropped, for a host to
/// store in its own structs. Using it takes a scope of the same isolate.
///
/// Once its isolate has closed, a pinned handle is inert: [`Pinned::get`]
/// fails and dropping it does nothing.
pub struct Pinned<K: PinKind> {
    iso: Weak<Shared>,
    slot: u32,
    _kind: PhantomData<fn() -> K>,
}

impl<K: PinKind> Pinned<K> {
    pub(crate) fn new(shared: &Rc<Shared>, slot: u32) -> Pinned<K> {
        Pinned {
            iso: Rc::downgrade(shared),
            slot,
            _kind: PhantomData,
        }
    }

    /// The slot, if this handle belongs to the isolate `shared` belongs to.
    pub(crate) fn slot_in(&self, shared: &Rc<Shared>) -> Option<u32> {
        std::ptr::eq(self.iso.as_ptr(), Rc::as_ptr(shared)).then_some(self.slot)
    }

    /// The value as a handle in scope `s`. Fails if the handle belongs to
    /// another isolate, or its isolate has closed.
    pub fn get<'s>(&self, s: &'s Scope<'s>) -> Result<K::At<'s>, Error> {
        let Some(slot) = self.slot_in(&s.shared) else {
            return Err(Error::runtime(if self.iso.strong_count() == 0 {
                "pinned handle used after its isolate closed"
            } else {
                "pinned handle used in another isolate"
            }));
        };
        let v = s.ctx().vm.pin_slot_value(slot);
        Ok(K::at(s.root(v)).expect("a pin slot holds the kind it was pinned as"))
    }
}

impl<K: PinKind> Drop for Pinned<K> {
    fn drop(&mut self) {
        if let Some(shared) = self.iso.upgrade() {
            shared.released.borrow_mut().push(self.slot);
        }
    }
}

impl<K: PinKind> fmt::Debug for Pinned<K> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Pinned")
            .field("slot", &self.slot)
            .finish_non_exhaustive()
    }
}
