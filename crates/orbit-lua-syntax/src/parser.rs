//! The Lua 5.5 parser, a port of the grammar and scoping rules of `lparser.c`.
//!
//! Like the reference, it is a single pass: names are resolved, goto labels matched and
//! static errors raised in the same order `lua5.5` raises them, so the first error
//! reported is the same one, with the same message and line.

use crate::ast::*;
use crate::error::{ErrorKind, SyntaxError};
use crate::fold;
use crate::lexer::{Lexer, Tok, Token};
use crate::span::Span;

/// Parser settings.
#[derive(Debug, Clone)]
pub struct ParseOptions {
    /// Treat `global` as a reserved word. The default `lua5.5` build defines
    /// `LUA_COMPAT_GLOBAL`, which keeps `global` a regular name and recognizes `global`
    /// statements by the token that follows; that is the default here too.
    pub global_is_keyword: bool,
    /// Nesting depth at which parsing fails with "C stack overflow". The reference
    /// shares this budget (`LUAI_MAXCCALLS`, 200) with the C calls already active when
    /// the parser runs, so a host lowers it by its own nesting: `load` called from a
    /// script's main chunk under `lua5.5` has 198 left.
    pub max_depth: u32,
}

impl Default for ParseOptions {
    fn default() -> Self {
        ParseOptions {
            global_is_keyword: false,
            max_depth: 200,
        }
    }
}

/// Parses a chunk with the default options.
pub fn parse(src: &[u8]) -> Result<Chunk, SyntaxError> {
    parse_with(src, &ParseOptions::default())
}

/// Parses a chunk.
pub fn parse_with(src: &[u8], options: &ParseOptions) -> Result<Chunk, SyntaxError> {
    let mut p = Parser::new(src, options);
    p.main_func().map_err(|e| *e)
}

/// Skips what `luaL_loadfile` skips at the start of a file: a UTF-8 byte order mark
/// and a first line starting with `#`. The newline ending that line is kept, so line
/// numbers do not change.
pub fn skip_file_prefix(src: &[u8]) -> &[u8] {
    let src = src.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(src);
    if src.first() == Some(&b'#') {
        let nl = src.iter().position(|&c| c == b'\n').unwrap_or(src.len());
        &src[nl..]
    } else {
        src
    }
}

type PResult<T> = Result<T, Box<SyntaxError>>;

/// Maximum number of registers used by local variables in one function (`MAXVARS`).
const MAX_VARS: usize = 200;
/// Maximum number of upvalues of one function (`MAXUPVAL`).
const MAX_UPVAL: usize = 255;

/// The kind of an entry in the active variable list (`vd.kind` in the reference).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VarKind {
    Reg,
    Const,
    VarargTable,
    Close,
    CompileTimeConst,
    Global,
    GlobalConst,
}

impl VarKind {
    fn in_register(self) -> bool {
        matches!(
            self,
            VarKind::Reg | VarKind::Const | VarKind::VarargTable | VarKind::Close
        )
    }

    fn is_global(self) -> bool {
        matches!(self, VarKind::Global | VarKind::GlobalConst)
    }
}

/// An entry in the list of declared variables (`Vardesc`). Global declarations live in
/// the same list, as in the reference, so they scope like locals.
#[derive(Debug)]
struct VarDesc {
    /// `None` for a collective `global *` declaration.
    name: Option<Box<str>>,
    kind: VarKind,
    /// The AST local, for named user-visible locals.
    id: Option<LocalId>,
}

#[derive(Debug)]
struct UpvalDesc {
    name: Box<str>,
    source: UpvalSource,
    kind: VarKind,
}

#[derive(Debug)]
struct BlockCnt {
    nactvar: usize,
    first_label: usize,
    first_goto: usize,
    is_loop: bool,
}

#[derive(Debug)]
struct FuncState {
    /// Index of this function's first variable in `Parser::actvar`.
    first_local: usize,
    /// Number of active variables (declarations in scope).
    nactvar: usize,
    blocks: Vec<BlockCnt>,
    is_vararg: bool,
    upvalues: Vec<UpvalDesc>,
    /// Index of this function's first label in `Parser::labels`.
    first_label: usize,
    line_defined: u32,
}

#[derive(Debug)]
struct LabelDesc {
    name: Box<str>,
    line: u32,
    nactvar: usize,
}

/// The result of looking up a name (the reference's `expdesc` for variables).
#[derive(Debug, Clone, Copy)]
enum VarExp {
    /// A local of the function being searched: its index there.
    Local(usize),
    Upval(u32),
    /// A compile-time constant, possibly of an enclosing function.
    Const(LocalId),
    /// A global. The payload is the reference's `u.info`: -1 when no global declaration
    /// is in scope, -2 when some are but none covers this name, otherwise the index in
    /// `actvar` of the declaration that covers it.
    Global(i64),
}

struct Parser<'a> {
    lx: Lexer<'a>,
    src: &'a [u8],
    tok: Token,
    ahead: Option<Token>,
    /// Line of the last token consumed (`ls->lastline`).
    lastline: u32,
    /// End offset of the last token consumed.
    prev_end: u32,
    global_is_keyword: bool,
    max_depth: u32,
    depth: u32,
    actvar: Vec<VarDesc>,
    funcs: Vec<FuncState>,
    labels: Vec<LabelDesc>,
    gotos: Vec<LabelDesc>,
    locals: Vec<LocalInfo>,
}

impl<'a> Parser<'a> {
    fn new(src: &'a [u8], options: &ParseOptions) -> Self {
        Parser {
            lx: Lexer::new(src),
            src,
            tok: Token {
                tok: Tok::Eof,
                span: Span::new(0, 0, 1),
            },
            ahead: None,
            lastline: 1,
            prev_end: 0,
            global_is_keyword: options.global_is_keyword,
            max_depth: options.max_depth,
            depth: 0,
            actvar: Vec::new(),
            funcs: Vec::new(),
            labels: Vec::new(),
            gotos: Vec::new(),
            locals: Vec::new(),
        }
    }

    // ---------------------------------------------------------------------------------
    // Tokens and errors

    fn lex(&mut self) -> PResult<Token> {
        let mut t = self.lx.next_token()?;
        if t.tok == Tok::Global && !self.global_is_keyword {
            t.tok = Tok::Name("global".into());
        }
        Ok(t)
    }

    fn next(&mut self) -> PResult<()> {
        self.lastline = self.lx.line();
        self.prev_end = self.tok.span.end;
        self.tok = match self.ahead.take() {
            Some(t) => t,
            None => self.lex()?,
        };
        Ok(())
    }

