//! `string.pack`, `string.unpack` and `string.packsize` (manual §6.4.2).
//!
//! Sizes and alignment are those of the reference interpreter on 64-bit
//! targets: `h`=2, `i`=4, `l`=`j`=`T`=8, `f`=4, `d`=`n`=8, native order
//! little-endian, and `!` alone means a maximum alignment of 16.

use crate::object::Value;
use crate::vm::{LuaError, NativeCtx, NativeReturn};

type R = Result<NativeReturn, LuaError>;

/// Largest integer size an `i`/`I`/`s`/`!` option may name.
const MAX_INT_SIZE: usize = 16;
/// Bytes in a Lua integer.
const INT_SIZE: usize = 8;
/// The alignment `!` sets when it has no number.
const NATIVE_ALIGN: usize = 16;
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

impl FormatError {
    fn raise(self, ctx: &mut NativeCtx<'_>) -> LuaError {
        match self {
            FormatError::SizeOutOfLimits(n) => {
                ctx.error(format!("integral size ({n}) out of limits [1,16]"))
            }
            FormatError::InvalidOption(c) => {
                ctx.error(format!("invalid format option '{}'", c as char))
            }
            FormatError::MissingSize => ctx.error("missing size for format option 'c'"),
            FormatError::NotPowerOfTwo => {
                ctx.arg_error(0, "format asks for alignment not power of 2")
            }
            FormatError::InvalidNextOption => {
                ctx.arg_error(0, "invalid next option for option 'X'")
            }
        }
    }
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
            if n > (MAX_SIZE - 9) / 10 {
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

/// `bad argument` for a missing or wrong pack value. The reference reports
/// a missing value as `nil` here.
fn value_error(ctx: &mut NativeCtx<'_>, arg: usize, expected: &str) -> LuaError {
    if arg >= ctx.nargs() {
        ctx.arg_error(arg, format!("{expected} expected, got nil"))
    } else {
        ctx.type_error(arg, expected)
    }
}

fn pack_int(ctx: &mut NativeCtx<'_>, arg: usize) -> Result<i64, LuaError> {
    if arg >= ctx.nargs() {
        return Err(value_error(ctx, arg, "number"));
    }
    ctx.check_int(arg)
}

fn pack_str(ctx: &mut NativeCtx<'_>, arg: usize) -> Result<Vec<u8>, LuaError> {
    if arg >= ctx.nargs() {
        return Err(value_error(ctx, arg, "string"));
    }
    let s = ctx.check_str(arg)?;
    Ok(ctx.bytes(s).to_vec())
}

pub(crate) fn pack(ctx: &mut NativeCtx<'_>) -> R {
    let fmt = ctx.check_str(0)?;
    let fmt = ctx.bytes(fmt).to_vec();
    let mut p = Parser::new(&fmt);
    let mut out = Vec::new();
    let mut arg = 0;
    while !p.done() {
        let item = p.next(out.len()).map_err(|e| e.raise(ctx))?;
        if item
            .size
            .checked_add(item.pad)
            .and_then(|n| n.checked_add(out.len()))
            .is_none_or(|n| n > MAX_SIZE)
        {
            return Err(ctx.arg_error(0, "result too long"));
        }
        out.resize(out.len() + item.pad, 0);
        let little = p.little;
        match item.kind {
            Kind::Int { signed } => {
                arg += 1;
                let n = pack_int(ctx, arg)?;
                if item.size < INT_SIZE {
                    let bits = 8 * item.size as u32;
                    if signed {
                        let lim = 1i64 << (bits - 1);
                        if !(-lim..lim).contains(&n) {
                            return Err(ctx.arg_error(arg, "integer overflow"));
                        }
                    } else if (n as u64) >> bits != 0 {
                        return Err(ctx.arg_error(arg, "unsigned overflow"));
                    }
                }
                put_int(&mut out, n, item.size, little, signed);
            }
            Kind::Float32 | Kind::Float64 => {
                arg += 1;
                if arg >= ctx.nargs() {
                    return Err(value_error(ctx, arg, "number"));
                }
                let x = ctx.check_f64(arg)?;
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
                let s = pack_str(ctx, arg)?;
                if s.len() > item.size {
                    return Err(ctx.arg_error(arg, "string longer than given size"));
                }
                out.extend_from_slice(&s);
                out.resize(out.len() + item.size - s.len(), 0);
            }
            Kind::Prefixed => {
                arg += 1;
                let s = pack_str(ctx, arg)?;
                if item.size < INT_SIZE && (s.len() as u64) >> (8 * item.size) != 0 {
                    return Err(ctx.arg_error(arg, "string length does not fit in given size"));
                }
                put_int(&mut out, s.len() as i64, item.size, little, false);
                out.extend_from_slice(&s);
            }
            Kind::Zero => {
                arg += 1;
                let s = pack_str(ctx, arg)?;
                if s.contains(&0) {
                    return Err(ctx.arg_error(arg, "string contains zeros"));
                }
                out.extend_from_slice(&s);
                out.push(0);
            }
            Kind::Pad => out.push(0),
            Kind::Align | Kind::Nop => {}
        }
    }
    let r = ctx.new_str_owned(out);
    ctx.ret([r])
}

pub(crate) fn packsize(ctx: &mut NativeCtx<'_>) -> R {
    let fmt = ctx.check_str(0)?;
    let fmt = ctx.bytes(fmt).to_vec();
    let mut p = Parser::new(&fmt);
    let mut total = 0usize;
    while !p.done() {
        let item = p.next(total).map_err(|e| e.raise(ctx))?;
        if matches!(item.kind, Kind::Prefixed | Kind::Zero) {
            return Err(ctx.arg_error(0, "variable-length format"));
        }
        match total
            .checked_add(item.pad)
            .and_then(|n| n.checked_add(item.size))
        {
            Some(n) if n <= MAX_SIZE => total = n,
            _ => return Err(ctx.arg_error(0, "format result too large")),
        }
    }
    ctx.ret([Value::Int(total as i64)])
}

pub(crate) fn unpack(ctx: &mut NativeCtx<'_>) -> R {
    let fmt = ctx.check_str(0)?;
    let data = ctx.check_str(1)?;
    let len = ctx.bytes(data).len();
    let init = ctx.opt_int(2, 1)?;
    let mut pos = super::start_pos(init, len) - 1;
    if pos > len {
        return Err(ctx.arg_error(2, "initial position out of string"));
    }
    let fmt = ctx.bytes(fmt).to_vec();
    let mut p = Parser::new(&fmt);
    let mut n = 0;
    while !p.done() {
        let item = p.next(pos).map_err(|e| e.raise(ctx))?;
        let short = item
            .pad
            .checked_add(item.size)
            .is_none_or(|need| need > len - pos);
        if short {
            return Err(ctx.arg_error(1, "data string too short"));
        }
        pos += item.pad;
        let little = p.little;
        let v = match item.kind {
            Kind::Int { signed } => {
                let r = get_int(&ctx.bytes(data)[pos..pos + item.size], little, signed);
                match r {
                    Ok(v) => Value::Int(v),
                    Err(size) => {
                        return Err(
                            ctx.error(format!("{size}-byte integer does not fit into Lua Integer"))
                        );
                    }
                }
            }
            Kind::Float32 => {
                let mut b = [0u8; 4];
                b.copy_from_slice(&ctx.bytes(data)[pos..pos + 4]);
                if !little {
                    b.reverse();
                }
                Value::Float(f32::from_le_bytes(b) as f64)
            }
            Kind::Float64 => {
                let mut b = [0u8; 8];
                b.copy_from_slice(&ctx.bytes(data)[pos..pos + 8]);
                if !little {
                    b.reverse();
                }
                Value::Float(f64::from_le_bytes(b))
            }
            Kind::Fixed => {
                let s = ctx.bytes(data)[pos..pos + item.size].to_vec();
                ctx.new_str_owned(s)
            }
            Kind::Prefixed => {
                let r = get_int(&ctx.bytes(data)[pos..pos + item.size], little, false);
                let strlen = match r {
                    Ok(v) => v as u64,
                    Err(size) => {
                        return Err(
                            ctx.error(format!("{size}-byte integer does not fit into Lua Integer"))
                        );
                    }
                };
                let start = pos + item.size;
                if strlen > (len - start) as u64 {
                    return Err(ctx.arg_error(1, "data string too short"));
                }
                let s = ctx.bytes(data)[start..start + strlen as usize].to_vec();
                pos = start;
                let v = ctx.new_str_owned(s);
                pos += strlen as usize;
                ctx.push(v);
                n += 1;
                continue;
            }
            Kind::Zero => {
                let rest = &ctx.bytes(data)[pos..];
                let Some(end) = rest.iter().position(|&b| b == 0) else {
                    return Err(ctx.arg_error(1, "unfinished string for format 'z'"));
                };
                let s = rest[..end].to_vec();
                pos += end + 1;
                let v = ctx.new_str_owned(s);
                ctx.push(v);
                n += 1;
                continue;
            }
            Kind::Pad | Kind::Align | Kind::Nop => {
                pos += item.size;
                continue;
            }
        };
        pos += item.size;
        ctx.push(v);
        n += 1;
    }
    ctx.push(Value::Int(pos as i64 + 1));
    Ok(NativeReturn::Return(n + 1))
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

    #[test]
    fn sizes_and_alignment() {
        assert_eq!(size("!xi16"), Ok(32));
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
            Err(FormatError::SizeOutOfLimits(-1486618625))
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
