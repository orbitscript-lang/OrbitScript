//! The formatter on hand-written cases and on every OrbitScript file in the repository.

use std::path::{Path, PathBuf};

use orbit_fmt::{Options, format, format_range};
use orbit_hir::Span;

fn fmt(src: &str) -> String {
    match format(src.as_bytes(), Options::default()) {
        Ok(out) => out,
        Err(e) => panic!("{e}\n{src}"),
    }
}

/// Checks that `src` formats as `want`, and that `want` is left as it is.
#[track_caller]
fn case(src: &str, want: &str) {
    assert_eq!(fmt(src), want);
    assert_eq!(fmt(want), want, "a second pass changed the output");
}

#[test]
fn spacing_and_statements() {
    case(
        "let   a=1+2*3;let b=#{a:1,b:2};",
        "let a = 1 + 2 * 3;\nlet b = #{ a: 1, b: 2 };\n",
    );
    case(
        "let t=[ 1,2 ];let e=#{ };",
        "let t = [1, 2];\nlet e = #{};\n",
    );
    case("x+=1;(a,b)=f( );", "x += 1;\n(a, b) = f();\n");
    case(
        "let n = -x;let m = !ok;let l = #t;",
        "let n = -x;\nlet m = !ok;\nlet l = #t;\n",
    );
    case("for i in 1..=n {}", "for i in 1..=n {}\n");
}

#[test]
fn bodies_always_split() {
    case(
        "fn   area(s)->float{ match s { Shape::Circle{r}=>PI*r*r, Shape::Rect { w, h } => w*h, } }",
        "fn area(s) -> float {\n    match s {\n        Shape::Circle { r } => PI * r * r,\n        Shape::Rect { w, h } => w * h,\n    }\n}\n",
    );
    case(
        "struct P { x: int, y: int }",
        "struct P {\n    x: int,\n    y: int,\n}\n",
    );
    case("if a { b(); }", "if a {\n    b();\n}\n");
    case("fn f() {}", "fn f() {}\n");
}

#[test]
fn short_forms_stay() {
    case(
        "let n = if a { 1 } else { 2 };",
        "let n = if a { 1 } else { 2 };\n",
    );
    case(
        "let n = if a_long_condition { first_value } else { second_value };",
        "let n = if a_long_condition {\n    first_value\n} else {\n    second_value\n};\n",
    );
    case(
        "table::sort(t, |a, b| a < b);",
        "table::sort(t, |a, b| a < b);\n",
    );
}

#[test]
fn long_lines_break() {
    let args = "a_fairly_long_argument_name";
    case(
        &format!("call({args}, {args}, {args}, {args});"),
        &format!("call(\n    {args},\n    {args},\n    {args},\n    {args},\n);\n"),
    );
    // The last argument breaks from the call's line when it's a table or a closure.
    case(
        "fn f() { if x { rpc::notify(\"textDocument/publishDiagnostics\", #{ uri: doc.uri, diagnostics: json::array() }); } }",
        "fn f() {\n    if x {\n        rpc::notify(\"textDocument/publishDiagnostics\", #{\n            uri: doc.uri,\n            diagnostics: json::array(),\n        });\n    }\n}\n",
    );
    case(
        "let total = first_operand_name + second_operand_name + third_operand_name + fourth_operand_name + fifth;",
        "let total = first_operand_name\n    + second_operand_name\n    + third_operand_name\n    + fourth_operand_name\n    + fifth;\n",
    );
    case(
        "let r = builder.with_name(name).with_size(size).with_colour(colour).with_border(border).with_title(title).build();",
        "let r = builder\n    .with_name(name)\n    .with_size(size)\n    .with_colour(colour)\n    .with_border(border)\n    .with_title(title)\n    .build();\n",
    );
}

#[test]
fn a_match_or_an_only_call_breaks_from_the_call_line() {
    let args = "a_fairly_long_argument_name";
    case(
        &format!("say(string::format(\"%s %s %s\", {args}, {args}, {args}));"),
        &format!(
            "say(string::format(\n    \"%s %s %s\",\n    {args},\n    {args},\n    {args},\n));\n"
        ),
    );
    case(
        "print(map.name.byte(1) == b'o', b'\\n', match map.name.byte(2) { b'a'..=b'z' => \"lower\", _ => \"other\" });",
        "print(map.name.byte(1) == b'o', b'\\n', match map.name.byte(2) {\n    b'a'..=b'z' => \"lower\",\n    _ => \"other\",\n});\n",
    );
}

