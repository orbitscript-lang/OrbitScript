//! Control flow: whether code can continue past a block or expression.

use orbit_hir::StrId;
use orbit_orb_syntax::ast::{self, ExprKind, StmtKind};

use super::{Checker, Node, walk_block};

impl<'a, 'b, 'c> Checker<'a, 'b, 'c> {
    pub(super) fn block_diverges(&self, b: &ast::Block) -> bool {
        b.stmts
            .iter()
            .any(|s| matches!(&s.kind, StmtKind::Expr(e) if self.expr_diverges(e)))
            || b.tail.as_ref().is_some_and(|t| self.expr_diverges(t))
    }

    /// Whether control never continues after `e` (`return`, `break`, `error(...)`, ...).
    pub(super) fn expr_diverges(&self, e: &ast::Expr) -> bool {
        match &e.kind {
            ExprKind::Return(_) | ExprKind::Break { .. } | ExprKind::Continue { .. } => true,
            ExprKind::Call { callee, .. } => {
                self.is_prelude(callee, "error") || self.is_prelude(callee, "panic")
            }
            ExprKind::If {
                then,
                else_: Some(e),
                ..
            } => self.block_diverges(then) && self.expr_diverges(e),
            ExprKind::Block(b) => self.block_diverges(b),
            ExprKind::Match { arms, .. } => {
                !arms.is_empty() && arms.iter().all(|a| self.expr_diverges(&a.body))
            }
            ExprKind::Loop { label, body } => !contains_break(body, label.as_ref().map(|l| l.sym)),
            ExprKind::Paren(e) => self.expr_diverges(e),
            _ => false,
        }
    }
}

/// Whether a `break` leaves the loop with this body and `label`: an unlabelled `break`
/// outside any nested loop, or a `break` naming `label` at any depth (not inside a function).
fn contains_break(body: &ast::Block, label: Option<StrId>) -> bool {
    breaks_out(body, label, true)
}

/// Like [`contains_break`]; `unlabelled` says whether a bare `break` here leaves the loop,
/// which stops being true inside a nested loop.
fn breaks_out(body: &ast::Block, label: Option<StrId>, unlabelled: bool) -> bool {
    let mut found = false;
    walk_block(body, &mut |n| match n {
        Node::Expr(e) => match &e.kind {
            ExprKind::Break { label: l, .. } => {
                found |= match l {
                    None => unlabelled,
                    Some(l) => label == Some(l.sym),
                };
                !found
            }
            ExprKind::Loop { body, .. }
            | ExprKind::While { body, .. }
            | ExprKind::For { body, .. } => {
                // A nested loop's own breaks are its business, but one naming our label
                // still leaves us. (Its header is not searched.)
                found = found || (label.is_some() && breaks_out(body, label, false));
                false
            }
            ExprKind::Closure(_) => false,
            _ => !found,
        },
        Node::Stmt(s) => !matches!(s.kind, StmtKind::Item(_)),
    });
    found
}
