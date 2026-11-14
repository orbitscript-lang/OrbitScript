//! What `e as T` can name: the one table of cast targets, read by lowering, which compiles the
//! cast, and by the type checker, which says when a cast can never succeed.
//!
//! A cast runs while the program does, so its target must be something the runtime can test: a
//! scalar type, a struct, enum or variant, or `any`. Structural types, type parameters and
//! aliases can't be tested then, and are rejected ([`Rejected`], T0210).

use orbit_hir::{CastKind, StringPool};

use crate::ast::{self, TypeKind};

/// What a name written alone as a target means where it is written. The caller knows; the
/// table asks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NameKind {
    /// A type parameter in scope.
    TypeParam,
    /// A type alias.
    Alias,
    /// A struct or enum, which may shadow a built-in type's name.
    Type,
    /// Anything else, a built-in type's name among them.
    Other,
}

/// A type the runtime converts to, or tests for, by the value's own type.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scalar {
    /// An integer, or a float with an exact integer value.
    Int,
    /// A number, as a float.
    Float,
    /// An integer or a float.
    Number,
    /// A string; a number or boolean as `tostring` writes it; a value with `__tostring`.
    String,
    /// Truthiness; in a union, a boolean.
    Bool,
    Nil,
}

impl Scalar {
    /// The cast the HIR has for it.
    pub fn kind(self) -> CastKind {
        match self {
            Scalar::Int => CastKind::Int,
            Scalar::Float => CastKind::Float,
            Scalar::Number => CastKind::Number,
            Scalar::String => CastKind::String,
            Scalar::Bool => CastKind::Bool,
            Scalar::Nil => CastKind::Nil,
        }
    }
}

/// What a target that isn't a union or an optional is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    /// `any`: the cast does nothing.
    Any,
    Scalar(Scalar),
    /// A path or `Self`, for a struct, enum or variant: tests the value's metatable.
    Named,
}

/// Why a target can't be cast to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rejected {
    /// Nothing the runtime can test: a structural type, a type parameter, `table`.
    Uncheckable,
    /// A type alias, by this name.
    Alias(ast::Name),
}

impl Rejected {
    /// The T0210 message for the target `ty`.
    pub fn message(self, ty: &ast::Type, strings: &StringPool) -> String {
        match self {
            Rejected::Uncheckable => format!(
                "`as` can't check `{}` while the program runs; give the value a type annotation instead",
                type_text(ty, strings)
            ),
            Rejected::Alias(name) => format!(
                "`as` can't use the type alias `{}`; name the type it stands for",
                strings.display(name.sym)
            ),
        }
    }
}

/// Classifies the target `kind` (not a union or an optional: see [`strip_optional`]).
/// `name_kind` is asked about a target that is one name with no type arguments.
pub fn target(
    kind: &TypeKind,
    strings: &StringPool,
    name_kind: impl FnOnce(ast::Name) -> NameKind,
) -> Result<Target, Rejected> {
    match kind {
        TypeKind::Nil => Ok(Target::Scalar(Scalar::Nil)),
        TypeKind::SelfType => Ok(Target::Named),
        // The parser has reported it.
        TypeKind::Error => Ok(Target::Any),
        TypeKind::Path { segments, args } if args.is_empty() => {
            let first = segments[0];
            if segments.len() > 1 {
                return Ok(Target::Named);
            }
            match name_kind(first) {
                NameKind::TypeParam => Err(Rejected::Uncheckable),
                NameKind::Alias => Err(Rejected::Alias(first)),
                NameKind::Type => Ok(Target::Named),
                NameKind::Other => match strings.get(first.sym) {
                    b"int" => Ok(Target::Scalar(Scalar::Int)),
                    b"float" => Ok(Target::Scalar(Scalar::Float)),
                    b"number" => Ok(Target::Scalar(Scalar::Number)),
                    b"string" => Ok(Target::Scalar(Scalar::String)),
                    b"bool" => Ok(Target::Scalar(Scalar::Bool)),
                    b"any" => Ok(Target::Any),
                    // Built-in type names with no cast rule.
                    b"table" | b"function" | b"thread" | b"userdata" => Err(Rejected::Uncheckable),
                    _ => Ok(Target::Named),
                },
            }
        }
        _ => Err(Rejected::Uncheckable),
    }
}

/// `T??` is `T` with `nil` allowed.
pub fn strip_optional(mut ty: &ast::Type) -> (&ast::Type, bool) {
    let mut nilable = false;
    while let Some(TypeKind::Optional(inner)) = ty.kind.as_deref() {
        ty = inner;
        nilable = true;
    }
    (ty, nilable)
}

/// A type as messages write it: `[int]`, `Shape::Circle`, `int | string`.
pub fn type_text(ty: &ast::Type, strings: &StringPool) -> String {
    let mut s = String::new();
    write_type(ty, strings, &mut s);
    s
}

fn write_type(ty: &ast::Type, strings: &StringPool, s: &mut String) {
    let list = |s: &mut String, items: &[ast::Type]| {
        for (i, t) in items.iter().enumerate() {
            if i > 0 {
                s.push_str(", ");
            }
            write_type(t, strings, s);
        }
    };
    let Some(kind) = ty.kind.as_deref() else {
        s.push('?');
        return;
    };
    match kind {
        TypeKind::Path { segments, args } => {
            for (i, seg) in segments.iter().enumerate() {
                if i > 0 {
                    s.push_str("::");
                }
                s.push_str(&strings.display(seg.sym));
            }
            if !args.is_empty() {
                s.push('<');
                list(s, args);
                s.push('>');
            }
        }
        TypeKind::Nil => s.push_str("nil"),
        TypeKind::SelfType => s.push_str("Self"),
        TypeKind::Optional(inner) => {
            let group = matches!(
                inner.kind.as_deref(),
                Some(TypeKind::Union(_) | TypeKind::Fn { .. })
            );
            if group {
                s.push('(');
            }
            write_type(inner, strings, s);
            if group {
                s.push(')');
            }
            s.push('?');
        }
        TypeKind::Union(members) => {
            for (i, t) in members.iter().enumerate() {
                if i > 0 {
                    s.push_str(" | ");
                }
                write_type(t, strings, s);
            }
        }
        TypeKind::Array(t) => {
            s.push('[');
            write_type(t, strings, s);
            s.push(']');
        }
        TypeKind::Map { key, value } => {
            s.push_str("#{[");
            write_type(key, strings, s);
            s.push_str("]: ");
            write_type(value, strings, s);
            s.push('}');
        }
        TypeKind::Record(fields) => {
            s.push_str("#{");
            for (i, f) in fields.iter().enumerate() {
                s.push_str(if i > 0 { ", " } else { " " });
                s.push_str(&strings.display(f.name.sym));
                s.push_str(": ");
                write_type(&f.ty, strings, s);
            }
            s.push_str(if fields.is_empty() { "}" } else { " }" });
        }
        TypeKind::Tuple(items) => {
            s.push('(');
            list(s, items);
            if items.len() == 1 {
                s.push(',');
            }
            s.push(')');
        }
        TypeKind::Fn { params, rest, ret } => {
            s.push_str("fn(");
            list(s, params);
            if let Some(r) = rest {
                if !params.is_empty() {
                    s.push_str(", ");
                }
                s.push_str("...");
                write_type(r, strings, s);
            }
            s.push(')');
            if let Some(r) = ret {
                s.push_str(" -> ");
                write_type(r, strings, s);
            }
        }
        TypeKind::Rest(t) => {
            s.push_str("...");
            write_type(t, strings, s);
        }
        TypeKind::Error => s.push('?'),
    }
}
