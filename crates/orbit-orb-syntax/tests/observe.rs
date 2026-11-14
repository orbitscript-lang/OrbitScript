//! What lowering reports to an observer (`lower_observed`): each definition once, each use
//! with the handle its definition was given, and scopes that nest.

use std::path::{Path, PathBuf};

use orbit_hir::Span;
use orbit_orb_syntax::ast::{self, Ast};
use orbit_orb_syntax::observe::{Assign, Local, Observer, TypeNames};
use orbit_orb_syntax::{ParseOptions, lower_collecting, lower_observed, parse_recovering};

/// Writes down what it is told, one line each. A definition's handle is its number in the
/// order of definition, from 1; 0 is nothing.
struct Log<'a> {
    ast: &'a Ast,
    src: &'a [u8],
    lines: Vec<String>,
    defined: u32,
    /// Open scopes, and open `impl` blocks.
    open: (i32, i32),
    /// Whether a scope or block was closed that wasn't open, or a handle given that no
    /// definition has.
    broken: bool,
}

impl<'a> Log<'a> {
    fn new(ast: &'a Ast, src: &'a [u8]) -> Log<'a> {
        Log {
            ast,
            src,
            lines: Vec::new(),
            defined: 0,
            open: (0, 0),
            broken: false,
        }
    }

    fn text(&self, name: ast::Name) -> String {
        self.ast.strings.display(name.sym).into_owned()
    }

    fn define(&mut self, what: &str, name: ast::Name) -> u32 {
        self.defined += 1;
        let line = format!("{what} {} = {}", self.text(name), self.defined);
        self.lines.push(line);
        self.defined
    }

    fn handle(&mut self, sym: u32) -> u32 {
        self.broken |= sym > self.defined;
        sym
    }
}

impl<'a> Observer<'a> for Log<'a> {
    type Sym = u32;
    const NOTHING: u32 = 0;
    const ACTIVE: bool = true;

    fn scope_opened(&mut self, _: Span) {
        self.open.0 += 1;
        self.lines.push("{".to_string());
    }

    fn scope_closed(&mut self) {
        self.open.0 -= 1;
        self.broken |= self.open.0 < 0;
        self.lines.push("}".to_string());
    }

    fn function_opened(&mut self, item: u32, _: Span) {
        self.open.0 += 1;
        let item = self.handle(item);
        self.lines.push(format!("function {item} {{"));
    }

    fn item_declared(&mut self, item: &'a ast::Item, _: bool) -> u32 {
        let name = match &item.kind {
            ast::ItemKind::Fn(f) => f.name,
            ast::ItemKind::Const { name, .. } | ast::ItemKind::TypeAlias { name, .. } => *name,
            ast::ItemKind::Struct(s) => s.name,
            ast::ItemKind::Enum(e) => e.name,
            _ => panic!("not an item that is declared"),
        };
        self.define("item", name)
    }

    fn impl_declared(&mut self, block: &'a ast::ImplDecl, target: u32) {
        let target = self.handle(target);
        let line = format!("impl {} is {target}", self.text(block.target));
        self.lines.push(line);
    }

    fn item_defined(&mut self, item: u32) {
        let item = self.handle(item);
        self.lines.push(format!("defined {item}"));
    }

    fn impl_opened(&mut self, _: &'a ast::ImplDecl, target: u32) {
        self.open.1 += 1;
        let target = self.handle(target);
        self.lines.push(format!("impl {target} ["));
    }

    fn impl_generic(&mut self, param: &'a ast::GenericParam) -> u32 {
        self.define("impl generic", param.name)
    }

    fn impl_member(&mut self, member: &'a ast::Item) -> u32 {
        let name = match &member.kind {
            ast::ItemKind::Fn(f) => f.name,
            ast::ItemKind::Const { name, .. } => *name,
            _ => panic!("not a member"),
        };
        self.define("member", name)
    }

    fn impl_closed(&mut self) {
        self.open.1 -= 1;
        self.broken |= self.open.1 < 0;
        self.lines.push("]".to_string());
    }

    fn generic(&mut self, param: &'a ast::GenericParam, owner: u32) -> u32 {
        let owner = self.handle(owner);
        let id = self.define("generic", param.name);
        self.lines.push(format!("  of {owner}"));
        id
    }

    fn module_used(&mut self, _: &'a ast::UseDecl, name: ast::Name, _: Span) -> u32 {
        self.define("module", name)
    }

    fn member_used(
        &mut self,
        _: &'a ast::UseDecl,
        member: ast::Name,
        alias: Option<ast::Name>,
        _: Span,
    ) -> u32 {
        self.define("import", alias.unwrap_or(member))
    }

    fn global(&mut self, name: ast::Name, _: Span, _: Option<&'a ast::Expr>, _: bool) -> u32 {
        self.define("global", name)
    }

    fn param(&mut self, name: ast::Name, _: &'a ast::Param) -> u32 {
        self.define("param", name)
    }

    fn rest_param(&mut self, name: ast::Name, _: Option<&'a ast::Type>) -> u32 {
        self.define("rest", name)
    }

    fn local(&mut self, name: ast::Name, local: Local<'a>) -> u32 {
        let what = match local.def {
            Some(def) if def.slot.is_some() => "let (..)",
            Some(_) => "let",
            None => "bound",
        };
        self.define(what, name)
    }

    fn assigned(&mut self, target: u32, _: Assign<'a>) {
        let target = self.handle(target);
        self.lines.push(format!("assigned {target}"));
    }

    fn reference(&mut self, span: Span, sym: u32) {
        let sym = self.handle(sym);
        let text = &self.src[span.start as usize..span.end as usize];
        let line = format!("use {} is {sym}", String::from_utf8_lossy(text));
        self.lines.push(line);
    }

    fn prelude(&mut self, name: &[u8]) -> u32 {
        self.lines
            .push(format!("prelude {}", String::from_utf8_lossy(name)));
        0
    }

    fn member(&mut self, base: u32, _: bool, name: ast::Name) -> u32 {
        let base = self.handle(base);
        let line = format!("member {} of {base}", self.text(name));
        self.lines.push(line);
        0
    }

    fn field(&mut self, owner: u32, name: ast::Name) {
        let owner = self.handle(owner);
        let line = format!("field {} of {owner}", self.text(name));
        self.lines.push(line);
    }

    fn ty(&mut self, ty: &'a ast::Type, names: &dyn TypeNames<u32>) {
        // The names of a plain type path, as lowering's scopes know them.
        if let Some(ast::TypeKind::Path { segments, .. }) = ty.kind.as_deref() {
            let name = segments[0];
            let found = names.lookup(name.sym).map(|s| self.handle(s));
            let line = format!("type {} is {found:?}", self.text(name));
            self.lines.push(line);
        }
    }
}

