//! `OrbitString` and `OrbitStr`: the byte-string types embedders and native functions use
//! (ADR-5).
//!
//! Script strings are arbitrary bytes, so Rust's `String` and `&str` can't hold all of
//! them. These wrappers play the role `OsString` and `&OsStr` play for OS strings: they
//! hold any bytes, and convert to and from Rust strings, failing only when the bytes
//! aren't valid UTF-8.
//!
//! `OrbitStr<'a>` is a `Copy` struct around `&'a [u8]` rather than an unsized type used
//! as `&OrbitStr`, because turning a `&[u8]` into a reference to a custom unsized type
//! needs a pointer cast, which is `unsafe`.

use std::borrow::{Borrow, Cow};
use std::fmt;
use std::ops::Deref;
use std::str::Utf8Error;

/// A borrowed byte string, like `&OsStr`.
///
/// Native functions receive string arguments as `OrbitStr`, borrowed straight from the
/// GC heap, so reading an argument doesn't copy it.
#[derive(Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct OrbitStr<'a>(&'a [u8]);

/// An owned byte string, like `OsString`.
///
/// It lives outside the GC heap, so it can be stored, sent between threads and outlive
/// the VM state.
#[derive(Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct OrbitString(Vec<u8>);

impl<'a> OrbitStr<'a> {
    /// Wraps a byte slice.
    pub const fn new(bytes: &'a [u8]) -> Self {
        OrbitStr(bytes)
    }

    /// The bytes of the string.
    pub const fn as_bytes(self) -> &'a [u8] {
        self.0
    }

    /// Length in bytes.
    pub const fn len(self) -> usize {
        self.0.len()
    }

    /// Whether the string has no bytes.
    pub const fn is_empty(self) -> bool {
        self.0.is_empty()
    }

    /// The string as `&str`, if it is valid UTF-8.
    pub fn to_str(self) -> Result<&'a str, Utf8Error> {
        std::str::from_utf8(self.0)
    }

    /// The string as text, with invalid UTF-8 replaced by U+FFFD.
    pub fn to_string_lossy(self) -> Cow<'a, str> {
        String::from_utf8_lossy(self.0)
    }

    /// Copies the string into an owned `OrbitString`.
    pub fn to_owned_string(self) -> OrbitString {
        OrbitString(self.0.to_vec())
    }
}

impl OrbitString {
    /// An empty string.
    pub const fn new() -> Self {
        OrbitString(Vec::new())
    }

    /// An empty string with room for `capacity` bytes.
    pub fn with_capacity(capacity: usize) -> Self {
        OrbitString(Vec::with_capacity(capacity))
    }

    /// A borrowed view of the string.
    pub fn as_orbit_str(&self) -> OrbitStr<'_> {
        OrbitStr(&self.0)
    }

    /// The bytes of the string.
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    /// The underlying byte vector.
    pub fn into_bytes(self) -> Vec<u8> {
        self.0
    }

    /// Converts to a `String`, or gives the string back if it isn't valid UTF-8.
    pub fn into_string(self) -> Result<String, OrbitString> {
        String::from_utf8(self.0).map_err(|e| OrbitString(e.into_bytes()))
    }

    /// The string as `&str`, if it is valid UTF-8.
    pub fn to_str(&self) -> Result<&str, Utf8Error> {
        std::str::from_utf8(&self.0)
    }

    /// The string as text, with invalid UTF-8 replaced by U+FFFD.
    pub fn to_string_lossy(&self) -> Cow<'_, str> {
        String::from_utf8_lossy(&self.0)
    }

    /// Appends bytes or text.
    pub fn push(&mut self, s: impl AsRef<[u8]>) {
        self.0.extend_from_slice(s.as_ref());
    }

    /// Appends one byte.
    pub fn push_byte(&mut self, b: u8) {
        self.0.push(b);
    }
}

impl Deref for OrbitString {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        &self.0
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

impl Borrow<[u8]> for OrbitString {
    fn borrow(&self) -> &[u8] {
        &self.0
    }
}

// ---- conversions into OrbitString ----------------------------------------

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

impl From<Box<[u8]>> for OrbitString {
    fn from(b: Box<[u8]>) -> Self {
        OrbitString(b.into_vec())
    }
}

impl From<char> for OrbitString {
    fn from(c: char) -> Self {
        OrbitString::from(c.encode_utf8(&mut [0; 4]) as &str)
    }
}

impl From<OrbitStr<'_>> for OrbitString {
    fn from(s: OrbitStr<'_>) -> Self {
        s.to_owned_string()
    }
}

