//! `string.pack`, `string.packsize` and `string.unpack`, ported from `lstrlib.c` with
//! the sizes and alignment of the 64-bit targets OrbitScript supports (`int` 4 bytes,
//! `long` and `size_t` 8, maximum alignment 16) and native byte order.

use std::borrow::Cow;

use crate::LibError;

/// `MAXINTSIZE`: the largest size of a packed integer.
const MAX_INT_SIZE: usize = 16;

/// `MAX_SIZE`: the limit on string lengths (the largest `lua_Integer`).
const MAX_SIZE: usize = i64::MAX as usize;

/// `SZINT`: the size of a Lua integer.
const SZINT: usize = 8;

/// `offsetof(struct cD, u)`, the default for '!': the alignment of `LUAI_MAXALIGN`,
/// which includes `long double`. That is 16 on x86_64 and aarch64 Linux, the primary
/// targets (the reference build agrees); it is 8 on Windows and Apple aarch64.
const NATIVE_MAX_ALIGN: usize = 16;

const NATIVE_LITTLE: bool = cfg!(target_endian = "little");

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KOption {
    Int,
    Uint,
    Float,
    Number,
    Double,
    Char,
    String,
    Zstr,
    Padding,
    PaddAlign,
    Nop,
}

/// The format being read (`Header` plus the C format pointer).
struct Header<'f> {
    fmt: &'f [u8],
    pos: usize,
    little: bool,
    max_align: usize,
}

impl<'f> Header<'f> {
    fn new(fmt: &'f [u8]) -> Self {
        // luaL_checkstring: the format ends at its first '\0'.
        let end = fmt.iter().position(|&c| c == 0).unwrap_or(fmt.len());
        Header {
            fmt: &fmt[..end],
            pos: 0,
            little: NATIVE_LITTLE,
            max_align: 1,
        }
    }

    fn peek(&self) -> u8 {
        self.fmt.get(self.pos).copied().unwrap_or(0)
    }

    fn at_end(&self) -> bool {
        self.pos >= self.fmt.len()
    }

    /// `getnum`
    fn get_num(&mut self, default: usize) -> usize {
        if !self.peek().is_ascii_digit() {
            return default;
        }
        let mut a = 0usize;
        loop {
            a = a * 10 + usize::from(self.peek() - b'0');
            self.pos += 1;
            if !(self.peek().is_ascii_digit() && a <= (MAX_SIZE - 9) / 10) {
                return a;
            }
        }
    }

    /// `getnumlimit`
    fn get_num_limit(&mut self, default: usize) -> Result<usize, LibError> {
        let sz = self.get_num(default);
        if sz.wrapping_sub(1) >= MAX_INT_SIZE {
            // lua_pushfstring's %d reads an int
            return Err(LibError::msg(format!(
                "integral size ({}) out of limits [1,{MAX_INT_SIZE}]",
                sz as u32 as i32
            )));
        }
        Ok(sz)
    }

    /// `getoption`: the next option and its size.
    fn get_option(&mut self) -> Result<(KOption, usize), LibError> {
        let opt = self.peek();
        self.pos += 1;
        Ok(match opt {
            b'b' => (KOption::Int, 1),
            b'B' => (KOption::Uint, 1),
            b'h' => (KOption::Int, 2),
            b'H' => (KOption::Uint, 2),
            b'l' | b'j' => (KOption::Int, 8),
            b'L' | b'J' | b'T' => (KOption::Uint, 8),
            b'f' => (KOption::Float, 4),
            b'n' => (KOption::Number, 8),
            b'd' => (KOption::Double, 8),
            b'i' => (KOption::Int, self.get_num_limit(4)?),
            b'I' => (KOption::Uint, self.get_num_limit(4)?),
            b's' => (KOption::String, self.get_num_limit(8)?),
            b'c' => {
                let size = self.get_num(usize::MAX);
                if size == usize::MAX {
                    return Err(LibError::msg("missing size for format option 'c'"));
                }
                (KOption::Char, size)
            }
            b'z' => (KOption::Zstr, 0),
            b'x' => (KOption::Padding, 1),
            b'X' => (KOption::PaddAlign, 0),
            b' ' => (KOption::Nop, 0),
            b'<' => {
                self.little = true;
                (KOption::Nop, 0)
            }
            b'>' => {
                self.little = false;
                (KOption::Nop, 0)
            }
            b'=' => {
                self.little = NATIVE_LITTLE;
                (KOption::Nop, 0)
            }
            b'!' => {
                self.max_align = self.get_num_limit(NATIVE_MAX_ALIGN)?;
                (KOption::Nop, 0)
            }
            _ => {
                let mut m = b"invalid format option '".to_vec();
                m.push(opt);
                m.push(b'\'');
                return Err(LibError::Message(m));
            }
        })
    }

