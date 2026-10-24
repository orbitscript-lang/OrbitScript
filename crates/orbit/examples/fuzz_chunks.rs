//! Mutation fuzzer for binary chunks and the verifier (a dev tool for the
//! verifier spike in docs/design/chunks-io-os-cli.md; not shipped).
//!
//! ```sh
//! cargo run --example fuzz_chunks -- [mutants] [seed]
//! ```
//!
//! Compiles every Lua file under `tests/` and `bench/` that compiles, then
//! makes `mutants` mutated chunks from them (operand and opcode changes,
//! swapped, duplicated and deleted instructions, changed register, cell and
//! cache counts, changed captures and constants). Each mutant runs in a child
//! process (this binary with `run`) at three verifier levels: none (decoded
//! and linked as is), structure only, and full. It reports how many each
//! level rejects, and how many of the accepted ones panic in the VM, grouped
//! by panic site. A child that runs past its time limit is a hang (bad
//! bytecode may legitimately loop forever) and is counted apart.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use orbit_compiler::bytecode::verify::{Level, verify};
use orbit_compiler::bytecode::{CaptureSource, Constant, UnlinkedChunk, format};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("run") {
        run_child(Path::new(&args[2]), &args[3]);
        return;
    }
    let mutants: usize = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(2000);
    let seed: u64 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(1);
    fuzz(mutants, seed);
}

/// Child: load the chunk at `path` with the given verifier level and run it.
/// Exit 0 when it ran or was rejected; a VM panic exits 101.
fn run_child(path: &Path, level: &str) {
    let bytes = std::fs::read(path).unwrap();
    let chunk = format::decode(&bytes).unwrap();
    let level = match level {
        "none" => None,
        "structure" => Some(Level::Structure),
        _ => Some(Level::Full),
    };
    if let Some(l) = level {
        if verify(&chunk, l).is_err() {
            std::process::exit(3);
        }
    }
    let mut iso = orbit::Runtime::new().new_isolate(orbit::Config::default());
    iso.scope(|s| {
        let _ = s.load_compiled(&chunk).call(());
    });
}

fn corpus() -> Vec<UnlinkedChunk> {
    sources()
        .iter()
        .filter_map(|s| {
            orbit::Compiler
                .compile(s, "fuzz", orbit::Language::Lua)
                .ok()
        })
        .collect()
}

/// The sources of every Lua file under `tests/` and `bench/`.
fn sources() -> Vec<Vec<u8>> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, out);
            } else if p.extension().is_some_and(|x| x == "lua") {
                out.push(p);
            }
        }
    }
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut files = Vec::new();
    walk(&root.join("tests"), &mut files);
    walk(&root.join("bench"), &mut files);
    files.sort();
    files.iter().filter_map(|f| std::fs::read(f).ok()).collect()
}

/// xorshift64*: no dependency, reproducible from the seed.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
}

/// Apply one to three random mutations to `c`.
fn mutate(c: &mut UnlinkedChunk, rng: &mut Rng) {
    for _ in 0..1 + rng.below(3) {
        let pi = rng.below(c.protos.len());
        let p = &mut c.protos[pi];
        let n = p.code.len();
        match rng.below(10) {
            // Change one byte of an instruction's encoding: an operand, or
            // the opcode (reinterpreting the operands).
            0..=4 => {
                let i = rng.below(n);
                let mut b = format::encode_op(p.code[i]);
                let j = rng.below(b.len());
                b[j] = match rng.below(3) {
                    0 => b[j] ^ (1 << rng.below(8)),
                    1 => b[j].wrapping_add(1),
                    _ => rng.next() as u8,
                };
                // Pad so a longer instruction can still decode.
                b.extend_from_slice(&[0; 16]);
                if let Ok(op) = format::decode_op(&b) {
                    p.code[i] = op;
                }
            }
            5 => {
                let (i, j) = (rng.below(n), rng.below(n));
                p.code.swap(i, j);
            }
            6 => {
                let i = rng.below(n);
                if rng.below(2) == 0 {
                    let op = p.code[rng.below(n)];
                    p.code.insert(i, op);
                } else if n > 1 {
                    p.code.remove(i);
                }
            }
            7 => match rng.below(4) {
                0 => p.nregs = p.nregs.wrapping_add(rng.below(5) as u8).wrapping_sub(2),
                1 => p.ncells = p.ncells.wrapping_sub(1),
                2 => p.ncaches = p.ncaches.wrapping_sub(1),
                _ => p.nparams = p.nparams.wrapping_add(1),
            },
            8 => {
                if p.captures.is_empty() {
                    p.captures.push(CaptureSource::Env);
                } else {
                    let i = rng.below(p.captures.len());
                    p.captures[i] = match rng.below(4) {
                        0 => CaptureSource::Env,
                        1 => CaptureSource::Cell(rng.below(4) as u8),
                        2 => CaptureSource::Capture(rng.below(4) as u8),
                        _ => {
                            p.captures.pop();
                            continue;
                        }
                    };
                }
            }
            _ => {
                if !p.consts.is_empty() {
                    let i = rng.below(p.consts.len());
                    p.consts[i] = match rng.below(3) {
                        0 => Constant::Int(rng.next() as i64),
                        1 => Constant::Float(f64::from_bits(rng.next())),
                        _ => Constant::Str(b"k".as_slice().into()),
                    };
                } else if !p.children.is_empty() {
                    p.children.pop();
                }
            }
        }
    }
}