impl From<Cow<'_, str>> for OrbitString {
    fn from(s: Cow<'_, str>) -> Self {
        OrbitString::from(s.into_owned())
    }
}

impl From<OrbitString> for Vec<u8> {
    fn from(s: OrbitString) -> Self {
        s.0
    }
}

impl FromIterator<u8> for OrbitString {
    fn from_iter<I: IntoIterator<Item = u8>>(iter: I) -> Self {
        OrbitString(iter.into_iter().collect())
    }
}

impl Extend<u8> for OrbitString {
    fn extend<I: IntoIterator<Item = u8>>(&mut self, iter: I) {
        self.0.extend(iter);
    }
}

impl<'b> Extend<&'b u8> for OrbitString {
    fn extend<I: IntoIterator<Item = &'b u8>>(&mut self, iter: I) {
        self.0.extend(iter);
    }
}

impl std::str::FromStr for OrbitString {
    type Err = std::convert::Infallible;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(OrbitString::from(s))
    }
}

// ---- conversions into OrbitStr -------------------------------------------

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

impl<'a, const N: usize> From<&'a [u8; N]> for OrbitStr<'a> {
    fn from(b: &'a [u8; N]) -> Self {
        OrbitStr(b)
    }
}

impl<'a> From<&'a OrbitString> for OrbitStr<'a> {
    fn from(s: &'a OrbitString) -> Self {
        s.as_orbit_str()
    }
}

impl<'a> From<&'a String> for OrbitStr<'a> {
    fn from(s: &'a String) -> Self {
        OrbitStr(s.as_bytes())
    }
}

impl<'a> TryFrom<OrbitStr<'a>> for &'a str {
    type Error = Utf8Error;
    fn try_from(s: OrbitStr<'a>) -> Result<Self, Utf8Error> {
        s.to_str()
    }
}

impl TryFrom<OrbitString> for String {
    type Error = OrbitString;
    fn try_from(s: OrbitString) -> Result<Self, OrbitString> {
        s.into_string()
    }
}

// ---- comparisons ---------------------------------------------------------

macro_rules! impl_eq {
    ($lhs:ty, $rhs:ty, |$a:ident| $abytes:expr, |$b:ident| $bbytes:expr) => {
        impl PartialEq<$rhs> for $lhs {
            fn eq(&self, other: &$rhs) -> bool {
                let $a = self;
                let $b = other;
                $abytes == $bbytes
            }
        }
        impl PartialEq<$lhs> for $rhs {
            fn eq(&self, other: &$lhs) -> bool {
                other == self
            }
        }
    };
}

impl_eq!(OrbitStr<'_>, str, |a| a.0, |b| b.as_bytes());
impl_eq!(OrbitStr<'_>, &str, |a| a.0, |b| b.as_bytes());
impl_eq!(OrbitStr<'_>, String, |a| a.0, |b| b.as_bytes());
impl_eq!(OrbitStr<'_>, [u8], |a| a.0, |b| b);
impl_eq!(OrbitStr<'_>, &[u8], |a| a.0, |b| *b);
impl_eq!(OrbitStr<'_>, OrbitString, |a| a.0, |b| b.as_bytes());
impl_eq!(OrbitString, str, |a| a.as_bytes(), |b| b.as_bytes());
impl_eq!(OrbitString, &str, |a| a.as_bytes(), |b| b.as_bytes());
impl_eq!(OrbitString, String, |a| a.as_bytes(), |b| b.as_bytes());
impl_eq!(OrbitString, [u8], |a| a.as_bytes(), |b| b);
impl_eq!(OrbitString, &[u8], |a| a.as_bytes(), |b| *b);

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

impl<const N: usize> PartialEq<[u8; N]> for OrbitString {
    fn eq(&self, other: &[u8; N]) -> bool {
        self.0 == other
    }
}

impl<const N: usize> PartialEq<&[u8; N]> for OrbitString {
    fn eq(&self, other: &&[u8; N]) -> bool {
        self.0 == *other
    }
}

impl PartialOrd<OrbitString> for OrbitStr<'_> {
    fn partial_cmp(&self, other: &OrbitString) -> Option<std::cmp::Ordering> {
        Some(self.0.cmp(other.as_bytes()))
    }
}

impl PartialOrd<OrbitStr<'_>> for OrbitString {
    fn partial_cmp(&self, other: &OrbitStr<'_>) -> Option<std::cmp::Ordering> {
        Some(self.as_bytes().cmp(other.0))
    }
}

// ---- formatting ----------------------------------------------------------

/// Writes `bytes` as text, replacing invalid UTF-8 with U+FFFD, without allocating.
fn write_lossy(f: &mut fmt::Formatter<'_>, bytes: &[u8]) -> fmt::Result {
    for chunk in bytes.utf8_chunks() {
        f.write_str(chunk.valid())?;
        if !chunk.invalid().is_empty() {
            f.write_str("\u{FFFD}")?;
        }
    }
    Ok(())
}

/// Writes `bytes` as a quoted Lua string literal. Valid UTF-8 text other than control
/// characters prints as itself; everything else uses Lua escapes, so non-UTF-8 bytes
/// stay visible.
pub(crate) fn write_escaped(f: &mut impl fmt::Write, bytes: &[u8]) -> fmt::Result {
    f.write_char('"')?;
    for chunk in bytes.utf8_chunks() {
        for c in chunk.valid().chars() {
            match c {
                '"' => f.write_str("\\\"")?,
                '\\' => f.write_str("\\\\")?,
                '\n' => f.write_str("\\n")?,
                '\r' => f.write_str("\\r")?,
                '\t' => f.write_str("\\t")?,
                '\0' => f.write_str("\\0")?,
                c if c.is_control() => {
                    let mut buf = [0; 4];
                    for b in c.encode_utf8(&mut buf).bytes() {
                        write!(f, "\\x{b:02x}")?;
                    }
                }
                c => f.write_char(c)?,
            }
        }
        for b in chunk.invalid() {
            write!(f, "\\x{b:02x}")?;
        }
    }
    f.write_char('"')
}

impl fmt::Display for OrbitStr<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write_lossy(f, self.0)
    }
}

impl fmt::Display for OrbitString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write_lossy(f, &self.0)
    }
}

