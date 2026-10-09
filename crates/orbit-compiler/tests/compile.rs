//! Compiler tests on HIR built by hand (the front end isn't needed).

use orbit_bytecode::{CaptureSource, MULTI, Op, Origin};
use orbit_compiler::{compile, disassemble};
use orbit_hir::*;

const SP: Span = Span::new(0, 0);

/// A tiny HIR builder: one function under construction at a time.
struct B {
    chunk: Chunk,
    stack: Vec<(FuncId, Function)>,
    blocks: Vec<Vec<StmtId>>,
}

impl B {
    fn new() -> Self {
        let chunk = Chunk::new(SourceInfo::new(
            "t.lua",
            Language::Lua,
            b"line1\nline2\nline3\n",
        ));
        let mut main = Function::new(FunctionName::Main, SP, None);
        main.is_vararg = true;
        let mut b = B {
            chunk,
            stack: Vec::new(),
            blocks: vec![Vec::new()],
        };
        b.chunk
            .add_function(Function::new(FunctionName::Main, SP, None)); // placeholder
        b.stack.push((FuncId(0), main));
        b
    }
    fn f(&mut self) -> &mut Function {
        &mut self.stack.last_mut().unwrap().1
    }
    fn s(&mut self, s: &str) -> StrId {
        self.chunk.strings.intern(s.as_bytes())
    }
    fn e(&mut self, kind: ExprKind) -> ExprId {
        self.f().add_expr(kind, SP)
    }
    fn int(&mut self, i: i64) -> ExprId {
        self.e(ExprKind::Int(i))
    }
    fn str(&mut self, s: &str) -> ExprId {
        let s = self.s(s);
        self.e(ExprKind::Str(s))
    }
    fn global(&mut self, name: &str) -> ExprId {
        let s = self.s(name);
        self.e(ExprKind::Global(s))
    }
    fn local(&mut self, l: LocalId) -> ExprId {
        self.e(ExprKind::Local(l))
    }
    fn bin(&mut self, op: BinOp, lhs: ExprId, rhs: ExprId) -> ExprId {
        self.e(ExprKind::Binary { op, lhs, rhs })
    }
    fn call(&mut self, callee: ExprId, args: Vec<ExprId>) -> ExprId {
        self.e(ExprKind::Call {
            callee,
            args: args.into(),
        })
    }
    fn field(&mut self, obj: ExprId, name: &str) -> ExprId {
        let key = self.str(name);
        self.e(ExprKind::Index { obj, key })
    }
    fn new_local(&mut self, name: &str, kind: LocalKind) -> LocalId {
        let n = self.s(name);
        self.f().add_local(Local::new(Some(n), SP, kind))
    }
    fn stmt(&mut self, kind: StmtKind) {
        let s = self.f().add_stmt(kind, SP);
        self.blocks.last_mut().unwrap().push(s);
    }
    fn let_(&mut self, name: &str, value: Option<ExprId>) -> LocalId {
        let l = self.new_local(name, LocalKind::Normal);
        self.stmt(StmtKind::Let {
            locals: vec![l],
            values: value.into_iter().collect::<Vec<_>>().into(),
        });
        l
    }
    fn open(&mut self) {
        self.blocks.push(Vec::new());
    }
    fn close(&mut self) -> BlockId {
        let stmts = self.blocks.pop().unwrap();
        self.f().add_block(Block { stmts, span: SP })
    }
    /// Starts a nested function; returns its id.
    fn begin_fn(&mut self, params: &[&str]) -> (FuncId, Vec<LocalId>) {
        let id = FuncId::from_index(self.chunk.functions.len());
        self.chunk
            .add_function(Function::new(FunctionName::Anonymous, SP, None)); // placeholder
        let parent = self.stack.last().unwrap().0;
        let mut f = Function::new(FunctionName::Anonymous, SP, Some(parent));
        let ps = params
            .iter()
            .map(|p| {
                let n = self.chunk.strings.intern(p.as_bytes());
                f.add_param(Some(n), SP)
            })
            .collect();
        self.stack.push((id, f));
        self.blocks.push(Vec::new());
        (id, ps)
    }
    fn end_fn(&mut self) {
        let stmts = self.blocks.pop().unwrap();
        let (id, mut f) = self.stack.pop().unwrap();
        let body = f.body;
        f.blocks[body.index()].stmts = stmts;
        self.chunk.functions[id.index()] = f;
    }
    fn finish(mut self) -> Chunk {
        let stmts = self.blocks.pop().unwrap();
        let (id, mut f) = self.stack.pop().unwrap();
        let body = f.body;
        f.blocks[body.index()].stmts = stmts;
        self.chunk.functions[id.index()] = f;
        analyze(&mut self.chunk).unwrap();
        verify(&self.chunk).unwrap();
        self.chunk
    }
}