fn options() -> ParseOptions {
    ParseOptions {
        type_trees: true,
        ..Default::default()
    }
}

fn log(src: &str) -> Vec<String> {
    let (ast, syntax) = parse_recovering("test.orb", src.as_bytes(), &options());
    assert!(syntax.is_empty(), "{syntax:?}");
    let (errors, log) = lower_observed(&ast, Log::new(&ast, src.as_bytes()));
    assert!(errors.is_empty(), "{errors:?}");
    assert!(!log.broken && log.open == (0, 0));
    log.lines
}

#[test]
fn definitions_and_uses() {
    let found = log("\
fn twice(n: int) -> int {
    let m = n + n;
    m
}
print(twice(2));
");
    let expected = "\
{
item twice = 1
defined 1
function 1 {
type int is None
type int is None
param n = 2
{
use n is 2
use n is 2
let m = 3
use m is 3
}
}
prelude print
use print is 0
use twice is 1
}";
    assert_eq!(found.join("\n"), expected);
}

#[test]
fn items_are_declared_before_the_block_runs() {
    // `area` uses `Shape` and `PI`, defined after it; `impl` members belong to their type
    // from the start of the block. A function's result type is read before its parameters.
    let found = log("\
fn area(s: Shape) -> float { PI * s.r * s.r }
struct Shape { r: float }
impl Shape {
    fn unit() -> Self { Self { r: 1.0 } }
}
const PI = 3.14;
");
    let expected = "\
{
item area = 1
item Shape = 2
item PI = 3
impl Shape is 2
defined 1
function 1 {
type float is None
type Shape is Some(2)
param s = 4
{
use PI is 3
use s is 4
use s is 4
}
}
defined 2
{
type float is None
}
use Shape is 2
{
impl 2 [
member unit = 5
function 5 {
{
use Self is 2
field r of 2
}
}
]
}
defined 3
}";
    assert_eq!(found.join("\n"), expected);
}

#[test]
fn patterns_loops_and_assignments() {
    let found = log("\
enum E { A { x: int }, B }
fn f(e: E, ...rest) {
    let mut total = 0;
    for (i, v) in ipairs([...rest]) {
        total += v;
    }
    match e {
        E::A { x } => total = x,
        E::B => {}
    }
}
");
    let expected = "\
{
item E = 1
item f = 2
defined 1
{
type int is None
}
defined 2
function 2 {
type E is Some(1)
param e = 3
rest rest = 4
{
let total = 5
prelude ipairs
use ipairs is 0
use rest is 4
{
bound i = 6
bound v = 7
{
use total is 5
assigned 5
use v is 7
}
}
use e is 3
use E is 1
member A of 1
field x of 0
{
bound x = 8
{
use total is 5
assigned 5
use x is 8
}
}
use E is 1
member B of 1
{
{
}
}
}
}
}";
    assert_eq!(found.join("\n"), expected);
}

/// The OrbitScript programs of the examples, the benchmarks and both crates' tests.
fn programs() -> Vec<PathBuf> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut out = Vec::new();
    let mut dirs = vec![
        root.join("examples"),
        root.join("bench/orb"),
        root.join("crates/orbit-orb-syntax/tests/programs"),
        root.join("crates/orbit-check/tests/cases"),
        root.join("crates/orbit-check/stubs"),
    ];
    while let Some(dir) = dirs.pop() {
        for e in std::fs::read_dir(&dir).unwrap() {
            let p = e.unwrap().path();
            if p.is_dir() {
                dirs.push(p);
            } else if p.extension().is_some_and(|x| x == "orb" || x == "orbi") {
                out.push(p);
            }
        }
    }
    out.sort();
    assert!(out.len() > 60, "found {}", out.len());
    out
}

#[test]
fn scopes_nest_and_handles_come_from_definitions() {
    for path in programs() {
        let name = path.display().to_string();
        let src = std::fs::read(&path).unwrap();
        // Also where the parse recovered, and in a stub, whose items have no bodies.
        let (ast, _) = parse_recovering(&name, &src, &options());
        let (errors, log) = lower_observed(&ast, Log::new(&ast, &src));
        assert!(!log.broken, "{name}");
        assert_eq!(log.open, (0, 0), "{name}");
        // Observing changes nothing about the errors.
        assert_eq!(errors, lower_collecting(&ast), "{name}");
    }
}
