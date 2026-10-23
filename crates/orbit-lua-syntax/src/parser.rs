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
    /// Keep every comment in [`Ast::comments`] (for a formatter or an editor).
    pub keep_trivia: bool,
    /// Go on after a syntax error, for an editor: the error is recorded, the broken statement
    /// becomes an `Error` node and parsing resumes at the next statement. Use
    /// [`parse_recovering`] to get every error; [`parse_with`] ignores it and gives only the
    /// first, as the reference reports it.
    pub recover: bool,
}

impl Default for ParseOptions {
    fn default() -> ParseOptions {
        ParseOptions {
            max_depth: 200,
            skip_comment_line: false,
            keep_trivia: false,
            recover: false,
        }
    }
}

/// Parses one chunk. `chunk_name` is the name messages show, for example `errors.lua`.
pub fn parse(chunk_name: &str, src: &[u8]) -> Result<Ast, Diagnostic> {
    parse_with(chunk_name, src, &ParseOptions::default())
}

pub fn parse_with(chunk_name: &str, src: &[u8], options: &ParseOptions) -> Result<Ast, Diagnostic> {
    parse_chunk(chunk_name, src, options, false).map(|(ast, _)| ast)
}

/// Parses one chunk, going on after syntax errors ([`ParseOptions::recover`]): the AST holds
/// everything that parsed, with `Error` nodes where something didn't, and the errors come in
/// source order. A chunk without errors gets the AST [`parse_with`] gives. In one with errors,
/// indentation counts: a statement starting a line no deeper than the line opening its block
/// (when the block's body was indented deeper) or a function at the start of a line ends the
/// block, so a missing `end` is reported there rather than at the end of the chunk.
pub fn parse_recovering(
    chunk_name: &str,
    src: &[u8],
    options: &ParseOptions,
) -> (Ast, Vec<Diagnostic>) {
    // Only a chunk with errors is parsed again, with the indentation-based recovery, so a valid
    // one can never be misread.
    if let Ok(result) = parse_chunk(chunk_name, src, options, false) {
        return result;
    }
    match parse_chunk(chunk_name, src, options, true) {
        Ok(result) => result,
        // Only a source too large to parse at all gets here.
        Err(d) => {
            let ast = Ast {
                chunk: Chunk {
                    body: Block {
                        stmts: Vec::new(),
                        ret: None,
                        span: Span::default(),
                    },
                    span: Span::default(),
                },
                strings: Default::default(),
                source: SourceInfo {
                    name: chunk_name.into(),
                    language: Language::Lua,
                    line_starts: vec![0],
                },
                comments: Vec::new(),
            };
            (ast, vec![d])
        }
    }
}

fn parse_chunk(
    chunk_name: &str,
    src: &[u8],
    options: &ParseOptions,
    recover: bool,
) -> Result<(Ast, Vec<Diagnostic>), Diagnostic> {
    if u32::try_from(src.len()).is_err() {
        return Err(Diagnostic::new(Span::default(), 1, "chunk too large", None));
    }
    let mut lexer = Lexer::new(src);
    if options.skip_comment_line {
        lexer.skip_comment_line();
    }
    if options.keep_trivia {
        lexer.keep_comments();
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
            active: 0,
            line: None,
        }],
        sym_global: global,
        recover,
        errors: Vec::new(),
        lex_error_at: None,
        stmt_start: 0,
        stmt_indent: 0,
        layout: recover && uses_layout(src),
    };
    p.advance()?;
    let mut body = p.block()?;
    while p.tok.kind != TokenKind::Eof {
        let d = p.error_expected("<eof>");
        if !p.recover {
            return Err(d);
        }
        // A stray `end` (or a statement after `return`): skip it and go on.
        p.recovered(d);
        if !matches!(p.tok.kind, TokenKind::Keyword(Keyword::Return)) {
            p.advance()?;
        }
        let more = p.block()?;
        body.stmts.extend(more.stmts);
        body.ret = more.ret.or(body.ret);
        body.span = span_from(body.span.start, more.span.end);
    }
    let span = Span::new(0, src.len() as u32);
    let comments = p.lx.take_comments();
    let errors = std::mem::take(&mut p.errors);
    let (strings, line_starts) = p.lx.finish();
    let ast = Ast {
        chunk: Chunk { body, span },
        strings,
        source: SourceInfo {
            name: chunk_name.into(),
            language: Language::Lua,
            line_starts,
        },
        comments,
    };
    Ok((ast, errors))
}

