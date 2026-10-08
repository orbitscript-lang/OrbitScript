//! Prints an AST back as Lua source, with every operator application parenthesised.
//!
//! The output parses back to the same tree (apart from spans and `Paren` nodes the printer
//! adds), which makes it a readable AST dump and a way to check the parser against the
//! reference interpreter: the printed program must behave exactly like the original.

use crate::ast::*;
use orbit_hir::{StrId, StringPool};

/// Lua source for `ast`, fully parenthesised.
pub fn unparse(ast: &Ast) -> Vec<u8> {
    let mut p = Printer {
        strings: &ast.strings,
        out: Vec::new(),
        indent: 0,
    };
    p.block(&ast.chunk.body);
    p.out
}

struct Printer<'a> {
    strings: &'a StringPool,
    out: Vec<u8>,
    indent: usize,
}

impl Printer<'_> {
    fn w(&mut self, s: &str) {
        self.out.extend_from_slice(s.as_bytes());
    }

    fn name(&mut self, sym: StrId) {
        self.out.extend_from_slice(self.strings.get(sym));
    }

    fn line(&mut self) {
        self.out.push(b'\n');
        for _ in 0..self.indent {
            self.out.extend_from_slice(b"  ");
        }
    }

    fn block(&mut self, b: &Block) {
        self.indent += 1;
        for s in &b.stmts {
            self.line();
            self.stmt(s);
            self.w(";");
        }
        if let Some(ret) = &b.ret {
            self.line();
            self.w("return");
            if !ret.values.is_empty() {
                self.w(" ");
                self.exprs(&ret.values);
            }
            self.w(";");
        }
        self.indent -= 1;
        self.line();
    }

    fn attrib(&mut self, a: Option<Attrib>) {
        match a.map(|a| a.kind) {
            None => {}
            Some(AttribKind::Const) => self.w(" <const>"),
            Some(AttribKind::Close) => self.w(" <close>"),
        }
    }

    fn att_names(&mut self, prefix: Option<Attrib>, names: &[AttName], values: &[Expr]) {
        self.attrib(prefix);
        for (i, n) in names.iter().enumerate() {
            self.w(if i == 0 { " " } else { ", " });
            self.name(n.name.sym);
            self.attrib(n.attrib);
        }
        if !values.is_empty() {
            self.w(" = ");
            self.exprs(values);
        }
    }

    fn stmt(&mut self, s: &Stmt) {
        match &s.kind {
            StmtKind::Local {
                attrib,
                names,
                values,
            } => {
                self.w("local");
                self.att_names(*attrib, names, values);
            }
            StmtKind::Global {
                attrib,
                names,
                values,
            } => {
                self.w("global");
                self.att_names(*attrib, names, values);
            }
            StmtKind::GlobalAll { attrib } => {
                self.w("global");
                self.attrib(*attrib);
                self.w(" *");
            }
            StmtKind::Assign { targets, values } => {
                self.exprs(targets);
                self.w(" = ");
                self.exprs(values);
            }
            StmtKind::Call(e) => self.expr(e),
            StmtKind::Do(b) => {
                self.w("do");
                self.block(b);
                self.w("end");
            }
            StmtKind::While { cond, body } => {
                self.w("while ");
                self.expr(cond);
                self.w(" do");
                self.block(body);
                self.w("end");
            }
            StmtKind::Repeat { body, until } => {
                self.w("repeat");
                self.block(body);
                self.w("until ");
                self.expr(until);
            }
            StmtKind::If { arms, else_ } => {
                for (i, (cond, body)) in arms.iter().enumerate() {
                    self.w(if i == 0 { "if " } else { "elseif " });
                    self.expr(cond);
                    self.w(" then");
                    self.block(body);
                }
                if let Some(b) = else_ {
                    self.w("else");
                    self.block(b);
                }
                self.w("end");
            }
            StmtKind::NumericFor {
                var,
                start,
                limit,
                step,
                body,
                ..
            } => {
                self.w("for ");
                self.name(var.sym);
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
                self.w("end");
            }
            StmtKind::GenericFor { vars, exprs, body } => {
                self.w("for ");
                for (i, v) in vars.iter().enumerate() {
                    if i > 0 {
                        self.w(", ");
                    }
                    self.name(v.sym);
                }
                self.w(" in ");
                self.exprs(exprs);
                self.w(" do");
                self.block(body);
                self.w("end");
            }
            StmtKind::Function { path, func } => {
                self.w("function ");
                self.name(path.base.sym);
                for f in &path.fields {
                    self.w(".");
                    self.name(f.sym);
                }
                if let Some(m) = path.method {
                    self.w(":");
                    self.name(m.sym);
                }
                self.func_body(func);
            }
            StmtKind::LocalFunction { name, func } => {
                self.w("local function ");
                self.name(name.sym);
                self.func_body(func);
            }
            StmtKind::GlobalFunction { name, func } => {
                self.w("global function ");
                self.name(name.sym);
                self.func_body(func);
            }
            StmtKind::Break => self.w("break"),
            StmtKind::Goto(n) => {
                self.w("goto ");
                self.name(n.sym);
            }
            StmtKind::Label(n) => {
                self.w("::");
                self.name(n.sym);
                self.w("::");
            }
        }
    }

    fn func_body(&mut self, f: &FuncBody) {
        self.w("(");
        for (i, p) in f.params.iter().enumerate() {
            if i > 0 {
                self.w(", ");
            }
            self.name(p.sym);
        }
        if let Some(v) = f.vararg {
            if !f.params.is_empty() {
                self.w(", ");
            }
            self.w("...");
            if let Some(n) = v.name {
                self.name(n.sym);
            }
        }
        self.w(")");
        self.block(&f.body);
        self.w("end");
    }

    fn exprs(&mut self, es: &[Expr]) {
        for (i, e) in es.iter().enumerate() {
            if i > 0 {
                self.w(", ");
            }
            self.expr(e);
        }
    }

    fn string(&mut self, id: StrId) {
        self.out.push(b'"');
        for &c in self.strings.get(id) {
            match c {
                b'"' | b'\\' => {
                    self.out.push(b'\\');
                    self.out.push(c);
                }
                b' '..=b'~' => self.out.push(c),
                // Three digits, so a following digit can't extend the escape.
                _ => self.out.extend_from_slice(format!("\\{c:03}").as_bytes()),
            }
        }
        self.out.push(b'"');
    }

    fn expr(&mut self, e: &Expr) {
        match &e.kind {
            ExprKind::Nil => self.w("nil"),
            ExprKind::True => self.w("true"),
            ExprKind::False => self.w("false"),
            // A negative integer literal comes from a hexadecimal one that wrapped (decimal
            // literals are never negative), and only a hexadecimal literal reads back as it.
            ExprKind::Int(n) if *n < 0 => self.w(&format!("0x{:x}", *n as u64)),
            ExprKind::Int(n) => self.w(&n.to_string()),
            ExprKind::Float(x) if x.is_nan() => self.w("(0/0)"),
            ExprKind::Float(x) if x.is_infinite() => self.w("1e999"),
            // `{:?}` is the shortest text that reads back as the same float, and keeps `.0`.
            ExprKind::Float(x) => self.w(&format!("{x:?}")),
            ExprKind::Str(s) => self.string(*s),
            ExprKind::Vararg => self.w("..."),
            ExprKind::Name(n) => self.name(*n),
            ExprKind::Index { obj, key } => {
                self.expr(obj);
                self.w("[");
                self.expr(key);
                self.w("]");
            }
            ExprKind::Call { callee, args, .. } => {
                self.expr(callee);
                self.w("(");
                self.exprs(args);
                self.w(")");
            }
            ExprKind::Method {
                obj, name, args, ..
            } => {
                self.expr(obj);
                self.w(":");
                self.name(name.sym);
                self.w("(");
                self.exprs(args);
                self.w(")");
            }
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
                            self.name(n.sym);
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
            ExprKind::Binary { op, lhs, rhs, .. } => {
                self.w("(");
                self.expr(lhs);
                self.w(" ");
                self.w(op.as_str());
                self.w(" ");
                self.expr(rhs);
                self.w(")");
            }
            ExprKind::Unary { op, expr } => {
                self.w("(");
                self.w(op.as_str());
                self.w(" ");
                self.expr(expr);
                self.w(")");
            }
            // Operator applications carry their own parentheses.
            ExprKind::Paren(inner)
                if matches!(inner.kind, ExprKind::Binary { .. } | ExprKind::Unary { .. }) =>
            {
                self.expr(inner)
            }
            ExprKind::Paren(inner) => {
                self.w("(");
                self.expr(inner);
                self.w(")");
            }
        }
    }
}
