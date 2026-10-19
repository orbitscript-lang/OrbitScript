//! Error recovery (`ParseOptions::recover`) and comment keeping (`keep_trivia`), for tools.

use orbit_lua_syntax::ast::*;
use orbit_lua_syntax::lexer::Lexer;
use orbit_lua_syntax::token::TokenKind;
use orbit_lua_syntax::{
    DiagnosticKind, ParseOptions, Span, StringPool, parse_recovering, parse_with, to_hir,
};
use std::path::{Path, PathBuf};

fn options() -> ParseOptions {
    ParseOptions {
        skip_comment_line: true,
        ..ParseOptions::default()
    }
}

fn recovering(src: &str) -> (Ast, Vec<String>) {
    let (ast, errors) = parse_recovering("t.lua", src.as_bytes(), &options());
    let messages = errors.iter().map(|d| d.to_lua_message("t.lua")).collect();
    (ast, messages)
}

/// The chunk's top-level declarations, as `function f`, `local x` and so on.
fn top_items(ast: &Ast) -> Vec<String> {
    let s = |p: &StringPool, n: Name| String::from_utf8_lossy(p.get(n.sym)).into_owned();
    let p = &ast.strings;
    let mut out = Vec::new();
    for st in &ast.chunk.body.stmts {
        match &st.kind {
            StmtKind::Function { path, .. } => {
                let mut name = s(p, path.base);
                for f in &path.fields {
                    name = format!("{name}.{}", s(p, *f));
                }
                if let Some(m) = path.method {
                    name = format!("{name}:{}", s(p, m));
                }
                out.push(format!("function {name}"));
            }
            StmtKind::LocalFunction { name, .. } => {
                out.push(format!("local function {}", s(p, *name)))
            }
            StmtKind::Local { names, .. } => {
                for n in names {
                    out.push(format!("local {}", s(p, n.name)));
                }
            }
            _ => {}
        }
    }
    out
}

fn kinds(stmts: &[Stmt]) -> Vec<&'static str> {
    stmts
        .iter()
        .map(|s| match &s.kind {
            StmtKind::Local { .. } => "local",
            StmtKind::Assign { .. } => "assign",
            StmtKind::Call(_) => "call",
            StmtKind::Function { .. } | StmtKind::LocalFunction { .. } => "function",
            StmtKind::If { .. } => "if",
            StmtKind::Error => "error",
            _ => "other",
        })
        .collect()
}

#[test]
fn several_errors_in_one_chunk() {
    let src = "local function a() local x = 1 + end\n\
               local function b() end\n\
               local y = (1, 2)\n\
               local s = \"bad\\q\"\n\
               function c() end\n";
    let (ast, errors) = recovering(src);
    assert_eq!(
        errors,
        [
            "t.lua:1: unexpected symbol near 'end'",
            "t.lua:3: ')' expected near ','",
            "t.lua:4: invalid escape sequence near '\"bad\\q'",
        ]
    );
    assert_eq!(
        top_items(&ast),
        [
            "local function a",
            "local function b",
            "local y",
            "local s",
            "function c"
        ],
    );
}

/// Without a missing `end` that indentation places earlier, the first error is the one
/// `parse_with` gives (the reference's), whatever `recover` says.
#[test]
fn the_first_error_is_the_reference_one() {
    let recover = ParseOptions {
        recover: true,
        ..options()
    };
    for src in [
        "x = = 1",
        "local function f() return 1 print(2) end",
        "if x then",
        "for i = 1 print(i) end",
        "f(1,, 2)",
        "local t = {1, 2",
        "x = 'abc\ny = 2",
        "end",
    ] {
        let plain = parse_with("t.lua", src.as_bytes(), &options()).unwrap_err();
        assert_eq!(
            parse_with("t.lua", src.as_bytes(), &recover).unwrap_err(),
            plain
        );
        let (_, errors) = parse_recovering("t.lua", src.as_bytes(), &options());
        assert_eq!(errors[0], plain, "{src}");
    }
}

#[test]
fn a_broken_statement_becomes_an_error_node() {
    let (ast, errors) = recovering("local a = 1\nx y z\nlocal c = 3\n");
    assert_eq!(errors, ["t.lua:2: syntax error near 'y'"]);
    assert_eq!(kinds(&ast.chunk.body.stmts), ["local", "error", "local"]);
    // A missing value keeps the statement.
    let (ast, errors) = recovering("local a = 1\nlocal b = )\nlocal c = 3\n");
    assert_eq!(errors, ["t.lua:2: unexpected symbol near ')'"]);
    assert_eq!(kinds(&ast.chunk.body.stmts)[..2], ["local", "local"]);
}

#[test]
fn missing_then_and_do_are_taken_as_there() {
    let (ast, errors) = recovering("if x\n  print(1)\nend\nwhile y\n  print(2)\nend\nz = 1\n");
    assert_eq!(
        errors,
        [
            "t.lua:2: 'then' expected near 'print'",
            "t.lua:5: 'do' expected near 'print'",
        ]
    );
    assert_eq!(kinds(&ast.chunk.body.stmts), ["if", "other", "assign"]);
}

