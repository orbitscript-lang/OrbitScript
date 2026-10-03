//! Structural checks on a chunk: ids in range, a tree shape, `break` only in loops, every
//! local declared exactly once, and captures consistent with the enclosing functions.
//! Run in debug builds and tests; a failure is a bug in a front end or a pass.

use std::fmt;

use crate::{
    BlockId, CaptureFrom, CaptureId, Chunk, ExprId, ExprKind, ExprList, FuncId, Function, LocalId,
    Place, StmtId, StmtKind,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifyError {
    pub func: FuncId,
    pub message: String,
}

impl fmt::Display for VerifyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "invalid HIR in function {}: {}",
            self.func.0, self.message
        )
    }
}

impl std::error::Error for VerifyError {}

/// Checks an analysed chunk (after [`crate::analyze`]): no `Outer` nodes may remain.
pub fn verify(chunk: &Chunk) -> Result<(), VerifyError> {
    verify_chunk(chunk, true)
}

/// Checks a chunk as a front end produced it, before analysis: `Outer` nodes are allowed and
/// capture lists must be empty.
pub fn verify_lowered(chunk: &Chunk) -> Result<(), VerifyError> {
    verify_chunk(chunk, false)
}

fn verify_chunk(chunk: &Chunk, analyzed: bool) -> Result<(), VerifyError> {
    if chunk.functions.is_empty() {
        return Err(VerifyError {
            func: FuncId(0),
            message: "chunk has no functions".into(),
        });
    }
    for i in 0..chunk.functions.len() {
        let func = FuncId::from_index(i);
        Checker::new(chunk, func, analyzed)
            .run()
            .map_err(|message| VerifyError { func, message })?;
    }
    Ok(())
}

type Check = Result<(), String>;

struct Checker<'a> {
    chunk: &'a Chunk,
    id: FuncId,
    f: &'a Function,
    analyzed: bool,
    seen_exprs: Vec<bool>,
    seen_stmts: Vec<bool>,
    seen_blocks: Vec<bool>,
    declared: Vec<bool>,
    loops: u32,
}

impl<'a> Checker<'a> {
    fn new(chunk: &'a Chunk, id: FuncId, analyzed: bool) -> Self {
        let f = chunk.function(id);
        Checker {
            chunk,
            id,
            f,
            analyzed,
            seen_exprs: vec![false; f.exprs.len()],
            seen_stmts: vec![false; f.stmts.len()],
            seen_blocks: vec![false; f.blocks.len()],
            declared: vec![false; f.locals.len()],
            loops: 0,
        }
    }

    fn run(mut self) -> Check {
        self.check_parent()?;
        self.check_captures()?;
        for &p in &self.f.params {
            self.declare(p)?;
        }
        self.block(self.f.body)?;
        if let Some(i) = self.declared.iter().position(|d| !d) {
            return Err(format!("local {i} is never declared"));
        }
        Ok(())
    }

    fn check_parent(&self) -> Check {
        let n = self.chunk.functions.len();
        match (self.id.0, self.f.parent) {
            (0, None) => Ok(()),
            (0, Some(_)) => Err("the main function has a parent".into()),
            (_, None) => Err("only the main function may have no parent".into()),
            (_, Some(p)) if p.index() >= n || p == self.id => Err("parent out of range".into()),
            _ => Ok(()),
        }
    }

