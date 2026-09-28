//! The Lua 5.5 standard library (stream D).
//!
//! This first part holds the pieces that need no VM: they are pure functions over byte
//! slices and plain values, ported from PUC Lua 5.5 so that results and error messages
//! match the reference interpreter. The VM wires them into library functions later,
//! supplying argument access through small traits ([`format::FormatArgs`],
//! [`pack::PackArgs`]) and turning [`LibError`]s into Lua errors.
//!
//! - [`pattern`] has Lua patterns (`string.find`, `match`, `gmatch`, `gsub`).
//! - [`format`] has `string.format`.
//! - [`pack`] has `string.pack`, `packsize` and `unpack`.
//! - [`float_to_str`] is Lua's float-to-string conversion (`tostring`, `..`).
//! - [`random`] has the `xoshiro256**` generator of `math.random`.

mod error;
pub mod format;
mod numfmt;
pub mod pack;
pub mod pattern;
pub mod random;

pub use error::LibError;
pub use numfmt::float_to_str;
