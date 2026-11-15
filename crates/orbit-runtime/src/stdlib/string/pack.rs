//! `string.pack`, `string.unpack` and `string.packsize` (manual §6.4.2).
//!
//! Sizes and alignment are those of the reference interpreter on 64-bit
//! targets: `h`=2, `i`=4, `l`=`j`=`T`=8, `f`=4, `d`=`n`=8, native order
//! little-endian. `!` alone sets the maximum alignment to the reference
//! interpreter's on the same platform (see [`NATIVE_ALIGN`]).

use crate::stack::{Buffer, Fail, IntoRaised, Raised, Return, Stack, StackResult};
use orbit_lua_version::LUA;
use orbit_macros::native;

/// Largest integer size an `i`/`I`/`s`/`!` option may name.
const MAX_INT_SIZE: usize = 16;
/// Bytes in a Lua integer.
const INT_SIZE: usize = 8;
/// The alignment `!` sets when it has no number: the C compiler's largest
/// alignment for a basic type (`max_align_t`), which the reference
/// interpreter reports. It is 16 where `long double` is 16 bytes (x86_64,
/// aarch64 Linux and the BSDs) and 8 where `long double` is `double`
/// (arm64 macOS, MSVC Windows, arm64 Windows).
const NATIVE_ALIGN: usize = if cfg!(any(
    all(target_vendor = "apple", target_arch = "aarch64"),
    all(target_os = "windows", target_arch = "aarch64"),
    target_env = "msvc"
)) {
    8
} else {
    16
};
/// Largest total size of a packed string.
const MAX_SIZE: usize = i64::MAX as usize;

/// What one format option does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Int {
        signed: bool,
    },
    Float32,
    Float64,
    /// `cN`: fixed-size string.
    Fixed,
    /// `sN`: string with an N-byte length prefix.
    Prefixed,
    /// `z`: zero-terminated string.
    Zero,
    /// `x`: one padding byte.
    Pad,
    /// `X`: alignment only (to the next option's size).
    Align,
    /// Spaces and the endianness/alignment settings.
    Nop,
}

/// Format-string errors. Some are positioned at argument 1, the rest are
/// plain errors, as in the reference.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FormatError {
    SizeOutOfLimits(i32),
    InvalidOption(u8),
    MissingSize,
    NotPowerOfTwo,
    InvalidNextOption,
}

impl From<FormatError> for Fail {
    #[cold]
    fn from(e: FormatError) -> Fail {
        match e {
            FormatError::SizeOutOfLimits(n) => {
                format!("integral size ({n}) out of limits [1,16]").into()
            }
            FormatError::InvalidOption(c) => {
                Fail::Message([b"invalid format option '", &[c][..], b"'"].concat().into())
            }
            FormatError::MissingSize => "missing size for format option 'c'".into(),
            FormatError::NotPowerOfTwo => Fail::arg(1, "format asks for alignment not power of 2"),
            FormatError::InvalidNextOption => Fail::arg(1, "invalid next option for option 'X'"),
        }
    }
}

#[cold]
#[inline(never)]
fn raise(s: &mut Stack<'_>, e: FormatError) -> Raised {
    Fail::from(e).into_raised(s)
}

/// One parsed option: what it does, its size, and the padding before it.
#[derive(Debug, Clone, Copy)]
struct Item {
    kind: Kind,
    size: usize,
    pad: usize,
}

#[derive(Debug)]
struct Parser<'a> {
    fmt: &'a [u8],
    pos: usize,
    little: bool,
    max_align: usize,
}

impl<'a> Parser<'a> {
    fn new(fmt: &'a [u8]) -> Self {
        Parser {
            fmt,
            pos: 0,
            little: true,
            max_align: 1,
        }
    }

    fn done(&self) -> bool {
        self.pos >= self.fmt.len()
    }