#[test]
fn a_missing_end_before_a_function_at_column_zero() {
    let src = "function a()\n  if x then\n    y()\n  \nfunction b() end\nlocal function c()\n  return 1\n\nprint(1)\n";
    let (ast, errors) = recovering(src);
    assert_eq!(
        errors,
        [
            "t.lua:5: 'end' expected (to close 'if' at line 2) near 'function'",
            "t.lua:9: 'end' expected (to close 'function' at line 6) near 'print'",
        ]
    );
    assert_eq!(
        top_items(&ast),
        ["function a", "function b", "local function c"]
    );
}

#[test]
fn statements_after_return_and_stray_ends() {
    let (ast, errors) =
        recovering("local function f()\n  return 1\n  print(2)\nend\nend\nlocal g = 1\n");
    assert_eq!(errors.len(), 2, "{errors:?}");
    assert_eq!(top_items(&ast), ["local function f", "local g"]);
}

#[test]
fn unexpected_eof_kind() {
    for src in [
        "function f()",
        "local s = \"abc",
        "--[[ never closed",
        "local s = [[ never",
        "x = 1 +",
        "if x then",
        "t = {",
    ] {
        let d = parse_with("t.lua", src.as_bytes(), &options()).unwrap_err();
        assert_eq!(d.kind, DiagnosticKind::UnexpectedEof, "{src}: {d}");
        let (_, errors) = parse_recovering("t.lua", src.as_bytes(), &options());
        assert_eq!(
            errors.last().unwrap().kind,
            DiagnosticKind::UnexpectedEof,
            "{src}"
        );
    }
    for src in [
        "x = )",
        "local x = (1,",
        "local s = 'abc\nx = 1",
        "x = 1 end",
    ] {
        let d = parse_with("t.lua", src.as_bytes(), &options()).unwrap_err();
        assert_eq!(d.kind, DiagnosticKind::Other, "{src}");
    }
}

#[test]
fn lowering_rejects_error_nodes() {
    let (ast, _) = parse_recovering("t.lua", b"x y", &options());
    let d = orbit_lua_syntax::lower(ast, b"x y").unwrap_err();
    assert_eq!(d.message, "this code has a syntax error, so it can't run");
    let recover = ParseOptions {
        recover: true,
        ..options()
    };
    assert!(to_hir("t.lua", b"x y", &recover).is_err());
}

#[test]
fn keep_trivia_keeps_every_comment() {
    let src = "-- plain\n--- doc\n---- not doc\n--[==[ block\n]==]\nx = 1 --[[ inline ]] -- tail\n";
    let all = ParseOptions {
        keep_trivia: true,
        ..options()
    };
    let ast = parse_with("t.lua", src.as_bytes(), &all).unwrap();
    let got: Vec<_> = ast
        .comments
        .iter()
        .map(|c| (c.kind, &src[c.span.start as usize..c.span.end as usize]))
        .collect();
    assert_eq!(
        got,
        [
            (CommentKind::Line, "-- plain"),
            (CommentKind::Doc, "--- doc"),
            (CommentKind::Line, "---- not doc"),
            (CommentKind::Block, "--[==[ block\n]==]"),
            (CommentKind::Block, "--[[ inline ]]"),
            (CommentKind::Line, "-- tail"),
        ]
    );
    assert!(
        parse_with("t.lua", src.as_bytes(), &options())
            .unwrap()
            .comments
            .is_empty()
    );
}

// ---- corpora ----------------------------------------------------------------------------------

fn lua_files(dirs: &[&str]) -> Vec<PathBuf> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, out);
            } else if path.extension().is_some_and(|e| e == "lua") {
                out.push(path);
            }
        }
    }
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut files = Vec::new();
    for dir in dirs {
        walk(&root.join(dir), &mut files);
    }
    files.sort();
    files
}

/// The small programs: everything but the official suite.
fn small_corpus() -> Vec<PathBuf> {
    let files = lua_files(&["tests/diff", "tests/first-light", "bench", "examples"]);
    assert!(files.len() >= 50, "found only {}", files.len());
    files
}

#[test]
fn valid_files_parse_identically_with_recovery() {
    let mut files = small_corpus();
    files.extend(lua_files(&["tests/lua-5.5/suite"]));
    let mut n = 0;
    for path in files {
        let src = std::fs::read(&path).unwrap();
        let name = path.display().to_string();
        let Ok(plain) = parse_with(&name, &src, &options()) else {
            continue; // a test of syntax errors
        };
        let (recovered, errors) = parse_recovering(&name, &src, &options());
        assert!(errors.is_empty(), "{name}: {errors:?}");
        assert_eq!(
            format!("{:?}", plain.chunk),
            format!("{:?}", recovered.chunk),
            "{name}"
        );
        n += 1;
    }
    assert!(n >= 100, "only {n} files");
}

