//! Parser tests: tree shapes (as S-expressions), whole example programs, and error messages.

use orbit_orb_syntax::ast::*;
use orbit_orb_syntax::{StringPool, parse};

struct Dump<'a> {
    strings: &'a StringPool,
    out: String,
}

impl Dump<'_> {
    fn s(&self, sym: orbit_orb_syntax::StrId) -> String {
        self.strings.display(sym).into_owned()
    }

    fn block(&mut self, b: &Block) {
        self.out.push('{');
        for (i, s) in b.stmts.iter().enumerate() {
            if i > 0 {
                self.out.push(' ');
            }
            self.stmt(s);
        }
        if let Some(t) = &b.tail {
            if !b.stmts.is_empty() {
                self.out.push(' ');
            }
            self.out.push_str("tail:");
            self.expr(t);
        }
        self.out.push('}');
    }

    fn stmt(&mut self, s: &Stmt) {
        match &s.kind {
            StmtKind::Error => self.out.push_str("(error)"),
            StmtKind::Let {
                scope, pat, value, ..
            } => {
                self.out
                    .push_str(if *scope { "(let-scope " } else { "(let " });
                self.pat(pat);
                if let Some(v) = value {
                    self.out.push(' ');
                    self.expr(v);
                }
                self.out.push(')');
            }
            StmtKind::Expr(e) => {
                self.expr(e);
                self.out.push(';');
            }
            StmtKind::Assign { targets, value } => {
                self.out.push_str("(= (");
                self.list(targets);
                self.out.push_str(") ");
                self.expr(value);
                self.out.push(')');
            }
            StmtKind::CompoundAssign {
                op, target, value, ..
            } => {
                self.out.push_str(&format!("({}= ", op.as_str()));
                self.expr(target);
                self.out.push(' ');
                self.expr(value);
                self.out.push(')');
            }
            StmtKind::Scope(inner) => {
                self.out.push_str("(scope ");
                self.stmt(inner);
                self.out.push(')');
            }
            StmtKind::Item(item) => self.item(item),
        }
    }

    fn item(&mut self, item: &Item) {
        if item.is_pub {
            self.out.push_str("pub ");
        }
        match &item.kind {
            ItemKind::Error => self.out.push_str("(error)"),
            ItemKind::Fn(f) => {
                self.out.push_str(&format!("(fn {} ", self.s(f.name.sym)));
                self.sig(&f.sig);
                self.out.push(' ');
                match &f.body {
                    Some(b) => self.block(b),
                    None => self.out.push(';'),
                }
                self.out.push(')');
            }
            ItemKind::Struct(s) => {
                let fields: Vec<_> = s.fields.iter().map(|f| self.s(f.name.sym)).collect();
                self.out.push_str(&format!(
                    "(struct {} {})",
                    self.s(s.name.sym),
                    fields.join(" ")
                ));
            }
            ItemKind::Enum(e) => {
                self.out.push_str(&format!("(enum {}", self.s(e.name.sym)));
                for v in &e.variants {
                    self.out.push_str(&format!(" {}", self.s(v.name.sym)));
                    if let Some(fields) = &v.fields {
                        let names: Vec<_> = fields.iter().map(|f| self.s(f.name.sym)).collect();
                        self.out.push_str(&format!("{{{}}}", names.join(" ")));
                    }
                }
                self.out.push(')');
            }
            ItemKind::Impl(i) => {
                self.out
                    .push_str(&format!("(impl {}", self.s(i.target.sym)));
                for m in &i.members {
                    self.out.push(' ');
                    self.item(m);
                }
                self.out.push(')');
            }
            ItemKind::Use(u) => {
                let path: Vec<_> = u.path.iter().map(|n| self.s(n.sym)).collect();
                self.out.push_str(&format!("(use {}", path.join("::")));
                match &u.tail {
                    UseTail::Module { alias: Some(a) } => {
                        self.out.push_str(&format!(" as {}", self.s(a.sym)))
                    }
                    UseTail::Module { alias: None } => {}
                    UseTail::Members(m) => {
                        for (n, a) in m {
                            self.out.push_str(&format!(" {}", self.s(n.sym)));
                            if let Some(a) = a {
                                self.out.push_str(&format!("={}", self.s(a.sym)));
                            }
                        }
                    }
                }
                self.out.push(')');
            }
            ItemKind::Const { name, value, .. } => {
                self.out.push_str(&format!("(const {} ", self.s(name.sym)));
                match value {
                    Some(v) => self.expr(v),
                    None => self.out.push(';'),
                }
                self.out.push(')');
            }
            ItemKind::TypeAlias { name, .. } => {
                self.out.push_str(&format!("(type {})", self.s(name.sym)));
            }
            ItemKind::Global { name, value } => {
                self.out.push_str(&format!("(global {} ", self.s(name.sym)));
                self.expr(value);
                self.out.push(')');
            }
            ItemKind::Extern(names) => {
                let names: Vec<_> = names.iter().map(|n| self.s(n.sym)).collect();
                self.out.push_str(&format!("(extern {})", names.join(" ")));
            }
        }
    }

    fn sig(&mut self, sig: &FnSig) {
        let mut ps = Vec::new();
        if sig.self_param.is_some() {
            ps.push("self".to_string());
        }
        for p in &sig.params {
            let name = p.name.map_or("_".into(), |n| self.s(n.sym));
            ps.push(if p.mutable {
                format!("mut {name}")
            } else {
                name
            });
        }
        if let Some(r) = sig.rest {
            ps.push(format!("...{}", self.s(r.sym)));
        }
        self.out.push_str(&format!("({})", ps.join(" ")));
    }

    fn list(&mut self, es: &[Expr]) {
        for (i, e) in es.iter().enumerate() {
            if i > 0 {
                self.out.push(' ');
            }
            self.expr(e);
        }
    }

    fn label(&mut self, l: &Option<Name>) {
        if let Some(l) = l {
            self.out.push_str(&format!("'{} ", self.s(l.sym)));
        }
    }

    fn expr(&mut self, e: &Expr) {
        match &e.kind {
            ExprKind::Error => self.out.push_str("<error>"),
            ExprKind::Nil => self.out.push_str("nil"),
            ExprKind::True => self.out.push_str("true"),
            ExprKind::False => self.out.push_str("false"),
            ExprKind::Int(v) => self.out.push_str(&v.to_string()),
            ExprKind::Float(v) => self.out.push_str(&format!("{v:?}")),
            ExprKind::Str(s) => self.out.push_str(&format!("{:?}", self.s(*s))),
            ExprKind::Template(pieces) => {
                self.out.push_str("(f");
                for p in pieces {
                    self.out.push(' ');
                    match p {
                        TemplatePiece::Text(t) => self
                            .out
                            .push_str(&format!("{:?}", String::from_utf8_lossy(t))),
                        TemplatePiece::Hole { expr, spec } => {
                            self.out.push('{');
                            self.expr(expr);
                            if let Some(spec) = spec {
                                self.out
                                    .push_str(&format!(":{}", String::from_utf8_lossy(&spec.text)));
                            }
                            self.out.push('}');
                        }
                    }
                }
                self.out.push(')');
            }
            ExprKind::Name(s) => self.out.push_str(&self.s(*s)),
            ExprKind::SelfValue => self.out.push_str("self"),
            ExprKind::SelfType => self.out.push_str("Self"),
            ExprKind::Path { base, name } => {
                self.expr(base);
                self.out.push_str(&format!("::{}", self.s(name.sym)));
            }
            ExprKind::Field { obj, name } => {
                self.out.push_str("(. ");
                self.expr(obj);
                self.out.push_str(&format!(" {})", self.s(name.sym)));
            }
            ExprKind::Index { obj, key } => {
                self.out.push_str("([] ");
                self.expr(obj);
                self.out.push(' ');
                self.expr(key);
                self.out.push(')');
            }
            ExprKind::Call { callee, args, .. } => {
                self.out.push_str("(call ");
                self.expr(callee);
                for a in args {
                    self.out.push(' ');
                    self.expr(a);
                }
                self.out.push(')');
            }
            ExprKind::MethodCall {
                obj, name, args, ..
            } => {
                self.out.push_str(&format!("(method {} ", self.s(name.sym)));
                self.expr(obj);
                for a in args {
                    self.out.push(' ');
                    self.expr(a);
                }
                self.out.push(')');
            }
            ExprKind::Closure(c) => {
                self.out.push_str("(closure ");
                self.sig(&c.sig);
                self.out.push(' ');
                self.expr(&c.body);
                self.out.push(')');
            }
            ExprKind::Array(items) => {
                self.out.push('[');
                self.list(items);
                self.out.push(']');
            }
            ExprKind::Map(entries) => {
                self.out.push_str("#{");
                for (i, en) in entries.iter().enumerate() {
                    if i > 0 {
                        self.out.push(' ');
                    }
                    match en {
                        MapEntry::Positional(v) => self.expr(v),
                        MapEntry::Named(k, _, v) => {
                            self.out.push_str(&format!("{}:", self.s(*k)));
                            self.expr(v);
                        }
                        MapEntry::Keyed(k, v) => {
                            self.out.push('[');
                            self.expr(k);
                            self.out.push_str("]:");
                            self.expr(v);
                        }
                    }
                }
                self.out.push('}');
            }
            ExprKind::StructLit { path, fields } => {
                self.out.push_str("(new ");
                self.expr(path);
                for f in fields {
                    self.out.push_str(&format!(" {}", self.s(f.name.sym)));
                    if let Some(v) = &f.value {
                        self.out.push(':');
                        self.expr(v);
                    }
                }
                self.out.push(')');
            }
            ExprKind::Binary { op, lhs, rhs, .. } => {
                self.out.push_str(&format!("({} ", op.as_str()));
                self.expr(lhs);
                self.out.push(' ');
                self.expr(rhs);
                self.out.push(')');
            }
            ExprKind::Unary { op, expr } => {
                self.out.push_str(&format!("({} ", op.as_str()));
                self.expr(expr);
                self.out.push(')');
            }
            ExprKind::Paren(e) => {
                self.out.push_str("(paren ");
                self.expr(e);
                self.out.push(')');
            }
            ExprKind::As { expr, optional, .. } => {
                self.out.push_str(if *optional { "(as? " } else { "(as " });
                self.expr(expr);
                self.out.push(')');
            }
            ExprKind::Tuple(items) => {
                self.out.push_str("(tuple ");
                self.list(items);
                self.out.push(')');
            }
            ExprKind::Spread(e) => {
                self.out.push_str("...");
                self.expr(e);
            }
            ExprKind::Try(e) => {
                self.expr(e);
                self.out.push('?');
            }
            ExprKind::Range {
                start,
                end,
                inclusive,
            } => {
                self.out.push_str(if *inclusive { "(..= " } else { "(.. " });
                self.expr(start);
                self.out.push(' ');
                self.expr(end);
                self.out.push(')');
            }
            ExprKind::If { cond, then, else_ } => {
                self.out.push_str("(if ");
                self.expr(cond);
                self.out.push(' ');
                self.block(then);
                if let Some(e) = else_ {
                    self.out.push_str(" else ");
                    self.expr(e);
                }
                self.out.push(')');
            }
            ExprKind::Match { scrutinee, arms } => {
                self.out.push_str("(match ");
                self.expr(scrutinee);
                for arm in arms {
                    self.out.push_str(" [");
                    self.pat(&arm.pat);
                    if let Some(g) = &arm.guard {
                        self.out.push_str(" if ");
                        self.expr(g);
                    }
                    self.out.push_str(" => ");
                    self.expr(&arm.body);
                    self.out.push(']');
                }
                self.out.push(')');
            }
            ExprKind::Block(b) => self.block(b),
            ExprKind::Loop { label, body } => {
                self.out.push_str("(loop ");
                self.label(label);
                self.block(body);
                self.out.push(')');
            }
            ExprKind::While { label, cond, body } => {
                self.out.push_str("(while ");
                self.label(label);
                self.expr(cond);
                self.out.push(' ');
                self.block(body);
                self.out.push(')');
            }
            ExprKind::For {
                label,
                pat,
                iter,
                body,
            } => {
                self.out.push_str("(for ");
                self.label(label);
                self.pat(pat);
                self.out.push(' ');
                self.expr(iter);
                self.out.push(' ');
                self.block(body);
                self.out.push(')');
            }
            ExprKind::Break { label, value } => {
                self.out.push_str("(break");
                if label.is_some() || value.is_some() {
                    self.out.push(' ');
                }
                self.label(label);
                if let Some(v) = value {
                    self.expr(v);
                }
                self.out.push(')');
            }
            ExprKind::Continue { label } => {
                self.out.push_str("(continue");
                if let Some(l) = label {
                    self.out.push_str(&format!(" '{}", self.s(l.sym)));
                }
                self.out.push(')');
            }
            ExprKind::Return(v) => {
                self.out.push_str("(return");
                if let Some(v) = v {
                    self.out.push(' ');
                    self.expr(v);
                }
                self.out.push(')');
            }
        }
    }

    fn pat(&mut self, p: &Pat) {
        match &p.kind {
            PatKind::Wild => self.out.push('_'),
            PatKind::Bind { name, mutable } => {
                if *mutable {
                    self.out.push_str("mut ");
                }
                self.out.push_str(&self.s(name.sym));
            }
            PatKind::Lit(e) => self.expr(e),
            PatKind::Range {
                start,
                end,
                inclusive,
            } => {
                self.expr(start);
                self.out.push_str(if *inclusive { "..=" } else { ".." });
                self.expr(end);
            }
            PatKind::Or(alts) => {
                for (i, a) in alts.iter().enumerate() {
                    if i > 0 {
                        self.out.push_str(" | ");
                    }
                    self.pat(a);
                }
            }
            PatKind::Tuple(items) => {
                self.out.push('(');
                for (i, a) in items.iter().enumerate() {
                    if i > 0 {
                        self.out.push_str(", ");
                    }
                    self.pat(a);
                }
                self.out.push(')');
            }
            PatKind::Struct { path, fields, rest } => {
                self.expr(path);
                self.out.push('{');
                for (i, f) in fields.iter().enumerate() {
                    if i > 0 {
                        self.out.push(' ');
                    }
                    self.out.push_str(&self.s(f.name.sym));
                    if let Some(p) = &f.pat {
                        self.out.push(':');
                        self.pat(p);
                    }
                }
                if *rest {
                    self.out.push_str(" ..");
                }
                self.out.push('}');
            }
            PatKind::Path(e) => self.expr(e),
        }
    }
}