    /// Digits at the current position, if any. Reading stops before the
    /// number could exceed the size limit, as the reference does.
    fn number(&mut self) -> Option<usize> {
        let start = self.pos;
        let mut n = 0usize;
        while let Some(d) = self.fmt.get(self.pos).filter(|d| d.is_ascii_digit()) {
            if n > (PACKSIZE_MAX - 9) / 10 {
                break;
            }
            n = n * 10 + (d - b'0') as usize;
            self.pos += 1;
        }
        (self.pos > start).then_some(n)
    }

    /// An integer size: digits or `default`, within 1..=16.
    fn int_size(&mut self, default: usize) -> Result<usize, FormatError> {
        match self.number() {
            None => Ok(default),
            Some(n) if (1..=MAX_INT_SIZE).contains(&n) => Ok(n),
            // The reference reports the size as a C `int`.
            Some(n) => Err(FormatError::SizeOutOfLimits(n as i32)),
        }
    }

    /// The next option's kind and size, without alignment.
    fn option(&mut self) -> Result<(Kind, usize), FormatError> {
        let c = self.fmt[self.pos];
        self.pos += 1;
        Ok(match c {
            b'b' => (Kind::Int { signed: true }, 1),
            b'B' => (Kind::Int { signed: false }, 1),
            b'h' => (Kind::Int { signed: true }, 2),
            b'H' => (Kind::Int { signed: false }, 2),
            b'l' | b'j' => (Kind::Int { signed: true }, 8),
            b'L' | b'J' | b'T' => (Kind::Int { signed: false }, 8),
            b'f' => (Kind::Float32, 4),
            b'n' | b'd' => (Kind::Float64, 8),
            b'i' => (Kind::Int { signed: true }, self.int_size(4)?),
            b'I' => (Kind::Int { signed: false }, self.int_size(4)?),
            b's' => (Kind::Prefixed, self.int_size(8)?),
            b'c' => match self.number() {
                Some(n) => (Kind::Fixed, n),
                None => return Err(FormatError::MissingSize),
            },
            b'z' => (Kind::Zero, 0),
            b'x' => (Kind::Pad, 1),
            b'X' => (Kind::Align, 0),
            b' ' => (Kind::Nop, 0),
            b'<' => {
                self.little = true;
                (Kind::Nop, 0)
            }
            b'>' => {
                self.little = false;
                (Kind::Nop, 0)
            }
            b'=' => {
                self.little = true;
                (Kind::Nop, 0)
            }
            b'!' => {
                self.max_align = self.int_size(NATIVE_ALIGN)?;
                (Kind::Nop, 0)
            }
            _ => return Err(FormatError::InvalidOption(c)),
        })
    }

    /// The next option, with the padding needed before it at `total`.
    fn next(&mut self, total: usize) -> Result<Item, FormatError> {
        let (kind, size) = self.option()?;
        let align = match kind {
            Kind::Int { .. } | Kind::Float32 | Kind::Float64 | Kind::Prefixed => size,
            Kind::Align => {
                if self.done() {
                    return Err(FormatError::InvalidNextOption);
                }
                match self.option()? {
                    (Kind::Fixed | Kind::Zero | Kind::Pad | Kind::Align | Kind::Nop, _) => {
                        return Err(FormatError::InvalidNextOption);
                    }
                    (_, 0) => return Err(FormatError::InvalidNextOption),
                    (_, n) => n,
                }
            }
            _ => 1,
        };
        let align = align.min(self.max_align);
        let mut pad = 0;
        if align > 1 {
            if !align.is_power_of_two() {
                return Err(FormatError::NotPowerOfTwo);
            }
            pad = (align - (total & (align - 1))) & (align - 1);
        }
        Ok(Item { kind, size, pad })
    }
}

/// Write `n` as `size` bytes, extending past 8 bytes with the sign (or
/// zeros for unsigned).
fn put_int(out: &mut Vec<u8>, n: i64, size: usize, little: bool, signed: bool) {
    let fill = if signed && n < 0 { 0xff } else { 0 };
    let bytes = n.to_le_bytes();
    let start = out.len();
    for i in 0..size {
        out.push(bytes.get(i).copied().unwrap_or(fill));
    }
    if !little {
        out[start..].reverse();
    }
}

