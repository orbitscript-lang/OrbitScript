use orbit_hir::*;

const SP: Span = Span::new(0, 0);

fn push(f: &mut Function, kind: StmtKind) {
    let s = f.add_stmt(kind, SP);
    let body = f.body;
    f.blocks[body.index()].stmts.push(s);
}

/// local x = 1
/// f = function() local function g() x = x + 1; print(x) end end
fn three_levels() -> Chunk {
    let mut chunk = Chunk::new(SourceInfo::new("t.lua", Language::Lua, b""));
    let x_name = chunk.strings.intern(b"x");
    let g_name = chunk.strings.intern(b"g");
    let f_name = chunk.strings.intern(b"f");
    let print = chunk.strings.intern(b"print");

    let mut main = Function::new(FunctionName::Main, SP, None);
    main.is_vararg = true;
    let x = main.add_local(Local::new(Some(x_name), SP, LocalKind::Normal));
    let one = main.add_expr(ExprKind::Int(1), SP);
    push(
        &mut main,
        StmtKind::Let {
            locals: vec![x],
            values: vec![one].into(),
        },
    );
    let clo = main.add_expr(ExprKind::Closure(FuncId(1)), SP);
    push(
        &mut main,
        StmtKind::Assign {
            targets: vec![Place::Global(f_name)],
            values: vec![clo].into(),
        },
    );

    let mut f = Function::new(FunctionName::Global(f_name), SP, Some(FuncId(0)));
    let g = f.add_local(Local::new(Some(g_name), SP, LocalKind::Normal));
    push(
        &mut f,
        StmtKind::LocalFunction {
            local: g,
            func: FuncId(2),
        },
    );

    let mut gf = Function::new(FunctionName::Local(g_name), SP, Some(FuncId(1)));
    let xr = gf.add_expr(
        ExprKind::Outer {
            func: FuncId(0),
            local: x,
        },
        SP,
    );
    let one = gf.add_expr(ExprKind::Int(1), SP);
    let sum = gf.add_expr(
        ExprKind::Binary {
            op: BinOp::Add,
            lhs: xr,
            rhs: one,
        },
        SP,
    );
    push(
        &mut gf,
        StmtKind::Assign {
            targets: vec![Place::Outer {
                func: FuncId(0),
                local: x,
            }],
            values: vec![sum].into(),
        },
    );
    let p = gf.add_expr(ExprKind::Global(print), SP);
    let xr = gf.add_expr(
        ExprKind::Outer {
            func: FuncId(0),
            local: x,
        },
        SP,
    );
    let call = gf.add_expr(
        ExprKind::Call {
            callee: p,
            args: vec![xr].into(),
        },
        SP,
    );
    push(&mut gf, StmtKind::Eval(call));

    chunk.add_function(main);
    chunk.add_function(f);
    chunk.add_function(gf);
    chunk
}

#[test]
fn captures_thread_through_every_level() {
    let mut chunk = three_levels();
    verify_lowered(&chunk).unwrap();
    assert!(verify(&chunk).is_err());
    analyze(&mut chunk).unwrap();
    verify(&chunk).unwrap();

    let main = chunk.function(FuncId(0));
    assert_eq!(
        main.captures.iter().map(|c| c.from).collect::<Vec<_>>(),
        vec![CaptureFrom::Env]
    );
    assert!(main.locals[0].captured);
    assert!(main.locals[0].reassigned);

    let f = chunk.function(FuncId(1));
    assert_eq!(
        f.captures.iter().map(|c| c.from).collect::<Vec<_>>(),
        vec![
            CaptureFrom::ParentCapture(CaptureId(0)),
            CaptureFrom::ParentLocal(LocalId(0))
        ]
    );
    assert!(!f.locals[0].captured, "g is not captured by anything");

    let g = chunk.function(FuncId(2));
    assert_eq!(
        g.captures.iter().map(|c| c.from).collect::<Vec<_>>(),
        vec![
            CaptureFrom::ParentCapture(CaptureId(0)),
            CaptureFrom::ParentCapture(CaptureId(1))
        ]
    );
    assert_eq!(chunk.strings.get(g.captures[1].name), b"x");
    // Both reads and the write resolve to the same capture.
    assert!(matches!(g.exprs[0].kind, ExprKind::Capture(CaptureId(1))));
    assert!(matches!(g.exprs[4].kind, ExprKind::Capture(CaptureId(1))));
    match &g.stmts[0].kind {
        StmtKind::Assign { targets, .. } => assert_eq!(targets[0], Place::Capture(CaptureId(1))),
        _ => panic!(),
    }

    let dump = print(&chunk);
    assert!(dump.contains("capture c1 \"x\" <- parent c1"), "{dump}");
    assert!(dump.contains("c1 = (c1 + 1)"), "{dump}");
}

