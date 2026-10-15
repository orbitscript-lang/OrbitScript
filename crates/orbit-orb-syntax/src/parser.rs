//! Recursive-descent parser for items and statements, precedence climbing for expressions.
//!
//! Parsing is purely syntactic: names stay unresolved until lowering. The grammar follows the
//! locked proposal (`planning/orbitscript-syntax/proposal.md`) and, where it is silent, Rust:
//! struct literals are not allowed in `if`/`while`/`match`/`for` headers, a block-like
//! expression at the start of a statement ends the statement, comparisons don't chain.

use crate::ast::*;
use crate::diag::Diagnostic;
use crate::lexer::{Lexer, TemplatePart};
use crate::token::{Keyword, Punct, Token, TokenKind};
use orbit_hir::{Span, StrId};

/// Parser settings.
#[derive(Clone, Debug)]
pub struct ParseOptions {
    /// Maximum nesting of blocks and expressions, so deeply nested input can't overflow the
    /// Rust stack.
    pub max_depth: u32,
}

impl Default for ParseOptions {
    fn default() -> ParseOptions {
        ParseOptions { max_depth: 200 }
    }
}

/// Parses one module. `chunk_name` is the name messages show, for example `main.orb`.
pub fn parse(chunk_name: &str, src: &[u8]) -> Result<Ast, Diagnostic> {
    parse_with(chunk_name, src, &ParseOptions::default())
}

pub fn parse_with(chunk_name: &str, src: &[u8], options: &ParseOptions) -> Result<Ast, Diagnostic> {
    if u32::try_from(src.len()).is_err() {
        return Err(Diagnostic::new(Span::default(), 1, "chunk too large"));
    }
    let mut lx = Lexer::new(chunk_name, src);
    let sym_scope = lx.strings_mut().intern(b"scope");
    let sym_underscore = lx.strings_mut().intern(b"_");
    let mut p = Parser {
        lx,
        tok: Token {
            kind: TokenKind::Eof,
            span: Span::default(),
        },
        ahead: None,
        prev_end: 0,
        depth: 0,
        max_depth: options.max_depth,
        sym_scope,
        sym_underscore,
    };
    p.advance()?;
    let start = p.tok.span.start;
    let (stmts, tail) = p.block_body(None)?;
    let body = Block {
        stmts,
        tail,
        span: Span::new(start.min(p.tok.span.end), src.len() as u32),
    };
    let (strings, source) = p.lx.finish();
    Ok(Ast {
        body,
        strings,
        source,
    })
}

/// Where an expression appears, for Rust's struct-literal restriction.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Ctx {
    Normal,
    /// An `if`, `while`, `match` or `for` header: `Name {` starts the body, not a literal.
    NoStruct,
}

struct Parser<'a> {
    lx: Lexer<'a>,
    tok: Token,
    ahead: Option<Token>,
    /// End of the last consumed token.
    prev_end: u32,
    depth: u32,
    max_depth: u32,
    sym_scope: StrId,
    sym_underscore: StrId,
}

type PResult<T> = Result<T, Diagnostic>;