fn code(chunk: &Chunk, proto: usize) -> Vec<Op> {
    compile(chunk).unwrap().protos[proto].code.clone()
}

#[test]
fn hello_world() {
    // print("hello")
    let mut b = B::new();
    let p = b.global("print");
    let s = b.str("hello");
    let c = b.call(p, vec![s]);
    b.stmt(StmtKind::Eval(c));
    let chunk = b.finish();
    let out = compile(&chunk).unwrap();
    let main = &out.protos[0];
    assert_eq!(
        main.code,
        vec![
            Op::GetGlobal {
                dst: 0,
                name: 0,
                ic: 0
            },
            Op::LoadConst { dst: 1, k: 1 },
            Op::Call {
                func: 0,
                nargs: 1,
                nresults: 0
            },
            Op::Return0,
        ]
    );
    assert_eq!(main.captures, vec![CaptureSource::Env]);
    assert_eq!(main.nregs, 2);
    assert_eq!(
        main.debug.origin(2, 0),
        Some(&Origin::Global(b"print".as_slice().into()))
    );
}

#[test]
fn locals_use_their_registers_directly() {
    // local a = 1; local b = a + 2; return a * b
    let mut b = B::new();
    let one = b.int(1);
    let a = b.let_("a", Some(one));
    let ar = b.local(a);
    let two = b.int(2);
    let sum = b.bin(BinOp::Add, ar, two);
    let bl = b.let_("b", Some(sum));
    let ar = b.local(a);
    let br = b.local(bl);
    let prod = b.bin(BinOp::Mul, ar, br);
    b.stmt(StmtKind::Return(vec![prod].into()));
    let chunk = b.finish();
    assert_eq!(
        code(&chunk, 0),
        vec![
            Op::LoadInt { dst: 0, value: 1 },
            Op::AddInt {
                dst: 1,
                a: 0,
                imm: 2
            },
            Op::Mul { dst: 2, a: 0, b: 1 },
            Op::Return1 { src: 2 },
        ]
    );
}

#[test]
fn constant_folding_and_const_locals() {
    // local k <const> = 2 * 3; return k + 1, 1 // 0, "x" .. k
    let mut b = B::new();
    let k = b.new_local("k", LocalKind::Const);
    let two = b.int(2);
    let three = b.int(3);
    let six = b.bin(BinOp::Mul, two, three);
    b.stmt(StmtKind::Let {
        locals: vec![k],
        values: vec![six].into(),
    });
    let kr = b.local(k);
    let one = b.int(1);
    let r1 = b.bin(BinOp::Add, kr, one);
    let one = b.int(1);
    let zero = b.int(0);
    let r2 = b.bin(BinOp::IDiv, one, zero);
    let x = b.str("x");
    let kr = b.local(k);
    let r3 = b.bin(BinOp::Concat, x, kr);
    b.stmt(StmtKind::Return(vec![r1, r2, r3].into()));
    let chunk = b.finish();
    let c = code(&chunk, 0);
    assert_eq!(c[0], Op::LoadInt { dst: 0, value: 6 }, "{c:?}");
    assert_eq!(c[1], Op::LoadInt { dst: 1, value: 7 }, "k + 1 folds");
    // 1 // 0 is a run-time error, so it is not folded.
    assert!(c.iter().any(|op| matches!(op, Op::IDivK { .. })), "{c:?}");
    assert!(
        c.iter().any(|op| matches!(op, Op::Concat { count: 2, .. })),
        "{c:?}"
    );
}

