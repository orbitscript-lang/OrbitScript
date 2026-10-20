//! Awkward signatures for `#[native]`.

use orbit::stack::{Stack, TableArg};
use orbit::{Chunk, Config, Runtime, native};

/// Doc comment, attributes, `pub(crate)`, `mut` and tuple-free patterns.
#[native]
#[allow(clippy::needless_pass_by_value)]
pub(crate) fn docs(mut a: i64, _: i64) -> i64 {
    a += 1;
    a
}

/// Nested generics (`>>` is two tokens) and full paths with `::`.
#[native]
fn nested(v: Option<Vec<u8>>, t: std::string::String) -> std::option::Option<i64> {
    Some(v.map_or(0, |v| v.len() as i64) + t.len() as i64)
}

/// The stack in the middle, and a trailing comma.
#[native]
fn middle(a: i64, s: &mut Stack<'_>, t: Option<TableArg>) -> i64 {
    a + t.map_or(0, |t| s.raw_len(t.0) as i64)
}

/// No arguments, no return type.
#[native]
fn nothing() {}

/// A `where`-free fn returning `impl` (the return type is never parsed).
#[native]
fn imp(x: i64) -> impl orbit::stack::Results {
    (x, x * 2)
}

#[test]
fn awkward_signatures() {
    let mut iso = Runtime::new().new_isolate(Config::default());
    iso.scope(|s| {
        let g = s.globals();
        g.set("docs", s.create_native::<docs>())?;
        g.set("nested", s.create_native::<nested>())?;
        g.set("middle", s.create_native::<middle>())?;
        g.set("nothing", s.create_native::<nothing>())?;
        g.set("imp", s.create_native::<imp>())?;
        s.load(Chunk::new(
            r##"
            assert(docs(1, 2) == 2)
            assert(nested("abc", "de") == 5 and nested(nil, "") == 0)
            assert(middle(1, {1, 2}) == 3 and middle(1) == 1)
            assert(select("#", nothing()) == 0)
            local a, b = imp(3)
            assert(a == 3 and b == 6)
            "##,
        ))?
        .call(())?;
        Ok::<_, orbit::Error>(())
    })
    .unwrap();
}