#[derive(Default)]
struct Tally {
    rejected: usize,
    ran: usize,
    hangs: usize,
    crashes: usize,
    panics: BTreeMap<String, usize>,
}

fn fuzz(mutants: usize, seed: u64) {
    let corpus = corpus();
    println!("corpus: {} chunks", corpus.len());
    // Verifier cost over the unmutated corpus.
    let encoded: Vec<Vec<u8>> = corpus.iter().map(|c| format::encode(c, false)).collect();
    let bytes: usize = encoded.iter().map(Vec::len).sum();
    let reps = 50;
    for (name, level) in [
        ("decode", None),
        ("structure", Some(Level::Structure)),
        ("full", Some(Level::Full)),
    ] {
        let t = Instant::now();
        for _ in 0..reps {
            for (c, e) in corpus.iter().zip(&encoded) {
                match level {
                    None => drop(format::decode(e).unwrap()),
                    Some(l) => verify(c, l).unwrap(),
                }
            }
        }
        let per = t.elapsed() / reps;
        println!(
            "{name}: {:?} for {bytes} bytes ({:.1} MB/s)",
            per,
            bytes as f64 / per.as_secs_f64() / 1e6
        );
    }
    // And compiling the same sources, for scale.
    let sources = sources();
    let t = Instant::now();
    for _ in 0..reps {
        for s in &sources {
            let _ = orbit::Compiler.compile(s, "fuzz", orbit::Language::Lua);
        }
    }
    println!("compile: {:?}", t.elapsed() / reps);
    if mutants == 0 {
        return;
    }
    let exe = std::env::current_exe().unwrap();
    let dir = std::env::temp_dir().join(format!("orbit-fuzz-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let mut rng = Rng(seed.max(1));
    let levels = ["none", "structure", "full"];
    let mut tallies: [Tally; 3] = Default::default();
    for m in 0..mutants {
        let mut c = corpus[rng.below(corpus.len())].clone();
        mutate(&mut c, &mut rng);
        let path = dir.join(format!("m{}.bin", m % 64));
        std::fs::File::create(&path)
            .unwrap()
            .write_all(&format::encode(&c, false))
            .unwrap();
        for (li, level) in levels.iter().enumerate() {
            let t = &mut tallies[li];
            // The verifier decides in-process; only accepted mutants run.
            if li > 0 {
                let l = if li == 1 {
                    Level::Structure
                } else {
                    Level::Full
                };
                if verify(&c, l).is_err() {
                    t.rejected += 1;
                    continue;
                }
            }
            match run(&exe, &path, level) {
                Outcome::Ran => t.ran += 1,
                Outcome::Hang => t.hangs += 1,
                Outcome::Crash => t.crashes += 1,
                Outcome::Panic(site) => *t.panics.entry(site).or_default() += 1,
            }
        }
        if (m + 1) % 500 == 0 {
            eprintln!("{} mutants", m + 1);
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
    println!("\nmutants: {mutants} (seed {seed})");
    for (li, t) in tallies.iter().enumerate() {
        let panics: usize = t.panics.values().sum();
        println!(
            "\n{}: rejected {}, ran {}, hung {}, crashed {}, panicked {} at {} sites",
            levels[li],
            t.rejected,
            t.ran,
            t.hangs,
            t.crashes,
            panics,
            t.panics.len()
        );
        for (site, n) in &t.panics {
            println!("  {n:5}  {site}");
        }
    }
}

enum Outcome {
    Ran,
    Hang,
    Crash,
    Panic(String),
}

fn run(exe: &Path, path: &Path, level: &str) -> Outcome {
    let mut child = Command::new(exe)
        .args(["run".as_ref(), path.as_os_str(), level.as_ref()])
        .env("RUST_BACKTRACE", "0")
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let start = Instant::now();
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            let mut err = String::new();
            use std::io::Read;
            child
                .stderr
                .take()
                .unwrap()
                .read_to_string(&mut err)
                .unwrap();
            return match status.code() {
                Some(0) | Some(3) => Outcome::Ran,
                Some(101) => Outcome::Panic(panic_site(&err)),
                _ => Outcome::Crash,
            };
        }
        if start.elapsed() > Duration::from_secs(2) {
            let _ = child.kill();
            let _ = child.wait();
            return Outcome::Hang;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// `file:line: message` of the first panic in `stderr`.
fn panic_site(stderr: &str) -> String {
    let mut lines = stderr.lines();
    while let Some(l) = lines.next() {
        if let Some(rest) = l.split("panicked at ").nth(1) {
            // Group by site and the message's kind, not its numbers.
            let msg = lines.next().unwrap_or("");
            let msg = msg.split(": the len").next().unwrap_or(msg);
            let msg: String = msg.chars().take(70).collect();
            return format!("{} {msg}", rest.trim_end_matches(':'));
        }
    }
    "unknown".into()
}