fn dump(src: &str) -> String {
    let ast = parse("t.orb", src.as_bytes()).unwrap_or_else(|e| panic!("{src:?}: {e}"));
    let mut d = Dump {
        strings: &ast.strings,
        out: String::new(),
    };
    d.block(&ast.body);
    d.out
}

fn err(src: &str) -> String {
    match parse("t.orb", src.as_bytes()) {
        Ok(_) => panic!("expected an error for {src:?}"),
        Err(d) => d.to_message("t.orb"),
    }
}

#[test]
fn precedence() {
    let cases = [
        ("a + b * c", "(+ a (* b c))"),
        ("a * b + c", "(+ (* a b) c)"),
        ("a - b - c", "(- (- a b) c)"),
        ("-2 ** 2", "(- (** 2 2))"),
        ("2 ** 3 ** 2", "(** 2 (** 3 2))"),
        ("2 ** -1", "(** 2 (- 1))"),
        ("a ~ b + c", "(+ (~ a b) c)"),
        ("a \\ b % c", "(% (\\ a b) c)"),
        ("a || b && c", "(|| a (&& b c))"),
        ("a == b || c != d", "(|| (== a b) (!= c d))"),
        ("a < b | c", "(< a (| b c))"),
        ("a & b ^ c | d", "(| (^ (& a b) c) d)"),
        ("a << 1 + 2", "(<< a (+ 1 2))"),
        ("!a && ~b", "(&& (! a) (~ b))"),
        ("#t + 1", "(+ (# t) 1)"),
        ("-x?", "(- x?)"),
        ("a.b.c(1)[2]", "([] (method c (. a b) 1) 2)"),
        ("math::sqrt(x)", "(call math::sqrt x)"),
        ("(obj.f)(1)", "(call (paren (. obj f)) 1)"),
        ("a::b::c", "a::b::c"),
        ("\"%d\".format(n)", "(method format \"%d\" n)"),
        ("f(...args, ...g())", "(call f ...args ...(call g))"),
    ];
    for (src, want) in cases {
        assert_eq!(dump(src), format!("{{tail:{want}}}"), "{src}");
    }
}

