//! `#[orbit::native]`: typed parameters and results over the stack API.

use orbit::stack::{Any, Either, Fail, Nil, Rest, Return, Stack, StackResult, TableArg};
use orbit::{Chunk, Config, Runtime, native};

#[native(name = "add")]
fn add(a: i64, b: i64) -> i64 {
    a + b
}

/// Borrowed strings, with numbers converted in place as `lua_tolstring` does.
#[native(name = "join")]
fn join(a: &[u8], b: &[u8]) -> Vec<u8> {
    [a, b"|", b].concat()
}

#[native(name = "opt")]
fn opt(x: Option<i64>, label: Option<&str>) -> (i64, String) {
    (x.unwrap_or(-1), label.unwrap_or("none").to_owned())
}

/// The stack and typed arguments together: the table's length plus `n`.
#[native(name = "len_plus")]
fn len_plus(s: &mut Stack<'_>, t: TableArg, n: i64) -> i64 {
    s.raw_len(t.0) as i64 + n
}

#[native(name = "count")]
fn count(first: Any, rest: Rest) -> (Any, i64) {
    (first, rest.count as i64)
}

#[native(name = "check")]
fn check(n: i64) -> Result<Either<bool, (Nil, &'static str)>, Fail> {
    match n {
        0 => Ok(Either::Left(true)),
        1 => Ok(Either::Right((Nil, "one"))),
        _ => Err(Fail::arg(1, "too big")),
    }
}

/// Pushing by hand still works.
#[native(name = "raw")]
fn raw(s: &mut Stack<'_>) -> StackResult<Return> {
    s.push_integer(7);
    s.push_integer(8);
    Ok(Return::values(2))
}

#[test]
fn typed_natives() {
    let mut iso = Runtime::new().new_isolate(Config::default());
    iso.scope(|s| {
        let g = s.globals();
        g.set("add", s.create_native::<add>())?;
        g.set("join", s.create_native::<join>())?;
        g.set("opt", s.create_native::<opt>())?;
        g.set("len_plus", s.create_native::<len_plus>())?;
        g.set("count", s.create_native::<count>())?;
        g.set("check", s.create_native::<check>())?;
        g.set("raw", s.create_native::<raw>())?;
        let src = r##"
            assert(add(2, 3) == 5 and add("4", 2.0) == 6)
            assert(select(2, pcall(add, 1, "x")) == "bad argument #2 to 'add' (number expected, got string)")
            assert(select(2, pcall(add, 1.5, 1)) == "bad argument #1 to 'add' (number has no integer representation)")
            assert(join("a", 12) == "a|12")
            assert(select(2, pcall(join, {}, "b")) == "bad argument #1 to 'join' (string expected, got table)")
            local a, b = opt()
            assert(a == -1 and b == "none")
            a, b = opt(5, "x")
            assert(a == 5 and b == "x")
            assert(len_plus({1, 2, 3}, 10) == 13)
            assert(select(2, pcall(len_plus, 1, 2)) == "bad argument #1 to 'len_plus' (table expected, got number)")
            local t = {}
            local f, n = count(t, 1, 2, 3)
            assert(f == t and n == 3)
            assert(select(2, pcall(count)) == "bad argument #1 to 'count' (value expected)")
            assert(select("#", check(0)) == 1 and check(0) == true)
            local x, msg = check(1)
            assert(x == nil and msg == "one" and select("#", check(1)) == 2)
            assert(select(2, pcall(check, 2)) == "bad argument #1 to 'check' (too big)")
            local p, q = raw()
            assert(p == 7 and q == 8)
        "##;
        s.load(Chunk::new(src).name("=test"))?.call(())?;
        Ok::<_, orbit::Error>(())
    })
    .unwrap();
}
