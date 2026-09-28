use super::*;

fn caps(s: &[u8], c: &Captures) -> Vec<String> {
    c.as_slice()
        .iter()
        .map(|c| match c {
            Capture::Str { .. } => String::from_utf8_lossy(c.bytes(s).unwrap()).into_owned(),
            Capture::Position(p) => p.to_string(),
        })
        .collect()
}

fn m(s: &str, p: &str) -> Option<Vec<String>> {
    match_(s.as_bytes(), p.as_bytes(), 1)
        .unwrap()
        .map(|c| caps(s.as_bytes(), &c))
}

fn m_err(s: &str, p: &str) -> String {
    match_(s.as_bytes(), p.as_bytes(), 1)
        .unwrap_err()
        .to_string()
}

fn gsub_str(s: &str, p: &str, r: &str, max: Option<i64>) -> (String, i64) {
    let (out, n) = gsub::<LibError>(
        s.as_bytes(),
        p.as_bytes(),
        Replacement::Str(r.as_bytes()),
        max,
    )
    .unwrap();
    (
        String::from_utf8(out.unwrap_or_else(|| s.as_bytes().to_vec())).unwrap(),
        n,
    )
}

#[test]
fn matching() {
    assert_eq!(
        m("hello world", "(h)(.-)o"),
        Some(vec!["h".into(), "ell".into()])
    );
    assert_eq!(m("hello", "()ll()"), Some(vec!["3".into(), "5".into()]));
    assert_eq!(m("  x = (a(b)c) ", "%b()"), Some(vec!["(a(b)c)".into()]));
    assert_eq!(m("THE (quick) fox", "%f[%a]%a+"), Some(vec!["THE".into()]));
    assert_eq!(m("abcabc", "(abc)%1"), Some(vec!["abc".into()]));
    assert_eq!(m("a]b", "[]]"), Some(vec!["]".into()]));
    assert_eq!(m("a-b", "[a%-]+"), Some(vec!["a-".into()]));
    assert_eq!(m("x", "^$"), None);
    assert_eq!(m("", "^$"), Some(vec!["".into()]));
    assert_eq!(m("a$b", "a$b"), Some(vec!["a$b".into()]));
    assert_eq!(m("aaab", "a-b"), Some(vec!["aaab".into()]));
    assert_eq!(m("word", "%f[%W]"), Some(vec!["".into()]));
}

#[test]
fn find_and_gmatch() {
    let f = find(b"hello", b"l+", 1, false).unwrap().unwrap();
    assert_eq!((f.start, f.end), (2, 4));
    let f = find(b"a.b", b".", 1, true).unwrap().unwrap();
    assert_eq!((f.start, f.end), (1, 2));
    assert!(find(b"abc", b"b", 10, false).unwrap().is_none());
    let f = find(b"abc", b"", 4, false).unwrap().unwrap();
    assert_eq!((f.start, f.end), (3, 3));
    let f = find(b"abc", b"c", -1, false).unwrap().unwrap();
    assert_eq!((f.start, f.end), (2, 3));

    let s = b"one two  three";
    let mut g = GMatch::new(s.len(), 1);
    let mut words = Vec::new();
    while let Some(c) = g.next(s, b"%a*").unwrap() {
        words.extend(caps(s, &c));
    }
    assert_eq!(words, ["one", "two", "", "three"]); // as lua5.5 prints
}

#[test]
fn substitution() {
    assert_eq!(
        gsub_str("hello world", "o", "0", None),
        ("hell0 w0rld".into(), 2)
    );
    assert_eq!(
        gsub_str("hello world", "(o)", "[%1%%]", Some(1)),
        ("hell[o%] world".into(), 1)
    );
    assert_eq!(gsub_str("abc", "", "-", None), ("-a-b-c-".into(), 4));
    assert_eq!(gsub_str("abc", "^", ">", None), (">abc".into(), 1));
    assert_eq!(gsub_str("abc", "()", "%1", None), ("1a2b3c4".into(), 4));
    assert_eq!(gsub_str("abc", "x*", "-", None), ("-a-b-c-".into(), 4));

    // A callback that keeps some matches: only "b" is replaced.
    let mut f = |m: &GsubMatch<'_, '_>, out: &mut Vec<u8>| -> Result<bool, LibError> {
        let c = m.first()?;
        if c.bytes(m.subject()) == Some(b"b") {
            out.extend_from_slice(b"B");
            Ok(true)
        } else {
            Ok(false)
        }
    };
    let (out, n) = gsub(b"abc", b"%a", Replacement::Call(&mut f), None).unwrap();
    assert_eq!((out.as_deref(), n), (Some(&b"aBc"[..]), 3));
    let (out, n) = gsub(b"xyz", b"%a", Replacement::Call(&mut f), None).unwrap();
    assert_eq!((out, n), (None, 3));
}

#[test]
fn errors() {
    assert_eq!(m_err("a", "%"), "malformed pattern (ends with '%')");
    assert_eq!(m_err("a", "[a"), "malformed pattern (missing ']')");
    assert_eq!(m_err("a", "(a"), "unfinished capture");
    assert_eq!(m_err("a", "a)"), "invalid pattern capture");
    assert_eq!(m_err("a", "%1"), "invalid capture index %1");
    assert_eq!(
        m_err("a", "%b"),
        "malformed pattern (missing arguments to '%b')"
    );
    assert_eq!(m_err("a", "%fa"), "missing '[' after '%f' in pattern");
    assert_eq!(m_err("a", &"(".repeat(33)), "too many captures");
    assert_eq!(
        m_err(&"a".repeat(300), &"a?".repeat(300)),
        "pattern too complex"
    );
    let e = gsub::<LibError>(b"a", b"a", Replacement::Str(b"%x"), None).unwrap_err();
    assert_eq!(e.to_string(), "invalid use of '%' in replacement string");
    let e = gsub::<LibError>(b"a", b"a", Replacement::Str(b"%2"), None).unwrap_err();
    assert_eq!(e.to_string(), "invalid capture index %2");
}
