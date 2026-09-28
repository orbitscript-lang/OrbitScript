//! A garbage collector in safe Rust: typed arenas, `u32` handles and an
//! incremental tri-color mark-sweep collector modeled on PUC Lua's `lgc.c`
//! (ADR-2).
//!
//! `orbit-gc` knows nothing about Lua. The runtime declares its object types
//! in a [`space!`], implements [`Trace`] for each, and drives the collector
//! from its own safe points. What the crate provides:
//!
//! - **Arenas and handles.** One paged [`Arena`] per object type. A [`Gc<T>`]
//!   is a 4-byte slot index. Colors live in side byte vectors, so objects
//!   carry no header.
//! - **Incremental marking with barriers.** [`Heap::step`] does work in
//!   proportion to allocation, paced by bytes with Lua 5.5's `pause` and
//!   `stepmul` defaults ([`Params`]). Stores into already-traced objects need
//!   [`Heap::barrier_back`] (or [`Heap::update`]) or [`Heap::barrier_forward`].
//!   Roots are rescanned in the atomic phase, so they need no barrier.
//! - **Weak references and ephemerons.** An object declares its weak mode
//!   while being traced ([`Marker::weak`]) and drops dead entries in
//!   [`Trace::clear_weak`]. Ephemerons converge in the atomic phase.
//! - **Finalization with resurrection.** [`Heap::register_finalizer`] and
//!   [`Heap::take_finalizable`] implement Lua's `__gc` semantics and order.
//! - **Stress mode.** [`Heap::set_stress`] collects at nearly every safe point.
//!
//! # Rooting
//!
//! Allocation never collects. Collection happens only inside [`Heap::step`]
//! and [`Heap::full_collect`], which take the [`Roots`]. Any handle the owner
//! holds across one of those calls must be reachable from those roots, or it
//! may be freed. Using a freed handle panics, or reads whichever object reused
//! the slot; it is never undefined behavior. A borrow-checked rooting layer
//! for embedders belongs in the `orbit` crate, on top of this one.
//!
//! # Example
//!
//! ```
//! use orbit_gc::{Gc, Heap, Marker, Trace, space};
//!
//! #[derive(Default)]
//! struct Node {
//!     next: Option<Gc<Node>>,
//! }
//!
//! space! {
//!     struct Nodes {
//!         nodes: Node,
//!     }
//! }
//!
//! impl Trace<Nodes> for Node {
//!     fn trace(&self, m: &mut Marker<'_, Nodes>) {
//!         if let Some(n) = self.next {
//!             m.mark(n);
//!         }
//!     }
//! }
//!
//! let mut heap = Heap::<Nodes>::new();
//! let a = heap.alloc(Node::default());
//! let b = heap.alloc(Node { next: Some(a) });
//! let garbage = heap.alloc(Node::default());
//! heap.update(garbage, |n| n.next = Some(garbage)); // a cycle
//!
//! heap.full_collect(&mut |m: &mut Marker<'_, Nodes>| m.mark(b));
//! assert!(heap.is_live(a) && heap.is_live(b));
//! assert!(!heap.is_live(garbage));
//! ```

mod arena;
mod heap;
mod trace;

pub use arena::{AnyGc, Arena, Gc};
pub use heap::{Heap, Params, Phase};
pub use trace::{Holds, Liveness, Marker, Roots, Space, Trace, WeakMode};

