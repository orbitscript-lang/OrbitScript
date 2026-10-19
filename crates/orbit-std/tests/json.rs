//! The `std.json` module.
#![cfg(feature = "json")]

use orbit::{Chunk, Config, Error, Isolate, Runtime};

fn isolate() -> Isolate {
    let mut iso = Runtime::new().new_isolate(Config::default());
    orbit_std::install(&mut iso, &orbit_std::Options::default()).unwrap();
    iso
}

/// Run Lua `src` with `json` in scope and give its results as text.
fn lua(src: &str) -> Result<Vec<String>, String> {
    run(Chunk::new(&format!("local json = require 'std.json'\n{src}")).name("=test"))
}

fn run(chunk: Chunk<'_>) -> Result<Vec<String>, String> {
    let mut iso = isolate();
    iso.scope(|s| {
        let values = s.load(chunk)?.call(())?;
        values
            .iter()
            .map(|v| Ok(String::from_utf8_lossy(&v.to_display()?).into_owned()))
            .collect::<Result<Vec<_>, Error>>()
    })
    .map_err(|e| e.to_string())
}

fn one(src: &str) -> String {
    let mut v = lua(src).unwrap();
    assert_eq!(v.len(), 1, "{v:?}");
    v.pop().unwrap()
}

#[test]
fn scalars_encode() {
    assert_eq!(one("return json.encode(nil)"), "null");
    assert_eq!(one("return json.encode(json.null)"), "null");
    assert_eq!(
        one("return json.encode(true) .. json.encode(false)"),
        "truefalse"
    );
    assert_eq!(one("return json.encode(42)"), "42");
    assert_eq!(one("return json.encode(-7)"), "-7");
    assert_eq!(one("return json.encode(1.0)"), "1.0");
    assert_eq!(one("return json.encode(0.1)"), "0.1");
    assert_eq!(one("return json.encode(1e300)"), "1e+300");
    assert_eq!(
        one("return json.encode(math.mininteger)"),
        "-9223372036854775808"
    );
    assert_eq!(
        one(r#"return json.encode("a\"b\\c\n\1é")"#),
        r#""a\"b\\c\n\u0001é""#
    );
}

#[test]
fn tables_encode_as_arrays_or_objects() {
    assert_eq!(one("return json.encode({})"), "[]");
    assert_eq!(
        one("return json.encode({1, 'two', {3}})"),
        r#"[1,"two",[3]]"#
    );
    assert_eq!(one("return json.encode({a = 1})"), r#"{"a":1}"#);
    // Keys that aren't exactly 1..n make an object, with numbers as text.
    assert_eq!(
        one("return json.encode({[1] = 'a', [3] = 'c'}, {sort_keys = true})"),
        r#"{"1":"a","3":"c"}"#
    );
    assert_eq!(
        one("return json.encode({1, 2, x = true}, {sort_keys = true})"),
        r#"{"1":1,"2":2,"x":true}"#
    );
    assert_eq!(one("return json.encode({[1.5] = 0})"), r#"{"1.5":0}"#);
    assert_eq!(one("return json.encode({json.null, 2})"), "[null,2]");
}

#[test]
fn markers_choose_the_shape() {
    assert_eq!(one("return json.encode(json.object())"), "{}");
    assert_eq!(one("return json.encode(json.array())"), "[]");
    assert_eq!(
        one("return json.encode(json.object({1, 2}))"),
        r#"{"1":1,"2":2}"#
    );
    assert_eq!(
        one("return json.encode({items = json.array({})})"),
        r#"{"items":[]}"#
    );
    // A class can ask for its own shape.
    assert_eq!(
        one("return json.encode(setmetatable({}, {__jsontype = 'object'}))"),
        "{}"
    );
    // Marking twice is fine; a table with another metatable is refused.
    assert_eq!(one("return json.encode(json.object(json.object()))"), "{}");
    let e = lua("return json.object(setmetatable({}, {}))").unwrap_err();
    assert!(e.contains("already has a metatable"), "{e}");
    let e = lua("return json.encode(json.array({x = 1}))").unwrap_err();
    assert!(e.contains("marked as an array"), "{e}");
}

#[test]
fn tojson_gives_the_value_to_write() {
    let src = r#"
        local Point = {}
        Point.__index = Point
        function Point.__tojson(p) return json.array({p.x, p.y}) end
        local p = setmetatable({x = 3, y = 4}, Point)
        local Wrap = {__tojson = function(w) return w.inner end}
        return json.encode({p, setmetatable({inner = p}, Wrap)}),
            json.encode(setmetatable({}, {__tojson = function() return json.null end}))
    "#;
    assert_eq!(lua(src).unwrap(), ["[[3,4],[3,4]]", "null"]);
    // A hook that gives back its own value would never end.
    let e = lua(
        "local t = setmetatable({}, {__tojson = function(t) return json.array({t}) end}) return json.encode(t)",
    )
    .unwrap_err();
    assert!(
        e.contains("cannot encode a table that contains itself"),
        "{e}"
    );
    // A table it gives must say its shape.
    let e = lua("return json.encode(setmetatable({}, {__tojson = function() return {1} end}))")
        .unwrap_err();
    assert!(
        e.contains("__tojson returned a table that is not json.array"),
        "{e}"
    );
    assert_eq!(
        one(
            "return json.encode(setmetatable({}, {__tojson = function() return json.object({}) end}))"
        ),
        "{}"
    );
    // Its errors are the caller's.
    let e = lua("return json.encode(setmetatable({}, {__tojson = function() error('no') end}))")
        .unwrap_err();
    assert!(e.contains("no"), "{e}");
    // What it gives must be encodable, and the path says where.
    let e =
        lua("return json.encode({a = setmetatable({}, {__tojson = function() return print end})})")
            .unwrap_err();
    assert!(e.contains("cannot encode a function at value.a"), "{e}");
}

#[test]
fn arrays_with_holes_and_a_length() {
    // A marked array writes its holes as null, up to its largest key or the
    // length it was given.
    assert_eq!(
        one("return json.encode(json.array({1, nil, 3}))"),
        "[1,null,3]"
    );
    assert_eq!(
        one("return json.encode(json.array({[2] = 'b'}, 4))"),
        "[null,\"b\",null,null]"
    );
    assert_eq!(one("return json.encode(json.array({}, 0))"), "[]");
    assert_eq!(one("return json.encode(json.array(nil, 2))"), "[null,null]");
    let e = lua("return json.encode(json.array({1, 2, 3}, 2))").unwrap_err();
    assert!(
        e.contains("array of length 2 with an element at [3]"),
        "{e}"
    );
    let e = lua("return json.encode(json.array({x = 1}))").unwrap_err();
    assert!(e.contains("keys aren't positive integers"), "{e}");
    let e = lua("return json.array({}, -1)").unwrap_err();
    assert!(e.contains("length must not be negative"), "{e}");
    // Unmarked, a table with holes is still an object.
    assert_eq!(one("return json.encode({1, nil, 3})"), "{\"1\":1,\"3\":3}");
}

#[test]
fn scalar_conversions() {
    let src = r#"
        return json.encode({json.int(2.0), json.int(-7), json.float(2), json.float(1000),
            json.string(2), json.string(1.5), json.string(true), json.string("s"),
            json.bool(0), json.bool(""), json.bool(nil), json.bool(false), json.bool(json.null),
            json.string(setmetatable({}, {__tostring = function() return "custom" end}))})
    "#;
    assert_eq!(
        lua(src).unwrap(),
        [r#"[2,-7,2.0,1000.0,"2","1.5","true","s",true,true,false,false,true,"custom"]"#]
    );
    // json.null is a value like any other to `as bool`: only nil and false are false.
    // A plain conversion says the same thing.
    assert_eq!(
        one("return json.encode({math.tointeger(2.0), 2 + 0.0, tostring(2)})"),
        r#"[2,2.0,"2"]"#
    );
    for (call, message) in [
        ("json.int(2.5)", "no integer representation"),
        (
            "json.int('7')",
            "bad argument #1 to 'int' (cannot cast string to int)",
        ),
        ("json.float('1')", "cannot cast string to float"),
        ("json.int({})", "cannot cast table to int"),
        ("json.string({})", "cannot cast table to string"),
        ("json.string(nil)", "cannot cast nil to string"),
        ("json.int()", "value expected"),
    ] {
        let e = lua(&format!("return {call}")).unwrap_err();
        assert!(e.contains(message), "{call}: {e}");
    }
}

#[test]
fn numbers_written_exactly() {
    let src = r#"
        local id = json.number("12345678901234567890")
        return json.encode({id = id, price = json.number("19.90"), e = json.number("-1.5e-7")},
            {sort_keys = true}), tostring(id)
    "#;
    assert_eq!(
        lua(src).unwrap(),
        [
            r#"{"e":-1.5e-7,"id":12345678901234567890,"price":19.90}"#,
            "12345678901234567890"
        ]
    );
    for bad in [
        "'1.'", "'01'", "'+1'", "' 1'", "'1e'", "'NaN'", "''", "'0x10'",
    ] {
        let e = lua(&format!("return json.number({bad})")).unwrap_err();
        assert!(e.contains("not a JSON number"), "{bad}: {e}");
    }
}

#[test]
fn decode_can_drop_nulls() {
    let src = r#"
        local v = json.decode('{"a": null, "b": [1, null, 3, null]}', {keep_null = false})
        assert(v.a == nil and v.b[2] == nil and v.b[3] == 3)
        -- The array keeps its length, so it encodes back the same.
        return json.encode(v.b), tostring(json.decode('null', {keep_null = false}))
    "#;
    assert_eq!(lua(src).unwrap(), ["[1,null,3,null]", "nil"]);
    assert_eq!(one("return json.decode('[null]')[1] == json.null"), "true");
}

#[test]
fn sort_keys_orders_members_by_bytes() {
    assert_eq!(
        one(
            "return json.encode({b = 1, a = {d = 2, c = 3}, [10] = 0, [9] = 0}, {sort_keys = true})"
        ),
        r#"{"10":0,"9":0,"a":{"c":3,"d":2},"b":1}"#
    );
}

#[test]
fn pretty_indents_by_two_spaces() {
    assert_eq!(
        one(
            "return json.encode({a = {1, 2}, b = {}, c = json.object()}, {pretty = true, sort_keys = true})"
        ),
        "{\n  \"a\": [\n    1,\n    2\n  ],\n  \"b\": [],\n  \"c\": {}\n}"
    );
}

#[test]
fn what_json_cannot_hold_is_an_error_naming_where() {
    let cases = [
        ("print", "cannot encode a function"),
        (
            "{1, {x = {print}}}",
            "cannot encode a function at value[2].x[1]",
        ),
        ("{['a b'] = 0/0}", r#"cannot encode NaN at value["a b"]"#),
        ("{x = 1/0}", "cannot encode inf at value.x"),
        ("{[true] = 1}", "cannot encode a key of type boolean"),
        ("'\\255'", "cannot encode a string that isn't valid UTF-8"),
        ("coroutine.create(print)", "cannot encode a thread"),
    ];
    for (value, message) in cases {
        let e = lua(&format!("return json.encode({value})")).unwrap_err();
        assert!(e.contains(message), "{value}: {e}");
    }
    let e = lua("local t = {}; t.self = t; return json.encode(t)").unwrap_err();
    assert!(e.contains("contains itself at value.self"), "{e}");
    let e = lua("local t = {}; for i = 1, 200 do t = {t} end; return json.encode(t)").unwrap_err();
    assert!(e.contains("nested over 128 deep"), "{e}");
    // The same table twice, not nested, is fine.
    assert_eq!(
        one("local t = {1}; return json.encode({t, t})"),
        "[[1],[1]]"
    );
}

#[test]
fn decode_builds_values() {
    let src = r#"
        local v = json.decode('{"a": [1, 2.0, -3e2, true, null, "x\\u00e9"], "b": {}, "c": [], "": 0}')
        assert(math.type(v.a[1]) == 'integer' and v.a[1] == 1)
        assert(math.type(v.a[2]) == 'float' and v.a[2] == 2.0)
        assert(v.a[3] == -300.0 and math.type(v.a[3]) == 'float')
        assert(v.a[4] == true and v.a[5] == json.null and v.a[6] == 'xé')
        assert(#v.a == 6 and v[''] == 0)
        assert(next(v.b) == nil and next(v.c) == nil)
        return json.encode(v.b), json.encode(v.c), tostring(json.null)
    "#;
    assert_eq!(lua(src).unwrap(), ["{}", "[]", "null"]);
    assert_eq!(
        one("return json.decode('18446744073709551615')"),
        "1.8446744073709552e+19"
    );
    assert_eq!(
        one("return math.type(json.decode('9223372036854775807'))"),
        "integer"
    );
    assert_eq!(one("return json.decode('  \"s\"  ')"), "s");
}

#[test]
fn decode_reports_bad_text() {
    let cases = [
        ("'{\"a\": }'", "expected value at line 1 column 7"),
        ("'[1, 2'", "EOF while parsing a list"),
        ("'1 2'", "trailing characters"),
        ("''", "EOF while parsing a value"),
        ("'{1: 2}'", "key must be a string"),
        (
            "string.rep('[', 200) .. string.rep(']', 200)",
            "recursion limit exceeded",
        ),
    ];
    for (text, message) in cases {
        let v = lua(&format!("return json.decode({text})")).unwrap();
        assert_eq!(v[0], "nil", "{text}");
        assert!(v[1].contains(message), "{text}: {}", v[1]);
    }
    let e = lua("return json.decode({})").unwrap_err();
    assert!(e.contains("bad argument #1 to 'decode'"), "{e}");
}

#[test]
fn values_round_trip() {
    let src = r#"
        local text = '{"id":7,"list":[1,2.5,null,{},[]],"method":"textDocument/hover","params":{"position":{"character":4,"line":12},"textDocument":{"uri":"file:///a.orb"}}}'
        local again = json.encode(json.decode(text), {sort_keys = true})
        assert(again == text, again)
        return again
    "#;
    lua(src).unwrap();
}

#[test]
fn null_is_read_only() {
    let e = lua("json.null.x = 1").unwrap_err();
    assert!(e.contains("json.null is read-only"), "{e}");
}

#[test]
fn not_a_global() {
    assert_eq!(one("return tostring(rawget(_G, 'json'))"), "nil");
}

#[test]
fn orbitscript_uses_it() {
    let src = "use std::json;\n\
               let v = json::decode(\"[1, {\\\"k\\\": \\\"v\\\"}]\");\n\
               json::encode(v)";
    let v = run(Chunk::new(src)
        .name("=test.orb")
        .language(orbit::Language::OrbitScript))
    .unwrap();
    assert_eq!(v, [r#"[1,{"k":"v"}]"#]);
}
