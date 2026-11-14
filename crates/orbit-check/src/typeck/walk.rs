//! The AST walk that the typing walk's helpers and the passes share.

use orbit_orb_syntax::ast::{self, ExprKind, ItemKind, MapEntry, StmtKind};

/// A statement or expression the walk visits.
#[derive(Clone, Copy)]
pub(super) enum Node<'a> {
    Stmt(&'a ast::Stmt),
    Expr(&'a ast::Expr),
}

/// Visits the statements and expressions in `b`, depth first. `f` says whether to go into a
/// node's children.
pub(super) fn walk_block<'a>(b: &'a ast::Block, f: &mut dyn FnMut(Node<'a>) -> bool) {
    for s in &b.stmts {
        walk_stmt(s, f);
    }
    if let Some(t) = &b.tail {
        walk_expr(t, f);
    }
}

fn walk_stmt<'a>(s: &'a ast::Stmt, f: &mut dyn FnMut(Node<'a>) -> bool) {
    if !f(Node::Stmt(s)) {
        return;
    }
    match &s.kind {
        StmtKind::Error => {}
        StmtKind::Let { value, .. } => {
            if let Some(v) = value {
                walk_expr(v, f);
            }
        }
        StmtKind::Expr(e) => walk_expr(e, f),
        StmtKind::Assign { targets, value } => {
            for t in targets {
                walk_expr(t, f);
            }
            walk_expr(value, f);
        }
        StmtKind::CompoundAssign { target, value, .. } => {
            walk_expr(target, f);
            walk_expr(value, f);
        }
        StmtKind::Scope(inner) => walk_stmt(inner, f),
        StmtKind::Item(item) => walk_item(item, f),
    }
}

fn walk_item<'a>(item: &'a ast::Item, f: &mut dyn FnMut(Node<'a>) -> bool) {
    match &item.kind {
        ItemKind::Fn(d) => {
            if let Some(b) = &d.body {
                walk_block(b, f);
            }
        }
        ItemKind::Impl(imp) => {
            for m in &imp.members {
                walk_item(m, f);
            }
        }
        ItemKind::Const { value: Some(v), .. } | ItemKind::Global { value: v, .. } => {
            walk_expr(v, f)
        }
        _ => {}
    }
}

/// Chains of operators and suffixes (`a + b + c`, `a.b(c)[d]`) and `else if` chains are trees
/// as deep as they are long, so they are walked in a loop: down a chain's left operands, then
/// back up through each link's other operands, and on along `else`.
fn walk_expr<'a>(e: &'a ast::Expr, f: &mut dyn FnMut(Node<'a>) -> bool) {
    let mut links = Vec::new();
    let mut next = Some(e);
    while let Some(e) = next.take() {
        if !f(Node::Expr(e)) {
            break;
        }
        if let Some(lhs) = e.chain_lhs() {
            links.push(e);
            next = Some(lhs);
        } else if let ExprKind::If { cond, then, else_ } = &e.kind {
            walk_expr(cond, f);
            walk_block(then, f);
            next = else_.as_deref();
        } else {
            walk_children(e, f);
        }
    }
    for link in links.into_iter().rev() {
        match &link.kind {
            ExprKind::Index { key: x, .. } | ExprKind::Binary { rhs: x, .. } => walk_expr(x, f),
            ExprKind::Call { args, .. } | ExprKind::MethodCall { args, .. } => {
                for a in args {
                    walk_expr(a, f);
                }
            }
            _ => {}
        }
    }
}

/// Walks the children of `e`, which is neither a chain link nor an `if`.
fn walk_children<'a>(e: &'a ast::Expr, f: &mut dyn FnMut(Node<'a>) -> bool) {
    match &e.kind {
        ExprKind::Nil
        | ExprKind::Error
        | ExprKind::True
        | ExprKind::False
        | ExprKind::Int(_)
        | ExprKind::Float(_)
        | ExprKind::Str(_)
        | ExprKind::Name(_)
        | ExprKind::SelfValue
        | ExprKind::SelfType
        | ExprKind::Continue { .. } => {}
        ExprKind::Template(pieces) => {
            for p in pieces {
                if let ast::TemplatePiece::Hole { expr, .. } = p {
                    walk_expr(expr, f);
                }
            }
        }
        ExprKind::Unary { expr: x, .. } | ExprKind::Paren(x) | ExprKind::Spread(x) => {
            walk_expr(x, f)
        }
        ExprKind::Range {
            start: a, end: b, ..
        } => {
            walk_expr(a, f);
            walk_expr(b, f);
        }
        ExprKind::Closure(c) => walk_expr(&c.body, f),
        ExprKind::Array(items) | ExprKind::Tuple(items) => {
            for i in items {
                walk_expr(i, f);
            }
        }
        ExprKind::Map(entries) => {
            for entry in entries {
                match entry {
                    MapEntry::Positional(v) | MapEntry::Named(_, _, v) => walk_expr(v, f),
                    MapEntry::Keyed(k, v) => {
                        walk_expr(k, f);
                        walk_expr(v, f);
                    }
                }
            }
        }
        ExprKind::StructLit { path, fields } => {
            walk_expr(path, f);
            for fi in fields {
                if let Some(v) = &fi.value {
                    walk_expr(v, f);
                }
            }
        }
        ExprKind::Path { .. }
        | ExprKind::Field { .. }
        | ExprKind::Index { .. }
        | ExprKind::Call { .. }
        | ExprKind::MethodCall { .. }
        | ExprKind::Binary { .. }
        | ExprKind::As { .. }
        | ExprKind::Try(_)
        | ExprKind::If { .. } => unreachable!("walked by `walk_expr`"),
        ExprKind::Match { scrutinee, arms } => {
            walk_expr(scrutinee, f);
            for a in arms {
                if let Some(g) = &a.guard {
                    walk_expr(g, f);
                }
                walk_expr(&a.body, f);
            }
        }
        ExprKind::Block(b) | ExprKind::Loop { body: b, .. } => walk_block(b, f),
        ExprKind::While { cond, body, .. } => {
            walk_expr(cond, f);
            walk_block(body, f);
        }
        ExprKind::For { iter, body, .. } => {
            walk_expr(iter, f);
            walk_block(body, f);
        }
        ExprKind::Break { value, .. } | ExprKind::Return(value) => {
            if let Some(v) = value {
                walk_expr(v, f);
            }
        }
    }
}