    fn lookahead(&mut self) -> PResult<&Tok> {
        debug_assert!(self.ahead.is_none());
        let t = self.lex()?;
        Ok(&self.ahead.insert(t).tok)
    }

    /// `ls->linenumber`: the line where the scanner is.
    fn line(&self) -> u32 {
        self.lx.line()
    }

    fn span_from(&self, start: Span) -> Span {
        Span::new(start.start, self.prev_end.max(start.start), start.line)
    }

    fn syntax_error(&self, msg: impl Into<String>) -> Box<SyntaxError> {
        // A NUL byte is token 0, which the reference takes to mean "no token".
        let near = match self.tok.tok {
            Tok::Char(0) => None,
            _ => Some(self.tok.near_text(self.src).into_boxed_slice()),
        };
        Box::new(SyntaxError {
            kind: ErrorKind::Syntax,
            message: msg.into(),
            line: self.line(),
            near,
            span: self.tok.span,
        })
    }

    fn sem_error(&self, msg: impl Into<String>, span: Span) -> Box<SyntaxError> {
        Box::new(SyntaxError {
            kind: ErrorKind::Semantic,
            message: msg.into(),
            line: self.lastline,
            near: None,
            span,
        })
    }

    fn error_expected(&self, t: &Tok) -> Box<SyntaxError> {
        self.syntax_error(format!("{} expected", describe(t)))
    }

    fn check(&self, t: &Tok) -> PResult<()> {
        if &self.tok.tok == t {
            Ok(())
        } else {
            Err(self.error_expected(t))
        }
    }

