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
/// - other captures are numbered in first-reference order;
/// - `Local::captured` and `Local::reassigned` are set.
pub fn analyze(chunk: &mut Chunk) -> Result<(), AnalysisError> {
    check_parents(chunk)?;
    add_env_captures(chunk);
    rewrite_outer(chunk)?;
    mark_reassigned(chunk);
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
    Place { stmt: usize, target: usize },
}

fn rewrite_outer(chunk: &mut Chunk) -> Result<(), AnalysisError> {
    for g in 0..chunk.functions.len() {
        let mut sites = Vec::new();
        {
            let f = &chunk.functions[g];
            for (i, e) in f.exprs.iter().enumerate() {
                if let ExprKind::Outer { func, local } = e.kind {
                    sites.push((Site::Expr(i), func, local));
                }
            }
            for (i, s) in f.stmts.iter().enumerate() {
                for (t, place) in s.kind.places().iter().enumerate() {
                    if let Place::Outer { func, local } = *place {
                        sites.push((Site::Place { stmt: i, target: t }, func, local));
                    }
                }
            }
        }
        let gid = FuncId::from_index(g);
        for (site, owner, local) in sites {
            let cap = capture_for(chunk, gid, owner, local)?;
            let owner_local = &mut chunk.functions[owner.index()].locals[local.index()];
            owner_local.captured = true;
            if matches!(site, Site::Place { .. }) {
                owner_local.reassigned = true;
            }
            let f = &mut chunk.functions[g];
            match site {
                Site::Expr(i) => f.exprs[i].kind = ExprKind::Capture(cap),
                Site::Place { stmt, target } => {
                    f.stmts[stmt].kind.places_mut()[target] = Place::Capture(cap);
                }
            }
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
