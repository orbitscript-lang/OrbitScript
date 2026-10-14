//! Template strings: `f"..."` becomes a `string.format` call.

use orbit_hir as hir;
use orbit_hir::{ExprId, Span, StmtId};

use super::expr::Part;
use super::{LResult, Lowerer};
use crate::ast;

impl Lowerer {
    pub(super) fn template(
        &mut self,
        pieces: &[ast::TemplatePiece],
        span: Span,
        out: &mut Vec<StmtId>,
    ) -> LResult<ExprId> {
        let mut fmt = Vec::new();
        let mut holes = Vec::new();
        for piece in pieces {
            match piece {
                ast::TemplatePiece::Text(text) => {
                    for &b in text {
                        if b == b'%' {
                            fmt.push(b'%');
                        }
                        fmt.push(b);
                    }
                }
                ast::TemplatePiece::Hole { expr, spec } => {
                    let conv = match spec {
                        Some(spec) => self.format_spec(spec)?,
                        None => b"%s".to_vec(),
                    };
                    fmt.extend_from_slice(&conv);
                    holes.push(Part::Item(expr, false));
                }
            }
        }
        let fmt = self.intern(&fmt);
        let fmt = self.str_expr(fmt, span);
        if holes.is_empty() {
            return Ok(fmt);
        }
        let (values, _) = self.parts(&holes, out)?;
        let string = self.global_expr(self.syms.string, span);
        let callee = self.field(string, self.syms.format, span);
        let mut args = Vec::with_capacity(values.len() + 1);
        args.push(fmt);
        args.extend(values);
        Ok(self.add_expr(
            hir::ExprKind::Call {
                callee,
                args: args.into(),
            },
            span,
        ))
    }

    /// Translates a hole's spec (Rust's mini-language) to a `string.format` conversion.
    fn format_spec(&self, spec: &ast::FormatSpec) -> LResult<Vec<u8>> {
        let bad = |lx: &Self, what: &str| {
            lx.error(
                spec.span,
                format!(
                    "format spec `{}`: {what}",
                    String::from_utf8_lossy(&spec.text)
                ),
            )
        };
        let t = &spec.text[..];
        let mut i = 0;
        // Fill and alignment: an optional fill character before `<`, `^` or `>`.
        let mut left = false;
        let is_align = |b: u8| matches!(b, b'<' | b'>' | b'^');
        let align = if t.len() >= 2 && is_align(t[1]) && !is_align(t[0]) {
            if t[0] != b' ' {
                return Err(bad(
                    self,
                    "fill characters other than a space are not supported yet",
                ));
            }
            i = 2;
            Some(t[1])
        } else if !t.is_empty() && is_align(t[0]) {
            i = 1;
            Some(t[0])
        } else {
            None
        };
        match align {
            Some(b'<') => left = true,
            Some(b'^') => return Err(bad(self, "centering `^` is not supported yet")),
            _ => {}
        }
        let mut plus = false;
        let mut alt = false;
        let mut zero = false;
        if t.get(i) == Some(&b'+') {
            plus = true;
            i += 1;
        }
        if t.get(i) == Some(&b'#') {
            alt = true;
            i += 1;
        }
        if t.get(i) == Some(&b'0') {
            zero = true;
            i += 1;
        }
        let digits = |i: &mut usize| {
            let start = *i;
            while t.get(*i).is_some_and(u8::is_ascii_digit) {
                *i += 1;
            }
            &t[start..*i]
        };
        let width = digits(&mut i);
        let precision = if t.get(i) == Some(&b'.') {
            i += 1;
            let p = digits(&mut i);
            if p.is_empty() {
                return Err(bad(self, "expected digits after `.`"));
            }
            Some(p)
        } else {
            None
        };
        let ty = match &t[i..] {
            [] => None,
            [b @ (b'x' | b'X' | b'o' | b'e' | b'E' | b'?')] => Some(*b),
            [b'b'] => return Err(bad(self, "binary `b` is not supported yet")),
            _ => return Err(bad(self, "expected a type: one of `x X o e E ?`")),
        };
        if width.len() > 2 || precision.is_some_and(|p| p.len() > 2) {
            return Err(bad(self, "width and precision must be at most 99"));
        }
        let conv = match (ty, precision) {
            (Some(b'?'), _) => {
                if plus
                    || alt
                    || zero
                    || !width.is_empty()
                    || precision.is_some()
                    || align.is_some()
                {
                    return Err(bad(self, "`?` takes no other options"));
                }
                return Ok(b"%q".to_vec());
            }
            (Some(b'x' | b'X' | b'o'), Some(_)) => {
                return Err(bad(self, "precision does not apply to integer types"));
            }
            (Some(b), _) => b,
            (None, Some(_)) => b'f',
            (None, None) => b's',
        };
        let numeric = conv != b's';
        if !numeric {
            if plus || alt {
                return Err(bad(self, "`+` and `#` need a numeric type or a precision"));
            }
            if zero {
                return Err(bad(
                    self,
                    "zero padding `0` needs a numeric type or a precision",
                ));
            }
        }
        if plus && matches!(conv, b'x' | b'X' | b'o') {
            return Err(bad(self, "`+` does not apply to `x`, `X` or `o`"));
        }
        let mut out = vec![b'%'];
        if left {
            out.push(b'-');
        }
        if plus {
            out.push(b'+');
        }
        if alt {
            out.push(b'#');
        }
        if zero && !left {
            out.push(b'0');
        }
        out.extend_from_slice(width);
        if let Some(p) = precision {
            out.push(b'.');
            out.extend_from_slice(p);
        }
        out.push(conv);
        Ok(out)
    }
}