#[test]
fn env_is_capture_zero_even_when_first_used_late() {
    // function() local y; return function() return y, z end end: the inner function references
    // `y` before the global `z`, but the environment still gets capture 0.
    let mut chunk = Chunk::new(SourceInfo::new("t.lua", Language::Lua, b""));
    let y_name = chunk.strings.intern(b"y");
    let z = chunk.strings.intern(b"z");
    let mut main = Function::new(FunctionName::Main, SP, None);
    main.is_vararg = true;
    let y = main.add_local(Local::new(Some(y_name), SP, LocalKind::Normal));
    push(
        &mut main,
        StmtKind::Let {
            locals: vec![y],
            values: ExprList::default(),
        },
    );
    let clo = main.add_expr(ExprKind::Closure(FuncId(1)), SP);
    push(&mut main, StmtKind::Return(vec![clo].into()));
    let mut inner = Function::new(FunctionName::Anonymous, SP, Some(FuncId(0)));
    let a = inner.add_expr(
        ExprKind::Outer {
            func: FuncId(0),
            local: y,
        },
        SP,
    );
    let b = inner.add_expr(ExprKind::Global(z), SP);
    push(&mut inner, StmtKind::Return(vec![a, b].into()));
    chunk.add_function(main);
    chunk.add_function(inner);

    analyze(&mut chunk).unwrap();
    verify(&chunk).unwrap();
    let inner = chunk.function(FuncId(1));
    assert_eq!(
        inner.captures[0].from,
        CaptureFrom::ParentCapture(CaptureId(0))
    );
    assert_eq!(inner.captures[1].from, CaptureFrom::ParentLocal(y));
    assert!(!chunk.function(FuncId(0)).locals[0].reassigned);
}

#[test]
fn functions_without_globals_get_no_env() {
    let mut chunk = Chunk::new(SourceInfo::new("t.lua", Language::Lua, b""));
    let mut main = Function::new(FunctionName::Main, SP, None);
    main.is_vararg = true;
    let one = main.add_expr(ExprKind::Int(1), SP);
    push(&mut main, StmtKind::Return(vec![one].into()));
    chunk.add_function(main);
    analyze(&mut chunk).unwrap();
    verify(&chunk).unwrap();
    assert!(chunk.function(FuncId(0)).captures.is_empty());
}

#[test]
fn verifier_rejects_bad_shapes() {
    let mut chunk = Chunk::new(SourceInfo::new("t.lua", Language::Lua, b""));
    let mut main = Function::new(FunctionName::Main, SP, None);
    push(&mut main, StmtKind::Break);
    chunk.add_function(main);
    let err = verify(&chunk).unwrap_err();
    assert_eq!(err.message, "break outside a loop");

    let mut chunk = Chunk::new(SourceInfo::new("t.lua", Language::Lua, b""));
    let mut main = Function::new(FunctionName::Main, SP, None);
    let e = main.add_expr(ExprKind::Int(1), SP);
    push(&mut main, StmtKind::Eval(e));
    push(&mut main, StmtKind::Eval(e));
    chunk.add_function(main);
    assert_eq!(
        verify(&chunk).unwrap_err().message,
        "expression 0 used twice"
    );

    let mut chunk = Chunk::new(SourceInfo::new("t.lua", Language::Lua, b""));
    let mut main = Function::new(FunctionName::Main, SP, None);
    let v = main.add_expr(ExprKind::Vararg, SP);
    push(&mut main, StmtKind::Return(vec![v].into()));
    main.add_local(Local::new(None, SP, LocalKind::Normal));
    chunk.add_function(main);
    assert_eq!(
        verify(&chunk).unwrap_err().message,
        "'...' in a function that isn't vararg"
    );
}

#[test]
fn outer_must_name_an_enclosing_function() {
    let mut chunk = three_levels();
    // Point g's first reference at a local of itself instead of an enclosing function.
    chunk.functions[2].exprs[0].kind = ExprKind::Outer {
        func: FuncId(2),
        local: LocalId(0),
    };
    assert!(verify_lowered(&chunk).is_err());
    assert!(analyze(&mut chunk).is_err());
}
