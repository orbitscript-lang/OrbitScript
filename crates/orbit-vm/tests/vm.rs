//! Interpreter tests on hand-assembled chunks, so the VM is tested without
//! the compiler. End-to-end programs live in `tests/first-light`.

use orbit_bytecode::{
    CaptureSource, Constant, DebugInfo, FunctionName, Op, UnlinkedChunk, UnlinkedProto,
};
use orbit_core::Value;
use orbit_vm::{CallOutcome, LuaError, NativeCtx, NativeReturn, Vm};

fn proto(nregs: u8, code: Vec<Op>) -> UnlinkedProto {
    UnlinkedProto {
        name: FunctionName::Main,
        line_defined: 0,
        last_line: 1,
        nparams: 0,
        is_vararg: true,
        nregs,
        ncells: 0,
        ncaches: 0,
        code,
        consts: Vec::new(),
        children: Vec::new(),
        captures: vec![CaptureSource::Env],
        self_capture: None,
        debug: DebugInfo {
            lines: vec![(0, 1)],
            ..DebugInfo::default()
        },
    }
}

fn function(nparams: u8, nregs: u8, code: Vec<Op>) -> UnlinkedProto {
    UnlinkedProto {
        name: FunctionName::Local(b"f".as_slice().into()),
        nparams,
        is_vararg: false,
        captures: Vec::new(),
        ..proto(nregs, code)
    }
}

fn run(vm: &mut Vm, protos: Vec<UnlinkedProto>) -> Result<Vec<Value>, LuaError> {
    let chunk = UnlinkedChunk {
        name: "t".into(),
        source: Box::default(),
        protos,
    };
    let main = vm.load(&chunk);
    vm.call(main, &[])
}

#[test]
fn numeric_for_sums() {
    let mut vm = Vm::new();
    let code = vec![
        Op::LoadInt { dst: 0, value: 0 },
        Op::LoadInt { dst: 1, value: 1 },
        Op::LoadInt { dst: 2, value: 10 },
        Op::LoadInt { dst: 3, value: 1 },
        Op::ForPrep { base: 1, exit: 2 },
        Op::Add { dst: 0, a: 0, b: 4 },
        Op::ForLoop { base: 1, back: -2 },
        Op::Return1 { src: 0 },
    ];
    let r = run(&mut vm, vec![proto(5, code)]).unwrap();
    assert!(matches!(r[..], [Value::Int(55)]));
}

#[test]
fn closures_share_a_captured_cell() {
    let mut vm = Vm::new();
    let mut main = proto(
        3,
        vec![
            Op::LoadInt { dst: 0, value: 0 },
            Op::CellNew { cell: 0, src: 0 },
            Op::Closure { dst: 1, proto: 0 },
            Op::Move { dst: 2, src: 1 },
            Op::Call {
                func: 2,
                nargs: 0,
                nresults: 0,
            },
            Op::Move { dst: 2, src: 1 },
            Op::Call {
                func: 2,
                nargs: 0,
                nresults: 1,
            },
            Op::Return1 { src: 2 },
        ],
    );
    main.ncells = 1;
    main.children = vec![1];
    let mut counter = function(
        0,
        1,
        vec![
            Op::CapGet { dst: 0, cap: 0 },
            Op::AddInt {
                dst: 0,
                a: 0,
                imm: 1,
            },
            Op::CapSet { cap: 0, src: 0 },
            Op::Return1 { src: 0 },
        ],
    );
    counter.captures = vec![CaptureSource::Cell(0)];
    let r = run(&mut vm, vec![main, counter]).unwrap();
    assert!(matches!(r[..], [Value::Int(2)]));
}

fn apply(ctx: &mut NativeCtx<'_>) -> Result<NativeReturn, LuaError> {
    let f = ctx.check_any(0)?;
    Ok(NativeReturn::CallThen {
        f,
        nargs: ctx.nargs() - 1,
        k: apply_done,
        protected: false,
    })
}

fn apply_done(_: &mut NativeCtx<'_>, outcome: CallOutcome) -> Result<NativeReturn, LuaError> {
    match outcome {
        CallOutcome::Ok(n) => Ok(NativeReturn::Return(n)),
        CallOutcome::Err(_) => unreachable!("unprotected call"),
    }
}

