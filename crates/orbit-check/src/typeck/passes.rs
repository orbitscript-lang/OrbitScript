//! The passes that run after the typing walk. Each reads the typed module (the AST, the resolved
//! names and the type the walk found for each expression) and reports what it finds. None of
//! them changes a type, so they can run in any order.

use std::cmp::Reverse;

use orbit_hir::Span;
use orbit_orb_syntax::ast::{self, BinOp, ExprKind, PatKind, StmtKind, UnOp};

use super::lint::{Unused, ZeroBasedLoops};
use super::view::SymbolView;
use super::walk::{Node, walk_block};
use crate::symbols::{SymbolId, SymbolKind};
use crate::types::{Names, Ty, TypeId, Types};
use crate::{Code, Diagnostic, FileId};

/// A module after the typing walk: what the passes read.
pub(super) struct Typed<'x, 'm, 'c> {
    pub file: FileId,
    pub ast: &'x ast::Ast,
    pub syms: &'x SymbolView<'m, 'c>,
    pub types: &'x Types,
    /// Strict mode: conditions must be `bool`.
    pub strict: bool,
    /// The module's text, when it gets lints (it isn't a stub or a bundled module).
    pub lint_text: Option<&'x [u8]>,
    /// What resolution and the walk reported.
    pub diagnostics: &'x [Diagnostic],
    /// The type the walk recorded for each expression, sorted by span (by start, then longest
    /// first). The walk can record a span more than once; the last one counts.
    pub expr_types: &'x [(Span, TypeId)],
}

impl Typed<'_, '_, '_> {
    /// The type the walk found for the expression or binding at `span`. Block-like expressions
    /// (`if`, `match`, blocks and loops) have none.
    pub fn type_at(&self, span: Span) -> Option<TypeId> {
        let key = |s: Span| (s.start, Reverse(s.end));
        let i = self
            .expr_types
            .partition_point(|&(s, _)| key(s) <= key(span));
        let &(s, t) = self.expr_types.get(i.checked_sub(1)?)?;
        (s == span).then_some(t)
    }

    pub fn type_of(&self, e: &ast::Expr) -> Option<TypeId> {
        self.type_at(e.span)
    }

    fn show(&self, t: TypeId) -> String {
        format!("`{}`", self.types.display(t, self.syms))
    }

    /// `t`'s members other than `nil`: none for `nil` itself.
    fn non_nil_members(&self, t: TypeId) -> Vec<TypeId> {
        let mut ms = self.types.members(t);
        ms.retain(|&m| m != self.types.nil);
        ms
    }

    /// Whether the operand type `t` is a number once `nil` is set aside (an operand that may be
    /// nil is reported as such), and whether it may be a float.
    fn numeric(&self, t: TypeId) -> (bool, bool) {
        let ms = self.non_nil_members(t);
        let numeric = !ms.is_empty()
            && ms
                .iter()
                .all(|&m| matches!(self.types.get(m), Ty::Int | Ty::Float));
        (numeric, ms.contains(&self.types.float))
    }
}

/// A check on the typed module. Most look at one statement or expression at a time, in a walk
/// the passes share; some look at the module as a whole.
pub(super) trait Pass {
    /// Looks at one statement or expression: every one in the module, in function bodies too.
    fn node(&self, _m: &Typed<'_, '_, '_>, _n: Node<'_>, _out: &mut Vec<Diagnostic>) {}

