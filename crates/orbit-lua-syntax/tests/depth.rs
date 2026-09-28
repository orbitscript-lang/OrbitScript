//! Deep nesting: the parser must stop with the reference's error before it exhausts the
//! native stack, including on a test thread's small stack in debug builds.

use orbit_lua_syntax::{ErrorKind, ParseOptions, parse, parse_with};

fn nest(init: &str, rep: &str, close: &str, repc: &str, n: usize) -> Vec<u8> {
    format!("{init}{}{close}{}", rep.repeat(n), repc.repeat(n)).into_bytes()
}

/// The shapes `errors.lua` uses to test syntax limits: 100 levels load, 500 do not.
const CASES: &[(&str, &str, &str, &str)] = &[
    ("local a", ",a", ";", ""),
    ("local a", ",a", "= 1", ",1"),
    ("local a", ",a", "= f()", ""),
    ("local a; a", ",a", "= 1", ",1"),
    ("local a; a=", "{", "0", "}"),
    ("return ", "(", "2", ")"),
    (
        "local function a (x) return x end; return ",
        "a(",
        "2.2",
        ")",
    ),
    ("", "do ", "", " end"),
    ("", "while a do ", "", " end"),
    ("local a; ", "if a then else ", "", " end"),
    ("", "function foo () ", "", " end"),
    ("local a = ''; return ", "a..", "'a'", ""),
    ("local a = 1; return ", "a^", "a", ""),
];

#[test]
fn syntax_limits() {
    for &(init, rep, close, repc) in CASES {
        assert!(
            parse(&nest(init, rep, close, repc, 100)).is_ok(),
            "{init}{rep}"
        );
        let err = parse(&nest(init, rep, close, repc, 500)).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("too many") || msg.contains("overflow"),
            "{init}{rep}: {msg}"
        );
    }
}

/// Where nesting starts to fail, as measured with `lua5.5` calling `load` from a script's
/// main chunk, which has two of the 200 levels in use already.
const BOUNDARIES: &[(&str, &str, &str, &str, usize)] = &[
    ("local a; a", ",a", "= 1", ",1", 196),
    ("local a; a=", "{", "0", "}", 196),
    ("return ", "(", "2", ")", 196),
    (
        "local function a (x) return x end; return ",
        "a(",
        "2.2",
        ")",
        196,
    ),
    ("", "do ", "", " end", 198),
    ("", "while a do ", "", " end", 197),
    ("local a; ", "if a then else ", "", " end", 197),
    ("", "function foo () ", "", " end", 198),
    ("local a = ''; return ", "a..", "'a'", "", 196),
    ("local a = 1; return ", "a^", "a", "", 196),
    ("x = ", "- ", "1", "", 196),
    ("x = ", "{", "", "}", 197),
];

#[test]
fn nesting_boundaries_match_reference() {
    let options = ParseOptions {
        max_depth: 198,
        ..ParseOptions::default()
    };
    for &(init, rep, close, repc, fail_at) in BOUNDARIES {
        let ok = parse_with(&nest(init, rep, close, repc, fail_at - 1), &options);
        assert!(ok.is_ok(), "{init}{rep} x{}: {:?}", fail_at - 1, ok.err());
        let err = parse_with(&nest(init, rep, close, repc, fail_at), &options).unwrap_err();
        assert_eq!(err.kind, ErrorKind::Overflow, "{init}{rep} x{fail_at}");
        assert_eq!(err.to_lua_message(b"=t"), b"C stack overflow");
    }
}