fn try_(ctx: &mut NativeCtx<'_>) -> Result<NativeReturn, LuaError> {
    let f = ctx.check_any(0)?;
    Ok(NativeReturn::CallThen {
        f,
        nargs: ctx.nargs() - 1,
        k: try_done,
        protected: true,
    })
}

fn try_done(ctx: &mut NativeCtx<'_>, outcome: CallOutcome) -> Result<NativeReturn, LuaError> {
    let ok = matches!(outcome, CallOutcome::Ok(_));
    ctx.ret1(Value::Bool(ok))
}

/// `main` calls native `name(double, 21)`; `double` returns `x + x`, or
/// `nil + x` when `fail` is set.
fn native_calls_lua(
    name: &'static str,
    f: orbit_vm::NativeFn,
    fail: bool,
) -> Result<Value, String> {
    let mut vm = Vm::new();
    let v = vm.register_native(name, f);
    vm.set_global(name, v);
    let mut main = proto(
        3,
        vec![
            Op::GetGlobal {
                dst: 0,
                name: 0,
                ic: 0,
            },
            Op::Closure { dst: 1, proto: 0 },
            Op::LoadInt { dst: 2, value: 21 },
            Op::Call {
                func: 0,
                nargs: 2,
                nresults: 1,
            },
            Op::Return1 { src: 0 },
        ],
    );
    main.consts = vec![Constant::Str(name.as_bytes().into())];
    main.ncaches = 1;
    main.children = vec![1];
    let mut body = vec![Op::Add { dst: 0, a: 0, b: 0 }, Op::Return1 { src: 0 }];
    if fail {
        body.insert(0, Op::LoadNil { dst: 1, count: 1 });
        body[1] = Op::Add { dst: 0, a: 1, b: 0 };
    }
    let double = function(1, 2, body);
    let r = run(&mut vm, vec![main, double]).map_err(|e| vm.error_message(&e))?;
    // The VM stays usable whichever way the call ended.
    assert!(matches!(vm.global(name), Value::Native(_)));
    Ok(r[0])
}

#[test]
fn native_calls_back_into_lua() {
    let r = native_calls_lua("apply", apply, false).unwrap();
    assert!(matches!(r, Value::Int(42)));
}

#[test]
fn errors_carry_position_and_propagate_through_natives() {
    let msg = native_calls_lua("apply", apply, true).unwrap_err();
    assert_eq!(msg, "t:1: attempt to perform arithmetic on a nil value");
}

#[test]
fn protected_native_calls_catch_errors() {
    let r = native_calls_lua("try", try_, true).unwrap();
    assert!(matches!(r, Value::Bool(false)));
    let r = native_calls_lua("try", try_, false).unwrap();
    assert!(matches!(r, Value::Bool(true)));
}

#[test]
fn tables_survive_a_stressed_collector() {
    let mut vm = Vm::new();
    vm.heap.set_stress(true);
    // local list = nil; for i = 1, 2000 do list = { next = list } end
    // return list.next.next
    let mut main = proto(
        6,
        vec![
            Op::LoadNil { dst: 0, count: 1 },
            Op::LoadInt { dst: 1, value: 1 },
            Op::LoadInt {
                dst: 2,
                value: 2000,
            },
            Op::LoadInt { dst: 3, value: 1 },
            Op::ForPrep { base: 1, exit: 4 },
            Op::NewTable {
                dst: 5,
                array_hint: 0,
                hash_hint: 1,
            },
            Op::SetField {
                obj: 5,
                key: 0,
                src: 0,
                ic: 0,
            },
            Op::Move { dst: 0, src: 5 },
            Op::ForLoop { base: 1, back: -4 },
            Op::GetField {
                dst: 0,
                obj: 0,
                key: 0,
                ic: 0,
            },
            Op::GetField {
                dst: 0,
                obj: 0,
                key: 0,
                ic: 0,
            },
            Op::Return1 { src: 0 },
        ],
    );
    main.consts = vec![Constant::Str(b"next".as_slice().into())];
    main.ncaches = 1;
    let r = run(&mut vm, vec![main]).unwrap();
    assert!(matches!(r[..], [Value::Table(_)]));
    vm.full_collect();
}
