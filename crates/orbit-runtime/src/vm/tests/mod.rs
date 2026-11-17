//! Tests that drive the VM through raw natives, which only the crate can
//! write ([`NativeCtx`](super::NativeCtx) is crate-private).

mod debugger;
mod errors;
mod finalizers;
mod interp;
