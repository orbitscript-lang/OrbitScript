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

/// Parser settings. The defaults are what the runtime uses; the other flags are for tools
/// (`orbit-check`, a language server) and never change how a program runs.
#[derive(Clone, Debug)]
pub struct ParseOptions {
    /// Maximum nesting of blocks and expressions, so deeply nested input can't overflow the
    /// Rust stack.
    pub max_depth: u32,
    /// Build a [`TypeKind`] tree for every type annotation (otherwise only spans are kept).
    pub type_trees: bool,
    /// Parse a stub (`.orbi`): `fn` items and `impl` functions may end in `;` instead of a body,
    /// and `const NAME: T;` needs no value. A stub can't be lowered.
    pub stub: bool,
    /// Keep `///` and `//!` comments ([`Ast::comments`], [`Item::docs`]).
    pub doc_comments: bool,
    /// Keep every comment in [`Ast::comments`] (for a formatter or an editor).
    pub keep_trivia: bool,
    /// Go on after a syntax error, for an editor: the error is recorded, the broken statement
    /// becomes an `Error` node and parsing resumes at the next statement. Use
    /// [`parse_recovering`] to get every error; [`parse_with`] ignores it and gives only the
    /// first.
    pub recover: bool,
}

impl Default for ParseOptions {
    fn default() -> ParseOptions {
        ParseOptions {
            max_depth: 200,
            type_trees: false,
            stub: false,
            doc_comments: false,
            keep_trivia: false,
            recover: false,
        }
    }
}

/// Parses one module. `chunk_name` is the name messages show, for example `main.orb`.
pub fn parse(chunk_name: &str, src: &[u8]) -> Result<Ast, Diagnostic> {
    parse_with(chunk_name, src, &ParseOptions::default())
}

pub fn parse_with(chunk_name: &str, src: &[u8], options: &ParseOptions) -> Result<Ast, Diagnostic> {
    let plain;
    let options = if options.recover {
        plain = ParseOptions {
            recover: false,
            ..options.clone()
        };
        &plain
    } else {
        options
    };
    parse_module(chunk_name, src, options).map(|(ast, _)| ast)
}

/// Parses one module, going on after syntax errors ([`ParseOptions::recover`]): the AST
/// holds everything that parsed, with `Error` nodes where something didn't, and the errors
/// come in source order. A module without errors gets the AST [`parse_with`] gives.
pub fn parse_recovering(
    chunk_name: &str,
    src: &[u8],
    options: &ParseOptions,
) -> (Ast, Vec<Diagnostic>) {
    // Only a module with errors is parsed again with recovery, whose layout heuristics (an
    // item keyword at column 0 ends a block, for one) could misread valid code.
    if let Ok(ast) = parse_with(chunk_name, src, options) {
        return (ast, Vec::new());
    }
    let options = ParseOptions {
        recover: true,
        ..options.clone()
    };
    match parse_module(chunk_name, src, &options) {
        Ok(result) => result,
        // Only a source too large to parse at all gets here.
        Err(d) => {
            let (strings, source) = Lexer::new(chunk_name, b"").finish();
            let body = Block {
                stmts: Vec::new(),
                tail: None,
                span: Span::default(),
            };
            let ast = Ast {
                body,
                strings,
                source,
                comments: Vec::new(),
            };
            (ast, vec![d])
        }
    }
}

fn parse_module(
    chunk_name: &str,
    src: &[u8],
    options: &ParseOptions,
) -> Result<(Ast, Vec<Diagnostic>), Diagnostic> {
    if u32::try_from(src.len()).is_err() {
        return Err(Diagnostic::new(Span::default(), 1, "chunk too large"));
    }
    let mut lx = Lexer::new(chunk_name, src);
    let sym_scope = lx.strings_mut().intern(b"scope");
    let sym_underscore = lx.strings_mut().intern(b"_");
    let sym_type = lx.strings_mut().intern(b"type");
    if options.doc_comments || options.keep_trivia {
        lx.keep_comments(options.keep_trivia);
    }
    let mut p = Parser {
        lx,
        tok: Token {
            kind: TokenKind::Eof,
            span: Span::default(),
        },
        ahead: None,
        prev_end: 0,
        depth: 0,
        result_parens: false,
        max_depth: options.max_depth,
        type_trees: options.type_trees,
        stub: options.stub,
        recover: options.recover,
        errors: Vec::new(),
        lex_error_at: None,
        next_doc: 0,
        sym_scope,
        sym_underscore,
        sym_type,
    };
    p.advance()?;
    let start = p.tok.span.start;
    let (stmts, tail) = p.block_body(None)?;
    let body = Block {
        stmts,
        tail,
        span: Span::new(start.min(p.tok.span.end), src.len() as u32),
    };
    let comments = p.lx.take_comments();
    let errors = std::mem::take(&mut p.errors);
    let (strings, source) = p.lx.finish();
    let ast = Ast {
        body,
        strings,
        source,
        comments,
    };
    Ok((ast, errors))
}