fn get_int(data: &[u8], little: bool, signed: bool) -> Result<i64, usize> {
    let size = data.len();
    let byte = |i: usize| if little { data[i] } else { data[size - 1 - i] };
    let mut n: u64 = 0;
    for i in (0..size.min(INT_SIZE)).rev() {
        n = n << 8 | byte(i) as u64;
    }
    if size < INT_SIZE {
        if signed {
            let shift = 64 - 8 * size as u32;
            n = (((n << shift) as i64) >> shift) as u64;
        }
    } else if size > INT_SIZE {
        // Extra bytes must only extend the value.
        let fill = if signed && (n as i64) < 0 { 0xff } else { 0 };
        if (INT_SIZE..size).any(|i| byte(i) != fill) {
            return Err(size);
        }
    }
    Ok(n as i64)
}

/// `bad argument` for a missing or wrong pack value (`arg` is 1-based).
/// The reference reports a missing value as `nil` here.
#[cold]
#[inline(never)]
fn value_error(s: &mut Stack<'_>, arg: i32, expected: &str) -> Raised {
    if arg > s.top() {
        s.arg_error(arg, &crate::arg_errors::type_mismatch(expected, "nil"))
    } else {
        s.type_error(arg, expected)
    }
}

fn pack_int(s: &mut Stack<'_>, arg: i32) -> StackResult<i64> {
    if arg > s.top() {
        return Err(value_error(s, arg, "number"));
    }
    s.check_integer(arg)
}

/// The string value `arg`, which stays in the window: read its bytes
/// through the handle.
fn pack_str(s: &mut Stack<'_>, arg: i32) -> StackResult<crate::object::Gc<crate::object::LuaStr>> {
    if arg > s.top() {
        return Err(value_error(s, arg, "string"));
    }
    s.check_string_handle(arg)
}

#[native(crate = crate, name = "string.pack")]
pub(super) fn pack(s: &mut Stack<'_>) -> StackResult<Buffer> {
    let fmt = s.check_string_handle(1)?;
    let fmt = s.handle_bytes(fmt).to_vec();
    let mut p = Parser::new(&fmt);
    let mut out = s.take_buffer();
    // Argument 1 is the format; the values follow it.
    let mut arg = 1;
    while !p.done() {
        let item = p.next(out.len()).map_err(|e| raise(s, e))?;
        if item
            .size
            .checked_add(item.pad)
            .and_then(|n| n.checked_add(out.len()))
            .map_or(true, |n| n > MAX_SIZE)
        {
            return Err(s.arg_error(1, "result too long"));
        }
        out.resize(out.len() + item.pad, 0);
        let little = p.little;
        match item.kind {
            Kind::Int { signed } => {
                arg += 1;
                let n = pack_int(s, arg)?;
                if item.size < INT_SIZE {
                    let bits = 8 * item.size as u32;
                    if signed {
                        let lim = 1i64 << (bits - 1);
                        if !(-lim..lim).contains(&n) {
                            return Err(s.arg_error(arg, "integer overflow"));
                        }
                    } else if (n as u64) >> bits != 0 {
                        return Err(s.arg_error(arg, "unsigned overflow"));
                    }
                }
                put_int(&mut out, n, item.size, little, signed);
            }
            Kind::Float32 | Kind::Float64 => {
                arg += 1;
                if arg > s.top() {
                    return Err(value_error(s, arg, "number"));
                }
                let x = s.check_number(arg)?;
                let start = out.len();
                if item.kind == Kind::Float32 {
                    out.extend_from_slice(&(x as f32).to_le_bytes());
                } else {
                    out.extend_from_slice(&x.to_le_bytes());
                }
                if !little {
                    out[start..].reverse();
                }
            }
            Kind::Fixed => {
                arg += 1;
                let h = pack_str(s, arg)?;
                let v = s.handle_bytes(h);
                if v.len() > item.size {
                    return Err(s.arg_error(arg, "string longer than given size"));
                }
                let pad = item.size - v.len();
                out.extend_from_slice(v);
                out.resize(out.len() + pad, 0);
            }
            Kind::Prefixed => {
                arg += 1;
                let h = pack_str(s, arg)?;
                let n = s.handle_bytes(h).len();
                if item.size < INT_SIZE && (n as u64) >> (8 * item.size) != 0 {
                    return Err(s.arg_error(arg, "string length does not fit in given size"));
                }
                put_int(&mut out, n as i64, item.size, little, false);
                out.extend_from_slice(s.handle_bytes(h));
            }
            Kind::Zero => {
                arg += 1;
                let h = pack_str(s, arg)?;
                if s.handle_bytes(h).contains(&0) {
                    return Err(s.arg_error(arg, "string contains zeros"));
                }
                out.extend_from_slice(s.handle_bytes(h));
                out.push(0);
            }
            Kind::Pad => out.push(0),
            Kind::Align | Kind::Nop => {}
        }
    }
    Ok(Buffer(out))
}

