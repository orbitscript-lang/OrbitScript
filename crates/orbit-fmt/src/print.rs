//! The printer: a module's AST and its comments back to text, in the canonical layout
//! (planning/dev-tooling/formatter.md).
//!
//! Every node first tries its one-line form ([`Printer::flat`]); when that is too wide, or
//! can't be one line (a block body, a comment inside), it breaks in the way its kind does:
//! lists one entry per line, operator chains before each operator, method chains before each
//! `.`. Comments are placed between the entries of a sequence (statements, list entries,
//! arms, fields); one anywhere else can't be placed yet, and the file is left alone.

use orbit_hir::Span;
use orbit_orb_syntax::ast::*;
use orbit_orb_syntax::lexer::Lexer;
use orbit_orb_syntax::token::{Keyword, Punct, TokenKind};

/// Columns per line.
pub(crate) const WIDTH: usize = 100;
const INDENT: usize = 4;
/// The widest `if c { a } else { b }` kept on one line (rustfmt's
/// `single_line_if_else_max_width`).
const SHORT_IF: usize = 50;

pub(crate) struct Printer<'a> {
    src: &'a [u8],
    comments: &'a [Comment],
    used: Vec<bool>,
    /// The first construct the printer couldn't print, as an offset and a reason.
    pub(crate) failure: Option<(u32, &'static str)>,
    /// Columns the next [`Printer::expr`] leaves free at its end, for what follows it on
    /// its last line (`;`, `,`, `)`).
    reserve: usize,
}

