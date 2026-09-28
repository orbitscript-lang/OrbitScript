//! The embedding API under a GC stress collector: native functions that call
//! back into Lua, rooted handles, and what happens without rooting.

use spike_common::{Asm, Op::*, Proto};
use spike_embed::{Lua, R, Root, Val};
use spike_vm_arena::{NativeCall, Value, Vm};
use std::rc::Rc;

const N: i64 = 200;

/// local cmp = function(a, b) local junk = {} return a - b end
/// local t = {} for i = 1, N do t[i] = i * 7919 % 1009 end
/// return sorted(t, cmp)          -- `sorted` is main's parameter
fn sort_program() -> Proto {
    let mut cmp = Asm::new();
    cmp.op(NewTable { a: 2 })
        .op(Sub { a: 2, b: 0, c: 1 })
        .op(Return { a: 2, n: 1 });
    let cmp = cmp.finish("cmp", 2, 3, vec![]);

    let mut m = Asm::new();
    let p = m.proto(cmp);
    let kn = m.k_int(N);
    let k7919 = m.k_int(7919);
    let k1009 = m.k_int(1009);
    let (exit, body) = (m.label(), m.label());
    m.op(Closure { a: 1, p }).op(NewTable { a: 2 });
    m.op(LoadI { a: 3, i: 1 })
        .op(LoadK { a: 4, k: kn })
        .op(LoadI { a: 5, i: 1 })
        .op_to(ForPrep { a: 3, exit: 0 }, exit);
    m.bind(body);
    m.op(LoadK { a: 7, k: k7919 })
        .op(Mul { a: 7, b: 6, c: 7 })
        .op(LoadK { a: 8, k: k1009 })
        .op(Mod { a: 7, b: 7, c: 8 })
        .op(SetTable { a: 2, b: 6, c: 7 })
        .op_to(ForLoop { a: 3, body: 0 }, body);
    m.bind(exit);
    m.op(Move { a: 3, b: 0 })
        .op(Move { a: 4, b: 2 })
        .op(Move { a: 5, b: 1 })
        .op(Call {
            a: 3,
            nargs: 2,
            nres: 1,
        })
        .op(Return { a: 3, n: 1 });
    m.finish("main", 1, 9, vec![])
}

/// return function() for i = 1, 20 do local junk = {} end return 0 end
fn junk_program() -> Proto {
    let mut f = Asm::new();
    let (exit, body) = (f.label(), f.label());
    f.op(LoadI { a: 0, i: 1 })
        .op(LoadI { a: 1, i: 20 })
        .op(LoadI { a: 2, i: 1 })
        .op_to(ForPrep { a: 0, exit: 0 }, exit);
    f.bind(body);
    f.op(NewTable { a: 4 })
        .op_to(ForLoop { a: 0, body: 0 }, body);
    f.bind(exit);
    f.op(LoadI { a: 0, i: 0 }).op(Return { a: 0, n: 1 });
    let f = f.finish("junk", 0, 5, vec![]);
    let mut m = Asm::new();
    let p = m.proto(f);
    m.op(Closure { a: 0, p }).op(Return { a: 0, n: 1 });
    m.finish("main", 0, 1, vec![])
}

/// `sorted(t, cmp)`: an insertion sort in Rust that compares through a Lua
/// callback, building its result table before the callbacks run.
fn register_sorted(lua: &mut Lua) -> Root {
    lua.create_function(|cx| {
        let t = cx.arg(0);
        let n = cx.len(t)?;
        let mut xs = (1..=n)
            .map(|i| cx.get(t, Val::int(i)).map(|v| v.as_int().unwrap()))
            .collect::<R<Vec<i64>>>()?;
        // Created before the callbacks, so it must be rooted to survive them.
        let out = cx.root(cx.new_table());
        for i in 1..xs.len() {
            let mut j = i;
            while j > 0 {
                let cmp = cx.arg(1); // re-read after each call: args live on the stack
                let call = cx.push_call(cmp, &[Val::int(xs[j - 1]), Val::int(xs[j])]);
                let d = cx.finish(call)?.as_int().unwrap();
                if d <= 0 {
                    break;
                }
                xs.swap(j - 1, j);
                j -= 1;
            }
        }
        let out = cx.get_root(&out);
        for (i, x) in xs.iter().enumerate() {
            cx.set(out, Val::int(i as i64 + 1), Val::int(*x))?;
        }
        Ok(out)
    })
}

