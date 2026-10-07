//! Lowering tests: name resolution, the subset gate, and the HIR the first-light programs give.

use std::path::Path;

use orbit_hir::{ExprKind, FunctionName, LocalKind, StmtKind};
use orbit_lua_syntax::{ParseOptions, lower, parse, to_hir};

fn hir(src: &str) -> orbit_hir::Chunk {
    to_hir("s.lua", src.as_bytes(), &ParseOptions::default())
        .unwrap_or_else(|d| panic!("{src}: {}", d.to_lua_message("s.lua")))
}

fn err(src: &str) -> String {
    match to_hir("s.lua", src.as_bytes(), &ParseOptions::default()) {
        Ok(_) => panic!("expected an error for {src:?}"),
        Err(d) => d.to_lua_message("s.lua"),
    }
}

fn dump(src: &str) -> String {
    orbit_hir::print(&hir(src))
}

#[test]
fn const_assignment_matches_the_reference() {
    let cases = [
        (
            "local x <const> = 1; x = 2",
            "s.lua:1: attempt to assign to const variable 'x'",
        ),
        (
            "for i=1,2 do i = 3 end",
            "s.lua:1: attempt to assign to const variable 'i'",
        ),
        (
            "for k, v in pairs({}) do k = 1 end",
            "s.lua:1: attempt to assign to const variable 'k'",
        ),
        (
            "local x <const> = 1\nx\n=\n2\n",
            "s.lua:2: attempt to assign to const variable 'x'",
        ),
        (
            "local x <const> = 1\na,\nx\n=\n1,2",
            "s.lua:3: attempt to assign to const variable 'x'",
        ),
        (
            "local x <const>\n= 1\nx = 2",
            "s.lua:3: attempt to assign to const variable 'x'",
        ),
        (
            "local x <const>, y = 1, 2 y = 3 x = 4",
            "s.lua:1: attempt to assign to const variable 'x'",
        ),
        (
            "local <const> a, b = 1, 2 b = 3",
            "s.lua:1: attempt to assign to const variable 'b'",
        ),
        (
            "local f <const> = 1 function f() end",
            "s.lua:1: attempt to assign to const variable 'f'",
        ),
        (
            "local x <const> = 1 local function f() local function g() x = 2 end end",
            "s.lua:1: attempt to assign to const variable 'x'",
        ),
        (
            "for i = 1, 2 do local function f() i = 3 end end",
            "s.lua:1: attempt to assign to const variable 'i'",
        ),
    ];
    for (src, expected) in cases {
        assert_eq!(err(src), expected, "source: {src:?}");
    }
    // Only the first generic-for variable is read-only; shadowing lifts the restriction.
    hir("for k, v in pairs({}) do v = 1 end");
    hir("local x <const> = 1 do local x = 2 x = 3 end");
    hir("local t <const> = {} t.x = 1");
}

#[test]
fn ordinary_names_stay_ordinary() {
    // `global` stays an ordinary name, and `_ENV` as a field is just a string key.
    hir("global = 1 print(global) t = {} t._ENV = 1");
}

