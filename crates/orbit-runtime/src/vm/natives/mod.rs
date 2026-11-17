//! The runtime's raw natives: the library functions that work the VM's
//! control, switch coroutines or read its frames, which the stack
//! interface does not express. Every other native is `#[native]` on
//! [`Stack`](crate::stack::Stack); raw natives are written only here
//! ([`NativeCtx`](super::NativeCtx) is private to the crate).

pub(crate) mod base;
pub(crate) mod coroutine;
#[cfg(feature = "debug")]
pub(crate) mod debug;
pub(crate) mod exit;
pub(crate) mod load;
