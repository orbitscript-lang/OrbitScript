//! Lowering: resolves Lua's scoping rules and builds the shared HIR.
//!
//! One walk over the AST builds one `hir::Function` per function body. Names resolve to a local
//! of the current function, a local of an enclosing function (`Outer`, which the HIR's capture
//! analysis turns into captures), or a global. This is also the subset gate: constructs outside
//! the first-light subset fail here with `<construct> is not supported yet`.

use orbit_hir as hir;
use orbit_hir::{BlockId, ExprId, FuncId, FunctionName, LocalId, LocalKind, Span, StrId};

use crate::ast::{self, Ast, AttribKind, ExprKind, Field, StmtKind};
use crate::diag::Diagnostic;

/// Lowers a parsed chunk to HIR, before the HIR's shared analysis (`orbit_hir::analyze`).
pub fn lower(ast: Ast) -> Result<hir::Chunk, Diagnostic> {
    let Ast {
        chunk,
        strings,
        source,
    } = ast;
    let mut lx = Lowerer {
        chunk: hir::Chunk {
            source,
            strings,
            functions: Vec::new(),
        },
        funcs: Vec::new(),
        sym_self: StrId(0),
        sym_env: StrId(0),
    };
    lx.sym_self = lx.chunk.strings.intern(b"self");
    lx.sym_env = lx.chunk.strings.intern(b"_ENV");
    let main = lx.begin_function(FunctionName::Main, chunk.span);
    lx.cur().f.is_vararg = true;
    let body = lx.block_contents(&chunk.body)?;
    lx.cur().f.body = body;
    lx.end_function(main);
    Ok(lx.chunk)
}

struct FnCtx {
    id: FuncId,
    f: hir::Function,
    /// Innermost last. Each scope lists the locals declared in it, in order.
    scopes: Vec<Vec<(StrId, LocalId)>>,
}

struct Lowerer {
    chunk: hir::Chunk,
    /// Functions being lowered, innermost last.
    funcs: Vec<FnCtx>,
    sym_self: StrId,
    sym_env: StrId,
}

type LResult<T> = Result<T, Diagnostic>;

/// Where a name refers to.
enum Resolved {
    Local(LocalId),
    Outer { func: FuncId, local: LocalId },
    Global,
}

impl Lowerer {
    // ---- functions, scopes and names ----------------------------------------------------------

    fn cur(&mut self) -> &mut FnCtx {
        self.funcs.last_mut().expect("function being lowered")
    }

    fn f(&mut self) -> &mut hir::Function {
        &mut self.cur().f
    }

    /// Reserves a function id (so children can name their parent) and starts building it.
    fn begin_function(&mut self, name: FunctionName, span: Span) -> FuncId {
        let parent = self.funcs.last().map(|c| c.id);
        let id = self
            .chunk
            .add_function(hir::Function::new(name, span, parent));
        let mut f = hir::Function::new(name, span, parent);
        // `Function::new` gives an empty body block 0; lowering allocates its own blocks.
        f.blocks.clear();
        self.funcs.push(FnCtx {
            id,
            f,
            scopes: vec![Vec::new()],
        });
        id
    }

    fn end_function(&mut self, id: FuncId) {
        let ctx = self.funcs.pop().expect("function being lowered");
        debug_assert_eq!(ctx.id, id);
        self.chunk.functions[id.index()] = ctx.f;
    }

    fn line_of(&self, span: Span) -> u32 {
        self.chunk.source.line_of(span.start)
    }

    fn error(&self, span: Span, message: impl Into<String>) -> Diagnostic {
        Diagnostic::new(span, self.line_of(span), message, None)
    }

    fn not_supported(&self, span: Span, what: &str) -> Diagnostic {
        self.error(span, format!("{what} not supported yet"))
    }

    fn name_text(&self, sym: StrId) -> String {
        String::from_utf8_lossy(self.chunk.strings.get(sym)).into_owned()
    }

    /// Rejects `_ENV`, which first light doesn't give its Lua meaning.
    fn check_name(&self, name: ast::Name) -> LResult<()> {
        if name.sym == self.sym_env {
            return Err(self.not_supported(name.span, "'_ENV' is"));
        }
        Ok(())
    }