#[test]
fn to_be_closed_variables_match_the_reference() {
    let errors = [
        (
            "local a <close>, b <close> = nil, nil",
            "s.lua:1: multiple to-be-closed variables in local list",
        ),
        (
            "local <close> a, b = nil, 1",
            "s.lua:1: multiple to-be-closed variables in local list",
        ),
        // Reported at the end of the second declaration, before the values are read.
        (
            "local a <close>, b\n <close\n >\n\n = nil\n",
            "s.lua:3: multiple to-be-closed variables in local list",
        ),
        (
            "local <close>\n a,\n b\n\n = nil\n",
            "s.lua:3: multiple to-be-closed variables in local list",
        ),
        (
            "local a <close>,\n b,\n c <close>\n = nil x = = 1\n",
            "s.lua:3: multiple to-be-closed variables in local list",
        ),
        (
            "local a <close>, b <close> = undeclared",
            "s.lua:1: multiple to-be-closed variables in local list",
        ),
        (
            "local a <close> = nil; a = 1",
            "s.lua:1: attempt to assign to const variable 'a'",
        ),
        (
            "local x <close> = nil; return function() x = 1 end",
            "s.lua:1: attempt to assign to const variable 'x'",
        ),
        ("local a <foo> = 1", "s.lua:1: unknown attribute 'foo'"),
        (
            "global x <close>",
            "s.lua:1: global variables cannot be to-be-closed",
        ),
    ];
    for (src, expected) in errors {
        assert_eq!(err(src), expected, "source: {src:?}");
    }
    // A per-name attribute overrides the prefix one, and one closing variable per list is fine.
    for src in [
        "local <close> a <const> = nil",
        "local a <const>, b <close> = 1, nil",
        "local a, b <close> = 1, nil",
        "local x <close> = nil; local y <close> = nil",
        "goto l; local x <close> = nil; ::l::",
    ] {
        hir(src);
    }
    let chunk = hir("local a, b <close>, c <const> = 1, nil, 2");
    let kinds: Vec<_> = chunk.functions[0].locals.iter().map(|l| l.kind).collect();
    assert_eq!(
        kinds,
        vec![LocalKind::Normal, LocalKind::Close, LocalKind::Const]
    );
    let out = dump("local x <close> = nil");
    assert!(out.contains("close"), "{out}");
}

#[test]
fn scoping_rules() {
    // `local x = x` reads the outer `x`.
    let out = dump("local x = 1 do local x = x end");
    assert!(out.contains("let x#l1 = x#l0"), "{out}");

    // `local function` sees itself, `local f = function` doesn't.
    let out = dump("local function f() return f end local g = function() return g end");
    assert!(out.contains("return c0"), "{out}");
    assert!(out.contains("return global \"g\""), "{out}");

    // `repeat ... until` sees the body's locals.
    let out = dump("repeat local y = 1 until y");
    assert!(out.contains("until y#l0"), "{out}");

    // Locals declared after a use don't capture it.
    let out = dump("print(z) local z = 1");
    assert!(out.contains("call global \"print\"(global \"z\")"), "{out}");
}

#[test]
fn functions_and_methods() {
    let chunk = hir("function a.b:c(x) return self, x end function g() end local function h() end");
    let names: Vec<_> = chunk.functions.iter().map(|f| f.name).collect();
    let s = |b: &[u8]| {
        (0..chunk.strings.len())
            .map(|i| orbit_hir::StrId(i as u32))
            .find(|&id| chunk.strings.get(id) == b)
            .unwrap()
    };
    assert_eq!(
        names,
        vec![
            FunctionName::Main,
            FunctionName::Method(s(b"c")),
            FunctionName::Global(s(b"g")),
            FunctionName::Local(s(b"h")),
        ]
    );
    let method = &chunk.functions[1];
    assert_eq!(method.params.len(), 2);
    assert_eq!(method.local(method.params[0]).name, Some(s(b"self")));
    assert!(chunk.functions[0].is_vararg);
    assert!(!method.is_vararg);
}

#[test]
fn parenthesised_calls_give_one_value() {
    let chunk = hir("local function f() end return (f()), (...), (1)");
    let main = &chunk.functions[0];
    let ret = main.block(main.body).stmts.last().copied().unwrap();
    let StmtKind::Return(list) = &main.stmt(ret).kind else {
        panic!()
    };
    let kinds: Vec<_> = list
        .items
        .iter()
        .map(|&e| main.expr(e).kind.clone())
        .collect();
    assert!(matches!(kinds[0], ExprKind::First(_)));
    assert!(matches!(kinds[1], ExprKind::First(_)));
    assert_eq!(kinds[2], ExprKind::Int(1));
}

#[test]
fn for_loops() {
    let chunk = hir("for i = 1, 10, 2 do end for k, v in next, {} do end");
    let main = &chunk.functions[0];
    let kinds: Vec<_> = main.locals.iter().map(|l| l.kind).collect();
    use LocalKind::*;
    assert_eq!(
        kinds,
        vec![
            ForControl, Hidden, Hidden, Hidden, Hidden, ForControl, Normal
        ]
    );
}