#[test]
fn swap_reads_before_writing() {
    // local a, b = 1, 2; a, b = b, a
    let mut b = B::new();
    let a = b.new_local("a", LocalKind::Normal);
    let bl = b.new_local("b", LocalKind::Normal);
    let one = b.int(1);
    let two = b.int(2);
    b.stmt(StmtKind::Let {
        locals: vec![a, bl],
        values: vec![one, two].into(),
    });
    let br = b.local(bl);
    let ar = b.local(a);
    b.stmt(StmtKind::Assign {
        targets: vec![Place::Local(a), Place::Local(bl)],
        values: vec![br, ar].into(),
    });
    let chunk = b.finish();
    assert_eq!(
        code(&chunk, 0)[2..],
        [
            // `a` goes to a temporary because `b`'s value reads it; `b`'s value goes
            // straight into `b`.
            Op::Move { dst: 2, src: 1 },
            Op::Move { dst: 1, src: 0 },
            Op::Move { dst: 0, src: 2 },
            Op::Return0,
        ]
    );
}

#[test]
fn index_key_is_read_before_assignment() {
    // local i, t = 1, {}; i, t[i] = i + 1, 20
    let mut b = B::new();
    let i = b.new_local("i", LocalKind::Normal);
    let t = b.new_local("t", LocalKind::Normal);
    let one = b.int(1);
    let tbl = b.e(ExprKind::Table(vec![]));
    b.stmt(StmtKind::Let {
        locals: vec![i, t],
        values: vec![one, tbl].into(),
    });
    let tr = b.local(t);
    let ir = b.local(i);
    let ir2 = b.local(i);
    let one = b.int(1);
    let inc = b.bin(BinOp::Add, ir2, one);
    let twenty = b.int(20);
    b.stmt(StmtKind::Assign {
        targets: vec![Place::Local(i), Place::Index { obj: tr, key: ir }],
        values: vec![inc, twenty].into(),
    });
    let chunk = b.finish();
    let c = code(&chunk, 0);
    // The key `i` is copied to a temporary before `i` is reassigned, and `i + 1` (which
    // no later value reads) goes straight into `i`.
    let copy_key = c
        .iter()
        .position(|op| *op == Op::Move { dst: 2, src: 0 })
        .unwrap();
    let set_i = c
        .iter()
        .position(|op| {
            *op == Op::AddInt {
                dst: 0,
                a: 0,
                imm: 1,
            }
        })
        .unwrap();
    assert!(copy_key < set_i, "{c:?}");
    assert!(
        c.contains(&Op::SetIndex {
            obj: 1,
            key: 2,
            src: 3
        }),
        "{c:?}"
    );
}

#[test]
fn table_constructor_with_multi_value_tail() {
    // return {1, 2, x = 3, f()}
    let mut b = B::new();
    let one = b.int(1);
    let two = b.int(2);
    let xk = b.str("x");
    let three = b.int(3);
    let f = b.global("f");
    let call = b.call(f, vec![]);
    let t = b.e(ExprKind::Table(vec![
        TableEntry::Positional(one),
        TableEntry::Positional(two),
        TableEntry::Keyed(xk, three),
        TableEntry::Positional(call),
    ]));
    b.stmt(StmtKind::Return(vec![t].into()));
    let chunk = b.finish();
    assert_eq!(
        code(&chunk, 0),
        vec![
            Op::NewTable {
                dst: 0,
                array_hint: 3,
                hash_hint: 1
            },
            Op::LoadInt { dst: 1, value: 1 },
            Op::LoadInt { dst: 2, value: 2 },
            Op::LoadInt { dst: 3, value: 3 },
            Op::SetField {
                obj: 0,
                key: 0,
                src: 3,
                ic: 0
            },
            Op::GetGlobal {
                dst: 3,
                name: 1,
                ic: 1
            },
            Op::Call {
                func: 3,
                nargs: 0,
                nresults: MULTI
            },
            Op::SetList {
                table: 0,
                first: 1,
                count: MULTI,
                start: 1
            },
            Op::Return1 { src: 0 },
        ]
    );
}

