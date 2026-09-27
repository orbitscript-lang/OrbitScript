//! Error messages, lines and "near" tokens, compared with the reference.
//!
//! Each expected value is what `load(src, "=t")` returns in `lua5.5` (5.5.1, default
//! build), or `None` when it loads. The cases cover every error the parser can raise.

use orbit_lua_syntax::parse;

#[rustfmt::skip]
const CASES: &[(&[u8], Option<&[u8]>)] = &[
    (b"x = 3x", Some(b"t:1: malformed number near '3x'")),
    (b"x = 0x", Some(b"t:1: malformed number near '0x'")),
    (b"x = 1e+", Some(b"t:1: malformed number near '1e+'")),
    (b"x = .5.5", Some(b"t:1: malformed number near '.5.5'")),
    (b"x = 08", None),
    (b"x = 'abc", Some(b"t:1: unfinished string near <eof>")),
    (b"x = 'abc\x0ay'", Some(b"t:1: unfinished string near ''abc'")),
    (b"x = '\\q'", Some(b"t:1: invalid escape sequence near ''\\q'")),
    (b"x = '\\256'", Some(b"t:1: decimal escape too large near ''\\256''")),
    (b"x = '\\xg1'", Some(b"t:1: hexadecimal digit expected near ''\\xg'")),
    (b"x = '\\u{110000000}'", Some(b"t:1: UTF-8 value too large near ''\\u{110000000'")),
    (b"x = '\\u{12'", Some(b"t:1: missing '}' near ''\\u{12''")),
    (b"x = '\\u12'", Some(b"t:1: missing '{' near ''\\u1'")),
    (b"x = [==[ abc", Some(b"t:1: unfinished long string (starting at line 1) near <eof>")),
    (b"--[[ abc\x0a\x0a", Some(b"t:3: unfinished long comment (starting at line 1) near <eof>")),
    (b"x = [=", Some(b"t:1: invalid long string delimiter near '[='")),
    (b"x = @", Some(b"t:1: unexpected symbol near '@'")),
    (b"x = \x01", Some(b"t:1: unexpected symbol near '<\\1>'")),
    (b"x = \xc8", Some(b"t:1: unexpected symbol near '<\\200>'")),
    (b"x = '\\z  \x0a  \x0a'", None),
    (b"x = \x00", Some(b"t:1: unexpected symbol")),
    (b"x = = 1", Some(b"t:1: unexpected symbol near '='")),
    (b"x = 1 +", Some(b"t:1: unexpected symbol near <eof>")),
    (b"local = 1", Some(b"t:1: <name> expected near '='")),
    (b"local function (x) end", Some(b"t:1: <name> expected near '('")),
    (b"for i do end", Some(b"t:1: '=' or 'in' expected near 'do'")),
    (b"for i = 1 do end", Some(b"t:1: ',' expected near 'do'")),
    (b"for a, b = 1, 2 do end", Some(b"t:1: 'in' expected near '='")),
    (b"if x then", Some(b"t:1: 'end' expected near <eof>")),
    (b"if x then else", Some(b"t:1: 'end' expected near <eof>")),
    (b"if x then\x0a\x0aelseif y\x0aend", Some(b"t:4: 'then' expected near 'end'")),
    (b"while x do\x0a  y()\x0a", Some(b"t:3: 'end' expected (to close 'while' at line 1) near <eof>")),
    (b"repeat x = 1", Some(b"t:1: 'until' expected near <eof>")),
    (b"function f(a, 1) end", Some(b"t:1: <name> or '...' expected near '1'")),
    (b"function f(..., a) end", Some(b"t:1: ')' expected near ','")),
    (b"function f(a,) end", Some(b"t:1: <name> or '...' expected near ')'")),
    (b"f(", Some(b"t:1: unexpected symbol near <eof>")),
    (b"f(1,\x0a2", Some(b"t:2: ')' expected (to close '(' at line 1) near <eof>")),
    (b"t = {1, 2", Some(b"t:1: '}' expected near <eof>")),
    (b"t = {\x0a1, 2\x0a", Some(b"t:3: '}' expected (to close '{' at line 1) near <eof>")),
    (b"t = {[1] 2}", Some(b"t:1: '=' expected near '2'")),
    (b"x.1 = 2", Some(b"t:1: syntax error near '.1'")),
    (b"x:y = 1", Some(b"t:1: function arguments expected near '='")),
    (b"x:y", Some(b"t:1: function arguments expected near <eof>")),
    (b"f() = 1", Some(b"t:1: syntax error near '='")),
    (b"a, f() = 1", Some(b"t:1: syntax error near '='")),
    (b"(a) = 1", Some(b"t:1: syntax error near '='")),
    (b"x", Some(b"t:1: syntax error near <eof>")),
    (b"x y", Some(b"t:1: syntax error near 'y'")),
    (b"a.b:c 1", Some(b"t:1: function arguments expected near '1'")),
    (b"return return", Some(b"t:1: unexpected symbol near 'return'")),
    (b"return 1 x = 2", Some(b"t:1: <eof> expected near 'x'")),
    (b"do return end x = 1", None),
    (b"local x <const = 1", Some(b"t:1: '>' expected near '='")),
    (b"local x <foo> = 1", Some(b"t:1: unknown attribute 'foo'")),
    (b"local <close> a, b", Some(b"t:1: multiple to-be-closed variables in local list")),
    (b"local a <close>, b <close>", Some(b"t:1: multiple to-be-closed variables in local list")),
    (b"goto", Some(b"t:1: <name> expected near <eof>")),
    (b"goto 1", Some(b"t:1: <name> expected near '1'")),
    (b"::a", Some(b"t:1: '::' expected near <eof>")),
    (b"::1::", Some(b"t:1: <name> expected near '1'")),
    (b"break", Some(b"t:1: break outside loop near 'break'")),
    (b"while true do local function f() break end end", Some(b"t:1: break outside loop near 'break'")),
    (b"x = ...", None),
    (b"function f() return ... end", Some(b"t:1: cannot use '...' outside a vararg function near '...'")),
    (b"local function f(...) return function() return ... end end", Some(b"t:1: cannot use '...' outside a vararg function near '...'")),
    (b"x = function", Some(b"t:1: '(' expected near <eof>")),
    (b"x = function(", Some(b"t:1: <name> or '...' expected near <eof>")),
    (b"a.b.c:d.e()", Some(b"t:1: function arguments expected near '.'")),
    (b"return (1", Some(b"t:1: ')' expected near <eof>")),
    (b"return (1\x0a\x0a", Some(b"t:3: ')' expected (to close '(' at line 1) near <eof>")),
    (b"x = 1 == == 2", Some(b"t:1: unexpected symbol near '=='")),
    (b"x = not", Some(b"t:1: unexpected symbol near <eof>")),
    (b"x = #", Some(b"t:1: unexpected symbol near <eof>")),
    (b"x = - -", Some(b"t:1: unexpected symbol near <eof>")),
    (b"x = {x = }", Some(b"t:1: unexpected symbol near '}'")),
    (b"x = {= 1}", Some(b"t:1: unexpected symbol near '='")),
    (b"x = a[1", Some(b"t:1: ']' expected near <eof>")),
    (b"x = a..b..", Some(b"t:1: unexpected symbol near <eof>")),
    (b"f{", Some(b"t:1: unexpected symbol near <eof>")),
    (b"f'x' 'y'", None),
    (b"f[[x]] 1", Some(b"t:1: unexpected symbol near '1'")),
    (b"local x <const> = 1; x = 2", Some(b"t:1: attempt to assign to const variable 'x'")),
    (b"local x <const> = {}; x = 2", Some(b"t:1: attempt to assign to const variable 'x'")),
    (b"local x <close> = nil; x = 2", Some(b"t:1: attempt to assign to const variable 'x'")),
    (b"for i = 1, 2 do i = 3 end", Some(b"t:1: attempt to assign to const variable 'i'")),
    (b"for k, v in pairs(t) do k = 1 end", Some(b"t:1: attempt to assign to const variable 'k'")),
    (b"for k, v in pairs(t) do v = 1 end", None),
    (b"local function f(...t) t = 1 end", Some(b"t:1: attempt to assign to const variable 't'")),
    (b"local function f(...t) return function() t = 1 end end", Some(b"t:1: attempt to assign to const variable 't'")),
    (b"local x <const> = 1; function x() end", Some(b"t:1: attempt to assign to const variable 'x'")),
    (b"local x <const> = {}; local function g() x = 1 end", Some(b"t:1: attempt to assign to const variable 'x'")),
    (b"local a <const> = 1; local b = 2; a, b = 3, 4", Some(b"t:1: attempt to assign to const variable 'a'")),
    (b"local a = 1; local b <const> = 2; a, b = 3, 4", Some(b"t:1: attempt to assign to const variable 'b'")),
    (b"global none; x = 1", Some(b"t:1: variable 'x' not declared")),
    (b"global none; print(1)", Some(b"t:1: variable 'print' not declared")),
    (b"global print; print(1); y = 1", Some(b"t:1: variable 'y' not declared")),
    (b"global <const> *; x = 1", Some(b"t:1: attempt to assign to const variable 'x'")),
    (b"global <const> *; local function f() x = 1 end", Some(b"t:1: attempt to assign to const variable 'x'")),
    (b"global <const> x; x = 1", Some(b"t:1: attempt to assign to const variable 'x'")),
    (b"global x <const>; x = 1", Some(b"t:1: attempt to assign to const variable 'x'")),
    (b"global <const> x, y <const>; y = 1", Some(b"t:1: attempt to assign to const variable 'y'")),
    (b"global *; global <const> y; x = 1; y = 2", Some(b"t:1: attempt to assign to const variable 'y'")),
    (b"global <const> *; global y; y = 1; x = 2", Some(b"t:1: attempt to assign to const variable 'x'")),
    (b"global x <close>", Some(b"t:1: global variables cannot be to-be-closed")),
    (b"global <close> *", Some(b"t:1: global variables cannot be to-be-closed")),
    (b"global <foo> x", Some(b"t:1: unknown attribute 'foo'")),
    (b"global _ENV; x = 1", Some(b"t:1: variable 'x' not declared")),
    (b"local _ENV <const> = 1; x = 2", None),
    (b"global function f() end; global none; f = 1", None),
    (b"global none; function f() end", Some(b"t:1: variable 'f' not declared")),
    (b"global none; global function f() end; f()", None),
    (b"do global none end; x = 1", None),
    (b"global x = 1, 2; x = 3", None),
    (b"global none; global a = b", Some(b"t:1: variable 'b' not declared")),
    (b"goto l", Some(b"t:1: no visible label 'l' for <goto> at line 1")),
    (b"do goto l end", Some(b"t:1: no visible label 'l' for <goto> at line 1")),
    (b"::l:: ::l::", Some(b"t:1: label 'l' already defined on line 1")),
    (b"::l:: do ::l:: end", Some(b"t:1: label 'l' already defined on line 1")),
    (b"do ::l:: end ::l::", None),
    (b"goto l; local x; ::l:: print(x)", Some(b"t:1: <goto l> at line 1 jumps into the scope of 'x'")),
    (b"goto l; local x; ::l::", None),
    (b"goto l; local x; ::l:: ;;", None),
    (b"do goto l; local x; ::l:: end", None),
    (b"repeat goto l; local x; ::l:: until x", Some(b"t:1: <goto l> at line 1 jumps into the scope of 'x'")),
    (b"goto l; global *; ::l:: print(3)", Some(b"t:1: <goto l> at line 1 jumps into the scope of '*'")),
    (b"goto l; global x; ::l:: x = 1", Some(b"t:1: <goto l> at line 1 jumps into the scope of 'x'")),
    (b"local function f() goto l end ::l::", Some(b"t:1: no visible label 'l' for <goto> at line 1")),
    (b"::l:: local function f() goto l end", Some(b"t:1: no visible label 'l' for <goto> at line 1")),
    (b"while x do goto continue; local y = 1; ::continue:: end", None),
    (b"for i = 1, 2 do\x0a  goto l\x0a  local z\x0a  ::l::\x0a  print(z)\x0aend", Some(b"t:5: <goto l> at line 2 jumps into the scope of 'z'")),
    (b"local x <const> = 1\x0alocal y <const> = x + 1\x0ay = 3", Some(b"t:3: attempt to assign to const variable 'y'")),
    (b"local x <const> = 2^53\x0ax = 1", Some(b"t:2: attempt to assign to const variable 'x'")),
    (b"local a <const> = 'x' .. 'y'; a = 1", Some(b"t:1: attempt to assign to const variable 'a'")),
];

