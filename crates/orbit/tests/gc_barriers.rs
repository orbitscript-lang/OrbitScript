//! One test for each way a script or a host can store a reference into an
//! object the collector may already have traced: the write paths that need
//! a write barrier.
//!
//! Each test makes the object that is stored into old (a full collection in
//! generational mode leaves every survivor old), stores a new object into
//! it through one path, from a function that has returned so no register
//! still holds it, runs a minor collection, and then uses the new object. A
//! minor collection traces only young objects and old ones a barrier
//! remembered, so a missing barrier frees the new object. In debug builds
//! the heap verifier checks every marking and reports that at once, with
//! the chain of objects that leads to it.
//!
//! `tests/gc-barriers/check-deletions.sh` proves each test: it deletes the
//! barrier the test depends on (the patches in `tests/gc-barriers/`) and
//! expects the test to fail.

use orbit::stack::{REGISTRY_INDEX, Return, Stack, StackResult, upvalue_index};
use orbit::{Chunk, Config, Runtime, Scope};

/// Helpers every test's script can use.
const PRELUDE: &str = r#"
    -- Make everything reachable now old: in generational mode a full
    -- collection leaves its survivors old.
    function age() collectgarbage() return 0 end
    -- One minor collection: it traces young objects and remembered old
    -- ones only.
    function minor() collectgarbage("step") end
    -- Overwrite the registers a returned helper used, so no dead register
    -- keeps a new object alive.
    function scrub()
      local a, b, c, d, e, f, g, h, i, j, k, l, m, n, o, p, q, r, s, t, u, v, w, x, y, z =
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0
      return a
    end
    -- Run `store` (which makes and stores a new object), then collect.
    function store_then_collect(store)
      store()
      scrub()
      minor()
      minor()
    end
"#;

/// `ud_new(nuv)`: a full userdata with `nuv` user values.
struct UdNew;
impl orbit::stack::Native for UdNew {
    const NAME: &'static str = "ud_new";
    fn call(s: &mut Stack<'_>) -> StackResult<Return> {
        let nuv = s.check_integer(1)? as i32;
        s.new_userdata(8, nuv)?;
        Ok(Return::values(1))
    }
}

/// `ud_set(u, n, v)`: user value `n` of `u` becomes `v`.
struct UdSet;
impl orbit::stack::Native for UdSet {
    const NAME: &'static str = "ud_set";
    fn call(s: &mut Stack<'_>) -> StackResult<Return> {
        let n = s.check_integer(2)? as i32;
        s.push_copy(3);
        assert!(s.set_user_value(1, n)?);
        Ok(Return::values(0))
    }
}

/// `ud_get(u, n)`: user value `n` of `u`.
struct UdGet;
impl orbit::stack::Native for UdGet {
    const NAME: &'static str = "ud_get";
    fn call(s: &mut Stack<'_>) -> StackResult<Return> {
        let n = s.check_integer(2)? as i32;
        s.get_user_value(1, n);
        Ok(Return::values(1))
    }
}

/// A closure over one upvalue: called with a value it stores it there,
/// called without one it returns it.
struct UpvalueCell;
impl orbit::stack::Native for UpvalueCell {
    const NAME: &'static str = "upvalue_cell";
    fn call(s: &mut Stack<'_>) -> StackResult<Return> {
        if s.top() >= 1 {
            s.copy(1, upvalue_index(1))?;
            return Ok(Return::values(0));
        }
        s.push_copy(upvalue_index(1));
        Ok(Return::values(1))
    }
}

/// `new_upvalue_cell()`: an [`UpvalueCell`] holding nil.
struct NewUpvalueCell;
impl orbit::stack::Native for NewUpvalueCell {
    const NAME: &'static str = "new_upvalue_cell";
    fn call(s: &mut Stack<'_>) -> StackResult<Return> {
        s.push_nil();
        s.push_native_closure::<UpvalueCell>(1, 0)?;
        Ok(Return::values(1))
    }
}

/// `reg_set(k, v)` and `reg_get(k)`: field `k` of the registry.
struct RegSet;
impl orbit::stack::Native for RegSet {
    const NAME: &'static str = "reg_set";
    fn call(s: &mut Stack<'_>) -> StackResult<Return> {
        let k = s.check_string(1)?.to_vec();
        s.push_copy(2);
        s.set_field(REGISTRY_INDEX, &k[..])?;
        Ok(Return::values(0))
    }
}

