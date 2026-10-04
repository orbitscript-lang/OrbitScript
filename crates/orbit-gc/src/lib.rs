//! A garbage collector in safe Rust: typed arenas, `u32` handles and an
//! incremental tri-color mark-sweep collector that provides Lua's weak-table
//! and finalizer semantics (ADR-2).
//!
//! The design is this crate's own. Lua fixes what a program can observe
//! (which objects survive, when weak entries go, the order finalizers run
//! in), and that is what the collector reproduces; how it gets there is not
//! modeled on any Lua implementation.
//!
//! `orbit-gc` knows nothing about Lua. The runtime declares its object types
//! in a [`space!`], implements [`Trace`] for each, and drives the collector
//! from its own safe points. What the crate provides:
//!
//! - **Arenas and handles.** One [`Arena`] per object type, a flat slot vector. A [`Gc<T>`]
//!   is a 4-byte slot index. Colors live in side byte vectors, so objects
//!   carry no header.
//! - **Incremental marking with barriers.** [`Heap::step`] processes heap
//!   bytes in proportion to bytes allocated, and a cycle starts when the heap
//!   has grown a set percentage past what the last one found reachable
//!   ([`Params`]). Stores into already-traced objects need
//!   [`Heap::barrier_back`] (or [`Heap::update`]) or [`Heap::barrier_forward`].
//!   Roots are rescanned when marking finishes, so they need no barrier.
//! - **Pages go back to the allocator.** Each arena is split into pages of
//!   a fixed number of slots ([`DEFAULT_PAGE_SLOTS`] unless the [`space!`]
//!   sets `PAGE_SLOTS`). New objects fill the lowest pages first, and once
//!   the sweep leaves the pages at the end of an arena empty, the arena is
//!   cut back, so memory taken during a spike is given back when the spike
//!   ends. A live object at the end of an arena keeps the empty slots below
//!   it until it dies, though the memory those objects owned is freed. glibc keeps freed memory in the
//!   process unless asked; the `malloc-trim` feature asks, once a cycle finds
//!   the reachable heap has shrunk well below its peak
//!   ([`Params::trim_percent`]). Support for other allocators and systems
//!   belongs in this crate too, not in its hosts.
//! - **Weak references and ephemerons.** An object declares its weak mode
//!   while being traced ([`Marker::weak`]) and drops dead entries in
//!   [`Trace::clear_weak`]. Ephemeron entries are reported with
//!   [`Marker::ephemeron`]: a value whose key is not marked yet waits for it,
//!   so marking never rescans ephemeron tables.
//! - **Finalization with resurrection.** [`Heap::register_finalizer`] and
//!   [`Heap::take_finalizable`] implement Lua's `__gc` semantics and order.
//! - **Stress mode.** [`Heap::set_stress`] collects at nearly every safe point.
//!
//! # Rooting
//!
//! Allocation never collects. Collection happens only inside [`Heap::step`]
//! and [`Heap::full_collect`], which take the [`Roots`]. Any handle the owner
//! holds across one of those calls must be reachable from those roots, or it
//! may be freed. Using a freed handle is never undefined behavior. In debug
//! builds, or with the `checked-handles` feature, every handle carries its
//! slot's generation and a stale one always panics ([`CHECKED_HANDLES`]).
//! Otherwise handles are 4 bytes and unchecked: a stale one panics until its
//! slot is reused, then reads the new object. A borrow-checked rooting layer
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
mod os;
mod trace;

