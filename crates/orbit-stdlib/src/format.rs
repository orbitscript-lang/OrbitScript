//! `string.format`, ported from `str_format` in `lstrlib.c`: the same accepted
//! specifications, the same order of argument checks and the same messages, with glibc's
//! output for each conversion (see [`crate::numfmt`]).

use std::borrow::Cow;

use crate::LibError;
use crate::numfmt::{
    Spec, fmt_bytes, fmt_float, fmt_hex_float, fmt_pointer, fmt_signed, fmt_unsigned,
};

/// Flags accepted by each group of conversions (`L_FMTFLAGS*`).
const FLAGS_F: &[u8] = b"-+#0 ";
const FLAGS_X: &[u8] = b"-#0";
const FLAGS_I: &[u8] = b"-+0 ";
const FLAGS_U: &[u8] = b"-0";
const FLAGS_C: &[u8] = b"-";

/// `MAX_FORMAT`: specifications of 22 or more bytes are rejected as too long.
const MAX_FORMAT: usize = 32;

/// What `%q` needs to know about an argument (`lua_type` plus its value).
#[derive(Debug, Clone, Copy)]
pub enum Literal<'a> {
    /// A string.
    Str(&'a [u8]),
    /// An integer.
    Int(i64),
    /// A float.
    Float(f64),
    /// `nil` or a boolean, written with [`FormatArgs::tostring`].
    NilOrBool,
    /// Anything else, which has no literal form.
    Other,
}

/// Access to the arguments of a `string.format` call. Argument numbers are Lua's: the
/// format string is argument 1.
pub trait FormatArgs {
    /// The VM's error type. Errors from the format string convert into it.
    type Error: From<LibError>;

    /// `lua_gettop`: the number of arguments, counting the format string.
    fn top(&self) -> usize;

    /// `luaL_checkinteger`.
    fn integer(&mut self, arg: usize) -> Result<i64, Self::Error>;

    /// `luaL_checknumber`.
    fn number(&mut self, arg: usize) -> Result<f64, Self::Error>;

    /// `luaL_tolstring`, which may call `__tostring` and `__name`.
    fn tostring(&mut self, arg: usize) -> Result<Cow<'_, [u8]>, Self::Error>;

    /// The argument's type and value for `%q`.
    fn literal(&mut self, arg: usize) -> Literal<'_>;

    /// `lua_topointer`: an identity for tables, functions, threads and userdata, printed
    /// by `%p`; `None` for other values, which print as `(null)`.
    fn pointer(&mut self, arg: usize) -> Option<usize>;
}

/// The byte at `i`, or `'\0'` past the end, as C reads a Lua string.
fn at(s: &[u8], i: usize) -> u8 {
    s.get(i).copied().unwrap_or(0)
}

/// A specification as C sees it: up to the first `'\0'`.
fn c_str(form: &[u8]) -> &[u8] {
    let end = form.iter().position(|&c| c == 0).unwrap_or(form.len());
    &form[..end]
}

fn quoted_msg(prefix: &str, form: &[u8], suffix: &str) -> LibError {
    let mut m = prefix.as_bytes().to_vec();
    m.extend_from_slice(c_str(form));
    m.extend_from_slice(suffix.as_bytes());
    LibError::Message(m)
}

/// `get2digits`
fn skip2digits(s: &[u8], mut i: usize) -> usize {
    if at(s, i).is_ascii_digit() {
        i += 1;
        if at(s, i).is_ascii_digit() {
            i += 1;
        }
    }
    i
}

/// `checkformat`: `form` starts with '%' and ends with the conversion character.
fn check_format(form: &[u8], flags: &[u8], precision: bool) -> Result<(), LibError> {
    let mut i = 1;
    while i < form.len() && flags.contains(&form[i]) {
        i += 1;
    }
    if at(form, i) != b'0' {
        i = skip2digits(form, i);
        if at(form, i) == b'.' && precision {
            i = skip2digits(form, i + 1);
        }
    }
    if !at(form, i).is_ascii_alphabetic() {
        return Err(quoted_msg("invalid conversion specification: '", form, "'"));
    }
    Ok(())
}

/// `addquoted`: a string as a Lua literal.
fn add_quoted(out: &mut Vec<u8>, s: &[u8]) {
    out.push(b'"');
    for (i, &c) in s.iter().enumerate() {
        if c == b'"' || c == b'\\' || c == b'\n' {
            out.push(b'\\');
            out.push(c);
        } else if c.is_ascii_control() {
            let text = if at(s, i + 1).is_ascii_digit() {
                format!("\\{c:03}")
            } else {
                format!("\\{c}")
            };
            out.extend_from_slice(text.as_bytes());
        } else {
            out.push(c);
        }
    }
    out.push(b'"');
}

