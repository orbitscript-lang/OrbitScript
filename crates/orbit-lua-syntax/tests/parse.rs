//! Parser tests: error messages as the reference `lua5.5` prints them, and whole programs.

use std::path::{Path, PathBuf};

use orbit_lua_syntax::ast::{BinOp, ExprKind, StmtKind};
use orbit_lua_syntax::{ParseOptions, parse, parse_with, unparse};

fn err(src: &str) -> String {
    match parse("s.lua", src.as_bytes()) {
        Ok(_) => panic!("expected an error for {src:?}"),
        Err(d) => d.to_lua_message("s.lua"),
    }
}

/// Each message was checked against the reference interpreter (Lua 5.5.1).
#[test]
fn syntax_errors_match_the_reference() {
    let cases = [
        ("x = = 1", "s.lua:1: unexpected symbol near '='"),
        (
            "if x then\n",
            "s.lua:2: 'end' expected (to close 'if' at line 1) near <eof>",
        ),
        ("while true do", "s.lua:1: 'end' expected near <eof>"),
        ("x = 1 print(x", "s.lua:1: ')' expected near <eof>"),
        (
            "f(\n1",
            "s.lua:2: ')' expected (to close '(' at line 1) near <eof>",
        ),
        ("x = {1, 2", "s.lua:1: '}' expected near <eof>"),
        ("x = {a b}", "s.lua:1: '}' expected near 'b'"),
        ("x = {,}", "s.lua:1: unexpected symbol near ','"),
        ("break", "s.lua:1: break outside loop near 'break'"),
        (
            "while 1 do end break",
            "s.lua:1: break outside loop near 'break'",
        ),
        (
            "while 1 do local f = function() break end end",
            "s.lua:1: break outside loop near 'break'",
        ),
        (
            "function f() return ... end",
            "s.lua:1: cannot use '...' outside a vararg function near '...'",
        ),
        ("f() = 1", "s.lua:1: syntax error near '='"),
        ("(a) = 1", "s.lua:1: syntax error near '='"),
        ("a.b", "s.lua:1: syntax error near <eof>"),
        ("return 1 print(2)", "s.lua:1: <eof> expected near 'print'"),
        ("return 1;;", "s.lua:1: <eof> expected near ';'"),
        (
            "x = function() end end",
            "s.lua:1: <eof> expected near 'end'",
        ),
        ("local x <foo> = 1", "s.lua:1: unknown attribute 'foo'"),
        ("local x\n<foo> = 1", "s.lua:2: unknown attribute 'foo'"),
        (
            "local <close> x, y",
            "s.lua:1: multiple to-be-closed variables in local list",
        ),
        ("for i = 1 do end", "s.lua:1: ',' expected near 'do'"),
        ("for i do end", "s.lua:1: '=' or 'in' expected near 'do'"),
        ("for a, b = 1, 2 do end", "s.lua:1: 'in' expected near '='"),
        (
            "f = function(1) end",
            "s.lua:1: <name> or '...' expected near '1'",
        ),
        ("local 1 = 2", "s.lua:1: <name> expected near '1'"),
        ("local function", "s.lua:1: <name> expected near <eof>"),
        ("x = a.1", "s.lua:1: unexpected symbol near '.1'"),
        ("x = a:b", "s.lua:1: function arguments expected near <eof>"),
        ("x = a:b.c", "s.lua:1: function arguments expected near '.'"),
        ("function a.b:c.d() end", "s.lua:1: '(' expected near '.'"),
        ("x = 1 +", "s.lua:1: unexpected symbol near <eof>"),
        ("x = \"a\" \"b\"", "s.lua:1: unexpected symbol near '\"b\"'"),
        ("x = 1 'a\\65b'", "s.lua:1: unexpected symbol near ''aAb''"),
        (
            "x = 1 [==[a]]b]==]",
            "s.lua:1: unexpected symbol near '[==[a]]b]==]'",
        ),
        ("x = \u{1}", "s.lua:1: unexpected symbol near '<\\1>'"),
        ("x = @", "s.lua:1: unexpected symbol near '@'"),
        ("x = ~= 1", "s.lua:1: unexpected symbol near '~='"),
        ("global < 3", "s.lua:1: <name> expected near '3'"),
        ("global in", "s.lua:1: syntax error near 'in'"),
        ("global function", "s.lua:1: <name> expected near <eof>"),
    ];
    for (src, expected) in cases {
        assert_eq!(err(src), expected, "source: {src:?}");
    }
}

#[test]
fn global_is_contextual() {
    for src in [
        "global = print global(1)",
        "global = {} global.x = 1",
        "local global = 3",
        "global, y = 1, 2",
        "global x, y",
        "global <const> x = 1",
        "global <const> *",
        "global function f() end",
    ] {
        parse("s.lua", src.as_bytes()).unwrap_or_else(|d| panic!("{src}: {d}"));
    }
    let ast = parse("s.lua", b"global = 1 global x").unwrap();
    assert!(matches!(
        ast.chunk.body.stmts[0].kind,
        StmtKind::Assign { .. }
    ));
    assert!(matches!(
        ast.chunk.body.stmts[1].kind,
        StmtKind::Global { .. }
    ));
}