    fn open_scope(&mut self) {
        self.cur().scopes.push(Vec::new());
    }

    fn close_scope(&mut self) {
        self.cur().scopes.pop();
    }

    fn new_local(&mut self, name: Option<StrId>, span: Span, kind: LocalKind) -> LocalId {
        self.f().add_local(hir::Local::new(name, span, kind))
    }

    /// Brings a local into scope under `name`.
    fn bind(&mut self, name: StrId, local: LocalId) {
        self.cur()
            .scopes
            .last_mut()
            .expect("scope")
            .push((name, local));
    }

    fn resolve(&self, sym: StrId) -> Resolved {
        for (depth, ctx) in self.funcs.iter().enumerate().rev() {
            let found = ctx
                .scopes
                .iter()
                .rev()
                .flat_map(|scope| scope.iter().rev())
                .find(|(s, _)| *s == sym)
                .map(|&(_, l)| l);
            if let Some(local) = found {
                return if depth == self.funcs.len() - 1 {
                    Resolved::Local(local)
                } else {
                    Resolved::Outer {
                        func: ctx.id,
                        local,
                    }
                };
            }
        }
        Resolved::Global
    }

    fn local_kind(&self, func: FuncId, local: LocalId) -> LocalKind {
        let ctx = self
            .funcs
            .iter()
            .find(|c| c.id == func)
            .expect("enclosing function");
        ctx.f.local(local).kind
    }

    /// The assignment target for a name, checking it isn't read-only.
    fn place_for_name(&mut self, name: ast::Name) -> LResult<hir::Place> {
        self.check_name(name)?;
        let current = self.cur().id;
        let (place, kind) = match self.resolve(name.sym) {
            Resolved::Local(l) => (hir::Place::Local(l), self.local_kind(current, l)),
            Resolved::Outer { func, local } => (
                hir::Place::Outer { func, local },
                self.local_kind(func, local),
            ),
            Resolved::Global => return Ok(hir::Place::Global(name.sym)),
        };
        if matches!(kind, LocalKind::Const | LocalKind::ForControl) {
            let message = format!(
                "attempt to assign to const variable '{}'",
                self.name_text(name.sym)
            );
            return Err(self.error(name.span, message));
        }
        Ok(place)
    }

    // ---- blocks and statements ----------------------------------------------------------------

    fn add_expr(&mut self, kind: hir::ExprKind, span: Span) -> ExprId {
        self.f().add_expr(kind, span)
    }

    fn add_stmt(&mut self, stmts: &mut Vec<hir::StmtId>, kind: hir::StmtKind, span: Span) {
        let id = self.f().add_stmt(kind, span);
        stmts.push(id);
    }

    /// A block in its own scope.
    fn block(&mut self, b: &ast::Block) -> LResult<BlockId> {
        self.open_scope();
        let block = self.block_contents(b);
        self.close_scope();
        block
    }

    /// A block's statements in the current scope.
    fn block_contents(&mut self, b: &ast::Block) -> LResult<BlockId> {
        let mut stmts = Vec::new();
        self.stmts_into(b, &mut stmts)?;
        Ok(self.f().add_block(hir::Block {
            stmts,
            span: b.span,
        }))
    }

    fn stmts_into(&mut self, b: &ast::Block, stmts: &mut Vec<hir::StmtId>) -> LResult<()> {
        for s in &b.stmts {
            self.stmt(s, stmts)?;
        }
        if let Some(ret) = &b.ret {
            let values = self.expr_list(&ret.values)?;
            self.add_stmt(stmts, hir::StmtKind::Return(values), ret.span);
        }
        Ok(())
    }