/// The largest size `packsize` reports, and the most a size in a format
/// may read: Lua 5.4 caps them at a C `int` (ADR-14).
const PACKSIZE_MAX: usize = if LUA.is_5_4() {
    i32::MAX as usize
} else {
    MAX_SIZE
};

#[native(crate = crate, name = "string.packsize")]
pub(super) fn packsize(fmt: &[u8]) -> Result<i64, Fail> {
    let mut p = Parser::new(fmt);
    let mut total = 0usize;
    while !p.done() {
        let item = p.next(total)?;
        if matches!(item.kind, Kind::Prefixed | Kind::Zero) {
            return Err(Fail::arg(1, "variable-length format"));
        }
        match total
            .checked_add(item.pad)
            .and_then(|n| n.checked_add(item.size))
        {
            Some(n) if n <= PACKSIZE_MAX => total = n,
            _ => return Err(Fail::arg(1, "format result too large")),
        }
    }
    Ok(total as i64)
}

#[cold]
#[inline(never)]
fn too_big(s: &mut Stack<'_>, size: usize) -> Raised {
    s.error(format!("{size}-byte integer does not fit into Lua Integer").as_str())
}

#[native(crate = crate, name = "string.unpack")]
pub(super) fn unpack(s: &mut Stack<'_>) -> StackResult<Return> {
    let fmt = s.check_string_handle(1)?;
    let data = s.check_string_handle(2)?;
    let len = s.handle_bytes(data).len();
    let init = s.opt_integer(3, 1)?;
    let mut pos = super::start_pos(init, len) - 1;
    if pos > len {
        return Err(s.arg_error(3, "initial position out of string"));
    }
    let fmt = s.handle_bytes(fmt).to_vec();
    let mut p = Parser::new(&fmt);
    let mut n = 0;
    while !p.done() {
        let item = p.next(pos).map_err(|e| raise(s, e))?;
        // One value per item, and the next position after them all.
        s.need_room(2, "too many results")?;
        let short = item
            .pad
            .checked_add(item.size)
            .map_or(true, |need| need > len - pos);
        if short {
            return Err(s.arg_error(2, "data string too short"));
        }
        pos += item.pad;
        let little = p.little;
        let field = &s.handle_bytes(data)[pos..pos + item.size];
        match item.kind {
            Kind::Int { signed } => match get_int(field, little, signed) {
                Ok(v) => s.push_integer(v),
                Err(size) => return Err(too_big(s, size)),
            },
            Kind::Float32 => {
                let mut b = [0u8; 4];
                b.copy_from_slice(field);
                if !little {
                    b.reverse();
                }
                s.push_number(f64::from(f32::from_le_bytes(b)));
            }
            Kind::Float64 => {
                let mut b = [0u8; 8];
                b.copy_from_slice(field);
                if !little {
                    b.reverse();
                }
                s.push_number(f64::from_le_bytes(b));
            }
            Kind::Fixed => s.push_substring(data, pos, pos + item.size),
            Kind::Prefixed => {
                let strlen = match get_int(field, little, false) {
                    Ok(v) => v as u64,
                    Err(size) => return Err(too_big(s, size)),
                };
                let start = pos + item.size;
                if strlen > (len - start) as u64 {
                    return Err(s.arg_error(2, "data string too short"));
                }
                s.push_substring(data, start, start + strlen as usize);
                pos = start + strlen as usize;
                n += 1;
                continue;
            }
            Kind::Zero => {
                let rest = &s.handle_bytes(data)[pos..];
                let Some(end) = rest.iter().position(|&b| b == 0) else {
                    return Err(s.arg_error(2, "unfinished string for format 'z'"));
                };
                s.push_substring(data, pos, pos + end);
                pos += end + 1;
                n += 1;
                continue;
            }
            Kind::Pad | Kind::Align | Kind::Nop => {
                pos += item.size;
                continue;
            }
        }
        pos += item.size;
        n += 1;
    }
    s.push_integer(pos as i64 + 1);
    Ok(Return::values(n + 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn size(fmt: &str) -> Result<usize, FormatError> {
        let mut p = Parser::new(fmt.as_bytes());
        let mut total = 0;
        while !p.done() {
            let it = p.next(total)?;
            total += it.pad + it.size;
        }
        Ok(total)
    }

    /// `NATIVE_ALIGN` is C's `max_align_t` alignment. Rust can't name that
    /// type, so the constant is a table by target; this checks it against
    /// the `libc` crate's definition on the targets that have one (not
    /// MSVC Windows or arm64 Windows).
    #[cfg(any(
        target_os = "linux",
        target_os = "android",
        target_vendor = "apple",
        all(windows, target_env = "gnu", target_arch = "x86_64")
    ))]
    #[test]
    fn native_alignment_is_max_align_t() {
        assert_eq!(NATIVE_ALIGN, std::mem::align_of::<libc::max_align_t>());
    }

    #[test]
    fn sizes_and_alignment() {
        assert_eq!(size("!xi16"), Ok(16 + NATIVE_ALIGN));
        assert_eq!(size("!xd"), Ok(16));
        assert_eq!(size("xXd"), Ok(1));
        assert_eq!(size("!4xi8"), Ok(12));
        assert_eq!(size("!2i3"), Ok(3));
        assert_eq!(size("!i3"), Err(FormatError::NotPowerOfTwo));
        assert_eq!(size(" b b Xd b Xb x"), Ok(4));
        assert_eq!(size("!8 xXi2"), Ok(2));
        assert_eq!(size("Xz"), Err(FormatError::InvalidNextOption));
        assert_eq!(size("i0"), Err(FormatError::SizeOutOfLimits(0)));
        assert_eq!(
            size("i9999999999999999999"),
            // lua5.4 stops reading digits before the size overflows an `int`.
            Err(FormatError::SizeOutOfLimits(if LUA.is_5_4() {
                999999999
            } else {
                -1486618625
            }))
        );
        assert_eq!(size("c1".to_owned().as_str()), Ok(1));
        assert_eq!(size("i3r"), Err(FormatError::InvalidOption(b'r')));
    }

    #[test]
    fn ints() {
        let mut out = Vec::new();
        put_int(&mut out, -1, 10, true, false);
        assert_eq!(out, [255, 255, 255, 255, 255, 255, 255, 255, 0, 0]);
        assert_eq!(get_int(&[0xf0, 0xff], true, true), Ok(-16));
        assert_eq!(get_int(&[0xff; 9], true, false), Err(9));
        assert_eq!(get_int(&[0xff; 16], true, true), Ok(-1));
    }
}
