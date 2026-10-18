//! `Config::memory_limit`: past the cap, scripts get a catchable
//! `not enough memory`; garbage is collected first; large allocations are
//! refused before they happen.

use orbit::{Chunk, Config, Runtime};

const CAP: usize = 16 << 20;

/// How far past the cap a script may go while it handles the error.
const GRACE: usize = 8;

#[test]
fn scripts_stay_under_the_cap() {
    let rt = Runtime::new();
    let mut iso = rt.new_isolate(Config::default());
    assert_eq!(iso.gc().limit(), None);
    drop(iso);
    let mut iso = rt.new_isolate(Config::default().memory_limit(Some(CAP)));
    assert_eq!(iso.gc().limit(), Some(CAP));
    // The script checks each case with `assert`.
    let n = iso
        .scope(|s| {
            let code = include_bytes!("memory_limit.lua");
            let f = s.load(Chunk::new(code).name("=memory_limit"))?;
            Ok::<_, orbit::Error>(f.call(())?.len())
        })
        .unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(n, 1);
    assert!(iso.gc().used_bytes() <= CAP + CAP / GRACE);
}

#[test]
fn no_cap_by_default() {
    let mut iso = Runtime::new().new_isolate(Config::default());
    let ok: bool = iso
        .scope(|s| {
            s.load(Chunk::new("return pcall(table.create, 1 << 22)"))?
                .call(())?[0]
                .get()
        })
        .unwrap();
    assert!(ok);
}
