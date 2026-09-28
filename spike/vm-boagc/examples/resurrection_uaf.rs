//! boa_gc 0.22: resurrecting an object from `Finalize` corrupts its children's
//! reference counts. 100% safe code.
#![forbid(unsafe_code)]
use boa_gc::{Finalize, Gc, GcRefCell, Trace, WeakGc, force_collect};
use std::cell::RefCell;

// Run with `cargo run -p spike-vm-boagc --example resurrection_uaf`: debug builds
// panic with "attempt to subtract with overflow" in `GcHeader::dec_ref_count`.

#[derive(Trace)]
struct Obj {
    child: Gc<GcRefCell<i64>>,
    this: GcRefCell<Option<WeakGc<Obj>>>,
}

thread_local!(static SAVED: RefCell<Vec<Gc<Obj>>> = const { RefCell::new(Vec::new()) });

impl Finalize for Obj {
    fn finalize(&self) {
        // Resurrect: keep a strong handle to the dying object.
        if let Some(me) = self.this.borrow().as_ref().and_then(WeakGc::upgrade) {
            SAVED.with(|s| s.borrow_mut().push(me));
        }
    }
}

fn main() {
    let obj = Gc::new(Obj {
        child: Gc::new(GcRefCell::new(7)),
        this: GcRefCell::new(None),
    });
    *obj.this.borrow_mut() = Some(WeakGc::new(&obj));
    drop(obj);
    force_collect(); // obj unreachable: finalized and resurrected
    let obj = SAVED.with(|s| s.borrow_mut().pop()).expect("resurrected");
    println!("child after resurrection: {}", obj.child.borrow());
    drop(obj);
    force_collect(); // child's count was already decremented once
    for _ in 0..1000 {
        let _junk = Gc::new(GcRefCell::new([0u64; 4]));
    }
    force_collect();
    println!("survived");
}
