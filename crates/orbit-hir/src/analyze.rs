//! Shared analysis passes that run after a front end has lowered a chunk: capture analysis
//! (turns `Outer` references into per-function capture lists) and assignment analysis
//! (marks locals that are assigned after their declaration).

use std::fmt;

use crate::{Capture, CaptureFrom, CaptureId, Chunk, ExprKind, FuncId, LocalId, Place};

/// A front end produced a reference the passes can't resolve. This is a front-end bug, not a
/// user error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnalysisError {
    pub func: FuncId,
    pub message: String,
}

impl fmt::Display for AnalysisError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "HIR analysis, function {}: {}",
            self.func.0, self.message
        )
    }
}

impl std::error::Error for AnalysisError {}

/// Runs capture analysis and assignment analysis. Call it once, on a chunk straight from a
/// front end (its capture lists empty).
///
/// Afterwards:
/// - every `Outer` expression or place is a `Capture`;
/// - a function that reads or writes a global, or contains a function that does, has the
///   environment as capture 0 (`Env` in the main function, `ParentCapture(0)` below it);
/// - other captures are numbered in first-reference order (source order, counting uses in
///   nested functions), and `Function::env_debug_index` says where the environment's first
///   use falls among them;
/// - `Local::captured` and `Local::reassigned` are set;
/// - a vararg table that is only ever indexed for reading is elided: those reads are
///   `VarargIndex` and `Function::vararg_table_elided` is set.
pub fn analyze(chunk: &mut Chunk) -> Result<(), AnalysisError> {
    check_parents(chunk)?;
    add_env_captures(chunk);
    rewrite_outer(chunk)?;
    mark_reassigned(chunk);
    elide_vararg_tables(chunk);
    Ok(())
}

fn check_parents(chunk: &Chunk) -> Result<(), AnalysisError> {
    for (i, f) in chunk.functions.iter().enumerate() {
        let func = FuncId::from_index(i);
        let err = |message: &str| {
            Err(AnalysisError {
                func,
                message: message.into(),
            })
        };
        if !f.captures.is_empty() {
            return err("capture list is not empty before analysis");
        }
        match (i, f.parent) {
            (0, None) => {}
            (0, Some(_)) => return err("the main function has a parent"),
            (_, None) => return err("only the main function may have no parent"),
            (_, Some(p)) if p.index() >= chunk.functions.len() || p == func => {
                return err("parent out of range");
            }
            _ => {}
        }
        // The parent chain must reach the main function without looping.
        let mut cur = f.parent;
        let mut steps = 0;
        while let Some(p) = cur {
            steps += 1;
            if steps > chunk.functions.len() {
                return err("parent chain has a cycle");
            }
            cur = chunk.functions[p.index()].parent;
        }
    }
    Ok(())
}

fn uses_env(chunk: &Chunk, func: usize) -> bool {
    let f = &chunk.functions[func];
    f.exprs
        .iter()
        .any(|e| matches!(e.kind, ExprKind::Global(_) | ExprKind::Env))
        || f.stmts.iter().any(|s| {
            s.kind
                .places()
                .iter()
                .any(|t| matches!(t, Place::Global(_) | Place::Env))
        })
}

/// Gives capture 0 to every function that needs the environment, before any other capture,
/// so the VM's global instructions can always go through `U[0]`.
fn add_env_captures(chunk: &mut Chunk) {
    let n = chunk.functions.len();
    let mut needs = vec![false; n];
    for i in 0..n {
        if uses_env(chunk, i) {
            let mut cur = Some(FuncId::from_index(i));
            while let Some(f) = cur {
                if needs[f.index()] {
                    break;
                }
                needs[f.index()] = true;
                cur = chunk.functions[f.index()].parent;
            }
        }
    }
    let name = chunk.strings.intern(b"_ENV");
    for (i, f) in chunk.functions.iter_mut().enumerate() {
        if needs[i] {
            let from = if f.parent.is_none() {
                CaptureFrom::Env
            } else {
                CaptureFrom::ParentCapture(CaptureId(0))
            };
            f.captures.push(Capture { name, from });
        }
    }
}

enum Site {
    Expr(usize),
    Place {
        stmt: usize,
        target: usize,
    },
    /// A global read or write: a use of the environment.
    Global,
}