#[test]
fn lowered_hir_verifies_before_and_after_analysis() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut files = Vec::new();
    for dir in ["tests/first-light", "bench"] {
        for entry in std::fs::read_dir(root.join(dir)).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().is_some_and(|e| e == "lua") {
                files.push(path);
            }
        }
    }
    assert!(files.len() >= 8);
    for path in files {
        let src = std::fs::read(&path).unwrap();
        let name = path.display().to_string();
        let ast = parse(&name, &src).unwrap_or_else(|d| panic!("{}", d.to_lua_message(&name)));
        let mut chunk = lower(ast).unwrap_or_else(|d| panic!("{}", d.to_lua_message(&name)));
        orbit_hir::verify_lowered(&chunk).unwrap_or_else(|e| panic!("{name}: {e}"));
        orbit_hir::analyze(&mut chunk).unwrap_or_else(|e| panic!("{name}: {e}"));
        orbit_hir::verify(&chunk).unwrap_or_else(|e| panic!("{name}: {e}"));
    }
}

/// `global` declarations and `_ENV`. Each message was checked against the reference.
#[test]
fn global_declarations_match_the_reference() {
    let cases = [
        ("global x; y = 1", "s.lua:1: variable 'y' not declared"),
        (
            "global y; global x = x",
            "s.lua:1: variable 'x' not declared",
        ),
        (
            "global x; local function f() global y; z = 1 end",
            "s.lua:1: variable 'z' not declared",
        ),
        ("global _ENV; x = 1", "s.lua:1: variable 'x' not declared"),
        (
            "global _ENV, x; local function f() return x end",
            "s.lua:1: _ENV is global when accessing variable 'x'",
        ),
        (
            "global _ENV, x; _ENV = 1",
            "s.lua:1: _ENV is global when accessing variable '_ENV'",
        ),
        (
            "local _ENV = {}; global _ENV; global x; x = 1",
            "s.lua:1: _ENV is global when accessing variable 'x'",
        ),
        (
            "global <close> x",
            "s.lua:1: global variables cannot be to-be-closed",
        ),
        (
            "global x <close>",
            "s.lua:1: global variables cannot be to-be-closed",
        ),
        (
            "global <const> z = 1; z = 2",
            "s.lua:1: attempt to assign to const variable 'z'",
        ),
        (
            "global<const> *; x = 1",
            "s.lua:1: attempt to assign to const variable 'x'",
        ),
        (
            "global x <const>; global *; x = 1",
            "s.lua:1: attempt to assign to const variable 'x'",
        ),
    ];
    for (src, expected) in cases {
        assert_eq!(err(src), expected, "source: {src:?}");
    }
    // `global *` covers every other name, `_ENV` is never undeclared, and locals shadow.
    hir("global *; global x; y = 1");
    hir("global x; local y = _ENV");
    hir("global x; local y = 1; y = 2");
    hir("global <const> x; global x = 3");
    hir("global x <const>; global function x() end");
    hir("global x; do local x = 1; x = 3 end");
}

#[test]
fn env_lowers_to_the_environment_or_a_local() {
    let text = dump("_ENV = {}; local e = _ENV; x = 1");
    assert!(text.contains("env = "), "{text}");
    assert!(text.contains("global \"x\" = 1"), "{text}");
    let text = dump("local _ENV = {}; x = y");
    assert!(text.contains("_ENV#l0[\"x\"] = _ENV#l0[\"y\"]"), "{text}");
    let text = dump("global x = 1; global function f() end");
    assert!(text.contains("define global \"x\" = 1"), "{text}");
    assert!(text.contains("define global \"f\" = closure fn1"), "{text}");
}

