//! What lowering tells a tool about the names in a module, as it resolves them.
//!
//! Lowering is the one place that knows OrbitScript's scoping: where a scope opens, when a
//! name comes into it, what a use refers to. The compiler needs none of that afterwards. A
//! checker or an editor needs all of it, so lowering reports each step to an [`Observer`]:
//! every definition, reference and scope, with the syntax that made it.
//!
//! The observer answers each definition with a [`Observer::Sym`], its own handle for the
//! thing defined. Lowering keeps the handle beside the binding in its scopes and hands it
//! back with every reference, so the observer never looks a name up itself. Questions only
//! the observer can answer go the same way: what a prelude name is ([`Observer::prelude`]),
//! what `module::item` names in another file ([`Observer::member`]).
//!
//! The compiler's observer is [`Nothing`]: its handle is `()`, every method is empty and
//! inlined away, and [`Observer::ACTIVE`] is `false`, a constant that removes the few places
//! where lowering does work only to tell the observer (reading a type annotation, say).

use orbit_hir::{Span, StrId};

use crate::ast;

/// Receives the definitions, references and scopes of a module while it is lowered. `'a` is
/// the syntax tree's lifetime: the observer may keep the nodes it is given.
///
/// Every method has an empty default, so an observer implements what it uses.
pub trait Observer<'a> {
    /// The observer's handle for something a name can stand for.
    type Sym: Copy;

    /// The handle for a name with nothing behind it: `self`, or a name that didn't resolve.
    const NOTHING: Self::Sym;

    /// Whether anything is observed. Lowering tests it before work done only for the observer.
    const ACTIVE: bool;

    // ---- scopes -------------------------------------------------------------------------------

    /// A scope opens over `extent`: a block, a function, a `match` arm, a `for` loop.
    fn scope_opened(&mut self, extent: Span) {
        let _ = extent;
    }

    /// The innermost scope closes.
    fn scope_closed(&mut self) {}

    /// A function's scope opens over `extent`, for its generic parameters and parameters: the
    /// function is `item` (an `fn` item or an `impl` member), or a closure when `item` is
    /// [`Observer::NOTHING`]. Closed with [`Observer::scope_closed`].
    fn function_opened(&mut self, item: Self::Sym, extent: Span) {
        let _ = (item, extent);
    }

    // ---- items --------------------------------------------------------------------------------

    /// An item of a block being entered (`fn`, `const`, `struct`, `enum` or `type`): items are
    /// declared together, before any statement of the block is lowered, because code earlier
    /// in the block may refer to them. `module_level` is whether the block is the module.
    fn item_declared(&mut self, item: &'a ast::Item, module_level: bool) -> Self::Sym {
        let _ = (item, module_level);
        Self::NOTHING
    }

    /// An `impl` block of a block being entered, after every item of the block is declared.
    /// `target` is what its target names there.
    fn impl_declared(&mut self, block: &'a ast::ImplDecl, target: Self::Sym) {
        let _ = (block, target);
    }

    /// Lowering reaches the definition of a declared item: from here the name is bound.
    fn item_defined(&mut self, item: Self::Sym) {
        let _ = item;
    }

    /// An `impl` block's generic parameters and members follow, until
    /// [`Observer::impl_closed`]. `target` is what its target names where it stands.
    fn impl_opened(&mut self, block: &'a ast::ImplDecl, target: Self::Sym) {
        let _ = (block, target);
    }

    /// A generic parameter of the open `impl` block.
    fn impl_generic(&mut self, param: &'a ast::GenericParam) -> Self::Sym {
        let _ = param;
        Self::NOTHING
    }

    /// A member of the open `impl` block is lowered next. The answer is the member itself.
    fn impl_member(&mut self, member: &'a ast::Item) -> Self::Sym {
        let _ = member;
        Self::NOTHING
    }

    fn impl_closed(&mut self) {}

    /// A generic parameter of the item `owner`.
    fn generic(&mut self, param: &'a ast::GenericParam, owner: Self::Sym) -> Self::Sym {
        let _ = (param, owner);
        Self::NOTHING
    }

    /// `use path;` or `use path as alias;` binds `name` to a module.
    fn module_used(&mut self, decl: &'a ast::UseDecl, name: ast::Name, full: Span) -> Self::Sym {
        let _ = (decl, name, full);
        Self::NOTHING
    }

    /// `use path::{member}` or `use path::{member as alias}` binds a name to an item of a
    /// module.
    fn member_used(
        &mut self,
        decl: &'a ast::UseDecl,
        member: ast::Name,
        alias: Option<ast::Name>,
        full: Span,
    ) -> Self::Sym {
        let _ = (decl, member, alias, full);
        Self::NOTHING
    }

    /// `global name = value;` (with its value), or a name of `extern a, b;`. `exported` is
    /// whether it is `pub` at module level.
    fn global(
        &mut self,
        name: ast::Name,
        full: Span,
        value: Option<&'a ast::Expr>,
        exported: bool,
    ) -> Self::Sym {
        let _ = (name, full, value, exported);
        Self::NOTHING
    }

    // ---- locals -------------------------------------------------------------------------------

    /// A parameter with a name.
    fn param(&mut self, name: ast::Name, param: &'a ast::Param) -> Self::Sym {
        let _ = (name, param);
        Self::NOTHING
    }

    /// A rest parameter (`...name`), with its type annotation.
    fn rest_param(&mut self, name: ast::Name, ty: Option<&'a ast::Type>) -> Self::Sym {
        let _ = (name, ty);
        Self::NOTHING
    }

    /// A name bound by a pattern: of a `let` (see [`Local`]), a `match` arm or a `for` loop.
    fn local(&mut self, name: ast::Name, local: Local<'a>) -> Self::Sym {
        let _ = (name, local);
        Self::NOTHING
    }

    /// `target = value`, `(a, target) = value` or `target op= value`, where `target` is a
    /// name.
    fn assigned(&mut self, target: Self::Sym, assign: Assign<'a>) {
        let _ = (target, assign);
    }

    // ---- uses ---------------------------------------------------------------------------------

    /// The name at `span` refers to `sym`.
    fn reference(&mut self, span: Span, sym: Self::Sym) {
        let _ = (span, sym);
    }

    /// What the prelude name `name` (as the module spells it) is.
    fn prelude(&mut self, name: &[u8]) -> Self::Sym {
        let _ = name;
        Self::NOTHING
    }

    /// What `base::name` is, where `base` is `base` (`self_base` when it is written `Self`):
    /// an item of a module, a variant or an associated item. The observer notes the
    /// reference itself.
    fn member(&mut self, base: Self::Sym, self_base: bool, name: ast::Name) -> Self::Sym {
        let _ = (base, self_base, name);
        Self::NOTHING
    }

    /// The field `name` is used in a literal or a pattern of `owner` (a struct or a variant).
    fn field(&mut self, owner: Self::Sym, name: ast::Name) {
        let _ = (owner, name);
    }

    /// A type annotation. Lowering doesn't read types (but for `as`): the observer does, and
    /// asks `names` what each name in it is.
    fn ty(&mut self, ty: &'a ast::Type, names: &dyn TypeNames<Self::Sym>) {
        let _ = (ty, names);
    }
}