/// Rewrites `Outer` sites to captures. Sites are visited in source order
/// across all functions, the way a one-pass compiler meets them, so each
/// function's captures (after the environment) are numbered by first use in
/// its text, including uses inside nested functions.
fn rewrite_outer(chunk: &mut Chunk) -> Result<(), AnalysisError> {
    // (position, tiebreak, function, site, owner and local of an Outer site)
    let mut sites = Vec::new();
    for (g, f) in chunk.functions.iter().enumerate() {
        for (i, e) in f.exprs.iter().enumerate() {
            match e.kind {
                ExprKind::Outer { func, local } => {
                    sites.push((e.span.start, 0, g, Site::Expr(i), Some((func, local))));
                }
                ExprKind::Global(_) | ExprKind::Env => {
                    sites.push((e.span.start, 0, g, Site::Global, None))
                }
                _ => {}
            }
        }
        for (i, s) in f.stmts.iter().enumerate() {
            // Targets have no span of their own; they start the statement.
            for (t, place) in s.kind.places().iter().enumerate() {
                match *place {
                    Place::Outer { func, local } => sites.push((
                        s.span.start,
                        t,
                        g,
                        Site::Place { stmt: i, target: t },
                        Some((func, local)),
                    )),
                    Place::Global(_) | Place::Env => {
                        sites.push((s.span.start, t, g, Site::Global, None))
                    }
                    _ => {}
                }
            }
        }
    }
    // Sort packed keys, with each site's index last so equal keys keep their order: a sort of
    // `u128`s is one small routine shared with the compiler, where a stable sort of these
    // tuples would be a large one of its own.
    let mut order: Vec<u128> = sites
        .iter()
        .enumerate()
        .map(|(i, &(pos, tie, g, ..))| {
            (pos as u128) << 96 | (tie as u128) << 64 | (g as u128) << 32 | i as u128
        })
        .collect();
    order.sort_unstable();
    let mut sites: Vec<_> = sites.into_iter().map(Some).collect();
    let mut env_seen = vec![false; chunk.functions.len()];
    for key in order {
        let (_, _, g, site, outer) = sites[key as u32 as usize].take().expect("site");
        let gid = FuncId::from_index(g);
        let Some((owner, local)) = outer else {
            // The first use of the environment in `g` or any function it
            // encloses fixes its debug position in `g` and its ancestors.
            let mut cur = Some(gid);
            while let Some(f) = cur {
                if std::mem::replace(&mut env_seen[f.index()], true) {
                    break;
                }
                let f = &mut chunk.functions[f.index()];
                f.env_debug_index = Some((f.captures.len() - 1).min(u8::MAX as usize) as u8);
                cur = f.parent;
            }
            continue;
        };
        let cap = capture_for(chunk, gid, owner, local)?;
        let owner_local = &mut chunk.functions[owner.index()].locals[local.index()];
        owner_local.captured = true;
        if matches!(site, Site::Place { .. }) {
            owner_local.reassigned = true;
            owner_local.assigned_in_closure = true;
        }
        let f = &mut chunk.functions[g];
        match site {
            Site::Expr(i) => f.exprs[i].kind = ExprKind::Capture(cap),
            Site::Place { stmt, target } => {
                f.stmts[stmt].kind.places_mut()[target] = Place::Capture(cap);
            }
            Site::Global => {}
        }
    }
    Ok(())
}

/// The capture in `g` for local `local` of `owner`, adding captures along the parent chain.
fn capture_for(
    chunk: &mut Chunk,
    g: FuncId,
    owner: FuncId,
    local: LocalId,
) -> Result<CaptureId, AnalysisError> {
    let err = |message: String| AnalysisError { func: g, message };
    if owner.index() >= chunk.functions.len() {
        return Err(err(format!(
            "Outer names function {} which doesn't exist",
            owner.0
        )));
    }
    let Some(parent) = chunk.functions[g.index()].parent else {
        return Err(err(format!(
            "Outer names function {} which doesn't enclose it",
            owner.0
        )));
    };
    let from = if parent == owner {
        let locals = &chunk.functions[owner.index()].locals;
        if local.index() >= locals.len() {
            return Err(err(format!(
                "Outer names local {} of function {} which doesn't exist",
                local.0, owner.0
            )));
        }
        CaptureFrom::ParentLocal(local)
    } else {
        CaptureFrom::ParentCapture(capture_for(chunk, parent, owner, local)?)
    };
    let name = match chunk.functions[owner.index()].locals[local.index()].name {
        Some(name) => name,
        None => chunk.strings.intern(b"?"),
    };
    let f = &mut chunk.functions[g.index()];
    if let Some(i) = f.captures.iter().position(|c| c.from == from) {
        return Ok(CaptureId::from_index(i));
    }
    f.captures.push(Capture { name, from });
    Ok(CaptureId::from_index(f.captures.len() - 1))
}

/// When a function's vararg table isn't captured and every use of it is `t[k]` read, rewrites
/// those reads to `VarargIndex(k)` so the table is never built. Writes, `#t`, method calls and
/// passing `t` along all keep it.
fn elide_vararg_tables(chunk: &mut Chunk) {
    for f in &mut chunk.functions {
        let Some(t) = f.vararg_table else { continue };
        if f.locals[t.index()].captured {
            continue;
        }
        let is_t = |kind: &ExprKind| matches!(*kind, ExprKind::Local(l) if l == t);
        let uses = f.exprs.iter().filter(|e| is_t(&e.kind)).count();
        let reads: Vec<usize> = (0..f.exprs.len())
            .filter(|&i| match f.exprs[i].kind {
                ExprKind::Index { obj, .. } => is_t(&f.exprs[obj.index()].kind),
                _ => false,
            })
            .collect();
        if uses != reads.len() {
            continue;
        }
        for i in reads {
            let ExprKind::Index { obj, key } = f.exprs[i].kind else {
                unreachable!("collected above")
            };
            f.exprs[obj.index()].kind = ExprKind::Nil;
            f.exprs[i].kind = ExprKind::VarargIndex(key);
        }
        f.vararg_table_elided = true;
    }
}

fn mark_reassigned(chunk: &mut Chunk) {
    for f in &mut chunk.functions {
        let mut assigned = Vec::new();
        for s in &f.stmts {
            for t in s.kind.places() {
                if let Place::Local(l) = *t {
                    assigned.push(l);
                }
            }
        }
        for l in assigned {
            if let Some(local) = f.locals.get_mut(l.index()) {
                local.reassigned = true;
            }
        }
    }
}