#[test]
fn named_varargs_keep_their_table_only_when_needed() {
    assert_eq!(
        err("local function f(...t) t = 1 end"),
        "s.lua:1: attempt to assign to const variable 't'"
    );
    // Only read by indexing: the table is elided.
    let text = dump("local function f(...t) return t.n, t[1] end");
    assert!(text.contains("(...t#l0 elided)"), "{text}");
    assert!(text.contains("...[\"n\"], ...[1]"), "{text}");
    // Written, measured, passed on or captured: the table is built.
    for src in [
        "local function f(...t) t[1] = 2 end",
        "local function f(...t) return #t end",
        "local function f(...t) return g(t) end",
        "local function f(...t) return function() return t[1] end end",
    ] {
        let text = dump(src);
        assert!(text.contains("(...t#l0)"), "{src}: {text}");
    }
}

/// `goto` and labels. Each message was checked against the reference.
#[test]
fn goto_rules_match_the_reference() {
    let cases = [
        (
            "goto f\nlocal x\n::f::\nprint(x)",
            "s.lua:4: <goto f> at line 1 jumps into the scope of 'x'",
        ),
        // Reported at the line of the block's last token (`end` for a function).
        (
            "do goto f\nlocal x\n::f::\nprint(x)\n\nend",
            "s.lua:4: <goto f> at line 1 jumps into the scope of 'x'",
        ),
        (
            "local function g() goto f\nlocal x\n::f::\nprint(x)\n\nend",
            "s.lua:6: <goto f> at line 1 jumps into the scope of 'x'",
        ),
        (
            "repeat goto f\nlocal x\n::f::\nuntil\nx",
            "s.lua:5: <goto f> at line 1 jumps into the scope of 'x'",
        ),
        (
            "goto f\nlocal x\n::f::\nreturn",
            "s.lua:4: <goto f> at line 1 jumps into the scope of 'x'",
        ),
        (
            "goto f\nglobal *\n::f::\nx = 1",
            "s.lua:4: <goto f> at line 1 jumps into the scope of '*'",
        ),
        (
            "goto f; local a; goto g; local b; ::g:: ::f:: x = 1",
            "s.lua:1: <goto f> at line 1 jumps into the scope of 'a'",
        ),
        (
            "goto x\n\ny = 1\n-- c\n",
            "s.lua:3: no visible label 'x' for <goto> at line 1",
        ),
        (
            "local function f()\n goto x\n end\ny = 2",
            "s.lua:3: no visible label 'x' for <goto> at line 2",
        ),
        (
            "goto f; do ::f:: end",
            "s.lua:1: no visible label 'f' for <goto> at line 1",
        ),
        (
            "::a:: do ::a:: end",
            "s.lua:1: label 'a' already defined on line 1",
        ),
        (
            "::a::\nx=1\n::a::",
            "s.lua:3: label 'a' already defined on line 1",
        ),
        // Lua takes a run of labels last to first.
        (
            "::a::\n::b::\n::a::",
            "s.lua:3: label 'a' already defined on line 3",
        ),
    ];
    for (src, expected) in cases {
        assert_eq!(err(src), expected, "source: {src:?}");
    }
    // A label followed only by labels is outside the block's scopes.
    hir("goto f; local x; ::f:: ::g::");
    hir("do goto f; local x; ::f:: end");
    hir("do ::a:: end ::a::");
    hir("::top:: local x = 1 goto top");
    let text = dump("do goto f end ::f::");
    assert!(text.contains("goto \"f\"#L0"), "{text}");
    assert!(text.contains("::\"f\"#L0::"), "{text}");
}

#[test]
fn captures_are_numbered_by_first_use() {
    // `_ENV` stays capture 0; `env_debug_index` gives its first-use position, and an
    // explicit `_ENV` counts as a use like a free name.
    let chunk = hir("local a, b = 1, 2 return function() local _ = b; _ENV.x = a end");
    let f = &chunk.functions[1];
    let names: Vec<_> = f
        .captures
        .iter()
        .map(|c| String::from_utf8_lossy(chunk.strings.get(c.name)).into_owned())
        .collect();
    assert_eq!(names, ["_ENV", "b", "a"]);
    assert_eq!(f.env_debug_index, Some(1));
}
