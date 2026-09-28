//! Prints an AST back to Lua source.
//!
//! The output parses back to the same tree (modulo spans and the spelling of
//! equivalent forms, such as `f"x"` versus `f("x")`), and `lua5.5` compiles it to the
//! same bytecode as the original, which is how the parser is tested against the
//! reference. Statements are separated by `;` so that none can run into the next, and
//! statements and function boundaries are printed on their original lines, so that
//! function line ranges survive the round trip too.

use crate::ast::*;

/// Printer settings.
#[derive(Debug, Clone, Default)]
pub struct PrintOptions {
    /// Rename every local to a name made unique by its [`LocalId`] (except `_ENV` and
    /// the implicit `self`). If name resolution is right, the program's behavior does
    /// not change; if it is wrong, some use now refers to a different variable.
    pub rename_locals: bool,
}

/// Prints `chunk` as Lua source.
pub fn print_chunk(chunk: &Chunk, options: &PrintOptions) -> Vec<u8> {
    let mut p = Printer {
        out: Vec::new(),
        chunk,
        options,
        upvals: vec![&chunk.body.upvalues],
        implicit_selves: Vec::new(),
        line: 1,
    };
    p.block(&chunk.body.block);
    p.out
}

struct Printer<'a> {
    out: Vec<u8>,
    chunk: &'a Chunk,
    options: &'a PrintOptions,
    /// Upvalue lists of the functions being printed, innermost last.
    upvals: Vec<&'a [Upvalue]>,
    implicit_selves: Vec<LocalId>,
    line: u32,
}

fn is_keyword(s: &str) -> bool {
    matches!(
        s,
        "and"
            | "break"
            | "do"
            | "else"
            | "elseif"
            | "end"
            | "false"
            | "for"
            | "function"
            | "goto"
            | "if"
            | "in"
            | "local"
            | "nil"
            | "not"
            | "or"
            | "repeat"
            | "return"
            | "then"
            | "true"
            | "until"
            | "while"
    )
}

fn is_identifier(s: &[u8]) -> bool {
    match s.split_first() {
        Some((c, rest)) => {
            (c.is_ascii_alphabetic() || *c == b'_')
                && rest.iter().all(|c| c.is_ascii_alphanumeric() || *c == b'_')
                && !is_keyword(std::str::from_utf8(s).unwrap_or(""))
        }
        None => false,
    }
}

impl<'a> Printer<'a> {
    fn w(&mut self, s: &str) {
        self.out.extend_from_slice(s.as_bytes());
    }

    fn nl(&mut self) {
        self.out.push(b' ');
    }

    /// Moves the output to `line` if it is behind.
    fn advance(&mut self, line: u32) {
        while self.line < line {
            self.out.push(b'\n');
            self.line += 1;
        }
    }

    fn local_name(&self, id: LocalId) -> String {
        let info = self.chunk.local(id);
        if !self.options.rename_locals || &*info.name == "_ENV" || self.is_implicit_self(id) {
            info.name.to_string()
        } else {
            format!("{}_{}", info.name, id.0)
        }
    }

    fn is_implicit_self(&self, id: LocalId) -> bool {
        self.implicit_selves.contains(&id)
    }

    /// The local an upvalue of the innermost function ultimately refers to.
    fn upvalue_local(&self, idx: u32) -> Option<LocalId> {
        let mut level = self.upvals.len() - 1;
        let mut idx = idx;
        loop {
            match self.upvals[level][idx as usize].source {
                UpvalSource::Local(id) => return Some(id),
                UpvalSource::ChunkEnv => return None,
                UpvalSource::Upvalue(i) => {
                    idx = i;
                    level -= 1;
                }
            }
        }
    }

    fn name_ref(&mut self, n: &NameRef) {
        let s = match n.binding {
            Binding::Local(id) | Binding::Const(id) => self.local_name(id),
            Binding::Upvalue(i) => match self.upvalue_local(i) {
                Some(id) => self.local_name(id),
                None => n.name.name.to_string(),
            },
            Binding::Global(_) => n.name.name.to_string(),
        };
        self.w(&s);
    }

    fn decl(&mut self, d: &LocalDecl) {
        let s = self.local_name(d.id);
        self.w(&s);
    }

    fn block(&mut self, b: &'a Block) {
        for s in &b.stats {
            self.nl();
            self.advance(s.span.line);
            self.stat(s);
            self.w(";");
        }
        if let Some(r) = &b.ret {
            self.nl();
            self.advance(r.span.line);
            self.w("return");
            if !r.values.is_empty() {
                self.w(" ");
                self.exprs(&r.values);
            }
            self.w(";");
        }
    }

    fn exprs(&mut self, es: &'a [Expr]) {
        for (i, e) in es.iter().enumerate() {
            if i > 0 {
                self.w(", ");
            }
            self.expr(e);
        }
    }

