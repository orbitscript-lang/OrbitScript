//! Error recovery (`ParseOptions::recover`) and comment keeping (`keep_trivia`), for tools.

use orbit_orb_syntax::ast::*;
use orbit_orb_syntax::lexer::Lexer;
use orbit_orb_syntax::token::TokenKind;
use orbit_orb_syntax::{
    DiagnosticKind, ParseOptions, Span, StringPool, parse_recovering, parse_with, to_hir,
};
use std::path::{Path, PathBuf};

fn recovering(src: &str) -> (Ast, Vec<String>) {
    let (ast, errors) = parse_recovering("t.orb", src.as_bytes(), &ParseOptions::default());
    let messages = errors.iter().map(|d| d.to_message("t.orb")).collect();
    (ast, messages)
}

/// The top-level items, as `fn name`, `struct name`, `impl name` and so on.
fn top_items(ast: &Ast) -> Vec<String> {
    let s = |p: &StringPool, n: Name| String::from_utf8_lossy(p.get(n.sym)).into_owned();
    let p = &ast.strings;
    ast.body
        .stmts
        .iter()
        .filter_map(|st| match &st.kind {
            StmtKind::Item(item) => Some(match &item.kind {
                ItemKind::Fn(f) => format!("fn {}", s(p, f.name)),
                ItemKind::Struct(d) => format!("struct {}", s(p, d.name)),
                ItemKind::Enum(d) => format!("enum {}", s(p, d.name)),
                ItemKind::Impl(d) => format!("impl {}", s(p, d.target)),
                ItemKind::Const { name, .. } => format!("const {}", s(p, *name)),
                ItemKind::TypeAlias { name, .. } => format!("type {}", s(p, *name)),
                ItemKind::Global { name, .. } => format!("global {}", s(p, *name)),
                ItemKind::Use(_) => "use".into(),
                ItemKind::Extern(_) => "extern".into(),
                ItemKind::Error => "error".into(),
            }),
            _ => None,
        })
        .collect()
}

fn kinds(stmts: &[Stmt]) -> Vec<&'static str> {
    stmts
        .iter()
        .map(|s| match &s.kind {
            StmtKind::Let { .. } => "let",
            StmtKind::Expr(_) => "expr",
            StmtKind::Assign { .. } | StmtKind::CompoundAssign { .. } => "assign",
            StmtKind::Scope(_) => "scope",
            StmtKind::Item(_) => "item",
            StmtKind::Error => "error",
        })
        .collect()
}

#[test]
fn several_errors_in_one_file() {
    let src = "fn a() { let x = 1 + ; }\n\
               fn b() {}\n\
               let y = (1, ;\n\
               struct S { x: int }\n\
               let z = @;\n\
               fn c() {}\n";
    let (ast, errors) = recovering(src);
    assert_eq!(
        errors,
        [
            "t.orb:1: expected an expression, found `;`",
            "t.orb:3: expected an expression, found `;`",
            "t.orb:5: unexpected character `@`",
        ]
    );
    assert_eq!(
        top_items(&ast),
        ["fn a", "fn b", "struct S", "fn c"],
        "every item survives"
    );
}

#[test]
fn a_broken_statement_becomes_an_error_node() {
    let (ast, errors) = recovering("let a = 1;\nlet b = 1 2 3 +* 4;\nlet c = 3;\n");
    // A missing `;`: the rest of the line is skipped.
    assert_eq!(errors, ["t.orb:2: expected `;`, found a number"]);
    assert_eq!(kinds(&ast.body.stmts), ["let", "let", "let"]);
    // A missing value: the `let` stays, with an `Error` expression.
    let (ast, errors) = recovering("let a = 1;\nlet b = )(;\nlet c = 3;\n");
    assert_eq!(errors, ["t.orb:2: expected an expression, found `)`"]);
    assert_eq!(kinds(&ast.body.stmts), ["let", "let", "let"]);
    // Nothing usable: the statement is an `Error` node.
    let (ast, errors) = recovering("let a = 1;\n) x + (;\nlet c = 3;\n");
    assert_eq!(errors, ["t.orb:2: expected an expression, found `)`"]);
    assert_eq!(kinds(&ast.body.stmts), ["let", "error", "let"]);
}

#[test]
fn missing_semicolons_are_reported_and_skipped() {
    let (ast, errors) = recovering("fn f() {\n    let x = 1\n    g(x)\n    x = 2\n    h();\n}\n");
    assert_eq!(
        errors,
        [
            "t.orb:3: expected `;`, found `g`",
            "t.orb:4: expected `;`, found `x`",
            "t.orb:5: expected `;`, found `h`",
        ]
    );
    let StmtKind::Item(item) = &ast.body.stmts[0].kind else {
        panic!()
    };
    let ItemKind::Fn(f) = &item.kind else {
        panic!()
    };
    let body = f.body.as_ref().unwrap();
    assert_eq!(kinds(&body.stmts), ["let", "expr", "assign", "expr"]);
}

