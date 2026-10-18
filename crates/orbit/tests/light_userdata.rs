//! Light userdata through the embedding API: opaque tokens with identity
//! semantics.

use orbit::{Chunk, Config, Error, Runtime, Table, Value};

#[test]
fn light_userdata_compares_by_identity() {
    let mut iso = Runtime::new().new_isolate(Config::default());
    let text: Vec<String> = iso
        .scope(|s| {
            let g = s.globals();
            g.set(
                "token",
                s.create_function(|c| {
                    let n: i64 = c.check(0)?;
                    c.ret(Value::LightUserdata(n as u64))
                }),
            )?;
            g.set(
                "needs_table",
                s.create_function(|c| {
                    c.check::<Table>(0)?;
                    c.ret(())
                }),
            )?;
            let out = s
                .load(Chunk::new(
                    "local a, b, c = token(1), token(1), token(2)
                     local t = {[a] = 'one'}
                     local ok, msg = pcall(needs_table, a)
                     return type(a), a == b, a == c, t[b], t[c], msg,
                            tostring(a) == tostring(b), rawequal(a, b), a",
                ))?
                .call(())?;
            assert!(matches!(out[8], Value::LightUserdata(1)));
            out[..8]
                .iter()
                .map(|v| Ok(String::from_utf8_lossy(&v.to_display()?).into_owned()))
                .collect::<Result<Vec<_>, Error>>()
        })
        .unwrap();
    assert_eq!(
        text,
        [
            "userdata",
            "true",
            "false",
            "one",
            "nil",
            "bad argument #1 to 'needs_table' (table expected, got light userdata)",
            "true",
            "true",
        ]
    );
}