    fn attrib(&mut self, a: Option<Attrib>) {
        match a {
            Some(Attrib::Const) => self.w(" <const>"),
            Some(Attrib::Close) => self.w(" <close>"),
            None => {}
        }
    }

    fn stat(&mut self, s: &'a Stat) {
        match &s.kind {
            StatKind::Assign { targets, values } => {
                self.exprs(targets);
                self.w(" = ");
                self.exprs(values);
            }
            StatKind::Call(e) => self.expr(e),
            StatKind::Do(b) => {
                self.w("do");
                self.block(b);
                self.nl();
                self.w("end");
            }
            StatKind::While(w) => {
                let WhileStat { cond, body } = &**w;
                self.w("while ");
                self.expr(cond);
                self.w(" do");
                self.block(body);
                self.nl();
                self.w("end");
            }
            StatKind::Repeat(r) => {
                let RepeatStat { body, cond } = &**r;
                self.w("repeat");
                self.block(body);
                self.nl();
                self.w("until ");
                self.expr(cond);
            }
            StatKind::If(i) => {
                let IfStat {
                    clauses,
                    else_block,
                } = &**i;
                for (i, c) in clauses.iter().enumerate() {
                    if i > 0 {
                        self.nl();
                        self.w("else");
                    }
                    self.w("if ");
                    self.expr(&c.cond);
                    self.w(" then");
                    self.block(&c.block);
                }
                if let Some(b) = else_block {
                    self.nl();
                    self.w("else");
                    self.block(b);
                }
                self.nl();
                self.w("end");
            }
            StatKind::NumericFor(f) => {
                let NumericFor {
                    var,
                    start,
                    limit,
                    step,
                    body,
                } = &**f;
                self.w("for ");
                self.decl(var);
                self.w(" = ");
                self.expr(start);
                self.w(", ");
                self.expr(limit);
                if let Some(step) = step {
                    self.w(", ");
                    self.expr(step);
                }
                self.w(" do");
                self.block(body);
                self.nl();
                self.w("end");
            }
            StatKind::GenericFor(f) => {
                let GenericFor {
                    vars, exprs, body, ..
                } = &**f;
                self.w("for ");
                for (i, v) in vars.iter().enumerate() {
                    if i > 0 {
                        self.w(", ");
                    }
                    self.decl(v);
                }
                self.w(" in ");
                self.exprs(exprs);
                self.w(" do");
                self.block(body);
                self.nl();
                self.w("end");
            }
            StatKind::Function { name, body } => {
                self.w("function ");
                self.name_ref(&name.base);
                for f in &name.fields {
                    self.w(".");
                    self.w(&f.name);
                }
                if let Some(m) = &name.method {
                    self.w(":");
                    self.w(&m.name);
                }
                self.func_body(body);
            }
            StatKind::LocalFunction { var, body } => {
                self.w("local function ");
                self.decl(var);
                self.func_body(body);
            }
            StatKind::GlobalFunction { name, body, .. } => {
                self.w("global function ");
                self.w(&name.name);
                self.func_body(body);
            }
            StatKind::Local { vars, values } => {
                self.w("local ");
                for (i, v) in vars.iter().enumerate() {
                    if i > 0 {
                        self.w(", ");
                    }
                    self.decl(&v.decl);
                    self.attrib(v.attrib);
                }
                if !values.is_empty() {
                    self.w(" = ");
                    self.exprs(values);
                }
            }
            StatKind::Global { vars, values, .. } => {
                self.w("global ");
                for (i, v) in vars.iter().enumerate() {
                    if i > 0 {
                        self.w(", ");
                    }
                    self.w(&v.name.name);
                    if v.is_const {
                        self.w(" <const>");
                    }
                }
                if !values.is_empty() {
                    self.w(" = ");
                    self.exprs(values);
                }
            }
            StatKind::GlobalAll { is_const } => {
                self.w(if *is_const {
                    "global <const> *"
                } else {
                    "global *"
                });
            }
            StatKind::Label(n) => {
                self.w("::");
                self.w(&n.name);
                self.w("::");
            }
            StatKind::Goto(n) => {
                self.w("goto ");
                self.w(&n.name);
            }
            StatKind::Break => self.w("break"),
        }
    }

    fn func_body(&mut self, f: &'a FuncBody) {
        self.upvals.push(&f.upvalues);
        self.advance(f.line_defined);
        self.w("(");
        let params = if f.is_method {
            self.implicit_selves.push(f.params[0].id);
            &f.params[1..]
        } else {
            &f.params[..]
        };
        for (i, p) in params.iter().enumerate() {
            if i > 0 {
                self.w(", ");
            }
            self.decl(p);
        }
        if let Some(v) = &f.vararg {
            if !params.is_empty() {
                self.w(", ");
            }
            self.w("...");
            if let Some(n) = &v.name {
                self.decl(n);
            }
        }
        self.w(")");
        self.block(&f.block);
        self.nl();
        self.advance(f.last_line_defined);
        self.w("end");
        self.upvals.pop();
    }