/// What [`Parser::synchronize`] skips after an error.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Sync {
    Statement,
    /// A member of an `impl`: up to the next `fn`, `pub` or `const` starting a line, not
    /// the next `let` or `;` (which may be in the member's body).
    Member,
    /// The rest of a line.
    Line,
    /// A match arm.
    Arm,
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
    /// The next type is a function's result, where a tuple may end with `...T`.
    result_parens: bool,
    max_depth: u32,
    type_trees: bool,
    stub: bool,
    recover: bool,
    /// The errors recovered from so far (only with `recover`).
    errors: Vec<Diagnostic>,
    /// Where the last lexer error recovered from was.
    lex_error_at: Option<u32>,
    /// The first doc comment not yet attached to (or passed by) an item.
    next_doc: usize,
    sym_scope: StrId,
    sym_underscore: StrId,
    sym_type: StrId,
}

type PResult<T> = Result<T, Diagnostic>;

/// Keywords that start a statement or item: when recovering from an error, one at the start
/// of a line is where the next statement most likely begins. The first `ITEM_KEYWORDS` of
/// them can't appear inside an expression, so they end a broken statement even inside an
/// unclosed bracket.
const STATEMENT_KEYWORDS: &[Keyword] = &[
    Keyword::Let,
    Keyword::Fn,
    Keyword::Pub,
    Keyword::Struct,
    Keyword::Enum,
    Keyword::Impl,
    Keyword::Use,
    Keyword::Const,
    Keyword::Global,
    Keyword::Extern,
    // ITEM_KEYWORDS ends here.
    Keyword::Return,
    Keyword::Break,
    Keyword::Continue,
    Keyword::If,
    Keyword::While,
    Keyword::For,
    Keyword::Loop,
    Keyword::Match,
];
const ITEM_KEYWORDS: usize = 10;

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
            None => self.lex()?,
        };
        Ok(())
    }

    #[inline(never)]
    fn peek(&mut self) -> PResult<TokenKind> {
        if self.ahead.is_none() {
            self.ahead = Some(self.lex()?);
        }
        Ok(self.ahead.expect("lookahead").kind)
    }

    /// The next token from the lexer. When recovering, a lexer error is recorded and the bad
    /// token skipped or replaced.
    fn lex(&mut self) -> PResult<Token> {
        loop {
            match self.lx.next_token() {
                Ok(t) => return Ok(t),
                Err(d) if self.recover => {
                    self.lex_error_at = Some(d.span.start);
                    self.report(d);
                    if let Some(t) = self.lx.skip_bad_token() {
                        return Ok(t);
                    }
                }
                Err(d) => return Err(d),
            }
        }
    }

    /// Records an error recovered from, unless one was already reported at the same place.
    #[inline(never)]
    fn report(&mut self, d: Diagnostic) {
        if self
            .errors
            .last()
            .is_none_or(|e| e.span.start != d.span.start)
        {
            self.errors.push(d);
        }
    }

    /// Records a parse error recovered from, unless it comes right after a lexer error
    /// (which most likely caused it).
    fn recovered(&mut self, d: Diagnostic) {
        if !self.lex_error_at.is_some_and(|at| at >= self.prev_end) {
            self.report(d);
        }
    }

    /// Records the error that ended a statement or member starting at `start`, unless it
    /// follows a lexer error in the same statement (it is most likely caused by it).
    fn report_in(&mut self, d: Diagnostic, start: u32) {
        if !self.lex_error_at.is_some_and(|at| at >= start) {
            self.report(d);
        }
    }

    /// Skips the rest of something broken that started at `start`, after an error: through
    /// a `;` (or a `,` ending a match arm) or a `}` that closes a bracket opened in it, or up
    /// to a `}` closing the enclosing block (`braced`), a statement keyword that starts a line
    /// (for a line or an arm, any token that does), a line indented no deeper than the start,
    /// or the end of input. Always moves on at least one token.
    fn synchronize(&mut self, start: u32, braced: bool, sync: Sync) {
        let mut moved = self.tok.span.start != start;
        // Brackets opened between `start` and the error are still open.
        let mut depth = if moved && sync != Sync::Line {
            self.lx.open_brackets(Span::new(start, self.tok.span.start))
        } else {
            0
        };
        let indent = (sync != Sync::Line).then(|| self.line_indent(start));
        loop {
            // Indentation outlasts brackets the error left unbalanced.
            if moved
                && let Some(indent) = indent
                && self.starts_line()
            {
                let col = self.line_indent(self.tok.span.start);
                match self.tok.kind {
                    _ if col > indent => {}
                    TokenKind::Punct(Punct::RParen | Punct::RBracket) => {}
                    TokenKind::Punct(Punct::RBrace) if !braced || col == indent => {}
                    _ => return,
                }
            }
            match self.tok.kind {
                TokenKind::Eof => return,
                TokenKind::Punct(Punct::Semi) if depth == 0 && sync != Sync::Member => {
                    let _ = self.advance();
                    return;
                }
                TokenKind::Punct(Punct::Comma) if depth == 0 && sync == Sync::Arm => {
                    let _ = self.advance();
                    return;
                }
                kind if moved
                    && match kind {
                        TokenKind::Keyword(k) if sync == Sync::Member => {
                            matches!(k, Keyword::Fn | Keyword::Pub | Keyword::Const)
                        }
                        TokenKind::Keyword(k) => STATEMENT_KEYWORDS
                            .iter()
                            .position(|&s| s == k)
                            .is_some_and(|i| depth == 0 || i < ITEM_KEYWORDS),
                        _ => depth == 0 && matches!(sync, Sync::Line | Sync::Arm),
                    }
                    && self.starts_line() =>
                {
                    return;
                }
                TokenKind::Punct(
                    Punct::LParen | Punct::LBracket | Punct::LBrace | Punct::HashBrace,
                ) => depth += 1,
                TokenKind::Punct(Punct::RParen | Punct::RBracket) => {
                    depth = depth.saturating_sub(1);
                }
                TokenKind::Punct(Punct::RBrace) if depth > 0 => {
                    depth -= 1;
                    // A block at the end of a line ends a statement (in an arm, it may be a
                    // pattern's).
                    if depth == 0 && matches!(sync, Sync::Statement | Sync::Member) {
                        let _ = self.advance();
                        if self.starts_line() {
                            return;
                        }
                        continue;
                    }
                }
                TokenKind::Punct(Punct::RBrace) if braced && moved => return,
                _ => {}
            }
            // Lexing can't fail while recovering.
            let _ = self.advance();
            moved = true;
        }
    }

    /// Whether the current token is the first on its line.
    fn starts_line(&self) -> bool {
        let src = self.lx.src();
        let mut i = self.tok.span.start as usize;
        while i > 0 && matches!(src[i - 1], b' ' | b'\t') {
            i -= 1;
        }
        i == 0 || matches!(src[i - 1], b'\n' | b'\r')
    }

    /// How far the line holding `offset` is indented (spaces and tabs, one column each).
    fn line_indent(&self, offset: u32) -> usize {
        let src = self.lx.src();
        let mut i = (offset as usize).min(src.len());
        while i > 0 && !matches!(src[i - 1], b'\n' | b'\r') {
            i -= 1;
        }
        src[i..]
            .iter()
            .take_while(|&&c| c == b' ' || c == b'\t')
            .count()
    }

    /// When recovering, whether the current token starts an item at the very start of a
    /// line: inside a block that means its `}` is most likely missing.
    fn item_at_line_start(&self) -> bool {
        let at = self.tok.span.start as usize;
        self.recover
            && matches!(
                self.tok.kind,
                TokenKind::Keyword(
                    Keyword::Fn
                        | Keyword::Pub
                        | Keyword::Struct
                        | Keyword::Enum
                        | Keyword::Impl
                        | Keyword::Use
                        | Keyword::Const
                        | Keyword::Global
                        | Keyword::Extern
                )
            )
            && (at == 0 || self.lx.src()[at - 1] == b'\n')
    }

    /// When recovering, whether the `{` at `open` is missing its `}` because an item starts
    /// a line here; reports it if so.
    fn unclosed_before_item(&mut self, open: u32) -> bool {
        let missing = self.item_at_line_start();
        if missing {
            let d = self.unclosed(open);
            self.recovered(d);
        }
        missing
    }

    /// The error for a `{` (at `open`) that is never closed.
    fn unclosed(&self, open: u32) -> Diagnostic {
        self.expected(&format!(
            "`}}` to close the `{{` at line {}",
            self.lx.line_of(open)
        ))
    }

    /// `;` ending a statement or item. When recovering, a missing `;` is recorded and
    /// parsing goes on as if it were there.
    #[inline(never)]
    fn semi(&mut self) -> PResult<()> {
        if self.is(Punct::Semi) {
            return self.advance();
        }
        let d = self.expected("`;`");
        if !self.recover {
            return Err(d);
        }
        self.recovered(d);
        // On the same line, what follows is most likely more of the broken statement.
        if !self.starts_line() {
            self.synchronize(u32::MAX, true, Sync::Line);
        }
        Ok(())
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
        let d = self.error(
            self.tok.span,
            format!("expected {what}, found {}", self.describe(self.tok.kind)),
        );
        if self.tok.kind == TokenKind::Eof && self.lx.at_source_end() {
            return d.at_eof();
        }
        d
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

    /// The contextual keyword `type` (not `r#type`).
    fn is_type_keyword(&self) -> bool {
        self.is_ident(self.sym_type) && self.tok.span.end - self.tok.span.start == 4
    }

    // ---- blocks and statements ----------------------------------------------------------------

    /// `{ ... }`.
    fn block(&mut self) -> PResult<Block> {
        self.enter()?;
        let start = self.expect(Punct::LBrace)?.start;
        let (stmts, tail) = self.block_body(Some(start))?;
        // When recovering, `block_body` may stop early and report the missing `}` itself.
        if !self.recover || self.is(Punct::RBrace) {
            self.expect(Punct::RBrace)?;
        }
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
                        let d = self.unclosed(open);
                        if !self.recover {
                            return Err(d);
                        }
                        self.recovered(d);
                    }
                    return Ok((stmts, None));
                }
                TokenKind::Punct(Punct::Semi) => self.advance()?,
                _ if open.is_some() && self.item_at_line_start() => {
                    let d = self.unclosed(open.unwrap_or_default());
                    self.recovered(d);
                    return Ok((stmts, None));
                }
                _ => {
                    let start = self.tok.span.start;
                    let depth = self.depth;
                    let err = match self.statement() {
                        Ok(StmtOrTail::Stmt(s)) => {
                            stmts.push(s);
                            continue;
                        }
                        Ok(StmtOrTail::Tail(e)) => {
                            let at_end = match open {
                                Some(_) => self.is(Punct::RBrace),
                                None => self.tok.kind == TokenKind::Eof,
                            };
                            if at_end {
                                return Ok((stmts, Some(Box::new(e))));
                            }
                            let d = self.expected("`;`");
                            if self.recover && self.tok.span.start != start {
                                // A missing `;`: keep the expression and go on.
                                self.recovered(d);
                                let span = e.span;
                                stmts.push(Stmt {
                                    kind: StmtKind::Expr(e),
                                    span,
                                });
                                continue;
                            }
                            d
                        }
                        Err(d) => d,
                    };
                    if !self.recover {
                        return Err(err);
                    }
                    self.depth = depth;
                    self.report_in(err, start);
                    self.synchronize(start, open.is_some(), Sync::Statement);
                    stmts.push(Stmt {
                        kind: StmtKind::Error,
                        span: self.span_from(start),
                    });
                }
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
                    kind: StmtKind::Item(Box::new(item)),
                }));
            }
            TokenKind::Ident(_) if self.is_type_keyword() && self.type_is_keyword()? => {
                let item = self.item()?;
                return Ok(StmtOrTail::Stmt(Stmt {
                    span: item.span,
                    kind: StmtKind::Item(Box::new(item)),
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
                self.semi()?;
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
        self.semi()?;
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
        let docs = self.item_docs();
        let is_pub = self.eat_kw(Keyword::Pub)?;
        let kind = match self.tok.kind {
            TokenKind::Ident(_) if self.is_type_keyword() => self.type_alias()?,
            TokenKind::Keyword(Keyword::Fn) => ItemKind::Fn(self.fn_decl(false)?),
            TokenKind::Keyword(Keyword::Struct) => ItemKind::Struct(self.struct_decl()?),
            TokenKind::Keyword(Keyword::Enum) => ItemKind::Enum(self.enum_decl()?),
            TokenKind::Keyword(Keyword::Impl) if !is_pub => ItemKind::Impl(self.impl_decl()?),
            TokenKind::Keyword(Keyword::Use) if !is_pub => ItemKind::Use(self.use_decl()?),
            TokenKind::Keyword(Keyword::Const) => {
                self.advance()?;
                let name = self.name()?;
                self.const_rest(name)?
            }
            TokenKind::Keyword(Keyword::Global) if !is_pub => {
                self.advance()?;
                let name = self.name()?;
                self.expect(Punct::Assign)?;
                let value = self.expr()?;
                self.semi()?;
                ItemKind::Global { name, value }
            }
            TokenKind::Keyword(Keyword::Extern) if !is_pub => {
                self.advance()?;
                let mut names = vec![self.name()?];
                while self.eat(Punct::Comma)? {
                    names.push(self.name()?);
                }
                self.semi()?;
                ItemKind::Extern(names)
            }
            _ if is_pub => {
                return Err(self.expected("`fn`, `struct`, `enum`, `const` or `type` after `pub`"));
            }
            _ => return Err(self.expected("an item")),
        };
        Ok(Item {
            is_pub,
            kind,
            span: self.span_from(start),
            docs,
        })
    }

    /// `[: T] = value;` after `const NAME`; a stub may write `: T;` with no value.
    fn const_rest(&mut self, name: Name) -> PResult<ItemKind> {
        let ty = if self.eat(Punct::Colon)? {
            Some(self.type_ann()?)
        } else {
            None
        };
        let value = if self.stub && ty.is_some() && self.is(Punct::Semi) {
            None
        } else {
            self.expect(Punct::Assign)?;
            Some(self.expr()?)
        };
        self.semi()?;
        Ok(ItemKind::Const { name, ty, value })
    }

    /// `type Name<T> = T;` (the current token is `type`).
    fn type_alias(&mut self) -> PResult<ItemKind> {
        self.advance()?; // type
        let name = self.name()?;
        let generics = self.generics()?;
        self.expect(Punct::Assign)?;
        let ty = self.type_ann()?;
        self.semi()?;
        Ok(ItemKind::TypeAlias { name, generics, ty })
    }

    /// Whether the current `type` token starts an alias (`type Name`), not a use of a
    /// variable or function named `type` (`type(x)`).
    fn type_is_keyword(&mut self) -> PResult<bool> {
        Ok(matches!(self.peek()?, TokenKind::Ident(_)))
    }

    /// The `///` comments between the previous token and the current one, which starts an
    /// item. Doc comments before anything else are passed over.
    fn item_docs(&mut self) -> Vec<Span> {
        let mut docs = Vec::new();
        let all = self.lx.comments();
        while let Some(d) = all.get(self.next_doc) {
            if d.span.start >= self.tok.span.start {
                break;
            }
            if d.kind == CommentKind::Doc && d.span.start >= self.prev_end {
                docs.push(d.span);
            }
            self.next_doc += 1;
        }
        docs
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
        let generics = self.generics()?;
        let sig = self.fn_params()?;
        let body = if self.stub && self.eat(Punct::Semi)? {
            None
        } else {
            Some(self.block()?)
        };
        Ok(FnDecl {
            name,
            generics,
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
            sig.ret = Some(self.result_type(true)?);
        }
        Ok(sig)
    }

    /// A parameter list up to (not including) `close`.
    fn params(&mut self, close: Punct) -> PResult<FnSig> {
        // In a closure's `|...|`, a parameter type can't be a bare union (the `|` would end
        // the list); `|x: (int | nil)|` works.
        let unions = close != Punct::Pipe;
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
                    sig.self_ty = Some(self.type_of(unions)?);
                }
            } else if self.eat(Punct::DotDotDot)? {
                sig.rest = Some(self.name()?);
                if self.eat(Punct::Colon)? {
                    sig.rest_ty = Some(self.type_of(unions)?);
                }
            } else {
                let mutable = self.eat_kw(Keyword::Mut)?;
                let name = self.name()?;
                let name = (name.sym != self.sym_underscore).then_some(name);
                let ty = if self.eat(Punct::Colon)? {
                    Some(self.type_of(unions)?)
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

    /// `<T, U>` generic parameters, which only `orbit-check` reads. There are no traits, so
    /// a parameter is just a name.
    fn generics(&mut self) -> PResult<Vec<GenericParam>> {
        let mut params = Vec::new();
        if !self.eat(Punct::Lt)? {
            return Ok(params);
        }
        while !self.is(Punct::Gt) {
            params.push(GenericParam { name: self.name()? });
            if self.is(Punct::Colon) {
                return Err(self.error(
                    self.tok.span,
                    "generic parameters can't have bounds (there are no traits yet)",
                ));
            }
            if !self.eat(Punct::Comma)? {
                break;
            }
        }
        self.expect(Punct::Gt)?;
        Ok(params)
    }

    fn field_decls(&mut self) -> PResult<Vec<FieldDecl>> {
        let open = self.expect(Punct::LBrace)?.start;
        let mut fields = Vec::new();
        while !self.is(Punct::RBrace) {
            if self.unclosed_before_item(open) {
                return Ok(fields);
            }
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
        if !self.unclosed_before_item(open) {
            self.expect(Punct::RBrace)?;
        }
        Ok(fields)
    }

    fn struct_decl(&mut self) -> PResult<StructDecl> {
        self.advance()?; // struct
        let name = self.name()?;
        let generics = self.generics()?;
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
        Ok(StructDecl {
            name,
            generics,
            fields,
        })
    }

    fn enum_decl(&mut self) -> PResult<EnumDecl> {
        self.advance()?; // enum
        let name = self.name()?;
        let generics = self.generics()?;
        let open = self.expect(Punct::LBrace)?.start;
        let mut variants = Vec::new();
        while !self.is(Punct::RBrace) {
            if self.unclosed_before_item(open) {
                break;
            }
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
        if !self.unclosed_before_item(open) {
            self.expect(Punct::RBrace)?;
        }
        Ok(EnumDecl {
            name,
            generics,
            variants,
        })
    }

    fn impl_decl(&mut self) -> PResult<ImplDecl> {
        self.advance()?; // impl
        let generics = self.generics()?;
        let target = match self.tok.kind {
            TokenKind::Ident(_) => self.name()?,
            _ => return Err(self.expected("a struct or enum name")),
        };
        let target_args = self.type_args()?;
        if self.is_kw(Keyword::For) {
            return Err(self.error(
                self.tok.span,
                "traits are not supported yet; write metamethods such as `fn __add` directly in `impl`",
            ));
        }
        let open = self.expect(Punct::LBrace)?.start;
        let mut members = Vec::new();
        while !self.is(Punct::RBrace) {
            if self.recover && (self.tok.kind == TokenKind::Eof || self.item_at_line_start()) {
                let d = self.unclosed(open);
                self.recovered(d);
                break;
            }
            let start = self.tok.span.start;
            let depth = self.depth;
            match self.impl_member() {
                Ok(member) => members.push(member),
                Err(d) if self.recover => {
                    self.depth = depth;
                    self.report_in(d, start);
                    self.synchronize(start, true, Sync::Member);
                    members.push(Item {
                        is_pub: false,
                        kind: ItemKind::Error,
                        span: self.span_from(start),
                        docs: Vec::new(),
                    });
                }
                Err(d) => return Err(d),
            }
        }
        if !self.recover || self.is(Punct::RBrace) {
            self.expect(Punct::RBrace)?;
        }
        Ok(ImplDecl {
            generics,
            target,
            target_args,
            members,
        })
    }

    /// A `fn` or `const` in an `impl` block.
    fn impl_member(&mut self) -> PResult<Item> {
        let start = self.tok.span.start;
        let docs = self.item_docs();
        let is_pub = self.eat_kw(Keyword::Pub)?;
        let kind = match self.tok.kind {
            TokenKind::Keyword(Keyword::Fn) => ItemKind::Fn(self.fn_decl(true)?),
            TokenKind::Keyword(Keyword::Const) => {
                self.advance()?;
                let name = self.member_name()?;
                self.const_rest(name)?
            }
            _ => return Err(self.expected("`fn` or `const` in `impl`")),
        };
        Ok(Item {
            is_pub,
            kind,
            span: self.span_from(start),
            docs,
        })
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
        self.semi()?;
        Ok(UseDecl { path, tail })
    }

    // ---- types --------------------------------------------------------------------------------

    /// A type annotation; with `type_trees` off, only its span is kept and nothing is
    /// allocated.
    fn type_ann(&mut self) -> PResult<Type> {
        self.type_of(true)
    }

    /// A type, where `unions` says whether a top-level `A | B` is allowed (not between a
    /// closure's `|...|`, nor after `as`, where `|` is an operator).
    fn type_of(&mut self, unions: bool) -> PResult<Type> {
        let start = self.tok.span.start;
        let first = self.type_suffixed(unions)?;
        if !unions || !self.is(Punct::Pipe) {
            return Ok(first);
        }
        let mut members = Vec::new();
        self.push_member(&mut members, first);
        while self.eat(Punct::Pipe)? {
            let t = self.type_suffixed(unions)?;
            self.push_member(&mut members, t);
        }
        Ok(self.make_type(start, || TypeKind::Union(members)))
    }

    /// A function's result type: a type, `...T` (any number of `T`s), or a tuple whose last
    /// item may be `...T`, as in `(bool, ...any)`.
    fn result_type(&mut self, unions: bool) -> PResult<Type> {
        if self.is(Punct::DotDotDot) {
            return self.rest_type();
        }
        // `type_primary` takes the flag, so only the outermost tuple sees it.
        self.result_parens = true;
        let t = self.type_of(unions);
        self.result_parens = false;
        t
    }

    /// `...T`, at `...`.
    fn rest_type(&mut self) -> PResult<Type> {
        let start = self.tok.span.start;
        self.advance()?;
        let t = self.type_ann()?;
        Ok(self.make_type(start, || TypeKind::Rest(t)))
    }

    /// A type with any `?` suffixes.
    fn type_suffixed(&mut self, unions: bool) -> PResult<Type> {
        let start = self.tok.span.start;
        let mut t = self.type_primary(unions)?;
        while self.eat(Punct::Question)? {
            t = self.make_type(start, || TypeKind::Optional(t));
        }
        Ok(t)
    }

    fn type_primary(&mut self, unions: bool) -> PResult<Type> {
        self.enter()?;
        let start = self.tok.span.start;
        let result = std::mem::take(&mut self.result_parens);
        let kind = match self.tok.kind {
            TokenKind::Punct(Punct::LParen) => {
                self.advance()?;
                let mut items = Vec::new();
                let mut count = 0;
                let mut trailing_comma = false;
                while !self.is(Punct::RParen) {
                    if result && self.is(Punct::DotDotDot) {
                        // `(A, ...B)`: the rest is last.
                        let t = self.rest_type()?;
                        self.push_type(&mut items, t);
                        count += 1;
                        trailing_comma = self.eat(Punct::Comma)?;
                        break;
                    }
                    let t = self.type_ann()?;
                    self.push_type(&mut items, t);
                    count += 1;
                    trailing_comma = self.eat(Punct::Comma)?;
                    if !trailing_comma {
                        break;
                    }
                }
                self.expect(Punct::RParen)?;
                if count == 1 && !trailing_comma {
                    // `(T)` groups, as in `(string | number)?`.
                    self.leave();
                    let inner = items.pop();
                    return Ok(Type {
                        span: self.span_from(start),
                        kind: inner.and_then(|t| t.kind),
                    });
                }
                TypeKind::Tuple(items)
            }
            TokenKind::Punct(Punct::LBracket) => {
                self.advance()?;
                let t = self.type_ann()?;
                self.expect(Punct::RBracket)?;
                TypeKind::Array(t)
            }
            TokenKind::Punct(Punct::HashBrace) => {
                self.advance()?;
                if self.eat(Punct::LBracket)? {
                    let key = self.type_ann()?;
                    self.expect(Punct::RBracket)?;
                    self.expect(Punct::Colon)?;
                    let value = self.type_ann()?;
                    self.expect(Punct::RBrace)?;
                    TypeKind::Map { key, value }
                } else {
                    let mut fields = Vec::new();
                    while !self.is(Punct::RBrace) {
                        let name = match self.tok.kind {
                            TokenKind::Str(sym) => {
                                let span = self.tok.span;
                                self.advance()?;
                                Name { sym, span }
                            }
                            _ => self.member_name()?,
                        };
                        self.expect(Punct::Colon)?;
                        let ty = self.type_ann()?;
                        if self.type_trees {
                            fields.push(RecordField { name, ty });
                        }
                        if !self.eat(Punct::Comma)? {
                            break;
                        }
                    }
                    self.expect(Punct::RBrace)?;
                    TypeKind::Record(fields)
                }
            }
            TokenKind::Keyword(Keyword::Fn) => {
                self.advance()?;
                self.expect(Punct::LParen)?;
                let mut params = Vec::new();
                let mut rest = None;
                while !self.is(Punct::RParen) {
                    if self.eat(Punct::DotDotDot)? {
                        rest = Some(self.type_ann()?);
                        break;
                    }
                    let t = self.type_ann()?;
                    self.push_type(&mut params, t);
                    if !self.eat(Punct::Comma)? {
                        break;
                    }
                }
                self.expect(Punct::RParen)?;
                let ret = if self.eat(Punct::Arrow)? {
                    Some(self.result_type(unions)?)
                } else {
                    None
                };
                TypeKind::Fn { params, rest, ret }
            }
            TokenKind::Keyword(Keyword::Nil) => {
                self.advance()?;
                TypeKind::Nil
            }
            TokenKind::Keyword(Keyword::SelfType) => {
                self.advance()?;
                TypeKind::SelfType
            }
            TokenKind::Ident(_) => {
                let first = self.name()?;
                let mut segments = Vec::new();
                if self.type_trees {
                    segments.push(first);
                }
                while self.eat(Punct::ColonColon)? {
                    let seg = self.member_name()?;
                    if self.type_trees {
                        segments.push(seg);
                    }
                }
                let args = self.type_args()?;
                TypeKind::Path { segments, args }
            }
            _ => return Err(self.expected("a type")),
        };
        self.leave();
        Ok(self.make_type(start, || kind))
    }

    /// `<A, B>` generic arguments, if present.
    fn type_args(&mut self) -> PResult<Vec<Type>> {
        let mut args = Vec::new();
        if self.eat(Punct::Lt)? {
            while !self.is(Punct::Gt) && !self.is(Punct::Shr) {
                let t = self.type_ann()?;
                self.push_type(&mut args, t);
                if !self.eat(Punct::Comma)? {
                    break;
                }
            }
            self.expect_gt()?;
        }
        Ok(args)
    }

    /// A type spanning from `start` to here, with a tree only when asked for.
    fn make_type(&self, start: u32, kind: impl FnOnce() -> TypeKind) -> Type {
        Type {
            span: self.span_from(start),
            kind: self.type_trees.then(|| Box::new(kind())),
        }
    }

    /// Collects a child type, unless trees are off (so nothing is allocated).
    fn push_type(&self, list: &mut Vec<Type>, t: Type) {
        if self.type_trees {
            list.push(t);
        }
    }

    /// Adds a union member, flattening a (parenthesised) union into its members.
    fn push_member(&self, members: &mut Vec<Type>, t: Type) {
        match t.kind {
            Some(kind) if matches!(*kind, TypeKind::Union(_)) => {
                let TypeKind::Union(inner) = *kind else {
                    unreachable!()
                };
                members.extend(inner);
            }
            kind => self.push_type(members, Type { span: t.span, kind }),
        }
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
        loop {
            if self.is_kw(Keyword::As) {
                if AS_POWER < min_bp {
                    break;
                }
                lhs = self.cast(lhs, start)?;
                continue;
            }
            let Some(op) = self.binop() else {
                break;
            };
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

    /// `lhs as T` or `lhs as? T`, at `as`. Out of line, to keep `binary`'s frame small for
    /// deeply nested expressions.
    #[inline(never)]
    fn cast(&mut self, lhs: Expr, start: u32) -> PResult<Expr> {
        // `|` after the type is the operator: `x as int | 1`.
        self.advance()?;
        // `as?`: nil when the value isn't of the type.
        let optional = self.eat(Punct::Question)?;
        // The cast runs, so its target's tree is always built.
        let trees = std::mem::replace(&mut self.type_trees, true);
        let ty = self.type_of(false);
        self.type_trees = trees;
        Ok(Expr {
            kind: ExprKind::As {
                expr: Box::new(lhs),
                ty: ty?,
                optional,
            },
            span: self.span_from(start),
        })
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
            TokenKind::Eof
            | TokenKind::Punct(
                Punct::Semi | Punct::Comma | Punct::RParen | Punct::RBracket | Punct::RBrace,
            ) if self.recover => {
                // A missing expression, as in `let x = ;`: what's around it still parses.
                let d = self.expected("an expression");
                self.recovered(d);
                Expr {
                    kind: ExprKind::Error,
                    span: Span::new(start, start),
                }
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
                    let e = if expr.start == expr.end
                        || self.lx.src()[expr.start as usize..expr.end as usize]
                            .iter()
                            .all(u8::is_ascii_whitespace)
                    {
                        Err(self.error(
                            expr,
                            "empty `{}` in template; write `{{}}` for literal braces",
                        ))
                    } else {
                        // Parse the hole's source range as an expression, then resume after
                        // the template, whether or not the hole parsed.
                        let saved_tok = self.tok;
                        let saved_ahead = self.ahead.take();
                        let saved_prev = self.prev_end;
                        let saved_depth = self.depth;
                        let saved_lx = self.lx.enter(expr);
                        let e = self.hole_expr();
                        self.lx.restore(saved_lx);
                        self.tok = saved_tok;
                        self.ahead = saved_ahead;
                        self.prev_end = saved_prev;
                        self.depth = saved_depth;
                        e
                    };
                    let e = match e {
                        Ok(e) => e,
                        Err(d) if self.recover => {
                            self.recovered(d);
                            Expr {
                                kind: ExprKind::Error,
                                span: expr,
                            }
                        }
                        Err(d) => return Err(d),
                    };
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

    /// The expression in a template hole, with the lexer limited to the hole.
    fn hole_expr(&mut self) -> PResult<Expr> {
        self.advance()?;
        let e = self.expr()?;
        if self.tok.kind != TokenKind::Eof {
            return Err(self.expected("`}` to end the template hole"));
        }
        Ok(e)
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
                    } else if let ExprKind::Name(name) = e.kind
                        && self.tok.kind != TokenKind::Eof
                    {
                        // (At the end of input it may be a field name still being typed.)
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
            sig.ret = Some(self.result_type(true)?);
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
        let open = self.expect(Punct::LBrace)?.start;
        let mut arms = Vec::new();
        while !self.is(Punct::RBrace) {
            if self.recover && self.match_ended() {
                let d = self.unclosed(open);
                self.recovered(d);
                break;
            }
            let arm_start = self.tok.span.start;
            let depth = self.depth;
            let arm = match self.arm() {
                Ok(arm) => arm,
                Err(d) if self.recover => {
                    self.depth = depth;
                    self.report_in(d, arm_start);
                    self.synchronize(arm_start, true, Sync::Arm);
                    continue;
                }
                Err(d) => return Err(d),
            };
            let block_like = arm.body.kind.is_block_like();
            arms.push(arm);
            if !self.eat(Punct::Comma)? && !block_like {
                if !self.recover || self.is(Punct::RBrace) {
                    break;
                }
                // A missing `,`: on the same line, the arm goes on with something broken.
                let d = self.expected("`,`");
                self.recovered(d);
                if !self.starts_line() {
                    self.synchronize(arm_start, true, Sync::Arm);
                }
            }
        }
        if !self.recover || self.is(Punct::RBrace) {
            self.expect(Punct::RBrace)?;
        }
        Ok(Expr {
            kind: ExprKind::Match {
                scrutinee: Box::new(scrutinee),
                arms,
            },
            span: self.span_from(start),
        })
    }

    /// When recovering, whether the token where a match arm should start shows that the
    /// match's `}` is missing: the end of input, a `;`, or a `let` or item keyword starting
    /// a line.
    fn match_ended(&self) -> bool {
        match self.tok.kind {
            TokenKind::Eof | TokenKind::Punct(Punct::Semi) => true,
            TokenKind::Keyword(k) => {
                STATEMENT_KEYWORDS[..ITEM_KEYWORDS].contains(&k) && self.starts_line()
            }
            _ => false,
        }
    }

    /// `pat [if guard] => body`.
    fn arm(&mut self) -> PResult<Arm> {
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
        Ok(Arm {
            pat,
            guard,
            body,
            span: self.span_from(arm_start),
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

// Only ever a return value, moved once; boxing the statement would cost an allocation each.
#[allow(clippy::large_enum_variant)]
enum StmtOrTail {
    Stmt(Stmt),
    /// An expression not followed by `;`: the block's value if the block ends here.
    Tail(Expr),
}
