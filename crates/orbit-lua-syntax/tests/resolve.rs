//! Name resolution, upvalues, compile-time constants and line information.

use orbit_lua_syntax::ast::*;
use orbit_lua_syntax::{parse, skip_file_prefix};

fn chunk(src: &str) -> Chunk {
    parse(src.as_bytes()).unwrap_or_else(|e| panic!("{src}: {e}"))
}

/// The `return` values of the main function.
fn returns(c: &Chunk) -> &[Expr] {
    &c.body.block.ret.as_ref().expect("return").values
}

fn binding(e: &Expr) -> Binding {
    match &e.kind {
        ExprKind::Name(n) => n.binding,
        other => panic!("not a name: {other:?}"),
    }
}

fn func(e: &Expr) -> &FuncBody {
    match &e.kind {
        ExprKind::Function(f) => f,
        other => panic!("not a function: {other:?}"),
    }
}

fn local_named(c: &Chunk, name: &str) -> LocalId {
    let i = c.locals.iter().position(|l| &*l.name == name).expect(name);
    LocalId(i as u32)
}

#[test]
fn locals_globals_and_env() {
    let c = chunk("local a = 1; return a, b, _ENV");
    let a = local_named(&c, "a");
    let r = returns(&c);
    assert_eq!(binding(&r[0]), Binding::Local(a));
    assert_eq!(binding(&r[1]), Binding::Global(EnvRef::Upvalue(0)));
    assert_eq!(binding(&r[2]), Binding::Upvalue(0));
    assert_eq!(c.body.upvalues[0].name.as_ref(), "_ENV");
    assert_eq!(c.body.upvalues[0].source, UpvalSource::ChunkEnv);
    assert!(!c.local(a).captured);

    let c = chunk("local _ENV = {}; return x");
    let env = local_named(&c, "_ENV");
    assert_eq!(
        binding(&returns(&c)[0]),
        Binding::Global(EnvRef::Local(env))
    );
}

#[test]
fn upvalues_in_creation_order() {
    let c = chunk(
        "local a, b = 1, 2
         return function()
           return function() return b, x, a end
         end",
    );
    let (a, b) = (local_named(&c, "a"), local_named(&c, "b"));
    let outer = func(&returns(&c)[0]);
    let inner = func(&outer.block.ret.as_ref().unwrap().values[0]);
    // Upvalues are created as names are met: b, then _ENV (for x), then a.
    let names: Vec<&str> = inner.upvalues.iter().map(|u| &*u.name).collect();
    assert_eq!(names, ["b", "_ENV", "a"]);
    let srcs: Vec<UpvalSource> = outer.upvalues.iter().map(|u| u.source).collect();
    assert_eq!(
        srcs,
        [
            UpvalSource::Local(b),
            UpvalSource::Upvalue(0),
            UpvalSource::Local(a)
        ]
    );
    let inner_srcs: Vec<UpvalSource> = inner.upvalues.iter().map(|u| u.source).collect();
    assert_eq!(
        inner_srcs,
        [
            UpvalSource::Upvalue(0),
            UpvalSource::Upvalue(1),
            UpvalSource::Upvalue(2)
        ]
    );
    let r = &inner.block.ret.as_ref().unwrap().values;
    assert_eq!(binding(&r[0]), Binding::Upvalue(0));
    assert_eq!(binding(&r[1]), Binding::Global(EnvRef::Upvalue(1)));
    assert_eq!(binding(&r[2]), Binding::Upvalue(2));
    assert!(c.local(a).captured && c.local(b).captured);
}

#[test]
fn compile_time_constants() {
    let c = chunk(
        "local k <const> = 10
         local s <const> = 'x'
         local t <const> = {}
         local n <const> = k * 2 + 1
         local z <const> = 1 - 1.0
         local j <const> = x and false or 3
         local a <const>, b <const> = 1, 2
         return function() return k, t, n end",
    );
    let kind = |name| &c.local(local_named(&c, name)).kind;
    assert_eq!(kind("k"), &LocalKind::CompileTimeConst(Const::Int(10)));
    assert_eq!(
        kind("s"),
        &LocalKind::CompileTimeConst(Const::Str(b"x".as_slice().into()))
    );
    assert_eq!(kind("t"), &LocalKind::Const);
    assert_eq!(kind("n"), &LocalKind::CompileTimeConst(Const::Int(21)));
    // Float zero results are not folded, to keep -0.0 intact.
    assert_eq!(kind("z"), &LocalKind::Const);
    // `x and false` jumps past `3` only when `x` is false, so the value is always 3.
    assert_eq!(kind("j"), &LocalKind::CompileTimeConst(Const::Int(3)));
    // Only the last variable of a list can become a compile-time constant.
    assert_eq!(kind("a"), &LocalKind::Const);
    assert_eq!(kind("b"), &LocalKind::CompileTimeConst(Const::Int(2)));

    // Constants are not upvalues: the closure only captures `t`.
    let f = func(&returns(&c)[0]);
    let names: Vec<&str> = f.upvalues.iter().map(|u| &*u.name).collect();
    assert_eq!(names, ["t"]);
    let r = &f.block.ret.as_ref().unwrap().values;
    assert_eq!(binding(&r[0]), Binding::Const(local_named(&c, "k")));
    assert_eq!(binding(&r[2]), Binding::Const(local_named(&c, "n")));
}