    fn stmt(&mut self, s: &ast::Stmt, stmts: &mut Vec<hir::StmtId>) -> LResult<()> {
        let kind = match &s.kind {
            StmtKind::Local {
                attrib,
                names,
                values,
            } => {
                let values = self.expr_list(values)?;
                let mut locals = Vec::with_capacity(names.len());
                let mut bindings = Vec::with_capacity(names.len());
                for n in names {
                    self.check_name(n.name)?;
                    let kind = match n.attrib.or(*attrib) {
                        None => LocalKind::Normal,
                        Some(a) if a.kind == AttribKind::Const => LocalKind::Const,
                        Some(a) => {
                            return Err(self.not_supported(a.span, "'<close>' variables are"));
                        }
                    };
                    let local = self.new_local(Some(n.name.sym), n.name.span, kind);
                    locals.push(local);
                    bindings.push((n.name.sym, local));
                }
                // The names come into scope after the whole statement.
                for (sym, local) in bindings {
                    self.bind(sym, local);
                }
                hir::StmtKind::Let { locals, values }
            }
            StmtKind::Assign { targets, values } => {
                let mut places = Vec::with_capacity(targets.len());
                for t in targets {
                    places.push(self.place(t)?);
                }
                let values = self.expr_list(values)?;
                hir::StmtKind::Assign {
                    targets: places,
                    values,
                }
            }
            StmtKind::Call(e) => hir::StmtKind::Eval(self.expr(e)?),
            StmtKind::Do(b) => hir::StmtKind::Block(self.block(b)?),
            StmtKind::While { cond, body } => {
                let cond = self.expr(cond)?;
                hir::StmtKind::While {
                    cond,
                    body: self.block(body)?,
                }
            }
            StmtKind::Repeat { body, until } => {
                // The condition sees the body's locals.
                self.open_scope();
                let result = self
                    .block_contents(body)
                    .and_then(|b| Ok((b, self.expr(until)?)));
                self.close_scope();
                let (body, until) = result?;
                hir::StmtKind::Repeat { body, until }
            }
            StmtKind::If { arms, else_ } => {
                let mut hir_arms = Vec::with_capacity(arms.len());
                for (cond, body) in arms {
                    let cond = self.expr(cond)?;
                    hir_arms.push((cond, self.block(body)?));
                }
                let else_ = match else_ {
                    Some(b) => Some(self.block(b)?),
                    None => None,
                };
                hir::StmtKind::If {
                    arms: hir_arms,
                    else_,
                }
            }
            StmtKind::NumericFor {
                var,
                start,
                limit,
                step,
                body,
            } => {
                let start = self.expr(start)?;
                let limit = self.expr(limit)?;
                let step = match step {
                    Some(e) => Some(self.expr(e)?),
                    None => None,
                };
                self.check_name(*var)?;
                self.open_scope();
                let local = self.new_local(Some(var.sym), var.span, LocalKind::ForControl);
                self.bind(var.sym, local);
                let body = self.block(body);
                self.close_scope();
                hir::StmtKind::NumericFor {
                    var: local,
                    start,
                    limit,
                    step,
                    body: body?,
                }
            }
            StmtKind::GenericFor { vars, exprs, body } => {
                let iter = self.expr_list(exprs)?;
                let hidden = [(); 3].map(|_| self.new_local(None, s.span, LocalKind::Hidden));
                self.open_scope();
                let mut locals = Vec::with_capacity(vars.len());
                let mut result = Ok(());
                for (i, v) in vars.iter().enumerate() {
                    if let Err(e) = self.check_name(*v) {
                        result = Err(e);
                        break;
                    }
                    // Only the first variable is read-only (5.5).
                    let kind = if i == 0 {
                        LocalKind::ForControl
                    } else {
                        LocalKind::Normal
                    };
                    let local = self.new_local(Some(v.sym), v.span, kind);
                    self.bind(v.sym, local);
                    locals.push(local);
                }
                let body = result.and_then(|()| self.block(body));
                self.close_scope();
                hir::StmtKind::GenericFor {
                    vars: locals,
                    iter,
                    hidden,
                    body: body?,
                }
            }
            StmtKind::Function { path, func } => {
                let mut target = if path.fields.is_empty() && path.method.is_none() {
                    None
                } else {
                    Some(self.name_expr(path.base)?)
                };
                let mut fields = path.fields.iter().chain(path.method.iter()).peekable();
                let mut last_key = None;
                while let Some(field) = fields.next() {
                    let key = self.add_expr(hir::ExprKind::Str(field.sym), field.span);
                    if fields.peek().is_none() {
                        last_key = Some(key);
                        break;
                    }
                    let obj = target.expect("object for a field");
                    let span = self.f().expr(obj).span.to(field.span);
                    target = Some(self.add_expr(hir::ExprKind::Index { obj, key }, span));
                }
                let (place, name) = match (target, last_key) {
                    (Some(obj), Some(key)) => {
                        let last = path.method.or(path.fields.last().copied()).expect("field");
                        let name = if path.method.is_some() {
                            FunctionName::Method(last.sym)
                        } else {
                            FunctionName::Field(last.sym)
                        };
                        (hir::Place::Index { obj, key }, name)
                    }
                    _ => {
                        let place = self.place_for_name(path.base)?;
                        let name = match place {
                            hir::Place::Global(_) => FunctionName::Global(path.base.sym),
                            _ => FunctionName::Local(path.base.sym),
                        };
                        (place, name)
                    }
                };
                let closure = self.function(func, name, path.method.is_some())?;
                let value = self.add_expr(hir::ExprKind::Closure(closure), func.span);
                hir::StmtKind::Assign {
                    targets: vec![place],
                    values: vec![value].into(),
                }
            }
            StmtKind::LocalFunction { name, func } => {
                self.check_name(*name)?;
                // In scope inside its own body, so it can recurse.
                let local = self.new_local(Some(name.sym), name.span, LocalKind::Normal);
                self.bind(name.sym, local);
                let func = self.function(func, FunctionName::Local(name.sym), false)?;
                hir::StmtKind::LocalFunction { local, func }
            }
            StmtKind::Break => hir::StmtKind::Break,
            StmtKind::Goto(_) => return Err(self.not_supported(s.span, "'goto' is")),
            StmtKind::Label(_) => return Err(self.not_supported(s.span, "labels are")),
            StmtKind::Global { .. }
            | StmtKind::GlobalFunction { .. }
            | StmtKind::GlobalAll { .. } => {
                return Err(self.not_supported(s.span, "'global' declarations are"));
            }
        };
        self.add_stmt(stmts, kind, s.span);
        Ok(())
    }

