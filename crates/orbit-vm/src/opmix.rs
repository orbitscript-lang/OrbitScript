//! SPIKE: dynamic opcode and opcode-pair counts, dumped at exit.
use std::cell::RefCell;
use std::collections::HashMap;

use orbit_bytecode::Op;

thread_local! {
    static MIX: RefCell<(HashMap<&'static str, u64>, HashMap<(&'static str, &'static str), u64>, &'static str)> =
        RefCell::new((HashMap::new(), HashMap::new(), "-"));
}

fn name(op: &Op) -> &'static str {
    let s = format!("{op:?}");
    let n = s.split([' ', '{', '(']).next().unwrap_or("?").to_string();
    Box::leak(n.into_boxed_str())
}

thread_local! { static NAMES: RefCell<HashMap<std::mem::Discriminant<Op>, &'static str>> = RefCell::new(HashMap::new()); }

pub fn count(op: &Op) {
    let n = NAMES.with(|m| *m.borrow_mut().entry(std::mem::discriminant(op)).or_insert_with(|| name(op)));
    MIX.with(|m| {
        let mut m = m.borrow_mut();
        *m.0.entry(n).or_default() += 1;
        let prev = m.2;
        *m.1.entry((prev, n)).or_default() += 1;
        m.2 = n;
    });
    TOTAL.with(|t| {
        let v = t.get() + 1;
        t.set(v);
        if v % (1 << 20) == 0 {
            dump();
        }
    });
}

thread_local! { static TOTAL: std::cell::Cell<u64> = const { std::cell::Cell::new(0) }; }

pub fn dump() {
    let Ok(path) = std::env::var("ORBIT_OPMIX") else { return };
    MIX.with(|m| {
        let m = m.borrow();
        let mut out = String::new();
        let mut ops: Vec<_> = m.0.iter().collect();
        ops.sort_by(|a, b| b.1.cmp(a.1));
        for (k, v) in ops { out += &format!("op {k} {v}\n"); }
        let mut pairs: Vec<_> = m.1.iter().collect();
        pairs.sort_by(|a, b| b.1.cmp(a.1));
        for ((a, b), v) in pairs.iter().take(60) { out += &format!("pair {a} {b} {v}\n"); }
        std::fs::write(path, out).ok();
    });
}