#[test]
fn closures_capture_cells_through_levels() {
    // local x = 1
    // local function f() return function() x = x + 1; return x end end
    let mut b = B::new();
    let one = b.int(1);
    let x = b.let_("x", Some(one));
    let fl = b.new_local("f", LocalKind::Normal);
    let (fid, _) = b.begin_fn(&[]);
    let (gid, _) = b.begin_fn(&[]);
    let xr = b.e(ExprKind::Outer {
        func: FuncId(0),
        local: x,
    });
    let one = b.int(1);
    let sum = b.bin(BinOp::Add, xr, one);
    b.stmt(StmtKind::Assign {
        targets: vec![Place::Outer {
            func: FuncId(0),
            local: x,
        }],
        values: vec![sum].into(),
    });
    let xr = b.e(ExprKind::Outer {
        func: FuncId(0),
        local: x,
    });
    b.stmt(StmtKind::Return(vec![xr].into()));
    b.end_fn();
    let clo = b.e(ExprKind::Closure(gid));
    b.stmt(StmtKind::Return(vec![clo].into()));
    b.end_fn();
    b.stmt(StmtKind::LocalFunction {
        local: fl,
        func: fid,
    });
    let chunk = b.finish();

    let out = compile(&chunk).unwrap();
    assert_eq!(out.protos.len(), 3);
    let main = &out.protos[0];
    assert_eq!(
        main.code,
        vec![
            Op::LoadInt { dst: 0, value: 1 },
            Op::CellNew { cell: 0, src: 0 },
            Op::Closure { dst: 1, proto: 0 },
            Op::Return0,
        ]
    );
    assert_eq!(main.children, vec![1]);
    assert!(main.captures.is_empty());
    assert_eq!(out.protos[1].captures, vec![CaptureSource::Cell(0)]);
    assert_eq!(out.protos[1].children, vec![2]);
    assert_eq!(out.protos[2].captures, vec![CaptureSource::Capture(0)]);
    assert_eq!(
        out.protos[2].code,
        vec![
            Op::CapGet { dst: 0, cap: 0 },
            Op::AddInt {
                dst: 0,
                a: 0,
                imm: 1
            },
            Op::CapSet { cap: 0, src: 0 },
            Op::CapGet { dst: 0, cap: 0 },
            Op::Return1 { src: 0 },
        ]
    );
    assert_eq!(
        out.protos[2].debug.capture_names,
        vec![Box::from(b"x".as_slice())]
    );
}

#[test]
fn recursive_local_function_gets_its_cell_first() {
    // local function f(n) return f(n) end
    let mut b = B::new();
    let fl = b.new_local("f", LocalKind::Normal);
    let (fid, ps) = b.begin_fn(&["n"]);
    let fr = b.e(ExprKind::Outer {
        func: FuncId(0),
        local: fl,
    });
    let n = b.local(ps[0]);
    let c = b.call(fr, vec![n]);
    b.stmt(StmtKind::Return(vec![c].into()));
    b.end_fn();
    b.stmt(StmtKind::LocalFunction {
        local: fl,
        func: fid,
    });
    let chunk = b.finish();
    let out = compile(&chunk).unwrap();
    assert_eq!(
        out.protos[0].code,
        vec![
            Op::LoadNil { dst: 0, count: 1 },
            Op::CellNew { cell: 0, src: 0 },
            Op::Closure { dst: 0, proto: 0 },
            Op::CellSet { cell: 0, src: 0 },
            Op::Return0,
        ]
    );
    // `f` is never assigned, so its capture always holds the running closure.
    assert_eq!(
        out.protos[1].code,
        vec![
            Op::CapSelf { dst: 1 },
            Op::Move { dst: 2, src: 0 },
            Op::TailCall { func: 1, nargs: 1 },
            Op::Return {
                first: 1,
                count: MULTI
            },
        ]
    );
    assert_eq!(
        out.protos[1].debug.origin(2, 0),
        Some(&Origin::Upvalue(b"f".as_slice().into()))
    );
}