    fn check_captures(&self) -> Check {
        if !self.analyzed {
            return if self.f.captures.is_empty() {
                Ok(())
            } else {
                Err("captures before analysis".into())
            };
        }
        let parent = self.f.parent.map(|p| self.chunk.function(p));
        for (i, c) in self.f.captures.iter().enumerate() {
            if c.name.index() >= self.chunk.strings.len() {
                return Err(format!("capture {i} has a bad name"));
            }
            match (c.from, parent) {
                (CaptureFrom::Env, None) if i == 0 => {}
                (CaptureFrom::Env, _) => {
                    return Err(format!(
                        "capture {i}: only the main function's capture 0 is Env"
                    ));
                }
                (CaptureFrom::ParentLocal(l), Some(p)) => match p.locals.get(l.index()) {
                    Some(local) if local.captured => {}
                    Some(_) => {
                        return Err(format!(
                            "capture {i}: parent local {} isn't marked captured",
                            l.0
                        ));
                    }
                    None => return Err(format!("capture {i}: parent local {} out of range", l.0)),
                },
                (CaptureFrom::ParentCapture(c), Some(p)) => {
                    if c.index() >= p.captures.len() {
                        return Err(format!("capture {i}: parent capture {} out of range", c.0));
                    }
                }
                (_, None) => {
                    return Err(format!(
                        "capture {i}: the main function can only capture Env"
                    ));
                }
            }
        }
        Ok(())
    }

    fn is_env(&self) -> bool {
        match self.f.captures.first() {
            Some(c) => match c.from {
                CaptureFrom::Env => true,
                CaptureFrom::ParentCapture(CaptureId(0)) => {
                    let mut cur = self.f.parent;
                    while let Some(p) = cur {
                        let pf = self.chunk.function(p);
                        match pf.captures.first().map(|c| c.from) {
                            Some(CaptureFrom::Env) => return true,
                            Some(CaptureFrom::ParentCapture(CaptureId(0))) => cur = pf.parent,
                            _ => return false,
                        }
                    }
                    false
                }
                CaptureFrom::ParentLocal(_) | CaptureFrom::ParentCapture(_) => false,
            },
            None => false,
        }
    }

    fn global(&self) -> Check {
        if self.analyzed && !self.is_env() {
            return Err("uses a global but capture 0 isn't the environment".into());
        }
        Ok(())
    }

    fn local(&self, l: LocalId) -> Check {
        if l.index() >= self.f.locals.len() {
            return Err(format!("local {} out of range", l.0));
        }
        Ok(())
    }

    fn declare(&mut self, l: LocalId) -> Check {
        self.local(l)?;
        if std::mem::replace(&mut self.declared[l.index()], true) {
            return Err(format!("local {} declared twice", l.0));
        }
        Ok(())
    }

    fn capture(&self, c: CaptureId) -> Check {
        if !self.analyzed {
            return Err("Capture before analysis".into());
        }
        if c.index() >= self.f.captures.len() {
            return Err(format!("capture {} out of range", c.0));
        }
        Ok(())
    }

    fn outer(&self, func: FuncId, local: LocalId) -> Check {
        if self.analyzed {
            return Err("Outer reference left after analysis".into());
        }
        let mut cur = self.f.parent;
        while let Some(p) = cur {
            if p == func {
                return match self.chunk.function(p).locals.get(local.index()) {
                    Some(_) => Ok(()),
                    None => Err(format!("Outer local {} out of range", local.0)),
                };
            }
            cur = self.chunk.function(p).parent;
        }
        Err(format!(
            "Outer names function {} which doesn't enclose this one",
            func.0
        ))
    }

    fn child(&self, func: FuncId) -> Check {
        match self.chunk.functions.get(func.index()) {
            Some(child) if child.parent == Some(self.id) => Ok(()),
            Some(_) => Err(format!("function {} is not a child of this one", func.0)),
            None => Err(format!("function {} out of range", func.0)),
        }
    }

    fn string(&self, s: crate::StrId) -> Check {
        if s.index() >= self.chunk.strings.len() {
            return Err(format!("string {} out of range", s.0));
        }
        Ok(())
    }

    fn block(&mut self, b: BlockId) -> Check {
        let Some(seen) = self.seen_blocks.get_mut(b.index()) else {
            return Err(format!("block {} out of range", b.0));
        };
        if std::mem::replace(seen, true) {
            return Err(format!("block {} used twice", b.0));
        }
        for &s in &self.f.blocks[b.index()].stmts {
            self.stmt(s)?;
        }
        Ok(())
    }

    fn loop_body(&mut self, b: BlockId) -> Check {
        self.loops += 1;
        let r = self.block(b);
        self.loops -= 1;
        r
    }