/// `addliteral`: the `%q` item.
fn add_literal<A: FormatArgs>(out: &mut Vec<u8>, args: &mut A, arg: usize) -> Result<(), A::Error> {
    match args.literal(arg) {
        Literal::Str(s) => add_quoted(out, s),
        Literal::Int(n) => {
            let text = if n == i64::MIN {
                format!("0x{n:x}")
            } else {
                n.to_string()
            };
            out.extend_from_slice(text.as_bytes());
        }
        Literal::Float(n) => {
            if n == f64::INFINITY {
                out.extend_from_slice(b"1e9999");
            } else if n == f64::NEG_INFINITY {
                out.extend_from_slice(b"-1e9999");
            } else if n.is_nan() {
                out.extend_from_slice(b"(0/0)");
            } else {
                let spec = Spec {
                    conv: b'a',
                    ..Spec::default()
                };
                fmt_hex_float(out, &spec, n);
            }
        }
        Literal::NilOrBool => {
            let s = args.tostring(arg)?;
            out.extend_from_slice(&s);
        }
        Literal::Other => return Err(LibError::arg(arg, "value has no literal form").into()),
    }
    Ok(())
}

/// `string.format(fmt, ...)`.
pub fn format<A: FormatArgs>(fmt: &[u8], args: &mut A) -> Result<Vec<u8>, A::Error> {
    let mut out = Vec::with_capacity(fmt.len() + 16);
    let top = args.top();
    let mut arg = 1;
    let mut i = 0;
    while i < fmt.len() {
        let c = fmt[i];
        i += 1;
        if c != b'%' {
            out.push(c);
            continue;
        }
        if at(fmt, i) == b'%' {
            out.push(b'%');
            i += 1;
            continue;
        }
        arg += 1;
        if arg > top {
            return Err(LibError::arg(arg, "no value").into());
        }
        // getformat: flags, width and precision ('0' included as a flag), plus the
        // conversion character.
        let mut len = 0;
        while i + len < fmt.len() && b"-+#0 123456789.".contains(&fmt[i + len]) {
            len += 1;
        }
        len += 1;
        if len >= MAX_FORMAT - 10 {
            return Err(LibError::msg("invalid format (too long)").into());
        }
        let mut form = Vec::with_capacity(len + 1);
        form.push(b'%');
        form.extend((0..len).map(|k| at(fmt, i + k)));
        let conv = at(fmt, i + len - 1);
        i += len;
        let spec = || Spec::parse(&form[1..]);
        match conv {
            b'c' => {
                check_format(&form, FLAGS_C, false)?;
                let n = args.integer(arg)?;
                fmt_bytes(&mut out, &spec(), &[n as u8]);
            }
            b'd' | b'i' | b'u' | b'o' | b'x' | b'X' => {
                let n = args.integer(arg)?;
                let flags = match conv {
                    b'd' | b'i' => FLAGS_I,
                    b'u' => FLAGS_U,
                    _ => FLAGS_X,
                };
                check_format(&form, flags, true)?;
                if matches!(conv, b'd' | b'i') {
                    fmt_signed(&mut out, &spec(), n);
                } else {
                    fmt_unsigned(&mut out, &spec(), n as u64);
                }
            }
            b'a' | b'A' => {
                check_format(&form, FLAGS_F, true)?;
                let n = args.number(arg)?;
                fmt_hex_float(&mut out, &spec(), n);
            }
            b'f' | b'e' | b'E' | b'g' | b'G' => {
                let n = args.number(arg)?;
                check_format(&form, FLAGS_F, true)?;
                fmt_float(&mut out, &spec(), n);
            }
            b'p' => {
                let p = args.pointer(arg);
                check_format(&form, FLAGS_C, false)?;
                match p {
                    Some(p) => fmt_pointer(&mut out, &spec(), p),
                    None => fmt_bytes(&mut out, &spec(), b"(null)"),
                }
            }
            b'q' => {
                if form.len() > 2 {
                    return Err(LibError::msg("specifier '%q' cannot have modifiers").into());
                }
                add_literal(&mut out, args, arg)?;
            }
            b's' => {
                let s = args.tostring(arg)?;
                if form.len() == 2 {
                    out.extend_from_slice(&s);
                } else {
                    if s.contains(&0) {
                        return Err(LibError::arg(arg, "string contains zeros").into());
                    }
                    check_format(&form, FLAGS_C, true)?;
                    if !form.contains(&b'.') && s.len() >= 100 {
                        // no precision and too long to be formatted: keep it all
                        out.extend_from_slice(&s);
                    } else {
                        fmt_bytes(&mut out, &spec(), &s);
                    }
                }
            }
            _ => {
                return Err(quoted_msg("invalid conversion '", &form, "' to 'format'").into());
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Arguments given as Rust values, for tests.
    #[derive(Debug, Clone)]
    pub(crate) enum V {
        I(i64),
        F(f64),
        S(Vec<u8>),
        Nil,
        B(bool),
        T(usize),
    }

    pub(crate) struct Args(pub Vec<V>);

    impl FormatArgs for Args {
        type Error = LibError;
        fn top(&self) -> usize {
            self.0.len() + 1
        }
        fn integer(&mut self, arg: usize) -> Result<i64, LibError> {
            match &self.0[arg - 2] {
                V::I(i) => Ok(*i),
                V::F(f) if f.fract() == 0.0 && f.abs() < 9.2e18 => Ok(*f as i64),
                V::F(_) => Err(LibError::arg(arg, "number has no integer representation")),
                _ => Err(LibError::arg(arg, "number expected")),
            }
        }
        fn number(&mut self, arg: usize) -> Result<f64, LibError> {
            match &self.0[arg - 2] {
                V::I(i) => Ok(*i as f64),
                V::F(f) => Ok(*f),
                _ => Err(LibError::arg(arg, "number expected")),
            }
        }
        fn tostring(&mut self, arg: usize) -> Result<Cow<'_, [u8]>, LibError> {
            Ok(match &self.0[arg - 2] {
                V::I(i) => Cow::Owned(i.to_string().into_bytes()),
                V::F(f) => Cow::Owned(orbit_vm::number::fmt_float(*f).into_bytes()),
                V::S(s) => Cow::Borrowed(s),
                V::Nil => Cow::Borrowed(b"nil"),
                V::B(b) => Cow::Owned(b.to_string().into_bytes()),
                V::T(p) => Cow::Owned(format!("table: {p:#x}").into_bytes()),
            })
        }
        fn literal(&mut self, arg: usize) -> Literal<'_> {
            match &self.0[arg - 2] {
                V::I(i) => Literal::Int(*i),
                V::F(f) => Literal::Float(*f),
                V::S(s) => Literal::Str(s),
                V::Nil | V::B(_) => Literal::NilOrBool,
                V::T(_) => Literal::Other,
            }
        }
        fn pointer(&mut self, arg: usize) -> Option<usize> {
            match &self.0[arg - 2] {
                V::T(p) => Some(*p),
                _ => None,
            }
        }
    }

    fn fmt(f: &str, args: Vec<V>) -> Result<String, LibError> {
        format(f.as_bytes(), &mut Args(args)).map(|v| String::from_utf8(v).unwrap())
    }

    // Expected strings come from the reference lua5.5 on x86_64 Linux (glibc).
    #[test]
    fn items() {
        use V::*;
        assert_eq!(
            fmt(
                "%5.3s|%-5c|%5s|%.0s|%#o %#.0o %#x %#X %.0d %+.3d % d %05d %-05d %+05.2d %5.0x",
                vec![
                    S(b"abcdef".to_vec()),
                    I(65),
                    S(b"ab".to_vec()),
                    S(b"zz".to_vec()),
                    I(8),
                    I(0),
                    I(255),
                    I(0),
                    I(0),
                    I(7),
                    I(3),
                    I(-42),
                    I(42),
                    I(3),
                    I(0),
                ]
            )
            .unwrap(),
            "  abc|A    |   ab||010 0 0xff 0  +007  3 -0042 42      +03      "
        );
        assert_eq!(
            fmt("%q", vec![S(b"a\0b\n\"\\\x01\x012".to_vec())]).unwrap(),
            "\"a\\0b\\\n\\\"\\\\\\1\\0012\""
        );
        assert_eq!(fmt("%q", vec![I(i64::MIN)]).unwrap(), "0x8000000000000000");
        assert_eq!(
            fmt("%q %q %q", vec![F(1.0), Nil, B(true)]).unwrap(),
            "0x1p+0 nil true"
        );
        assert_eq!(
            fmt("%p %5p|%-8p|", vec![T(0x1234), I(1), Nil]).unwrap(),
            "0x1234 (null)|(null)  |"
        );
        assert_eq!(
            fmt("%x %d", vec![I(-1), F(3.0)]).unwrap(),
            "ffffffffffffffff 3"
        );
    }

    #[test]
    fn errors() {
        use V::*;
        let msg = |e: LibError| e.to_string();
        assert_eq!(
            msg(fmt("%10q", vec![S(vec![])]).unwrap_err()),
            "specifier '%q' cannot have modifiers"
        );
        assert_eq!(
            msg(fmt("%d %d", vec![I(1)]).unwrap_err()),
            "bad argument #3 (no value)"
        );
        assert_eq!(
            msg(fmt("%y", vec![I(1)]).unwrap_err()),
            "invalid conversion '%y' to 'format'"
        );
        assert_eq!(
            msg(fmt("%", vec![I(1)]).unwrap_err()),
            "invalid conversion '%' to 'format'"
        );
        assert_eq!(
            msg(fmt("%100d", vec![I(1)]).unwrap_err()),
            "invalid conversion specification: '%100d'"
        );
        assert_eq!(
            msg(fmt("%#d", vec![I(1)]).unwrap_err()),
            "invalid conversion specification: '%#d'"
        );
        assert_eq!(
            msg(fmt("%.3c", vec![I(1)]).unwrap_err()),
            "invalid conversion specification: '%.3c'"
        );
        assert_eq!(
            msg(fmt("%0000000000000000000000d", vec![I(1)]).unwrap_err()),
            "invalid format (too long)"
        );
        assert_eq!(
            msg(fmt("%10s", vec![S(b"a\0".to_vec())]).unwrap_err()),
            "bad argument #2 (string contains zeros)"
        );
        assert_eq!(
            msg(fmt("%q", vec![T(1)]).unwrap_err()),
            "bad argument #2 (value has no literal form)"
        );
    }
}
