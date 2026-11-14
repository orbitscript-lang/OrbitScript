//! Text streams' line endings (Windows text mode).
//!
//! On Windows the C library writes a text stream's `\n` as `\r\n` and reads
//! `\r\n` back as `\n`, ending input at a Ctrl-Z; a stream opened with `b`
//! is binary. Elsewhere text and binary streams are the same. An isolate
//! chooses with [`LineEndings`]: the pure byte transforms are here, the
//! streams in `orbit-std`.

use std::io::{self, Write};

/// The byte that ends a text stream's input on Windows (Ctrl-Z).
pub const END_OF_TEXT: u8 = 0x1a;

/// How an isolate's text streams (standard streams, `io.open` without `b`,
/// pipes, script files) treat line endings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LineEndings {
    /// The platform's text mode, as the C library's: on Windows `\n` is
    /// written as `\r\n`, `\r\n` reads as `\n` and a Ctrl-Z ends input;
    /// elsewhere bytes pass unchanged.
    #[default]
    Native,
    /// Bytes pass unchanged on every platform: for a host whose standard
    /// streams carry a byte protocol.
    Binary,
}

impl LineEndings {
    /// Whether text streams translate on this platform.
    pub fn translates(self) -> bool {
        cfg!(windows) && self == LineEndings::Native
    }
}

/// Append `bytes` to `out` as a text stream writes them: each `\n` as
/// `\r\n`.
pub fn encode_into(bytes: &[u8], out: &mut Vec<u8>) {
    let mut rest = bytes;
    while let Some(i) = rest.iter().position(|&b| b == b'\n') {
        out.extend_from_slice(&rest[..i]);
        out.extend_from_slice(b"\r\n");
        rest = &rest[i + 1..];
    }
    out.extend_from_slice(rest);
}

/// A writer that writes each `\n` as `\r\n` to the one it wraps.
#[derive(Debug)]
pub struct TextWriter<W: Write> {
    inner: W,
}

impl<W: Write> TextWriter<W> {
    pub fn new(inner: W) -> TextWriter<W> {
        TextWriter { inner }
    }
}

impl<W: Write> Write for TextWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.write_all(buf)?;
        Ok(buf.len())
    }

    fn write_all(&mut self, buf: &[u8]) -> io::Result<()> {
        let mut rest = buf;
        while let Some(i) = rest.iter().position(|&b| b == b'\n') {
            self.inner.write_all(&rest[..i])?;
            self.inner.write_all(b"\r\n")?;
            rest = &rest[i + 1..];
        }
        self.inner.write_all(rest)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// Decodes a text stream's input: `\r\n` becomes `\n`, a lone `\r` stays,
/// and a Ctrl-Z ends the input. Decoding happens in place, one buffer at
/// a time; the decoder remembers how many raw bytes back the decoded ones
/// not yet consumed, so a reader can give them back exactly.
#[derive(Debug, Default, Clone)]
pub struct TextDecoder {
    /// Positions in the decoded buffer of the `\n`s a `\r\n` became.
    joined: Vec<usize>,
    /// Raw bytes of the buffer from the Ctrl-Z on, not decoded.
    after_end: usize,
    /// Whether a Ctrl-Z ended the input.
    ended: bool,
}

impl TextDecoder {
    /// Whether a Ctrl-Z has ended the input.
    pub fn ended(&self) -> bool {
        self.ended
    }

    /// Decode the raw bytes in `buf` in place, truncating it to the decoded
    /// bytes. `buf` must not end with a `\r` unless the input ends there:
    /// a reader that reads a buffer ending in `\r` reads on to see whether
    /// `\n` follows.
    pub fn decode(&mut self, buf: &mut Vec<u8>) {
        self.joined.clear();
        self.after_end = 0;
        let mut w = 0;
        let mut r = 0;
        while r < buf.len() {
            let b = buf[r];
            if b == END_OF_TEXT {
                self.ended = true;
                self.after_end = buf.len() - r;
                break;
            }
            if b == b'\r' && buf.get(r + 1) == Some(&b'\n') {
                self.joined.push(w);
                r += 1;
                continue;
            }
            buf[w] = b;
            w += 1;
            r += 1;
        }
        buf.truncate(w);
    }

    /// The raw bytes behind the decoded bytes from `pos` on in the last
    /// buffer, the Ctrl-Z and what follows it included.
    pub fn raw_len_from(&self, pos: usize, decoded_len: usize) -> usize {
        let joined = self.joined.iter().filter(|&&p| p >= pos).count();
        decoded_len.saturating_sub(pos) + joined + self.after_end
    }

    /// Forget the last buffer and the end: the reader moved (gave the
    /// unread bytes back or seeked).
    pub fn reset(&mut self) {
        self.joined.clear();
        self.after_end = 0;
        self.ended = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode(raw: &[u8]) -> (Vec<u8>, TextDecoder) {
        let mut d = TextDecoder::default();
        let mut b = raw.to_vec();
        d.decode(&mut b);
        (b, d)
    }

    #[test]
    fn encodes_newlines() {
        let mut out = Vec::new();
        encode_into(b"a\nb\r\nc\rd\x1ae\n", &mut out);
        assert_eq!(out, b"a\r\nb\r\r\nc\rd\x1ae\r\n");
        let mut w = TextWriter::new(Vec::new());
        w.write_all(b"\n\nx").unwrap();
        assert_eq!(w.inner, b"\r\n\r\nx");
    }

    #[test]
    fn decodes_crlf_keeps_lone_cr() {
        let (b, d) = decode(b"l1\r\nl2\n\r\nl3\rX");
        assert_eq!(b, b"l1\nl2\n\nl3\rX");
        assert!(!d.ended());
        // "a\r\r\nb\r" reads as "a\r\nb\r".
        assert_eq!(decode(b"a\r\r\nb\r").0, b"a\r\nb\r");
    }

    #[test]
    fn ctrl_z_ends_input() {
        let (b, d) = decode(b"ab\r\nc\x1aafter\r\n");
        assert_eq!(b, b"ab\nc");
        assert!(d.ended());
        // From "c": 1 decoded byte, then 8 raw from the Ctrl-Z on.
        assert_eq!(d.raw_len_from(3, b.len()), 9);
        // From the start: 4 decoded, 1 joined `\r`, 8 after.
        assert_eq!(d.raw_len_from(0, b.len()), 13);
    }

    #[test]
    fn raw_lengths_count_joined_crs() {
        let (b, d) = decode(b"a\r\nb\r\nc");
        assert_eq!(b, b"a\nb\nc");
        assert_eq!(d.raw_len_from(0, b.len()), 7);
        assert_eq!(d.raw_len_from(2, b.len()), 4);
        assert_eq!(d.raw_len_from(4, b.len()), 1);
        assert_eq!(d.raw_len_from(5, b.len()), 0);
    }

    #[test]
    fn only_native_translates_and_only_on_windows() {
        assert_eq!(LineEndings::Native.translates(), cfg!(windows));
        assert!(!LineEndings::Binary.translates());
    }
}