#[test]
fn a_missing_close_brace_before_an_item_at_column_zero() {
    let src = "fn a() {\n    let x = 1;\n\nfn b() {}\nimpl P {\n    fn m() {}\n\nstruct Q {}\n";
    let (ast, errors) = recovering(src);
    assert_eq!(
        errors,
        [
            "t.orb:4: expected `}` to close the `{` at line 1, found `fn`",
            "t.orb:8: expected `}` to close the `{` at line 5, found `struct`",
        ]
    );
    assert_eq!(top_items(&ast), ["fn a", "fn b", "impl P", "struct Q"]);
}

#[test]
fn a_broken_impl_member_keeps_the_others() {
    let src = "impl P {\n    fn a(self) {}\n    fn b(self, { }\n    fn c(self) {}\n    oops\n    const D = 1;\n}\nfn after() {}\n";
    let (ast, errors) = recovering(src);
    assert_eq!(errors.len(), 2, "{errors:?}");
    let StmtKind::Item(item) = &ast.body.stmts[0].kind else {
        panic!()
    };
    let ItemKind::Impl(i) = &item.kind else {
        panic!()
    };
    let members: Vec<_> = i
        .members
        .iter()
        .map(|m| match &m.kind {
            ItemKind::Fn(f) => String::from_utf8_lossy(ast.strings.get(f.name.sym)).into_owned(),
            ItemKind::Const { .. } => "const".into(),
            ItemKind::Error => "error".into(),
            _ => panic!(),
        })
        .collect();
    assert_eq!(members, ["a", "error", "c", "error", "const"]);
    assert_eq!(top_items(&ast), ["impl P", "fn after"]);
}

#[test]
fn bad_tokens_get_stand_ins() {
    let (ast, errors) = recovering("let s = \"a\\qb\";\nlet n = 12abc;\nlet t = 1;\n");
    assert_eq!(
        errors,
        [
            "t.orb:1: unknown escape `\\q`",
            "t.orb:2: malformed number `12abc`"
        ]
    );
    assert_eq!(kinds(&ast.body.stmts), ["let", "let", "let"]);
}

#[test]
fn a_broken_template_hole_stays_inside_the_template() {
    let (ast, errors) = recovering("let a = f\"x {1 +} y\";\nlet b = f\"{}\";\nlet c = 1;\n");
    assert_eq!(errors.len(), 2, "{errors:?}");
    assert_eq!(kinds(&ast.body.stmts), ["let", "let", "let"]);
}

#[test]
fn unexpected_eof_kind() {
    let eof = |src: &str| {
        let (_, errors) = parse_recovering("t.orb", src.as_bytes(), &ParseOptions::default());
        errors.last().map(|d| d.kind)
    };
    for src in [
        "fn f() {",
        "let x = (1,",
        "let s = \"abc",
        "/* never closed",
        "let t = f\"{x",
        "impl P {",
        "let x = 1 +",
    ] {
        assert_eq!(eof(src), Some(DiagnosticKind::UnexpectedEof), "{src}");
        // Without recovery, the one error has the same kind.
        let d = parse_with("t.orb", src.as_bytes(), &ParseOptions::default()).unwrap_err();
        assert_eq!(d.kind, DiagnosticKind::UnexpectedEof, "{src}");
    }
    for src in ["let x = );", "fn f() { ) }", "let s = \"\\q\";"] {
        assert_eq!(eof(src), Some(DiagnosticKind::Other), "{src}");
    }
    // Inside a template hole, the end of the hole is not the end of the file.
    let d = parse_with("t.orb", b"let s = f\"{1 +}\";", &ParseOptions::default()).unwrap_err();
    assert_eq!(d.kind, DiagnosticKind::Other);
}

#[test]
fn parse_with_reports_the_first_error_and_lowering_rejects_error_nodes() {
    let options = ParseOptions {
        recover: true,
        ..ParseOptions::default()
    };
    let d = parse_with("t.orb", b"let a = ;\nlet b = ;\n", &options).unwrap_err();
    assert_eq!(d.line, 1);
    let d = to_hir("t.orb", b"let a = ;", &options).unwrap_err();
    assert_eq!(d.message, "expected an expression, found `;`");
    let (ast, _) = parse_recovering("t.orb", b"let a = ;", &ParseOptions::default());
    let d = orbit_orb_syntax::lower(ast).unwrap_err();
    assert_eq!(d.message, "this code has a syntax error, so it can't run");
}

#[test]
fn keep_trivia_keeps_every_comment() {
    let src = "//! Module.\n// plain\n/* block /* nested */ */\n/// Doc.\nfn f() {} //// not doc\n";
    let text = |c: &Comment| (c.kind, &src[c.span.start as usize..c.span.end as usize]);
    let all = ParseOptions {
        keep_trivia: true,
        ..ParseOptions::default()
    };
    let ast = parse_with("t.orb", src.as_bytes(), &all).unwrap();
    let got: Vec<_> = ast.comments.iter().map(text).collect();
    assert_eq!(
        got,
        [
            (CommentKind::InnerDoc, "//! Module."),
            (CommentKind::Line, "// plain"),
            (CommentKind::Block, "/* block /* nested */ */"),
            (CommentKind::Doc, "/// Doc."),
            (CommentKind::Line, "//// not doc"),
        ]
    );
    let StmtKind::Item(item) = &ast.body.stmts[0].kind else {
        panic!()
    };
    assert_eq!(item.docs.len(), 1);
    // Doc comments only.
    let docs = ParseOptions {
        doc_comments: true,
        ..ParseOptions::default()
    };
    let ast = parse_with("t.orb", src.as_bytes(), &docs).unwrap();
    let got: Vec<_> = ast.comments.iter().map(|c| c.kind).collect();
    assert_eq!(got, [CommentKind::InnerDoc, CommentKind::Doc]);
}