#[test]
fn statements() {
    let cases = [
        ("let x = 1;", "{(let x 1)}"),
        ("let mut x;", "{(let mut x)}"),
        (
            "let (q, r) = divmod(7, 2);",
            "{(let (q, r) (call divmod 7 2))}",
        ),
        ("let n: int = 3;", "{(let n 3)}"),
        ("x += 1;", "{(+= x 1)}"),
        ("a.b <<= 2;", "{(<<= (. a b) 2)}"),
        ("(a, b) = (b, a);", "{(= (a b) (tuple b a))}"),
        ("t[#t + 1] = v;", "{(= (([] t (+ (# t) 1))) v)}"),
        (
            "let scope f = io::open(p)?;",
            "{(let-scope f (call io::open p)?)}",
        ),
        ("scope f.close();", "{(scope (method close f);)}"),
        (
            "scope { b.flush(); b.close(); }",
            "{(scope {(method flush b); (method close b);};)}",
        ),
        // `scope` stays an ordinary name where it is used like one.
        (
            "let scope = 3; scope = 4; scope.x(); f(scope)",
            "{(let scope 3) (= (scope) 4) (method x scope); tail:(call f scope)}",
        ),
        (
            "if a { b() } else if c { d() } else { e() }",
            "{tail:(if a {tail:(call b)} else (if c {tail:(call d)} else {tail:(call e)}))}",
        ),
        ("if a { b(); } x", "{(if a {(call b);}); tail:x}"),
        (
            "let s = if x >= 0 { 1 } else { -1 };",
            "{(let s (if (>= x 0) {tail:1} else {tail:(- 1)}))}",
        ),
        (
            "loop { if done() { break; } }",
            "{tail:(loop {tail:(if (call done) {(break);})})}",
        ),
        (
            "let v = loop { break 42; };",
            "{(let v (loop {(break 42);}))}",
        ),
        ("for i in 1..=10 { }", "{tail:(for i (..= 1 10) {})}"),
        (
            "for i in (0..n).rev() { }",
            "{tail:(for i (method rev (paren (.. 0 n))) {})}",
        ),
        (
            "for (k, v) in pairs(t) { }",
            "{tail:(for (k, v) (call pairs t) {})}",
        ),
        (
            "'outer: for i in 1..=n { continue 'outer; break 'outer; }",
            "{tail:(for 'outer i (..= 1 n) {(continue 'outer); (break 'outer );})}",
        ),
        (
            "while i < n { i += 1; }",
            "{tail:(while (< i n) {(+= i 1)})}",
        ),
        ("return;", "{(return);}"),
        ("return (a, b);", "{(return (tuple a b));}"),
        ("{ let a = 1; a }", "{tail:{(let a 1) tail:a}}"),
    ];
    for (src, want) in cases {
        assert_eq!(dump(src), want, "{src}");
    }
}