/// Every token of `src` with the line it starts on.
fn tokens(src: &[u8]) -> Vec<(u32, usize, usize)> {
    let mut lx = Lexer::new(src);
    lx.skip_comment_line();
    let mut out = Vec::new();
    loop {
        let Ok(t) = lx.next_token() else { return out };
        if t.kind == TokenKind::Eof {
            return out;
        }
        out.push((
            lx.line_of(t.span.start),
            t.span.start as usize,
            t.span.end as usize,
        ));
    }
}

/// Deleting the first or the last token of a line keeps every top-level declaration before
/// and after the break.
#[test]
fn broken_files_keep_the_declarations_around_the_break() {
    let mut cases = 0;
    let mut failures = Vec::new();
    for path in small_corpus() {
        let src = std::fs::read(&path).unwrap();
        let name = path.display().to_string();
        let Ok(orig) = parse_with(&name, &src, &options()) else {
            continue;
        };
        let spans: Vec<Span> = orig
            .chunk
            .body
            .stmts
            .iter()
            .filter(|s| {
                matches!(
                    s.kind,
                    StmtKind::Function { .. }
                        | StmtKind::LocalFunction { .. }
                        | StmtKind::Local { .. }
                )
            })
            .map(|s| s.span)
            .collect();
        let items = top_items(&orig);
        // `local a, b` gives two items for one statement.
        let mut item_spans = Vec::new();
        for (s, st) in orig
            .chunk
            .body
            .stmts
            .iter()
            .map(|s| (s.span, &s.kind))
            .filter(|(sp, _)| spans.contains(sp))
        {
            let n = match st {
                StmtKind::Local { names, .. } => names.len(),
                _ => 1,
            };
            item_spans.extend(std::iter::repeat_n(s, n));
        }
        let toks = tokens(&src);
        for (i, &(line, start, end)) in toks.iter().enumerate() {
            let first = i == 0 || toks[i - 1].0 != line;
            let last = i + 1 == toks.len() || toks[i + 1].0 != line;
            if !(first || last) {
                continue;
            }
            cases += 1;
            let mut broken = src[..start].to_vec();
            broken.extend_from_slice(&src[end..]);
            let (ast, _) = parse_recovering(&name, &broken, &options());
            let mut have = top_items(&ast);
            for (item, span) in items.iter().zip(&item_spans) {
                if (span.start as usize..span.end as usize).contains(&start) {
                    continue;
                }
                match have.iter().position(|h| h == item) {
                    Some(i) => {
                        have.remove(i);
                    }
                    None => failures.push(format!(
                        "{name}:{line}: deleting `{}` lost `{item}`",
                        String::from_utf8_lossy(&src[start..end])
                    )),
                }
            }
        }
    }
    assert!(cases > 2000, "only {cases} cases");
    assert!(
        failures.is_empty(),
        "{} of {cases} cases lost declarations:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

/// Deleting any one token never panics or hangs, and a chunk cut off after any token either
/// parses or fails with `UnexpectedEof`, as the reference REPL's "incomplete" check needs.
#[test]
fn every_deleted_token_and_every_cut() {
    for path in small_corpus() {
        let src = std::fs::read(&path).unwrap();
        let name = path.display().to_string();
        if parse_with(&name, &src, &options()).is_err() {
            continue;
        }
        for (line, start, end) in tokens(&src) {
            let mut broken = src[..start].to_vec();
            broken.extend_from_slice(&src[end..]);
            parse_recovering(&name, &broken, &options());
            if let Err(d) = parse_with(&name, &src[..end], &options()) {
                assert_eq!(
                    d.kind,
                    DiagnosticKind::UnexpectedEof,
                    "{name}:{line}: cut after `{}`: {d}",
                    String::from_utf8_lossy(&src[start..end])
                );
            }
        }
    }
}

#[test]
fn indentation_shows_what_is_missing() {
    // An unclosed call, table or value before a line indented no deeper.
    let src = "local function f()\n  g(1,\n  local t = {\n  x =\n  return t\nend\nlocal y = 1\n";
    let (ast, errors) = recovering(src);
    assert_eq!(
        errors,
        [
            "t.lua:3: unexpected symbol near 'local'",
            "t.lua:4: '}' expected (to close '{' at line 3) near 'x'",
            "t.lua:5: unexpected symbol near 'return'",
        ]
    );
    assert_eq!(top_items(&ast), ["local function f", "local y"]);
    // Code written flush: only a function at the start of a line ends an unclosed block.
    let src = "while x do\nlocal a = 1\nend\nif y then\nlocal b = 2\nfunction g() end\n";
    let (ast, errors) = recovering(src);
    assert_eq!(
        errors,
        ["t.lua:6: 'end' expected (to close 'if' at line 4) near 'function'"]
    );
    assert_eq!(top_items(&ast), ["function g"]);
}