    /// `getdetails`: the next option, its size, and the padding that aligns it at
    /// `total`.
    fn get_details(&mut self, total: usize) -> Result<(KOption, usize, usize), LibError> {
        let (opt, size) = self.get_option()?;
        let mut align = size;
        if opt == KOption::PaddAlign {
            let bad = if self.at_end() {
                true
            } else {
                let (next, next_align) = self.get_option()?;
                align = next_align;
                next == KOption::Char || align == 0
            };
            if bad {
                return Err(LibError::arg(1, "invalid next option for option 'X'"));
            }
        }
        let ntoalign = if align <= 1 || opt == KOption::Char {
            0
        } else {
            if align > self.max_align {
                align = self.max_align;
            }
            if !align.is_power_of_two() {
                return Err(LibError::arg(1, "format asks for alignment not power of 2"));
            }
            (align - (total & (align - 1))) & (align - 1)
        };
        Ok((opt, size, ntoalign))
    }
}

/// Access to the arguments of a `string.pack` call (argument 1 is the format).
pub trait PackArgs {
    /// The VM's error type. Errors from the format convert into it.
    type Error: From<LibError>;

    /// `luaL_checkinteger`.
    fn integer(&mut self, arg: usize) -> Result<i64, Self::Error>;

    /// `luaL_checknumber`.
    fn number(&mut self, arg: usize) -> Result<f64, Self::Error>;

    /// `luaL_checklstring` (numbers are converted to strings).
    fn string(&mut self, arg: usize) -> Result<Cow<'_, [u8]>, Self::Error>;
}

fn reserve(out: &mut Vec<u8>, n: usize) -> Result<(), LibError> {
    out.try_reserve(n)
        .map_err(|_| LibError::msg("not enough memory"))
}

/// `packint`
fn pack_int(out: &mut Vec<u8>, n: u64, little: bool, size: usize, neg: bool) {
    let start = out.len();
    out.resize(start + size, 0);
    let buf = &mut out[start..];
    let mut n = n;
    for i in 0..size {
        let idx = if little { i } else { size - 1 - i };
        buf[idx] = if i >= SZINT {
            if neg { 0xff } else { 0 }
        } else {
            n as u8
        };
        n = n.checked_shr(8).unwrap_or(0);
    }
}

fn push_endian(out: &mut Vec<u8>, bytes: &[u8], little: bool) {
    if little {
        out.extend_from_slice(bytes);
    } else {
        out.extend(bytes.iter().rev());
    }
}

/// `string.pack(fmt, ...)`.
pub fn pack<A: PackArgs>(fmt: &[u8], args: &mut A) -> Result<Vec<u8>, A::Error> {
    let mut h = Header::new(fmt);
    let mut out = Vec::new();
    let mut arg = 1;
    let mut total = 0usize;
    while !h.at_end() {
        let (opt, size, ntoalign) = h.get_details(total)?;
        if size.saturating_add(ntoalign) > MAX_SIZE - total {
            return Err(LibError::arg(arg, "result too long").into());
        }
        total += ntoalign + size;
        reserve(&mut out, ntoalign)?;
        out.resize(out.len() + ntoalign, 0);
        arg += 1;
        match opt {
            KOption::Int => {
                let n = args.integer(arg)?;
                if size < SZINT {
                    let lim = 1i64 << (size * 8 - 1);
                    if !(-lim <= n && n < lim) {
                        return Err(LibError::arg(arg, "integer overflow").into());
                    }
                }
                pack_int(&mut out, n as u64, h.little, size, n < 0);
            }
            KOption::Uint => {
                let n = args.integer(arg)?;
                if size < SZINT && (n as u64) >= 1u64 << (size * 8) {
                    return Err(LibError::arg(arg, "unsigned overflow").into());
                }
                pack_int(&mut out, n as u64, h.little, size, false);
            }
            KOption::Float => {
                let f = args.number(arg)? as f32;
                push_endian(&mut out, &f.to_le_bytes(), h.little);
            }
            KOption::Number | KOption::Double => {
                let f = args.number(arg)?;
                push_endian(&mut out, &f.to_le_bytes(), h.little);
            }
            KOption::Char => {
                let s = args.string(arg)?;
                if s.len() > size {
                    return Err(LibError::arg(arg, "string longer than given size").into());
                }
                reserve(&mut out, size)?;
                out.extend_from_slice(&s);
                out.resize(out.len() + (size - s.len()), 0);
            }
            KOption::String => {
                let s = args.string(arg)?;
                if !(size >= 8 || (s.len() as u64) < 1u64 << (size * 8)) {
                    return Err(
                        LibError::arg(arg, "string length does not fit in given size").into(),
                    );
                }
                pack_int(&mut out, s.len() as u64, h.little, size, false);
                reserve(&mut out, s.len())?;
                out.extend_from_slice(&s);
                total += s.len();
            }
            KOption::Zstr => {
                let s = args.string(arg)?;
                if s.contains(&0) {
                    return Err(LibError::arg(arg, "string contains zeros").into());
                }
                reserve(&mut out, s.len() + 1)?;
                out.extend_from_slice(&s);
                out.push(0);
                total += s.len() + 1;
            }
            KOption::Padding => {
                out.push(0);
                arg -= 1;
            }
            KOption::PaddAlign | KOption::Nop => arg -= 1,
        }
    }
    Ok(out)
}

