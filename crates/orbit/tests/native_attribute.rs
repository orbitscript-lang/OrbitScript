//! `#[orbit::native]`: typed parameters and results over the stack API.

use std::cell::Cell;
use std::rc::Rc;

use orbit::stack::{
    Any, Either, Fail, Nil, Number, REGISTRY_INDEX, Rest, Return, Stack, StackResult, State,
    TableArg, Ud, UserType,
};
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

/// Natives are told apart by type: one named like a built-in is its own
/// function, and so are two that share a name.
mod first {
    #[orbit::native(name = "tostring")]
    pub fn close() -> i64 {
        1
    }
}

mod second {
    #[orbit::native(name = "tostring")]
    pub fn close() -> i64 {
        2
    }
}

#[test]
fn natives_sharing_a_name_stay_distinct() {
    let mut iso = Runtime::new().new_isolate(Config::default());
    iso.scope(|s| {
        let g = s.globals();
        g.set("first", s.create_native::<first::close>())?;
        g.set("second", s.create_native::<second::close>())?;
        let src = r##"
            assert(first ~= tostring and second ~= tostring and first ~= second)
            assert(first() == 1 and second() == 2 and tostring(3) == "3")
        "##;
        s.load(Chunk::new(src).name("=test"))?.call(())?;
        Ok::<_, orbit::Error>(())
    })
    .unwrap();
}

/// `math.abs`'s rule: an integer stays one, a float or a numeric string is
/// a float.
#[native(name = "num.abs")]
fn abs(n: Number) -> Number {
    match n {
        Number::Int(i) => Number::Int(i.wrapping_abs()),
        Number::Float(f) => Number::Float(f.abs()),
    }
}

/// A host value scripts hold, shared so natives can change it.
#[derive(Clone)]
struct Counter(Rc<Cell<i64>>);

impl UserType for Counter {
    const NAME: &'static str = "Counter";
}

#[native(name = "Counter.bump")]
fn bump(c: Ud<Counter>, by: Option<i64>) -> i64 {
    let n = &(c.0).0;
    n.set(n.get() + by.unwrap_or(1));
    n.get()
}

#[native(name = "new_counter")]
fn new_counter(s: &mut Stack<'_>) -> StackResult<Return> {
    s.push_userdata(Counter(Rc::new(Cell::new(0))));
    s.raw_get_field(REGISTRY_INDEX, b"Counter")?;
    s.set_metatable(-2)?;
    Ok(Return::values(1))
}

/// Per-isolate state, shared by every call in the isolate.
#[derive(Clone, Default)]
struct Calls(Rc<Cell<i64>>);

#[native(name = "calls")]
fn calls(state: State<Calls>, add: &[u8]) -> i64 {
    // The state takes no argument slot, and can sit beside borrowed
    // arguments.
    let n = &(state.0).0;
    n.set(n.get() + add.len() as i64);
    n.get()
}

#[native(name = "boom")]
fn boom(n: i64) -> i64 {
    if n > 0 {
        panic!("too far");
    }
    n
}

/// The attribute pointed at the API by another path.
mod renamed {
    use orbit as api;

    #[api::native(crate = api, name = "renamed.twice")]
    pub fn twice(n: i64) -> i64 {
        2 * n
    }
}

#[test]
fn numbers_userdata_state_and_panics() {
    let mut iso = Runtime::new().new_isolate(Config::default());
    iso.scope(|s| {
        let g = s.globals();
        let num = s.create_table();
        num.add_native::<abs>()?;
        num.add_native::<renamed::twice>()?;
        g.set("num", num)?;
        g.set("new_counter", s.create_native::<new_counter>())?;
        g.set("calls", s.create_native::<calls>())?;
        g.set("boom", s.create_native::<boom>())?;
        let class = s.define_class::<Counter>("Counter", |b| {
            b.native::<bump>("bump")?;
            Ok(())
        })?;
        s.registry().raw_set("Counter", class.metatable(s)?)?;
        let src = r##"
            assert(math.type(num.abs(-3)) == "integer" and num.abs(-3) == 3)
            assert(math.type(num.abs(-2.5)) == "float" and num.abs(-2.5) == 2.5)
            assert(math.type(num.abs("-3")) == "float" and num.abs("-3") == 3.0)
            assert(select(2, pcall(num.abs, {})) == "bad argument #1 to 'num.abs' (number expected, got table)")
            assert(num.twice(21) == 42)
            local c = new_counter()
            assert(c:bump() == 1 and c:bump(10) == 11)
            assert(select(2, pcall(c.bump, {})) == "bad argument #1 to 'Counter.bump' (Counter expected, got table)")
            assert(select(2, pcall(function() return c.bump(nil) end)):find("Counter expected, got nil", 1, true))
            assert(calls("ab") == 2 and calls("cde") == 5)
            assert(select(2, pcall(calls)) == "bad argument #1 to 'calls' (string expected, got no value)")
            assert(boom(0) == 0)
            local ok, msg = pcall(boom, 1)
            assert(not ok and msg:find("panicked: too far", 1, true), msg)
        "##;
        s.load(Chunk::new(src).name("=test"))?.call(())?;
        Ok::<_, orbit::Error>(())
    })
    .unwrap();
}
