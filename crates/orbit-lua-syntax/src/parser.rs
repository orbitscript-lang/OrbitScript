//! Recursive-descent parser for statements, precedence climbing for expressions.
//!
//! Parsing is purely syntactic: names stay unresolved until lowering. The checks that only need
//! the syntax (`break` outside a loop, `...` outside a vararg function, attribute names) happen
//! here, so their messages point where the reference's do.

use crate::ast::*;
use crate::diag::{Diagnostic, Near};
use crate::lexer::Lexer;
use crate::token::{Keyword, Punct, Token, TokenKind};
use orbit_hir::{Language, SourceInfo, Span, StrId};

/// Parser settings.
#[derive(Clone, Debug)]
pub struct ParseOptions {
    /// Maximum nesting of blocks and expressions, so deeply nested input can't overflow the Rust
    /// stack. The reference allows about 200 levels.
    pub max_depth: u32,
    /// Skip a first line starting with `#`, as the standalone interpreter does for script files.
    pub skip_comment_line: bool,
}

impl Default for ParseOptions {
    fn default() -> ParseOptions {
        ParseOptions {
            max_depth: 200,
            skip_comment_line: false,
        }
    }
}

/// Parses one chunk. `chunk_name` is the name messages show, for example `errors.lua`.
pub fn parse(chunk_name: &str, src: &[u8]) -> Result<Ast, Diagnostic> {
    parse_with(chunk_name, src, &ParseOptions::default())
}

pub fn parse_with(chunk_name: &str, src: &[u8], options: &ParseOptions) -> Result<Ast, Diagnostic> {
    if u32::try_from(src.len()).is_err() {
        return Err(Diagnostic::new(Span::default(), 1, "chunk too large", None));
    }
    let mut lexer = Lexer::new(src);
    if options.skip_comment_line {
        lexer.skip_comment_line();
    }
    let global = lexer.strings_mut().intern(b"global");
    let mut p = Parser {
        lx: lexer,
        tok: Token {
            kind: TokenKind::Eof,
            span: Span::default(),
        },
        ahead: None,
        prev_end: 0,
        depth: 0,
        max_depth: options.max_depth,
        funcs: vec![FuncState {
            vararg: true,
            loops: 0,
        }],
        sym_global: global,
    };
    p.advance()?;
    let body = p.block()?;
    if p.tok.kind != TokenKind::Eof {
        return Err(p.error_expected("<eof>"));
    }
    let span = Span::new(0, src.len() as u32);
    let (strings, line_starts) = p.lx.finish();
    Ok(Ast {
        chunk: Chunk { body, span },
        strings,
        source: SourceInfo {
            name: chunk_name.into(),
            language: Language::Lua,
            line_starts,
        },
    })
}

struct FuncState {
    vararg: bool,
    /// Loops enclosing the current position, within this function.
    loops: u32,
}

struct Parser<'a> {
    lx: Lexer<'a>,
    tok: Token,
    ahead: Option<Token>,
    /// End of the last consumed token.
    prev_end: u32,
    depth: u32,
    max_depth: u32,
    funcs: Vec<FuncState>,
    sym_global: StrId,
}

type PResult<T> = Result<T, Diagnostic>;

fn span_from(start: u32, end: u32) -> Span {
    Span::new(start, end.max(start))
}