impl Parser<'_> {
    // ---- tokens -------------------------------------------------------------------------------

    // Token helpers are kept out of line: they are called from hundreds of places, and
    // inlined they cost binary size for no measurable parse speed (see the design doc,
    // "Binary size").
    #[inline(never)]
    fn advance(&mut self) -> PResult<()> {
        self.prev_end = self.tok.span.end;
        self.tok = match self.ahead.take() {
            Some(t) => t,
            None => self.lx.next_token()?,
        };
        Ok(())
    }

    #[inline(never)]
    fn peek(&mut self) -> PResult<TokenKind> {
        if self.ahead.is_none() {
            self.ahead = Some(self.lx.next_token()?);
        }
        Ok(self.ahead.expect("lookahead").kind)
    }

    fn is(&self, p: Punct) -> bool {
        self.tok.kind == TokenKind::Punct(p)
    }

    fn is_kw(&self, k: Keyword) -> bool {
        self.tok.kind == TokenKind::Keyword(k)
    }

    #[inline(never)]
    fn eat(&mut self, p: Punct) -> PResult<bool> {
        if self.is(p) {
            self.advance()?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    #[inline(never)]
    fn eat_kw(&mut self, k: Keyword) -> PResult<bool> {
        if self.is_kw(k) {
            self.advance()?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    #[inline(never)]
    fn expect(&mut self, p: Punct) -> PResult<Span> {
        if self.is(p) {
            let span = self.tok.span;
            self.advance()?;
            Ok(span)
        } else {
            Err(self.expected(&format!("`{}`", p.as_str())))
        }
    }

    /// Expects `>`, splitting a `>>` (as in `Map<K, Vec<V>>`): the second `>` stays as the
    /// current token.
    #[inline(never)]
    fn expect_gt(&mut self) -> PResult<()> {
        if self.is(Punct::Shr) {
            self.tok.kind = TokenKind::Punct(Punct::Gt);
            self.tok.span.start += 1;
            self.prev_end = self.tok.span.start;
            return Ok(());
        }
        self.expect(Punct::Gt).map(|_| ())
    }

    fn describe(&self, kind: TokenKind) -> String {
        match kind {
            TokenKind::Ident(s) => format!("`{}`", self.lx.strings().display(s)),
            TokenKind::Int(_) | TokenKind::Float(_) => "a number".into(),
            TokenKind::Str(_) | TokenKind::Template(_) => "a string".into(),
            TokenKind::Label(s) => format!("label `'{}`", self.lx.strings().display(s)),
            TokenKind::Keyword(k) => format!("`{}`", k.as_str()),
            TokenKind::Punct(p) => format!("`{}`", p.as_str()),
            TokenKind::Eof => "end of file".into(),
        }
    }

    #[inline(never)]
    fn error(&self, span: Span, message: impl Into<String>) -> Diagnostic {
        Diagnostic::new(span, self.lx.line_of(span.start), message)
    }

    #[inline(never)]
    fn expected(&self, what: &str) -> Diagnostic {
        self.error(
            self.tok.span,
            format!("expected {what}, found {}", self.describe(self.tok.kind)),
        )
    }

    fn span_from(&self, start: u32) -> Span {
        Span::new(start, self.prev_end.max(start))
    }

    #[inline(never)]
    fn enter(&mut self) -> PResult<()> {
        self.depth += 1;
        if self.depth > self.max_depth {
            return Err(self.error(self.tok.span, "expression or block nested too deeply"));
        }
        Ok(())
    }

    fn leave(&mut self) {
        self.depth -= 1;
    }

    #[inline(never)]
    fn name(&mut self) -> PResult<Name> {
        match self.tok.kind {
            TokenKind::Ident(sym) => {
                let span = self.tok.span;
                self.advance()?;
                Ok(Name { sym, span })
            }
            TokenKind::Keyword(k) if !matches!(k, Keyword::SelfValue | Keyword::SelfType) => {
                let mut d = self.expected("an identifier");
                d.message += &format!("; write `r#{0}` to use `{0}` as a name", k.as_str());
                Err(d)
            }
            _ => Err(self.expected("an identifier")),
        }
    }

    /// A name in a place where only a name can appear: a field or method after `.`, a path
    /// segment after `::`, a field name, or a method in `impl`. Keywords are names here
    /// (`string::match`, `t.loop`), since nothing else could be meant.
    #[inline(never)]
    fn member_name(&mut self) -> PResult<Name> {
        match self.tok.kind {
            TokenKind::Keyword(k) => {
                let sym = self.lx.strings_mut().intern(k.as_str().as_bytes());
                let span = self.tok.span;
                self.advance()?;
                Ok(Name { sym, span })
            }
            _ => self.name(),
        }
    }

    /// The error for a keyword written as a member name in a place that would also bind or
    /// read a local of that name (`P { match }`, `use m::match;`), where it needs `r#`.
    fn keyword_binding(&self, name: Name) -> Diagnostic {
        let text = self.lx.strings().display(name.sym).to_string();
        self.error(
            name.span,
            format!("`{text}` is a keyword; write `r#{text}` to use it as a name"),
        )
    }

    fn is_ident(&self, sym: StrId) -> bool {
        self.tok.kind == TokenKind::Ident(sym)
    }

    /// The contextual keyword `scope`, as opposed to an ordinary name (`r#scope` included).
    fn is_scope(&self) -> bool {
        self.is_ident(self.sym_scope) && self.tok.span.end - self.tok.span.start == 5
    }

    // ---- blocks and statements ----------------------------------------------------------------

    /// `{ ... }`.
    fn block(&mut self) -> PResult<Block> {
        self.enter()?;
        let start = self.expect(Punct::LBrace)?.start;
        let (stmts, tail) = self.block_body(Some(start))?;
        self.expect(Punct::RBrace)?;
        self.leave();
        Ok(Block {
            stmts,
            tail,
            span: self.span_from(start),
        })
    }

    /// Statements up to `}` (when `open` is the `{`) or the end of the module.
    fn block_body(&mut self, open: Option<u32>) -> PResult<(Vec<Stmt>, Option<Box<Expr>>)> {
        let mut stmts = Vec::new();
        loop {
            match self.tok.kind {
                TokenKind::Punct(Punct::RBrace) if open.is_some() => return Ok((stmts, None)),
                TokenKind::Eof => {
                    if let Some(open) = open {
                        return Err(self.error(
                            self.tok.span,
                            format!(
                                "expected `}}` to close the `{{` at line {}, found end of file",
                                self.lx.line_of(open)
                            ),
                        ));
                    }
                    return Ok((stmts, None));
                }
                TokenKind::Punct(Punct::Semi) => self.advance()?,
                _ => match self.statement()? {
                    StmtOrTail::Stmt(s) => stmts.push(s),
                    StmtOrTail::Tail(e) => {
                        let at_end = match open {
                            Some(_) => self.is(Punct::RBrace),
                            None => self.tok.kind == TokenKind::Eof,
                        };
                        if at_end {
                            return Ok((stmts, Some(Box::new(e))));
                        }
                        return Err(self.expected("`;`"));
                    }
                },
            }
        }
    }

    fn statement(&mut self) -> PResult<StmtOrTail> {
        let start = self.tok.span.start;
        let kind = self.tok.kind;
        match kind {
            TokenKind::Keyword(Keyword::Let) => return self.let_stmt().map(StmtOrTail::Stmt),
            TokenKind::Keyword(
                Keyword::Fn
                | Keyword::Pub
                | Keyword::Struct
                | Keyword::Enum
                | Keyword::Impl
                | Keyword::Use
                | Keyword::Const
                | Keyword::Global
                | Keyword::Extern,
            ) => {
                let item = self.item()?;
                return Ok(StmtOrTail::Stmt(Stmt {
                    span: item.span,
                    kind: StmtKind::Item(item),
                }));
            }
            TokenKind::Ident(_) if self.is_scope() && self.scope_is_keyword()? => {
                self.advance()?;
                let inner = if self.is(Punct::LBrace) {
                    let block = self.block()?;
                    let span = block.span;
                    Stmt {
                        kind: StmtKind::Expr(Expr {
                            kind: ExprKind::Block(block),
                            span,
                        }),
                        span,
                    }
                } else {
                    match self.statement()? {
                        StmtOrTail::Stmt(s) => s,
                        StmtOrTail::Tail(_) => return Err(self.expected("`;`")),
                    }
                };
                if matches!(inner.kind, StmtKind::Let { .. } | StmtKind::Item(_)) {
                    return Err(self.error(
                        inner.span,
                        "`scope` takes a statement to run at scope exit; to close a value, write `let scope name = value;`",
                    ));
                }
                return Ok(StmtOrTail::Stmt(Stmt {
                    kind: StmtKind::Scope(Box::new(inner)),
                    span: self.span_from(start),
                }));
            }
            _ => {}
        }

        // A block-like expression at the start of a statement ends there.
        if self.starts_block_like() {
            let e = self.primary(Ctx::Normal)?;
            if self.eat(Punct::Semi)? || !self.is(Punct::RBrace) && self.tok.kind != TokenKind::Eof
            {
                let span = e.span;
                return Ok(StmtOrTail::Stmt(Stmt {
                    kind: StmtKind::Expr(e),
                    span,
                }));
            }
            return Ok(StmtOrTail::Tail(e));
        }

        let e = self.expr()?;
        let e = match self.assignment(start, e)? {
            Ok(assign) => {
                self.expect(Punct::Semi)?;
                return Ok(StmtOrTail::Stmt(assign));
            }
            Err(e) => e,
        };
        if self.eat(Punct::Semi)? {
            let span = self.span_from(start);
            return Ok(StmtOrTail::Stmt(Stmt {
                kind: StmtKind::Expr(e),
                span,
            }));
        }
        Ok(StmtOrTail::Tail(e))
    }

    /// The rest of an assignment whose target `e` (starting at `start`) is
    /// parsed, when `=` or a compound operator follows; otherwise `e` back.
    /// The caller takes the `;`.
    fn assignment(&mut self, start: u32, e: Expr) -> PResult<Result<Stmt, Expr>> {
        if self.is(Punct::Assign) {
            self.advance()?;
            let value = self.expr()?;
            let targets = match e.kind {
                ExprKind::Tuple(items) => items,
                _ => vec![e],
            };
            for t in &targets {
                if !matches!(
                    t.kind,
                    ExprKind::Name(_) | ExprKind::Field { .. } | ExprKind::Index { .. }
                ) {
                    return Err(self.error(t.span, "invalid left-hand side of assignment"));
                }
            }
            return Ok(Ok(Stmt {
                kind: StmtKind::Assign { targets, value },
                span: self.span_from(start),
            }));
        }
        if let Some(op) = self.compound_op() {
            let op_at = self.tok.span.start;
            self.advance()?;
            let value = self.expr()?;
            if !matches!(
                e.kind,
                ExprKind::Name(_) | ExprKind::Field { .. } | ExprKind::Index { .. }
            ) {
                return Err(self.error(e.span, "invalid left-hand side of assignment"));
            }
            return Ok(Ok(Stmt {
                kind: StmtKind::CompoundAssign {
                    op,
                    op_at,
                    target: e,
                    value,
                },
                span: self.span_from(start),
            }));
        }
        Ok(Err(e))
    }

    /// `scope` is a keyword at the start of a statement unless it's used as a variable
    /// (followed by `=`, `.`, `[`, `(`, an operator, ...).
    fn scope_is_keyword(&mut self) -> PResult<bool> {
        Ok(matches!(
            self.peek()?,
            TokenKind::Ident(_)
                | TokenKind::Keyword(_)
                | TokenKind::Int(_)
                | TokenKind::Float(_)
                | TokenKind::Str(_)
                | TokenKind::Template(_)
                | TokenKind::Label(_)
                | TokenKind::Punct(Punct::LBrace)
        ))
    }

    fn starts_block_like(&self) -> bool {
        matches!(
            self.tok.kind,
            TokenKind::Keyword(
                Keyword::If | Keyword::Match | Keyword::Loop | Keyword::While | Keyword::For,
            ) | TokenKind::Punct(Punct::LBrace)
                | TokenKind::Label(_)
        )
    }

    fn compound_op(&self) -> Option<BinOp> {
        let TokenKind::Punct(p) = self.tok.kind else {
            return None;
        };
        Some(match p {
            Punct::PlusEq => BinOp::Add,
            Punct::MinusEq => BinOp::Sub,
            Punct::StarEq => BinOp::Mul,
            Punct::SlashEq => BinOp::Div,
            Punct::PercentEq => BinOp::Mod,
            Punct::AmpEq => BinOp::BAnd,
            Punct::PipeEq => BinOp::BOr,
            Punct::CaretEq => BinOp::BXor,
            Punct::ShlEq => BinOp::Shl,
            Punct::ShrEq => BinOp::Shr,
            _ => return None,
        })
    }

    fn let_stmt(&mut self) -> PResult<Stmt> {
        let start = self.tok.span.start;
        self.advance()?;
        let mut scope = false;
        if self.is_scope()
            && matches!(
                self.peek()?,
                TokenKind::Ident(_) | TokenKind::Keyword(Keyword::Mut)
            )
        {
            scope = true;
            self.advance()?;
            if self.is_kw(Keyword::Mut) {
                return Err(self.error(
                    self.tok.span,
                    "a `let scope` binding cannot be `mut`: it is closed with the value it was given",
                ));
            }
        }
        let pat = self.pattern()?;
        let ty = if self.eat(Punct::Colon)? {
            Some(self.type_ann()?)
        } else {
            None
        };
        let value = if self.eat(Punct::Assign)? {
            Some(self.expr()?)
        } else {
            None
        };
        self.expect(Punct::Semi)?;
        Ok(Stmt {
            kind: StmtKind::Let {
                scope,
                pat,
                ty,
                value,
            },
            span: self.span_from(start),
        })
    }

    // ---- items --------------------------------------------------------------------------------

    fn item(&mut self) -> PResult<Item> {
        let start = self.tok.span.start;
        let is_pub = self.eat_kw(Keyword::Pub)?;
        let kind = match self.tok.kind {
            TokenKind::Keyword(Keyword::Fn) => ItemKind::Fn(self.fn_decl(false)?),
            TokenKind::Keyword(Keyword::Struct) => ItemKind::Struct(self.struct_decl()?),
            TokenKind::Keyword(Keyword::Enum) => ItemKind::Enum(self.enum_decl()?),
            TokenKind::Keyword(Keyword::Impl) if !is_pub => ItemKind::Impl(self.impl_decl()?),
            TokenKind::Keyword(Keyword::Use) if !is_pub => ItemKind::Use(self.use_decl()?),
            TokenKind::Keyword(Keyword::Const) => {
                self.advance()?;
                let name = self.name()?;
                let ty = if self.eat(Punct::Colon)? {
                    Some(self.type_ann()?)
                } else {
                    None
                };
                self.expect(Punct::Assign)?;
                let value = self.expr()?;
                self.expect(Punct::Semi)?;
                ItemKind::Const { name, ty, value }
            }
            TokenKind::Keyword(Keyword::Global) if !is_pub => {
                self.advance()?;
                let name = self.name()?;
                self.expect(Punct::Assign)?;
                let value = self.expr()?;
                self.expect(Punct::Semi)?;
                ItemKind::Global { name, value }
            }
            TokenKind::Keyword(Keyword::Extern) if !is_pub => {
                self.advance()?;
                let mut names = vec![self.name()?];
                while self.eat(Punct::Comma)? {
                    names.push(self.name()?);
                }
                self.expect(Punct::Semi)?;
                ItemKind::Extern(names)
            }
            _ if is_pub => {
                return Err(self.expected("`fn`, `struct`, `enum` or `const` after `pub`"));
            }
            _ => return Err(self.expected("an item")),
        };
        Ok(Item {
            is_pub,
            kind,
            span: self.span_from(start),
        })
    }

    /// `fn name(...) { ... }`; a `member` (a function in `impl`) may be named by a keyword.
    fn fn_decl(&mut self, member: bool) -> PResult<FnDecl> {
        let start = self.tok.span.start;
        self.advance()?; // fn
        let name = if member {
            self.member_name()?
        } else {
            self.name()?
        };
        self.skip_generics()?;
        let sig = self.fn_params()?;
        let body = self.block()?;
        Ok(FnDecl {
            name,
            sig,
            body,
            span: self.span_from(start),
        })
    }

    /// `(params) [-> T]`.
    fn fn_params(&mut self) -> PResult<FnSig> {
        self.expect(Punct::LParen)?;
        let mut sig = self.params(Punct::RParen)?;
        self.expect(Punct::RParen)?;
        if self.eat(Punct::Arrow)? {
            sig.ret = Some(self.type_ann()?);
        }
        Ok(sig)
    }

    /// A parameter list up to (not including) `close`.
    fn params(&mut self, close: Punct) -> PResult<FnSig> {
        let mut sig = FnSig::default();
        let mut first = true;
        while !self.is(close) {
            if sig.rest.is_some() {
                return Err(self.error(
                    self.tok.span,
                    "the rest parameter `...name` must be the last parameter",
                ));
            }
            let start = self.tok.span.start;
            if self.is_kw(Keyword::SelfValue) {
                if !first {
                    return Err(self.error(self.tok.span, "`self` must be the first parameter"));
                }
                sig.self_param = Some(self.tok.span);
                self.advance()?;
                if self.eat(Punct::Colon)? {
                    self.type_ann()?;
                }
            } else if self.eat(Punct::DotDotDot)? {
                sig.rest = Some(self.name()?);
                if self.eat(Punct::Colon)? {
                    self.type_ann()?;
                }
            } else {
                let mutable = self.eat_kw(Keyword::Mut)?;
                let name = self.name()?;
                let name = (name.sym != self.sym_underscore).then_some(name);
                let ty = if self.eat(Punct::Colon)? {
                    Some(self.type_ann()?)
                } else {
                    None
                };
                sig.params.push(Param {
                    name,
                    mutable,
                    ty,
                    span: self.span_from(start),
                });
            }
            first = false;
            if !self.eat(Punct::Comma)? {
                break;
            }
        }
        Ok(sig)
    }

    /// Skips `<...>` generic parameters, which only `orbit-check` will read.
    fn skip_generics(&mut self) -> PResult<()> {
        if !self.is(Punct::Lt) {
            return Ok(());
        }
        let mut depth = 0i32;
        loop {
            match self.tok.kind {
                TokenKind::Punct(Punct::Lt) => depth += 1,
                TokenKind::Punct(Punct::Gt) => depth -= 1,
                TokenKind::Punct(Punct::Shr) => depth -= 2,
                TokenKind::Eof => return Err(self.expected("`>`")),
                _ => {}
            }
            self.advance()?;
            if depth <= 0 {
                return Ok(());
            }
        }
    }

    fn field_decls(&mut self) -> PResult<Vec<FieldDecl>> {
        self.expect(Punct::LBrace)?;
        let mut fields = Vec::new();
        while !self.is(Punct::RBrace) {
            // `pub` is a field's visibility unless it is the field's own name (`pub: bool`).
            let is_pub = self.is_kw(Keyword::Pub)
                && !matches!(
                    self.peek()?,
                    TokenKind::Punct(Punct::Colon | Punct::Comma | Punct::RBrace)
                );
            if is_pub {
                self.advance()?;
            }
            let name = self.member_name()?;
            let ty = if self.eat(Punct::Colon)? {
                Some(self.type_ann()?)
            } else {
                None
            };
            fields.push(FieldDecl { is_pub, name, ty });
            if !self.eat(Punct::Comma)? {
                break;
            }
        }
        self.expect(Punct::RBrace)?;
        Ok(fields)
    }

    fn struct_decl(&mut self) -> PResult<StructDecl> {
        self.advance()?; // struct
        let name = self.name()?;
        self.skip_generics()?;
        let fields = if self.eat(Punct::Semi)? {
            Vec::new()
        } else if self.is(Punct::LParen) {
            return Err(self.error(
                self.tok.span,
                "tuple structs are not supported; name the fields: `struct P { x: float }`",
            ));
        } else {
            self.field_decls()?
        };
        Ok(StructDecl { name, fields })
    }

    fn enum_decl(&mut self) -> PResult<EnumDecl> {
        self.advance()?; // enum
        let name = self.name()?;
        self.skip_generics()?;
        self.expect(Punct::LBrace)?;
        let mut variants = Vec::new();
        while !self.is(Punct::RBrace) {
            let start = self.tok.span.start;
            let vname = self.member_name()?;
            let fields = if self.is(Punct::LBrace) {
                Some(self.field_decls()?)
            } else if self.is(Punct::LParen) {
                return Err(self.error(
                    self.tok.span,
                    "tuple variants are not supported; name the fields: `Circle { r: float }`",
                ));
            } else {
                None
            };
            variants.push(Variant {
                name: vname,
                fields,
                span: self.span_from(start),
            });
            if !self.eat(Punct::Comma)? {
                break;
            }
        }
        self.expect(Punct::RBrace)?;
        Ok(EnumDecl { name, variants })
    }

    fn impl_decl(&mut self) -> PResult<ImplDecl> {
        self.advance()?; // impl
        self.skip_generics()?;
        let target = match self.tok.kind {
            TokenKind::Ident(_) => self.name()?,
            _ => return Err(self.expected("a struct or enum name")),
        };
        self.skip_generics()?;
        if self.is_kw(Keyword::For) {
            return Err(self.error(
                self.tok.span,
                "traits are not supported yet; write metamethods such as `fn __add` directly in `impl`",
            ));
        }
        self.expect(Punct::LBrace)?;
        let mut members = Vec::new();
        while !self.is(Punct::RBrace) {
            let start = self.tok.span.start;
            let is_pub = self.eat_kw(Keyword::Pub)?;
            let kind = match self.tok.kind {
                TokenKind::Keyword(Keyword::Fn) => ItemKind::Fn(self.fn_decl(true)?),
                TokenKind::Keyword(Keyword::Const) => {
                    self.advance()?;
                    let name = self.member_name()?;
                    let ty = if self.eat(Punct::Colon)? {
                        Some(self.type_ann()?)
                    } else {
                        None
                    };
                    self.expect(Punct::Assign)?;
                    let value = self.expr()?;
                    self.expect(Punct::Semi)?;
                    ItemKind::Const { name, ty, value }
                }
                _ => return Err(self.expected("`fn` or `const` in `impl`")),
            };
            members.push(Item {
                is_pub,
                kind,
                span: self.span_from(start),
            });
        }
        self.expect(Punct::RBrace)?;
        Ok(ImplDecl { target, members })
    }

    fn use_decl(&mut self) -> PResult<UseDecl> {
        self.advance()?; // use
        // Path segments may be keywords (`use text::match as m;`), but a segment that becomes a
        // local's name needs `r#` or an `as` alias.
        let mut keyword_last = matches!(self.tok.kind, TokenKind::Keyword(_));
        let mut path = vec![self.member_name()?];
        let mut tail = UseTail::Module { alias: None };
        while self.eat(Punct::ColonColon)? {
            if self.is(Punct::LBrace) {
                self.advance()?;
                let mut members = Vec::new();
                while !self.is(Punct::RBrace) {
                    let keyword = matches!(self.tok.kind, TokenKind::Keyword(_));
                    let name = self.member_name()?;
                    let alias = if self.eat_kw(Keyword::As)? {
                        Some(self.name()?)
                    } else if keyword {
                        return Err(self.keyword_binding(name));
                    } else {
                        None
                    };
                    members.push((name, alias));
                    if !self.eat(Punct::Comma)? {
                        break;
                    }
                }
                self.expect(Punct::RBrace)?;
                tail = UseTail::Members(members);
                keyword_last = false;
                break;
            }
            keyword_last = matches!(self.tok.kind, TokenKind::Keyword(_));
            path.push(self.member_name()?);
        }
        if matches!(tail, UseTail::Module { .. }) && self.eat_kw(Keyword::As)? {
            tail = UseTail::Module {
                alias: Some(self.name()?),
            };
        } else if keyword_last {
            return Err(self.keyword_binding(*path.last().expect("a path segment")));
        }
        self.expect(Punct::Semi)?;
        Ok(UseDecl { path, tail })
    }

    // ---- types --------------------------------------------------------------------------------

    fn type_ann(&mut self) -> PResult<Type> {
        self.enter()?;
        let start = self.tok.span.start;
        match self.tok.kind {
            TokenKind::Punct(Punct::LParen) => {
                self.advance()?;
                while !self.is(Punct::RParen) {
                    self.type_ann()?;
                    if !self.eat(Punct::Comma)? {
                        break;
                    }
                }
                self.expect(Punct::RParen)?;
            }
            TokenKind::Punct(Punct::LBracket) => {
                self.advance()?;
                self.type_ann()?;
                self.expect(Punct::RBracket)?;
            }
            TokenKind::Punct(Punct::HashBrace) => {
                self.advance()?;
                self.type_ann()?;
                self.expect(Punct::Colon)?;
                self.type_ann()?;
                self.expect(Punct::RBrace)?;
            }
            TokenKind::Keyword(Keyword::Fn) => {
                self.advance()?;
                self.expect(Punct::LParen)?;
                while !self.is(Punct::RParen) {
                    self.type_ann()?;
                    if !self.eat(Punct::Comma)? {
                        break;
                    }
                }
                self.expect(Punct::RParen)?;
                if self.eat(Punct::Arrow)? {
                    self.type_ann()?;
                }
            }
            TokenKind::Ident(_) | TokenKind::Keyword(Keyword::SelfType | Keyword::Nil) => {
                self.advance()?;
                while self.eat(Punct::ColonColon)? {
                    self.member_name()?;
                }
                if self.eat(Punct::Lt)? {
                    while !self.is(Punct::Gt) && !self.is(Punct::Shr) {
                        self.type_ann()?;
                        if !self.eat(Punct::Comma)? {
                            break;
                        }
                    }
                    self.expect_gt()?;
                }
            }
            _ => return Err(self.expected("a type")),
        }
        while self.eat(Punct::Question)? {}
        self.leave();
        Ok(Type {
            span: self.span_from(start),
        })
    }

    // ---- patterns -----------------------------------------------------------------------------

    fn pattern(&mut self) -> PResult<Pat> {
        let start = self.tok.span.start;
        self.eat(Punct::Pipe)?;
        let first = self.pattern_single()?;
        if !self.is(Punct::Pipe) {
            return Ok(first);
        }
        let mut alts = vec![first];
        while self.eat(Punct::Pipe)? {
            alts.push(self.pattern_single()?);
        }
        Ok(Pat {
            kind: PatKind::Or(alts),
            span: self.span_from(start),
        })
    }

    fn pattern_single(&mut self) -> PResult<Pat> {
        self.enter()?;
        let start = self.tok.span.start;
        let kind = match self.tok.kind {
            TokenKind::Ident(s) if s == self.sym_underscore => {
                self.advance()?;
                PatKind::Wild
            }
            TokenKind::Keyword(Keyword::Mut) => {
                self.advance()?;
                let name = self.name()?;
                PatKind::Bind {
                    name,
                    mutable: true,
                }
            }
            TokenKind::Ident(_) | TokenKind::Keyword(Keyword::SelfType) => {
                let path = self.path_expr()?;
                if self.is(Punct::LBrace) {
                    self.struct_pattern(path)?
                } else {
                    match path.kind {
                        ExprKind::Name(sym) => PatKind::Bind {
                            name: Name {
                                sym,
                                span: path.span,
                            },
                            mutable: false,
                        },
                        _ => PatKind::Path(path),
                    }
                }
            }
            TokenKind::Punct(Punct::LParen) => {
                self.advance()?;
                let mut items = Vec::new();
                while !self.is(Punct::RParen) {
                    items.push(self.pattern()?);
                    if !self.eat(Punct::Comma)? {
                        break;
                    }
                }
                self.expect(Punct::RParen)?;
                if items.len() == 1 {
                    self.leave();
                    return Ok(items.pop().expect("one pattern"));
                }
                PatKind::Tuple(items)
            }
            _ => {
                let lit = self.literal_pattern()?;
                if self.is(Punct::DotDotEq) || self.is(Punct::DotDot) {
                    let inclusive = self.is(Punct::DotDotEq);
                    self.advance()?;
                    let end = self.literal_pattern()?;
                    PatKind::Range {
                        start: lit,
                        end,
                        inclusive,
                    }
                } else {
                    PatKind::Lit(lit)
                }
            }
        };
        self.leave();
        Ok(Pat {
            kind,
            span: self.span_from(start),
        })
    }

    fn literal_pattern(&mut self) -> PResult<Expr> {
        let start = self.tok.span.start;
        let neg = self.eat(Punct::Minus)?;
        let kind = match self.tok.kind {
            TokenKind::Int(v) => ExprKind::Int(if neg { v.wrapping_neg() } else { v }),
            TokenKind::Float(v) => ExprKind::Float(if neg { -v } else { v }),
            TokenKind::Str(s) if !neg => ExprKind::Str(s),
            TokenKind::Keyword(Keyword::Nil) if !neg => ExprKind::Nil,
            TokenKind::Keyword(Keyword::True) if !neg => ExprKind::True,
            TokenKind::Keyword(Keyword::False) if !neg => ExprKind::False,
            TokenKind::Keyword(k) if !neg && k != Keyword::SelfValue => {
                let mut d = self.expected("a pattern");
                d.message += &format!("; write `r#{0}` to use `{0}` as a name", k.as_str());
                return Err(d);
            }
            _ => return Err(self.expected("a pattern")),
        };
        self.advance()?;
        Ok(Expr {
            kind,
            span: self.span_from(start),
        })
    }

    fn struct_pattern(&mut self, path: Expr) -> PResult<PatKind> {
        self.expect(Punct::LBrace)?;
        let mut fields = Vec::new();
        let mut rest = false;
        while !self.is(Punct::RBrace) {
            if self.eat(Punct::DotDot)? {
                rest = true;
                break;
            }
            let keyword = matches!(self.tok.kind, TokenKind::Keyword(_));
            let name = self.member_name()?;
            let pat = if self.eat(Punct::Colon)? {
                Some(self.pattern()?)
            } else if keyword {
                return Err(self.keyword_binding(name));
            } else {
                None
            };
            fields.push(FieldPat { name, pat });
            if !self.eat(Punct::Comma)? {
                break;
            }
        }
        self.expect(Punct::RBrace)?;
        Ok(PatKind::Struct { path, fields, rest })
    }

    /// `Name`, `Self` or `a::b::c` (for patterns and struct literals).
    fn path_expr(&mut self) -> PResult<Expr> {
        let start = self.tok.span.start;
        let mut e = match self.tok.kind {
            TokenKind::Keyword(Keyword::SelfType) => {
                self.advance()?;
                Expr {
                    kind: ExprKind::SelfType,
                    span: self.span_from(start),
                }
            }
            _ => {
                let n = self.name()?;
                Expr {
                    kind: ExprKind::Name(n.sym),
                    span: n.span,
                }
            }
        };
        while self.eat(Punct::ColonColon)? {
            let name = self.member_name()?;
            e = Expr {
                kind: ExprKind::Path {
                    base: Box::new(e),
                    name,
                },
                span: self.span_from(start),
            };
        }
        Ok(e)
    }

    // ---- expressions --------------------------------------------------------------------------

    fn expr(&mut self) -> PResult<Expr> {
        self.expr_ctx(Ctx::Normal)
    }

    /// A full expression, including a trailing range (`a..b`).
    fn expr_ctx(&mut self, ctx: Ctx) -> PResult<Expr> {
        let start = self.tok.span.start;
        let lhs = self.binary(0, ctx)?;
        if self.is(Punct::DotDot) || self.is(Punct::DotDotEq) {
            let inclusive = self.is(Punct::DotDotEq);
            self.advance()?;
            let end = self.binary(0, ctx)?;
            return Ok(Expr {
                kind: ExprKind::Range {
                    start: Box::new(lhs),
                    end: Box::new(end),
                    inclusive,
                },
                span: self.span_from(start),
            });
        }
        Ok(lhs)
    }

    fn binop(&self) -> Option<BinOp> {
        let TokenKind::Punct(p) = self.tok.kind else {
            return None;
        };
        Some(match p {
            Punct::PipePipe => BinOp::Or,
            Punct::AmpAmp => BinOp::And,
            Punct::EqEq => BinOp::Eq,
            Punct::NotEq => BinOp::Ne,
            Punct::Lt => BinOp::Lt,
            Punct::Le => BinOp::Le,
            Punct::Gt => BinOp::Gt,
            Punct::Ge => BinOp::Ge,
            Punct::Pipe => BinOp::BOr,
            Punct::Caret => BinOp::BXor,
            Punct::Amp => BinOp::BAnd,
            Punct::Shl => BinOp::Shl,
            Punct::Shr => BinOp::Shr,
            Punct::Plus => BinOp::Add,
            Punct::Minus => BinOp::Sub,
            Punct::Tilde => BinOp::Concat,
            Punct::Star => BinOp::Mul,
            Punct::Slash => BinOp::Div,
            Punct::Backslash => BinOp::IDiv,
            Punct::Percent => BinOp::Mod,
            Punct::StarStar => BinOp::Pow,
            _ => return None,
        })
    }

    fn binary(&mut self, min_bp: u8, ctx: Ctx) -> PResult<Expr> {
        self.enter()?;
        let start = self.tok.span.start;
        let mut lhs = self.unary(ctx)?;
        let mut last_cmp: Option<BinOp> = None;
        while let Some(op) = self.binop() {
            let (l, r) = op.binding_power();
            if l < min_bp {
                break;
            }
            if op.is_comparison() {
                if let Some(prev) = last_cmp {
                    return Err(self.error(
                        self.tok.span,
                        format!(
                            "comparison operators cannot be chained: `{}` after `{}`; use `&&`",
                            op.as_str(),
                            prev.as_str()
                        ),
                    ));
                }
                last_cmp = Some(op);
            }
            let op_at = self.tok.span.start;
            self.advance()?;
            let rhs = self.binary(r, ctx)?;
            lhs = Expr {
                kind: ExprKind::Binary {
                    op,
                    op_at,
                    lhs: Box::new(lhs),
                    rhs: Box::new(rhs),
                },
                span: self.span_from(start),
            };
        }
        self.leave();
        Ok(lhs)
    }

    fn unary(&mut self, ctx: Ctx) -> PResult<Expr> {
        let start = self.tok.span.start;
        let op = match self.tok.kind {
            TokenKind::Punct(Punct::Minus) => Some(UnOp::Neg),
            TokenKind::Punct(Punct::Bang) => Some(UnOp::Not),
            TokenKind::Punct(Punct::Hash) => Some(UnOp::Len),
            TokenKind::Punct(Punct::Tilde) => Some(UnOp::BNot),
            _ => None,
        };
        if let Some(op) = op {
            self.advance()?;
            let expr = self.binary(UNARY_POWER, ctx)?;
            return Ok(Expr {
                kind: ExprKind::Unary {
                    op,
                    expr: Box::new(expr),
                },
                span: self.span_from(start),
            });
        }
        if self.is(Punct::DotDotDot) {
            self.advance()?;
            let expr = self.unary(ctx)?;
            return Ok(Expr {
                kind: ExprKind::Spread(Box::new(expr)),
                span: self.span_from(start),
            });
        }
        let e = self.primary(ctx)?;
        self.postfix(e, ctx)
    }

    fn postfix(&mut self, mut e: Expr, ctx: Ctx) -> PResult<Expr> {
        let start = e.span.start;
        loop {
            match self.tok.kind {
                TokenKind::Punct(Punct::Dot) => {
                    self.advance()?;
                    let name = self.member_name()?;
                    if self.is(Punct::LParen) {
                        let args_at = self.tok.span.start;
                        let args = self.call_args()?;
                        e = Expr {
                            kind: ExprKind::MethodCall {
                                obj: Box::new(e),
                                name,
                                args,
                                args_at,
                            },
                            span: self.span_from(start),
                        };
                    } else {
                        e = Expr {
                            kind: ExprKind::Field {
                                obj: Box::new(e),
                                name,
                            },
                            span: self.span_from(start),
                        };
                    }
                }
                TokenKind::Punct(Punct::ColonColon) => {
                    self.advance()?;
                    if self.is(Punct::Lt) {
                        return Err(self.error(
                            self.tok.span,
                            "generic arguments are only allowed in type annotations",
                        ));
                    }
                    let name = self.member_name()?;
                    e = Expr {
                        kind: ExprKind::Path {
                            base: Box::new(e),
                            name,
                        },
                        span: self.span_from(start),
                    };
                    if self.is(Punct::LBrace) && ctx == Ctx::Normal {
                        e = self.struct_lit(e)?;
                    }
                }
                TokenKind::Punct(Punct::LParen) => {
                    let args_at = self.tok.span.start;
                    let args = self.call_args()?;
                    e = Expr {
                        kind: ExprKind::Call {
                            callee: Box::new(e),
                            args,
                            args_at,
                        },
                        span: self.span_from(start),
                    };
                }
                TokenKind::Punct(Punct::LBracket) => {
                    self.advance()?;
                    let key = self.expr()?;
                    self.expect(Punct::RBracket)?;
                    e = Expr {
                        kind: ExprKind::Index {
                            obj: Box::new(e),
                            key: Box::new(key),
                        },
                        span: self.span_from(start),
                    };
                }
                TokenKind::Punct(Punct::Question) => {
                    self.advance()?;
                    e = Expr {
                        kind: ExprKind::Try(Box::new(e)),
                        span: self.span_from(start),
                    };
                }
                TokenKind::Keyword(Keyword::As) => {
                    return Err(self.error(
                        self.tok.span,
                        "`as` casts are not supported; use `math::tointeger` or `tostring`",
                    ));
                }
                _ => return Ok(e),
            }
        }
    }

    fn call_args(&mut self) -> PResult<Vec<Expr>> {
        self.expect(Punct::LParen)?;
        let mut args = Vec::new();
        while !self.is(Punct::RParen) {
            args.push(self.expr()?);
            if !self.eat(Punct::Comma)? {
                break;
            }
        }
        self.expect(Punct::RParen)?;
        Ok(args)
    }

    fn struct_lit(&mut self, path: Expr) -> PResult<Expr> {
        let start = path.span.start;
        self.expect(Punct::LBrace)?;
        let mut fields = Vec::new();
        while !self.is(Punct::RBrace) {
            let keyword = matches!(self.tok.kind, TokenKind::Keyword(_));
            let name = self.member_name()?;
            let value = if self.eat(Punct::Colon)? {
                Some(self.expr()?)
            } else if keyword {
                return Err(self.keyword_binding(name));
            } else {
                None
            };
            fields.push(FieldInit { name, value });
            if !self.eat(Punct::Comma)? {
                break;
            }
        }
        self.expect(Punct::RBrace)?;
        Ok(Expr {
            kind: ExprKind::StructLit {
                path: Box::new(path),
                fields,
            },
            span: self.span_from(start),
        })
    }

    fn primary(&mut self, ctx: Ctx) -> PResult<Expr> {
        self.enter()?;
        let start = self.tok.span.start;
        let simple = |p: &mut Self, kind: ExprKind| -> PResult<Expr> {
            p.advance()?;
            Ok(Expr {
                kind,
                span: p.span_from(start),
            })
        };
        let e = match self.tok.kind {
            TokenKind::Int(v) => simple(self, ExprKind::Int(v))?,
            TokenKind::Float(v) => simple(self, ExprKind::Float(v))?,
            TokenKind::Str(s) => simple(self, ExprKind::Str(s))?,
            TokenKind::Template(i) => {
                let t = self.lx.take_template(i);
                self.advance()?;
                self.template(t.parts, t.span)?
            }
            TokenKind::Keyword(Keyword::Nil) => simple(self, ExprKind::Nil)?,
            TokenKind::Keyword(Keyword::True) => simple(self, ExprKind::True)?,
            TokenKind::Keyword(Keyword::False) => simple(self, ExprKind::False)?,
            TokenKind::Keyword(Keyword::SelfValue) => simple(self, ExprKind::SelfValue)?,
            TokenKind::Keyword(Keyword::SelfType) => {
                let e = simple(self, ExprKind::SelfType)?;
                if self.is(Punct::LBrace) && ctx == Ctx::Normal {
                    self.struct_lit(e)?
                } else {
                    e
                }
            }
            TokenKind::Ident(sym) => {
                if sym == self.sym_underscore {
                    return Err(self.error(
                        self.tok.span,
                        "`_` can only be used in patterns and parameters",
                    ));
                }
                let e = simple(self, ExprKind::Name(sym))?;
                if self.is(Punct::LBrace) && ctx == Ctx::Normal {
                    self.struct_lit(e)?
                } else {
                    e
                }
            }
            TokenKind::Punct(Punct::LParen) => {
                self.advance()?;
                let mut items = Vec::new();
                let mut trailing_comma = false;
                while !self.is(Punct::RParen) {
                    items.push(self.expr()?);
                    trailing_comma = self.eat(Punct::Comma)?;
                    if !trailing_comma {
                        break;
                    }
                }
                self.expect(Punct::RParen)?;
                let span = self.span_from(start);
                match items.len() {
                    0 => {
                        return Err(self.error(span, "`()` is not a value; use `nil`"));
                    }
                    1 if !trailing_comma => Expr {
                        kind: ExprKind::Paren(Box::new(items.pop().expect("one item"))),
                        span,
                    },
                    _ => Expr {
                        kind: ExprKind::Tuple(items),
                        span,
                    },
                }
            }
            TokenKind::Punct(Punct::LBracket) => {
                self.advance()?;
                let mut items = Vec::new();
                while !self.is(Punct::RBracket) {
                    items.push(self.expr()?);
                    if !self.eat(Punct::Comma)? {
                        break;
                    }
                }
                self.expect(Punct::RBracket)?;
                Expr {
                    kind: ExprKind::Array(items),
                    span: self.span_from(start),
                }
            }
            TokenKind::Punct(Punct::HashBrace) => self.map_literal()?,
            TokenKind::Punct(Punct::LBrace) => {
                let block = self.block()?;
                Expr {
                    span: block.span,
                    kind: ExprKind::Block(block),
                }
            }
            TokenKind::Punct(Punct::Pipe | Punct::PipePipe) => self.closure()?,
            TokenKind::Keyword(Keyword::If) => self.if_expr()?,
            TokenKind::Keyword(Keyword::Match) => self.match_expr()?,
            TokenKind::Keyword(Keyword::Loop | Keyword::While | Keyword::For) => {
                self.loop_expr(None)?
            }
            TokenKind::Label(sym) => {
                let label = Name {
                    sym,
                    span: self.tok.span,
                };
                self.advance()?;
                self.expect(Punct::Colon)?;
                if !matches!(
                    self.tok.kind,
                    TokenKind::Keyword(Keyword::Loop | Keyword::While | Keyword::For)
                ) {
                    return Err(self.expected("`loop`, `while` or `for` after a label"));
                }
                self.loop_expr(Some(label))?
            }
            TokenKind::Keyword(Keyword::Return) => {
                self.advance()?;
                let value = if self.starts_expr() {
                    Some(Box::new(self.expr()?))
                } else {
                    None
                };
                Expr {
                    kind: ExprKind::Return(value),
                    span: self.span_from(start),
                }
            }
            TokenKind::Keyword(Keyword::Break) => {
                self.advance()?;
                let label = self.loop_label()?;
                let value = if self.starts_expr() {
                    Some(Box::new(self.expr()?))
                } else {
                    None
                };
                Expr {
                    kind: ExprKind::Break { label, value },
                    span: self.span_from(start),
                }
            }
            TokenKind::Keyword(Keyword::Continue) => {
                self.advance()?;
                let label = self.loop_label()?;
                Expr {
                    kind: ExprKind::Continue { label },
                    span: self.span_from(start),
                }
            }
            TokenKind::Keyword(Keyword::Let) => {
                return Err(self.error(
                    self.tok.span,
                    "`let` is a statement; it can't be used as an expression",
                ));
            }
            _ => return Err(self.expected("an expression")),
        };
        self.leave();
        Ok(e)
    }

    fn loop_label(&mut self) -> PResult<Option<Name>> {
        if let TokenKind::Label(sym) = self.tok.kind {
            let span = self.tok.span;
            self.advance()?;
            return Ok(Some(Name { sym, span }));
        }
        Ok(None)
    }

    /// Whether the current token can start an expression (for `return` and `break` values).
    fn starts_expr(&self) -> bool {
        match self.tok.kind {
            TokenKind::Eof => false,
            TokenKind::Punct(p) => matches!(
                p,
                Punct::LParen
                    | Punct::LBracket
                    | Punct::LBrace
                    | Punct::HashBrace
                    | Punct::Minus
                    | Punct::Bang
                    | Punct::Hash
                    | Punct::Tilde
                    | Punct::Pipe
                    | Punct::PipePipe
                    | Punct::DotDotDot
            ),
            TokenKind::Keyword(k) => !matches!(k, Keyword::Else | Keyword::As | Keyword::In),
            _ => true,
        }
    }

    fn template(&mut self, parts: Vec<TemplatePart>, span: Span) -> PResult<Expr> {
        let mut pieces = Vec::with_capacity(parts.len());
        for part in parts {
            match part {
                TemplatePart::Text(t) => pieces.push(TemplatePiece::Text(t)),
                TemplatePart::Hole { expr, spec } => {
                    if expr.start == expr.end
                        || self.lx.src()[expr.start as usize..expr.end as usize]
                            .iter()
                            .all(u8::is_ascii_whitespace)
                    {
                        return Err(self.error(
                            expr,
                            "empty `{}` in template; write `{{}}` for literal braces",
                        ));
                    }
                    // Parse the hole's source range as an expression, then resume after the
                    // template.
                    let saved_tok = self.tok;
                    let saved_ahead = self.ahead.take();
                    let saved_prev = self.prev_end;
                    let saved_lx = self.lx.enter(expr);
                    self.advance()?;
                    let e = self.expr()?;
                    if self.tok.kind != TokenKind::Eof {
                        return Err(self.expected("`}` to end the template hole"));
                    }
                    self.lx.restore(saved_lx);
                    self.tok = saved_tok;
                    self.ahead = saved_ahead;
                    self.prev_end = saved_prev;
                    pieces.push(TemplatePiece::Hole {
                        expr: e,
                        spec: spec.map(|s| FormatSpec {
                            text: s.text,
                            span: s.span,
                        }),
                    });
                }
            }
        }
        Ok(Expr {
            kind: ExprKind::Template(pieces),
            span,
        })
    }

    fn map_literal(&mut self) -> PResult<Expr> {
        let start = self.tok.span.start;
        self.advance()?; // #{
        let mut entries = Vec::new();
        while !self.is(Punct::RBrace) {
            let kind = self.tok.kind;
            let entry = match kind {
                TokenKind::Ident(sym) if self.peek()? == TokenKind::Punct(Punct::Colon) => {
                    let span = self.tok.span;
                    self.advance()?;
                    self.advance()?;
                    MapEntry::Named(sym, span, self.expr()?)
                }
                TokenKind::Keyword(_) if self.peek()? == TokenKind::Punct(Punct::Colon) => {
                    let name = self.member_name()?;
                    self.advance()?;
                    MapEntry::Named(name.sym, name.span, self.expr()?)
                }
                TokenKind::Str(sym) if self.peek()? == TokenKind::Punct(Punct::Colon) => {
                    let span = self.tok.span;
                    self.advance()?;
                    self.advance()?;
                    MapEntry::Named(sym, span, self.expr()?)
                }
                _ => {
                    let e = self.expr()?;
                    if self.eat(Punct::Colon)? {
                        match e.kind {
                            ExprKind::Array(mut items)
                                if items.len() == 1
                                    && !matches!(items[0].kind, ExprKind::Spread(_)) =>
                            {
                                let key = items.pop().expect("one key");
                                MapEntry::Keyed(key, self.expr()?)
                            }
                            _ => {
                                return Err(self.error(
                                    e.span,
                                    "a computed key is written in brackets: `[key]: value`",
                                ));
                            }
                        }
                    } else if let ExprKind::Name(name) = e.kind {
                        // Struct literals read `P { x }` as `x: x`; here it
                        // would silently be `[1] = x`, so neither is chosen.
                        let name = self.lx.strings().display(name).into_owned();
                        return Err(self.error(
                            e.span,
                            format!(
                                "a bare name in `#{{ }}` is ambiguous: write `{name}: {name}` for a field, `({name})` for an array entry, or use `[...]` for an array"
                            ),
                        ));
                    } else {
                        MapEntry::Positional(e)
                    }
                }
            };
            entries.push(entry);
            if !self.eat(Punct::Comma)? {
                break;
            }
        }
        self.expect(Punct::RBrace)?;
        Ok(Expr {
            kind: ExprKind::Map(entries),
            span: self.span_from(start),
        })
    }

    fn closure(&mut self) -> PResult<Expr> {
        let start = self.tok.span.start;
        let mut sig = if self.eat(Punct::PipePipe)? {
            FnSig::default()
        } else {
            self.expect(Punct::Pipe)?;
            let sig = self.params(Punct::Pipe)?;
            self.expect(Punct::Pipe)?;
            sig
        };
        if let Some(span) = sig.self_param {
            return Err(self.error(span, "closures cannot take `self`"));
        }
        let body = if self.eat(Punct::Arrow)? {
            sig.ret = Some(self.type_ann()?);
            let block = self.block()?;
            Expr {
                span: block.span,
                kind: ExprKind::Block(block),
            }
        } else {
            self.expr()?
        };
        let span = self.span_from(start);
        Ok(Expr {
            kind: ExprKind::Closure(Box::new(Closure { sig, body, span })),
            span,
        })
    }

    fn if_expr(&mut self) -> PResult<Expr> {
        let start = self.tok.span.start;
        self.advance()?; // if
        let cond = self.expr_ctx(Ctx::NoStruct)?;
        let then = self.block()?;
        let else_ = if self.eat_kw(Keyword::Else)? {
            if self.is_kw(Keyword::If) {
                Some(Box::new(self.if_expr()?))
            } else {
                let block = self.block()?;
                Some(Box::new(Expr {
                    span: block.span,
                    kind: ExprKind::Block(block),
                }))
            }
        } else {
            None
        };
        Ok(Expr {
            kind: ExprKind::If {
                cond: Box::new(cond),
                then,
                else_,
            },
            span: self.span_from(start),
        })
    }

    fn match_expr(&mut self) -> PResult<Expr> {
        let start = self.tok.span.start;
        self.advance()?; // match
        let scrutinee = self.expr_ctx(Ctx::NoStruct)?;
        self.expect(Punct::LBrace)?;
        let mut arms = Vec::new();
        while !self.is(Punct::RBrace) {
            let arm_start = self.tok.span.start;
            let pat = self.pattern()?;
            let guard = if self.eat_kw(Keyword::If)? {
                Some(self.expr()?)
            } else {
                None
            };
            self.expect(Punct::FatArrow)?;
            let body_start = self.tok.span.start;
            let body = self.expr()?;
            // `pat => target = value,` runs the assignment; the arm's value
            // is nil, as `pat => { target = value; }` (Rust's unit).
            let body = match self.assignment(body_start, body)? {
                Ok(assign) => {
                    let span = assign.span;
                    Expr {
                        kind: ExprKind::Block(Block {
                            stmts: vec![assign],
                            tail: None,
                            span,
                        }),
                        span,
                    }
                }
                Err(body) => body,
            };
            let block_like = body.kind.is_block_like();
            arms.push(Arm {
                pat,
                guard,
                body,
                span: self.span_from(arm_start),
            });
            if !self.eat(Punct::Comma)? && !block_like {
                break;
            }
        }
        self.expect(Punct::RBrace)?;
        Ok(Expr {
            kind: ExprKind::Match {
                scrutinee: Box::new(scrutinee),
                arms,
            },
            span: self.span_from(start),
        })
    }

    fn loop_expr(&mut self, label: Option<Name>) -> PResult<Expr> {
        let start = label.map_or(self.tok.span.start, |l| l.span.start);
        let kind = match self.tok.kind {
            TokenKind::Keyword(Keyword::Loop) => {
                self.advance()?;
                ExprKind::Loop {
                    label,
                    body: self.block()?,
                }
            }
            TokenKind::Keyword(Keyword::While) => {
                self.advance()?;
                let cond = self.expr_ctx(Ctx::NoStruct)?;
                ExprKind::While {
                    label,
                    cond: Box::new(cond),
                    body: self.block()?,
                }
            }
            _ => {
                self.advance()?; // for
                let pat = self.pattern()?;
                if !self.eat_kw(Keyword::In)? {
                    return Err(self.expected("`in`"));
                }
                let iter = self.expr_ctx(Ctx::NoStruct)?;
                ExprKind::For {
                    label,
                    pat: Box::new(pat),
                    iter: Box::new(iter),
                    body: self.block()?,
                }
            }
        };
        Ok(Expr {
            kind,
            span: self.span_from(start),
        })
    }
}

enum StmtOrTail {
    Stmt(Stmt),
    /// An expression not followed by `;`: the block's value if the block ends here.
    Tail(Expr),
}
