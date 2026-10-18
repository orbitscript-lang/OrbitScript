//! Userdata through the embedding API: a host type made a [`Class`] with
//! methods.

use orbit::{Chunk, Class, Config, Error, Isolate, Runtime, Scope, Table, Value};

#[derive(Debug)]
struct Counter {
    n: i64,
}

/// The `Counter` class: its metatable as global `Counter`, its `inc`
/// method as `counter_inc`, and `counter_new(start)` making one.
fn define<'s>(s: &'s Scope<'s>) -> Result<(), Error> {
    let class: Class<Counter> = s.define_class::<Counter>("Counter", |c| {
        c.method("inc", |me, call| {
            me.n += call.opt(1, 1)?;
            call.ret(me.n)
        })?;
        Ok(())
    })?;
    let meta = class.metatable(s)?;
    let methods: Table = meta.get("__index")?;
    s.globals().set("Counter", meta)?;
    s.globals()
        .set("counter_inc", methods.get::<Value>("inc")?)?;
    let new = s.create_function(move |c| {
        let start = c.opt(0, 0)?;
        let u = class.instance(c, Counter { n: start })?;
        c.ret(u)
    });
    s.globals().set("counter_new", new)?;
    Ok(())
}

fn isolate() -> Isolate {
    Runtime::new().new_isolate(Config::default())
}

#[test]
fn userdata_with_host_methods() {
    let mut iso = isolate();
    let shown: Vec<String> = iso
        .scope(|s| {
            define(s)?;
            let out = s
                .load(Chunk::new(
                    r#"
                    Counter.__eq = function (a, b) return a:inc(0) == b:inc(0) end
                    local c = counter_new(10)
                    c:inc(); c:inc(5)
                    local same = counter_new(16)
                    local ok, err = pcall(counter_inc, {})
                    return type(c), c:inc(0), c == same, c ~= counter_new(1),
                        tostring(c):sub(1, 9), err
                    "#,
                ))?
                .call(())?;
            out.iter()
                .map(|v| Ok(String::from_utf8_lossy(&v.to_display()?).into_owned()))
                .collect::<Result<Vec<_>, Error>>()
        })
        .unwrap();
    assert_eq!(
        shown,
        [
            "userdata",
            "16",
            "true",
            "true",
            "Counter: ",
            "bad argument #1 to 'counter_inc' (Counter expected, got table)"
        ]
    );
}

#[test]
fn userdata_survives_collections() {
    let mut iso = isolate();
    let sum: i64 = iso
        .scope(|s| {
            define(s)?;
            let out = s
                .load(Chunk::new(
                    r#"
                    local all = {}
                    for i = 1, 200 do
                        all[i] = counter_new(i); local _ = {}
                        if i % 10 == 0 then collectgarbage() end
                    end
                    collectgarbage()
                    local sum = 0
                    for i = 1, 200 do sum = sum + all[i]:inc() end
                    return sum
                    "#,
                ))?
                .call(())?;
            out[0].get()
        })
        .unwrap();
    assert_eq!(sum, 20300);
}
