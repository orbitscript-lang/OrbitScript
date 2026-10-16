//! SPIKE: a small `lua`-like program written only against `orbit-api`, so
//! the same source builds with the runtime linked statically or through
//! `liborbit`.
//!
//!   orbit-mini [-e chunk]... [script [args...]]
//!   orbit-mini --bench-boundary N    time crossings of the API boundary

use std::process::ExitCode;
use std::time::Instant;

use orbit_api::{Ctx, State, Val};

/// `host_add(a, b)`: a native the program provides.
fn host_add(ctx: &mut dyn Ctx) -> Result<usize, String> {
    match (ctx.arg(0), ctx.arg(1)) {
        (Val::Int(a), Val::Int(b)) => {
            ctx.push(&Val::Int(a.wrapping_add(b)));
            Ok(1)
        }
        _ => Err("host_add: two integers expected".into()),
    }
}

/// `host_apply(f, x)`: calls back into Lua.
fn host_apply(ctx: &mut dyn Ctx) -> Result<usize, String> {
    let f = ctx.arg(0);
    let x = ctx.arg(1);
    let out = ctx.call(&f, &[x])?;
    if let Val::Ref(_) = f {
        // References made for arguments are the program's to release; the
        // spike leaks these few.
    }
    let n = out.len();
    for v in &out {
        ctx.push(v);
    }
    Ok(n)
}

fn new_state() -> Result<State, String> {
    let mut st = State::new(true)?;
    st.register("host_add", host_add);
    st.register("host_apply", host_apply);
    Ok(st)
}

fn run(args: &[String]) -> Result<(), String> {
    let mut st = new_state()?;
    let mut i = 1;
    while i < args.len() && args[i] == "-e" {
        let chunk = args.get(i + 1).ok_or("-e needs an argument")?;
        let f = st.load(chunk.as_bytes(), "=(command line)")?;
        st.call(&f, &[])?;
        i += 2;
    }
    if let Some(script) = args.get(i) {
        let argt = st.new_table();
        for (n, a) in args[i..].iter().enumerate() {
            st.table_set(
                &argt,
                &Val::Int(n as i64),
                &Val::Str(a.clone().into_bytes()),
            );
        }
        st.set_global("arg", &argt);
        let src = std::fs::read(script).map_err(|e| format!("cannot open {script}: {e}"))?;
        let f = st.load(&src, &format!("@{script}"))?;
        let rest: Vec<Val> = args[i + 1..]
            .iter()
            .map(|a| Val::Str(a.clone().into_bytes()))
            .collect();
        st.call(&f, &rest)?;
    }
    Ok(())
}

fn ns(t: Instant, n: u64) -> f64 {
    t.elapsed().as_nanos() as f64 / n as f64
}

/// Each kind of crossing, `n` times.
fn bench(n: u64) -> Result<(), String> {
    let mut st = new_state()?;
    // Lua calling a program native.
    let f = st.load(
        b"local n = ... local add, s = host_add, 0 for i = 1, n do s = add(s, 1) end return s",
        "=bench",
    )?;
    let t = Instant::now();
    let r = st.call(&f, &[Val::Int(n as i64)])?;
    println!(
        "Lua -> native host_add      {:7.1} ns  ({:?})",
        ns(t, n),
        r[0]
    );
    // The program calling a Lua function.
    let g = st.load(b"return function(a, b) return a + b end", "=bench")?;
    let add = st.call(&g, &[])?.remove(0);
    let t = Instant::now();
    let mut s = 0;
    for i in 0..n {
        if let Val::Int(v) = st.call(&add, &[Val::Int(i as i64), Val::Int(1)])?[0] {
            s += v;
        }
    }
    println!("program -> Lua call         {:7.1} ns  ({s})", ns(t, n));
    // Globals.
    let t = Instant::now();
    for i in 0..n {
        st.set_global("x", &Val::Int(i as i64));
        let _ = st.global("x");
    }
    println!("set_global + global         {:7.1} ns", ns(t, n));
    // Strings both ways.
    let t = Instant::now();
    for _ in 0..n {
        st.set_global("s", &Val::Str(b"hello world".to_vec()));
        let _ = st.global("s");
    }
    println!("string global round trip    {:7.1} ns", ns(t, n));
    // A native calling back into Lua.
    let h = st.load(
        b"local n = ... local apply, inc, s = host_apply, function(x) return x + 1 end, 0 \
          for i = 1, n do s = apply(inc, s) end return s",
        "=bench",
    )?;
    let t = Instant::now();
    let r = st.call(&h, &[Val::Int(n as i64 / 10)])?;
    println!(
        "Lua -> native -> Lua        {:7.1} ns  ({:?})",
        ns(t, n / 10),
        r[0]
    );
    Ok(())
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let r = if args.get(1).map(String::as_str) == Some("--bench-boundary") {
        let n = args
            .get(2)
            .and_then(|s| s.parse().ok())
            .unwrap_or(1_000_000);
        bench(n)
    } else {
        run(&args)
    };
    match r {
        Ok(()) => ExitCode::SUCCESS,
        Err(m) => {
            eprintln!("orbit-mini: {m}");
            ExitCode::FAILURE
        }
    }
}
