//! OrbitScript and Lua code in the same VM.

use orbit::{Chunk, Config, Language, Runtime};

#[test]
fn orbitscript_and_lua_chunks_share_globals() {
    let mut iso = Runtime::new().new_isolate(Config::default());
    iso.exec(Chunk::new("shared = { n = 41 }").name("=lua"))
        .unwrap();
    let n: i64 = iso
        .scope(|s| {
            let f = s.load(
                Chunk::new("extern shared;\nshared.n += 1;\nshared.n")
                    .name("=orb")
                    .language(Language::OrbitScript),
            )?;
            f.call(())?[0].get()
        })
        .unwrap();
    assert_eq!(n, 42);
    // By its name, `@x.orb` is OrbitScript without saying so.
    let err = iso
        .exec(Chunk::new("let = 1;").name("@orb.orb"))
        .unwrap_err();
    assert_eq!(err.to_string(), "orb.orb:1: expected a pattern, found `=`");
}
