//! `Config::fixed_hash_seed`: isolates given the same seed hash strings and
//! table keys alike, so a table's traversal order (and the work behind it)
//! repeats; the default seed is random per isolate.

use orbit::{Chunk, Config, Runtime};

/// The order `pairs` visits a table with string, integer and float keys.
const ORDER: &str = "
    local t = {}
    for i = 1, 64 do
        t['k' .. i] = true
        t[i * 1000003] = true
        t[i + 0.5] = true
    end
    local keys = ''
    for k in pairs(t) do keys = keys .. ' ' .. tostring(k) end
    return keys
";

fn order(config: Config) -> String {
    let mut iso = Runtime::new().new_isolate(config);
    iso.scope(|s| {
        let out = s.load(Chunk::new(ORDER).name("=order"))?.call(())?;
        Ok::<_, orbit::Error>(String::from_utf8_lossy(&out[0].to_display()?).into_owned())
    })
    .unwrap_or_else(|e| panic!("{e}"))
}

#[test]
fn a_fixed_seed_repeats_the_order() {
    let a = order(Config::default().fixed_hash_seed(42));
    assert_eq!(a, order(Config::default().fixed_hash_seed(42)));
    assert_ne!(a, order(Config::default().fixed_hash_seed(43)));
}

#[test]
fn the_default_seed_differs_between_isolates() {
    // 192 keys in another order by chance is all but impossible.
    assert_ne!(order(Config::default()), order(Config::default()));
}