#[test]
fn literals_and_closures() {
    let cases = [
        ("[1, 2, 3]", "[1 2 3]"),
        (
            "#{ name: \"orbit\", \"two words\": true, [k]: v, 10 }",
            "#{name:\"orbit\" two words:true [k]:v 10}",
        ),
        ("#{ [1, 2] }", "#{[1 2]}"),
        ("Point { x: 4, y }", "(new Point x:4 y)"),
        ("Shape::Circle { r: 1.0 }", "(new Shape::Circle r:1.0)"),
        ("Self { x }", "(new Self x)"),
        ("|x| x * x", "(closure (x) (* x x))"),
        ("|| 1", "(closure () 1)"),
        (
            "|x: int, mut y| -> int { x + y }",
            "(closure (x mut y) {tail:(+ x y)})",
        ),
        (
            "f\"{name} has {n:>4} items {{x}}\"",
            "(f {name} \" has \" {n:>4} \" items {x}\")",
        ),
        ("f\"{t[#{a: 1}.a]}\"", "(f {([] t (. #{a:1} a))})"),
        ("r#\"a\"b\"#", "\"a\\\"b\""),
    ];
    for (src, want) in cases {
        assert_eq!(dump(src), format!("{{tail:{want}}}"), "{src}");
    }
}

#[test]
fn casts() {
    let cases = [
        ("x as int", "(as x)"),
        ("x as? int", "(as? x)"),
        ("x as int?", "(as x)"),
        ("(x as int)?", "(paren (as x))?"),
        ("x as int | 1", "(| (as x) 1)"),
        ("x as? (int | string) + 1", "(+ (as? x) 1)"),
        ("-x as int", "(as (- x))"),
    ];
    for (src, want) in cases {
        assert_eq!(dump(src), format!("{{tail:{want}}}"), "{src}");
    }
}