#[test]
fn messages_match_reference() {
    let mut failures = Vec::new();
    for &(src, expected) in CASES {
        let got = parse(src).err().map(|e| e.to_lua_message(b"=t"));
        if got.as_deref() != expected {
            failures.push(format!(
                "{:?}\n   expected {:?}\n   got      {:?}",
                String::from_utf8_lossy(src),
                expected.map(String::from_utf8_lossy),
                got.as_deref().map(String::from_utf8_lossy),
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn global_as_keyword() {
    use orbit_lua_syntax::{ParseOptions, parse_with};
    let opts = ParseOptions {
        global_is_keyword: true,
        ..ParseOptions::default()
    };
    // With LUA_COMPAT_GLOBAL off, `global` is reserved.
    let err = parse_with(b"global = 1", &opts).unwrap_err();
    assert_eq!(err.to_lua_message(b"=t"), b"t:1: <name> expected near '='");
    assert!(parse_with(b"global x = 1", &opts).is_ok());
    // By default it is a name unless a declaration follows.
    assert!(parse(b"global = 1; global.x = 2; global(1)").is_ok());
    assert!(parse(b"local global = 1; global = global + 1").is_ok());
}

#[test]
fn chunk_names() {
    let err = parse(b"x = = 1").unwrap_err();
    assert_eq!(
        err.to_lua_message(b"@script.lua"),
        b"script.lua:1: unexpected symbol near '='"
    );
    assert_eq!(
        err.to_lua_message(b"x = = 1"),
        b"[string \"x = = 1\"]:1: unexpected symbol near '='"
    );
    assert_eq!(err.to_string(), "1: unexpected symbol near '='");
}
