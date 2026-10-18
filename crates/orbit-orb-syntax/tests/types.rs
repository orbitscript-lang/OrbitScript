//! The tool-only parse options: type trees, stubs, generic parameters and doc comments, and
//! the type syntax only `orbit-check` reads (unions, records, aliases, `as`).

use orbit_orb_syntax::ast::*;
use orbit_orb_syntax::{ParseOptions, Span, StringPool, parse_with, to_hir};

fn tools() -> ParseOptions {
    ParseOptions {
        type_trees: true,
        doc_comments: true,
        ..ParseOptions::default()
    }
}

fn ty(strings: &StringPool, t: &Type) -> String {
    let s = |n: &Name| strings.display(n.sym).into_owned();
    let list = |ts: &[Type]| {
        ts.iter()
            .map(|t| ty(strings, t))
            .collect::<Vec<_>>()
            .join(" ")
    };
    match t.kind.as_deref().expect("type tree") {
        TypeKind::Path { segments, args } => {
            let path = segments.iter().map(s).collect::<Vec<_>>().join("::");
            if args.is_empty() {
                path
            } else {
                format!("({path} {})", list(args))
            }
        }
        TypeKind::Nil => "nil".into(),
        TypeKind::SelfType => "Self".into(),
        TypeKind::Optional(t) => format!("(? {})", ty(strings, t)),
        TypeKind::Union(ts) => format!("(| {})", list(ts)),
        TypeKind::Array(t) => format!("[{}]", ty(strings, t)),
        TypeKind::Map { key, value } => {
            format!("(map {} {})", ty(strings, key), ty(strings, value))
        }
        TypeKind::Record(fields) => {
            let fs: Vec<_> = fields
                .iter()
                .map(|f| format!("{}:{}", s(&f.name), ty(strings, &f.ty)))
                .collect();
            format!("(record {})", fs.join(" "))
        }
        TypeKind::Tuple(ts) => format!("(tuple {})", list(ts)),
        TypeKind::Fn { params, rest, ret } => {
            let mut ps = list(params);
            if let Some(r) = rest {
                ps = format!("{ps} ...{}", ty(strings, r))
                    .trim_start()
                    .to_string();
            }
            match ret {
                Some(r) => format!("(fn ({ps}) {})", ty(strings, r)),
                None => format!("(fn ({ps}))"),
            }
        }
        TypeKind::Error => "error".into(),
    }
}

/// The type of `let x: T;` (or of the value of `type`/`as`, whichever the source has first).
fn parse_type(src: &str) -> String {
    let ast = parse_with("t.orb", src.as_bytes(), &tools()).unwrap_or_else(|d| {
        panic!("{src}: {}", d.to_message("t.orb"));
    });
    let stmt = &ast.body.stmts[0];
    let t = match &stmt.kind {
        StmtKind::Let { ty: Some(t), .. } => t,
        StmtKind::Let {
            value:
                Some(Expr {
                    kind: ExprKind::As { ty, .. },
                    ..
                }),
            ..
        } => ty,
        StmtKind::Item(item) => match &item.kind {
            ItemKind::TypeAlias { ty, .. } => ty,
            _ => panic!("{src}: no type"),
        },
        _ => panic!("{src}: no type"),
    };
    ty(&ast.strings, t)
}

#[test]
fn type_trees() {
    for (src, expected) in [
        ("let x: int;", "int"),
        ("let x: geo::Vec2;", "geo::Vec2"),
        ("let x: Stack<int>;", "(Stack int)"),
        ("let x: #{[string]: [int]};", "(map string [int])"),
        (
            "let x: Map<string, [Pair<int>]>;",
            "(Map string [(Pair int)])",
        ),
        ("let x: string?;", "(? string)"),
        ("let x: string | number | nil;", "(| string number nil)"),
        ("let x: int? | bool;", "(| (? int) bool)"),
        ("let x: (string | number)?;", "(? (| string number))"),
        ("let x: (File?, string?);", "(tuple (? File) (? string))"),
        ("let x: ();", "(tuple )"),
        ("let x: (int,);", "(tuple int)"),
        ("let x: #{};", "(record )"),
        (
            r#"let x: #{ name: string, size: int, "two words": bool, type: Self };"#,
            "(record name:string size:int two words:bool type:Self)",
        ),
        ("let x: fn(int, string) -> bool;", "(fn (int string) bool)"),
        ("let x: fn();", "(fn ())"),
        (
            "let x: fn(string, ...any) -> int;",
            "(fn (string ...any) int)",
        ),
        ("let x: fn(...int);", "(fn (...int))"),
        (
            "let x: (int | nil) | (bool | string);",
            "(| int nil bool string)",
        ),
        ("let x: fn() -> int | nil;", "(fn () (| int nil))"),
        ("type Pair<T> = (T, T);", "(tuple T T)"),
        ("type Id = int | string;", "(| int string)"),
        ("let x = y as int?;", "(? int)"),
    ] {
        assert_eq!(parse_type(src), expected, "{src}");
    }
}

#[test]
fn trees_are_off_by_default() {
    let ast = parse_with(
        "t.orb",
        b"let x: #{ a: int } | nil = y as int;",
        &ParseOptions::default(),
    )
    .unwrap();
    let StmtKind::Let {
        ty: Some(t),
        value: Some(v),
        ..
    } = &ast.body.stmts[0].kind
    else {
        panic!()
    };
    assert!(t.kind.is_none());
    let ExprKind::As { ty, .. } = &v.kind else {
        panic!()
    };
    assert!(ty.kind.is_none());
    assert!(ast.doc_comments.is_empty());
}

