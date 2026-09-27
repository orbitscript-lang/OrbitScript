//! Hand-assembled benchmark programs. The matching Lua sources live in
//! `spike/lua/*.lua`; each comment shows the Lua being mirrored. Every program
//! returns one integer so results can be checked against `lua5.5`.

use crate::{Asm, Op::*, Proto, UpvalDesc};

pub const NAMES: &[&str] = &[
    "fib",
    "loop",
    "str",
    "gc",
    "objs_tables",
    "objs_closures",
    "objs_strings",
];

pub fn build(name: &str, n: i64) -> Proto {
    match name {
        "fib" => fib(n),
        "loop" => table_loop(n),
        "str" => strings(n),
        "gc" => gc_churn(n),
        "objs_tables" => objs_tables(n),
        "objs_closures" => objs_closures(n),
        "objs_strings" => objs_strings(n),
        _ => panic!("unknown bench {name}"),
    }
}

/// local function fib(n) if n < 2 then return n end return fib(n-1) + fib(n-2) end
/// return fib(N)
pub fn fib(n: i64) -> Proto {
    let mut f = Asm::new();
    let skip = f.label();
    f.op(LtI {
        a: 0,
        i: 2,
        k: false,
    });
    f.op_to(Jmp { to: 0 }, skip);
    f.op(Return { a: 0, n: 1 });
    f.bind(skip);
    f.op(GetUpval { a: 1, u: 0 })
        .op(AddI { a: 2, b: 0, i: -1 })
        .op(Call {
            a: 1,
            nargs: 1,
            nres: 1,
        })
        .op(GetUpval { a: 2, u: 0 })
        .op(AddI { a: 3, b: 0, i: -2 })
        .op(Call {
            a: 2,
            nargs: 1,
            nres: 1,
        })
        .op(Add { a: 1, b: 1, c: 2 })
        .op(Return { a: 1, n: 1 });
    let fibp = f.finish("fib", 1, 4, vec![UpvalDesc::Reg(0)]);

    let mut m = Asm::new();
    let p = m.proto(fibp);
    let k = m.k_int(n);
    m.op(LoadNil { a: 0 })
        .op(Closure { a: 0, p })
        .op(Move { a: 1, b: 0 })
        .op(LoadK { a: 2, k })
        .op(Call {
            a: 1,
            nargs: 1,
            nres: 1,
        })
        .op(Return { a: 1, n: 1 });
    m.finish("main", 0, 3, vec![])
}

/// local t = {} for i = 1, N do t[i] = i end
/// local s = 0 for r = 1, 10 do for i = 1, N do s = s + t[i] end end
/// local o = {} o.x = 0 for i = 1, N do o.x = o.x + i end
/// return s + o.x
pub fn table_loop(n: i64) -> Proto {
    let mut m = Asm::new();
    let kn = m.k_int(n);
    let kx = m.k_str("x") as u8;
    let (l1, b1, l2, b2, l3, b3, l4, b4) = (
        m.label(),
        m.label(),
        m.label(),
        m.label(),
        m.label(),
        m.label(),
        m.label(),
        m.label(),
    );
    m.op(LoadK { a: 0, k: kn }).op(NewTable { a: 1 });
    m.op(LoadI { a: 2, i: 1 })
        .op(Move { a: 3, b: 0 })
        .op(LoadI { a: 4, i: 1 })
        .op_to(ForPrep { a: 2, exit: 0 }, l1);
    m.bind(b1);
    m.op(SetTable { a: 1, b: 5, c: 5 })
        .op_to(ForLoop { a: 2, body: 0 }, b1);
    m.bind(l1);
    m.op(LoadI { a: 6, i: 0 });
    m.op(LoadI { a: 7, i: 1 })
        .op(LoadI { a: 8, i: 10 })
        .op(LoadI { a: 9, i: 1 })
        .op_to(ForPrep { a: 7, exit: 0 }, l2);
    m.bind(b2);
    m.op(LoadI { a: 11, i: 1 })
        .op(Move { a: 12, b: 0 })
        .op(LoadI { a: 13, i: 1 })
        .op_to(ForPrep { a: 11, exit: 0 }, l3);
    m.bind(b3);
    m.op(GetTable { a: 15, b: 1, c: 14 })
        .op(Add { a: 6, b: 6, c: 15 })
        .op_to(ForLoop { a: 11, body: 0 }, b3);
    m.bind(l3);
    m.op_to(ForLoop { a: 7, body: 0 }, b2);
    m.bind(l2);
    m.op(NewTable { a: 7 })
        .op(LoadI { a: 8, i: 0 })
        .op(SetField { a: 7, k: kx, c: 8 });
    m.op(LoadI { a: 8, i: 1 })
        .op(Move { a: 9, b: 0 })
        .op(LoadI { a: 10, i: 1 })
        .op_to(ForPrep { a: 8, exit: 0 }, l4);
    m.bind(b4);
    m.op(GetField { a: 12, b: 7, k: kx })
        .op(Add {
            a: 12,
            b: 12,
            c: 11,
        })
        .op(SetField { a: 7, k: kx, c: 12 })
        .op_to(ForLoop { a: 8, body: 0 }, b4);
    m.bind(l4);
    m.op(GetField { a: 8, b: 7, k: kx })
        .op(Add { a: 8, b: 6, c: 8 })
        .op(Return { a: 8, n: 1 });
    m.finish("main", 0, 16, vec![])
}

