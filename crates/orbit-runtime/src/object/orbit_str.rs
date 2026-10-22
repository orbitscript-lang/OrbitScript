//! The byte-string types natives and embedders use (ADR-5): [`OrbitString`]
//! (owned, like `OsString`) and [`OrbitStr`] (borrowed, like `&OsStr`). Lua
//! strings are bytes, not UTF-8, so these convert to and from Rust strings
//! and fail only when the bytes aren't UTF-8.

use std::borrow::Cow;
use std::fmt;
use std::ops::Deref;
use std::str::Utf8Error;

/// An owned byte string. It lives outside the GC heap, so it can be stored
/// and outlive the isolate.
#[derive(Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct OrbitString(Vec<u8>);

/// A borrowed byte string. A small `Copy` struct rather than an unsized type
/// behind a reference, which would take an `unsafe` cast (ADR-5).
#[derive(Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct OrbitStr<'a>(&'a [u8]);

impl OrbitString {
    pub fn new() -> Self {
        OrbitString(Vec::new())
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.0
    }

    pub fn as_orbit_str(&self) -> OrbitStr<'_> {
        OrbitStr(&self.0)
    }

    /// The text, if it is UTF-8.
    pub fn to_str(&self) -> Result<&str, Utf8Error> {
        std::str::from_utf8(&self.0)
    }

    /// The text, or the bytes back when they aren't UTF-8.
    pub fn into_string(self) -> Result<String, OrbitString> {
        String::from_utf8(self.0).map_err(|e| OrbitString(e.into_bytes()))
    }

    /// The text, with U+FFFD for bytes that aren't UTF-8.
    pub fn to_string_lossy(&self) -> Cow<'_, str> {
        String::from_utf8_lossy(&self.0)
    }

    pub fn push(&mut self, s: impl AsRef<[u8]>) {
        self.0.extend_from_slice(s.as_ref());
    }

    /// `parts` joined, as for a message quoting a script's bytes.
    pub fn concat(parts: &[&[u8]]) -> OrbitString {
        OrbitString(parts.concat())
    }
}

impl<'a> OrbitStr<'a> {
    pub fn as_bytes(self) -> &'a [u8] {
        self.0
    }

    /// The text, if it is UTF-8.
    pub fn to_str(self) -> Result<&'a str, Utf8Error> {
        std::str::from_utf8(self.0)
    }

    /// The text, with U+FFFD for bytes that aren't UTF-8.
    pub fn to_string_lossy(self) -> Cow<'a, str> {
        String::from_utf8_lossy(self.0)
    }

    pub fn to_owned_string(self) -> OrbitString {
        OrbitString(self.0.to_vec())
    }
}

impl Deref for OrbitString {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        &self.0
    }
}

impl Deref for OrbitStr<'_> {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        self.0
    }
}

impl AsRef<[u8]> for OrbitString {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

impl AsRef<[u8]> for OrbitStr<'_> {
    fn as_ref(&self) -> &[u8] {
        self.0
    }
}

// ---- conversions ----

impl From<&str> for OrbitString {
    fn from(s: &str) -> Self {
        OrbitString(s.as_bytes().to_vec())
    }
}

impl From<String> for OrbitString {
    fn from(s: String) -> Self {
        OrbitString(s.into_bytes())
    }
}

impl From<&String> for OrbitString {
    fn from(s: &String) -> Self {
        OrbitString(s.as_bytes().to_vec())
    }
}

impl From<&[u8]> for OrbitString {
    fn from(b: &[u8]) -> Self {
        OrbitString(b.to_vec())
    }
}

impl<const N: usize> From<&[u8; N]> for OrbitString {
    fn from(b: &[u8; N]) -> Self {
        OrbitString(b.to_vec())
    }
}

impl From<Vec<u8>> for OrbitString {
    fn from(b: Vec<u8>) -> Self {
        OrbitString(b)
    }
}

impl From<&Vec<u8>> for OrbitString {
    fn from(b: &Vec<u8>) -> Self {
        OrbitString(b.clone())
    }
}

impl From<Cow<'_, str>> for OrbitString {
    fn from(s: Cow<'_, str>) -> Self {
        OrbitString(s.into_owned().into_bytes())
    }
}

impl From<OrbitStr<'_>> for OrbitString {
    fn from(s: OrbitStr<'_>) -> Self {
        s.to_owned_string()
    }
}

impl From<&OrbitString> for OrbitString {
    fn from(s: &OrbitString) -> Self {
        s.clone()
    }
}

impl From<OrbitString> for Vec<u8> {
    fn from(s: OrbitString) -> Self {
        s.0
    }
}

impl<'a> From<&'a str> for OrbitStr<'a> {
    fn from(s: &'a str) -> Self {
        OrbitStr(s.as_bytes())
    }
}

