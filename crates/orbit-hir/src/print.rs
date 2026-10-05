//! A stable text dump of a chunk, for golden tests and `--dump-hir`.

use std::fmt::Write;

use crate::{
    BinOp, BlockId, CaptureFrom, Chunk, ExprId, ExprKind, ExprList, Function, FunctionName,
    LabelId, LocalId, LocalKind, Place, StmtId, StmtKind, TableEntry, UnOp,
};

/// Renders the chunk as indented text. The format is stable: changes to it are reviewed as
/// golden-file diffs.
pub fn print(chunk: &Chunk) -> String {
    let mut out = String::new();
    for i in 0..chunk.functions.len() {
        Printer {
            chunk,
            f: &chunk.functions[i],
            out: &mut out,
        }
        .function(i);
    }
    out
}

struct Printer<'a> {
    chunk: &'a Chunk,
    f: &'a Function,
    out: &'a mut String,
}

impl Printer<'_> {
    fn s(&self, id: crate::StrId) -> String {
        format!("{:?}", self.chunk.strings.display(id))
    }

    fn function(&mut self, index: usize) {
        let f = self.f;
        let name = match f.name {
            FunctionName::Main => "main".to_string(),
            FunctionName::Global(s) => format!("global {}", self.s(s)),
            FunctionName::Local(s) => format!("local {}", self.s(s)),
            FunctionName::Field(s) => format!("field {}", self.s(s)),
            FunctionName::Method(s) => format!("method {}", self.s(s)),
            FunctionName::Anonymous => "anonymous".to_string(),
        };
        let _ = write!(self.out, "fn{index} {name}");
        if let Some(p) = f.parent {
            let _ = write!(self.out, " parent=fn{}", p.0);
        }
        let mut params: Vec<_> = f.params.iter().map(|&p| self.local_name(p)).collect();
        match (f.is_vararg, f.vararg_table) {
            (false, _) => {}
            (true, None) => params.push("...".into()),
            (true, Some(t)) => params.push(format!(
                "...{}{}",
                self.local_name(t),
                if f.vararg_table_elided { " elided" } else { "" }
            )),
        }
        let _ = write!(self.out, " ({})", params.join(", "));
        let _ = writeln!(self.out, " @{}", self.chunk.source.line_of(f.span.start));
        for (i, l) in f.locals.iter().enumerate() {
            let kind = match l.kind {
                LocalKind::Normal => "",
                LocalKind::Param => " param",
                LocalKind::Const => " const",
                LocalKind::ForControl => " for",
                LocalKind::Hidden => " hidden",
            };
            let name = l.name.map(|n| self.s(n)).unwrap_or_else(|| "_".into());
            let _ = writeln!(
                self.out,
                "  local l{i} {name}{kind}{}{}",
                if l.captured { " captured" } else { "" },
                if l.reassigned { " reassigned" } else { "" }
            );
        }
        for (i, c) in f.captures.iter().enumerate() {
            let from = match c.from {
                CaptureFrom::Env => "env".to_string(),
                CaptureFrom::ParentLocal(l) => format!("parent l{}", l.0),
                CaptureFrom::ParentCapture(c) => format!("parent c{}", c.0),
            };
            let _ = writeln!(self.out, "  capture c{i} {} <- {from}", self.s(c.name));
        }
        self.block(f.body, 1);
    }

    fn local_name(&self, l: LocalId) -> String {
        match self.f.locals.get(l.index()).and_then(|l| l.name) {
            Some(n) => format!("{}#l{}", self.chunk.strings.display(n), l.0),
            None => format!("_#l{}", l.0),
        }
    }

    fn line(&mut self, depth: usize, text: &str) {
        for _ in 0..depth {
            self.out.push_str("  ");
        }
        self.out.push_str(text);
        self.out.push('\n');
    }

    fn block(&mut self, b: BlockId, depth: usize) {
        for &s in &self.f.blocks[b.index()].stmts {
            self.stmt(s, depth);
        }
    }

    fn list(&self, list: &ExprList) -> String {
        list.items
            .iter()
            .map(|&e| self.expr(e))
            .collect::<Vec<_>>()
            .join(", ")
    }

    fn locals(&self, locals: &[LocalId]) -> String {
        locals
            .iter()
            .map(|&l| self.local_name(l))
            .collect::<Vec<_>>()
            .join(", ")
    }

    fn label_name(&self, l: LabelId) -> String {
        match self.f.labels.get(l.index()) {
            Some(label) => format!("{}#L{}", self.s(label.name), l.0),
            None => format!("?#L{}", l.0),
        }
    }

    fn place(&self, p: &Place) -> String {
        match *p {
            Place::Local(l) => self.local_name(l),
            Place::Outer { func, local } => format!("outer(fn{}, l{})", func.0, local.0),
            Place::Capture(c) => format!("c{}", c.0),
            Place::Global(s) => format!("global {}", self.s(s)),
            Place::Env => "env".into(),
            Place::Index { obj, key } => format!("{}[{}]", self.expr(obj), self.expr(key)),
        }
    }

    fn stmt(&mut self, s: StmtId, depth: usize) {
        let f = self.f;
        match &f.stmts[s.index()].kind {
            StmtKind::Let { locals, values } => {
                let text = if values.is_empty() {
                    format!("let {}", self.locals(locals))
                } else {
                    format!("let {} = {}", self.locals(locals), self.list(values))
                };
                self.line(depth, &text);
            }
            StmtKind::LocalFunction { local, func } => {
                let text = format!("let function {} = fn{}", self.local_name(*local), func.0);
                self.line(depth, &text);
            }
            StmtKind::Assign { targets, values } => {
                let targets: Vec<_> = targets.iter().map(|t| self.place(t)).collect();
                let text = format!("{} = {}", targets.join(", "), self.list(values));
                self.line(depth, &text);
            }
            StmtKind::Define { targets, values } => {
                let targets: Vec<_> = targets.iter().map(|t| self.place(t)).collect();
                let text = format!("define {} = {}", targets.join(", "), self.list(values));
                self.line(depth, &text);
            }
            StmtKind::Eval(e) => {
                let text = self.expr(*e);
                self.line(depth, &text);
            }
            StmtKind::Goto(l) => {
                let text = format!("goto {}", self.label_name(*l));
                self.line(depth, &text);
            }
            StmtKind::Label(l) => {
                let text = format!("::{}::", self.label_name(*l));
                self.line(depth, &text);
            }
            StmtKind::Block(b) => {
                self.line(depth, "do");
                self.block(*b, depth + 1);
                self.line(depth, "end");
            }
            StmtKind::If { arms, else_ } => {
                for (i, &(cond, body)) in arms.iter().enumerate() {
                    let text = format!(
                        "{} {} then",
                        if i == 0 { "if" } else { "elseif" },
                        self.expr(cond)
                    );
                    self.line(depth, &text);
                    self.block(body, depth + 1);
                }
                if let Some(b) = else_ {
                    self.line(depth, "else");
                    self.block(*b, depth + 1);
                }
                self.line(depth, "end");
            }
            StmtKind::While { cond, body } => {
                let text = format!("while {} do", self.expr(*cond));
                self.line(depth, &text);
                self.block(*body, depth + 1);
                self.line(depth, "end");
            }
            StmtKind::Repeat { body, until } => {
                self.line(depth, "repeat");
                self.block(*body, depth + 1);
                let text = format!("until {}", self.expr(*until));
                self.line(depth, &text);
            }
            StmtKind::NumericFor {
                var,
                start,
                limit,
                step,
                body,
            } => {
                let mut text = format!(
                    "for {} = {}, {}",
                    self.local_name(*var),
                    self.expr(*start),
                    self.expr(*limit)
                );
                if let Some(step) = step {
                    let _ = write!(text, ", {}", self.expr(*step));
                }
                text.push_str(" do");
                self.line(depth, &text);
                self.block(*body, depth + 1);
                self.line(depth, "end");
            }
            StmtKind::GenericFor {
                vars,
                iter,
                hidden,
                body,
            } => {
                let text = format!(
                    "for {} in {} [{}] do",
                    self.locals(vars),
                    self.list(iter),
                    self.locals(hidden.as_slice())
                );
                self.line(depth, &text);
                self.block(*body, depth + 1);
                self.line(depth, "end");
            }
            StmtKind::Break => self.line(depth, "break"),
            StmtKind::Return(values) => {
                let text = if values.is_empty() {
                    "return".to_string()
                } else {
                    format!("return {}", self.list(values))
                };
                self.line(depth, &text);
            }
        }
    }

    fn expr(&self, e: ExprId) -> String {
        match &self.f.exprs[e.index()].kind {
            ExprKind::Nil => "nil".into(),
            ExprKind::Bool(b) => b.to_string(),
            ExprKind::Int(i) => i.to_string(),
            ExprKind::Float(x) => format!("{x:?}"),
            ExprKind::Str(s) => self.s(*s),
            ExprKind::Vararg => "...".into(),
            ExprKind::VarargIndex(key) => format!("...[{}]", self.expr(*key)),
            ExprKind::Local(l) => self.local_name(*l),
            ExprKind::Outer { func, local } => format!("outer(fn{}, l{})", func.0, local.0),
            ExprKind::Capture(c) => format!("c{}", c.0),
            ExprKind::Global(s) => format!("global {}", self.s(*s)),
            ExprKind::Env => "env".into(),
            ExprKind::Index { obj, key } => format!("{}[{}]", self.expr(*obj), self.expr(*key)),
            ExprKind::Call { callee, args } => {
                format!("call {}({})", self.expr(*callee), self.list(args))
            }
            ExprKind::MethodCall { obj, name, args } => {
                format!(
                    "call {}:{}({})",
                    self.expr(*obj),
                    self.chunk.strings.display(*name),
                    self.list(args)
                )
            }
            ExprKind::Closure(func) => format!("closure fn{}", func.0),
            ExprKind::Table(entries) => {
                let items: Vec<_> = entries
                    .iter()
                    .map(|entry| match *entry {
                        TableEntry::Positional(v) => self.expr(v),
                        TableEntry::Keyed(k, v) => format!("[{}] = {}", self.expr(k), self.expr(v)),
                    })
                    .collect();
                format!("{{{}}}", items.join(", "))
            }
            ExprKind::Binary { op, lhs, rhs } => {
                let op = match op {
                    BinOp::Add => "+",
                    BinOp::Sub => "-",
                    BinOp::Mul => "*",
                    BinOp::Div => "/",
                    BinOp::IDiv => "//",
                    BinOp::Mod => "%",
                    BinOp::Pow => "^",
                    BinOp::Concat => "..",
                    BinOp::Eq => "==",
                    BinOp::Ne => "~=",
                    BinOp::Lt => "<",
                    BinOp::Le => "<=",
                    BinOp::Gt => ">",
                    BinOp::Ge => ">=",
                    BinOp::BAnd => "&",
                    BinOp::BOr => "|",
                    BinOp::BXor => "~",
                    BinOp::Shl => "<<",
                    BinOp::Shr => ">>",
                };
                format!("({} {op} {})", self.expr(*lhs), self.expr(*rhs))
            }
            ExprKind::Unary { op, expr } => {
                let op = match op {
                    UnOp::Neg => "-",
                    UnOp::Not => "not ",
                    UnOp::Len => "#",
                    UnOp::BNot => "~",
                };
                format!("({op}{})", self.expr(*expr))
            }
            ExprKind::And { lhs, rhs } => format!("({} and {})", self.expr(*lhs), self.expr(*rhs)),
            ExprKind::Or { lhs, rhs } => format!("({} or {})", self.expr(*lhs), self.expr(*rhs)),
            ExprKind::First(e) => format!("first({})", self.expr(*e)),
        }
    }
}