/// `string.packsize(fmt)`.
pub fn packsize(fmt: &[u8]) -> Result<i64, LibError> {
    let mut h = Header::new(fmt);
    let mut total = 0usize;
    while !h.at_end() {
        let (opt, size, ntoalign) = h.get_details(total)?;
        if matches!(opt, KOption::String | KOption::Zstr) {
            return Err(LibError::arg(1, "variable-length format"));
        }
        let size = size + ntoalign;
        if total > MAX_SIZE - size {
            return Err(LibError::arg(1, "format result too large"));
        }
        total += size;
    }
    Ok(total as i64)
}

/// A value produced by `string.unpack`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Unpacked {
    /// An integer.
    Int(i64),
    /// A float.
    Float(f64),
    /// A string: the bytes `start..end` of the data.
    Str {
        /// Byte offset of the first byte.
        start: usize,
        /// Byte offset one past the last byte.
        end: usize,
    },
}

/// `unpackint`
fn unpack_int(b: &[u8], little: bool, size: usize, signed: bool) -> Result<i64, LibError> {
    let limit = size.min(SZINT);
    let byte = |i: usize| b[if little { i } else { size - 1 - i }];
    let mut res = 0u64;
    for i in (0..limit).rev() {
        res = (res << 8) | u64::from(byte(i));
    }
    if size < SZINT {
        if signed {
            let mask = 1u64 << (size * 8 - 1);
            res = (res ^ mask).wrapping_sub(mask);
        }
    } else if size > SZINT {
        let mask = if !signed || (res as i64) >= 0 {
            0
        } else {
            0xff
        };
        if (limit..size).any(|i| byte(i) != mask) {
            return Err(LibError::msg(format!(
                "{size}-byte integer does not fit into Lua Integer"
            )));
        }
    }
    Ok(res as i64)
}

fn endian_bytes<const N: usize>(b: &[u8], little: bool) -> [u8; N] {
    let mut a = [0u8; N];
    a.copy_from_slice(&b[..N]);
    if !little {
        a.reverse();
    }
    a
}

/// `string.unpack(fmt, s, init)`: `init` is the Lua value (default 1). Returns the
/// values and the position after the last one read.
pub fn unpack(fmt: &[u8], data: &[u8], init: i64) -> Result<(Vec<Unpacked>, i64), LibError> {
    let mut h = Header::new(fmt);
    let ld = data.len();
    let mut pos = crate::pattern::posrelat_start(init, ld) - 1;
    if pos > ld {
        return Err(LibError::arg(3, "initial position out of string"));
    }
    let mut values = Vec::new();
    while !h.at_end() {
        let (opt, size, ntoalign) = h.get_details(pos)?;
        if ntoalign.saturating_add(size) > ld - pos {
            return Err(LibError::arg(2, "data string too short"));
        }
        pos += ntoalign;
        let b = &data[pos..];
        match opt {
            KOption::Int | KOption::Uint => {
                values.push(Unpacked::Int(unpack_int(
                    b,
                    h.little,
                    size,
                    opt == KOption::Int,
                )?));
            }
            KOption::Float => {
                let f = f32::from_le_bytes(endian_bytes(b, h.little));
                values.push(Unpacked::Float(f64::from(f)));
            }
            KOption::Number | KOption::Double => {
                values.push(Unpacked::Float(f64::from_le_bytes(endian_bytes(
                    b, h.little,
                ))));
            }
            KOption::Char => values.push(Unpacked::Str {
                start: pos,
                end: pos + size,
            }),
            KOption::String => {
                let len = unpack_int(b, h.little, size, false)? as u64;
                if len > (ld - pos - size) as u64 {
                    return Err(LibError::arg(2, "data string too short"));
                }
                let start = pos + size;
                values.push(Unpacked::Str {
                    start,
                    end: start + len as usize,
                });
                pos += len as usize;
            }
            KOption::Zstr => {
                let len = b.iter().position(|&c| c == 0).unwrap_or(b.len());
                if pos + len >= ld {
                    return Err(LibError::arg(2, "unfinished string for format 'z'"));
                }
                values.push(Unpacked::Str {
                    start: pos,
                    end: pos + len,
                });
                pos += len + 1;
            }
            KOption::PaddAlign | KOption::Padding | KOption::Nop => {}
        }
        pos += size;
    }
    Ok((values, pos as i64 + 1))
}