    fn test_next(&mut self, t: &Tok) -> PResult<bool> {
        if &self.tok.tok == t {
            self.next()?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    fn check_next(&mut self, t: &Tok) -> PResult<()> {
        self.check(t)?;
        self.next()
    }

    fn check_match(&mut self, what: &Tok, who: &Tok, where_: u32) -> PResult<()> {
        if self.test_next(what)? {
            return Ok(());
        }
        if where_ == self.line() {
            Err(self.error_expected(what))
        } else {
            Err(self.syntax_error(format!(
                "{} expected (to close {} at line {})",
                describe(what),
                describe(who),
                where_
            )))
        }
    }

    fn check_name(&mut self) -> PResult<Name> {
        match &self.tok.tok {
            Tok::Name(n) => {
                let name = Name {
                    name: n.clone(),
                    span: self.tok.span,
                };
                self.next()?;
                Ok(name)
            }
            _ => Err(self.error_expected(&Tok::Name("".into()))),
        }
    }

    fn enter_level(&mut self) -> PResult<()> {
        self.depth += 1;
        if self.depth >= self.max_depth {
            return Err(Box::new(SyntaxError {
                kind: ErrorKind::Overflow,
                message: "C stack overflow".into(),
                line: self.line(),
                near: None,
                span: self.tok.span,
            }));
        }
        Ok(())
    }

    fn leave_level(&mut self) {
        self.depth -= 1;
    }

    fn is_char(&self, c: u8) -> bool {
        self.tok.tok == Tok::Char(c)
    }

    // ---------------------------------------------------------------------------------
    // Variables and scopes

    fn fs(&self) -> &FuncState {
        self.funcs.last().expect("inside a function")
    }

    fn fs_mut(&mut self) -> &mut FuncState {
        self.funcs.last_mut().expect("inside a function")
    }

    fn var(&self, level: usize, vidx: usize) -> &VarDesc {
        &self.actvar[self.funcs[level].first_local + vidx]
    }

    fn new_local_info(&mut self, name: &Name, kind: LocalKind) -> LocalId {
        let id = LocalId(self.locals.len() as u32);
        self.locals.push(LocalInfo {
            name: name.name.clone(),
            kind,
            captured: false,
            span: name.span,
        });
        id
    }

    /// Declares a user-visible local (not yet in scope) and returns its AST handle.
    fn new_local(&mut self, name: Name, kind: VarKind) -> LocalDecl {
        let lk = match kind {
            VarKind::Const => LocalKind::Const,
            VarKind::Close => LocalKind::Close,
            VarKind::VarargTable => LocalKind::VarargTable,
            _ => LocalKind::Regular,
        };
        let id = self.new_local_info(&name, lk);
        self.actvar.push(VarDesc {
            name: Some(name.name.clone()),
            kind,
            id: Some(id),
        });
        LocalDecl { name, id }
    }

    /// Declares an internal variable (such as the hidden `for` state).
    fn new_hidden(&mut self, name: &str) {
        self.actvar.push(VarDesc {
            name: Some(name.into()),
            kind: VarKind::Reg,
            id: None,
        });
    }

    /// Declares a global name (`None` for `*`).
    fn new_global(&mut self, name: Option<&Name>, kind: VarKind) {
        self.actvar.push(VarDesc {
            name: name.map(|n| n.name.clone()),
            kind,
            id: None,
        });
    }

    /// Number of registers used by the first `nvar` variables (`reglevel`).
    fn reg_level(&self, level: usize, nvar: usize) -> usize {
        let fs = &self.funcs[level];
        self.actvar[fs.first_local..fs.first_local + nvar]
            .iter()
            .filter(|v| v.kind.in_register())
            .count()
    }

    fn error_limit(&self, level: usize, limit: usize, what: &str) -> Box<SyntaxError> {
        let line = self.funcs[level].line_defined;
        let where_ = if line == 0 {
            "main function".to_string()
        } else {
            format!("function at line {line}")
        };
        self.syntax_error(format!("too many {what} (limit is {limit}) in {where_}"))
    }

    /// Brings the last `n` declared variables into scope (`adjustlocalvars`).
    fn adjust_local_vars(&mut self, n: usize) -> PResult<()> {
        let level = self.funcs.len() - 1;
        for _ in 0..n {
            self.fs_mut().nactvar += 1;
            let nact = self.fs().nactvar;
            if self.reg_level(level, nact) > MAX_VARS {
                return Err(self.error_limit(level, MAX_VARS, "local variables"));
            }
        }
        Ok(())
    }

    fn enter_block(&mut self, is_loop: bool) {
        let bl = BlockCnt {
            nactvar: self.fs().nactvar,
            first_label: self.labels.len(),
            first_goto: self.gotos.len(),
            is_loop,
        };
        self.fs_mut().blocks.push(bl);
    }

    fn leave_block(&mut self) -> PResult<()> {
        let bl = self.fs_mut().blocks.pop().expect("open block");
        // Variables leave scope, but their descriptions stay readable until the gotos
        // are solved, as the error for a jump into a scope names the variable.
        self.fs_mut().nactvar = bl.nactvar;
        self.solve_gotos(&bl)?;
        let first_local = self.fs().first_local;
        self.actvar.truncate(first_local + bl.nactvar);
        if self.fs().blocks.is_empty() {
            // The function's outermost block: every goto must be solved by now.
            if let Some(gt) = self.gotos.get(bl.first_goto) {
                let msg = format!(
                    "no visible label '{}' for <goto> at line {}",
                    gt.name, gt.line
                );
                return Err(self.sem_error(msg, self.tok.span));
            }
        }
        Ok(())
    }

    /// Matches the pending gotos of a finishing block against its labels; the others
    /// move out to the enclosing block (`solvegotos`).
    fn solve_gotos(&mut self, bl: &BlockCnt) -> PResult<()> {
        let mut i = bl.first_goto;
        while i < self.gotos.len() {
            let found = self.labels[bl.first_label..]
                .iter()
                .find(|l| l.name == self.gotos[i].name)
                .map(|l| l.nactvar);
            match found {
                Some(label_nactvar) => {
                    let gt = &self.gotos[i];
                    if gt.nactvar < label_nactvar {
                        let level = self.funcs.len() - 1;
                        let var = match &self.var(level, gt.nactvar).name {
                            Some(n) => n.to_string(),
                            None => "*".into(),
                        };
                        let msg = format!(
                            "<goto {}> at line {} jumps into the scope of '{}'",
                            gt.name, gt.line, var
                        );
                        return Err(self.sem_error(msg, self.tok.span));
                    }
                    self.gotos.remove(i);
                }
                None => {
                    self.gotos[i].nactvar = bl.nactvar;
                    i += 1;
                }
            }
        }
        self.labels.truncate(bl.first_label);
        Ok(())
    }

    fn open_func(&mut self, line_defined: u32) {
        self.funcs.push(FuncState {
            first_local: self.actvar.len(),
            nactvar: 0,
            blocks: Vec::new(),
            is_vararg: false,
            upvalues: Vec::new(),
            first_label: self.labels.len(),
            line_defined,
        });
        self.enter_block(false);
    }

    fn close_func(&mut self) -> PResult<Vec<Upvalue>> {
        self.leave_block()?;
        let fs = self.funcs.pop().expect("open function");
        debug_assert_eq!(self.actvar.len(), fs.first_local);
        Ok(fs
            .upvalues
            .into_iter()
            .map(|u| Upvalue {
                name: u.name,
                source: u.source,
            })
            .collect())
    }

    /// Looks for an active variable named `n` in function `level` (`searchvar`).
    fn search_var(&self, level: usize, n: &str, info: &mut i64) -> Option<VarExp> {
        let fs = &self.funcs[level];
        for i in (0..fs.nactvar).rev() {
            let vd = &self.actvar[fs.first_local + i];
            if vd.kind.is_global() {
                match &vd.name {
                    None => {
                        if *info < 0 {
                            *info = (fs.first_local + i) as i64;
                        }
                    }
                    Some(name) if &**name == n => {
                        return Some(VarExp::Global((fs.first_local + i) as i64));
                    }
                    Some(_) => {
                        if *info == -1 {
                            *info = -2;
                        }
                    }
                }
            } else if vd.name.as_deref() == Some(n) {
                return Some(if vd.kind == VarKind::CompileTimeConst {
                    VarExp::Const(vd.id.expect("named local"))
                } else {
                    VarExp::Local(i)
                });
            }
        }
        None
    }

    fn search_upvalue(&self, level: usize, n: &str) -> Option<u32> {
        self.funcs[level]
            .upvalues
            .iter()
            .position(|u| &*u.name == n)
            .map(|i| i as u32)
    }

    /// Adds an upvalue to function `level` for a variable of function `level - 1`.
    fn new_upvalue(&mut self, level: usize, n: &str, v: VarExp) -> PResult<u32> {
        if self.funcs[level].upvalues.len() + 1 > MAX_UPVAL {
            return Err(self.error_limit(level, MAX_UPVAL, "upvalues"));
        }
        let (source, kind) = match v {
            VarExp::Local(vidx) => {
                let vd = self.var(level - 1, vidx);
                let id = vd.id.expect("named local");
                (UpvalSource::Local(id), vd.kind)
            }
            VarExp::Upval(idx) => (
                UpvalSource::Upvalue(idx),
                self.funcs[level - 1].upvalues[idx as usize].kind,
            ),
            _ => unreachable!("only locals and upvalues become upvalues"),
        };
        let ups = &mut self.funcs[level].upvalues;
        ups.push(UpvalDesc {
            name: n.into(),
            source,
            kind,
        });
        Ok((ups.len() - 1) as u32)
    }

    /// Finds `n` starting at function `level`, creating upvalues in the functions in
    /// between (`singlevaraux`). `info` tracks global declarations, as in `search_var`.
    fn single_var_aux(
        &mut self,
        level: usize,
        n: &str,
        info: &mut i64,
        base: bool,
    ) -> PResult<VarExp> {
        if let Some(v) = self.search_var(level, n, info) {
            if !base && let VarExp::Local(vidx) = v {
                // Used as an upvalue by an inner function.
                let id = self.var(level, vidx).id.expect("named local");
                self.locals[id.0 as usize].captured = true;
            }
            return Ok(v);
        }
        if let Some(idx) = self.search_upvalue(level, n) {
            return Ok(VarExp::Upval(idx));
        }
        if level == 0 {
            return Ok(VarExp::Global(*info));
        }
        let v = self.single_var_aux(level - 1, n, info, false)?;
        match v {
            VarExp::Local(_) | VarExp::Upval(_) => {
                Ok(VarExp::Upval(self.new_upvalue(level, n, v)?))
            }
            other => Ok(other),
        }
    }

    fn cur_level(&self) -> usize {
        self.funcs.len() - 1
    }

    /// The binding and read-only status of a local or upvalue found by a lookup.
    fn binding_of(&self, v: VarExp) -> (Binding, Option<VarKind>) {
        let level = self.cur_level();
        match v {
            VarExp::Local(vidx) => {
                let vd = self.var(level, vidx);
                (Binding::Local(vd.id.expect("named local")), Some(vd.kind))
            }
            VarExp::Upval(i) => (
                Binding::Upvalue(i),
                Some(self.funcs[level].upvalues[i as usize].kind),
            ),
            VarExp::Const(id) => (Binding::Const(id), Some(VarKind::CompileTimeConst)),
            VarExp::Global(_) => unreachable!("globals are handled by the caller"),
        }
    }

    /// Resolves the `_ENV` used to access global `varname` (`buildglobal`).
    fn build_global(&mut self, varname: &str, span: Span) -> PResult<EnvRef> {
        let mut info = -1;
        let level = self.cur_level();
        let v = self.single_var_aux(level, "_ENV", &mut info, true)?;
        match v {
            VarExp::Global(_) => Err(self.sem_error(
                format!("_ENV is global when accessing variable '{varname}'"),
                span,
            )),
            VarExp::Local(_) | VarExp::Const(_) => match self.binding_of(v).0 {
                Binding::Local(id) | Binding::Const(id) => Ok(EnvRef::Local(id)),
                _ => unreachable!(),
            },
            VarExp::Upval(i) => Ok(EnvRef::Upvalue(i)),
        }
    }

    /// Resolves a variable name (`buildvar`). Returns its binding and whether it is
    /// read-only.
    fn build_var(&mut self, name: &Name) -> PResult<(Binding, bool)> {
        let mut info = -1;
        let level = self.cur_level();
        let v = self.single_var_aux(level, &name.name, &mut info, true)?;
        match v {
            VarExp::Global(info) => {
                if info == -2 {
                    return Err(
                        self.sem_error(format!("variable '{}' not declared", name.name), name.span)
                    );
                }
                let env = self.build_global(&name.name, name.span)?;
                let ro = info >= 0 && self.actvar[info as usize].kind == VarKind::GlobalConst;
                Ok((Binding::Global(env), ro))
            }
            other => {
                let (b, kind) = self.binding_of(other);
                Ok((b, kind.is_some_and(|k| k != VarKind::Reg)))
            }
        }
    }

    fn single_var(&mut self) -> PResult<Expr> {
        let name = self.check_name()?;
        let (binding, _) = self.build_var(&name)?;
        let span = name.span;
        Ok(Expr {
            kind: ExprKind::Name(NameRef { name, binding }),
            span,
        })
    }

    /// Raises the error for assigning to a read-only variable (`check_readonly`).
    fn check_readonly(&self, e: &Expr) -> PResult<()> {
        if let ExprKind::Name(n) = &e.kind {
            let ro = match n.binding {
                Binding::Const(_) => true,
                Binding::Local(id) => self.locals[id.0 as usize].kind.is_read_only(),
                Binding::Upvalue(i) => self.fs().upvalues[i as usize].kind != VarKind::Reg,
                Binding::Global(_) => self.global_is_readonly(&n.name.name),
            };
            if ro {
                return Err(self.sem_error(
                    format!("attempt to assign to const variable '{}'", n.name.name),
                    e.span,
                ));
            }
        }
        Ok(())
    }

    /// Whether global `n` is read-only in the current scope. Resolution has already
    /// succeeded, so this repeats the declaration search without side effects.
    fn global_is_readonly(&self, n: &str) -> bool {
        let mut info = -1;
        for level in (0..self.funcs.len()).rev() {
            match self.search_var(level, n, &mut info) {
                Some(VarExp::Global(i)) => {
                    return self.actvar[i as usize].kind == VarKind::GlobalConst;
                }
                Some(_) => return false,
                None => {}
            }
        }
        info >= 0 && self.actvar[info as usize].kind == VarKind::GlobalConst
    }

    // ---------------------------------------------------------------------------------
    // Grammar: chunks and blocks

    fn main_func(&mut self) -> PResult<Chunk> {
        self.open_func(0);
        self.fs_mut().is_vararg = true;
        self.fs_mut().upvalues.push(UpvalDesc {
            name: "_ENV".into(),
            source: UpvalSource::ChunkEnv,
            kind: VarKind::Reg,
        });
        self.next()?;
        let block = self.stat_list()?;
        self.check(&Tok::Eof)?;
        let upvalues = self.close_func()?;
        let end = self.src.len() as u32;
        Ok(Chunk {
            body: FuncBody {
                params: Vec::new(),
                is_method: false,
                vararg: Some(Vararg { name: None }),
                block,
                upvalues,
                line_defined: 0,
                last_line_defined: 0,
                span: Span::new(0, end, 1),
            },
            locals: std::mem::take(&mut self.locals),
        })
    }

    fn block_follow(&self, with_until: bool) -> bool {
        match self.tok.tok {
            Tok::Else | Tok::Elseif | Tok::End | Tok::Eof => true,
            Tok::Until => with_until,
            _ => false,
        }
    }

    fn stat_list(&mut self) -> PResult<Block> {
        let mut block = Block {
            stats: Vec::new(),
            ret: None,
        };
        while !self.block_follow(true) {
            if self.tok.tok == Tok::Return {
                self.enter_level()?;
                block.ret = Some(self.ret_stat()?);
                self.leave_level();
                break;
            }
            self.statement(&mut block.stats)?;
        }
        Ok(block)
    }

    fn block(&mut self) -> PResult<Block> {
        self.enter_block(false);
        let b = self.stat_list()?;
        self.leave_block()?;
        Ok(b)
    }

    // ---------------------------------------------------------------------------------
    // Grammar: statements

    fn statement(&mut self, out: &mut Vec<Stat>) -> PResult<()> {
        let line = self.line();
        let start = Span::new(self.tok.span.start, self.tok.span.end, line);
        self.enter_level()?;
        // Choose a parsing function first and call it from a single site, which keeps
        // this frame small: it is on the stack once per nesting level.
        type StatFn<'a> = fn(&mut Parser<'a>, u32) -> PResult<StatKind>;
        let parse: StatFn<'a> = match &self.tok.tok {
            Tok::Char(b';') => {
                self.next()?;
                self.leave_level();
                return Ok(());
            }
            Tok::DbColon => {
                self.next()?;
                let name = self.check_name()?;
                self.label_stat(name, line, start, out)?;
                self.leave_level();
                return Ok(());
            }
            Tok::If => Self::if_stat,
            Tok::While => Self::while_stat,
            Tok::Do => Self::do_stat,
            Tok::For => Self::for_stat,
            Tok::Repeat => Self::repeat_stat,
            Tok::Function => Self::func_stat,
            Tok::Local => Self::local_stat_or_func,
            Tok::Global => Self::global_stat_func,
            Tok::Break => Self::break_stat,
            Tok::Goto => Self::goto_stat,
            Tok::Name(n) if &**n == "global" && !self.global_is_keyword => {
                let lk = self.lookahead()?;
                if matches!(lk, Tok::Char(b'<' | b'*') | Tok::Name(_) | Tok::Function) {
                    Self::global_stat_func
                } else {
                    Self::expr_stat
                }
            }
            _ => Self::expr_stat,
        };
        let kind = parse(self, line)?;
        let span = self.span_from(start);
        out.push(Stat { kind, span });
        self.leave_level();
        Ok(())
    }