    fn place(&mut self, target: &ast::Expr) -> LResult<hir::Place> {
        match &target.kind {
            ExprKind::Name(sym) => self.place_for_name(ast::Name {
                sym: *sym,
                span: target.span,
            }),
            ExprKind::Index { obj, key } => {
                let obj = self.expr(obj)?;
                let key = self.expr(key)?;
                Ok(hir::Place::Index { obj, key })
            }
            // The parser only accepts names and indexing as targets.
            _ => unreachable!("assignment target is a name or an index"),
        }
    }

    /// Lowers a function body into a new HIR function and returns its id.
    fn function(
        &mut self,
        body: &ast::FuncBody,
        name: FunctionName,
        is_method: bool,
    ) -> LResult<FuncId> {
        let id = self.begin_function(name, body.span);
        let result = self.function_contents(body, is_method);
        self.end_function(id);
        result.map(|()| id)
    }

    fn function_contents(&mut self, body: &ast::FuncBody, is_method: bool) -> LResult<()> {
        if is_method {
            let sym = self.sym_self;
            let local = self.f().add_param(Some(sym), body.span);
            self.bind(sym, local);
        }
        for p in &body.params {
            self.check_name(*p)?;
            let local = self.f().add_param(Some(p.sym), p.span);
            self.bind(p.sym, local);
        }
        if let Some(v) = body.vararg {
            if v.name.is_some() {
                return Err(self.not_supported(v.span, "named varargs are"));
            }
            self.f().is_vararg = true;
        }
        let block = self.block_contents(&body.body)?;
        self.f().body = block;
        Ok(())
    }

    // ---- expressions --------------------------------------------------------------------------

    fn expr_list(&mut self, exprs: &[ast::Expr]) -> LResult<hir::ExprList> {
        let mut items = Vec::with_capacity(exprs.len());
        for e in exprs {
            items.push(self.expr(e)?);
        }
        Ok(hir::ExprList::new(items))
    }

    fn name_expr(&mut self, name: ast::Name) -> LResult<ExprId> {
        self.check_name(name)?;
        let kind = match self.resolve(name.sym) {
            Resolved::Local(l) => hir::ExprKind::Local(l),
            Resolved::Outer { func, local } => hir::ExprKind::Outer { func, local },
            Resolved::Global => hir::ExprKind::Global(name.sym),
        };
        Ok(self.add_expr(kind, name.span))
    }