/// Most local variables one function may have in scope at once, as in lua5.5.
const MAX_LOCALS: u32 = 200;

struct FuncState {
    vararg: bool,
    /// Loops enclosing the current position, within this function.
    loops: u32,
    /// Local variables in scope, counted as lua5.5 counts them against [`MAX_LOCALS`]: a
    /// `for` loop's hidden state and a vararg function's `...` take places, a `<const>` local
    /// whose value is a constant does not.
    active: u32,
    /// The line the function is defined on; `None` for the main function.
    line: Option<u32>,
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
    recover: bool,
    /// The errors recovered from so far (only with `recover`).
    errors: Vec<Diagnostic>,
    /// Where the last lexer error recovered from was.
    lex_error_at: Option<u32>,
    /// Where the statement being parsed starts, and how far its line is indented: when
    /// recovering, a later line indented no deeper starts something else.
    stmt_start: u32,
    stmt_indent: usize,
    /// Whether indentation shows where blocks end, as it does unless the code is written flush
    /// (no line indented, and some line starting with an `end` closing a multi-line block).
    layout: bool,
}

type PResult<T> = Result<T, Diagnostic>;

fn span_from(start: u32, end: u32) -> Span {
    Span::new(start, end.max(start))
}

impl Parser<'_> {
    // ---- tokens -------------------------------------------------------------------------------

    // Token helpers are kept out of line: they are called from hundreds of places, and
    // inlined they cost binary size for no measurable parse speed
    // (docs/design/orbitscript-front-end.md, "Binary size").
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

    fn is_punct(&self, p: Punct) -> bool {
        self.tok.kind == TokenKind::Punct(p)
    }

    fn is_kw(&self, k: Keyword) -> bool {
        self.tok.kind == TokenKind::Keyword(k)
    }

    #[inline(never)]
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
        near_token(&self.lx, tok)
    }

    /// `message near '<current token>'`.
    #[inline(never)]
    fn error_here(&self, message: impl Into<String>) -> Diagnostic {
        Diagnostic::new(
            self.tok.span,
            self.cur_line(),
            message,
            Some(self.near(self.tok)),
        )
    }

    /// `<what> expected near '<current token>'`; `what` is already quoted where Lua quotes it.
    #[inline(never)]
    fn error_expected(&self, what: &str) -> Diagnostic {
        self.error_here(format!("{what} expected"))
    }

    /// An error with no `near` part, reported at `span`'s line.
    #[inline(never)]
    fn error_at(&self, span: Span, message: impl Into<String>) -> Diagnostic {
        Diagnostic::new(span, self.line_of(span.start), message, None)
    }

    #[inline(never)]
    fn expect_punct(&mut self, p: Punct) -> PResult<()> {
        if !self.test_punct(p)? {
            return Err(self.error_expected(&format!("'{}'", p.as_str())));
        }
        Ok(())
    }

    #[inline(never)]
    fn expect_kw(&mut self, k: Keyword) -> PResult<()> {
        if !self.is_kw(k) {
            let d = self.error_expected(&format!("'{}'", k.as_str()));
            // When recovering, a missing `then` or `do` is taken as there.
            if self.recover && matches!(k, Keyword::Then | Keyword::Do) {
                self.recovered(d);
                return Ok(());
            }
            return Err(d);
        }
        self.advance()
    }

    /// Expects the token closing a construct opened by `who` at `open_line`.
    #[inline(never)]
    fn expect_match(&mut self, what: TokenKind, who: &str, open_line: u32) -> PResult<()> {
        if self.tok.kind == what {
            return self.advance();
        }
        let what_text = match what {
            TokenKind::Keyword(k) => k.as_str(),
            TokenKind::Punct(p) => p.as_str(),
            _ => unreachable!("closing token is a keyword or punctuation"),
        };
        let d = if open_line == self.cur_line() {
            self.error_expected(&format!("'{what_text}'"))
        } else {
            self.error_here(format!(
                "'{what_text}' expected (to close '{who}' at line {open_line})"
            ))
        };
        if !self.recover {
            return Err(d);
        }
        self.recovered(d);
        self.skip_to_close(what, open_line)
    }

    #[inline(never)]
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

    #[inline(never)]
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

    /// Brings `n` more locals into scope, failing near the current token past the limit.
    fn add_locals(&mut self, n: u32) -> PResult<()> {
        let f = self.func();
        f.active += n;
        if f.active <= MAX_LOCALS {
            return Ok(());
        }
        let place = match f.line {
            None => "main function".to_string(),
            Some(line) => format!("function at line {line}"),
        };
        Err(self.error_here(format!(
            "too many local variables (limit is {MAX_LOCALS}) in {place}"
        )))
    }

    // ---- error recovery -----------------------------------------------------------------------

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
        if self.lex_error_at.is_none_or(|at| at < self.prev_end) {
            self.report(d);
        }
    }

    /// Whether the current token is the first on its line.
    fn starts_line(&self) -> bool {
        let src = self.lx.source();
        let mut i = self.tok.span.start as usize;
        while i > 0 && matches!(src[i - 1], b' ' | b'\t') {
            i -= 1;
        }
        i == 0 || matches!(src[i - 1], b'\n' | b'\r')
    }

    /// When recovering, whether the current token starts a line (after the statement's first)
    /// indented no deeper than the statement's, and so most likely starts the next statement:
    /// the current one is missing something.
    fn dedented(&self) -> bool {
        self.recover
            && self.tok.span.start > self.stmt_start
            && !matches!(
                self.tok.kind,
                TokenKind::Eof
                    | TokenKind::Punct(Punct::RParen | Punct::RBracket | Punct::RBrace)
                    | TokenKind::Keyword(
                        Keyword::End | Keyword::Until | Keyword::Else | Keyword::Elseif
                    )
            )
            && self.starts_line()
            && self.line_indent(self.tok.span.start) <= self.stmt_indent
    }

    /// How far the line holding `offset` is indented (spaces and tabs, one column each).
    fn line_indent(&self, offset: u32) -> usize {
        let src = self.lx.source();
        let mut i = (offset as usize).min(src.len());
        while i > 0 && !matches!(src[i - 1], b'\n' | b'\r') {
            i -= 1;
        }
        src[i..]
            .iter()
            .take_while(|&&c| c == b' ' || c == b'\t')
            .count()
    }

    /// When recovering, whether the current token shows that the `end` of the block opened on
    /// a line indented by `opener` is missing: it starts a line no deeper than that one (when
    /// `indented`, the block's body not being written flush with it), or it starts a function
    /// definition at the very start of a line.
    fn block_end_missing(&mut self, opener: usize, indented: bool) -> PResult<bool> {
        if !self.recover || !self.starts_line() {
            return Ok(false);
        }
        let col = self.line_indent(self.tok.span.start);
        if indented && col <= opener && !self.is_punct(Punct::ColonColon) {
            return Ok(true);
        }
        Ok(col == 0
            && match self.tok.kind {
                TokenKind::Keyword(Keyword::Function) => true,
                TokenKind::Keyword(Keyword::Local) => {
                    self.peek()? == TokenKind::Keyword(Keyword::Function)
                }
                _ => false,
            })
    }

    /// Skips the rest of a broken statement that started at `start`: through a `;`, or a
    /// block or bracket closed at the end of a line, or up to a word that ends the enclosing
    /// block, a statement keyword starting a line, a line indented no deeper than the
    /// statement, or the end of input. Blocks and brackets opened before the error are counted
    /// (by lexing the statement again). Always moves on at least one token.
    fn synchronize(&mut self, start: u32) {
        let mut moved = self.tok.span.start != start;
        let mut depth = if moved {
            open_blocks(self.lx.source(), start, self.tok.span.start)
        } else {
            0
        };
        let indent = self.line_indent(start);
        loop {
            if moved && self.starts_line() {
                let col = self.line_indent(self.tok.span.start);
                let closer = matches!(
                    self.tok.kind,
                    TokenKind::Keyword(Keyword::End | Keyword::Until)
                        | TokenKind::Punct(Punct::RParen | Punct::RBracket | Punct::RBrace)
                );
                if col < indent || col == indent && !closer {
                    return;
                }
            }
            match self.tok.kind {
                TokenKind::Eof => return,
                TokenKind::Punct(Punct::Semi) if depth == 0 => {
                    let _ = self.advance();
                    return;
                }
                TokenKind::Keyword(
                    Keyword::End | Keyword::Until | Keyword::Else | Keyword::Elseif,
                ) if depth == 0 => {
                    if moved {
                        return;
                    }
                }
                // Words that can't be part of an expression start a statement anywhere;
                // `function` and `do` only at the start of a line.
                TokenKind::Keyword(
                    Keyword::Local
                    | Keyword::If
                    | Keyword::While
                    | Keyword::For
                    | Keyword::Repeat
                    | Keyword::Return
                    | Keyword::Goto
                    | Keyword::Break,
                )
                | TokenKind::Punct(Punct::ColonColon)
                    if depth == 0 && moved =>
                {
                    return;
                }
                TokenKind::Keyword(Keyword::Function | Keyword::Do)
                    if depth == 0 && moved && self.starts_line() =>
                {
                    return;
                }
                kind => match block_delta(kind) {
                    1 => depth += 1,
                    -1 => {
                        depth = depth.saturating_sub(1);
                        if depth == 0 && kind == TokenKind::Keyword(Keyword::End) {
                            let _ = self.advance();
                            if self.starts_line() {
                                return;
                            }
                            continue;
                        }
                    }
                    _ => {}
                },
            }
            // Lexing can't fail while recovering.
            let _ = self.advance();
            moved = true;
        }
    }

    /// After a construct opened at `open_line` was not closed by `what` where expected (and
    /// the error is recorded): skips to its closing token and consumes it, unless the input
    /// shows the closer is missing (the end, a word ending an outer block, a line indented no
    /// deeper than the opening one, or a function at the start of a line).
    fn skip_to_close(&mut self, what: TokenKind, open_line: u32) -> PResult<()> {
        let indent = self.line_indent(self.lx.line_start(open_line));
        let mut depth = 0u32;
        loop {
            if depth == 0 && self.tok.kind == what {
                return self.advance();
            }
            if self.tok.kind == TokenKind::Eof || self.block_end_missing(indent, true)? {
                return Ok(());
            }
            match block_delta(self.tok.kind) {
                1 => depth += 1,
                -1 if depth == 0 => return Ok(()),
                -1 => depth -= 1,
                _ => {}
            }
            self.advance()?;
        }
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
        let active = self.func().active;
        let start = self.tok.span.start;
        let mut stmts = Vec::new();
        let mut ret = None;
        // The indentation of the line opening the block, and whether the body doesn't start
        // on a line of its own that is no deeper (code written flush), to spot a missing `end`
        // when recovering.
        let (opener, indented) = if self.recover {
            let opener = self.line_indent(self.prev_end);
            let flush = self.starts_line() && self.line_indent(start) <= opener;
            (opener, self.layout || !flush)
        } else {
            (0, false)
        };
        let outer = (self.stmt_start, self.stmt_indent);
        while !self.block_follows() {
            if self.depth > 1 && self.block_end_missing(opener, indented)? {
                break;
            }
            let start = self.tok.span.start;
            if self.recover {
                self.stmt_start = start;
                self.stmt_indent = self.line_indent(start);
            }
            let (depth, funcs, func) = (self.depth, self.funcs.len(), self.func_counts());
            let result = if self.is_kw(Keyword::Return) {
                match self.return_stat() {
                    Ok(r) => {
                        ret = Some(r);
                        break;
                    }
                    Err(d) => Err(d),
                }
            } else {
                self.statement()
            };
            match result {
                Ok(Some(stmt)) => stmts.push(stmt),
                Ok(None) => {}
                Err(d) if self.recover => {
                    self.depth = depth;
                    self.funcs.truncate(funcs);
                    self.restore_func_counts(func);
                    if self.lex_error_at.is_none_or(|at| at < start) {
                        self.report(d);
                    }
                    self.synchronize(start);
                    stmts.push(Stmt {
                        kind: StmtKind::Error,
                        span: span_from(start, self.prev_end),
                    });
                }
                Err(d) => return Err(d),
            }
        }
        self.leave();
        self.func().active = active;
        (self.stmt_start, self.stmt_indent) = outer;
        Ok(Block {
            stmts,
            ret,
            span: span_from(start, self.prev_end),
        })
    }

    /// The current function's loop and local counts, to restore after an error.
    fn func_counts(&mut self) -> (u32, u32) {
        let f = self.func();
        (f.loops, f.active)
    }

    fn restore_func_counts(&mut self, (loops, active): (u32, u32)) {
        let f = self.func();
        f.loops = loops;
        f.active = active;
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
                let func = self.func_body(start, line, method.is_some())?;
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
                    self.add_locals(1)?;
                    let func = self.func_body(start, line, false)?;
                    StmtKind::LocalFunction { name, func }
                } else {
                    let attrib = self.opt_attrib()?;
                    let (names, values) = self.att_names_and_values(attrib, true)?;
                    let folded = match (names.as_slice(), values.as_slice()) {
                        ([name], [value]) => {
                            name.attrib.or(attrib).map(|a| a.kind) == Some(AttribKind::Const)
                                && is_constant(value)
                        }
                        _ => false,
                    };
                    self.add_locals(names.len() as u32 - u32::from(folded))?;
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
            let func = self.func_body(start, line, false)?;
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
        let active = self.func().active;
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
                let do_at = self.tok.span.start;
                // Two hidden places for the loop state, then the control variable.
                self.add_locals(2)?;
                self.expect_kw(Keyword::Do)?;
                self.add_locals(1)?;
                let body = self.loop_block()?;
                StmtKind::NumericFor {
                    var: first,
                    start,
                    limit,
                    step,
                    do_at,
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
                // Three hidden places for the iterator state, then the variables.
                self.add_locals(3)?;
                self.expect_kw(Keyword::Do)?;
                self.add_locals(vars.len() as u32)?;
                let body = self.loop_block()?;
                StmtKind::GenericFor { vars, exprs, body }
            }
            _ => return Err(self.error_expected("'=' or 'in'")),
        };
        self.expect_match(TokenKind::Keyword(Keyword::End), "for", line)?;
        self.func().active = active;
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
        let mut closing = false;
        loop {
            let name = self.name()?;
            let attrib = self.opt_attrib()?;
            // A name's own attribute overrides the prefix one.
            if is_local
                && attrib
                    .or(prefix)
                    .is_some_and(|a| a.kind == AttribKind::Close)
            {
                if closing {
                    // Reported at the end of the second declaration, as lua5.5 does.
                    let span = span_from(name.span.start, self.prev_end);
                    let line = self.line_of(self.prev_end.saturating_sub(1).max(span.start));
                    return Err(Diagnostic::new(
                        span,
                        line,
                        "multiple to-be-closed variables in local list",
                        None,
                    ));
                }
                closing = true;
            }
            names.push(AttName { name, attrib });
            if !self.test_punct(Punct::Comma)? {
                break;
            }
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
    /// (or `local`/`global` before it); a method has the implicit `self` parameter.
    fn func_body(&mut self, start: u32, line: u32, method: bool) -> PResult<FuncBody> {
        self.expect_punct(Punct::LParen)?;
        self.funcs.push(FuncState {
            vararg: false,
            loops: 0,
            active: 0,
            line: Some(line),
        });
        let body = self.func_contents(start, line, method);
        self.funcs.pop();
        body
    }

    fn func_contents(&mut self, start: u32, line: u32, method: bool) -> PResult<FuncBody> {
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
        // `...` takes a place, named or not.
        let n = u32::from(method) + params.len() as u32 + u32::from(vararg.is_some());
        self.add_locals(n)?;
        self.expect_punct(Punct::RParen)?;
        self.func().vararg = vararg.is_some();
        let body = self.block()?;
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
            let op_at = self.tok.span.start;
            self.advance()?;
            let rhs = self.subexpr(right)?;
            lhs = Expr {
                kind: ExprKind::Binary {
                    op,
                    op_at,
                    lhs: Box::new(lhs),
                    rhs: Box::new(rhs),
                },
                span: span_from(start, self.prev_end),
            };
        }
        self.leave();
        Ok(lhs)
    }

    /// Reports a missing expression at `span` and stands in an error node for it (recovery
    /// only). Out of line, so `simple_expr`'s frame stays small for deeply nested expressions
    /// in debug builds.
    #[inline(never)]
    fn missing_expr(&mut self, span: Span) -> Expr {
        let d = self.error_here("unexpected symbol");
        self.recovered(d);
        Expr {
            kind: ExprKind::Error,
            span: Span::new(span.start, span.start),
        }
    }

    fn simple_expr(&mut self) -> PResult<Expr> {
        let span = self.tok.span;
        if self.dedented() {
            // A missing expression before the next statement, as in `x =` on its own line.
            return Ok(self.missing_expr(span));
        }
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
                let body = self.func_body(span.start, line, false)?;
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
            TokenKind::Eof
            | TokenKind::Punct(
                Punct::Semi | Punct::Comma | Punct::RParen | Punct::RBracket | Punct::RBrace,
            )
            | TokenKind::Keyword(
                Keyword::End
                | Keyword::Then
                | Keyword::Do
                | Keyword::Else
                | Keyword::Elseif
                | Keyword::Until,
            ) if self.recover => {
                // A missing expression, as in `local x = )`: what's around it still parses.
                Ok(self.missing_expr(span))
            }
            _ => Err(self.error_here("unexpected symbol")),
        }
    }

    fn suffixed_expr(&mut self) -> PResult<Expr> {
        let start = self.tok.span.start;
        let e = self.primary_expr()?;
        self.suffixes(start, e)
    }

    /// The `.name`, `[key]`, `:name(args)` and call suffixes after `e`, which starts at
    /// `start`. Out of line, so the frames of a parenthesised expression's recursion
    /// (`suffixed_expr`, `primary_expr`) stay small: debug builds keep a slot for every
    /// temporary, and 200 nesting levels must fit a 2 MiB thread stack.
    #[inline(never)]
    fn suffixes(&mut self, start: u32, mut e: Expr) -> PResult<Expr> {
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
                    let args_at = self.tok.span.start;
                    let args = self.call_args()?;
                    ExprKind::Method {
                        obj: Box::new(e),
                        name,
                        args,
                        args_at,
                    }
                }
                TokenKind::Punct(Punct::LParen | Punct::LBrace) | TokenKind::Str(_) => {
                    let args_at = self.tok.span.start;
                    let args = self.call_args()?;
                    ExprKind::Call {
                        callee: Box::new(e),
                        args,
                        args_at,
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
                let args = if self.is_punct(Punct::RParen) || self.dedented() {
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
        while !self.is_punct(Punct::RBrace) && !self.dedented() {
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

/// Whether `src` has an indented line, or no line starting with `end` (see `Parser::layout`).
fn uses_layout(src: &[u8]) -> bool {
    let mut flush_end = false;
    for line in src.split(|&c| c == b'\n') {
        match line {
            [b' ' | b'\t', ..] => return true,
            [b'e', b'n', b'd', rest @ ..] => {
                flush_end |= !rest
                    .first()
                    .is_some_and(|c| c.is_ascii_alphanumeric() || *c == b'_')
            }
            _ => {}
        }
    }
    !flush_end
}

/// How a token changes the nesting of blocks and brackets: `function`, `if`, `do`, `repeat`
/// and opening brackets open one (`while` and `for` open theirs with `do`); `end`, `until`
/// and closing brackets close one.
fn block_delta(kind: TokenKind) -> i8 {
    match kind {
        TokenKind::Keyword(Keyword::Function | Keyword::If | Keyword::Do | Keyword::Repeat)
        | TokenKind::Punct(Punct::LParen | Punct::LBracket | Punct::LBrace) => 1,
        TokenKind::Keyword(Keyword::End | Keyword::Until)
        | TokenKind::Punct(Punct::RParen | Punct::RBracket | Punct::RBrace) => -1,
        _ => 0,
    }
}

/// How many blocks and brackets the source between `start` and `end` leaves open (for error
/// recovery; lexed again with a throwaway lexer).
fn open_blocks(src: &[u8], start: u32, end: u32) -> u32 {
    let mut lx = Lexer::new(&src[..end as usize]);
    lx.seek(start);
    let mut depth = 0u32;
    while let Ok(t) = lx.next_token() {
        match block_delta(t.kind) {
            _ if t.kind == TokenKind::Eof => break,
            1 => depth += 1,
            -1 => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    depth
}

/// Whether a `<const>` local initialized with `e` is a compile-time constant, which lua5.5
/// gives no place among the function's locals: a literal, or arithmetic folded from them.
fn is_constant(e: &Expr) -> bool {
    match &e.kind {
        ExprKind::Nil
        | ExprKind::True
        | ExprKind::False
        | ExprKind::Int(_)
        | ExprKind::Float(_)
        | ExprKind::Str(_) => true,
        ExprKind::Paren(e) => is_constant(e),
        ExprKind::Unary {
            op: UnOp::Neg,
            expr,
        } => is_number(expr),
        ExprKind::Binary {
            op: BinOp::Add | BinOp::Sub | BinOp::Mul,
            lhs,
            rhs,
            ..
        } => is_number(lhs) && is_number(rhs),
        _ => false,
    }
}

/// Whether `e` folds to a number constant.
fn is_number(e: &Expr) -> bool {
    match &e.kind {
        ExprKind::Int(_) | ExprKind::Float(_) => true,
        ExprKind::Paren(e) => is_number(e),
        ExprKind::Unary {
            op: UnOp::Neg,
            expr,
        } => is_number(expr),
        ExprKind::Binary {
            op: BinOp::Add | BinOp::Sub | BinOp::Mul,
            lhs,
            rhs,
            ..
        } => is_number(lhs) && is_number(rhs),
        _ => false,
    }
}

/// How an error message shows `tok`, lexed by `lx`.
pub(crate) fn near_token(lx: &Lexer<'_>, tok: Token) -> Near {
    match tok.kind {
        TokenKind::Eof => Near::Eof,
        TokenKind::Keyword(k) => Near::Text(k.as_str().as_bytes().to_vec()),
        TokenKind::Punct(p) => Near::Text(p.as_str().as_bytes().to_vec()),
        TokenKind::Other(c) if c.is_ascii_graphic() => Near::Text(vec![c]),
        TokenKind::Other(c) => Near::Text(format!("<\\{c}>").into_bytes()),
        // The reference shows a string as its delimiters around the decoded contents.
        TokenKind::Str(id) => {
            let raw = lx.text(tok.span);
            let contents = lx.strings().get(id);
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
            Near::Text(lx.text(tok.span).to_vec())
        }
    }
}