/// One entry of a sequence printed one per line.
#[derive(Clone, Copy)]
enum El<'a> {
    Stmt(&'a Stmt),
    /// A block's final expression.
    Tail(&'a Expr),
    /// A list entry: an argument, an array or tuple element.
    Expr(&'a Expr),
    Entry(&'a MapEntry),
    Field(&'a FieldInit),
    FieldDecl(&'a FieldDecl),
    Variant(&'a Variant),
    Member(&'a Item),
    Arm(&'a Arm),
    Param(&'a Param),
    UseName(&'a (Name, Option<Name>)),
}

/// An entry with the comments that go with it, found in source order before any sorting.
struct Placed<'a> {
    el: El<'a>,
    span: Span,
    /// Where the gap before the entry starts: the end of the entry before it.
    lead_from: u32,
    /// Where the gap after the entry ends: the start of the entry after it.
    trail_to: u32,
    /// The comments on their own lines before the entry, each with whether a blank line
    /// comes before it.
    lead: Vec<(usize, bool)>,
    /// Whether a blank line comes just before the entry (after its comments).
    blank: bool,
    /// The comment after the entry on its last line.
    trail: Option<usize>,
}

fn width(s: &str) -> usize {
    s.chars().count()
}

/// The width of the last line of `s`, starting at column `col` when `s` is one line.
fn end_col(col: usize, s: &str) -> usize {
    match s.rfind('\n') {
        Some(i) => width(&s[i + 1..]),
        None => col + width(s),
    }
}

fn spaces(n: usize) -> String {
    " ".repeat(n)
}

impl<'a> Printer<'a> {
    pub(crate) fn new(src: &'a [u8], comments: &'a [Comment]) -> Printer<'a> {
        Printer {
            src,
            comments,
            used: vec![false; comments.len()],
            failure: None,
            reserve: 0,
        }
    }

    fn fail(&mut self, at: u32, why: &'static str) {
        self.failure.get_or_insert((at, why));
    }

    /// The first comment the printer never placed.
    pub(crate) fn unplaced(&self) -> Option<u32> {
        let i = self.used.iter().position(|u| !u)?;
        Some(self.comments[i].span.start)
    }

    fn text(&self, span: Span) -> &'a str {
        let bytes = &self.src[span.start as usize..span.end as usize];
        std::str::from_utf8(bytes).unwrap_or("")
    }

    fn name(&self, n: Name) -> &'a str {
        self.text(n.span)
    }

    /// Whether a comment lies inside `span`.
    fn has_comment(&self, span: Span) -> bool {
        self.comments
            .iter()
            .any(|c| c.span.start >= span.start && c.span.end <= span.end)
    }

    /// Whether the source between `from` and `to` holds an empty line.
    fn blank_between(&self, from: u32, to: u32) -> bool {
        if from >= to {
            return false;
        }
        let gap = &self.src[from as usize..to as usize];
        let mut newlines = 0;
        for &b in gap {
            match b {
                b'\n' => {
                    newlines += 1;
                    if newlines >= 2 {
                        return true;
                    }
                }
                b' ' | b'\t' | b'\r' => {}
                _ => newlines = 0,
            }
        }
        false
    }

    fn comment_text(&self, c: &Comment) -> &'a str {
        self.text(c.span).trim_end()
    }

    // ---- sequences ------------------------------------------------------------------------

    /// The module's top level, from `start` to `end`.
    pub(crate) fn module(
        &mut self,
        body: &'a Block,
        start: u32,
        end: u32,
        sort_uses: bool,
    ) -> String {
        let els = block_els(body);
        let mut placed = self.place(&els, start, end);
        if sort_uses {
            self.sort_use_runs(&mut placed);
        }
        self.seq(&placed, 0, start, end)
    }

    /// The module's top-level entries that overlap `from..to`, and the source region they
    /// (and their comments) cover.
    pub(crate) fn module_part(
        &mut self,
        body: &'a Block,
        len: u32,
        from: u32,
        to: u32,
    ) -> Option<(Span, String)> {
        let els = block_els(body);
        let mut placed = self.place(&els, 0, len);
        let first = placed
            .iter()
            .position(|p| p.span.end >= from && p.trail_to > from)?;
        let mut last = first;
        while last + 1 < placed.len() && placed[last + 1].lead_from < to {
            last += 1;
        }
        // From the start of the first entry's first comment's line, to the end of the last
        // entry's line.
        let mut start = placed[first].span.start;
        if let Some(&(c, _)) = placed[first].lead.first() {
            start = self.comments[c].span.start;
        }
        placed[first].blank &= !placed[first].lead.is_empty();
        if let Some(c) = placed[first].lead.first_mut() {
            c.1 = false;
        }
        while start > 0 && !matches!(self.src[start as usize - 1], b'\n') {
            start -= 1;
        }
        let mut end = placed[last].span.end;
        while (end as usize) < self.src.len() && self.src[end as usize] != b'\n' {
            end += 1;
        }
        // Everything outside the region counts as placed.
        for (i, c) in self.comments.iter().enumerate() {
            if c.span.start < start || c.span.start >= end {
                self.used[i] = true;
            }
        }
        let slice = &placed[first..=last];
        let mut text = self.seq(slice, 0, start, end);
        if text.ends_with('\n') {
            text.pop();
        }
        Some((Span::new(start, end), text))
    }

    /// Gives each entry the comments in the gaps around it, in source order: a comment on
    /// the entry's last line trails it, and the rest of the gap before it leads it.
    fn place(&self, els: &[El<'a>], start: u32, end: u32) -> Vec<Placed<'a>> {
        let spans: Vec<Span> = els.iter().map(|e| self.el_span(*e)).collect();
        let mut out: Vec<Placed<'a>> = Vec::with_capacity(els.len());
        let mut last: Option<u32> = None;
        let mut prev_trail = None;
        for i in 0..els.len() {
            let lead_from = if i == 0 { start } else { spans[i - 1].end };
            let trail_to = spans.get(i + 1).map_or(end, |s| s.start);
            let mut lead = Vec::new();
            for (c, comment) in self.comments.iter().enumerate() {
                let at = comment.span.start;
                if !self.used[c]
                    && Some(c) != prev_trail
                    && at >= lead_from.max(start)
                    && at < spans[i].start
                {
                    lead.push((c, last.is_some_and(|l| self.blank_between(l, at))));
                    last = Some(comment.span.end);
                }
            }
            let blank = last.is_some_and(|l| self.blank_between(l, spans[i].start));
            let trail = self.trailing(spans[i].end, trail_to.min(end));
            last = Some(trail.map_or(spans[i].end, |c| self.comments[c].span.end));
            prev_trail = trail;
            out.push(Placed {
                el: els[i],
                span: spans[i],
                lead_from,
                trail_to,
                lead,
                blank,
                trail,
            });
        }
        out
    }

    /// `open`, with a comment that follows it on its line (`{ // why`). `at` is where `open`
    /// starts in the source.
    fn opener(&mut self, open: &str, at: u32) -> String {
        let mut out = open.to_owned();
        let from = at as usize + open.len();
        let found = (0..self.comments.len())
            .find(|&i| !self.used[i] && self.comments[i].span.start as usize >= from);
        if let Some(i) = found {
            let gap = &self.src[from.min(self.src.len())..self.comments[i].span.start as usize];
            if gap.iter().all(|b| matches!(b, b' ' | b'\t')) {
                out.push(' ');
                out.push_str(self.comment_text(&self.comments[i]));
                self.used[i] = true;
            }
        }
        out.push('\n');
        out
    }

    /// Prints `entries` one per line at indentation `ind`, with the comments of
    /// `start..end` between them: each line ends in `\n`.
    fn seq(&mut self, entries: &[Placed<'a>], ind: usize, start: u32, end: u32) -> String {
        let pad = spaces(ind);
        let mut out = String::new();
        let mut last: Option<u32> = None;
        for (n, p) in entries.iter().enumerate() {
            for &(i, blank) in &p.lead {
                if self.used[i] {
                    continue;
                }
                if blank {
                    out.push('\n');
                }
                out.push_str(&pad);
                out.push_str(self.comment_text(&self.comments[i]));
                out.push('\n');
                self.used[i] = true;
            }
            if p.blank {
                out.push('\n');
            }
            for &(i, _) in &p.lead {
                self.used[i] = true;
            }
            if let Some(i) = p.trail {
                self.used[i] = true;
            }
            out.push_str(&pad);
            let body = self.el(
                p.el,
                ind,
                self.separator(p.el, n + 1 == entries.len()).len(),
            );
            out.push_str(&body);
            out.push_str(self.separator(p.el, n + 1 == entries.len()));
            last = Some(p.span.end);
            if let Some(i) = p.trail {
                out.push(' ');
                out.push_str(self.comment_text(&self.comments[i]));
                last = Some(self.comments[i].span.end);
            }
            out.push('\n');
        }
        // Comments after the last entry; one inside an entry that its printing didn't place
        // stays unplaced, and the module is refused.
        let after = entries.last().map_or(start, |p| p.span.end);
        for i in 0..self.comments.len() {
            let c = self.comments[i];
            if !self.used[i] && c.span.start >= after && c.span.end <= end {
                if last.is_some_and(|l| self.blank_between(l, c.span.start)) {
                    out.push('\n');
                }
                out.push_str(&pad);
                out.push_str(self.comment_text(&c));
                out.push('\n');
                self.used[i] = true;
                last = Some(c.span.end);
            }
        }
        out
    }

    /// The comment after `from` on the same line, before `to`.
    fn trailing(&self, from: u32, to: u32) -> Option<usize> {
        let i = (0..self.comments.len()).find(|&i| {
            let c = self.comments[i].span;
            !self.used[i] && c.start >= from && c.start < to
        })?;
        let gap = &self.src[from as usize..self.comments[i].span.start as usize];
        (!gap.contains(&b'\n')).then_some(i)
    }

    fn separator(&self, el: El<'a>, last: bool) -> &'static str {
        match el {
            // Without its `;`, a block's last `loop { .. }` would be its value.
            El::Stmt(Stmt {
                kind: StmtKind::Expr(e),
                ..
            }) if last && e.kind.is_block_like() => ";",
            El::Stmt(_) | El::Tail(_) | El::Member(_) => "",
            El::Arm(a) if matches!(a.body.kind, ExprKind::Block(_)) => "",
            _ => ",",
        }
    }

    fn el_span(&self, el: El<'a>) -> Span {
        match el {
            El::Stmt(s) => match &s.kind {
                // A statement's span leaves out its `;`.
                StmtKind::Item(item) => item.span,
                _ => s.span,
            },
            El::Tail(e) | El::Expr(e) => e.span,
            El::Entry(entry) => match entry {
                MapEntry::Positional(e) => e.span,
                MapEntry::Named(_, key, v) => Span::new(key.start, v.span.end),
                MapEntry::Keyed(k, v) => Span::new(self.back_to(k.span.start, b'['), v.span.end),
            },
            El::Field(f) => Span::new(
                f.name.span.start,
                f.value.as_ref().map_or(f.name.span.end, |v| v.span.end),
            ),
            El::FieldDecl(f) => Span::new(
                f.name.span.start,
                f.ty.as_ref().map_or(f.name.span.end, |t| t.span.end),
            ),
            El::Variant(v) => v.span,
            El::Member(m) => m.span,
            El::Arm(a) => Span::new(a.pat.span.start, a.body.span.end),
            El::Param(p) => p.span,
            El::UseName((n, alias)) => {
                Span::new(n.span.start, alias.map_or(n.span.end, |a| a.span.end))
            }
        }
    }

    /// The offset of the `byte` before `at`, past whitespace.
    fn back_to(&self, mut at: u32, byte: u8) -> u32 {
        while at > 0 {
            at -= 1;
            if self.src[at as usize] == byte {
                return at;
            }
        }
        at
    }

    /// The entry, leaving `reserve` columns for its separator.
    fn el(&mut self, el: El<'a>, ind: usize, reserve: usize) -> String {
        match el {
            El::Stmt(s) => self.stmt(s, ind),
            El::Tail(e) | El::Expr(e) => {
                self.reserve = reserve;
                self.expr(e, ind, ind)
            }
            El::Entry(entry) => self.entry(entry, ind, ind, reserve),
            El::Field(f) => self.field_init(f, ind, ind, reserve),
            El::FieldDecl(f) => self.field_decl(f),
            El::Variant(v) => self.variant(v, ind),
            El::Member(m) => self.item(m, ind),
            El::Arm(a) => self.arm(a, ind),
            El::Param(p) => self.param(p),
            El::UseName(n) => self.use_name(n),
        }
    }

    /// Sorts each run of `use` lines with no blank line between them.
    fn sort_use_runs(&self, placed: &mut [Placed<'a>]) {
        let mut i = 0;
        while i < placed.len() {
            if self.use_key(placed[i].el).is_none() {
                i += 1;
                continue;
            }
            let mut j = i + 1;
            while j < placed.len()
                && self.use_key(placed[j].el).is_some()
                && !self.blank_between(placed[j - 1].span.end, placed[j].span.start)
            {
                j += 1;
            }
            // The comments above the run stay above it.
            let header = std::mem::take(&mut placed[i].lead);
            let head_blank = std::mem::replace(&mut placed[i].blank, false);
            placed[i..j].sort_by_key(|p| self.use_key(p.el));
            let first = &mut placed[i];
            match first.lead.first_mut() {
                Some(c) => c.1 = head_blank,
                None => first.blank = head_blank,
            }
            let own = std::mem::replace(&mut first.lead, header);
            first.lead.extend(own);
            i = j;
        }
    }

    fn use_key(&self, el: El<'a>) -> Option<String> {
        let El::Stmt(s) = el else { return None };
        let StmtKind::Item(item) = &s.kind else {
            return None;
        };
        let ItemKind::Use(u) = &item.kind else {
            return None;
        };
        Some(use_path_key(self.src, u))
    }

    // ---- items ------------------------------------------------------------------------------

    fn item(&mut self, item: &'a Item, ind: usize) -> String {
        let mut out = String::new();
        if item.is_pub {
            out.push_str("pub ");
        }
        match &item.kind {
            ItemKind::Fn(f) => {
                out.push_str("fn ");
                out.push_str(self.name(f.name));
                out.push_str(&self.generics(&f.generics));
                let head = out.clone();
                out = self.signature(
                    head,
                    &f.sig,
                    ind,
                    item.span,
                    f.body.as_ref().map(|b| b.span.start),
                );
                match &f.body {
                    Some(body) => {
                        out.push(' ');
                        let b = self.block(body, ind);
                        out.push_str(&b);
                    }
                    None => out.push(';'),
                }
            }
            ItemKind::Struct(s) => {
                out.push_str("struct ");
                out.push_str(self.name(s.name));
                out.push_str(&self.generics(&s.generics));
                let els: Vec<El> = s.fields.iter().map(El::FieldDecl).collect();
                let body = self.braced(&els, ind, item.span);
                out.push(' ');
                out.push_str(&body);
            }
            ItemKind::Enum(e) => {
                out.push_str("enum ");
                out.push_str(self.name(e.name));
                out.push_str(&self.generics(&e.generics));
                let els: Vec<El> = e.variants.iter().map(El::Variant).collect();
                let body = self.braced(&els, ind, item.span);
                out.push(' ');
                out.push_str(&body);
            }
            ItemKind::Impl(i) => {
                out.push_str("impl");
                out.push_str(&self.generics(&i.generics));
                out.push(' ');
                out.push_str(self.name(i.target));
                if !i.target_args.is_empty() {
                    let args: Vec<String> = i.target_args.iter().map(|t| self.ty(t)).collect();
                    out.push('<');
                    out.push_str(&args.join(", "));
                    out.push('>');
                }
                let els: Vec<El> = i.members.iter().map(El::Member).collect();
                let body = self.braced(&els, ind, item.span);
                out.push(' ');
                out.push_str(&body);
            }
            ItemKind::Use(u) => {
                out.push_str("use ");
                let path: Vec<&str> = u.path.iter().map(|n| self.name(*n)).collect();
                out.push_str(&path.join("::"));
                match &u.tail {
                    UseTail::Module { alias } => {
                        if let Some(a) = alias {
                            out.push_str(" as ");
                            out.push_str(self.name(*a));
                        }
                    }
                    UseTail::Members(members) => {
                        let mut els: Vec<&'a (Name, Option<Name>)> = members.iter().collect();
                        if !self.has_comment(item.span) {
                            els.sort_by_key(|m| self.name(m.0));
                        }
                        let flat: Vec<String> = els.iter().map(|m| self.use_name(m)).collect();
                        let one = format!("::{{{}}};", flat.join(", "));
                        if !self.has_comment(item.span) && ind + width(&out) + width(&one) <= WIDTH
                        {
                            out.push_str(&one);
                            return out;
                        }
                        let els: Vec<El> = els.into_iter().map(El::UseName).collect();
                        let open = self.back_to(members[0].0.span.start, b'{');
                        out.push_str("::");
                        let opened = self.opener("{", open);
                        out.push_str(&opened);
                        let placed = self.place(&els, open, item.span.end);
                        out.push_str(&self.seq(&placed, ind + INDENT, open, item.span.end));
                        out.push_str(&spaces(ind));
                        out.push_str("};");
                        return out;
                    }
                }
                out.push(';');
            }
            ItemKind::Const { name, ty, value } => {
                out.push_str("const ");
                out.push_str(self.name(*name));
                if let Some(t) = ty {
                    out.push_str(": ");
                    out.push_str(&self.ty(t));
                }
                if let Some(v) = value {
                    out.push_str(" = ");
                    let col = ind + width(&out);
                    self.reserve = 1;
                    out.push_str(&self.expr(v, ind, col));
                }
                out.push(';');
            }
            ItemKind::TypeAlias { name, generics, ty } => {
                out.push_str("type ");
                out.push_str(self.name(*name));
                out.push_str(&self.generics(generics));
                out.push_str(" = ");
                out.push_str(&self.ty(ty));
                out.push(';');
            }
            ItemKind::Global { name, value } => {
                out.push_str("global ");
                out.push_str(self.name(*name));
                out.push_str(" = ");
                let col = ind + width(&out);
                self.reserve = 1;
                out.push_str(&self.expr(value, ind, col));
                out.push(';');
            }
            ItemKind::Extern(names) => {
                out.push_str("extern ");
                let names: Vec<&str> = names.iter().map(|n| self.name(*n)).collect();
                out.push_str(&names.join(", "));
                out.push(';');
            }
            ItemKind::Error => self.fail(item.span.start, "a broken item"),
        }
        out
    }

    /// `{ entries }` of a struct, enum or `impl`, one entry per line (`{}` when empty).
    fn braced(&mut self, els: &[El<'a>], ind: usize, span: Span) -> String {
        let open = self.brace_after(span);
        if els.is_empty() && !self.has_comment(Span::new(open, span.end)) {
            return "{}".to_owned();
        }
        let mut out = self.opener("{", open);
        let placed = self.place(els, open, span.end);
        out.push_str(&self.seq(&placed, ind + INDENT, open, span.end));
        out.push_str(&spaces(ind));
        out.push('}');
        out
    }

    /// The offset of the first `{` of `span` that is a token (not in a comment or a type).
    fn brace_after(&self, span: Span) -> u32 {
        let text = &self.src[span.start as usize..span.end as usize];
        let mut lx = Lexer::new("", text);
        while let Ok(t) = lx.next_token() {
            match t.kind {
                TokenKind::Punct(Punct::LBrace) => return span.start + t.span.start,
                TokenKind::Eof => break,
                _ => {}
            }
        }
        span.start
    }

    fn generics(&self, generics: &[GenericParam]) -> String {
        if generics.is_empty() {
            return String::new();
        }
        let names: Vec<&str> = generics.iter().map(|g| self.name(g.name)).collect();
        format!("<{}>", names.join(", "))
    }

    /// `head(params) -> ret`, on one line when it fits with the ` {` after it, else one
    /// parameter per line.
    fn signature(
        &mut self,
        head: String,
        sig: &'a FnSig,
        ind: usize,
        span: Span,
        body: Option<u32>,
    ) -> String {
        let ret = sig
            .ret
            .as_ref()
            .map(|t| format!(" -> {}", self.ty(t)))
            .unwrap_or_default();
        let params = self.param_strings(sig);
        let flat = format!("{head}({}){ret}", params.join(", "));
        let params_end = body.unwrap_or(span.end);
        let open = self.paren_after(span.start);
        let inside = Span::new(open, params_end);
        if ind + width(&flat) + 2 <= WIDTH && !self.has_comment(inside) {
            return flat;
        }
        let mut out = head;
        let opened = self.opener("(", open);
        out.push_str(&opened);
        let pad = spaces(ind + INDENT);
        // `self` and the rest parameter aren't `Param`s; their comments go with the
        // neighbouring parameters.
        if sig.self_param.is_some() {
            out.push_str(&pad);
            out.push_str(&params[0]);
            out.push_str(",\n");
        }
        let els: Vec<El> = sig.params.iter().map(El::Param).collect();
        let placed = self.place(&els, open, params_end);
        out.push_str(&self.seq(&placed, ind + INDENT, open, params_end));
        if let Some(rest) = params.last().filter(|_| sig.rest.is_some()) {
            out.push_str(&pad);
            out.push_str(rest);
            out.push_str(",\n");
        }
        out.push_str(&spaces(ind));
        out.push(')');
        out.push_str(&ret);
        out
    }

    fn paren_after(&self, at: u32) -> u32 {
        let mut i = at as usize;
        while i < self.src.len() && self.src[i] != b'(' {
            i += 1;
        }
        i as u32
    }

    fn param_strings(&self, sig: &'a FnSig) -> Vec<String> {
        let mut out = Vec::new();
        if sig.self_param.is_some() {
            out.push(match &sig.self_ty {
                Some(t) => format!("self: {}", self.ty(t)),
                None => "self".to_owned(),
            });
        }
        out.extend(sig.params.iter().map(|p| self.param(p)));
        if let Some(rest) = sig.rest {
            let mut s = format!("...{}", self.name(rest));
            if let Some(t) = &sig.rest_ty {
                s.push_str(": ");
                s.push_str(&self.ty(t));
            }
            out.push(s);
        }
        out
    }

    fn param(&self, p: &'a Param) -> String {
        let mut s = String::new();
        if p.mutable {
            s.push_str("mut ");
        }
        s.push_str(p.name.map_or("_", |n| self.name(n)));
        if let Some(t) = &p.ty {
            s.push_str(": ");
            s.push_str(&self.ty(t));
        }
        s
    }

    fn field_decl(&self, f: &'a FieldDecl) -> String {
        let mut s = String::new();
        if f.is_pub {
            s.push_str("pub ");
        }
        s.push_str(self.name(f.name));
        if let Some(t) = &f.ty {
            s.push_str(": ");
            s.push_str(&self.ty(t));
        }
        s
    }

    fn variant(&mut self, v: &'a Variant, ind: usize) -> String {
        let name = self.name(v.name).to_owned();
        let Some(fields) = &v.fields else {
            return name;
        };
        if fields.is_empty() {
            return format!("{name} {{}}");
        }
        let flat: Vec<String> = fields.iter().map(|f| self.field_decl(f)).collect();
        let one = format!("{name} {{ {} }}", flat.join(", "));
        if !self.has_comment(v.span) && ind + width(&one) < WIDTH {
            return one;
        }
        let els: Vec<El> = fields.iter().map(El::FieldDecl).collect();
        format!("{name} {}", self.braced(&els, ind, v.span))
    }

    fn use_name(&self, n: &'a (Name, Option<Name>)) -> String {
        match n.1 {
            Some(alias) => format!("{} as {}", self.name(n.0), self.name(alias)),
            None => self.name(n.0).to_owned(),
        }
    }

    // ---- statements -------------------------------------------------------------------------

    fn stmt(&mut self, s: &'a Stmt, ind: usize) -> String {
        match &s.kind {
            StmtKind::Let {
                scope,
                pat,
                ty,
                value,
            } => {
                let mut out = "let ".to_owned();
                if *scope {
                    out.push_str("scope ");
                }
                out.push_str(&self.pat(pat));
                if let Some(t) = ty {
                    out.push_str(": ");
                    out.push_str(&self.ty(t));
                }
                if let Some(v) = value {
                    out.push_str(" = ");
                    let col = ind + width(&out);
                    self.reserve = 1;
                    out.push_str(&self.expr(v, ind, col));
                }
                out.push(';');
                out
            }
            StmtKind::Expr(e) => {
                self.reserve = usize::from(!e.kind.is_block_like());
                let mut out = self.expr(e, ind, ind);
                if !e.kind.is_block_like() {
                    out.push(';');
                }
                out
            }
            StmtKind::Assign { targets, value } => {
                let mut out = if targets.len() == 1 {
                    self.expr(&targets[0], ind, ind)
                } else {
                    let parts: Vec<String> =
                        targets.iter().map(|t| self.expr(t, ind, ind)).collect();
                    format!("({})", parts.join(", "))
                };
                out.push_str(" = ");
                let col = end_col(ind, &out);
                self.reserve = 1;
                out.push_str(&self.expr(value, ind, col));
                out.push(';');
                out
            }
            StmtKind::CompoundAssign {
                op, target, value, ..
            } => {
                let mut out = self.expr(target, ind, ind);
                out.push(' ');
                out.push_str(op.as_str());
                out.push_str("= ");
                let col = end_col(ind, &out);
                self.reserve = 1;
                out.push_str(&self.expr(value, ind, col));
                out.push(';');
                out
            }
            StmtKind::Scope(inner) => format!("scope {}", self.stmt(inner, ind)),
            StmtKind::Item(item) => self.item(item, ind),
            StmtKind::Error => {
                self.fail(s.span.start, "a broken statement");
                String::new()
            }
        }
    }

    fn block(&mut self, b: &'a Block, ind: usize) -> String {
        let els = block_els(b);
        if els.is_empty() && !self.has_comment(b.span) {
            return "{}".to_owned();
        }
        let mut out = self.opener("{", b.span.start);
        let placed = self.place(&els, b.span.start, b.span.end);
        out.push_str(&self.seq(&placed, ind + INDENT, b.span.start, b.span.end));
        out.push_str(&spaces(ind));
        out.push('}');
        out
    }

    // ---- expressions ------------------------------------------------------------------------

    /// `e` starting at column `col`, its later lines indented from `ind`.
    fn expr(&mut self, e: &'a Expr, ind: usize, col: usize) -> String {
        let reserve = std::mem::take(&mut self.reserve);
        if let Some(flat) = self.flat(e)
            && col + width(&flat) + reserve <= WIDTH
        {
            return flat;
        }
        match &e.kind {
            ExprKind::Call {
                callee,
                args,
                args_at,
            } => {
                let head = self.expr(callee, ind, col);
                self.call_args(head, args, *args_at, e.span.end, ind, col, reserve)
            }
            ExprKind::MethodCall { .. } | ExprKind::Field { .. } | ExprKind::Try(_)
                if chain_calls(e) >= 2 =>
            {
                self.chain(e, ind, col, reserve)
            }
            ExprKind::MethodCall {
                obj,
                name,
                args,
                args_at,
            } => {
                let mut head = self.expr(obj, ind, col);
                head.push('.');
                head.push_str(self.name(*name));
                self.call_args(head, args, *args_at, e.span.end, ind, col, reserve)
            }
            ExprKind::Field { obj, name } => {
                let mut out = self.expr(obj, ind, col);
                out.push('.');
                out.push_str(self.name(*name));
                out
            }
            ExprKind::Path { base, name } => {
                let mut out = self.expr(base, ind, col);
                out.push_str("::");
                out.push_str(self.name(*name));
                out
            }
            ExprKind::Index { obj, key } => {
                let mut out = self.expr(obj, ind, col);
                out.push('[');
                let c = end_col(col, &out);
                out.push_str(&self.expr(key, ind, c));
                out.push(']');
                out
            }
            ExprKind::Closure(c) => self.closure(c, ind, col),
            ExprKind::Array(items) => {
                let els: Vec<El> = items.iter().map(El::Expr).collect();
                self.list("[", "]", &els, ind, e.span)
            }
            ExprKind::Map(entries) => {
                let els: Vec<El> = entries.iter().map(El::Entry).collect();
                self.list("#{", "}", &els, ind, e.span)
            }
            ExprKind::StructLit { path, fields } => {
                let mut out = self.expr(path, ind, col);
                out.push(' ');
                let els: Vec<El> = fields.iter().map(El::Field).collect();
                let open = self.back_to_brace(path.span.end, e.span.end);
                out.push_str(&self.list("{", "}", &els, ind, Span::new(open, e.span.end)));
                out
            }
            ExprKind::Binary { .. } => self.binary(e, ind, col, reserve),
            ExprKind::Unary { op, expr } => {
                let mut out = op.as_str().to_owned();
                out.push_str(&self.expr(expr, ind, col + 1));
                out
            }
            ExprKind::Paren(inner) => format!("({})", self.expr(inner, ind, col + 1)),
            ExprKind::As { expr, ty, optional } => {
                let mut out = self.expr(expr, ind, col);
                out.push_str(if *optional { " as? " } else { " as " });
                out.push_str(&self.ty(ty));
                out
            }
            ExprKind::Tuple(items) => {
                let els: Vec<El> = items.iter().map(El::Expr).collect();
                self.list("(", ")", &els, ind, e.span)
            }
            ExprKind::Spread(inner) => format!("...{}", self.expr(inner, ind, col + 3)),
            ExprKind::Try(inner) => format!("{}?", self.expr(inner, ind, col)),
            ExprKind::Range {
                start,
                end,
                inclusive,
            } => {
                let mut out = self.expr(start, ind, col);
                out.push_str(if *inclusive { "..=" } else { ".." });
                let c = end_col(col, &out);
                out.push_str(&self.expr(end, ind, c));
                out
            }
            ExprKind::If { .. } => self.if_expr(e, ind, col),
            ExprKind::Match { scrutinee, arms } => {
                let mut out = "match ".to_owned();
                out.push_str(&self.expr(scrutinee, ind, col + 6));
                let open = self.back_to_brace(scrutinee.span.end, e.span.end);
                let els: Vec<El> = arms.iter().map(El::Arm).collect();
                if els.is_empty() && !self.has_comment(Span::new(open, e.span.end)) {
                    out.push_str(" {}");
                    return out;
                }
                out.push(' ');
                let opened = self.opener("{", open);
                out.push_str(&opened);
                let placed = self.place(&els, open, e.span.end);
                out.push_str(&self.seq(&placed, ind + INDENT, open, e.span.end));
                out.push_str(&spaces(ind));
                out.push('}');
                out
            }
            ExprKind::Block(b) => self.block(b, ind),
            ExprKind::Loop { label, body } => {
                let mut out = self.label(*label);
                out.push_str("loop ");
                out.push_str(&self.block(body, ind));
                out
            }
            ExprKind::While { label, cond, body } => {
                let mut out = self.label(*label);
                out.push_str("while ");
                let c = col + width(&out);
                out.push_str(&self.expr(cond, ind, c));
                out.push(' ');
                out.push_str(&self.block(body, ind));
                out
            }
            ExprKind::For {
                label,
                pat,
                iter,
                body,
            } => {
                let mut out = self.label(*label);
                out.push_str("for ");
                out.push_str(&self.pat(pat));
                out.push_str(" in ");
                let c = col + width(&out);
                out.push_str(&self.expr(iter, ind, c));
                out.push(' ');
                out.push_str(&self.block(body, ind));
                out
            }
            ExprKind::Return(Some(v)) => {
                self.reserve = reserve;
                format!("return {}", self.expr(v, ind, col + 7))
            }
            ExprKind::Break {
                label,
                value: Some(v),
            } => {
                let mut out = "break".to_owned();
                if let Some(l) = label {
                    out.push(' ');
                    out.push_str(&self.label_name(*l));
                }
                out.push(' ');
                let c = col + width(&out);
                out.push_str(&self.expr(v, ind, c));
                out
            }
            ExprKind::Error => {
                self.fail(e.span.start, "a broken expression");
                String::new()
            }
            // Leaves, and forms whose one-line text is all there is.
            _ => match self.flat(e) {
                Some(flat) => flat,
                None => {
                    self.fail(e.span.start, "a comment inside an expression");
                    self.text(e.span).to_owned()
                }
            },
        }
    }

    /// The offset of the `{` between `from` and `to`.
    fn back_to_brace(&self, from: u32, to: u32) -> u32 {
        let mut i = from as usize;
        while i < to as usize && self.src[i] != b'{' {
            i += 1;
        }
        i as u32
    }

    /// Whether `e` is a string written over several lines, which stays as written: a call
    /// whose last argument it is keeps its parentheses around it (`f(r#"` ... `"#)`).
    fn multiline_string(&self, e: &Expr) -> bool {
        matches!(e.kind, ExprKind::Str(_) | ExprKind::Template(_))
            && self.text(e.span).contains('\n')
    }

    /// `head(args)`: on one line; or with every argument but the last on the line and the
    /// last (a closure, a table, a `match`, a string over several lines, or a call that is
    /// the only argument) broken from there; or one argument per line.
    #[allow(clippy::too_many_arguments)]
    fn call_args(
        &mut self,
        head: String,
        args: &'a [Expr],
        args_at: u32,
        end: u32,
        ind: usize,
        col: usize,
        reserve: usize,
    ) -> String {
        let at = end_col(col, &head);
        let region = Span::new(args_at, end);
        let flats: Option<Vec<String>> = if self.has_comment(region) {
            None
        } else {
            args.iter().map(|a| self.flat(a)).collect()
        };
        if let Some(flats) = &flats {
            let one = format!("({})", flats.join(", "));
            if at + width(&one) + reserve <= WIDTH {
                return head + &one;
            }
        }
        if let Some(last) = args.last()
            && (overflows(last)
                || self.multiline_string(last)
                || (args.len() == 1 && matches!(last.kind, ExprKind::Call { .. })))
            && !self.has_comment(region)
        {
            let firsts: Option<Vec<String>> = args[..args.len() - 1]
                .iter()
                .map(|a| self.flat(a))
                .collect();
            if let Some(firsts) = firsts {
                let mut prefix = "(".to_owned();
                for f in &firsts {
                    prefix.push_str(f);
                    prefix.push_str(", ");
                }
                let c = at + width(&prefix);
                if c < WIDTH {
                    let saved = self.used.clone();
                    self.reserve = reserve + 1;
                    let tail = self.expr(last, ind, c);
                    let first_line = tail.split('\n').next().unwrap_or("");
                    if tail.contains('\n') && c + width(first_line) <= WIDTH {
                        return head + &prefix + &tail + ")";
                    }
                    self.used = saved;
                }
            }
        }
        let els: Vec<El> = args.iter().map(El::Expr).collect();
        head + &self.list("(", ")", &els, ind, region)
    }

    /// `open entries close`, one entry per line (`open close` when empty).
    fn list(&mut self, open: &str, close: &str, els: &[El<'a>], ind: usize, span: Span) -> String {
        if els.is_empty() && !self.has_comment(span) {
            return format!("{open}{close}");
        }
        let mut out = self.opener(open, span.start);
        let placed = self.place(els, span.start, span.end);
        out.push_str(&self.seq(&placed, ind + INDENT, span.start, span.end));
        out.push_str(&spaces(ind));
        out.push_str(close);
        out
    }

    /// A method chain broken before each `.`: `root` on the first line, each call after it
    /// on a line of its own.
    fn chain(&mut self, e: &'a Expr, ind: usize, col: usize, reserve: usize) -> String {
        let mut links = Vec::new();
        let mut root = e;
        loop {
            match &root.kind {
                ExprKind::MethodCall { obj, .. } | ExprKind::Field { obj, .. } => {
                    links.push(root);
                    root = obj;
                }
                ExprKind::Try(inner) => {
                    links.push(root);
                    root = inner;
                }
                _ => break,
            }
        }
        links.reverse();
        let mut out = self.expr(root, ind, col);
        let inner = ind + INDENT;
        let count = links.len();
        for (n, link) in links.into_iter().enumerate() {
            let reserve = if n + 1 == count { reserve } else { 0 };
            match &link.kind {
                ExprKind::MethodCall {
                    name,
                    args,
                    args_at,
                    ..
                } => {
                    out.push('\n');
                    out.push_str(&spaces(inner));
                    let head = format!(".{}", self.name(*name));
                    let call =
                        self.call_args(head, args, *args_at, link.span.end, inner, inner, reserve);
                    out.push_str(&call);
                }
                ExprKind::Field { name, .. } => {
                    out.push('\n');
                    out.push_str(&spaces(inner));
                    out.push('.');
                    out.push_str(self.name(*name));
                }
                _ => out.push('?'),
            }
        }
        out
    }

    /// An operator chain broken before each operator of the loosest kind in it.
    fn binary(&mut self, e: &'a Expr, ind: usize, col: usize, reserve: usize) -> String {
        let ExprKind::Binary { op, .. } = &e.kind else {
            unreachable!()
        };
        let power = op.binding_power();
        let mut operands = Vec::new();
        let mut cur = e;
        // `**` is right-associative: its chain runs down the right side.
        loop {
            match &cur.kind {
                ExprKind::Binary { op, lhs, rhs, .. }
                    if op.binding_power() == power && power.0 < power.1 =>
                {
                    operands.push((Some(*op), &**rhs));
                    cur = lhs;
                }
                _ => break,
            }
        }
        operands.push((None, cur));
        operands.reverse();
        if operands.len() == 1 {
            // `**`: a single break before the operator.
            let ExprKind::Binary { op, lhs, rhs, .. } = &e.kind else {
                unreachable!()
            };
            operands = vec![(None, &**lhs), (Some(*op), &**rhs)];
        }
        let inner = ind + INDENT;
        let mut out = self.expr(operands[0].1, ind, col);
        let count = operands.len();
        for (n, (op, operand)) in operands.iter().enumerate().skip(1) {
            let op = op.expect("an operator").as_str();
            if n + 1 == count {
                self.reserve = reserve;
            }
            // `if c {\n..\n} else {\n..\n} * 2`: after a first operand that ends in a
            // closing brace of its own, the rest stays on that line when it fits.
            let closer = out.rsplit('\n').next().is_some_and(|l| l.trim() == "}");
            if n == 1
                && closer
                && let Some(flat) = self.flat(operand)
                && end_col(col, &out)
                    + op.len()
                    + 2
                    + width(&flat)
                    + if n + 1 == count { reserve } else { 0 }
                    <= WIDTH
            {
                self.reserve = 0;
                out.push(' ');
                out.push_str(op);
                out.push(' ');
                out.push_str(&flat);
                continue;
            }
            out.push('\n');
            out.push_str(&spaces(inner));
            out.push_str(op);
            out.push(' ');
            let c = inner + op.len() + 1;
            out.push_str(&self.expr(operand, inner, c));
        }
        out
    }

    fn closure(&mut self, c: &'a Closure, ind: usize, col: usize) -> String {
        let mut out = self.closure_params(&c.sig);
        if let Some(ret) = &c.sig.ret {
            out.push_str(" -> ");
            out.push_str(&self.ty(ret));
        }
        out.push(' ');
        let c2 = col + width(&out);
        out.push_str(&self.expr(&c.body, ind, c2));
        out
    }

    fn closure_params(&self, sig: &'a FnSig) -> String {
        let params = self.param_strings(sig);
        if params.is_empty() {
            "||".to_owned()
        } else {
            format!("|{}|", params.join(", "))
        }
    }

    fn if_expr(&mut self, e: &'a Expr, ind: usize, col: usize) -> String {
        let ExprKind::If { cond, then, else_ } = &e.kind else {
            unreachable!()
        };
        let mut out = "if ".to_owned();
        out.push_str(&self.expr(cond, ind, col + 3));
        out.push(' ');
        out.push_str(&self.block(then, ind));
        if let Some(other) = else_ {
            out.push_str(" else ");
            match &other.kind {
                ExprKind::If { .. } => {
                    let c = end_col(col, &out);
                    out.push_str(&self.if_expr(other, ind, c));
                }
                ExprKind::Block(b) => out.push_str(&self.block(b, ind)),
                _ => {
                    let c = end_col(col, &out);
                    out.push_str(&self.expr(other, ind, c));
                }
            }
        }
        out
    }

    fn arm(&mut self, a: &'a Arm, ind: usize) -> String {
        let mut out = self.pat(&a.pat);
        if let Some(g) = &a.guard {
            out.push_str(" if ");
            let c = ind + width(&out);
            out.push_str(&self.expr(g, ind, c));
        }
        out.push_str(" => ");
        let c = end_col(ind, &out);
        match &a.body.kind {
            ExprKind::Block(b) => out.push_str(&self.block(b, ind)),
            _ => out.push_str(&self.expr(&a.body, ind, c)),
        }
        out
    }

    fn entry(&mut self, entry: &'a MapEntry, ind: usize, col: usize, reserve: usize) -> String {
        match entry {
            MapEntry::Positional(e) => {
                self.reserve = reserve;
                self.expr(e, ind, col)
            }
            MapEntry::Named(_, key, v) => {
                let mut out = self.text(*key).to_owned();
                out.push_str(": ");
                let c = col + width(&out);
                self.reserve = reserve;
                out.push_str(&self.expr(v, ind, c));
                out
            }
            MapEntry::Keyed(k, v) => {
                let mut out = "[".to_owned();
                out.push_str(&self.expr(k, ind, col + 1));
                out.push_str("]: ");
                let c = end_col(col, &out);
                self.reserve = reserve;
                out.push_str(&self.expr(v, ind, c));
                out
            }
        }
    }

    fn field_init(&mut self, f: &'a FieldInit, ind: usize, col: usize, reserve: usize) -> String {
        let mut out = self.name(f.name).to_owned();
        if let Some(v) = &f.value {
            out.push_str(": ");
            let c = col + width(&out);
            self.reserve = reserve;
            out.push_str(&self.expr(v, ind, c));
        }
        out
    }

    fn label(&self, label: Option<Name>) -> String {
        match label {
            Some(l) => format!("{}: ", self.label_name(l)),
            None => String::new(),
        }
    }

    fn label_name(&self, l: Name) -> String {
        let t = self.text(l.span);
        if t.starts_with('\'') {
            t.to_owned()
        } else {
            format!("'{t}")
        }
    }

    /// `e` on one line, if it can be: no block bodies, no comments inside.
    fn flat(&self, e: &'a Expr) -> Option<String> {
        if self.has_comment(e.span) {
            return None;
        }
        Some(match &e.kind {
            ExprKind::Nil
            | ExprKind::True
            | ExprKind::False
            | ExprKind::Int(_)
            | ExprKind::Float(_)
            | ExprKind::Str(_)
            | ExprKind::Template(_)
            | ExprKind::Name(_)
            | ExprKind::SelfValue
            | ExprKind::SelfType => self.text(e.span).to_owned(),
            ExprKind::Path { base, name } => format!("{}::{}", self.flat(base)?, self.name(*name)),
            ExprKind::Field { obj, name } => format!("{}.{}", self.flat(obj)?, self.name(*name)),
            ExprKind::Index { obj, key } => format!("{}[{}]", self.flat(obj)?, self.flat(key)?),
            ExprKind::Call { callee, args, .. } => {
                format!("{}({})", self.flat(callee)?, self.flat_list(args)?)
            }
            ExprKind::MethodCall {
                obj, name, args, ..
            } => {
                format!(
                    "{}.{}({})",
                    self.flat(obj)?,
                    self.name(*name),
                    self.flat_list(args)?
                )
            }
            ExprKind::Closure(c) => {
                if c.sig.ret.is_some() || c.body.kind.is_block_like() {
                    return None;
                }
                format!("{} {}", self.closure_params(&c.sig), self.flat(&c.body)?)
            }
            ExprKind::Array(items) => format!("[{}]", self.flat_list(items)?),
            ExprKind::Map(entries) => {
                if entries.is_empty() {
                    return Some("#{}".to_owned());
                }
                let parts: Option<Vec<String>> =
                    entries.iter().map(|en| self.flat_entry(en)).collect();
                format!("#{{ {} }}", parts?.join(", "))
            }
            ExprKind::StructLit { path, fields } => {
                let path = self.flat(path)?;
                if fields.is_empty() {
                    return Some(format!("{path} {{}}"));
                }
                let parts: Option<Vec<String>> = fields
                    .iter()
                    .map(|f| match &f.value {
                        Some(v) => Some(format!("{}: {}", self.name(f.name), self.flat(v)?)),
                        None => Some(self.name(f.name).to_owned()),
                    })
                    .collect();
                format!("{path} {{ {} }}", parts?.join(", "))
            }
            ExprKind::Binary { op, lhs, rhs, .. } => {
                format!("{} {} {}", self.flat(lhs)?, op.as_str(), self.flat(rhs)?)
            }
            ExprKind::Unary { op, expr } => format!("{}{}", op.as_str(), self.flat(expr)?),
            ExprKind::Paren(inner) => format!("({})", self.flat(inner)?),
            ExprKind::As { expr, ty, optional } => {
                format!(
                    "{} as{} {}",
                    self.flat(expr)?,
                    if *optional { "?" } else { "" },
                    self.ty(ty)
                )
            }
            ExprKind::Tuple(items) => format!("({})", self.flat_list(items)?),
            ExprKind::Spread(inner) => format!("...{}", self.flat(inner)?),
            ExprKind::Try(inner) => format!("{}?", self.flat(inner)?),
            ExprKind::Range {
                start,
                end,
                inclusive,
            } => {
                format!(
                    "{}{}{}",
                    self.flat(start)?,
                    if *inclusive { "..=" } else { ".." },
                    self.flat(end)?
                )
            }
            ExprKind::If { cond, then, else_ } => {
                // rustfmt's short form: `if c { a } else { b }`, each branch one expression.
                let other = else_.as_ref()?;
                let ExprKind::Block(other) = &other.kind else {
                    return None;
                };
                let a = self.short_branch(then)?;
                let b = self.short_branch(other)?;
                let s = format!("if {} {{ {a} }} else {{ {b} }}", self.flat(cond)?);
                if width(&s) > SHORT_IF {
                    return None;
                }
                s
            }
            ExprKind::Return(v) => match v {
                Some(v) => format!("return {}", self.flat(v)?),
                None => "return".to_owned(),
            },
            ExprKind::Break { label, value } => {
                let mut s = "break".to_owned();
                if let Some(l) = label {
                    s.push(' ');
                    s.push_str(&self.label_name(*l));
                }
                if let Some(v) = value {
                    s.push(' ');
                    s.push_str(&self.flat(v)?);
                }
                s
            }
            ExprKind::Continue { label } => match label {
                Some(l) => format!("continue {}", self.label_name(*l)),
                None => "continue".to_owned(),
            },
            ExprKind::Match { .. }
            | ExprKind::Block(_)
            | ExprKind::Loop { .. }
            | ExprKind::While { .. }
            | ExprKind::For { .. }
            | ExprKind::Error => return None,
        })
    }

    fn short_branch(&self, b: &'a Block) -> Option<String> {
        if !b.stmts.is_empty() || self.has_comment(b.span) {
            return None;
        }
        let tail = b.tail.as_ref()?;
        if tail.kind.is_block_like() {
            return None;
        }
        self.flat(tail)
    }

    fn flat_list(&self, items: &'a [Expr]) -> Option<String> {
        let parts: Option<Vec<String>> = items.iter().map(|e| self.flat(e)).collect();
        Some(parts?.join(", "))
    }

    fn flat_entry(&self, entry: &'a MapEntry) -> Option<String> {
        Some(match entry {
            MapEntry::Positional(e) => self.flat(e)?,
            MapEntry::Named(_, key, v) => format!("{}: {}", self.text(*key), self.flat(v)?),
            MapEntry::Keyed(k, v) => format!("[{}]: {}", self.flat(k)?, self.flat(v)?),
        })
    }

    // ---- patterns and types -------------------------------------------------------------------

    fn pat(&self, p: &'a Pat) -> String {
        match &p.kind {
            PatKind::Wild => "_".to_owned(),
            PatKind::Bind { name, mutable } => {
                format!("{}{}", if *mutable { "mut " } else { "" }, self.name(*name))
            }
            PatKind::Lit(e) => self.text(e.span).to_owned(),
            PatKind::Range {
                start,
                end,
                inclusive,
            } => format!(
                "{}{}{}",
                self.text(start.span),
                if *inclusive { "..=" } else { ".." },
                self.text(end.span)
            ),
            PatKind::Or(ps) => {
                let parts: Vec<String> = ps.iter().map(|p| self.pat(p)).collect();
                parts.join(" | ")
            }
            PatKind::Tuple(ps) => {
                let parts: Vec<String> = ps.iter().map(|p| self.pat(p)).collect();
                format!("({})", parts.join(", "))
            }
            PatKind::Struct { path, fields, rest } => {
                let path = self.text(path.span);
                let mut parts: Vec<String> = fields
                    .iter()
                    .map(|f| match &f.pat {
                        Some(p) => format!("{}: {}", self.name(f.name), self.pat(p)),
                        None => self.name(f.name).to_owned(),
                    })
                    .collect();
                if *rest {
                    parts.push("..".to_owned());
                }
                if parts.is_empty() {
                    format!("{path} {{}}")
                } else {
                    format!("{path} {{ {} }}", parts.join(", "))
                }
            }
            PatKind::Path(e) => self.text(e.span).to_owned(),
        }
    }

    /// A type annotation's tokens, spaced canonically. The tokens come from the source, so
    /// parentheses the tree flattens (`(A | B)?`) stay.
    fn ty(&self, t: &'a Type) -> String {
        let text = self.text(t.span);
        if self.has_comment(t.span) {
            return text.to_owned();
        }
        let mut lx = Lexer::new("", text.as_bytes());
        let mut out = String::new();
        let mut prev: Option<TokenKind> = None;
        loop {
            let Ok(tok) = lx.next_token() else {
                return text.to_owned();
            };
            if tok.kind == TokenKind::Eof {
                break;
            }
            let piece = &text[tok.span.start as usize..tok.span.end as usize];
            if let Some(p) = prev
                && space_between(p, tok.kind)
            {
                out.push(' ');
            }
            out.push_str(piece);
            prev = Some(tok.kind);
        }
        out
    }
}

/// Whether a type's tokens `a` and `b` take a space between them.
fn space_between(a: TokenKind, b: TokenKind) -> bool {
    use TokenKind::Punct as P;
    let word = |k: TokenKind| {
        matches!(
            k,
            TokenKind::Ident(_) | TokenKind::Keyword(_) | TokenKind::Str(_)
        )
    };
    match (a, b) {
        (P(Punct::Comma | Punct::Colon), _) => true,
        (P(Punct::Pipe | Punct::Arrow), _) | (_, P(Punct::Pipe | Punct::Arrow)) => true,
        (P(Punct::HashBrace), P(Punct::RBrace)) => false,
        (P(Punct::HashBrace), _) | (_, P(Punct::RBrace)) => true,
        (TokenKind::Keyword(Keyword::Fn), P(Punct::LParen)) => false,
        _ => word(a) && word(b),
    }
}

/// How many method calls a chain ending in `e` holds.
fn chain_calls(e: &Expr) -> usize {
    match &e.kind {
        ExprKind::MethodCall { obj, .. } => 1 + chain_calls(obj),
        ExprKind::Field { obj, .. } => chain_calls(obj),
        ExprKind::Try(inner) => chain_calls(inner),
        _ => 0,
    }
}

/// Whether a last argument may break from the call's line: `f(a, |x| {`, `f(#{` or
/// `f(a, match x {`.
fn overflows(e: &Expr) -> bool {
    matches!(
        e.kind,
        ExprKind::Closure(_)
            | ExprKind::Map(_)
            | ExprKind::Array(_)
            | ExprKind::StructLit { .. }
            | ExprKind::Match { .. }
    )
}

fn block_els(b: &Block) -> Vec<El<'_>> {
    let mut els: Vec<El> = b.stmts.iter().map(El::Stmt).collect();
    if let Some(t) = &b.tail {
        els.push(El::Tail(t));
    }
    els
}

/// The sort key of a `use` line: its path as written.
pub(crate) fn use_path_key(src: &[u8], u: &UseDecl) -> String {
    let text = |n: &Name| {
        String::from_utf8_lossy(&src[n.span.start as usize..n.span.end as usize]).into_owned()
    };
    let mut key: Vec<String> = u.path.iter().map(text).collect();
    if let UseTail::Module { alias: Some(a) } = &u.tail {
        key.push(format!(" as {}", text(a)));
    }
    key.join("::")
}
