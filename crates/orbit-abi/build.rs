//! The interface hash (ADR-10): stabby checks the boundary's types and its
//! traits' method names, not the methods' signatures, so a program and a
//! library built from different versions of this crate's source could
//! disagree unnoticed. Both sides compile in a hash of the source; the
//! library refuses a program whose hash differs.

use std::fmt::Write;

fn main() {
    println!("cargo:rerun-if-changed=src/lib.rs");
    let src = std::fs::read("src/lib.rs").expect("src/lib.rs");
    // FNV-1a, 64-bit: enough to tell versions apart, with no dependency.
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in &src {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    let mut out = String::new();
    let _ = writeln!(out, "/// A hash of the boundary's source.");
    let _ = writeln!(out, "pub const INTERFACE_HASH: u64 = {h:#018x};");
    let dir = std::env::var("OUT_DIR").expect("OUT_DIR");
    std::fs::write(format!("{dir}/hash.rs"), out).expect("write hash.rs");
}
