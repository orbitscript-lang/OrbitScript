//! The check that formatting changed only the layout: the input and the re-parsed output
//! must have the same AST once spans are left out, and the same comments.
//!
//! The comparison is on a dump of each top-level statement, with names and strings written
//! out (each parse interns them in its own order) and every source offset removed. `use`
//! lines are compared as sets within each run of them, since the formatter sorts them.

use orbit_orb_syntax::ast::{Ast, ItemKind, Stmt, StmtKind, UseTail};

/// The offset in `src` of the first top-level statement whose meaning changed, if any.
pub(crate) fn first_difference(before: &Ast, after: &Ast) -> Option<u32> {
    let a = dump_body(before);
    let b = dump_body(after);
    let at = |i: usize| {
        before
            .body
            .stmts
            .get(i)
            .map_or(before.body.span.end, |s| s.span.start)
    };
    for (i, (x, y)) in a.iter().zip(&b).enumerate() {
        if x != y {
            return Some(at(i));
        }
    }
    (a.len() != b.len()).then(|| at(a.len().min(b.len())))
}

/// Whether both sides hold the same comments, as text, in any order.
pub(crate) fn same_comments(src: &[u8], before: &Ast, out: &[u8], after: &Ast) -> bool {
    let texts = |src: &[u8], ast: &Ast| {
        let mut v: Vec<Vec<u8>> = ast
            .comments
            .iter()
            .map(|c| {
                src[c.span.start as usize..c.span.end as usize]
                    .trim_ascii_end()
                    .to_vec()
            })
            .collect();
        v.sort();
        v
    };
    texts(src, before) == texts(out, after)
}

/// One string per top-level statement (and the tail), with each run of `use` lines sorted.
fn dump_body(ast: &Ast) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut run_start: Option<usize> = None;
    for stmt in &ast.body.stmts {
        match use_line(ast, stmt) {
            Some(line) => {
                run_start.get_or_insert(out.len());
                out.push(line);
            }
            None => {
                if let Some(s) = run_start.take() {
                    out[s..].sort();
                }
                out.push(scrub(ast, &format!("{stmt:?}")));
            }
        }
    }
    if let Some(s) = run_start {
        out[s..].sort();
    }
    if let Some(tail) = &ast.body.tail {
        out.push(scrub(ast, &format!("{tail:?}")));
    }
    out
}

/// A `use` line written out, its member names sorted.
fn use_line(ast: &Ast, stmt: &Stmt) -> Option<String> {
    let StmtKind::Item(item) = &stmt.kind else {
        return None;
    };
    let ItemKind::Use(u) = &item.kind else {
        return None;
    };
    let name = |n: &orbit_orb_syntax::ast::Name| ast.strings.display(n.sym).into_owned();
    let path: Vec<String> = u.path.iter().map(name).collect();
    let tail = match &u.tail {
        UseTail::Module { alias } => alias
            .as_ref()
            .map(|a| format!(" as {}", name(a)))
            .unwrap_or_default(),
        UseTail::Members(members) => {
            let mut names: Vec<String> = members
                .iter()
                .map(|(n, alias)| match alias {
                    Some(a) => format!("{} as {}", name(n), name(a)),
                    None => name(n),
                })
                .collect();
            names.sort();
            format!("::{{{}}}", names.join(", "))
        }
    };
    Some(format!(
        "{}use {}{tail} docs={}",
        if item.is_pub { "pub " } else { "" },
        path.join("::"),
        item.docs.len()
    ))
}

/// `dump` without source offsets, and with each `StrId(n)` replaced by its string.
fn scrub(ast: &Ast, dump: &str) -> String {
    let mut out = String::with_capacity(dump.len());
    let mut rest = dump;
    while !rest.is_empty() {
        if let Some(r) = rest.strip_prefix("Span { start: ") {
            // `Span { start: N, end: N }`
            let close = r.find('}').map_or(r.len(), |i| i + 1);
            out.push_str("Span");
            rest = &r[close..];
        } else if let Some(r) = rest.strip_prefix("StrId(") {
            let close = r.find(')').unwrap_or(r.len());
            match r[..close].parse::<u32>() {
                Ok(n) => {
                    out.push_str(&format!("{:?}", ast.strings.display(orbit_hir::StrId(n))));
                }
                Err(_) => out.push_str(&r[..close]),
            }
            rest = r.get(close + 1..).unwrap_or("");
        } else if let Some(r) = rest
            .strip_prefix("op_at: ")
            .or_else(|| rest.strip_prefix("args_at: "))
        {
            let digits = r.find(|c: char| !c.is_ascii_digit()).unwrap_or(r.len());
            out.push_str("at");
            rest = &r[digits..];
        } else {
            let c = rest.chars().next().expect("non-empty");
            out.push(c);
            rest = &rest[c.len_utf8()..];
        }
    }
    out
}