    fn do_stat(&mut self, line: u32) -> PResult<StatKind> {
        self.next()?;
        let b = self.block()?;
        self.check_match(&Tok::End, &Tok::Do, line)?;
        Ok(StatKind::Do(b))
    }

    fn local_stat_or_func(&mut self, _line: u32) -> PResult<StatKind> {
        self.next()?;
        if self.test_next(&Tok::Function)? {
            self.local_func()
        } else {
            self.local_stat()
        }
    }

    fn goto_stat(&mut self, line: u32) -> PResult<StatKind> {
        self.next()?;
        let name = self.check_name()?;
        let nactvar = self.fs().nactvar;
        self.gotos.push(LabelDesc {
            name: name.name.clone(),
            line,
            nactvar,
        });
        Ok(StatKind::Goto(name))
    }

    fn if_stat(&mut self, line: u32) -> PResult<StatKind> {
        let mut clauses = Vec::new();
        loop {
            // test_then_block
            self.next()?; // skip IF or ELSEIF
            let cond = self.expr()?;
            self.check_next(&Tok::Then)?;
            let block = self.block()?;
            clauses.push(IfClause { cond, block });
            if self.tok.tok != Tok::Elseif {
                break;
            }
        }
        let else_block = if self.test_next(&Tok::Else)? {
            Some(self.block()?)
        } else {
            None
        };
        self.check_match(&Tok::End, &Tok::If, line)?;
        Ok(StatKind::If(Box::new(IfStat {
            clauses,
            else_block,
        })))
    }

