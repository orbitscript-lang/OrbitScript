//! Lowers OrbitScript programs, compiles them and runs them on the VM, checking what they
//! print (or the error they raise).

use orbit::{Chunk, Config, Error, Isolate, Runtime, Value};
use orbit_compiler::bytecode::{UnlinkedChunk, format};
use orbit_orb_syntax::{ParseOptions, to_hir};

fn isolate() -> Isolate {
    Runtime::new().new_isolate(Config::default())
}

/// `chunk` as a binary chunk the isolate loads (keeping its debug info).
fn binary(chunk: &UnlinkedChunk) -> Vec<u8> {
    format::encode(chunk, false)
}

/// Runs `src`, returning its printed lines, then `error: <message>` if it raised one.
fn run(src: &str) -> String {
    let mut iso = isolate();
    iso.exec(
        Chunk::new(
            "__out = {}
             print = function(...)
               local t = table.pack(...)
               for i = 1, t.n do t[i] = tostring(t[i]) end
               __out[#__out + 1] = table.concat(t, '\\t')
             end",
        )
        .name("=prelude"),
    )
    .unwrap();
    let hir = match to_hir("test", src.as_bytes(), &ParseOptions::default()) {
        Ok(hir) => hir,
        Err(d) => return format!("compile error: {}", d.to_message("test")),
    };
    orbit_hir::verify(&hir).expect("lowered HIR verifies");
    let chunk = match orbit_compiler::compile(&hir) {
        Ok(c) => c,
        Err(e) => return format!("compile error: {e}"),
    };
    let bytes = binary(&chunk);
    iso.scope(|s| {
        let err = s.load(Chunk::new(&bytes).name("=test"))?.call(()).err();
        let out = s
            .load(Chunk::new("return table.concat(__out, '\\n')").name("=out"))?
            .call(())?;
        let mut text: String = out[0].get()?;
        if let Some(e) = err {
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(&format!("error: {e}"));
        }
        Ok::<_, Error>(text)
    })
    .unwrap()
}

/// The compile error for `src`.
fn compile_error(src: &str) -> String {
    match to_hir("test", src.as_bytes(), &ParseOptions::default()) {
        Ok(_) => panic!("expected a compile error for {src:?}"),
        Err(d) => d.to_message("test"),
    }
}

#[track_caller]
fn check(src: &str, expected: &str) {
    assert_eq!(
        run(src),
        expected.trim_start_matches('\n').trim_end(),
        "\n{src}"
    );
}

#[test]
fn hello() {
    check(r##"print("hello", 1 + 2);"##, "hello\t3");
}

#[test]
fn bindings_and_functions() {
    check(
        r##"
let mut x = 1;
x += 1;
x *= 10;
let x = f"shadowed {x}";
print(x);
fn add(a, b) { a + b }
let sq = |x| x * x;
let clamp = |x, lo, hi| {
    if x < lo { return lo; }
    if x > hi { hi } else { x }
};
print(add(2, 3), sq(4), clamp(5, 1, 3), clamp(0, 1, 3), clamp(2, 1, 3));
fn divmod(a: int, b: int) -> (int, int) { (a \ b, a % b) }
let (q, r) = divmod(7, 2);
print(q, r, 7 / 2, 2 ** 10, "a" ~ 1 ~ "b");
fn log(fmt: string, ...args) {
    print(fmt.format(...args));
}
log("%d-%s", 4, "x");
fn count(...rest) { select("#", ...rest) }
print(count(), count(nil, nil));
"##,
        "
shadowed 20
5\t16\t3\t1\t2
3\t1\t3.5\t1024.0\ta1b
4-x
0\t2",
    );
}

#[test]
fn tables() {
    check(
        r##"
let k = "dyn";
let arr = [10, 20, 30];
let map = #{ name: "orbit", "two words": true, [k]: 5 };
let mixed = #{ 10, 20, tag: "x" };
print(#arr, arr[1], map.name, map["two words"], map.dyn, mixed[2], mixed.tag);
arr[#arr + 1] = 40;
print(#arr, arr[4]);
fn three() { (1, 2, 3) }
let all = [0, ...three()];
let one = [three(), 9];
print(#all, #one, one[1]);
"##,
        "
3\t10\torbit\ttrue\t5\t20\tx
4\t40
4\t2\t1",
    );
}

#[test]
fn control_flow() {
    check(
        r##"
let mut out = [];
fn push(t, v) { t[#t + 1] = v; }
for i in 1..=3 { push(out, i); }
for i in (1..=3).rev() { push(out, i); }
for i in (0..=10).step_by(5) { push(out, i); }
for (i, v) in ipairs(["a", "b"]) { push(out, f"{i}{v}"); }
let mut i = 0;
while i < 3 { i += 1; if i == 2 { continue; } push(out, f"w{i}"); }
let found = loop { i += 1; if i > 5 { break i * 10; } };
push(out, found);
'outer: for a in 1..=3 {
    for b in 1..=3 {
        if b == 2 { continue 'outer; }
        if a == 3 { break 'outer; }
        push(out, f"{a}{b}");
    }
}
let sign = |x| if x > 0 { 1 } else if x < 0 { -1 } else { 0 };
push(out, sign(-4));
push(out, sign(0));
print(table::concat(out, " "));
"##,
        "1 2 3 3 2 1 0 5 10 1a 2b w1 w3 60 11 21 -1 0",
    );
}

#[test]
fn match_expressions() {
    check(
        r##"
fn label(n) {
    match n {
        0 => "none",
        1 | 2 => "few",
        3..=9 => "some",
        x if type(x) == "number" && x < 0 => f"negative {x}",
        "str" => "a string",
        _ => "many",
    }
}
print(label(0), label(2), label(5), label(-3), label(100), label("str"), label("zzz"));
fn pick(n) { match n { 1 => "one", 2 => "two" } }
print(pick(2));
print(pick("x"));
"##,
        "
none\tfew\tsome\tnegative -3\tmany\ta string\tmany
two
error: test:13: no match arm for value of type string",
    );
}

#[test]
fn structs_and_impl() {
    check(
        r##"
struct Vec2 { x: float, y: float }

impl Vec2 {
    pub fn new(x, y) -> Vec2 { Vec2 { x, y } }
    pub fn len(self) -> float { math::sqrt(self.x * self.x + self.y * self.y) }
    pub fn translate(self, dx, dy) { self.x += dx; self.y += dy; }
    fn __add(self, other) { Vec2::new(self.x + other.x, self.y + other.y) }
    fn __eq(self, other) { self.x == other.x && self.y == other.y }
    fn __tostring(self) { f"({self.x}, {self.y})" }
    fn twice(self) -> Self { self + self }
}

let p = Vec2::new(3, 4);
print(p.len(), p, p + Vec2 { x: 1, y: 1 });
p.translate(1, 1);
print(p, p == Vec2::new(4, 5), p == Vec2::new(0, 0), p.twice());
print(getmetatable(p) == Vec2, getmetatable(p).__name, Vec2.__name);
let m = p.len;
print(m(Vec2::new(0, 2)));
"##,
        "
5.0\t(3, 4)\t(4, 5)
(4, 5)\ttrue\tfalse\t(8, 10)
true\tVec2\tVec2
2.0",
    );
}

#[test]
fn enums() {
    check(
        r##"
enum Shape {
    Circle { r: float },
    Rect { w: float, h: float },
    Empty,
}

impl Shape {
    pub fn area(self) -> float {
        match self {
            Shape::Circle { r } => 3 * r * r,
            Shape::Rect { w, h: height } => w * height,
            Shape::Empty => 0,
        }
    }
    fn __tostring(self) { f"shape with area {self.area()}" }
}

let shapes = [Shape::Circle { r: 2 }, Shape::Rect { w: 2, h: 5 }, Shape::Empty];
for (_, s) in ipairs(shapes) { print(s.area(), s); }
print(getmetatable(shapes[1]).__name, getmetatable(shapes[3]).__name);
print(Shape::Empty == Shape::Empty, rawequal(shapes[3], Shape::Empty));
let c = Shape::Circle::new(#{ r: 1 });
print(c.area());
let big = |s| match s {
    Shape::Rect { w, .. } if w > 1 => "wide rect",
    Shape::Rect { .. } => "rect",
    _ => "other",
};
print(big(shapes[2]), big(Shape::Rect { w: 1, h: 1 }), big(c));
print(...pcall(|| { Shape::Empty.x = 1; }));
"##,
        "
12\tshape with area 12
10\tshape with area 10
0\tshape with area 0
Shape::Circle\tShape::Empty
true\ttrue
3
wide rect\trect\tother
false\ttest:31: cannot modify unit variant Shape::Empty",
    );
}

#[test]
fn try_operator() {
    check(
        r##"
fn get(t, k) { if t[k] == nil { (nil, f"no {k}") } else { t[k] } }
fn both(t) {
    let a = get(t, "a")?;
    let b = get(t, "b")?;
    a + b
}
print(both(#{ a: 1, b: 2 }));
print(...both(#{ a: 1 }));
print(both(#{}));       // one value: only a spread passes them all
"##,
        "
3
nil\tno b
nil",
    );
}

#[test]
fn templates() {
    check(
        r##"
let name = "orbit";
let n = 3;
let pct = 41.666;
print(f"{name} has {n} items");
print(f"{name} is {pct:.1}% done");
print(f"{n:>4}|{n:<4}|{n:04x}|{255:x}|{255:#X}|{n:+.1}|{1234.5:e}|{1234.5:.2E}");
print(f"{{literal braces}} {nil} {true} {"q\n":?}");
print(f"total: {n + 1}, first: {[7, 8][1]}");
print(f"no holes", f"", "end");
"##,
        r#"
orbit has 3 items
orbit is 41.7% done
   3|3   |0003|ff|0XFF|+3.0|1.234500e+03|1.23E+03
{literal braces} nil true "q\
"
total: 4, first: 7
no holes		end"#,
    );
}

#[test]
fn modules_and_names() {
    check(
        r##"
package::preload["util"] = || #{ trim: |s| s.gsub("^%s+", "").gsub("%s+$", ""), twice: |x| x * 2 };
use util;
use util::{trim, twice};
use util as u;
print(f"[{trim("  hi ")}]", twice(4), u::twice(5), util::trim(" x"));
extern HOST_VALUE;
print(HOST_VALUE);
global counter = 1;
counter += 1;
print(counter, _G.counter);
const LIMIT = 10;
print(LIMIT * 2);
let print = |...a| { _G::print("wrapped", ...a); };
print(1);
"##,
        "
[hi]\t8\t10\tx
nil
2\t2
20
wrapped\t1",
    );
}

#[test]
fn exports() {
    let mut iso = isolate();
    let src = r##"
pub fn parse(s) { tonumber(s) }
fn helper() { 1 }
pub struct Config { path: string }
pub const VERSION = "1.2";
pub enum Mode { Fast, Slow }
"##;
    let hir = to_hir("m", src.as_bytes(), &ParseOptions::default()).unwrap();
    let bytes = binary(&orbit_compiler::compile(&hir).unwrap());
    let shown: Vec<String> = iso
        .scope(|s| {
            let r = s.load(Chunk::new(&bytes).name("=m"))?.call(())?;
            s.globals().set("m", r[0])?;
            let out = s
                .load(
                    Chunk::new(
                        "local keys = {} for k in pairs(m) do keys[#keys + 1] = k end table.sort(keys)
                         return table.concat(keys, ' '), m.parse('42'), m.VERSION, m.Config.__name,
                           getmetatable(m.Mode.Fast).__name",
                    )
                    .name("=check"),
                )?
                .call(())?;
            out.iter()
                .map(|v: &Value| Ok(String::from_utf8_lossy(&v.to_display()?).into_owned()))
                .collect::<Result<Vec<_>, Error>>()
        })
        .unwrap();
    assert_eq!(
        shown,
        [
            "Config Mode VERSION parse",
            "42",
            "1.2",
            "Config",
            "Mode::Fast"
        ]
    );
}

#[test]
fn forward_references() {
    check(
        r##"
fn is_even(n) { if n == 0 { true } else { is_odd(n - 1) } }
fn is_odd(n) { if n == 0 { false } else { is_even(n - 1) } }
print(is_even(10), is_odd(7));
fn make() { Point { x: 1, y: 2 } }
struct Point { x, y }
print(make().y);
"##,
        "true\ttrue\n2",
    );
}

#[test]
fn hoisting_keeps_order() {
    check(
        r##"
let mut log = [];
fn note(v) { log[#log + 1] = v; v }
let mut x = 1;
let r = note(x) + if note(2) > 1 { x = 10; x } else { 0 };
print(r, table::concat(log, ","));
let t = [note("a"), match note(3) { 3 => note("three"), _ => "?" }, note("b")];
print(table::concat(t, ","), table::concat(log, ","));
let both = note(false) || loop { break note("l"); };
print(both);
let v = [1, 2, 3];
let sum = { let mut s = 0; for (_, n) in ipairs(v) { s += n; } s };
print(sum);
"##,
        "
11\t1,2
a,three,b\t1,2,a,3,three,b
l
6",
    );
}

#[test]
fn metamethods_and_index() {
    check(
        r##"
struct Bag { items }
impl Bag {
    fn size(self) { #self.items }
    fn __index(self, k) { f"missing {k}" }
    fn __len(self) { 99 }
    fn __call(self, x) { x + 1 }
    const __metatable = "locked";
}
let b = Bag { items: [1, 2] };
print(b.size(), b.nothing, #b, b(41), getmetatable(b));
enum Op { Add, Neg { x } }
impl Op {
    fn __tostring(self) { "op" }
    fn __eq(self, o) { true }
}
print(Op::Add, Op::Neg { x: 1 }, Op::Neg { x: 1 } == Op::Neg { x: 2 });
"##,
        "
2\tmissing nothing\t99\t42\tlocked
op\top\ttrue",
    );
}

#[test]
fn scoped_cleanup() {
    check(
        r##"
struct Lock { name }
impl Lock {
    fn __close(self, err) { print("release", self.name); }
}
fn work() {
    let scope a = Lock { name: "a" };
    let scope b = Lock { name: "b" };
    print("working");
}
work();
"##,
        "working\nrelease\tb\nrelease\ta",
    );
}

#[test]
fn scoped_actions() {
    // Actions and bindings run in reverse order, and an action sees the current values.
    check(
        r##"
struct Lock { name }
impl Lock {
    fn __close(self, err) { print("release", self.name); }
}
fn work() {
    let mut n = 1;
    let scope a = Lock { name: "a" };
    scope print("action 1, n is", n);
    let scope b = Lock { name: "b" };
    scope {
        print("action 2");
        print("block form");
    }
    n = 2;
    print("working");
}
work();
scope print("main done");
print("main");
"##,
        "working\naction 2\nblock form\nrelease\tb\naction 1, n is\t2\nrelease\ta\nmain\nmain done",
    );
    // Every way out of the block runs them.
    check(
        r##"
fn early(x) {
    scope print("left", x);
    if x { return "returned"; }
    "fell through"
}
print(early(true));
print(early(false));
for i in 1..=3 {
    scope print("end of", i);
    if i == 2 { continue; }
    if i == 3 { break; }
    print("body", i);
}
fn boom() {
    scope print("cleanup");
    error("bad");
}
print(...pcall(boom));
let nested = || {
    let inner = || { scope print("inner"); 1 };
    scope print("outer");
    inner()
};
print(nested());
"##,
        "left\ttrue\nreturned\nleft\tfalse\nfell through\nbody\t1\nend of\t1\nend of\t2\nend of\t3\n\
         cleanup\nfalse\ttest:17: bad\ninner\nouter\n1",
    );
    // Closing a suspended coroutine runs them too.
    check(
        r##"
let co = coroutine::create(|| {
    scope print("closed");
    coroutine::yield(1);
    print("never");
});
coroutine::resume(co);
print("suspended");
coroutine::close(co);
"##,
        "suspended\nclosed",
    );
    // Loops and closures inside an action may use their own `break` and `return`.
    check(
        r##"
scope {
    for i in 1..10 { if i == 2 { break; } print("loop", i); }
    let f = || { return "closure"; };
    print(f());
}
"##,
        "loop\t1\nclosure",
    );
    let cases = [
        (
            "fn f() { scope return 1; }",
            "test:1: `return` can't leave a `scope` action",
        ),
        (
            "fn f(t) { scope t?; }",
            "test:1: `?` can't leave a `scope` action",
        ),
        (
            "for i in 1..3 { scope break; }",
            "test:1: `break` can't leave a `scope` action",
        ),
        (
            "for i in 1..3 { scope { continue; } }",
            "test:1: `continue` can't leave a `scope` action",
        ),
    ];
    for (src, want) in cases {
        let got = compile_error(src);
        assert!(got.starts_with(want), "{src}\n  got: {got}\n want: {want}");
    }
}

#[test]
fn keywords_as_names() {
    check(
        r##"
let (k, v) = string::match("key=val", "(%w+)=(%w+)");
print(k, v, "abc".match("b"));
let r#match = 7;
let r#scope = 1;
print(r#match + r#scope, f"{r#match}");
let t = #{ loop: 1, if: 2, true: 3 };
print(t.loop, t["if"], t["true"]);
struct P { loop, pub: bool, pub r#fn }
impl P {
    pub fn match(self, x) { self.loop ~ x }
}
let p = P { loop: "m", pub: true, fn: 1 };
print(p.match("!"), p.pub, p.r#fn);
match p { P { loop: l, .. } => print(l) }
use string::{match as m};
print(m("ab", "b"));
"##,
        "key\tval\tb\n8\t7\n1\t2\t3\nm!\ttrue\t1\nm\nb",
    );
}

#[test]
fn compile_errors() {
    let cases = [
        (
            "let x = 1; x = 2;",
            "test:1: cannot assign twice to immutable variable `x`",
        ),
        ("print(nope);", "test:1: cannot find"),
        (
            "fn f(a) { a = 1; }",
            "test:1: cannot assign to immutable parameter `a`",
        ),
        ("print(late); let late = 1;", "test:1: cannot find"),
        ("pub fn f() {} return 1;", "test:1:"),
        ("print = 1;", "test:1:"),
        ("break;", "test:1:"),
        ("struct P { x } let p = P { y: 1 };", "test:1:"),
        (
            "enum E { A } let e = match 1 { E::B => 1, _ => 2 };",
            "test:1: no variant `B` in enum `E`",
        ),
        ("let a = [...x, 1];", "test:1:"),
        ("let x = 99999999999999999999;", "test:1:"),
        ("print(f\"{1:^5}\");", "test:1: format spec `^5`: centering"),
    ];
    for (src, want) in cases {
        let got = compile_error(src);
        assert!(got.starts_with(want), "{src}\n  got: {got}\n want: {want}");
    }
}

#[test]
fn tour() {
    check(
        include_str!("programs/tour.orb"),
        r#"
6.4031242374328485	(4, 5)	(5, 6)	true
12 10 0 none some some 3 2 1 0 5 10 w1 w3 60 11 21
3 1 a8.0b
3	nil	no b
true	nil	3
 orbit|7  |0005|0.333|"s"
6	big	true
4
release	a"#,
    );
}

/// Mutated programs give an error or a chunk the compiler accepts, never a panic.
#[test]
fn mutations_never_panic() {
    let base = include_bytes!("programs/tour.orb");
    let alphabet = b"{}()[]#|&:;.,=<>!?\"'f r\\/*-+~^%_09xa\n";
    let mut seed: u64 = 0x9e37_79b9_7f4a_7c15;
    let mut next = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    let mut lowered = 0;
    for _ in 0..6000 {
        let mut src = base.to_vec();
        for _ in 0..(1 + next() % 3) {
            let at = (next() % src.len() as u64) as usize;
            match next() % 3 {
                0 => {
                    src.remove(at);
                }
                1 => src.insert(at, alphabet[(next() % alphabet.len() as u64) as usize]),
                _ => src[at] = alphabet[(next() % alphabet.len() as u64) as usize],
            }
        }
        if let Ok(hir) = to_hir("m", &src, &ParseOptions::default()) {
            orbit_hir::verify(&hir).expect("lowered HIR verifies");
            let _ = orbit_compiler::compile(&hir);
            lowered += 1;
        }
    }
    // About one in ten mutants still lowers; make sure the test keeps exercising lowering.
    assert!(lowered > 400, "only {lowered} mutated programs lowered");
}

#[test]
fn closures_and_tuples() {
    check(
        r##"
let mut fs = [];
for i in 1..=3 { fs[#fs + 1] = || i; }
print(fs[1](), fs[3]());
fn counter() {
    let mut n = 0;
    || { n += 1; n }
}
let c = counter();
c();
print(c());
fn classify(a, b) {
    match (a, b) {
        (0, 0) => "origin",
        (0, _) | (_, 0) => "axis",
        (x, y) if x == y => "diagonal",
        _ => "plane",
    }
}
print(classify(0, 0), classify(0, 5), classify(2, 2), classify(1, 2));
struct Pair { a, b }
struct Wrap { inner }
let w = Wrap { inner: Pair { a: 1, b: "two" } };
let d = match w {
    Wrap { inner: Pair { a: 1, b } } => f"one and {b}",
    _ => "other",
};
print(d);
"##,
        "
1\t3
2
origin\taxis\tdiagonal\tplane
one and two",
    );
}

#[test]
fn exclusive_ranges() {
    check(
        r##"
fn collect(f) { let mut t = []; f(|v| { t[#t + 1] = v; }); f"[{table::concat(t, " ")}]" }
print(collect(|push| { for i in 0..4 { push(i); } }));
print(collect(|push| { for i in 3..3 { push(i); } }));
print(collect(|push| { for i in (0..4).rev() { push(i); } }));
print(collect(|push| { for i in (0..10).step_by(3) { push(i); } }));
print(collect(|push| { for i in (0..9).step_by(3) { push(i); } }));
print(collect(|push| { for i in (10..0).step_by(-4) { push(i); } }));
print(collect(|push| { for i in (0..10).rev().step_by(4) { push(i); } }));
print(collect(|push| { for x in 0..2.5 { push(x); } }));
print(collect(|push| { for x in 0..3.0 { push(x); } }));
print(collect(|push| { for x in 0.5..2.5 { push(x); } }));
print(collect(|push| { for x in (0.0..1.0).step_by(0.25) { push(x); } }));
print(collect(|push| { for x in (1.0..0.0).step_by(-0.5) { push(x); } }));
let max = math::maxinteger;
let min = math::mininteger;
print(collect(|push| { for i in (max - 2)..max { push(i - max); } }));
print(collect(|push| { for i in min..min { push(i); } }));
print(collect(|push| { for i in ((min + 2)..min).step_by(-1) { push(i - min); } }));
print(collect(|push| { for i in 0..1e300 { if i > 2 { break; } push(i); } }));
print(collect(|push| { for i in 0..-1e300 { push(i); } }));
print(collect(|push| { for i in 0..(0.0 / 0.0) { push(i); } }));
print(...pcall(|| { for i in 0..10 { if i > 1 { break; } } for i in (0..1).step_by(0) {} }));
"##,
        r#"
[0 1 2 3]
[]
[3 2 1 0]
[0 3 6 9]
[0 3 6]
[10 6 2]
[9 5 1]
[0 1 2]
[0 1 2]
[0.5 1.5]
[0.0 0.25 0.5 0.75]
[1.0 0.5]
[-2 -1]
[]
[2 1]
[0 1 2]
[]
[]
false	test:23: 'for' step is zero"#,
    );
}

#[test]
fn match_arm_assignments() {
    check(
        r##"
let mut stack = [1, 2, 3];
let mut n = 0;
for (_, k) in ipairs(["end", "add", "x"]) {
    let r = match k {
        "end" => stack[#stack] = nil,
        "add" => n += 10,
        _ => {}
    };
    print(k, r);
}
print(#stack, n);
"##,
        "end\tnil\nadd\tnil\nx\tnil\n2\t10",
    );
    assert_eq!(
        compile_error("match 1 { _ => 1 + 2 = 3 }"),
        "test:1: invalid left-hand side of assignment"
    );
}

#[test]
fn map_literal_bare_names_are_rejected() {
    assert_eq!(
        compile_error("let word = 1;\nlet t = #{ word, n: 2 };"),
        "test:2: a bare name in `#{ }` is ambiguous: write `word: word` for a field, `(word)` for an array entry, or use `[...]` for an array"
    );
    check(
        r##"
let word = "w";
let t = #{ (word), word: word, word ~ "!", n: 2 };
print(t[1], t[2], t.word, t.n);
"##,
        "w\tw!\tw\t2",
    );
}

#[test]
fn methods_are_reachable_through_the_type() {
    check(
        r##"
struct Acc { n }
impl Acc {
    fn new() { Acc { n: 0 } }
    fn add(self, k) { self.n += k; self.n }
}
enum Shape { Sq { s }, Empty }
impl Shape {
    fn area(self) { match self { Shape::Sq { s } => s * s, Shape::Empty => 0 } }
}
let a = Acc::new();
print(Acc::add(a, 2), a.add(3));
let (ok, v) = pcall(Acc::add, a, 5);
print(ok, v, a.new, getmetatable(a) == Acc);
print(Shape::area(Shape::Sq { s: 3 }), Shape::area(Shape::Empty), Shape::Empty.area());
"##,
        "2\t5\ntrue\t10\tnil\ttrue\n9\t0\t0",
    );
    assert_eq!(
        compile_error("struct P {}\nimpl P { fn f() {} }\nimpl P {\n  fn f(self) {}\n}"),
        "test:4: `f` is already defined for this type"
    );
}