impl Parser<'_> {
    // ---- tokens -------------------------------------------------------------------------------

    fn advance(&mut self) -> PResult<()> {
        self.prev_end = self.tok.span.end;
        self.tok = match self.ahead.take() {
            Some(t) => t,
            None => self.lx.next_token()?,
        };
        Ok(())
    }

    fn peek(&mut self) -> PResult<TokenKind> {
        if self.ahead.is_none() {
            self.ahead = Some(self.lx.next_token()?);
        }
        Ok(self.ahead.expect("lookahead").kind)
    }

    fn is_punct(&self, p: Punct) -> bool {
        self.tok.kind == TokenKind::Punct(p)
    }

    fn is_kw(&self, k: Keyword) -> bool {
        self.tok.kind == TokenKind::Keyword(k)
    }

    fn test_punct(&mut self, p: Punct) -> PResult<bool> {
        if self.is_punct(p) {
            self.advance()?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    fn line_of(&self, offset: u32) -> u32 {
        self.lx.line_of(offset)
    }

    /// The line the reference reports for an error at the current token.
    fn cur_line(&self) -> u32 {
        self.line_of(self.tok.span.end)
    }

    // ---- errors -------------------------------------------------------------------------------

    fn near(&self, tok: Token) -> Near {
        match tok.kind {
            TokenKind::Eof => Near::Eof,
            TokenKind::Keyword(k) => Near::Text(k.as_str().as_bytes().to_vec()),
            TokenKind::Punct(p) => Near::Text(p.as_str().as_bytes().to_vec()),
            TokenKind::Other(c) if c.is_ascii_graphic() => Near::Text(vec![c]),
            TokenKind::Other(c) => Near::Text(format!("<\\{c}>").into_bytes()),
            // The reference shows a string as its delimiters around the decoded contents.
            TokenKind::Str(id) => {
                let raw = self.lx.text(tok.span);
                let contents = self.lx.strings().get(id);
                let open = if raw[0] == b'[' {
                    1 + raw[1..].iter().take_while(|&&c| c == b'=').count() + 1
                } else {
                    1
                };
                let mut text = raw[..open].to_vec();
                text.extend_from_slice(contents);
                text.extend_from_slice(&raw[raw.len() - open..]);
                Near::Text(text)
            }
            TokenKind::Name(_) | TokenKind::Int(_) | TokenKind::Float(_) => {
                Near::Text(self.lx.text(tok.span).to_vec())
            }
        }
    }

    /// `message near '<current token>'`.
    fn error_here(&self, message: impl Into<String>) -> Diagnostic {
        Diagnostic::new(
            self.tok.span,
            self.cur_line(),
            message,
            Some(self.near(self.tok)),
        )
    }

    /// `<what> expected near '<current token>'`; `what` is already quoted where Lua quotes it.
    fn error_expected(&self, what: &str) -> Diagnostic {
        self.error_here(format!("{what} expected"))
    }

    /// An error with no `near` part, reported at `span`'s line.
    fn error_at(&self, span: Span, message: impl Into<String>) -> Diagnostic {
        Diagnostic::new(span, self.line_of(span.start), message, None)
    }

    fn expect_punct(&mut self, p: Punct) -> PResult<()> {
        if !self.test_punct(p)? {
            return Err(self.error_expected(&format!("'{}'", p.as_str())));
        }
        Ok(())
    }

    fn expect_kw(&mut self, k: Keyword) -> PResult<()> {
        if !self.is_kw(k) {
            return Err(self.error_expected(&format!("'{}'", k.as_str())));
        }
        self.advance()
    }

    /// Expects the token closing a construct opened by `who` at `open_line`.
    fn expect_match(&mut self, what: TokenKind, who: &str, open_line: u32) -> PResult<()> {
        if self.tok.kind == what {
            return self.advance();
        }
        let what = match what {
            TokenKind::Keyword(k) => k.as_str(),
            TokenKind::Punct(p) => p.as_str(),
            _ => unreachable!("closing token is a keyword or punctuation"),
        };
        if open_line == self.cur_line() {
            Err(self.error_expected(&format!("'{what}'")))
        } else {
            Err(self.error_here(format!(
                "'{what}' expected (to close '{who}' at line {open_line})"
            )))
        }
    }

    fn name(&mut self) -> PResult<Name> {
        match self.tok.kind {
            TokenKind::Name(sym) => {
                let name = Name {
                    sym,
                    span: self.tok.span,
                };
                self.advance()?;
                Ok(name)
            }
            _ => Err(self.error_expected("<name>")),
        }
    }

    fn enter(&mut self) -> PResult<()> {
        self.depth += 1;
        if self.depth > self.max_depth {
            return Err(self.error_at(self.tok.span, "chunk has too many syntax levels"));
        }
        Ok(())
    }

    fn leave(&mut self) {
        self.depth -= 1;
    }

    fn func(&mut self) -> &mut FuncState {
        self.funcs.last_mut().expect("function state")
    }

    // ---- blocks and statements ----------------------------------------------------------------

    fn block_follows(&self) -> bool {
        matches!(
            self.tok.kind,
            TokenKind::Eof
                | TokenKind::Keyword(
                    Keyword::Else | Keyword::Elseif | Keyword::End | Keyword::Until
                )
        )
    }

    fn block(&mut self) -> PResult<Block> {
        self.enter()?;
        let start = self.tok.span.start;
        let mut stmts = Vec::new();
        let mut ret = None;
        while !self.block_follows() {
            if self.is_kw(Keyword::Return) {
                ret = Some(self.return_stat()?);
                break;
            }
            if let Some(stmt) = self.statement()? {
                stmts.push(stmt);
            }
        }
        self.leave();
        Ok(Block {
            stmts,
            ret,
            span: span_from(start, self.prev_end),
        })
    }

    fn loop_block(&mut self) -> PResult<Block> {
        self.func().loops += 1;
        let body = self.block();
        self.func().loops -= 1;
        body
    }

    fn return_stat(&mut self) -> PResult<Return> {
        let start = self.tok.span.start;
        self.advance()?;
        let values = if self.block_follows() || self.is_punct(Punct::Semi) {
            Vec::new()
        } else {
            self.expr_list()?
        };
        self.test_punct(Punct::Semi)?;
        Ok(Return {
            values,
            span: span_from(start, self.prev_end),
        })
    }

    fn statement(&mut self) -> PResult<Option<Stmt>> {
        let start = self.tok.span.start;
        let line = self.cur_line();
        let global_decl =
            self.tok.kind == TokenKind::Name(self.sym_global) && self.starts_global_decl()?;
        let kind = match self.tok.kind {
            TokenKind::Punct(Punct::Semi) => {
                self.advance()?;
                return Ok(None);
            }
            TokenKind::Keyword(Keyword::If) => self.if_stat(line)?,
            TokenKind::Keyword(Keyword::While) => {
                self.advance()?;
                let cond = self.expr()?;
                self.expect_kw(Keyword::Do)?;
                let body = self.loop_block()?;
                self.expect_match(TokenKind::Keyword(Keyword::End), "while", line)?;
                StmtKind::While { cond, body }
            }
            TokenKind::Keyword(Keyword::Do) => {
                self.advance()?;
                let body = self.block()?;
                self.expect_match(TokenKind::Keyword(Keyword::End), "do", line)?;
                StmtKind::Do(body)
            }
            TokenKind::Keyword(Keyword::For) => self.for_stat(line)?,
            TokenKind::Keyword(Keyword::Repeat) => {
                self.advance()?;
                let body = self.loop_block()?;
                self.expect_match(TokenKind::Keyword(Keyword::Until), "repeat", line)?;
                let until = self.expr()?;
                StmtKind::Repeat { body, until }
            }
            TokenKind::Keyword(Keyword::Function) => {
                self.advance()?;
                let base = self.name()?;
                let mut fields = Vec::new();
                while self.test_punct(Punct::Dot)? {
                    fields.push(self.name()?);
                }
                let method = if self.test_punct(Punct::Colon)? {
                    Some(self.name()?)
                } else {
                    None
                };
                let func = self.func_body(start, line)?;
                StmtKind::Function {
                    path: FuncPath {
                        base,
                        fields,
                        method,
                    },
                    func,
                }
            }
            TokenKind::Keyword(Keyword::Local) => {
                self.advance()?;
                if self.is_kw(Keyword::Function) {
                    self.advance()?;
                    let name = self.name()?;
                    let func = self.func_body(start, line)?;
                    StmtKind::LocalFunction { name, func }
                } else {
                    let attrib = self.opt_attrib()?;
                    let (names, values) = self.att_names_and_values(attrib, true)?;
                    StmtKind::Local {
                        attrib,
                        names,
                        values,
                    }
                }
            }
            TokenKind::Punct(Punct::ColonColon) => {
                self.advance()?;
                let name = self.name()?;
                self.expect_punct(Punct::ColonColon)?;
                StmtKind::Label(name)
            }
            TokenKind::Keyword(Keyword::Break) => {
                if self.func().loops == 0 {
                    return Err(self.error_here("break outside loop"));
                }
                self.advance()?;
                StmtKind::Break
            }
            TokenKind::Keyword(Keyword::Goto) => {
                self.advance()?;
                StmtKind::Goto(self.name()?)
            }
            TokenKind::Name(_) if global_decl => self.global_stat(start, line)?,
            _ => self.expr_stat()?,
        };
        Ok(Some(Stmt {
            kind,
            span: span_from(start, self.prev_end),
        }))
    }

    /// Whether the `global` at the current token starts a declaration rather than being used as
    /// a name (it is contextual in the default 5.5 build).
    fn starts_global_decl(&mut self) -> PResult<bool> {
        Ok(matches!(
            self.peek()?,
            TokenKind::Name(_)
                | TokenKind::Keyword(Keyword::Function)
                | TokenKind::Punct(Punct::Lt | Punct::Star)
        ))
    }

    fn global_stat(&mut self, start: u32, line: u32) -> PResult<StmtKind> {
        self.advance()?;
        if self.is_kw(Keyword::Function) {
            self.advance()?;
            let name = self.name()?;
            let func = self.func_body(start, line)?;
            return Ok(StmtKind::GlobalFunction { name, func });
        }
        let attrib = self.opt_attrib()?;
        if self.test_punct(Punct::Star)? {
            return Ok(StmtKind::GlobalAll { attrib });
        }
        let (names, values) = self.att_names_and_values(attrib, false)?;
        Ok(StmtKind::Global {
            attrib,
            names,
            values,
        })
    }

    fn if_stat(&mut self, line: u32) -> PResult<StmtKind> {
        let mut arms = Vec::new();
        self.advance()?;
        let cond = self.expr()?;
        self.expect_kw(Keyword::Then)?;
        arms.push((cond, self.block()?));
        let mut else_ = None;
        loop {
            if self.is_kw(Keyword::Elseif) {
                self.advance()?;
                let cond = self.expr()?;
                self.expect_kw(Keyword::Then)?;
                arms.push((cond, self.block()?));
            } else {
                if self.is_kw(Keyword::Else) {
                    self.advance()?;
                    else_ = Some(self.block()?);
                }
                break;
            }
        }
        self.expect_match(TokenKind::Keyword(Keyword::End), "if", line)?;
        Ok(StmtKind::If { arms, else_ })
    }

    fn for_stat(&mut self, line: u32) -> PResult<StmtKind> {
        self.advance()?;
        let first = self.name()?;
        let kind = match self.tok.kind {
            TokenKind::Punct(Punct::Assign) => {
                self.advance()?;
                let start = self.expr()?;
                self.expect_punct(Punct::Comma)?;
                let limit = self.expr()?;
                let step = if self.test_punct(Punct::Comma)? {
                    Some(self.expr()?)
                } else {
                    None
                };
                self.expect_kw(Keyword::Do)?;
                let body = self.loop_block()?;
                StmtKind::NumericFor {
                    var: first,
                    start,
                    limit,
                    step,
                    body,
                }
            }
            TokenKind::Punct(Punct::Comma) | TokenKind::Keyword(Keyword::In) => {
                let mut vars = vec![first];
                while self.test_punct(Punct::Comma)? {
                    vars.push(self.name()?);
                }
                self.expect_kw(Keyword::In)?;
                let exprs = self.expr_list()?;
                self.expect_kw(Keyword::Do)?;
                let body = self.loop_block()?;
                StmtKind::GenericFor { vars, exprs, body }
            }
            _ => return Err(self.error_expected("'=' or 'in'")),
        };
        self.expect_match(TokenKind::Keyword(Keyword::End), "for", line)?;
        Ok(kind)
    }

    /// An optional `<name>` attribute.
    fn opt_attrib(&mut self) -> PResult<Option<Attrib>> {
        if !self.is_punct(Punct::Lt) {
            return Ok(None);
        }
        let start = self.tok.span.start;
        self.advance()?;
        let name = self.name()?;
        self.expect_punct(Punct::Gt)?;
        let kind = match self.lx.strings().get(name.sym) {
            b"const" => AttribKind::Const,
            b"close" => AttribKind::Close,
            other => {
                let message = format!("unknown attribute '{}'", String::from_utf8_lossy(other));
                return Err(self.error_at(name.span, message));
            }
        };
        Ok(Some(Attrib {
            kind,
            span: span_from(start, self.prev_end),
        }))
    }

    /// `Name [attrib] {',' Name [attrib]} ['=' explist]`, after an optional prefix attribute.
    fn att_names_and_values(
        &mut self,
        prefix: Option<Attrib>,
        is_local: bool,
    ) -> PResult<(Vec<AttName>, Vec<Expr>)> {
        let mut names = Vec::new();
        let mut closes = usize::from(prefix.is_some_and(|a| a.kind == AttribKind::Close));
        loop {
            let name = self.name()?;
            let attrib = self.opt_attrib()?;
            if attrib.is_some_and(|a| a.kind == AttribKind::Close) {
                closes += 1;
            }
            names.push(AttName { name, attrib });
            if !self.test_punct(Punct::Comma)? {
                break;
            }
        }
        if is_local && (closes > 1 || (closes == 1 && prefix.is_some() && names.len() > 1)) {
            let span = span_from(names[0].name.span.start, self.prev_end);
            return Err(self.error_at(span, "multiple to-be-closed variables in local list"));
        }
        let values = if self.test_punct(Punct::Assign)? {
            self.expr_list()?
        } else {
            Vec::new()
        };
        Ok((names, values))
    }

    fn expr_stat(&mut self) -> PResult<StmtKind> {
        let first = self.suffixed_expr()?;
        if self.is_punct(Punct::Assign) || self.is_punct(Punct::Comma) {
            self.check_assignable(&first)?;
            let mut targets = vec![first];
            while self.test_punct(Punct::Comma)? {
                let target = self.suffixed_expr()?;
                self.check_assignable(&target)?;
                targets.push(target);
            }
            self.expect_punct(Punct::Assign)?;
            let values = self.expr_list()?;
            Ok(StmtKind::Assign { targets, values })
        } else {
            match first.kind {
                ExprKind::Call { .. } | ExprKind::Method { .. } => Ok(StmtKind::Call(first)),
                _ => Err(self.error_here("syntax error")),
            }
        }
    }

    fn check_assignable(&self, target: &Expr) -> PResult<()> {
        match target.kind {
            ExprKind::Name(_) | ExprKind::Index { .. } => Ok(()),
            _ => Err(self.error_here("syntax error")),
        }
    }

    /// `'(' [parlist] ')' block end`. `start` and `line` are those of the `function` keyword
    /// (or `local`/`global` before it).
    fn func_body(&mut self, start: u32, line: u32) -> PResult<FuncBody> {
        self.expect_punct(Punct::LParen)?;
        let mut params = Vec::new();
        let mut vararg = None;
        if !self.is_punct(Punct::RParen) {
            loop {
                match self.tok.kind {
                    TokenKind::Name(_) => params.push(self.name()?),
                    TokenKind::Punct(Punct::Ellipsis) => {
                        let dots = self.tok.span;
                        self.advance()?;
                        let name = match self.tok.kind {
                            TokenKind::Name(_) => Some(self.name()?),
                            _ => None,
                        };
                        vararg = Some(VarargParam {
                            name,
                            span: span_from(dots.start, self.prev_end),
                        });
                        break;
                    }
                    _ => return Err(self.error_expected("<name> or '...'")),
                }
                if !self.test_punct(Punct::Comma)? {
                    break;
                }
            }
        }
        self.expect_punct(Punct::RParen)?;
        self.funcs.push(FuncState {
            vararg: vararg.is_some(),
            loops: 0,
        });
        let body = self.block();
        self.funcs.pop();
        let body = body?;
        self.expect_match(TokenKind::Keyword(Keyword::End), "function", line)?;
        Ok(FuncBody {
            params,
            vararg,
            body,
            span: span_from(start, self.prev_end),
        })
    }

    // ---- expressions --------------------------------------------------------------------------

    fn expr_list(&mut self) -> PResult<Vec<Expr>> {
        let mut list = vec![self.expr()?];
        while self.test_punct(Punct::Comma)? {
            list.push(self.expr()?);
        }
        Ok(list)
    }

    pub(crate) fn expr(&mut self) -> PResult<Expr> {
        self.subexpr(0)
    }

    fn unary_op(&self) -> Option<UnOp> {
        Some(match self.tok.kind {
            TokenKind::Keyword(Keyword::Not) => UnOp::Not,
            TokenKind::Punct(Punct::Minus) => UnOp::Neg,
            TokenKind::Punct(Punct::Hash) => UnOp::Len,
            TokenKind::Punct(Punct::Tilde) => UnOp::BNot,
            _ => return None,
        })
    }

    fn binary_op(&self) -> Option<BinOp> {
        Some(match self.tok.kind {
            TokenKind::Keyword(Keyword::Or) => BinOp::Or,
            TokenKind::Keyword(Keyword::And) => BinOp::And,
            TokenKind::Punct(p) => match p {
                Punct::Lt => BinOp::Lt,
                Punct::Gt => BinOp::Gt,
                Punct::LtEq => BinOp::Le,
                Punct::GtEq => BinOp::Ge,
                Punct::NotEq => BinOp::Ne,
                Punct::EqEq => BinOp::Eq,
                Punct::Pipe => BinOp::BOr,
                Punct::Tilde => BinOp::BXor,
                Punct::Amp => BinOp::BAnd,
                Punct::Shl => BinOp::Shl,
                Punct::Shr => BinOp::Shr,
                Punct::DotDot => BinOp::Concat,
                Punct::Plus => BinOp::Add,
                Punct::Minus => BinOp::Sub,
                Punct::Star => BinOp::Mul,
                Punct::Slash => BinOp::Div,
                Punct::SlashSlash => BinOp::IDiv,
                Punct::Percent => BinOp::Mod,
                Punct::Caret => BinOp::Pow,
                _ => return None,
            },
            _ => return None,
        })
    }

    /// Precedence climbing: parses operators whose left binding power exceeds `limit`.
    fn subexpr(&mut self, limit: u8) -> PResult<Expr> {
        self.enter()?;
        let start = self.tok.span.start;
        let mut lhs = if let Some(op) = self.unary_op() {
            self.advance()?;
            let expr = self.subexpr(UNARY_POWER)?;
            Expr {
                kind: ExprKind::Unary {
                    op,
                    expr: Box::new(expr),
                },
                span: span_from(start, self.prev_end),
            }
        } else {
            self.simple_expr()?
        };
        while let Some(op) = self.binary_op() {
            let (left, right) = op.binding_power();
            if left <= limit {
                break;
            }
            self.advance()?;
            let rhs = self.subexpr(right)?;
            lhs = Expr {
                kind: ExprKind::Binary {
                    op,
                    lhs: Box::new(lhs),
                    rhs: Box::new(rhs),
                },
                span: span_from(start, self.prev_end),
            };
        }
        self.leave();
        Ok(lhs)
    }

    fn simple_expr(&mut self) -> PResult<Expr> {
        let span = self.tok.span;
        let kind = match self.tok.kind {
            TokenKind::Int(n) => ExprKind::Int(n),
            TokenKind::Float(x) => ExprKind::Float(x),
            TokenKind::Str(s) => ExprKind::Str(s),
            TokenKind::Keyword(Keyword::Nil) => ExprKind::Nil,
            TokenKind::Keyword(Keyword::True) => ExprKind::True,
            TokenKind::Keyword(Keyword::False) => ExprKind::False,
            TokenKind::Punct(Punct::Ellipsis) => {
                if !self.func().vararg {
                    return Err(self.error_here("cannot use '...' outside a vararg function"));
                }
                ExprKind::Vararg
            }
            TokenKind::Punct(Punct::LBrace) => return self.table(),
            TokenKind::Keyword(Keyword::Function) => {
                let line = self.cur_line();
                self.advance()?;
                let body = self.func_body(span.start, line)?;
                let span = body.span;
                return Ok(Expr {
                    kind: ExprKind::Function(Box::new(body)),
                    span,
                });
            }
            _ => return self.suffixed_expr(),
        };
        self.advance()?;
        Ok(Expr { kind, span })
    }

    fn primary_expr(&mut self) -> PResult<Expr> {
        let span = self.tok.span;
        match self.tok.kind {
            TokenKind::Name(sym) => {
                self.advance()?;
                Ok(Expr {
                    kind: ExprKind::Name(sym),
                    span,
                })
            }
            TokenKind::Punct(Punct::LParen) => {
                let line = self.cur_line();
                self.advance()?;
                let inner = self.expr()?;
                self.expect_match(TokenKind::Punct(Punct::RParen), "(", line)?;
                Ok(Expr {
                    kind: ExprKind::Paren(Box::new(inner)),
                    span: span_from(span.start, self.prev_end),
                })
            }
            _ => Err(self.error_here("unexpected symbol")),
        }
    }

    fn suffixed_expr(&mut self) -> PResult<Expr> {
        let start = self.tok.span.start;
        let mut e = self.primary_expr()?;
        loop {
            let kind = match self.tok.kind {
                TokenKind::Punct(Punct::Dot) => {
                    self.advance()?;
                    let name = self.name()?;
                    let key = Expr {
                        kind: ExprKind::Str(name.sym),
                        span: name.span,
                    };
                    ExprKind::Index {
                        obj: Box::new(e),
                        key: Box::new(key),
                    }
                }
                TokenKind::Punct(Punct::LBracket) => {
                    self.advance()?;
                    let key = self.expr()?;
                    self.expect_punct(Punct::RBracket)?;
                    ExprKind::Index {
                        obj: Box::new(e),
                        key: Box::new(key),
                    }
                }
                TokenKind::Punct(Punct::Colon) => {
                    self.advance()?;
                    let name = self.name()?;
                    let args = self.call_args()?;
                    ExprKind::Method {
                        obj: Box::new(e),
                        name,
                        args,
                    }
                }
                TokenKind::Punct(Punct::LParen | Punct::LBrace) | TokenKind::Str(_) => {
                    let args = self.call_args()?;
                    ExprKind::Call {
                        callee: Box::new(e),
                        args,
                    }
                }
                _ => return Ok(e),
            };
            e = Expr {
                kind,
                span: span_from(start, self.prev_end),
            };
        }
    }

    fn call_args(&mut self) -> PResult<Vec<Expr>> {
        match self.tok.kind {
            TokenKind::Str(s) => {
                let span = self.tok.span;
                self.advance()?;
                Ok(vec![Expr {
                    kind: ExprKind::Str(s),
                    span,
                }])
            }
            TokenKind::Punct(Punct::LBrace) => Ok(vec![self.table()?]),
            TokenKind::Punct(Punct::LParen) => {
                let line = self.cur_line();
                self.advance()?;
                let args = if self.is_punct(Punct::RParen) {
                    Vec::new()
                } else {
                    self.expr_list()?
                };
                self.expect_match(TokenKind::Punct(Punct::RParen), "(", line)?;
                Ok(args)
            }
            _ => Err(self.error_here("function arguments expected")),
        }
    }

    fn table(&mut self) -> PResult<Expr> {
        let start = self.tok.span.start;
        let line = self.cur_line();
        self.advance()?;
        let mut fields = Vec::new();
        while !self.is_punct(Punct::RBrace) {
            fields.push(self.field()?);
            if !self.test_punct(Punct::Comma)? && !self.test_punct(Punct::Semi)? {
                break;
            }
        }
        self.expect_match(TokenKind::Punct(Punct::RBrace), "{", line)?;
        Ok(Expr {
            kind: ExprKind::Table(fields),
            span: span_from(start, self.prev_end),
        })
    }

    fn field(&mut self) -> PResult<Field> {
        let named = matches!(self.tok.kind, TokenKind::Name(_))
            && self.peek()? == TokenKind::Punct(Punct::Assign);
        match self.tok.kind {
            TokenKind::Name(_) if named => {
                let name = self.name()?;
                self.advance()?;
                Ok(Field::Named(name, self.expr()?))
            }
            TokenKind::Punct(Punct::LBracket) => {
                self.advance()?;
                let key = self.expr()?;
                self.expect_punct(Punct::RBracket)?;
                self.expect_punct(Punct::Assign)?;
                Ok(Field::Keyed(key, self.expr()?))
            }
            _ => Ok(Field::Positional(self.expr()?)),
        }
    }
}