    fn while_stat(&mut self, line: u32) -> PResult<StatKind> {
        self.next()?;
        let cond = self.expr()?;
        self.enter_block(true);
        self.check_next(&Tok::Do)?;
        let body = self.block()?;
        self.check_match(&Tok::End, &Tok::While, line)?;
        self.leave_block()?;
        Ok(StatKind::While(Box::new(WhileStat { cond, body })))
    }

    fn repeat_stat(&mut self, line: u32) -> PResult<StatKind> {
        self.enter_block(true); // loop block
        self.enter_block(false); // scope block
        self.next()?;
        let body = self.stat_list()?;
        self.check_match(&Tok::Until, &Tok::Repeat, line)?;
        let cond = self.expr()?; // inside the scope block
        self.leave_block()?;
        self.leave_block()?;
        Ok(StatKind::Repeat(Box::new(RepeatStat { body, cond })))
    }

    fn for_stat(&mut self, line: u32) -> PResult<StatKind> {
        self.enter_block(true);
        self.next()?;
        let var = self.check_name()?;
        let kind = match &self.tok.tok {
            Tok::Char(b'=') => self.for_num(var)?,
            Tok::Char(b',') | Tok::In => self.for_list(var)?,
            _ => return Err(self.syntax_error("'=' or 'in' expected")),
        };
        self.check_match(&Tok::End, &Tok::For, line)?;
        self.leave_block()?;
        Ok(kind)
    }

    fn for_control(&mut self, name: Name) -> LocalDecl {
        let decl = self.new_local(name, VarKind::Const);
        self.locals[decl.id.0 as usize].kind = LocalKind::ForControl;
        decl
    }

    fn for_num(&mut self, var: Name) -> PResult<StatKind> {
        self.new_hidden("(for state)");
        self.new_hidden("(for state)");
        let var = self.for_control(var);
        self.check_next(&Tok::Char(b'='))?;
        let start = self.expr()?;
        self.check_next(&Tok::Char(b','))?;
        let limit = self.expr()?;
        let step = if self.test_next(&Tok::Char(b','))? {
            Some(self.expr()?)
        } else {
            None
        };
        self.adjust_local_vars(2)?;
        let body = self.for_body(1)?;
        Ok(StatKind::NumericFor(Box::new(NumericFor {
            var,
            start,
            limit,
            step,
            body,
        })))
    }

    fn for_list(&mut self, first: Name) -> PResult<StatKind> {
        self.new_hidden("(for state)");
        self.new_hidden("(for state)");
        self.new_hidden("(for state)");
        let mut vars = vec![self.for_control(first)];
        while self.test_next(&Tok::Char(b','))? {
            let n = self.check_name()?;
            vars.push(self.new_local(n, VarKind::Reg));
        }
        self.check_next(&Tok::In)?;
        let exprs_line = self.line();
        let exprs = self.expr_list()?;
        self.adjust_local_vars(3)?;
        let body = self.for_body(vars.len())?;
        Ok(StatKind::GenericFor(Box::new(GenericFor {
            vars,
            exprs,
            exprs_line,
            body,
        })))
    }

    fn for_body(&mut self, nvars: usize) -> PResult<Block> {
        self.check_next(&Tok::Do)?;
        self.enter_block(false);
        self.adjust_local_vars(nvars)?;
        let b = self.block()?;
        self.leave_block()?;
        Ok(b)
    }

    fn func_stat(&mut self, line: u32) -> PResult<StatKind> {
        self.next()?; // skip FUNCTION
        // funcname -> NAME {'.' NAME} [':' NAME]
        let base = self.single_var()?;
        let mut fields = Vec::new();
        while self.is_char(b'.') {
            self.next()?;
            fields.push(self.check_name()?);
        }
        let method = if self.is_char(b':') {
            self.next()?;
            Some(self.check_name()?)
        } else {
            None
        };
        if fields.is_empty() && method.is_none() {
            // Indexed targets are never read-only.
            self.check_readonly(&base)?;
        }
        let self_name = method.clone();
        let body = self.body(self_name, line)?;
        let ExprKind::Name(base) = base.kind else {
            unreachable!("single_var returns a name")
        };
        Ok(StatKind::Function {
            name: Box::new(FuncName {
                base,
                fields,
                method,
            }),
            body,
        })
    }

    fn local_func(&mut self) -> PResult<StatKind> {
        let name = self.check_name()?;
        let var = self.new_local(name, VarKind::Reg);
        self.adjust_local_vars(1)?;
        let line = self.line();
        let body = self.body(None, line)?;
        Ok(StatKind::LocalFunction { var, body })
    }

    /// attrib -> ['<' NAME '>']
    fn var_attribute(&mut self, default: Option<Attrib>) -> PResult<Option<Attrib>> {
        if self.test_next(&Tok::Char(b'<'))? {
            let start = self.tok.span;
            let name = self.check_name()?;
            self.check_next(&Tok::Char(b'>'))?;
            match &*name.name {
                "const" => Ok(Some(Attrib::Const)),
                "close" => Ok(Some(Attrib::Close)),
                other => Err(self.sem_error(
                    format!("unknown attribute '{other}'"),
                    self.span_from(start),
                )),
            }
        } else {
            Ok(default)
        }
    }

