//! Library behaviour that differs between the Lua 5.5 build and the Lua
//! 5.4 one (ADR-14), as the reference `lua5.5` and `lua5.4` show it. The
//! differential tests (`tests/diff`) only run against `lua5.5`, so the 5.4
//! side of each difference is checked here.

use orbit::{Chunk, Config, Runtime};

fn run(src: &str) -> String {
    let mut iso = Runtime::new().new_isolate(Config::default());
    iso.scope(|s| {
        let out = s.load(Chunk::new(src.as_bytes()).name("=test"))?.call(())?;
        out[0].get()
    })
    .unwrap()
}

#[test]
fn utf8_offset_at_a_continuation_byte() {
    // With n = 0, Lua 5.5 fails where the search ends on a continuation
    // byte; Lua 5.4 gives the position. Lua 5.5 also gives the last byte.
    let got = run(r#"
        local out = {}
        for _, a in ipairs{{"\x80abc", 0}, {"\x80\x80a", 0, 2}, {"a\x80", 0, 2}, {"\x80", 0}} do
            local r = table.pack(pcall(utf8.offset, table.unpack(a)))
            for i = 1, r.n do out[#out + 1] = tostring(r[i]) end
            out[#out + 1] = "|"
        end
        return table.concat(out, " ")
    "#);
    let want = if orbit::LUA.is_5_4() {
        "true 1 | true 1 | true 1 | true 1 |"
    } else {
        "false initial position is a continuation byte | \
         false initial position is a continuation byte | \
         true 1 1 | \
         false initial position is a continuation byte |"
    };
    assert_eq!(got, want);
}