pub use arena::{AnyGc, Arena, CHECKED_HANDLES, DEFAULT_PAGE_SLOTS, Gc, Store};
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
///
/// Every arena's pages hold [`DEFAULT_PAGE_SLOTS`] slots. A different count,
/// a power of two from 16 to 65536, goes after the struct:
///
/// ```
/// # use orbit_gc::{Marker, Trace, space};
/// # pub struct Node;
/// space! {
///     pub struct Small {
///         nodes: Node,
///     }
///     const PAGE_SLOTS: usize = 64;
/// }
/// # impl Trace<Small> for Node { const LEAF: bool = true; fn trace(&self, _: &mut Marker<'_, Small>) {} }
/// ```
///
/// Pages are the collector's unit of free lists and release; the slots of
/// an arena are one vector, so finding a handle's slot is one bounds check
/// (ADR-2).
#[macro_export]
macro_rules! space {
    (
        $(#[$meta:meta])*
        $vis:vis struct $name:ident {
            $( $field:ident : $ty:ty ),+ $(,)?
        }
    ) => {
        $crate::space! {
            @space ($crate::DEFAULT_PAGE_SLOTS)
            $(#[$meta])*
            $vis struct $name { $( $field: $ty, )+ }
        }
    };
    (
        $(#[$meta:meta])*
        $vis:vis struct $name:ident {
            $( $field:ident : $ty:ty ),+ $(,)?
        }
        const PAGE_SLOTS: usize = $slots:expr;
    ) => {
        $crate::space! {
            @space ($slots)
            $(#[$meta])*
            $vis struct $name { $( $field: $ty, )+ }
        }
    };
    (
        @space ($slots:expr)
        $(#[$meta:meta])*
        $vis:vis struct $name:ident {
            $( $field:ident : $ty:ty, )+
        }
    ) => {
        $(#[$meta])*
        #[derive(Default)]
        $vis struct $name {
            $( $field: $crate::Arena<$ty, { $slots }>, )+
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
                const PAGE_SLOTS: usize = $slots;

                fn trace_slot(
                    &self,
                    kind: u8,
                    index: u32,
                    m: &mut $crate::Marker<'_, Self>,
                ) -> usize {
                    $(
                        if kind == __Kind::$field as u8 {
                            return $crate::__private::trace::<Self, $ty, _>(&self.$field, index, m);
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
                            return $crate::__private::clear::<Self, $ty, _>(
                                &mut self.$field, index, mode, live,
                            );
                        }
                    )+
                    ::core::unreachable!()
                }

                fn free_slot(&mut self, kind: u8, index: u32) -> usize {
                    $(
                        if kind == __Kind::$field as u8 {
                            return $crate::__private::free::<Self, $ty, _>(&mut self.$field, index);
                        }
                    )+
                    ::core::unreachable!()
                }

                fn slot_bytes(kind: u8) -> usize {
                    $(
                        if kind == __Kind::$field as u8 {
                            return ::core::mem::size_of::<::core::option::Option<$ty>>();
                        }
                    )+
                    ::core::unreachable!()
                }

                fn release_page(&mut self, kind: u8, page: u32) {
                    $(
                        if kind == __Kind::$field as u8 {
                            return self.$field.release_page(page);
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
                    type Arena = $crate::Arena<$ty, { $slots }>;
                    #[inline(always)]
                    fn arena(&self) -> &Self::Arena {
                        &self.$field
                    }
                    #[inline(always)]
                    fn arena_mut(&mut self) -> &mut Self::Arena {
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

    /// Trace one object. Returns its marking work beyond the references
    /// it visits: one unit for the object plus one per 16 bytes it owns
    /// outside its slot, roughly Lua's one unit per slot traversed.
    #[inline]
    pub fn trace<S: Space, T: Trace<S>, const N: usize>(
        a: &Arena<T, N>,
        index: u32,
        m: &mut Marker<'_, S>,
    ) -> usize {
        let obj = a.get_slot(index);
        obj.trace(m);
        slot_size::<T>() + obj.extra_bytes()
    }

    pub fn clear<S: Space, T: Trace<S>, const N: usize>(
        a: &mut Arena<T, N>,
        index: u32,
        mode: WeakMode,
        live: &Liveness<'_, S>,
    ) {
        a.get_slot_mut(index).clear_weak(mode, live)
    }

    pub fn free<S: Space, T: Trace<S>, const N: usize>(a: &mut Arena<T, N>, index: u32) -> usize {
        let obj = a.take(index);
        slot_size::<T>() + obj.extra_bytes()
    }
}
