//! `Lua::set_memory_limit`: past the cap, scripts get a catchable
//! `not enough memory`; garbage is collected first; large allocations are
//! refused before they happen.

use orbit::{Lua, Value};

const CAP: usize = 16 << 20;

#[test]
fn scripts_stay_under_the_cap() {
    let mut lua = Lua::new();
    assert_eq!(lua.vm().memory_limit(), None);
    lua.set_memory_limit(Some(CAP));
    assert_eq!(lua.vm().memory_limit(), Some(CAP));
    // The script checks each case with `assert`.
    let r = lua.exec(include_bytes!("memory_limit.lua"), "=memory_limit");
    let v = r.unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(v.len(), 1);
    assert!(lua.vm().heap.bytes() <= CAP + CAP / orbit::LIMIT_GRACE);
}

#[test]
fn no_cap_by_default() {
    let mut lua = Lua::new();
    let v = lua
        .exec(b"return pcall(table.create, 1 << 22)", "=nocap")
        .unwrap();
    assert!(matches!(v[0], Value::Bool(true)));
}