#[test]
fn reassigned_local_function_reads_its_capture() {
    // local function f() return f end; f = 1
    let mut b = B::new();
    let fl = b.new_local("f", LocalKind::Normal);
    let (fid, _) = b.begin_fn(&[]);
    let fr = b.e(ExprKind::Outer {
        func: FuncId(0),
        local: fl,
    });
    b.stmt(StmtKind::Return(vec![fr].into()));
    b.end_fn();
    b.stmt(StmtKind::LocalFunction {
        local: fl,
        func: fid,
    });
    let one = b.int(1);
    b.stmt(StmtKind::Assign {
        targets: vec![Place::Local(fl)],
        values: vec![one].into(),
    });
    let chunk = b.finish();
    let out = compile(&chunk).unwrap();
    assert_eq!(out.protos[1].code[0], Op::CapGet { dst: 0, cap: 0 });
}

#[test]
fn numeric_for_with_captured_control_variable() {
    // local fs = {}; for i = 1, 3 do fs[i] = function() return i end end
    let mut b = B::new();
    let tbl = b.e(ExprKind::Table(vec![]));
    let fs = b.let_("fs", Some(tbl));
    let i = b.new_local("i", LocalKind::ForControl);
    let start = b.int(1);
    let limit = b.int(3);
    b.open();
    let (gid, _) = b.begin_fn(&[]);
    let ir = b.e(ExprKind::Outer {
        func: FuncId(0),
        local: i,
    });
    b.stmt(StmtKind::Return(vec![ir].into()));
    b.end_fn();
    let fsr = b.local(fs);
    let ir = b.local(i);
    let clo = b.e(ExprKind::Closure(gid));
    b.stmt(StmtKind::Assign {
        targets: vec![Place::Index { obj: fsr, key: ir }],
        values: vec![clo].into(),
    });
    let body = b.close();
    b.stmt(StmtKind::NumericFor {
        var: i,
        start,
        limit,
        step: None,
        body,
    });
    let chunk = b.finish();
    let c = code(&chunk, 0);
    assert_eq!(
        c,
        vec![
            Op::NewTable {
                dst: 0,
                array_hint: 0,
                hash_hint: 0
            },
            Op::LoadInt { dst: 1, value: 1 },
            Op::LoadInt { dst: 2, value: 3 },
            Op::LoadInt { dst: 3, value: 1 },
            Op::ForPrep { base: 1, exit: 5 },
            Op::CellNew { cell: 0, src: 4 },
            Op::CellGet { dst: 5, cell: 0 },
            Op::Closure { dst: 6, proto: 0 },
            Op::SetIndex {
                obj: 0,
                key: 5,
                src: 6
            },
            Op::ForLoop { base: 1, back: -5 },
            Op::Return0,
        ]
    );
}