#[test]
fn generics_and_parameter_types_are_kept() {
    let ast = parse_with(
        "t.orb",
        b"struct Stack<T> { items: [T] }
          enum Tree<K, V> { Leaf, Node { k: K, v: V } }
          impl<T> Stack<T> { fn push(self: Stack<T>, ...xs: T) {} }
          fn first<T>(t: [T]) -> T? { t[1] }",
        &tools(),
    )
    .unwrap();
    let name = |n: &Name| ast.strings.display(n.sym).into_owned();
    let generic_names = |g: &[GenericParam]| g.iter().map(|g| name(&g.name)).collect::<Vec<_>>();
    let items: Vec<&Item> = ast
        .body
        .stmts
        .iter()
        .map(|s| match &s.kind {
            StmtKind::Item(i) => &**i,
            _ => panic!(),
        })
        .collect();
    let ItemKind::Struct(s) = &items[0].kind else {
        panic!()
    };
    assert_eq!(generic_names(&s.generics), ["T"]);
    let ItemKind::Enum(e) = &items[1].kind else {
        panic!()
    };
    assert_eq!(generic_names(&e.generics), ["K", "V"]);
    let ItemKind::Impl(i) = &items[2].kind else {
        panic!()
    };
    assert_eq!(generic_names(&i.generics), ["T"]);
    assert_eq!(ty(&ast.strings, &i.target_args[0]), "T");
    let ItemKind::Fn(push) = &i.members[0].kind else {
        panic!()
    };
    assert_eq!(
        ty(&ast.strings, push.sig.self_ty.as_ref().unwrap()),
        "(Stack T)"
    );
    assert_eq!(ty(&ast.strings, push.sig.rest_ty.as_ref().unwrap()), "T");
    let ItemKind::Fn(first) = &items[3].kind else {
        panic!()
    };
    assert_eq!(generic_names(&first.generics), ["T"]);
    assert_eq!(ty(&ast.strings, first.sig.ret.as_ref().unwrap()), "(? T)");
}

#[test]
fn doc_comments_attach_to_the_next_item() {
    let src = "//! The module.\n\
               /// Adds.\n/// Twice.\nfn add() {}\n\
               let x = 1; /// Not an item's.\n\
               //// Not a doc comment.\n\
               struct P {}\n\
               impl P {\n    /// A method.\n    pub fn m(self) {}\n}\n";
    let ast = parse_with("t.orb", src.as_bytes(), &tools()).unwrap();
    let text = |s: Span| src[s.start as usize..s.end as usize].to_string();
    let all: Vec<_> = ast
        .doc_comments
        .iter()
        .map(|d| (d.inner, text(d.span)))
        .collect();
    assert_eq!(
        all,
        [
            (true, "//! The module.".to_string()),
            (false, "/// Adds.".to_string()),
            (false, "/// Twice.".to_string()),
            (false, "/// Not an item's.".to_string()),
            (false, "/// A method.".to_string()),
        ]
    );
    let docs: Vec<Vec<String>> = ast
        .body
        .stmts
        .iter()
        .filter_map(|s| match &s.kind {
            StmtKind::Item(i) => Some(&**i),
            _ => None,
        })
        .flat_map(|i| {
            let mut v = vec![i.docs.iter().map(|&s| text(s)).collect::<Vec<_>>()];
            if let ItemKind::Impl(imp) = &i.kind {
                v.extend(
                    imp.members
                        .iter()
                        .map(|m| m.docs.iter().map(|&s| text(s)).collect()),
                );
            }
            v
        })
        .collect();
    assert_eq!(
        docs,
        [
            vec!["/// Adds.".to_string(), "/// Twice.".to_string()],
            vec!["/// Not an item's.".to_string()],
            vec![],
            vec!["/// A method.".to_string()],
        ]
    );
}

#[test]
fn stubs() {
    let src = b"pub fn open(path: string, mode: string?) -> (File?, string?);
                const MAX: int;
                pub struct File {}
                impl File { pub fn close(self) -> bool?; const KIND: string; }";
    let stub = ParseOptions {
        stub: true,
        ..tools()
    };
    let ast = parse_with("io.orbi", src, &stub).unwrap();
    assert_eq!(ast.body.stmts.len(), 4);
    let err = parse_with("io.orbi", src, &ParseOptions::default()).unwrap_err();
    assert_eq!(
        err.to_message("io.orbi"),
        "io.orbi:1: expected `{`, found `;`"
    );
    // A stub can't be run.
    let err = to_hir("io.orbi", b"fn f();", &stub).unwrap_err();
    assert_eq!(
        err.to_message("io.orbi"),
        "io.orbi:1: a stub declaration has no body to run"
    );
}

#[test]
fn type_only_syntax_runs_unchanged() {
    // `type` stays an ordinary name where it isn't an alias, as `r#type` and as a call.
    let src = b"type Num = int | float;
                pub type Pt = #{ x: Num, y: Num };
                let type = 1;
                let n = (3 as Num) * 2 as int + type;
                fn kind(v) { type(v) }";
    to_hir("t.orb", src, &ParseOptions::default()).unwrap();
}