    fn list(&mut self, list: &ExprList) -> Check {
        list.items.iter().try_for_each(|&e| self.expr(e))
    }

    fn stmt(&mut self, s: StmtId) -> Check {
        let Some(seen) = self.seen_stmts.get_mut(s.index()) else {
            return Err(format!("statement {} out of range", s.0));
        };
        if std::mem::replace(seen, true) {
            return Err(format!("statement {} used twice", s.0));
        }
        let f = self.f;
        match &f.stmts[s.index()].kind {
            StmtKind::Let { locals, values } => {
                self.list(values)?;
                locals.iter().try_for_each(|&l| self.declare(l))?;
            }
            StmtKind::LocalFunction { local, func } => {
                self.declare(*local)?;
                self.child(*func)?;
            }
            StmtKind::Assign { targets, values } => {
                if targets.is_empty() {
                    return Err("assignment with no targets".into());
                }
                for t in targets {
                    match *t {
                        Place::Local(l) => self.local(l)?,
                        Place::Outer { func, local } => self.outer(func, local)?,
                        Place::Capture(c) => self.capture(c)?,
                        Place::Global(name) => {
                            self.string(name)?;
                            self.global()?;
                        }
                        Place::Index { obj, key } => {
                            self.expr(obj)?;
                            self.expr(key)?;
                        }
                    }
                }
                self.list(values)?;
            }
            StmtKind::Eval(e) => self.expr(*e)?,
            StmtKind::Block(b) => self.block(*b)?,
            StmtKind::If { arms, else_ } => {
                for &(cond, body) in arms {
                    self.expr(cond)?;
                    self.block(body)?;
                }
                if let Some(b) = else_ {
                    self.block(*b)?;
                }
            }
            StmtKind::While { cond, body } => {
                self.expr(*cond)?;
                self.loop_body(*body)?;
            }
            StmtKind::Repeat { body, until } => {
                self.loop_body(*body)?;
                self.expr(*until)?;
            }
            StmtKind::NumericFor {
                var,
                start,
                limit,
                step,
                body,
            } => {
                self.expr(*start)?;
                self.expr(*limit)?;
                if let Some(step) = step {
                    self.expr(*step)?;
                }
                self.declare(*var)?;
                self.loop_body(*body)?;
            }
            StmtKind::GenericFor {
                vars,
                iter,
                hidden,
                body,
            } => {
                self.list(iter)?;
                hidden.iter().try_for_each(|&l| self.declare(l))?;
                vars.iter().try_for_each(|&l| self.declare(l))?;
                if vars.is_empty() {
                    return Err("generic for with no variables".into());
                }
                self.loop_body(*body)?;
            }
            StmtKind::Break => {
                if self.loops == 0 {
                    return Err("break outside a loop".into());
                }
            }
            StmtKind::Return(values) => self.list(values)?,
        }
        Ok(())
    }

    fn expr(&mut self, e: ExprId) -> Check {
        let Some(seen) = self.seen_exprs.get_mut(e.index()) else {
            return Err(format!("expression {} out of range", e.0));
        };
        if std::mem::replace(seen, true) {
            return Err(format!("expression {} used twice", e.0));
        }
        let f = self.f;
        let kind = &f.exprs[e.index()].kind;
        match kind {
            ExprKind::Str(s) | ExprKind::MethodCall { name: s, .. } => self.string(*s)?,
            ExprKind::Global(s) => {
                self.string(*s)?;
                self.global()?;
            }
            ExprKind::Vararg if !f.is_vararg => {
                return Err("'...' in a function that isn't vararg".into());
            }
            ExprKind::Local(l) => self.local(*l)?,
            ExprKind::Outer { func, local } => self.outer(*func, *local)?,
            ExprKind::Capture(c) => self.capture(*c)?,
            ExprKind::Closure(func) => self.child(*func)?,
            _ => {}
        }
        let mut result = Ok(());
        kind.for_each_child(|c| {
            if result.is_ok() {
                result = self.expr(c);
            }
        });
        result
    }
}