#[test]
fn global_declarations() {
    let c = chunk(
        "global <const> print
         global x = 1
         global *
         y = print(x)",
    );
    let StatKind::Global { vars, values, env } = &c.body.block.stats[1].kind else {
        panic!()
    };
    assert_eq!(&*vars[0].name.name, "x");
    assert_eq!(values.len(), 1);
    assert_eq!(*env, Some(EnvRef::Upvalue(0)));
    assert!(matches!(
        c.body.block.stats[2].kind,
        StatKind::GlobalAll { is_const: false }
    ));
    let StatKind::Assign { targets, .. } = &c.body.block.stats[3].kind else {
        panic!()
    };
    assert_eq!(binding(&targets[0]), Binding::Global(EnvRef::Upvalue(0)));
}

#[test]
fn methods_and_varargs() {
    let c = chunk("function t.a.b:m(x, ...rest) return self, rest, ... end");
    let StatKind::Function { name, body } = &c.body.block.stats[0].kind else {
        panic!()
    };
    assert_eq!(name.fields.len(), 2);
    assert_eq!(&*name.method.as_ref().unwrap().name, "m");
    assert!(body.is_method);
    assert_eq!(body.params.len(), 2);
    assert_eq!(&*body.params[0].name.name, "self");
    let rest = body.vararg.as_ref().unwrap().name.as_ref().unwrap().id;
    assert_eq!(c.local(rest).kind, LocalKind::VarargTable);
    let r = &body.block.ret.as_ref().unwrap().values;
    assert_eq!(binding(&r[0]), Binding::Local(body.params[0].id));
    assert_eq!(binding(&r[1]), Binding::Local(rest));
    assert!(matches!(r[2].kind, ExprKind::Vararg));
}

#[test]
fn for_control_variables_are_read_only() {
    let c = chunk("for i = 1, 2 do end for k, v in next, {} do end");
    assert_eq!(c.local(local_named(&c, "i")).kind, LocalKind::ForControl);
    assert_eq!(c.local(local_named(&c, "k")).kind, LocalKind::ForControl);
    assert_eq!(c.local(local_named(&c, "v")).kind, LocalKind::Regular);
}

#[test]
fn precedence_and_associativity() {
    let c = chunk("return 1 + 2 * 3 ^ -4 ^ 5 .. 'a' .. 'b', not a == b, -x ^ 2");
    let r = returns(&c);
    // 1 + (2 * (3 ^ (-(4 ^ 5)))) .. ('a' .. 'b'), with `..` binding looser than `+`.
    let ExprKind::Binary { op, lhs, rhs, .. } = &r[0].kind else {
        panic!()
    };
    assert_eq!(*op, BinOp::Concat);
    assert!(matches!(
        rhs.kind,
        ExprKind::Binary {
            op: BinOp::Concat,
            ..
        }
    ));
    let ExprKind::Binary {
        op: BinOp::Add,
        rhs: mul,
        ..
    } = &lhs.kind
    else {
        panic!()
    };
    let ExprKind::Binary {
        op: BinOp::Mul,
        rhs: pow,
        ..
    } = &mul.kind
    else {
        panic!()
    };
    let ExprKind::Binary {
        op: BinOp::Pow,
        rhs: neg,
        ..
    } = &pow.kind
    else {
        panic!()
    };
    assert!(matches!(neg.kind, ExprKind::Unary { op: UnOp::Neg, .. }));
    // `not` binds tighter than `==`.
    assert!(matches!(r[1].kind, ExprKind::Binary { op: BinOp::Eq, .. }));
    // `^` binds tighter than unary minus.
    assert!(matches!(r[2].kind, ExprKind::Unary { op: UnOp::Neg, .. }));
}

#[test]
fn line_information() {
    let c = chunk(
        "local function f(a,
                          b)
           return a
             + b
         end
         f
         (1)
         local g = function
           () end",
    );
    let StatKind::LocalFunction { body, .. } = &c.body.block.stats[0].kind else {
        panic!()
    };
    assert_eq!((body.line_defined, body.last_line_defined), (1, 5));
    let ret = &body.block.ret.as_ref().unwrap().values[0];
    let ExprKind::Binary { line, .. } = ret.kind else {
        panic!()
    };
    assert_eq!(line, 4, "an operator's line is where the operator is");
    // `f\n(1)` is one call statement, whose line is where the arguments open.
    let StatKind::Call(call) = &c.body.block.stats[1].kind else {
        panic!()
    };
    let ExprKind::Call(call) = &call.kind else {
        panic!()
    };
    assert_eq!(call.line, 7);
    assert_eq!(c.body.block.stats[1].span.line, 6);
    let StatKind::Local { values, .. } = &c.body.block.stats[2].kind else {
        panic!()
    };
    // An anonymous function is defined on the line after `function`.
    assert_eq!(func(&values[0]).line_defined, 9);
}

#[test]
fn file_prefix() {
    assert_eq!(skip_file_prefix(b"#!/usr/bin/lua\nx = 1"), b"\nx = 1");
    assert_eq!(skip_file_prefix(b"\xEF\xBB\xBFx = 1"), b"x = 1");
    assert_eq!(skip_file_prefix(b"\xEF\xBB\xBF# c\nx"), b"\nx");
    assert_eq!(skip_file_prefix(b"x = 1"), b"x = 1");
}