struct RegGet;
impl orbit::stack::Native for RegGet {
    const NAME: &'static str = "reg_get";
    fn call(s: &mut Stack<'_>) -> StackResult<Return> {
        let k = s.check_string(1)?.to_vec();
        s.get_field(REGISTRY_INDEX, &k[..])?;
        Ok(Return::values(1))
    }
}

/// `pin(v)`: keep `v` in a pin slot, and give the slot; `pinned(slot)`:
/// the value in it.
struct Pin;
impl orbit::stack::Native for Pin {
    const NAME: &'static str = "pin";
    fn call(s: &mut Stack<'_>) -> StackResult<Return> {
        let slot = s.pin(1);
        s.push_integer(i64::from(slot));
        Ok(Return::values(1))
    }
}

struct Pinned;
impl orbit::stack::Native for Pinned {
    const NAME: &'static str = "pinned";
    fn call(s: &mut Stack<'_>) -> StackResult<Return> {
        let slot = s.check_integer(1)? as u32;
        s.push_pinned(slot);
        Ok(Return::values(1))
    }
}

fn install<'s>(s: &'s Scope<'s>) -> Result<(), orbit::Error> {
    let g = s.globals();
    g.set("ud_new", s.create_native::<UdNew>())?;
    g.set("ud_set", s.create_native::<UdSet>())?;
    g.set("ud_get", s.create_native::<UdGet>())?;
    g.set("new_upvalue_cell", s.create_native::<NewUpvalueCell>())?;
    g.set("reg_set", s.create_native::<RegSet>())?;
    g.set("reg_get", s.create_native::<RegGet>())?;
    g.set("pin", s.create_native::<Pin>())?;
    g.set("pinned", s.create_native::<Pinned>())?;
    s.load(Chunk::new(PRELUDE).name("=prelude"))?.call(())?;
    Ok(())
}

/// Run `src` in a generational isolate whose verifier (debug builds)
/// checks every marking.
fn run(name: &str, src: &str) {
    let mut iso = Runtime::new().new_isolate(Config::default());
    orbit_std::install(&mut iso, &orbit_std::Options::default()).unwrap();
    #[cfg(debug_assertions)]
    iso.gc().set_verify(true, true);
    iso.scope(|s| {
        install(s)?;
        s.load(Chunk::new(src).name(name))?.call(())?;
        Ok::<_, orbit::Error>(())
    })
    .unwrap_or_else(|e| panic!("{name}: {e}"));
}

// ---- tables ---------------------------------------------------------------

#[test]
fn table_store_by_integer_key() {
    run(
        "=int_key",
        r#"
        local t = {1, 2, 3}
        age()
        store_then_collect(function () t[2] = {"v"} end)
        assert(t[2][1] == "v")
        "#,
    );
}

#[test]
fn table_store_by_integer_key_into_a_big_table() {
    // A big old table is remembered by part (cards).
    run(
        "=int_key_big",
        r#"
        local t = {}
        for i = 1, 5000 do t[i] = i end
        age()
        store_then_collect(function () t[3000] = {"v"} end)
        assert(t[3000][1] == "v")
        "#,
    );
}

#[test]
fn table_raw_store_by_integer_key() {
    run(
        "=rawset_int",
        r#"
        local t = {1, 2, 3}
        age()
        store_then_collect(function () rawset(t, 2, {"v"}) end)
        assert(t[2][1] == "v")
        "#,
    );
}

#[test]
fn table_store_by_cached_string_key() {
    run(
        "=cached_field",
        r#"
        local t = {x = 0}
        local function put(v) t.x = v end
        put(1) -- fills the instruction's cache
        age()
        store_then_collect(function () put({"v"}) end)
        assert(t.x[1] == "v")
        "#,
    );
}

#[test]
fn table_store_by_string_key_missing_the_cache() {
    run(
        "=uncached_field",
        r#"
        local t = {x = 0}
        age()
        store_then_collect(function () t.x = {"v"} end)
        assert(t.x[1] == "v")
        "#,
    );
}

#[test]
fn table_store_of_a_new_string_key_into_a_free_slot() {
    run(
        "=new_field_free_slot",
        r#"
        local t = {a = 1, b = 2, c = 3}
        age()
        store_then_collect(function () t.d = {"v"} end)
        assert(t.d[1] == "v")
        "#,
    );
}