#[test]
fn strings_over_several_lines_stay_in_their_call() {
    case(
        "let g = Grid::parse(r#\"\n.#.\n..#\n\"#);",
        "let g = Grid::parse(r#\"\n.#.\n..#\n\"#);\n",
    );
    case("f(a,   \"one\ntwo\");", "f(a, \"one\ntwo\");\n");
}

#[test]
fn comments_stay_where_they_are() {
    case(
        "// lead\nlet a = 1; // trail\n\n\n/* block */\nlet b = 2;\n// end\n",
        "// lead\nlet a = 1; // trail\n\n/* block */\nlet b = 2;\n// end\n",
    );
    case(
        "fn f() { // why\n    g();\n}",
        "fn f() { // why\n    g();\n}\n",
    );
    case(
        "let s = #{ a: 1, // first\nb: 2 };",
        "let s = #{\n    a: 1, // first\n    b: 2,\n};\n",
    );
    case("/// Docs.\npub fn f() {}", "/// Docs.\npub fn f() {}\n");
}

#[test]
fn blank_lines() {
    case(
        "fn f() {\n\n    a();\n\n\n    b();\n\n}",
        "fn f() {\n    a();\n\n    b();\n}\n",
    );
}

#[test]
fn use_lines_sort() {
    case(
        "//! Module docs.\n\nuse zeta; // z\nuse alpha::{c, b as x, a};\n\nuse beta;\n",
        "//! Module docs.\n\nuse alpha::{a, b as x, c};\nuse zeta; // z\n\nuse beta;\n",
    );
}

#[test]
fn keeps_what_it_must() {
    // A block's last `loop` keeps its `;`, or it would become the block's value.
    case(
        "fn f() -> int { loop { break; }; }",
        "fn f() -> int {\n    loop {\n        break;\n    };\n}\n",
    );
    // Literals and parentheses as written.
    case(
        "let x = (0x1F) + 1e3 + \"a\\tb\" + b'a';",
        "let x = (0x1F) + 1e3 + \"a\\tb\" + b'a';\n",
    );
    case(
        "#!/usr/bin/env orbit\nlet a=1;",
        "#!/usr/bin/env orbit\nlet a = 1;\n",
    );
}

#[test]
fn stubs() {
    let out = format(
        b"pub fn f(a:int)->int;\npub const N:int;",
        Options { stub: true },
    )
    .unwrap();
    assert_eq!(out, "pub fn f(a: int) -> int;\npub const N: int;\n");
    assert_eq!(
        fmt("let m: #{[string]:int} = #{};"),
        "let m: #{ [string]: int } = #{};\n"
    );
}

#[test]
fn refuses() {
    let e = format(b"let x = ;", Options::default()).unwrap_err();
    assert!(!e.internal, "{e}");
    // Comments are kept only between statements, list entries, arms and fields for now.
    let e = format(b"let x = 1 + /* why */ 2;", Options::default()).unwrap_err();
    assert!(e.internal, "{e}");
}

#[test]
fn range() {
    let src = "let a=1;\nfn f() { g(); }\nlet  b=2;\n";
    let at = src.find("g()").unwrap() as u32;
    let edit = format_range(src.as_bytes(), Span::new(at, at), Options::default())
        .unwrap()
        .expect("an edit");
    let mut out = src.to_owned();
    out.replace_range(edit.span.start as usize..edit.span.end as usize, &edit.text);
    assert_eq!(out, "let a=1;\nfn f() {\n    g();\n}\nlet  b=2;\n");
}

fn orbit_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let path = e.path();
        let name = e.file_name();
        let name = name.to_string_lossy();
        if path.is_dir() {
            if !name.starts_with('.') && name != "target" && name != "node_modules" {
                orbit_files(&path, out);
            }
        } else if name.ends_with(".orb") || name.ends_with(".orbi") {
            out.push(path);
        }
    }
}

/// Every `.orb` and `.orbi` file in the repository formats, and formatting again changes
/// nothing.
#[test]
fn repository_files() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut files = Vec::new();
    orbit_files(&root, &mut files);
    assert!(files.len() > 40, "found only {} files", files.len());
    let mut failures = Vec::new();
    for file in &files {
        let src = std::fs::read(file).unwrap();
        let options = Options {
            stub: file.extension().is_some_and(|x| x == "orbi"),
        };
        match format(&src, options) {
            Ok(once) => match format(once.as_bytes(), options) {
                Ok(twice) if twice == once => {}
                Ok(_) => failures.push(format!("{}: a second pass changed it", file.display())),
                Err(e) => failures.push(format!("{}: the second pass failed: {e}", file.display())),
            },
            Err(e) => failures.push(format!("{}:{e}", file.display())),
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