    /// Looks at the module as a whole, after the walk.
    fn module(&self, _m: &Typed<'_, '_, '_>, _out: &mut Vec<Diagnostic>) {}
}

/// Runs every pass over `m`, adding what they report to `out`. The passes that look at nodes
/// share one walk. They are called directly rather than through `dyn Pass` so that they inline
/// into it: through a vtable, the passes took three times as many instructions.
pub(super) fn run(m: &Typed<'_, '_, '_>, out: &mut Vec<Diagnostic>) {
    walk_block(&m.ast.body, &mut |n| {
        ZeroIndex.node(m, n, out);
        FloatBitwise.node(m, n, out);
        NeverEqual.node(m, n, out);
        Conditions.node(m, n, out);
        Coverage.node(m, n, out);
        ScopeClose.node(m, n, out);
        ZeroBasedLoops.node(m, n, out);
        true
    });
    Unused.module(m, out);
}

/// `t[0]` on an array: Lua arrays start at 1.
struct ZeroIndex;

impl Pass for ZeroIndex {
    #[inline]
    fn node(&self, m: &Typed<'_, '_, '_>, n: Node<'_>, out: &mut Vec<Diagnostic>) {
        if let Node::Expr(e) = n {
            if let ExprKind::Index { obj, key } = &e.kind {
                if matches!(key.kind, ExprKind::Int(0)) {
                    if let Some(t) = m.type_of(obj) {
                        if let [a] = m.non_nil_members(t)[..] {
                            if matches!(m.types.get(a), Ty::Array(_)) {
                                let msg = "arrays start at 1, so `[0]` is never an element; the first is `[1]`";
                                out.push(Diagnostic::warning(
                                    m.file,
                                    key.span,
                                    Code::ZeroIndex,
                                    msg,
                                ));
                            }
                        }
                    }
                }
            }
        }
    }
}

/// A bitwise operator on a float: Lua converts only floats with an integral value. Operands
/// that aren't numbers are type errors, and ones with a metamethod aren't numbers.
struct FloatBitwise;

impl FloatBitwise {
    fn check(
        m: &Typed<'_, '_, '_>,
        op: BinOp,
        l: &ast::Expr,
        r: &ast::Expr,
        span: Span,
    ) -> Option<Diagnostic> {
        use BinOp::*;
        if !matches!(op, BAnd | BOr | BXor | Shl | Shr) {
            return None;
        }
        let (l_num, l_float) = m.numeric(m.type_of(l)?);
        let (r_num, r_float) = m.numeric(m.type_of(r)?);
        (l_num && r_num && (l_float || r_float)).then(|| {
            let msg = format!(
                "`{}` on a float: Lua converts only floats with an integral value",
                op.as_str()
            );
            Diagnostic::warning(m.file, span, Code::FloatBitwise, msg)
        })
    }
}

impl Pass for FloatBitwise {
    #[inline]
    fn node(&self, m: &Typed<'_, '_, '_>, n: Node<'_>, out: &mut Vec<Diagnostic>) {
        let found = match n {
            Node::Expr(e) => match &e.kind {
                ExprKind::Binary { op, lhs, rhs, .. } => Self::check(m, *op, lhs, rhs, e.span),
                ExprKind::Unary {
                    op: UnOp::BNot,
                    expr,
                } => m.type_of(expr).and_then(|t| {
                    let (num, float) = m.numeric(t);
                    (num && float).then(|| {
                        Diagnostic::warning(
                            m.file,
                            expr.span,
                            Code::FloatBitwise,
                            "`~` on a float: Lua converts only floats with an integral value",
                        )
                    })
                }),
                _ => None,
            },
            Node::Stmt(s) => match &s.kind {
                StmtKind::CompoundAssign {
                    op, target, value, ..
                } => Self::check(m, *op, target, value, s.span),
                _ => None,
            },
        };
        out.extend(found);
    }
}

/// `==` or `!=` on values of different Lua types, which are never equal.
struct NeverEqual;

impl NeverEqual {
    /// Lua's type families, as bits: values from different families are never equal.
    fn families(types: &Types, t: TypeId) -> u32 {
        match types.get(t) {
            Ty::Any | Ty::Param(_) | Ty::Rest(_) => u32::MAX,
            Ty::Nil => 1,
            Ty::Bool => 2,
            Ty::Int | Ty::Float => 4,
            Ty::Str => 8,
            Ty::Table
            | Ty::Array(_)
            | Ty::Map(..)
            | Ty::Record { .. }
            | Ty::Nominal(..)
            | Ty::Variant(..) => 16,
            Ty::Fn(_) | Ty::Function => 32,
            Ty::Thread => 64,
            Ty::Userdata => 128,
            Ty::Union(ms) => ms.iter().fold(0, |acc, &m| acc | Self::families(types, m)),
        }
    }
}

impl Pass for NeverEqual {
    #[inline]
    fn node(&self, m: &Typed<'_, '_, '_>, n: Node<'_>, out: &mut Vec<Diagnostic>) {
        if let Node::Expr(e) = n {
            if let ExprKind::Binary {
                op: BinOp::Eq | BinOp::Ne,
                lhs,
                rhs,
                ..
            } = &e.kind
            {
                if let (Some(l), Some(r)) = (m.type_of(lhs), m.type_of(rhs)) {
                    if Self::families(m.types, l) & Self::families(m.types, r) == 0 {
                        let msg = format!(
                            "this compares {} with {}, which are never equal",
                            m.show(l),
                            m.show(r)
                        );
                        out.push(Diagnostic::warning(
                            m.file,
                            e.span,
                            Code::AlwaysUnequal,
                            msg,
                        ));
                    }
                }
            }
        }
    }
}

/// The condition of an `if` or `while`: one that can't be false, and in strict mode one that
/// isn't a `bool`.
struct Conditions;

impl Conditions {
    fn check(m: &Typed<'_, '_, '_>, t: TypeId, span: Span) -> Option<Diagnostic> {
        let ms = m.types.members(t);
        if m.strict
            && ms
                .iter()
                .any(|&t| !matches!(m.types.get(t), Ty::Any | Ty::Bool))
        {
            let hint = if ms.iter().any(|&t| *m.types.get(t) == Ty::Nil) {
                "compare it, such as `x != nil`"
            } else {
                "compare it, such as `n > 0`"
            };
            let msg = format!(
                "this condition has type {}, not `bool` (strict mode); {hint}",
                m.show(t)
            );
            return Some(Diagnostic::error(m.file, span, Code::NotBool, msg));
        }
        let always = ms
            .iter()
            .all(|&t| !matches!(m.types.get(t), Ty::Any | Ty::Nil | Ty::Bool | Ty::Param(_)));
        always.then(|| {
            let msg = format!(
                "this condition has type {}, which is always true",
                m.show(t)
            );
            Diagnostic::warning(m.file, span, Code::AlwaysTrue, msg)
        })
    }
}

impl Pass for Conditions {
    #[inline]
    fn node(&self, m: &Typed<'_, '_, '_>, n: Node<'_>, out: &mut Vec<Diagnostic>) {
        if let Node::Expr(e) = n {
            if let ExprKind::If { cond, .. } | ExprKind::While { cond, .. } = &e.kind {
                if let Some(t) = m.type_of(cond) {
                    out.extend(Self::check(m, t, cond.span));
                }
            }
        }
    }
}

/// A `match` on an enum, `bool` or optional value that misses cases.
struct Coverage;

#[derive(Clone, Copy, PartialEq)]
enum Case {
    Variant(SymbolId),
    True,
    False,
    Nil,
}

impl Coverage {
    fn check(
        m: &Typed<'_, '_, '_>,
        st: TypeId,
        arms: &[ast::Arm],
        span: Span,
    ) -> Option<Diagnostic> {
        if st == m.types.any {
            return None;
        }
        let mut needed = Vec::new();
        for t in m.types.members(st) {
            match m.types.get(t) {
                Ty::Nominal(id, _) if m.syms.kind(*id) == Some(SymbolKind::Enum) => {
                    needed.extend(m.syms.variants(*id).into_iter().map(Case::Variant));
                }
                Ty::Variant(id, _) => needed.push(Case::Variant(*id)),
                Ty::Bool => needed.extend([Case::True, Case::False]),
                Ty::Nil => needed.push(Case::Nil),
                _ => {}
            }
        }
        if needed.is_empty() {
            return None;
        }
        let mut covered = Vec::new();
        for arm in arms {
            if arm.guard.is_none() && Self::covers(m.syms, &arm.pat, &mut covered) {
                return None;
            }
        }
        let missing: Vec<String> = needed
            .iter()
            .filter(|c| !covered.contains(c))
            .map(|c| match c {
                Case::Variant(v) => format!("`{}`", m.syms.name(*v)),
                Case::True => "`true`".into(),
                Case::False => "`false`".into(),
                Case::Nil => "`nil`".into(),
            })
            .collect();
        if missing.is_empty() {
            return None;
        }
        let msg = format!(
            "`match` doesn't cover {}; add {} or `_`",
            missing.join(", "),
            if missing.len() == 1 {
                "an arm for it"
            } else {
                "arms for them"
            }
        );
        Some(Diagnostic::error(m.file, span, Code::NonExhaustive, msg))
    }