#[test]
fn native_calls_back_into_lua_under_gc_stress() {
    let mut lua = Lua::new();
    lua.set_gc_stress();
    let sorted = register_sorted(&mut lua);
    let main = lua.load(&sort_program());
    let before = lua.vm().heap.cycles;
    let result: Root = lua.call(&main, &[(&sorted).into()]).expect("run");
    let cycles = lua.vm().heap.cycles - before;
    assert!(
        cycles > 10,
        "collector ran during the callbacks ({cycles} cycles)"
    );

    lua.gc();
    let xs: Vec<i64> = lua.with(|cx| {
        let t = cx.get_root(&result);
        (1..=cx.len(t).unwrap())
            .map(|i| cx.get(t, Val::int(i)).unwrap().as_int().unwrap())
            .collect()
    });
    let mut expected: Vec<i64> = (1..=N).map(|i| i * 7919 % 1009).collect();
    expected.sort();
    assert_eq!(xs, expected);
}

/// What the borrow rule prevents, shown on the internal (raw) layer: a table
/// held across calls back into Lua without rooting is collected.
#[test]
fn unrooted_value_held_across_callbacks_is_collected() {
    let mut lua = Lua::new();
    lua.set_gc_stress();
    let junk: Root = {
        let main = lua.load(&junk_program());
        lua.call(&main, &[]).expect("junk fn")
    };
    let probe = lua
        .vm()
        .register_native(Rc::new(|vm: &mut Vm, call: NativeCall| {
            let t = vm.heap.new_table(); // raw handle, never rooted
            let marker = vm.heap.new_str(b"marker");
            let k = vm.heap.key(Value::Str(marker)).unwrap();
            vm.heap.set(t, k, Value::Int(42));
            let before = vm.heap.cycles;
            for _ in 0..50 {
                vm.stack[call.top] = vm.stack[call.args];
                vm.call_value(call.top, 0)?;
            }
            assert!(vm.heap.cycles >= before + 2);
            let intact = vm.heap.tab_is_live(t) && matches!(vm.heap.get(t, k), Value::Int(42));
            Ok(Value::Int(intact as i64))
        }));
    let probe = lua.root_raw(probe);
    let intact: i64 = lua.call(&probe, &[(&junk).into()]).expect("probe");
    assert_eq!(intact, 0, "the unrooted table was freed or its slot reused");
}

#[test]
fn dropping_a_root_releases_the_value() {
    let mut lua = Lua::new();
    let r = lua.with(|cx| cx.root(cx.new_table()));
    let Value::Table(t) = lua.with(|cx| cx.get_root(&r).raw()) else {
        panic!()
    };
    lua.gc();
    assert!(lua.vm().heap.tab_is_live(t));
    drop(r);
    lua.gc();
    assert!(!lua.vm().heap.tab_is_live(t));
}

/// Embedders keep `Root`s in their own types across calls and collections.
#[test]
fn roots_live_in_host_structs() {
    struct Handlers {
        on_tick: Root,
    }
    let mut lua = Lua::new();
    lua.set_gc_stress();
    let main = lua.load(&junk_program());
    let h = Handlers {
        on_tick: lua.call(&main, &[]).expect("load"),
    };
    for _ in 0..10 {
        let r: i64 = lua.call(&h.on_tick, &[]).expect("tick");
        assert_eq!(r, 0);
        lua.gc();
    }
}