#[test]
fn precedence_and_associativity() {
    let cases = [
        ("return 1 + 2 * 3", "return (1 + (2 * 3));"),
        ("return -2 ^ 2", "return (- (2 ^ 2));"),
        ("return 2 ^ 3 ^ 2", "return (2 ^ (3 ^ 2));"),
        ("return 2 ^ -3", "return (2 ^ (- 3));"),
        ("return 1 .. 2 .. 3", "return (1 .. (2 .. 3));"),
        ("return 1 - 2 - 3", "return ((1 - 2) - 3);"),
        ("return a or b and c", "return (a or (b and c));"),
        ("return not a == b", "return ((not a) == b);"),
        ("return a < b == c", "return ((a < b) == c);"),
        ("return a .. b + c", "return (a .. (b + c));"),
        ("return #t + 1", "return ((# t) + 1);"),
        (
            "return a | b ~ c & d << 1",
            "return (a | (b ~ (c & (d << 1))));",
        ),
        ("return (f())", "return (f());"),
        ("return a.b.c:d(1)[2]", "return a[\"b\"][\"c\"]:d(1)[2];"),
        ("return f{1} .. f'x'", "return (f({1}) .. f(\"x\"));"),
    ];
    for (src, expected) in cases {
        let ast = parse("s.lua", src.as_bytes()).unwrap();
        let printed = String::from_utf8(unparse(&ast)).unwrap();
        assert_eq!(printed.trim(), expected, "source: {src}");
    }
}

#[test]
fn spans_cover_the_source() {
    let src = "local x = 1 +\n  foo(2)";
    let ast = parse("s.lua", src.as_bytes()).unwrap();
    let StmtKind::Local { values, .. } = &ast.chunk.body.stmts[0].kind else {
        panic!()
    };
    let e = &values[0];
    assert_eq!(
        &src[e.span.start as usize..e.span.end as usize],
        "1 +\n  foo(2)"
    );
    let ExprKind::Binary {
        op: BinOp::Add,
        rhs,
        ..
    } = &e.kind
    else {
        panic!()
    };
    assert_eq!(
        &src[rhs.span.start as usize..rhs.span.end as usize],
        "foo(2)"
    );
    assert_eq!(ast.source.line_of(rhs.span.start), 2);
    assert_eq!(ast.chunk.body.stmts[0].span.end as usize, src.len());
}

#[test]
fn nesting_is_limited() {
    let deep = format!("x = {}1{}", "(".repeat(300), ")".repeat(300));
    assert_eq!(err(&deep), "s.lua:1: chunk has too many syntax levels");
    let ok = format!("x = {}1{}", "(".repeat(150), ")".repeat(150));
    parse("s.lua", ok.as_bytes()).unwrap();
    let options = ParseOptions {
        max_depth: 10,
        ..ParseOptions::default()
    };
    assert!(
        parse_with(
            "s.lua",
            b"do do do do do do do do do do do end end end end end end end end end end end",
            &options
        )
        .is_err()
    );
}

#[test]
fn skips_a_first_comment_line() {
    let options = ParseOptions {
        skip_comment_line: true,
        ..ParseOptions::default()
    };
    let ast = parse_with("s.lua", b"#!/usr/bin/lua\nx = = 1", &options).unwrap_err();
    assert_eq!(
        ast.to_lua_message("s.lua"),
        "s.lua:2: unexpected symbol near '='"
    );
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn lua_files(dir: &Path) -> Vec<PathBuf> {
    let mut files: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "lua"))
        .collect();
    files.sort();
    files
}

/// Parses `path`, then checks that printing and re-parsing gives the same text.
fn parse_and_round_trip(path: &Path) -> Result<(), String> {
    let src = std::fs::read(path).unwrap();
    let options = ParseOptions {
        skip_comment_line: true,
        max_depth: 1000,
    };
    let ast = parse_with("f.lua", &src, &options).map_err(|d| d.to_lua_message("f.lua"))?;
    let printed = unparse(&ast);
    let again = parse_with("f.lua", &printed, &options).map_err(|d| d.to_lua_message("printed"))?;
    if unparse(&again) != printed {
        return Err("printing is not a fixed point".into());
    }
    Ok(())
}

#[test]
fn first_light_programs_parse() {
    let root = repo_root();
    let mut files = lua_files(&root.join("tests/first-light"));
    files.extend(lua_files(&root.join("bench")));
    assert!(files.len() >= 8);
    for path in files {
        parse_and_round_trip(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    }
}

/// The whole official suite parses.
#[test]
fn official_suite_parses() {
    let root = repo_root();
    let suite = std::env::var_os("LUA_SUITE")
        .map(PathBuf::from)
        .unwrap_or_else(|| root.join("tests/lua-5.5/suite"));
    let mut parsed = 0;
    for path in lua_files(&suite) {
        let name = path.file_name().unwrap().to_str().unwrap().to_string();
        if let Err(e) = parse_and_round_trip(&path) {
            panic!("{name}: {e}");
        }
        parsed += 1;
    }
    assert!(parsed >= 30, "only {parsed} suite files parsed");
}