    fn local_stat(&mut self) -> PResult<StatKind> {
        let default = self.var_attribute(None)?;
        let mut vars = Vec::new();
        let mut has_close = false;
        loop {
            let name = self.check_name()?;
            let attrib = self.var_attribute(default)?;
            let kind = match attrib {
                None => VarKind::Reg,
                Some(Attrib::Const) => VarKind::Const,
                Some(Attrib::Close) => VarKind::Close,
            };
            let decl = self.new_local(name, kind);
            if kind == VarKind::Close {
                if has_close {
                    return Err(self.sem_error(
                        "multiple to-be-closed variables in local list",
                        decl.name.span,
                    ));
                }
                has_close = true;
            }
            vars.push(LocalVar { decl, attrib });
            if !self.test_next(&Tok::Char(b','))? {
                break;
            }
        }
        let values = if self.test_next(&Tok::Char(b'='))? {
            self.expr_list()?
        } else {
            Vec::new()
        };
        let nvars = vars.len();
        let last = vars.last().expect("at least one variable");
        let ctc = if nvars == values.len() && last.attrib == Some(Attrib::Const) {
            let locals = &self.locals;
            fold::const_value(values.last().expect("values"), &|id: LocalId| {
                &locals[id.0 as usize].kind
            })
        } else {
            None
        };
        match ctc {
            Some(c) => {
                let id = last.decl.id;
                self.locals[id.0 as usize].kind = LocalKind::CompileTimeConst(c);
                self.actvar.last_mut().expect("declared").kind = VarKind::CompileTimeConst;
                self.adjust_local_vars(nvars - 1)?;
                self.fs_mut().nactvar += 1;
            }
            None => self.adjust_local_vars(nvars)?,
        }
        Ok(StatKind::Local { vars, values })
    }

    fn global_attribute(&mut self, default: VarKind) -> PResult<VarKind> {
        let start = self.tok.span;
        let d = match default {
            VarKind::GlobalConst => Some(Attrib::Const),
            _ => None,
        };
        match self.var_attribute(d)? {
            Some(Attrib::Close) => Err(self.sem_error(
                "global variables cannot be to-be-closed",
                self.span_from(start),
            )),
            Some(Attrib::Const) => Ok(VarKind::GlobalConst),
            None => Ok(VarKind::Global),
        }
    }

    fn global_stat_func(&mut self, line: u32) -> PResult<StatKind> {
        self.next()?; // skip 'global'
        if self.test_next(&Tok::Function)? {
            self.global_func(line)
        } else {
            self.global_stat()
        }
    }

    fn global_stat(&mut self) -> PResult<StatKind> {
        let default = self.global_attribute(VarKind::Global)?;
        if self.test_next(&Tok::Char(b'*'))? {
            self.new_global(None, default);
            self.fs_mut().nactvar += 1;
            return Ok(StatKind::GlobalAll {
                is_const: default == VarKind::GlobalConst,
            });
        }
        let mut vars = Vec::new();
        loop {
            let name = self.check_name()?;
            let kind = self.global_attribute(default)?;
            self.new_global(Some(&name), kind);
            vars.push(GlobalVar {
                name,
                is_const: kind == VarKind::GlobalConst,
            });
            if !self.test_next(&Tok::Char(b','))? {
                break;
            }
        }
        let nvars = vars.len();
        let (values, env) = if self.test_next(&Tok::Char(b'='))? {
            // initglobal: each target resolves `_ENV` before the values are read, one
            // nesting level per variable.
            let mut env = None;
            for v in &vars {
                env = Some(self.build_global(&v.name.name, v.name.span)?);
                self.enter_level()?;
            }
            let values = self.expr_list()?;
            for _ in 0..nvars {
                self.leave_level();
            }
            (values, env)
        } else {
            (Vec::new(), None)
        };
        self.fs_mut().nactvar += nvars;
        Ok(StatKind::Global { vars, values, env })
    }

    fn global_func(&mut self, _line: u32) -> PResult<StatKind> {
        let name = self.check_name()?;
        self.new_global(Some(&name), VarKind::Global);
        self.fs_mut().nactvar += 1;
        let env = self.build_global(&name.name, name.span)?;
        let line = self.line();
        let body = self.body(None, line)?;
        Ok(StatKind::GlobalFunction { name, env, body })
    }

    fn label_stat(
        &mut self,
        name: Name,
        line: u32,
        start: Span,
        out: &mut Vec<Stat>,
    ) -> PResult<()> {
        self.check_next(&Tok::DbColon)?;
        let index = out.len();
        out.push(Stat {
            kind: StatKind::Label(name.clone()),
            span: self.span_from(start),
        });
        // Skip other no-op statements, so a label followed only by them is "last".
        while self.is_char(b';') || self.tok.tok == Tok::DbColon {
            self.statement(out)?;
        }
        // checkrepeated
        let first = self.fs().first_label;
        if let Some(lb) = self.labels[first..].iter().find(|l| l.name == name.name) {
            let msg = format!("label '{}' already defined on line {}", name.name, lb.line);
            return Err(self.sem_error(msg, out[index].span));
        }
        let nactvar = if self.block_follow(false) {
            // Last statement of the block: the block's locals are out of scope here.
            self.fs().blocks.last().expect("open block").nactvar
        } else {
            self.fs().nactvar
        };
        self.labels.push(LabelDesc {
            name: name.name,
            line,
            nactvar,
        });
        Ok(())
    }

    fn break_stat(&mut self, _line: u32) -> PResult<StatKind> {
        if !self.fs().blocks.iter().any(|b| b.is_loop) {
            return Err(self.syntax_error("break outside loop"));
        }
        self.next()?;
        Ok(StatKind::Break)
    }

    fn ret_stat(&mut self) -> PResult<Return> {
        let start = self.tok.span;
        self.next()?; // skip RETURN
        let values = if self.block_follow(true) || self.is_char(b';') {
            Vec::new()
        } else {
            self.expr_list()?
        };
        self.test_next(&Tok::Char(b';'))?;
        Ok(Return {
            values,
            span: self.span_from(start),
        })
    }

    fn expr_stat(&mut self, _line: u32) -> PResult<StatKind> {
        let first = self.suffixed_exp()?;
        if self.is_char(b'=') || self.is_char(b',') {
            self.rest_assign(vec![first])
        } else if matches!(first.kind, ExprKind::Call(_) | ExprKind::MethodCall(_)) {
            Ok(StatKind::Call(first))
        } else {
            Err(self.syntax_error("syntax error"))
        }
    }

