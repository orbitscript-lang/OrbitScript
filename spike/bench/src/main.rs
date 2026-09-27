//! S0 benchmark driver: `spike <arena|gcarena> <match|super|closure> <bench> <n>`.
//! Prints the result, the in-process run time and the heap's GC cycle count.

use std::time::Instant;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 5 {
        eprintln!("usage: spike <arena|gcarena> <match|super|closure> <bench> <n>");
        std::process::exit(2);
    }
    let (vm, disp, bench) = (&args[1], &args[2], &args[3]);
    let n: i64 = args[4].parse().expect("n");
    let proto = spike_common::benches::build(bench, n);
    let t0 = Instant::now();
    let (result, cycles) = match vm.as_str() {
        "arena" => {
            use spike_vm_arena::{Dispatch, Value, Vm};
            let d = match disp.as_str() {
                "match" => Dispatch::Match,
                "super" => Dispatch::Super,
                "closure" => Dispatch::Closure,
                _ => panic!("dispatch"),
            };
            let mut vm = Vm::new(d);
            let main = vm.prepare(&proto);
            match vm.run(main) {
                Ok(Value::Int(i)) => (i, vm.heap.cycles),
                other => panic!("{other:?}"),
            }
        }
        "gcarena" => {
            let super_ = match disp.as_str() {
                "match" => false,
                "super" => true,
                _ => panic!("gcarena supports match|super"),
            };
            spike_vm_gcarena::run(&proto, super_).expect("run")
        }
        _ => panic!("vm"),
    };
    let dt = t0.elapsed();
    eprintln!("time={}ms gc_cycles={cycles}", dt.as_millis());
    println!("{result}");
}