#[test]
fn table_store_of_a_new_key_that_grows_the_table() {
    run(
        "=new_key_grows",
        r#"
        local t = {}
        age()
        store_then_collect(function () t[1.5] = {"v"} end)
        assert(t[1.5][1] == "v")
        "#,
    );
}

#[test]
fn table_store_of_a_new_object_key() {
    run(
        "=object_key",
        r#"
        local t = {}
        age()
        store_then_collect(function () t[{"k"}] = true end)
        local k = next(t)
        assert(k[1] == "k")
        "#,
    );
}

#[test]
fn table_store_past_a_newindex_metamethod_into_an_existing_key() {
    run(
        "=existing_key_with_newindex",
        r#"
        local t = setmetatable({x = 0}, {__newindex = function () error("not called") end})
        age()
        store_then_collect(function () t.x = {"v"} end)
        assert(t.x[1] == "v")
        "#,
    );
}

#[test]
fn global_store_by_cached_name() {
    run(
        "=cached_global",
        r#"
        local function put(v) g = v end
        put(1)
        age()
        store_then_collect(function () put({"v"}) end)
        assert(g[1] == "v")
        "#,
    );
}

#[test]
fn table_constructor_list_store() {
    // The table is made, then aged by the call in its constructor, then
    // filled: its list part is stored into an old table.
    run(
        "=constructor_list",
        r#"
        local t
        store_then_collect(function () t = {0, age(), {"v"}} end)
        assert(t[3][1] == "v")
        "#,
    );
}

#[test]
fn table_library_insert_and_move() {
    run(
        "=table_library",
        r#"
        local t, u = {1, 2, 3}, {1, 2, 3}
        age()
        store_then_collect(function ()
          table.insert(t, {"v"})
          table.move({{"w"}}, 1, 1, 2, u)
        end)
        assert(t[4][1] == "v" and u[2][1] == "w")
        "#,
    );
}