impl<'a> From<&'a [u8]> for OrbitStr<'a> {
    fn from(b: &'a [u8]) -> Self {
        OrbitStr(b)
    }
}

impl<'a> From<&'a String> for OrbitStr<'a> {
    fn from(s: &'a String) -> Self {
        OrbitStr(s.as_bytes())
    }
}

impl<'a, const N: usize> From<&'a [u8; N]> for OrbitStr<'a> {
    fn from(b: &'a [u8; N]) -> Self {
        OrbitStr(b)
    }
}

impl<'a> From<&'a Vec<u8>> for OrbitStr<'a> {
    fn from(b: &'a Vec<u8>) -> Self {
        OrbitStr(b)
    }
}

impl<'a> From<&'a Cow<'_, str>> for OrbitStr<'a> {
    fn from(s: &'a Cow<'_, str>) -> Self {
        OrbitStr(s.as_bytes())
    }
}

impl<'a> From<&'a OrbitString> for OrbitStr<'a> {
    fn from(s: &'a OrbitString) -> Self {
        s.as_orbit_str()
    }
}

// ---- comparisons: byte-wise, against Rust strings too ----

macro_rules! eq_bytes {
    ($($a:ty, $b:ty;)*) => {$(
        impl PartialEq<$b> for $a {
            fn eq(&self, other: &$b) -> bool {
                self.as_bytes() == AsRef::<[u8]>::as_ref(other)
            }
        }
    )*};
}

eq_bytes! {
    OrbitString, str;
    OrbitString, &str;
    OrbitString, [u8];
    OrbitString, &[u8];
    OrbitString, OrbitStr<'_>;
    OrbitStr<'_>, str;
    OrbitStr<'_>, &str;
    OrbitStr<'_>, [u8];
    OrbitStr<'_>, &[u8];
    OrbitStr<'_>, OrbitString;
}

impl<const N: usize> PartialEq<[u8; N]> for OrbitString {
    fn eq(&self, other: &[u8; N]) -> bool {
        self.as_bytes() == other
    }
}

impl<const N: usize> PartialEq<&[u8; N]> for OrbitString {
    fn eq(&self, other: &&[u8; N]) -> bool {
        self.as_bytes() == *other
    }
}

impl<const N: usize> PartialEq<[u8; N]> for OrbitStr<'_> {
    fn eq(&self, other: &[u8; N]) -> bool {
        self.0 == other
    }
}

impl<const N: usize> PartialEq<&[u8; N]> for OrbitStr<'_> {
    fn eq(&self, other: &&[u8; N]) -> bool {
        self.0 == *other
    }
}

// ---- formatting ----

impl fmt::Display for OrbitString {
    /// Lossy for bytes that aren't UTF-8.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.as_orbit_str().fmt(f)
    }
}

impl fmt::Display for OrbitStr<'_> {
    /// Lossy for bytes that aren't UTF-8.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.to_string_lossy(), f)
    }
}

impl fmt::Debug for OrbitString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.as_orbit_str().fmt(f)
    }
}

impl fmt::Debug for OrbitStr<'_> {
    /// A quoted Lua literal: printable ASCII as is, the rest escaped, so bytes
    /// that aren't UTF-8 stay visible.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("\"")?;
        for &b in self.0 {
            match b {
                b'"' => f.write_str("\\\"")?,
                b'\\' => f.write_str("\\\\")?,
                b'\n' => f.write_str("\\n")?,
                b'\r' => f.write_str("\\r")?,
                b'\t' => f.write_str("\\t")?,
                0x20..=0x7e => write!(f, "{}", b as char)?,
                _ => write!(f, "\\x{b:02x}")?,
            }
        }
        f.write_str("\"")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conversions() {
        let s = OrbitString::from("héllo");
        assert_eq!(s.to_str(), Ok("héllo"));
        assert_eq!(s.clone().into_string().as_deref(), Ok("héllo"));
        let bad = OrbitString::from(&b"a\xff"[..]);
        assert!(bad.to_str().is_err());
        assert_eq!(bad.clone().into_string(), Err(bad.clone()));
        assert_eq!(bad.to_string_lossy(), "a\u{fffd}");
        assert_eq!(bad.as_orbit_str().to_owned_string(), bad);
        assert_eq!(&bad[..], b"a\xff");
    }

    #[test]
    fn comparisons() {
        let s = OrbitString::from("print");
        assert!(s == "print");
        assert!(s.as_orbit_str() == "print");
        assert!(s == b"print"[..]);
        assert!(s.as_orbit_str() == s);
        assert!(OrbitStr::from("a") < OrbitStr::from("b"));
    }

    #[test]
    fn formatting() {
        let s = OrbitString::from(&b"a\"\\\n\xff"[..]);
        assert_eq!(format!("{s:?}"), r#""a\"\\\n\xff""#);
        assert_eq!(s.to_string(), "a\"\\\n\u{fffd}");
    }
}