impl fmt::Debug for OrbitStr<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write_escaped(f, self.0)
    }
}

impl fmt::Debug for OrbitString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write_escaped(f, &self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn conversions_round_trip() {
        let s = OrbitString::from("print");
        assert_eq!(s, "print");
        assert_eq!(s.as_orbit_str(), "print");
        assert_eq!(s.as_orbit_str().to_str(), Ok("print"));
        assert_eq!(s.clone().into_string().as_deref(), Ok("print"));
        assert_eq!(OrbitString::from(String::from("x")), *"x");
        assert_eq!(OrbitString::from(&b"a\xffb"[..]).len(), 3);
        assert_eq!(OrbitString::from(vec![1u8, 2]).into_bytes(), vec![1, 2]);
        let borrowed: OrbitStr<'_> = OrbitStr::from("hi");
        assert_eq!(OrbitString::from(borrowed), "hi");
        let text: &str = borrowed.try_into().unwrap();
        assert_eq!(text, "hi");
    }

    #[test]
    fn invalid_utf8_gives_bytes_back() {
        let s = OrbitString::from(&b"a\xffb"[..]);
        assert!(s.to_str().is_err());
        assert!(s.as_orbit_str().to_str().is_err());
        let back = s.clone().into_string().unwrap_err();
        assert_eq!(back, s);
        assert_eq!(s.to_string_lossy(), "a\u{FFFD}b");
        assert_eq!(s.to_string(), "a\u{FFFD}b");
    }

    #[test]
    fn debug_uses_lua_escapes() {
        let s = OrbitString::from(&b"a\"\\\n\0\x01\xffz\xc3\xa9"[..]);
        assert_eq!(format!("{s:?}"), r#""a\"\\\n\0\x01\xffzé""#);
        assert_eq!(format!("{:?}", s.as_orbit_str()), format!("{s:?}"));
    }

    #[test]
    fn ordering_is_bytewise() {
        let a = OrbitString::from("a");
        let b = OrbitString::from(&b"\xff"[..]);
        let z = OrbitString::from("Z");
        assert!(z < a && a < b);
        assert!(z.as_orbit_str() < a.as_orbit_str());
        assert!(a.as_orbit_str() < b);
    }

    #[test]
    fn hash_matches_byte_slices() {
        let mut m: HashMap<OrbitString, i32> = HashMap::new();
        m.insert(OrbitString::from("key"), 1);
        assert_eq!(m.get(&b"key"[..]), Some(&1));
    }

    #[test]
    fn building() {
        let mut s = OrbitString::with_capacity(8);
        s.push("ab");
        s.push(b"\x00");
        s.push_byte(b'c');
        s.extend(b"de");
        assert_eq!(s, b"ab\0cde");
        let collected: OrbitString = b"xyz".iter().copied().collect();
        assert_eq!(collected, "xyz");
    }
}