    /// restassign -> ',' suffixedexp restassign | '=' explist
    fn rest_assign(&mut self, mut targets: Vec<Expr>) -> PResult<StatKind> {
        loop {
            let last = targets.last().expect("a target");
            if !matches!(last.kind, ExprKind::Name(_) | ExprKind::Index { .. }) {
                return Err(self.syntax_error("syntax error"));
            }
            self.check_readonly(last)?;
            if !self.test_next(&Tok::Char(b','))? {
                break;
            }
            targets.push(self.suffixed_exp()?);
            self.enter_level()?;
        }
        self.check_next(&Tok::Char(b'='))?;
        let values = self.expr_list()?;
        for _ in 1..targets.len() {
            self.leave_level();
        }
        Ok(StatKind::Assign { targets, values })
    }

    // ---------------------------------------------------------------------------------
    // Grammar: functions

    /// body -> '(' parlist ')' block END. `method` is the method name for
    /// `function a:m()`, which adds the implicit `self` parameter.
    fn body(&mut self, method: Option<Name>, line: u32) -> PResult<Box<FuncBody>> {
        let start = self.tok.span;
        let mut f = self.body_head(method, line)?;
        f.block = self.stat_list()?;
        f.last_line_defined = self.line();
        self.check_match(&Tok::End, &Tok::Function, line)?;
        f.upvalues = self.close_func()?;
        f.span = self.span_from(start);
        Ok(f)
    }

    /// Opens the function and parses its parameter list.
    fn body_head(&mut self, method: Option<Name>, line: u32) -> PResult<Box<FuncBody>> {
        self.open_func(line);
        self.check_next(&Tok::Char(b'('))?;
        let mut params = Vec::new();
        let is_method = method.is_some();
        if let Some(m) = method {
            let self_name = Name {
                name: "self".into(),
                span: m.span,
            };
            params.push(self.new_local(self_name, VarKind::Reg));
            self.adjust_local_vars(1)?;
        }
        let vararg = self.par_list(&mut params)?;
        self.check_next(&Tok::Char(b')'))?;
        Ok(Box::new(FuncBody {
            params,
            is_method,
            vararg,
            block: Block {
                stats: Vec::new(),
                ret: None,
            },
            upvalues: Vec::new(),
            line_defined: line,
            last_line_defined: 0,
            span: Span::default(),
        }))
    }

    fn par_list(&mut self, params: &mut Vec<LocalDecl>) -> PResult<Option<Vararg>> {
        let mut nparams = 0;
        let mut vararg = None;
        if !self.is_char(b')') {
            loop {
                match &self.tok.tok {
                    Tok::Name(_) => {
                        let n = self.check_name()?;
                        params.push(self.new_local(n, VarKind::Reg));
                        nparams += 1;
                    }
                    Tok::Dots => {
                        self.next()?;
                        let name = if matches!(self.tok.tok, Tok::Name(_)) {
                            let n = self.check_name()?;
                            Some(self.new_local(n, VarKind::VarargTable))
                        } else {
                            self.new_hidden("(vararg table)");
                            None
                        };
                        vararg = Some(Vararg { name });
                        break;
                    }
                    _ => return Err(self.syntax_error("<name> or '...' expected")),
                }
                if !self.test_next(&Tok::Char(b','))? {
                    break;
                }
            }
        }
        self.adjust_local_vars(nparams)?;
        if vararg.is_some() {
            self.fs_mut().is_vararg = true;
            self.adjust_local_vars(1)?;
        }
        Ok(vararg)
    }

    // ---------------------------------------------------------------------------------
    // Grammar: expressions

    fn expr_list(&mut self) -> PResult<Vec<Expr>> {
        let mut v = vec![self.expr()?];
        while self.test_next(&Tok::Char(b','))? {
            v.push(self.expr()?);
        }
        Ok(v)
    }

    fn expr(&mut self) -> PResult<Expr> {
        self.sub_expr(0)
    }

    /// subexpr -> (simpleexp | unop subexpr) { binop subexpr }, where each binop binds
    /// tighter than `limit`.
    ///
    /// The expression functions are split so that the ones on the recursion path keep
    /// small stack frames, which matters for deeply nested input in debug builds.
    fn sub_expr(&mut self, limit: u8) -> PResult<Expr> {
        self.enter_level()?;
        let start = self.tok.span;
        let e = match unary_op(&self.tok.tok) {
            Some(op) => self.unary_exp(op, start)?,
            None => self.simple_exp()?,
        };
        let e = self.binary_tail(e, start, limit)?;
        self.leave_level();
        Ok(e)
    }

    fn unary_exp(&mut self, op: UnOp, start: Span) -> PResult<Expr> {
        let line = self.line();
        self.next()?;
        let operand = self.sub_expr(UNARY_PRIORITY)?;
        Ok(Expr {
            kind: ExprKind::Unary {
                op,
                operand: Box::new(operand),
                line,
            },
            span: self.span_from(start),
        })
    }

    fn binary_tail(&mut self, mut e: Expr, start: Span, limit: u8) -> PResult<Expr> {
        while let Some(op) = binary_op(&self.tok.tok) {
            let (left, right) = op.priority();
            if left <= limit {
                break;
            }
            let line = self.line();
            self.next()?;
            let rhs = self.sub_expr(right)?;
            e = Expr {
                kind: ExprKind::Binary {
                    op,
                    lhs: Box::new(e),
                    rhs: Box::new(rhs),
                    line,
                },
                span: self.span_from(start),
            };
        }
        Ok(e)
    }

    fn simple_exp(&mut self) -> PResult<Expr> {
        let start = self.tok.span;
        let kind = match &self.tok.tok {
            Tok::Float(f) => ExprKind::Float(*f),
            Tok::Int(i) => ExprKind::Int(*i),
            Tok::Str(s, _) => ExprKind::Str(s.clone()),
            Tok::Nil => ExprKind::Nil,
            Tok::True => ExprKind::True,
            Tok::False => ExprKind::False,
            Tok::Dots => {
                if !self.fs().is_vararg {
                    return Err(self.syntax_error("cannot use '...' outside a vararg function"));
                }
                ExprKind::Vararg
            }
            Tok::Char(b'{') => return self.table_exp(start),
            Tok::Function => return self.function_exp(start),
            _ => return self.suffixed_exp(),
        };
        self.next()?;
        Ok(Expr { kind, span: start })
    }

    fn table_exp(&mut self, start: Span) -> PResult<Expr> {
        let fields = self.constructor()?;
        Ok(Expr {
            kind: ExprKind::Table(fields),
            span: self.span_from(start),
        })
    }

    fn function_exp(&mut self, start: Span) -> PResult<Expr> {
        self.next()?;
        let line = self.line();
        let mut body = self.body(None, line)?;
        body.span = self.span_from(start);
        Ok(Expr {
            kind: ExprKind::Function(body),
            span: self.span_from(start),
        })
    }