// ---- the corpus: every OrbitScript file in the repository ---------------------------------

fn corpus() -> Vec<PathBuf> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if path
                    .file_name()
                    .is_some_and(|n| n == "target" || n == ".git")
                {
                    continue;
                }
                walk(&path, out);
            } else if path.extension().is_some_and(|e| e == "orb") {
                out.push(path);
            }
        }
    }
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut files = Vec::new();
    for dir in ["examples", "bench", "crates"] {
        walk(&root.join(dir), &mut files);
    }
    files.sort();
    assert!(files.len() >= 10, "found only {files:?}");
    files
}

#[test]
fn valid_files_parse_identically_with_recovery() {
    for path in corpus() {
        let src = std::fs::read(&path).unwrap();
        let name = path.display().to_string();
        let plain = parse_with(&name, &src, &ParseOptions::default())
            .unwrap_or_else(|e| panic!("{name}: {e}"));
        let (recovered, errors) = parse_recovering(&name, &src, &ParseOptions::default());
        assert!(errors.is_empty(), "{name}: {errors:?}");
        assert_eq!(
            format!("{:?}", plain.body),
            format!("{:?}", recovered.body),
            "{name}"
        );
    }
}

/// The byte ranges of every token in `src`, with the line each starts on.
fn tokens(src: &[u8]) -> Vec<(u32, usize, usize)> {
    let mut lx = Lexer::new("t.orb", src);
    let mut out = Vec::new();
    loop {
        let t = lx.next_token().expect("corpus files lex");
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

/// Deleting one token per line (the first, then separately the last) never panics, and the
/// top-level items before and after the broken one all survive.
#[test]
fn broken_files_keep_the_items_around_the_break() {
    let mut cases = 0;
    let mut failures = Vec::new();
    for path in corpus() {
        let src = std::fs::read(&path).unwrap();
        let name = path.display().to_string();
        let (orig, _) = parse_recovering(&name, &src, &ParseOptions::default());
        let items: Vec<(String, Span)> = top_items(&orig)
            .into_iter()
            .zip(
                orig.body
                    .stmts
                    .iter()
                    .filter(|s| matches!(s.kind, StmtKind::Item(_)))
                    .map(|s| s.span),
            )
            .collect();
        let toks = tokens(&src);
        let mut picks = Vec::new();
        for (i, t) in toks.iter().enumerate() {
            let first = i == 0 || toks[i - 1].0 != t.0;
            let last = i + 1 == toks.len() || toks[i + 1].0 != t.0;
            if first || last {
                picks.push(*t);
            }
        }
        for (line, start, end) in picks {
            cases += 1;
            let mut broken = src[..start].to_vec();
            broken.extend_from_slice(&src[end..]);
            let (ast, errors) = parse_recovering(&name, &broken, &ParseOptions::default());
            let _ = errors;
            // Items that don't contain the deleted token must all still be there.
            let mut have = top_items(&ast);
            for (item, span) in &items {
                if (span.start as usize..span.end as usize).contains(&start) {
                    continue;
                }
                match have.iter().position(|h| h == item) {
                    Some(i) => {
                        have.remove(i);
                    }
                    None => {
                        let deleted = String::from_utf8_lossy(&src[start..end]);
                        failures.push(format!("{name}:{line}: deleting `{deleted}` lost `{item}`"));
                    }
                }
            }
        }
    }
    assert!(cases > 1000, "only {cases} cases");
    assert!(
        failures.is_empty(),
        "{} of {cases} cases lost items:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

/// Deleting any one token never panics or hangs, and a file cut off after any token either
/// parses or fails with `UnexpectedEof` (what a REPL needs to ask for another line).
#[test]
fn every_deleted_token_and_every_cut() {
    let options = ParseOptions::default();
    for path in corpus() {
        let src = std::fs::read(&path).unwrap();
        let name = path.display().to_string();
        for (line, start, end) in tokens(&src) {
            let mut broken = src[..start].to_vec();
            broken.extend_from_slice(&src[end..]);
            parse_recovering(&name, &broken, &options);
            if let Err(d) = parse_with(&name, &src[..end], &options) {
                assert_eq!(
                    d.kind,
                    DiagnosticKind::UnexpectedEof,
                    "{name}:{line}: cut after `{}`: {}",
                    String::from_utf8_lossy(&src[start..end]),
                    d.message
                );
            }
        }
    }
}