/// How a pattern's name is bound.
#[derive(Clone, Copy, Debug)]
pub struct Local<'a> {
    /// The pattern that binds it (the name itself for a shorthand field).
    pub full: Span,
    /// For a `let`: where the statement ends, which is where the name comes into scope.
    pub let_end: Option<u32>,
    /// For a plain `let name` or `let (a, name)`: what gives the name its type.
    pub def: Option<LetDef<'a>>,
}

/// What `let name: ty = init;` says about `name`.
#[derive(Clone, Copy, Debug)]
pub struct LetDef<'a> {
    pub ty: Option<&'a ast::Type>,
    pub init: Option<&'a ast::Expr>,
    pub mutable: bool,
    /// The position in `let (a, b) = ...`.
    pub slot: Option<usize>,
}

/// A value assigned to a name after its definition.
#[derive(Clone, Copy, Debug)]
pub struct Assign<'a> {
    pub value: &'a ast::Expr,
    /// The position in `(a, b) = ...`.
    pub slot: Option<usize>,
    /// For `x op= value`, the operator.
    pub op: Option<ast::BinOp>,
}

/// Lowering's scopes, as far as a type annotation needs them.
pub trait TypeNames<S> {
    /// What `name` is where the annotation stands: a generic parameter, a type alias, or
    /// whatever the name is bound to as a value (a struct, an enum, a module). `None` if
    /// nothing binds it.
    fn lookup(&self, name: StrId) -> Option<S>;

    /// What `Self` is: the target of the enclosing `impl` block.
    fn self_type(&self) -> Option<S>;
}

/// The compiler's observer: nothing is observed.
#[derive(Debug, Default)]
pub struct Nothing;

impl<'a> Observer<'a> for Nothing {
    type Sym = ();
    const NOTHING: () = ();
    const ACTIVE: bool = false;
}