    fn string(&mut self, s: &[u8]) {
        self.out.push(b'"');
        for &c in s {
            match c {
                b'"' => self.w("\\\""),
                b'\\' => self.w("\\\\"),
                b'\n' => self.w("\\n"),
                0x20..=0x7e => self.out.push(c),
                _ => self.w(&format!("\\{c:03}")),
            }
        }
        self.out.push(b'"');
    }

    fn expr(&mut self, e: &'a Expr) {
        match &e.kind {
            ExprKind::Nil => self.w("nil"),
            ExprKind::True => self.w("true"),
            ExprKind::False => self.w("false"),
            ExprKind::Int(i) => {
                if *i < 0 {
                    self.w(&format!("0x{:x}", *i as u64));
                } else {
                    self.w(&i.to_string());
                }
            }
            ExprKind::Float(f) => {
                if f.is_infinite() {
                    self.w("1e9999");
                } else {
                    // `{:?}` prints the shortest representation that round-trips, and
                    // always marks floats (`1.0`, `1e300`).
                    self.w(&format!("{f:?}"));
                }
            }
            ExprKind::Str(s) => self.string(s),
            ExprKind::Vararg => self.w("..."),
            ExprKind::Function(f) => {
                self.w("function");
                self.func_body(f);
            }
            ExprKind::Table(fields) => {
                self.w("{");
                for (i, f) in fields.iter().enumerate() {
                    if i > 0 {
                        self.w(", ");
                    }
                    match f {
                        Field::Positional(v) => self.expr(v),
                        Field::Named(n, v) => {
                            self.w(&n.name);
                            self.w(" = ");
                            self.expr(v);
                        }
                        Field::Keyed(k, v) => {
                            self.w("[");
                            self.expr(k);
                            self.w("] = ");
                            self.expr(v);
                        }
                    }
                }
                self.w("}");
            }
            ExprKind::Name(n) => self.name_ref(n),
            ExprKind::Index { obj, key } => {
                self.expr(obj);
                match &key.kind {
                    ExprKind::Str(s) if is_identifier(s) => {
                        self.w(".");
                        self.out.extend_from_slice(s);
                    }
                    _ => {
                        self.w("[");
                        self.expr(key);
                        self.w("]");
                    }
                }
            }
            ExprKind::Call(c) => {
                self.expr(&c.func);
                self.w("(");
                self.exprs(&c.args);
                self.w(")");
            }
            ExprKind::MethodCall(c) => {
                self.expr(&c.obj);
                self.w(":");
                self.w(&c.method.name);
                self.w("(");
                self.exprs(&c.args);
                self.w(")");
            }
            ExprKind::Paren(inner) => {
                self.w("(");
                self.expr(inner);
                self.w(")");
            }
            ExprKind::Unary { op, operand, .. } => {
                self.w(match op {
                    UnOp::Neg => "- ",
                    UnOp::Not => "not ",
                    UnOp::Len => "#",
                    UnOp::BNot => "~ ",
                });
                let parens = match &operand.kind {
                    ExprKind::Binary { op, .. } => op.priority().0 <= UNARY_PRIORITY,
                    _ => false,
                };
                self.maybe_parens(operand, parens);
            }
            ExprKind::Binary { op, lhs, rhs, .. } => {
                let (left, right) = op.priority();
                let lparens = match &lhs.kind {
                    ExprKind::Binary { op: lop, .. } => left > lop.priority().1,
                    ExprKind::Unary { .. } => left > UNARY_PRIORITY,
                    _ => false,
                };
                let rparens = match &rhs.kind {
                    ExprKind::Binary { op: rop, .. } => rop.priority().0 <= right,
                    _ => false,
                };
                self.maybe_parens(lhs, lparens);
                self.w(" ");
                self.w(binop_text(*op));
                self.w(" ");
                self.maybe_parens(rhs, rparens);
            }
        }
    }

    fn maybe_parens(&mut self, e: &'a Expr, parens: bool) {
        if parens {
            // Not a `Paren` node: these parentheses only group, and an operator
            // expression has a single result anyway.
            self.w("(");
            self.expr(e);
            self.w(")");
        } else {
            self.expr(e);
        }
    }
}

fn binop_text(op: BinOp) -> &'static str {
    use BinOp::*;
    match op {
        Add => "+",
        Sub => "-",
        Mul => "*",
        Mod => "%",
        Pow => "^",
        Div => "/",
        IDiv => "//",
        BAnd => "&",
        BOr => "|",
        BXor => "~",
        Shl => "<<",
        Shr => ">>",
        Concat => "..",
        Eq => "==",
        Lt => "<",
        Le => "<=",
        Ne => "~=",
        Gt => ">",
        Ge => ">=",
        And => "and",
        Or => "or",
    }
}