/// local t = {} for i = 1, N do t["k" .. i] = i end
/// local s = 0 for i = 1, N do s = s + t["k" .. i] end
/// local acc = "" for i = 1, N // 50 do acc = acc .. "x" end
/// return s + #acc
pub fn strings(n: i64) -> Proto {
    let mut m = Asm::new();
    let kn = m.k_int(n);
    let kn50 = m.k_int(n / 50);
    let kk = m.k_str("k");
    let ke = m.k_str("");
    let kxs = m.k_str("x");
    let (l1, b1, l2, b2, l3, b3) = (
        m.label(),
        m.label(),
        m.label(),
        m.label(),
        m.label(),
        m.label(),
    );
    m.op(LoadK { a: 0, k: kn })
        .op(NewTable { a: 1 })
        .op(LoadK { a: 2, k: kk });
    m.op(LoadI { a: 3, i: 1 })
        .op(Move { a: 4, b: 0 })
        .op(LoadI { a: 5, i: 1 })
        .op_to(ForPrep { a: 3, exit: 0 }, l1);
    m.bind(b1);
    m.op(Concat { a: 7, b: 2, c: 6 })
        .op(SetTable { a: 1, b: 7, c: 6 })
        .op_to(ForLoop { a: 3, body: 0 }, b1);
    m.bind(l1);
    m.op(LoadI { a: 7, i: 0 });
    m.op(LoadI { a: 8, i: 1 })
        .op(Move { a: 9, b: 0 })
        .op(LoadI { a: 10, i: 1 })
        .op_to(ForPrep { a: 8, exit: 0 }, l2);
    m.bind(b2);
    m.op(Concat { a: 12, b: 2, c: 11 })
        .op(GetTable { a: 12, b: 1, c: 12 })
        .op(Add { a: 7, b: 7, c: 12 })
        .op_to(ForLoop { a: 8, body: 0 }, b2);
    m.bind(l2);
    m.op(LoadK { a: 8, k: ke }).op(LoadK { a: 9, k: kxs });
    m.op(LoadI { a: 10, i: 1 })
        .op(LoadK { a: 11, k: kn50 })
        .op(LoadI { a: 12, i: 1 })
        .op_to(ForPrep { a: 10, exit: 0 }, l3);
    m.bind(b3);
    m.op(Concat { a: 8, b: 8, c: 9 })
        .op_to(ForLoop { a: 10, body: 0 }, b3);
    m.bind(l3);
    m.op(Len { a: 10, b: 8 })
        .op(Add { a: 10, b: 7, c: 10 })
        .op(Return { a: 10, n: 1 });
    m.finish("main", 0, 14, vec![])
}