#[test]
fn struct_literals_are_not_parsed_in_headers() {
    assert_eq!(dump("if x == y { z }"), "{tail:(if (== x y) {tail:z})}");
    assert_eq!(
        dump("match p { Point { x, .. } => x, _ => 0 }"),
        "{tail:(match p [Point{x ..} => x] [_ => 0])}"
    );
    assert_eq!(
        dump("if (P { x: 1 }).x == 1 { }"),
        "{tail:(if (== (. (paren (new P x:1)) x) 1) {})}"
    );
}

#[test]
fn patterns() {
    let src = "match n { 0 => \"none\", 1 | 2 => \"few\", 3..=9 => \"some\", x if x < 0 => \"neg\", -1 => a, Shape::Empty => b, Shape::Rect { w, h: 2 } => c, (a, _) => d, nil => e, _ => \"many\" }";
    assert_eq!(
        dump(src),
        "{tail:(match n [0 => \"none\"] [1 | 2 => \"few\"] [3..=9 => \"some\"] [x if (< x 0) => \"neg\"] [-1 => a] [Shape::Empty => b] [Shape::Rect{w h:2} => c] [(a, _) => d] [nil => e] [_ => \"many\"])}"
    );
    // A block arm needs no comma.
    assert_eq!(
        dump("match x { 1 => { a() } _ => b }"),
        "{tail:(match x [1 => {tail:(call a)}] [_ => b])}"
    );
}