#[test]
fn generic_for_and_break() {
    // for k, v in pairs(t) do if v then break end end
    let mut b = B::new();
    let k = b.new_local("k", LocalKind::ForControl);
    let v = b.new_local("v", LocalKind::Normal);
    let h: Vec<LocalId> = (0..4)
        .map(|_| b.f().add_local(Local::new(None, SP, LocalKind::Hidden)))
        .collect();
    let pairs = b.global("pairs");
    let t = b.global("t");
    let call = b.call(pairs, vec![t]);
    b.open();
    let vr = b.local(v);
    b.open();
    b.stmt(StmtKind::Break);
    let then = b.close();
    b.stmt(StmtKind::If {
        arms: vec![(vr, then)],
        else_: None,
    });
    let body = b.close();
    b.stmt(StmtKind::GenericFor {
        vars: vec![k, v],
        iter: vec![call].into(),
        hidden: [h[0], h[1], h[2], h[3]],
        body,
    });
    let chunk = b.finish();
    let out = compile(&chunk).unwrap();
    let c = &out.protos[0].code;
    assert_eq!(
        *c,
        vec![
            Op::GetGlobal {
                dst: 0,
                name: 0,
                ic: 0
            },
            Op::GetGlobal {
                dst: 1,
                name: 1,
                ic: 1
            },
            Op::Call {
                func: 0,
                nargs: 1,
                nresults: 4
            },
            // The closing value.
            Op::Tbc { reg: 3 },
            Op::Jump { off: 2 },
            Op::JumpIfFalse { src: 5, off: 1 },
            Op::Jump { off: 2 },
            Op::GenericCall { base: 0, nvars: 2 },
            Op::GenericLoop { base: 0, back: -4 },
            Op::Close { from: 3 },
            Op::Return0,
        ]
    );
    assert!(out.protos[0].nregs >= 7);
}

#[test]
fn conditions_branch_without_booleans() {
    // if a < 1 and not b then x = 1 end; return a > b
    let mut b = B::new();
    let a = b.global("a");
    let one = b.int(1);
    let lt = b.bin(BinOp::Lt, a, one);
    let bg = b.global("b");
    let not = b.e(ExprKind::Unary {
        op: UnOp::Not,
        expr: bg,
    });
    let cond = b.e(ExprKind::And { lhs: lt, rhs: not });
    b.open();
    let one = b.int(1);
    let xs = b.s("x");
    b.stmt(StmtKind::Assign {
        targets: vec![Place::Global(xs)],
        values: vec![one].into(),
    });
    let then = b.close();
    b.stmt(StmtKind::If {
        arms: vec![(cond, then)],
        else_: None,
    });
    let a = b.global("a");
    let bg = b.global("b");
    let gt = b.bin(BinOp::Gt, a, bg);
    b.stmt(StmtKind::Return(vec![gt].into()));
    let chunk = b.finish();
    let c = code(&chunk, 0);
    assert_eq!(
        c,
        vec![
            Op::GetGlobal {
                dst: 0,
                name: 0,
                ic: 0
            },
            // `a < 1` compares with an immediate.
            Op::JumpLtInt {
                a: 0,
                imm: 1,
                want: false,
                off: 4
            },
            Op::GetGlobal {
                dst: 0,
                name: 1,
                ic: 1
            },
            Op::JumpIfTrue { src: 0, off: 2 },
            Op::LoadInt { dst: 0, value: 1 },
            Op::SetGlobal {
                src: 0,
                name: 2,
                ic: 2
            },
            Op::GetGlobal {
                dst: 1,
                name: 0,
                ic: 3
            },
            Op::GetGlobal {
                dst: 2,
                name: 1,
                ic: 4
            },
            Op::JumpNotLt { a: 2, b: 1, off: 2 },
            Op::LoadBool {
                dst: 0,
                value: true
            },
            Op::Jump { off: 1 },
            Op::LoadBool {
                dst: 0,
                value: false
            },
            Op::Return1 { src: 0 },
        ]
    );
}