    fn expr(&mut self, e: &ast::Expr) -> LResult<ExprId> {
        let span = e.span;
        let kind = match &e.kind {
            ExprKind::Nil => hir::ExprKind::Nil,
            ExprKind::True => hir::ExprKind::Bool(true),
            ExprKind::False => hir::ExprKind::Bool(false),
            ExprKind::Int(n) => hir::ExprKind::Int(*n),
            ExprKind::Float(x) => hir::ExprKind::Float(*x),
            ExprKind::Str(s) => hir::ExprKind::Str(*s),
            ExprKind::Vararg => hir::ExprKind::Vararg,
            ExprKind::Name(sym) => return self.name_expr(ast::Name { sym: *sym, span }),
            ExprKind::Index { obj, key } => {
                let obj = self.expr(obj)?;
                let key = self.expr(key)?;
                hir::ExprKind::Index { obj, key }
            }
            ExprKind::Call { callee, args } => {
                let callee = self.expr(callee)?;
                let args = self.expr_list(args)?;
                hir::ExprKind::Call { callee, args }
            }
            ExprKind::Method { obj, name, args } => {
                let obj = self.expr(obj)?;
                let args = self.expr_list(args)?;
                hir::ExprKind::MethodCall {
                    obj,
                    name: name.sym,
                    args,
                }
            }
            ExprKind::Function(body) => {
                hir::ExprKind::Closure(self.function(body, FunctionName::Anonymous, false)?)
            }
            ExprKind::Table(fields) => {
                let mut entries = Vec::with_capacity(fields.len());
                for field in fields {
                    entries.push(match field {
                        Field::Positional(v) => hir::TableEntry::Positional(self.expr(v)?),
                        Field::Named(n, v) => {
                            let key = self.add_expr(hir::ExprKind::Str(n.sym), n.span);
                            hir::TableEntry::Keyed(key, self.expr(v)?)
                        }
                        Field::Keyed(k, v) => {
                            let key = self.expr(k)?;
                            hir::TableEntry::Keyed(key, self.expr(v)?)
                        }
                    });
                }
                hir::ExprKind::Table(entries)
            }
            ExprKind::Binary { op, lhs, rhs } => {
                let lhs = self.expr(lhs)?;
                let rhs = self.expr(rhs)?;
                use ast::BinOp as A;
                use hir::BinOp as H;
                let op = match op {
                    A::And => return Ok(self.add_expr(hir::ExprKind::And { lhs, rhs }, span)),
                    A::Or => return Ok(self.add_expr(hir::ExprKind::Or { lhs, rhs }, span)),
                    A::Add => H::Add,
                    A::Sub => H::Sub,
                    A::Mul => H::Mul,
                    A::Div => H::Div,
                    A::IDiv => H::IDiv,
                    A::Mod => H::Mod,
                    A::Pow => H::Pow,
                    A::Concat => H::Concat,
                    A::Eq => H::Eq,
                    A::Ne => H::Ne,
                    A::Lt => H::Lt,
                    A::Le => H::Le,
                    A::Gt => H::Gt,
                    A::Ge => H::Ge,
                    A::BAnd => H::BAnd,
                    A::BOr => H::BOr,
                    A::BXor => H::BXor,
                    A::Shl => H::Shl,
                    A::Shr => H::Shr,
                };
                hir::ExprKind::Binary { op, lhs, rhs }
            }
            ExprKind::Unary { op, expr } => {
                let op = match op {
                    ast::UnOp::Neg => hir::UnOp::Neg,
                    ast::UnOp::Not => hir::UnOp::Not,
                    ast::UnOp::Len => hir::UnOp::Len,
                    ast::UnOp::BNot => hir::UnOp::BNot,
                };
                let expr = self.expr(expr)?;
                hir::ExprKind::Unary { op, expr }
            }
            ExprKind::Paren(inner) => {
                let multi = matches!(
                    inner.kind,
                    ExprKind::Call { .. } | ExprKind::Method { .. } | ExprKind::Vararg
                );
                let inner = self.expr(inner)?;
                if !multi {
                    return Ok(inner);
                }
                // `(f())` is exactly one value.
                hir::ExprKind::First(inner)
            }
        };
        Ok(self.add_expr(kind, span))
    }
}
