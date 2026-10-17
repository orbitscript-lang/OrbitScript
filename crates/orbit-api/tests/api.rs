//! The embedding API on the static backend. The shared backend runs the
//! same calls through liborbit; `cargo testkit package` builds it, and the
//! suite runs through its `orbit`.

use orbit_api::{Error, Options, State, Value};

fn state() -> State {
    State::new(&Options::default()).expect("state")
}

#[test]
fn loads_and_calls() {
    let mut lua = state();
    let f = lua.load(b"return ... + 1, 'two'", b"=t").unwrap();
    let out = lua.call(&f, &[Value::Int(41)]).unwrap();
    assert_eq!(out, [Value::Int(42), Value::from("two")]);
}

#[test]
fn syntax_and_runtime_errors() {
    let mut lua = state();
    let e = lua.load(b"return 1 +", b"=t").unwrap_err();
    assert!(
        matches!(&e, Error::Syntax(m) if m.contains("<eof>")),
        "{e:?}"
    );
    let f = lua.load(b"error('boom')", b"=t").unwrap();
    match lua.call(&f, &[]).unwrap_err() {
        Error::Runtime { message, .. } => assert_eq!(message, "t:1: boom"),
        e => panic!("{e:?}"),
    }
    let e = lua.call_with_traceback(&f, &[]).unwrap_err().to_string();
    assert!(e.starts_with("t:1: boom\nstack traceback:"), "{e}");
}

#[test]
fn tables_and_globals() {
    let mut lua = state();
    let t = lua.new_table();
    lua.set(&t, &Value::Int(1), &Value::from("a")).unwrap();
    lua.set(&t, &Value::from("k"), &Value::Float(1.5)).unwrap();
    lua.set_global("t", &t);
    assert!(lua.set(&t, &Value::Nil, &Value::Int(1)).is_err());
    assert!(
        lua.set(&Value::Int(1), &Value::Int(1), &Value::Int(1))
            .is_err()
    );
    let f = lua.load(b"return #t, t[1], t.k", b"=t").unwrap();
    let out = lua.call(&f, &[]).unwrap();
    assert_eq!(out, [Value::Int(1), Value::from("a"), Value::Float(1.5)]);
    let g = lua.global("t");
    assert_eq!(lua.len(&g), 1);
    assert_eq!(lua.get(&g, &Value::from("k")), Value::Float(1.5));
    assert_eq!(lua.len(&Value::from("four")), 4);
}

#[test]
fn natives_call_back_into_lua() {
    let mut lua = state();
    let apply = lua.function("apply", |ctx| {
        let (f, x) = (ctx.arg(0), ctx.arg(1));
        let out = ctx.call(&f, &[x]).map_err(|e| e.to_string())?;
        for v in &out {
            ctx.push(v);
        }
        Ok(out.len())
    });
    lua.set_global("apply", &apply);
    let fail = lua.function("fail", |_| Err("no".into()));
    lua.set_global("fail", &fail);
    let f = lua
        .load(
            b"return apply(function(x) return x * 2 end, 21), pcall(fail)",
            b"=t",
        )
        .unwrap();
    let out = lua.call(&f, &[]).unwrap();
    assert_eq!(out[0], Value::Int(42));
    assert_eq!(out[1], Value::Bool(false));
    assert!(
        matches!(&out[2], Value::Str(s) if s.ends_with(b"no")),
        "{:?}",
        out[2]
    );
}

#[test]
fn handles_keep_objects_alive() {
    let mut lua = state();
    let f = lua.load(
        b"return setmetatable({}, {__tostring = function() return 'T' end})",
        b"=t",
    );
    let t = lua.call(&f.unwrap(), &[]).unwrap().remove(0);
    let g = lua
        .load(b"collectgarbage() collectgarbage()", b"=t")
        .unwrap();
    lua.call(&g, &[]).unwrap();
    assert_eq!(lua.to_string(&t).unwrap(), b"T");
    // Dropped handles are forgotten at the next call; the slot is reused.
    for _ in 0..1000 {
        let _ = lua.new_table();
    }
    lua.call(&g, &[]).unwrap();
}

#[test]
fn interrupts() {
    let mut lua = state();
    let i = lua.interrupt();
    i.request();
    assert!(i.pending());
    let f = lua.load(b"while true do end", b"=t").unwrap();
    let e = lua.call(&f, &[]).unwrap_err().to_string();
    assert!(e.contains("interrupted!"), "{e}");
}

#[test]
fn tools_compile_and_list() {
    let c = orbit_api::tools::compile(b"return 1", b"=t", false, true).unwrap();
    assert!(c.listing.unwrap().contains("Return"));
    let mut lua = state();
    let f = lua.load(&c.chunk, b"=t").unwrap();
    assert_eq!(lua.call(&f, &[]).unwrap(), [Value::Int(1)]);
    let hir = orbit_api::tools::hir(b"local x = 1", b"=t").unwrap();
    assert!(!hir.is_empty());
    assert_eq!(orbit_api::tools::chunk_id(b"@a.lua"), "a.lua");
}