#[test]
fn error_origins() {
    // local t = {}; return t.a.b, t[1].c, t:m(), -t
    let mut b = B::new();
    let tbl = b.e(ExprKind::Table(vec![]));
    let t = b.let_("t", Some(tbl));
    let tr = b.local(t);
    let ta = b.field(tr, "a");
    let tab = b.field(ta, "b");
    let tr = b.local(t);
    let one = b.int(1);
    let t1 = b.e(ExprKind::Index { obj: tr, key: one });
    let t1c = b.field(t1, "c");
    let tr = b.local(t);
    let m = b.s("m");
    let mc = b.e(ExprKind::MethodCall {
        obj: tr,
        name: m,
        args: ExprList::default(),
    });
    let tr = b.local(t);
    let neg = b.e(ExprKind::Unary {
        op: UnOp::Neg,
        expr: tr,
    });
    b.stmt(StmtKind::Return(vec![tab, t1c, mc, neg].into()));
    let chunk = b.finish();
    let d = disassemble(&compile(&chunk).unwrap());
    assert!(
        d.contains("GetField { dst: 1, obj: 0, key: 0, ic: 0 } ; 0: local \"t\""),
        "{d}"
    );
    assert!(
        d.contains("GetField { dst: 1, obj: 1, key: 1, ic: 1 } ; 0: field \"a\""),
        "{d}"
    );
    assert!(d.contains("; 0: field \"integer index\""), "{d}");
    assert!(
        d.contains("SelfCall { dst: 3, obj: 0, key: 3, ic: 3 } ; 0: local \"t\""),
        "{d}"
    );
    assert!(d.contains("; 0: method \"m\""), "{d}");
    assert!(d.contains("Neg { dst: 4, src: 0 } ; 0: local \"t\""), "{d}");
}

#[test]
fn varargs_and_multiple_results() {
    // local a, b, c = ...; return select("#", ...), ...
    let mut b = B::new();
    let a = b.new_local("a", LocalKind::Normal);
    let bl = b.new_local("b", LocalKind::Normal);
    let c = b.new_local("c", LocalKind::Normal);
    let va = b.e(ExprKind::Vararg);
    b.stmt(StmtKind::Let {
        locals: vec![a, bl, c],
        values: vec![va].into(),
    });
    let sel = b.global("select");
    let hash = b.str("#");
    let va = b.e(ExprKind::Vararg);
    let call = b.call(sel, vec![hash, va]);
    let va = b.e(ExprKind::Vararg);
    b.stmt(StmtKind::Return(vec![call, va].into()));
    let chunk = b.finish();
    assert_eq!(
        code(&chunk, 0),
        vec![
            Op::Vararg { dst: 0, count: 3 },
            Op::GetGlobal {
                dst: 3,
                name: 0,
                ic: 0
            },
            Op::LoadConst { dst: 4, k: 1 },
            Op::Vararg {
                dst: 5,
                count: MULTI
            },
            Op::Call {
                func: 3,
                nargs: MULTI,
                nresults: 1
            },
            Op::Vararg {
                dst: 4,
                count: MULTI
            },
            Op::Return {
                first: 3,
                count: MULTI
            },
        ]
    );
}

#[test]
fn too_many_registers_is_a_compile_error() {
    // return 1, 2, ..., 300
    let mut b = B::new();
    let items: Vec<ExprId> = (0..300).map(|i| b.int(i)).collect();
    b.stmt(StmtKind::Return(items.into()));
    let chunk = b.finish();
    let err = compile(&chunk).unwrap_err();
    assert_eq!(
        err.to_string(),
        "t.lua:1: function or expression needs too many registers"
    );
}

#[test]
fn table_to_live_local_goes_through_a_temporary() {
    // local x = 1; x = {x}
    let mut b = B::new();
    let one = b.int(1);
    let x = b.let_("x", Some(one));
    let xr = b.local(x);
    let t = b.e(ExprKind::Table(vec![TableEntry::Positional(xr)]));
    b.stmt(StmtKind::Assign {
        targets: vec![Place::Local(x)],
        values: vec![t].into(),
    });
    let chunk = b.finish();
    assert_eq!(
        code(&chunk, 0)[1..],
        [
            Op::NewTable {
                dst: 1,
                array_hint: 1,
                hash_hint: 0
            },
            Op::Move { dst: 2, src: 0 },
            Op::SetList {
                table: 1,
                first: 2,
                count: 1,
                start: 1
            },
            Op::Move { dst: 0, src: 1 },
            Op::Return0,
        ]
    );
}