#[test]
fn items() {
    let cases = [
        (
            "fn add(a: int, b: int) -> int { a + b }",
            "{(fn add (a b) {tail:(+ a b)})}",
        ),
        (
            "pub fn log(fmt: string, ...args) { }",
            "{pub (fn log (fmt ...args) {})}",
        ),
        (
            "fn first<T>(t: [T]) -> T? { t[1] }",
            "{(fn first (t) {tail:([] t 1)})}",
        ),
        (
            "pub struct Point { x: float, pub y: float }",
            "{pub (struct Point x y)}",
        ),
        ("struct Unit;", "{(struct Unit )}"),
        (
            "pub enum Shape { Circle { r: float }, Rect { w: float, h: float }, Empty }",
            "{pub (enum Shape Circle{r} Rect{w h} Empty)}",
        ),
        (
            "impl Point { pub fn new(x, y) -> Point { Point { x, y } } fn __add(self, o) { } const __mode = \"k\"; }",
            "{(impl Point pub (fn new (x y) {tail:(new Point x y)}) (fn __add (self o) {}) (const __mode \"k\"))}",
        ),
        ("use json;", "{(use json)}"),
        (
            "use app::util::{trim, split as sp};",
            "{(use app::util trim split=sp)}",
        ),
        ("use app::util as u;", "{(use app::util as u)}"),
        (
            "pub const VERSION = \"1.2\";",
            "{pub (const VERSION \"1.2\")}",
        ),
        ("global x = 1;", "{(global x 1)}"),
        ("extern HOST_API, OTHER;", "{(extern HOST_API OTHER)}"),
        ("let ages: #{[string]: int} = #{};", "{(let ages #{})}"),
        (
            "fn f(t: [Map<string, Vec<int>>], g: fn(int) -> bool) -> (int, string?) { }",
            "{(fn f (t g) {})}",
        ),
    ];
    for (src, want) in cases {
        assert_eq!(dump(src), want, "{src}");
    }
}

/// Every OrbitScript example in the language design (`docs/design/orbitscript-syntax/proposal.md`) parses.
#[test]
fn proposal_examples() {
    let src = include_str!("programs/proposal.orb");
    parse("proposal.orb", src.as_bytes())
        .unwrap_or_else(|e| panic!("{}", e.to_message("proposal.orb")));
}