    /// Adds the cases `p` covers to `out`; true if it matches everything.
    fn covers(syms: &SymbolView<'_, '_>, p: &ast::Pat, out: &mut Vec<Case>) -> bool {
        match &p.kind {
            PatKind::Wild | PatKind::Bind { .. } => true,
            PatKind::Lit(e) => {
                match e.kind {
                    ExprKind::True => out.push(Case::True),
                    ExprKind::False => out.push(Case::False),
                    ExprKind::Nil => out.push(Case::Nil),
                    _ => {}
                }
                false
            }
            PatKind::Or(alts) => {
                let mut all = false;
                for a in alts {
                    all |= Self::covers(syms, a, out);
                }
                all
            }
            PatKind::Path(e) => {
                if let Some(id) = syms.path_symbol(e) {
                    if syms.kind(id) == Some(SymbolKind::Variant) {
                        out.push(Case::Variant(id));
                    }
                }
                false
            }
            PatKind::Struct { path, fields, .. } => {
                let irrefutable = fields.iter().all(|f| {
                    f.pat.as_ref().map_or(true, |p| {
                        matches!(p.kind, PatKind::Wild | PatKind::Bind { .. })
                    })
                });
                if irrefutable {
                    if let Some(id) = syms.path_symbol(path) {
                        if syms.kind(id) == Some(SymbolKind::Variant) {
                            out.push(Case::Variant(id));
                        }
                    }
                }
                false
            }
            PatKind::Range { .. } | PatKind::Tuple(_) => false,
        }
    }
}

impl Pass for Coverage {
    #[inline]
    fn node(&self, m: &Typed<'_, '_, '_>, n: Node<'_>, out: &mut Vec<Diagnostic>) {
        if let Node::Expr(e) = n {
            if let ExprKind::Match { scrutinee, arms } = &e.kind {
                if let Some(st) = m.type_of(scrutinee) {
                    out.extend(Self::check(m, st, arms, scrutinee.span));
                }
            }
        }
    }
}

/// `let scope x = v` needs a `v` with `__close` (or `nil`).
struct ScopeClose;

impl ScopeClose {
    fn check(m: &Typed<'_, '_, '_>, t: TypeId, span: Span) -> Option<Diagnostic> {
        let missing = m
            .types
            .members(t)
            .into_iter()
            .find(|&t| match m.types.get(t) {
                Ty::Nominal(id, _) | Ty::Variant(id, _) => {
                    m.syms.member_sym(m.syms.holder(*id), "__close").is_none()
                }
                Ty::Int | Ty::Float | Ty::Bool | Ty::Str | Ty::Fn(_) | Ty::Function => true,
                _ => false,
            })?;
        let msg = format!(
            "`let scope` needs a value with a `__close` method, but {} has none",
            m.show(missing)
        );
        Some(Diagnostic::error(m.file, span, Code::Mismatch, msg))
    }
}

impl Pass for ScopeClose {
    #[inline]
    fn node(&self, m: &Typed<'_, '_, '_>, n: Node<'_>, out: &mut Vec<Diagnostic>) {
        let Node::Stmt(s) = n else { return };
        let StmtKind::Let {
            scope: true,
            pat,
            value,
            ..
        } = &s.kind
        else {
            return;
        };
        // The binding's type, which an annotation gives; `let scope _ = v` has only `v`'s.
        let bound = match &pat.kind {
            PatKind::Bind { name, .. } => m.type_at(name.span),
            _ => None,
        };
        if let Some(t) = bound.or_else(|| value.as_ref().and_then(|v| m.type_of(v))) {
            out.extend(Self::check(m, t, pat.span));
        }
    }
}