/// local ring, keep = {}, 0
/// for i = 1, N do
///   local t = {i, i + 1, x = i}
///   local f = function() return t end
///   ring[i % 1000 + 1] = f
///   local c = {} c.self = c
///   keep = keep + #t
/// end
/// return keep
pub fn gc_churn(n: i64) -> Proto {
    let mut inner = Asm::new();
    inner.op(GetUpval { a: 0, u: 0 }).op(Return { a: 0, n: 1 });
    let innerp = inner.finish("f", 0, 1, vec![UpvalDesc::Reg(10)]);

    let mut m = Asm::new();
    let p = m.proto(innerp);
    let kn = m.k_int(n);
    let kx = m.k_str("x") as u8;
    let kself = m.k_str("self") as u8;
    let (l1, b) = (m.label(), m.label());
    m.op(LoadK { a: 0, k: kn })
        .op(NewTable { a: 1 })
        .op(LoadI { a: 2, i: 0 })
        .op(LoadI { a: 3, i: 1 })
        .op(LoadI { a: 4, i: 2 })
        .op(LoadI { a: 5, i: 1000 });
    m.op(LoadI { a: 6, i: 1 })
        .op(Move { a: 7, b: 0 })
        .op(LoadI { a: 8, i: 1 })
        .op_to(ForPrep { a: 6, exit: 0 }, l1);
    m.bind(b);
    m.op(NewTable { a: 10 })
        .op(SetTable { a: 10, b: 3, c: 9 })
        .op(AddI { a: 11, b: 9, i: 1 })
        .op(SetTable { a: 10, b: 4, c: 11 })
        .op(SetField { a: 10, k: kx, c: 9 })
        .op(Closure { a: 11, p })
        .op(Mod { a: 12, b: 9, c: 5 })
        .op(AddI { a: 12, b: 12, i: 1 })
        .op(SetTable { a: 1, b: 12, c: 11 })
        .op(NewTable { a: 12 })
        .op(SetField {
            a: 12,
            k: kself,
            c: 12,
        })
        .op(Len { a: 13, b: 10 })
        .op(Add { a: 2, b: 2, c: 13 })
        .op(Close { a: 10 })
        .op_to(ForLoop { a: 6, body: 0 }, b);
    m.bind(l1);
    m.op(Return { a: 2, n: 1 });
    m.finish("main", 0, 14, vec![])
}

/// local a = {} for i = 1, N do a[i] = {} end return #a
pub fn objs_tables(n: i64) -> Proto {
    let mut m = Asm::new();
    let kn = m.k_int(n);
    let (l, b) = (m.label(), m.label());
    m.op(LoadK { a: 0, k: kn }).op(NewTable { a: 1 });
    m.op(LoadI { a: 2, i: 1 })
        .op(Move { a: 3, b: 0 })
        .op(LoadI { a: 4, i: 1 })
        .op_to(ForPrep { a: 2, exit: 0 }, l);
    m.bind(b);
    m.op(NewTable { a: 6 })
        .op(SetTable { a: 1, b: 5, c: 6 })
        .op_to(ForLoop { a: 2, body: 0 }, b);
    m.bind(l);
    m.op(Len { a: 2, b: 1 }).op(Return { a: 2, n: 1 });
    m.finish("main", 0, 7, vec![])
}

/// local a = {} for i = 1, N do local x = i a[i] = function() return x end end
/// return #a
pub fn objs_closures(n: i64) -> Proto {
    let mut inner = Asm::new();
    inner.op(GetUpval { a: 0, u: 0 }).op(Return { a: 0, n: 1 });
    let innerp = inner.finish("f", 0, 1, vec![UpvalDesc::Reg(6)]);
    let mut m = Asm::new();
    let p = m.proto(innerp);
    let kn = m.k_int(n);
    let (l, b) = (m.label(), m.label());
    m.op(LoadK { a: 0, k: kn }).op(NewTable { a: 1 });
    m.op(LoadI { a: 2, i: 1 })
        .op(Move { a: 3, b: 0 })
        .op(LoadI { a: 4, i: 1 })
        .op_to(ForPrep { a: 2, exit: 0 }, l);
    m.bind(b);
    m.op(Move { a: 6, b: 5 })
        .op(Closure { a: 7, p })
        .op(SetTable { a: 1, b: 5, c: 7 })
        .op(Close { a: 6 })
        .op_to(ForLoop { a: 2, body: 0 }, b);
    m.bind(l);
    m.op(Len { a: 2, b: 1 }).op(Return { a: 2, n: 1 });
    m.finish("main", 0, 8, vec![])
}

/// local a = {} for i = 1, N do a[i] = "k" .. i end return #a
pub fn objs_strings(n: i64) -> Proto {
    let mut m = Asm::new();
    let kn = m.k_int(n);
    let kk = m.k_str("k");
    let (l, b) = (m.label(), m.label());
    m.op(LoadK { a: 0, k: kn })
        .op(NewTable { a: 1 })
        .op(LoadK { a: 7, k: kk });
    m.op(LoadI { a: 2, i: 1 })
        .op(Move { a: 3, b: 0 })
        .op(LoadI { a: 4, i: 1 })
        .op_to(ForPrep { a: 2, exit: 0 }, l);
    m.bind(b);
    m.op(Concat { a: 6, b: 7, c: 5 })
        .op(SetTable { a: 1, b: 5, c: 6 })
        .op_to(ForLoop { a: 2, body: 0 }, b);
    m.bind(l);
    m.op(Len { a: 2, b: 1 }).op(Return { a: 2, n: 1 });
    m.finish("main", 0, 8, vec![])
}
