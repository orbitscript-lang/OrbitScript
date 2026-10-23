//! Lints from the proposal ("Diagnostics and use"): warnings about code that runs but is probably
//! wrong, and that need only the AST and the resolved names. They skip stubs and bundled
//! modules.

use std::collections::HashSet;

use orbit_hir::Span;
use orbit_orb_syntax::ast::{self, ExprKind, ItemKind, PatKind, StmtKind, UnOp, UseTail};

use super::passes::{Pass, Typed};
use super::walk::{Node, walk_block};
use crate::symbols::{SymbolId, SymbolKind};
use crate::{Code, Diagnostic};

/// `for i in 0..#t { ... t[i] ... }` visits `t[0]`, which is never set, and misses `t[#t]`.
pub(super) struct ZeroBasedLoops;

impl Pass for ZeroBasedLoops {
    #[inline]
    fn node(&self, m: &Typed<'_, '_, '_>, n: Node<'_>, out: &mut Vec<Diagnostic>) {
        let Some(text) = m.lint_text else { return };
        let src = |s: Span| &text[s.start as usize..s.end as usize];
        let Node::Expr(e) = n else { return };
        let ExprKind::For {
            pat, iter, body, ..
        } = &e.kind
        else {
            return;
        };
        let (
            PatKind::Bind { name, .. },
            ExprKind::Range {
                start,
                end,
                inclusive: false,
            },
        ) = (&pat.kind, &iter.kind)
        else {
            return;
        };
        let (
            ExprKind::Int(0),
            ExprKind::Unary {
                op: UnOp::Len,
                expr: arr,
            },
        ) = (&start.kind, &end.kind)
        else {
            return;
        };
        let mut indexes = false;
        walk_block(body, &mut |n| {
            if let Node::Expr(e) = n
                && let ExprKind::Index { obj, key } = &e.kind
                && matches!(&key.kind, ExprKind::Name(k) if *k == name.sym)
                && src(obj.span) == src(arr.span)
            {
                indexes = true;
            }
            !indexes
        });
        if indexes {
            let arr = String::from_utf8_lossy(src(arr.span));
            out.push(Diagnostic::warning(
                m.file,
                iter.span,
                Code::ZeroBasedLoop,
                format!(
                    "arrays start at 1: this loop reads `{arr}[0]`, which is never set, and \
                     skips the last element; write `1..=#{arr}`"
                ),
            ));
        }
    }
}

/// Locals that are never read, and `use`d names that are never used. A name starting with `_`
/// is exempt, as is a `let scope` binding (it is there to be closed).
pub(super) struct Unused;

impl Pass for Unused {
    fn module(&self, m: &Typed<'_, '_, '_>, out: &mut Vec<Diagnostic>) {
        if m.lint_text.is_some() {
            unused(m, out);
        }
    }
}

fn unused(t: &Typed<'_, '_, '_>, out: &mut Vec<Diagnostic>) {
    let (file, ast, m) = (t.file, t.ast, t.syms.module());
    let id = |index: usize| SymbolId {
        file,
        index: index as u32,
    };
    // Where `use` items are: references inside them are the import itself, not uses.
    let mut uses: Vec<(Span, bool)> = Vec::new();
    let mut imported: Vec<(SymbolId, ast::Name)> = Vec::new();
    for s in &ast.body.stmts {
        if let StmtKind::Item(item) = &s.kind
            && let ItemKind::Use(u) = &item.kind
        {
            uses.push((item.span, item.is_pub));
            if item.is_pub {
                continue;
            }
            // Members of a typed module refer straight to its items.
            if let UseTail::Members(members) = &u.tail {
                for (field, alias) in members {
                    let shown = alias.unwrap_or(*field);
                    if let Some(&(_, target)) = m.refs.iter().find(|(sp, _)| *sp == field.span)
                        && target.file != file
                    {
                        imported.push((target, shown));
                    }
                }
            }
        }
    }
    let in_use = |s: Span| {
        uses.iter()
            .any(|(u, _)| u.start <= s.start && s.end <= u.end)
    };
    let mut scoped = HashSet::new();
    walk_block(&ast.body, &mut |n| {
        if let Node::Stmt(s) = n
            && let StmtKind::Let {
                scope: true, pat, ..
            } = &s.kind
            && let PatKind::Bind { name, .. } = &pat.kind
        {
            scoped.insert(name.span);
        }
        true
    });
    let read: HashSet<SymbolId> = m
        .refs
        .iter()
        // A symbol's own definition is among its references.
        .filter(|&&(s, target)| {
            !in_use(s) && !(target.file == file && m.symbols[target.index as usize].span == s)
        })
        .map(|&(_, target)| target)
        .collect();
    for (i, sym) in m.symbols.iter().enumerate() {
        if sym.name.starts_with('_') || read.contains(&id(i)) {
            continue;
        }
        let code = match sym.kind {
            SymbolKind::Local if !scoped.contains(&sym.span) => Code::UnusedLocal,
            // A module that wasn't found, or a member it lacks, is reported already.
            SymbolKind::Module if sym.module.is_none() => continue,
            SymbolKind::Import if t.diagnostics.iter().any(|d| d.span == sym.span) => continue,
            SymbolKind::Module | SymbolKind::Import
                if uses.iter().any(|&(u, public)| {
                    !public && u.start <= sym.span.start && sym.span.end <= u.end
                }) =>
            {
                Code::UnusedImport
            }
            _ => continue,
        };
        let msg = if code == Code::UnusedLocal {
            format!("`{0}` is never read; remove it or name it `_{0}`", sym.name)
        } else {
            format!("`{}` is imported but never used; remove it", sym.name)
        };
        out.push(Diagnostic::warning(file, sym.span, code, msg));
    }
    for (target, name) in imported {
        if !read.contains(&target) {
            let text = ast.strings.display(name.sym);
            let msg = format!("`{text}` is imported but never used; remove it");
            out.push(Diagnostic::warning(
                file,
                name.span,
                Code::UnusedImport,
                msg,
            ));
        }
    }
}