/// Declare a [`Space`]: a struct with one [`Arena`] per object type.
///
/// ```
/// # use orbit_gc::{Marker, Trace, space};
/// # #[derive(Debug)] pub struct Str(Box<[u8]>);
/// # #[derive(Debug)] pub struct Table;
/// space! {
///     /// The Lua heap.
///     pub struct LuaSpace {
///         strings: Str,
///         tables: Table,
///     }
/// }
/// # impl Trace<LuaSpace> for Str { const LEAF: bool = true; fn trace(&self, _: &mut Marker<'_, LuaSpace>) {} }
/// # impl Trace<LuaSpace> for Table { fn trace(&self, _: &mut Marker<'_, LuaSpace>) {} }
/// ```
///
/// Every type must implement [`Trace`] for the space, and appear only once.
/// Arenas are numbered in declaration order (the [`AnyGc::kind`]), and swept
/// in that order. At most 256 types.
#[macro_export]
macro_rules! space {
    (
        $(#[$meta:meta])*
        $vis:vis struct $name:ident {
            $( $field:ident : $ty:ty ),+ $(,)?
        }
    ) => {
        $(#[$meta])*
        #[derive(Default)]
        $vis struct $name {
            $( $field: $crate::Arena<$ty>, )+
        }

        impl ::core::fmt::Debug for $name {
            fn fmt(&self, f: &mut ::core::fmt::Formatter<'_>) -> ::core::fmt::Result {
                f.debug_struct(::core::stringify!($name)).finish_non_exhaustive()
            }
        }

        const _: () = {
            #[allow(non_camel_case_types, dead_code)]
            #[repr(u8)]
            enum __Kind { $( $field, )+ }

            impl $crate::Space for $name {
                const KINDS: usize = 0 $( + { let _ = __Kind::$field; 1 } )+;

                fn trace_slot(
                    &self,
                    kind: u8,
                    index: u32,
                    m: &mut $crate::Marker<'_, Self>,
                ) -> usize {
                    $(
                        if kind == __Kind::$field as u8 {
                            return $crate::__private::trace::<Self, $ty>(&self.$field, index, m);
                        }
                    )+
                    ::core::unreachable!()
                }

                fn clear_slot(
                    &mut self,
                    kind: u8,
                    index: u32,
                    mode: $crate::WeakMode,
                    live: &$crate::Liveness<'_, Self>,
                ) {
                    $(
                        if kind == __Kind::$field as u8 {
                            return $crate::__private::clear::<Self, $ty>(
                                &mut self.$field, index, mode, live,
                            );
                        }
                    )+
                    ::core::unreachable!()
                }

                fn free_slot(&mut self, kind: u8, index: u32) -> usize {
                    $(
                        if kind == __Kind::$field as u8 {
                            return $crate::__private::free::<Self, $ty>(&mut self.$field, index);
                        }
                    )+
                    ::core::unreachable!()
                }

                fn kind_name(kind: u8) -> &'static str {
                    $(
                        if kind == __Kind::$field as u8 {
                            return ::core::stringify!($ty);
                        }
                    )+
                    "?"
                }
            }

            $(
                impl $crate::Holds<$ty> for $name {
                    const KIND: u8 = __Kind::$field as u8;
                    #[inline(always)]
                    fn arena(&self) -> &$crate::Arena<$ty> {
                        &self.$field
                    }
                    #[inline(always)]
                    fn arena_mut(&mut self) -> &mut $crate::Arena<$ty> {
                        &mut self.$field
                    }
                }
            )+
        };
    };
}

/// Support for [`space!`]; not a public API.
#[doc(hidden)]
pub mod __private {
    use crate::heap::slot_size;
    use crate::{Arena, Liveness, Marker, Space, Trace, WeakMode};

    #[inline]
    pub fn trace<S: Space, T: Trace<S>>(a: &Arena<T>, index: u32, m: &mut Marker<'_, S>) -> usize {
        let obj = a.get(index);
        obj.trace(m);
        slot_size::<T>() + obj.extra_bytes()
    }

    pub fn clear<S: Space, T: Trace<S>>(
        a: &mut Arena<T>,
        index: u32,
        mode: WeakMode,
        live: &Liveness<'_, S>,
    ) {
        a.get_mut(index).clear_weak(mode, live)
    }

    pub fn free<S: Space, T: Trace<S>>(a: &mut Arena<T>, index: u32) -> usize {
        let obj = a.take(index);
        slot_size::<T>() + obj.extra_bytes()
    }
}