#[test]
fn errors() {
    let cases = [
        ("let x = 1", "t.orb:1: expected `;`, found end of file"),
        (
            "a < b < c",
            "t.orb:1: comparison operators cannot be chained: `<` after `<`; use `&&`",
        ),
        (
            "let x = 'a';",
            "t.orb:1: single quotes start a loop label such as `'outer`; strings use `\"`, bytes `b'a'`",
        ),
        (
            "fn f() {\n  g()\n  h()\n}",
            "t.orb:3: expected `;`, found `h`",
        ),
        (
            "fn f() {",
            "t.orb:1: expected `}` to close the `{` at line 1, found end of file",
        ),
        (
            "impl Add for P { }",
            "t.orb:1: traits are not supported yet; write metamethods such as `fn __add` directly in `impl`",
        ),
        (
            "let scope mut f = g();",
            "t.orb:1: a `let scope` binding cannot be `mut`: it is closed with the value it was given",
        ),
        (
            "fn f<T: Show>(x: T) {}",
            "t.orb:1: generic parameters can't have bounds (there are no traits yet)",
        ),
        ("fn f(x: int);", "t.orb:1: expected `{`, found `;`"),
        ("const N: int;", "t.orb:1: expected `=`, found `;`"),
        ("let x = ();", "t.orb:1: `()` is not a value; use `nil`"),
        (
            "f(_)",
            "t.orb:1: `_` can only be used in patterns and parameters",
        ),
        (
            "1 + 2 = 3;",
            "t.orb:1: invalid left-hand side of assignment",
        ),
        (
            "struct P(f64);",
            "t.orb:1: tuple structs are not supported; name the fields: `struct P { x: float }`",
        ),
        (
            "let s = f\"{}\";",
            "t.orb:1: empty `{}` in template; write `{{}}` for literal braces",
        ),
        (
            "let s = f\"{a b}\";",
            "t.orb:1: expected `}` to end the template hole, found `b`",
        ),
        (
            "x = .5;",
            "t.orb:1: a number cannot start with `.`; write `0.` before it",
        ),
        (
            "fn f(...a, b) {}",
            "t.orb:1: the rest parameter `...name` must be the last parameter",
        ),
        (
            "let x = let y = 1;",
            "t.orb:1: `let` is a statement; it can't be used as an expression",
        ),
        (
            "scope let x = 1;",
            "t.orb:1: `scope` takes a statement to run at scope exit; to close a value, write `let scope name = value;`",
        ),
        (
            "'a: if x {}",
            "t.orb:1: expected `loop`, `while` or `for` after a label, found `if`",
        ),
        (
            "let match = 1;",
            "t.orb:1: expected a pattern, found `match`; write `r#match` to use `match` as a name",
        ),
        (
            "fn loop() {}",
            "t.orb:1: expected an identifier, found `loop`; write `r#loop` to use `loop` as a name",
        ),
        (
            "let p = P { match };",
            "t.orb:1: `match` is a keyword; write `r#match` to use it as a name",
        ),
        (
            "match p { P { match } => 1 }",
            "t.orb:1: `match` is a keyword; write `r#match` to use it as a name",
        ),
        (
            "use text::match;",
            "t.orb:1: `match` is a keyword; write `r#match` to use it as a name",
        ),
        (
            "use text::{find, match};",
            "t.orb:1: `match` is a keyword; write `r#match` to use it as a name",
        ),
        ("let r#self = 1;", "t.orb:1: `self` cannot be a raw name"),
    ];
    for (src, want) in cases {
        assert_eq!(err(src), want, "{src}");
    }
}

#[test]
fn deep_nesting_is_an_error_not_a_crash() {
    let src = "(".repeat(5000) + &")".repeat(5000);
    assert!(err(&src).contains("nested too deeply"));
}

/// Mutated programs give an error or a tree, never a panic.
#[test]
fn mutations_never_panic() {
    let base = include_bytes!("programs/proposal.orb");
    let alphabet = b"{}()[]#|&:;.,=<>!?\"'f r\\/*-+~^%_09xa\n";
    let mut seed: u64 = 0x2545_f491_4f6c_dd1d;
    let mut next = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    for _ in 0..3000 {
        let mut src = base.to_vec();
        for _ in 0..(1 + next() % 4) {
            let at = (next() % src.len() as u64) as usize;
            match next() % 3 {
                0 => {
                    src.remove(at);
                }
                1 => src.insert(at, alphabet[(next() % alphabet.len() as u64) as usize]),
                _ => src[at] = alphabet[(next() % alphabet.len() as u64) as usize],
            }
        }
        let _ = parse("m.orb", &src);
    }
}
