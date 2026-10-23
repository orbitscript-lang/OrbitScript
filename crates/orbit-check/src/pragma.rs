//! Comment pragmas (proposal T13): `// orbit-check: ignore` or `ignore[T0207, T0201]` at the end
//! of a line drops what the checker reports on that line, and `//! orbit-check: strict` or `off`
//! sets how a whole module is checked.

use orbit_hir::{SourceInfo, Span};
use orbit_orb_syntax::ast::{Comment, CommentKind};

use crate::{Code, Diagnostic, FileId};

const PREFIX: &str = "orbit-check:";

/// How a module is checked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Mode {
    Default,
    Strict,
    /// Only syntax errors are reported.
    Off,
}

struct Ignore {
    line: u32,
    /// `None` ignores every code.
    codes: Option<Vec<Code>>,
    span: Span,
    used: bool,
}

pub(crate) struct Pragmas {
    /// The module's own `//! orbit-check: strict` or `off`.
    pub mode: Option<Mode>,
    ignores: Vec<Ignore>,
    /// Pragmas it doesn't understand, with what to say about each.
    bad: Vec<(Span, String)>,
}

impl Pragmas {
    pub fn read(text: &[u8], comments: &[Comment], source: &SourceInfo) -> Pragmas {
        let mut p = Pragmas {
            mode: None,
            ignores: Vec::new(),
            bad: Vec::new(),
        };
        for c in comments {
            let raw = &text[c.span.start as usize..c.span.end as usize];
            let body = match c.kind {
                CommentKind::InnerDoc => &raw[3..],
                CommentKind::Line => &raw[2..],
                CommentKind::Doc | CommentKind::Block => continue,
            };
            let Ok(body) = std::str::from_utf8(body) else {
                continue;
            };
            let Some(rest) = body.trim().strip_prefix(PREFIX) else {
                continue;
            };
            let rest = rest.trim();
            let line = source.line_of(c.span.start);
            match (c.kind, rest) {
                (CommentKind::InnerDoc, "strict") => p.mode = Some(Mode::Strict),
                (CommentKind::InnerDoc, "off") => p.mode = Some(Mode::Off),
                (CommentKind::Line, "ignore") => p.ignores.push(Ignore {
                    line,
                    codes: None,
                    span: c.span,
                    used: false,
                }),
                (CommentKind::Line, r) if r.starts_with("ignore[") && r.ends_with(']') => {
                    let mut codes = Vec::new();
                    for name in r["ignore[".len()..r.len() - 1].split(',').map(str::trim) {
                        match Code::parse(name) {
                            Some(code) => codes.push(code),
                            None => p
                                .bad
                                .push((c.span, format!("`{name}` is not a diagnostic code"))),
                        }
                    }
                    p.ignores.push(Ignore {
                        line,
                        codes: Some(codes),
                        span: c.span,
                        used: false,
                    });
                }
                (kind, r) => {
                    let msg = if kind == CommentKind::Line && matches!(r, "strict" | "off") {
                        format!("`{r}` applies to a whole module: write `//! orbit-check: {r}`")
                    } else {
                        format!(
                            "`orbit-check: {r}` is not a pragma; a line takes `// orbit-check: \
                             ignore` or `ignore[T0201]`, a module `//! orbit-check: strict` or `off`"
                        )
                    };
                    p.bad.push((c.span, msg));
                }
            }
        }
        p
    }

    /// Drops what the `ignore` pragmas cover from `diags`, then adds the pragmas' own warnings:
    /// ones it doesn't understand, and in strict mode ones that ignore nothing.
    pub fn apply(
        mut self,
        file: FileId,
        source: &SourceInfo,
        strict: bool,
        diags: &mut Vec<Diagnostic>,
    ) {
        diags.retain(|d| {
            // A syntax error is never ignored: the module wouldn't run.
            if d.code == Code::Syntax {
                return true;
            }
            let line = source.line_of(d.span.start);
            let hit = self.ignores.iter_mut().find(|i| {
                i.line == line && i.codes.as_ref().map_or(true, |cs| cs.contains(&d.code))
            });
            match hit {
                Some(i) => {
                    i.used = true;
                    false
                }
                None => true,
            }
        });
        for (span, msg) in self.bad {
            diags.push(Diagnostic::warning(file, span, Code::BadPragma, msg));
        }
        if strict {
            for i in self.ignores.iter().filter(|i| !i.used) {
                diags.push(Diagnostic::warning(
                    file,
                    i.span,
                    Code::UnusedIgnore,
                    "this `orbit-check: ignore` has nothing to ignore".to_string(),
                ));
            }
        }
    }
}
