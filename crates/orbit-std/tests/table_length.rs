//! The length of tables built with nils (`#t`, and `table.unpack(t)`,
//! which stops there), where the Lua 5.5 build and the Lua 5.4 one differ
//! (ADR-14), as the reference `lua5.5` and `lua5.4` show it. The manual
//! allows any border, but code written for `lua5.4` often relies on the one
//! it gives for a table a constructor, `{...}` or `table.pack` built. The
//! differential tests (`tests/diff`) only run against `lua5.5`, so the 5.4
//! side is checked here.

use orbit::{Chunk, Config, Runtime};

fn run(src: &str) -> String {
    let mut iso = Runtime::new().new_isolate(Config::default());
    orbit_std::install(&mut iso, &orbit_std::Options::default()).unwrap();
    iso.scope(|s| {
        let out = s.load(Chunk::new(src.as_bytes()).name("=test"))?.call(())?;
        out[0].get()
    })
    .unwrap()
}

/// `#t` and the number of values `table.unpack(t)` gives, for each table
/// the chunk's `tables()` returns, as `#:count` separated by spaces.
fn lengths(tables: &str) -> String {
    run(&format!(
        r##"
        local function tables() {tables} end
        local out = {{}}
        for _, t in ipairs(tables()) do
            out[#out + 1] = #t .. ":" .. select("#", table.unpack(t))
        end
        return table.concat(out, " ")
        "##
    ))
}

#[test]
fn constructors_with_nils() {
    let got = lengths(
        r#"
        local x = nil
        local function g() return nil, "b", "c" end
        return {
            { nil, "b", "c" }, { nil, "b" }, { "a", nil, "c" }, { "a", "b", nil },
            { nil, nil, "c" }, { nil }, { x, "b", "c" }, { g() },
            { nil, "b", k = 1 }, { nil, "b", nil },
        }
        "#,
    );
    let want = if orbit::LUA.is_5_4() {
        "3:3 2:2 3:3 2:2 3:3 0:0 3:3 3:3 2:2 2:2"
    } else {
        "0:0 0:0 1:1 2:2 0:0 0:0 0:0 0:0 0:0 0:0"
    };
    assert_eq!(got, want);
}

#[test]
fn constructors_with_nils_after_values_5_4() {
    // `lua5.5` gives 1 and 3 for these, where the 5.5 build gives 4 and 0;
    // both are borders, and only the 5.4 build aims to match here.
    if !orbit::LUA.is_5_4() {
        return;
    }
    let got = lengths(
        r#"
        local function g() return nil, "b", "c" end
        return { { "z", g() }, { nil, "b", "c", nil } }
        "#,
    );
    assert_eq!(got, "4:4 3:3");
}

#[test]
fn varargs_and_table_pack_with_nils() {
    let got = lengths(
        r#"
        local function f(...) return { ... } end
        return {
            f(nil, "b", "c"), f(nil, nil), f("a", nil),
            table.pack(nil, "b", "c"), table.pack(nil, nil), table.pack("a", nil),
            f(nil, nil, nil, nil, nil, nil, nil, nil, nil, nil, nil, nil, nil,
              nil, nil, nil, nil, nil, nil, nil, nil, nil, nil, nil, nil, nil,
              nil, nil, nil, nil, nil, nil, nil, nil, nil, nil, nil, nil, nil,
              nil, nil, nil, nil, nil, nil, nil, nil, nil, nil, nil, "z"),
        }
        "#,
    );
    let want = if orbit::LUA.is_5_4() {
        "3:3 0:0 1:1 3:3 0:0 1:1 51:51"
    } else {
        "0:0 0:0 1:1 0:0 0:0 1:1 0:0"
    };
    assert_eq!(got, want);
}

#[test]
fn constructor_with_nils_then_stores() {
    // Stores into the positions the constructor gave keep `lua5.4`'s
    // length; `table.insert` appends after it.
    let got = lengths(
        r#"
        local a = { nil, "b", "c" }; a[4] = "d"
        local b = { nil, "b", "c" }; b[3] = nil
        local c = { nil, "b", "c" }; c[1] = "a"
        local d = { "a", "b", nil }; d[3] = "c"
        local e = { nil, "b", "c" }; table.insert(e, "d")
        local f = { 1, 1, 1, 1, 1 }; f[5] = nil; f[2] = nil
        return { a, b, c, d, e, f }
        "#,
    );
    let want = if orbit::LUA.is_5_4() {
        "4:4 2:2 3:3 3:3 4:4 4:4"
    } else {
        "4:4 0:0 3:3 3:3 3:3 1:1"
    };
    assert_eq!(got, want);
}

#[test]
fn constructor_keys_also_given_by_position() {
    // A positional item wins over an explicit key for the same index; the
    // table stays consistent either way.
    let got = run(r#"
        local t = { [2] = "x", [5] = "y", nil, "b", "c" }
        local keys = {}
        for k, v in pairs(t) do keys[#keys + 1] = k .. "=" .. v end
        table.sort(keys)
        return table.concat(keys, " ") .. " " .. tostring(t[2]) .. " " .. tostring(t[5])
    "#);
    assert_eq!(got, "2=b 3=c 5=y b y");
}