    fn primary_exp(&mut self) -> PResult<Expr> {
        match &self.tok.tok {
            Tok::Char(b'(') => {
                let start = self.tok.span;
                let line = self.line();
                self.next()?;
                let e = self.expr()?;
                self.check_match(&Tok::Char(b')'), &Tok::Char(b'('), line)?;
                Ok(Expr {
                    kind: ExprKind::Paren(Box::new(e)),
                    span: self.span_from(start),
                })
            }
            Tok::Name(_) => self.single_var(),
            _ => Err(self.syntax_error("unexpected symbol")),
        }
    }

    fn suffixed_exp(&mut self) -> PResult<Expr> {
        let start = self.tok.span;
        let e = self.primary_exp()?;
        self.suffixes(e, start)
    }

    /// { '.' NAME | '[' exp ']' | ':' NAME funcargs | funcargs }
    fn suffixes(&mut self, mut e: Expr, start: Span) -> PResult<Expr> {
        loop {
            let kind = match &self.tok.tok {
                Tok::Char(b'.') => self.field_suffix(e)?,
                Tok::Char(b'[') => self.index_suffix(e)?,
                Tok::Char(b':') => self.method_suffix(e)?,
                Tok::Char(b'(' | b'{') | Tok::Str(..) => self.call_suffix(e)?,
                _ => return Ok(e),
            };
            e = Expr {
                kind,
                span: self.span_from(start),
            };
        }
    }

    fn field_suffix(&mut self, obj: Expr) -> PResult<ExprKind> {
        self.next()?;
        let name = self.check_name()?;
        let key = Expr {
            kind: ExprKind::Str(name.name.as_bytes().into()),
            span: name.span,
        };
        Ok(ExprKind::Index {
            obj: Box::new(obj),
            key: Box::new(key),
        })
    }

    fn index_suffix(&mut self, obj: Expr) -> PResult<ExprKind> {
        self.next()?;
        let key = self.expr()?;
        self.check_next(&Tok::Char(b']'))?;
        Ok(ExprKind::Index {
            obj: Box::new(obj),
            key: Box::new(key),
        })
    }

    fn method_suffix(&mut self, obj: Expr) -> PResult<ExprKind> {
        self.next()?;
        let method = self.check_name()?;
        let (args, line) = self.func_args()?;
        Ok(ExprKind::MethodCall(Box::new(MethodCall {
            obj,
            method,
            args,
            line,
        })))
    }

    fn call_suffix(&mut self, func: Expr) -> PResult<ExprKind> {
        let (args, line) = self.func_args()?;
        Ok(ExprKind::Call(Box::new(Call { func, args, line })))
    }

    fn func_args(&mut self) -> PResult<(Vec<Expr>, u32)> {
        let line = self.line();
        let start = self.tok.span;
        let args = match &self.tok.tok {
            Tok::Char(b'(') => {
                self.next()?;
                let args = if self.is_char(b')') {
                    Vec::new()
                } else {
                    self.expr_list()?
                };
                self.check_match(&Tok::Char(b')'), &Tok::Char(b'('), line)?;
                args
            }
            Tok::Char(b'{') => {
                let fields = self.constructor()?;
                vec![Expr {
                    kind: ExprKind::Table(fields),
                    span: self.span_from(start),
                }]
            }
            Tok::Str(s, _) => {
                let e = Expr {
                    kind: ExprKind::Str(s.clone()),
                    span: start,
                };
                self.next()?;
                vec![e]
            }
            _ => return Err(self.syntax_error("function arguments expected")),
        };
        Ok((args, line))
    }

    /// constructor -> '{' [ field { sep field } [sep] ] '}'
    fn constructor(&mut self) -> PResult<Vec<Field>> {
        let line = self.line();
        self.check_next(&Tok::Char(b'{'))?;
        let mut fields = Vec::new();
        loop {
            if self.is_char(b'}') {
                break;
            }
            fields.push(self.field()?);
            if !(self.test_next(&Tok::Char(b','))? || self.test_next(&Tok::Char(b';'))?) {
                break;
            }
        }
        self.check_match(&Tok::Char(b'}'), &Tok::Char(b'{'), line)?;
        Ok(fields)
    }

    fn field(&mut self) -> PResult<Field> {
        match &self.tok.tok {
            Tok::Name(_) => {
                if *self.lookahead()? != Tok::Char(b'=') {
                    Ok(Field::Positional(self.expr()?))
                } else {
                    let name = self.check_name()?;
                    self.check_next(&Tok::Char(b'='))?;
                    Ok(Field::Named(name, self.expr()?))
                }
            }
            Tok::Char(b'[') => {
                self.next()?;
                let key = self.expr()?;
                self.check_next(&Tok::Char(b']'))?;
                self.check_next(&Tok::Char(b'='))?;
                Ok(Field::Keyed(key, self.expr()?))
            }
            _ => Ok(Field::Positional(self.expr()?)),
        }
    }
}

fn describe(t: &Tok) -> String {
    String::from_utf8_lossy(&t.describe()).into_owned()
}

fn unary_op(t: &Tok) -> Option<UnOp> {
    Some(match t {
        Tok::Not => UnOp::Not,
        Tok::Char(b'-') => UnOp::Neg,
        Tok::Char(b'~') => UnOp::BNot,
        Tok::Char(b'#') => UnOp::Len,
        _ => return None,
    })
}

fn binary_op(t: &Tok) -> Option<BinOp> {
    Some(match t {
        Tok::Char(b'+') => BinOp::Add,
        Tok::Char(b'-') => BinOp::Sub,
        Tok::Char(b'*') => BinOp::Mul,
        Tok::Char(b'%') => BinOp::Mod,
        Tok::Char(b'^') => BinOp::Pow,
        Tok::Char(b'/') => BinOp::Div,
        Tok::IDiv => BinOp::IDiv,
        Tok::Char(b'&') => BinOp::BAnd,
        Tok::Char(b'|') => BinOp::BOr,
        Tok::Char(b'~') => BinOp::BXor,
        Tok::Shl => BinOp::Shl,
        Tok::Shr => BinOp::Shr,
        Tok::Concat => BinOp::Concat,
        Tok::Ne => BinOp::Ne,
        Tok::Eq => BinOp::Eq,
        Tok::Char(b'<') => BinOp::Lt,
        Tok::Le => BinOp::Le,
        Tok::Char(b'>') => BinOp::Gt,
        Tok::Ge => BinOp::Ge,
        Tok::And => BinOp::And,
        Tok::Or => BinOp::Or,
        _ => return None,
    })
}