#[test]
fn table_library_sort_of_strings_in_a_big_table() {
    // `table.sort` sorts a copy and stores the result back by index, as
    // OrbitScript code (the interpreter's integer store): new strings land
    // in parts of a big old table that nothing else wrote to.
    run(
        "=sort_strings",
        r#"
        local t = {}
        for i = 1, 3000 do t[i] = string.format("s%05d", 3000 - i) end
        age()
        store_then_collect(function ()
          t[3000] = string.rep("a", 40) -- new, sorts first
          table.sort(t)
        end)
        assert(t[1] == string.rep("a", 40) and #t[1] == 40)
        "#,
    );
}

#[test]
fn registry_field_store() {
    run(
        "=registry",
        r#"
        reg_set("k", 0)
        age()
        store_then_collect(function () reg_set("k", {"v"}) end)
        assert(reg_get("k")[1] == "v")
        "#,
    );
}

// ---- upvalues -------------------------------------------------------------

#[test]
fn upvalue_store_through_a_closure() {
    run(
        "=closure_upvalue",
        r#"
        local function make()
          local x = 0
          return function (v) x = v end, function () return x end
        end
        local set, get = make()
        age()
        store_then_collect(function () set({"v"}) end)
        assert(get()[1] == "v")
        "#,
    );
}

#[test]
fn upvalue_store_through_the_frames_own_cell() {
    run(
        "=own_cell",
        r#"
        local get
        do
          local x = 0
          get = function () return x end
          age()
          x = {"v"}
        end
        -- Overwrite the registers the block used, which still hold the
        -- table, so the store into the old cell is all that keeps it.
        local r1, r2, r3, r4 = 0, 0, 0, 0
        scrub()
        minor()
        minor()
        assert(get()[1] == "v")
        "#,
    );
}

#[test]
fn debug_setupvalue() {
    run(
        "=debug_setupvalue",
        r#"
        local x = 0
        local function get() return x end
        age()
        store_then_collect(function () debug.setupvalue(get, 1, {"v"}) end)
        assert(get()[1] == "v")
        "#,
    );
}

#[test]
fn debug_setlocal_of_a_captured_local() {
    run(
        "=debug_setlocal_cell",
        r#"
        local x = 0
        local function get() return x end
        age()
        debug.setlocal(1, 1, {"v"})
        scrub()
        minor()
        minor()
        assert(get()[1] == "v")
        "#,
    );
}

#[test]
fn debug_upvaluejoin() {
    run(
        "=debug_upvaluejoin",
        r#"
        local function make(v) local x = v; return function () return x end end
        local f = make(0)
        age()
        store_then_collect(function () debug.upvaluejoin(f, 1, make({"v"}), 1) end)
        assert(f()[1] == "v")
        "#,
    );
}

#[test]
fn native_closure_upvalue_store() {
    run(
        "=native_upvalue",
        r#"
        local cell = new_upvalue_cell()
        age()
        store_then_collect(function () cell({"v"}) end)
        assert(cell()[1] == "v")
        "#,
    );
}

// ---- metatables and userdata ----------------------------------------------

#[test]
fn table_metatable() {
    run(
        "=setmetatable",
        r#"
        local t = {}
        age()
        store_then_collect(function () setmetatable(t, {__index = {v = 1}}) end)
        assert(t.v == 1)
        "#,
    );
}

#[test]
fn userdata_metatable() {
    run(
        "=userdata_metatable",
        r#"
        local u = ud_new(1)
        age()
        store_then_collect(function () debug.setmetatable(u, {__index = {v = 1}}) end)
        assert(u.v == 1)
        "#,
    );
}

#[test]
fn userdata_user_value_from_the_debug_library() {
    run(
        "=debug_setuservalue",
        r#"
        local u = ud_new(1)
        age()
        store_then_collect(function () debug.setuservalue(u, {"v"}, 1) end)
        assert(debug.getuservalue(u, 1)[1] == "v")
        "#,
    );
}

#[test]
fn userdata_user_value_from_the_stack_api() {
    run(
        "=stack_set_user_value",
        r#"
        local u, w = ud_new(1), ud_new(3)
        age()
        store_then_collect(function () ud_set(u, 1, {"v"}); ud_set(w, 2, {"w"}) end)
        assert(ud_get(u, 1)[1] == "v" and ud_get(w, 2)[1] == "w")
        "#,
    );
}

// ---- coroutines -----------------------------------------------------------

#[test]
fn coroutine_parked_stack() {
    // The coroutine's thread is old when it parks a stack holding a new
    // object.
    run(
        "=parked_stack",
        r#"
        local co = coroutine.create(function ()
          coroutine.yield()
          local x = {"v"}
          coroutine.yield()
          return x[1]
        end)
        coroutine.resume(co)
        age()
        store_then_collect(function () coroutine.resume(co) end)
        assert(select(2, coroutine.resume(co)) == "v")
        "#,
    );
}

#[test]
fn coroutine_resume_and_yield_values() {
    run(
        "=transferred",
        r#"
        local co = coroutine.create(function (a)
          local b = coroutine.yield()
          coroutine.yield()
          return a[1] .. b[1]
        end)
        age()
        store_then_collect(function () coroutine.resume(co, {"v"}) end)
        store_then_collect(function () coroutine.resume(co, {"w"}) end)
        local ok, r = coroutine.resume(co)
        assert(ok and r == "vw", r)
        "#,
    );
}

#[test]
fn coroutine_error_value() {
    run(
        "=coroutine_error",
        r#"
        local co = coroutine.create(function () coroutine.yield(); error({"v"}) end)
        coroutine.resume(co)
        age()
        store_then_collect(function () coroutine.resume(co) end)
        local ok, e = coroutine.close(co)
        assert(not ok and e[1] == "v")
        "#,
    );
}

#[test]
fn debug_setlocal_in_a_suspended_coroutine() {
    run(
        "=debug_setlocal_thread",
        r#"
        local co = coroutine.create(function ()
          local x = 0
          coroutine.yield()
          return x[1]
        end)
        coroutine.resume(co)
        age()
        store_then_collect(function () debug.setlocal(co, 1, 1, {"v"}) end)
        assert(select(2, coroutine.resume(co)) == "v")
        "#,
    );
}

// ---- host roots -------------------------------------------------------------

#[test]
fn pinned_value() {
    // Pin slots are roots, traced at every marking: no barrier.
    run(
        "=pinned",
        r#"
        local slot
        age()
        store_then_collect(function () slot = pin({"v"}) end)
        assert(pinned(slot)[1] == "v")
        "#,
    );
}
