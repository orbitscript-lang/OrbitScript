//! `load` checks binary chunks at the level `Isolate::set_binary_checks`
//! chooses, never the environment, and rejects VM-internal instructions at
//! every level.

use orbit::{BinaryChecks, Chunk, Compiler, Config, Error, Isolate, Language, Runtime, Value};
use orbit_compiler::bytecode::{MULTI, Op, format};

/// A binary chunk whose main function is `code`.
fn chunk_with(code: Vec<Op>) -> Vec<u8> {
    let mut chunk = Compiler
        .compile(b"local a = 1", "t", Language::Lua)
        .unwrap();
    chunk.protos[0].code = code;
    chunk.protos[0].debug = Default::default();
    format::encode(&chunk, true)
}

/// Whether `load` accepts `bytes`, and its message if not.
fn load(iso: &mut Isolate, bytes: &[u8]) -> Result<(), String> {
    iso.scope(|s| {
        s.globals().set("B", bytes)?;
        let r = s.load(Chunk::new("return load(B)"))?.call(())?;
        Ok::<_, Error>(match r[..] {
            [Value::Function(_)] => Ok(()),
            [Value::Nil, m] => Err(String::from_utf8_lossy(&m.to_display()?).into_owned()),
            _ => panic!("unexpected load results"),
        })
    })
    .unwrap()
}

fn isolate() -> Isolate {
    Runtime::new().new_isolate(Config::default())
}

#[test]
fn each_level_runs_its_layers() {
    // Fails only layer 2: no open producer sets `top` for the `Return`.
    let flow = chunk_with(vec![Op::Return {
        first: 0,
        count: MULTI,
    }]);
    // Fails layer 1: a register past the frame.
    let structure = chunk_with(vec![Op::Return1 { src: 200 }]);
    let mut iso = isolate();

    iso.set_binary_checks(Some(BinaryChecks::Full));
    let e = load(&mut iso, &flow).unwrap_err();
    assert!(e.contains("open operand without an open producer"), "{e}");
    let e = load(&mut iso, &structure).unwrap_err();
    assert!(e.contains("bad binary format"), "{e}");

    iso.set_binary_checks(Some(BinaryChecks::Structure));
    assert_eq!(load(&mut iso, &flow), Ok(()));
    assert!(load(&mut iso, &structure).is_err());

    iso.set_binary_checks(None);
    assert_eq!(load(&mut iso, &structure), Ok(()));
}

/// A native stub's body in a chunk would call any native by number, so
/// decoding rejects it even with verification off.
#[test]
fn native_entry_is_rejected_at_every_level() {
    let forged = chunk_with(vec![Op::NativeEntry { id: 0 }]);
    let mut iso = isolate();
    for level in [
        Some(BinaryChecks::Full),
        Some(BinaryChecks::Structure),
        None,
    ] {
        iso.set_binary_checks(level);
        let e = load(&mut iso, &forged).unwrap_err();
        assert!(e.contains("VM-internal instruction"), "{level:?}: {e}");
    }
}

/// `ORBIT_VERIFY` is the `orbit` CLI's: an embedded isolate keeps its level
/// whatever the host process's environment holds.
#[test]
fn the_environment_cannot_lower_the_level() {
    std::env::set_var("ORBIT_VERIFY", "none");
    let structure = chunk_with(vec![Op::Return1 { src: 200 }]);
    assert!(load(&mut isolate(), &structure).is_err());
}

#[test]
fn scripts_cannot_lower_the_level() {
    let structure = chunk_with(vec![Op::Return1 { src: 200 }]);
    let mut iso = isolate();
    iso.set_binary_checks(Some(BinaryChecks::Structure));
    // The level once lived in the registry under this key.
    iso.exec(Chunk::new("debug.getregistry()._ORBIT_VERIFY = 0"))
        .unwrap();
    assert!(load(&mut iso, &structure).is_err());
}

#[test]
fn variables_to_close_stay_balanced() {
    let t = Op::LoadBool {
        dst: 0,
        value: true,
    };
    let cases = [
        (
            vec![t, Op::Tbc { reg: 0 }, Op::Close { from: 0 }, Op::Return0],
            None,
        ),
        (
            vec![t, Op::Tbc { reg: 0 }, Op::Return0],
            Some("return with a variable left to close"),
        ),
        (
            vec![
                t,
                Op::Tbc { reg: 0 },
                Op::LoadInt { dst: 0, value: 1 },
                Op::Close { from: 0 },
                Op::Return0,
            ],
            Some("write to a variable to close"),
        ),
        (
            vec![
                t,
                Op::JumpIfTrue { src: 0, off: 1 },
                Op::Tbc { reg: 0 },
                Op::Close { from: 0 },
                Op::Return0,
            ],
            Some("paths join with different variables to close"),
        ),
    ];
    let mut iso = isolate();
    iso.set_binary_checks(Some(BinaryChecks::Full));
    for (code, want) in cases {
        let r = load(&mut iso, &chunk_with(code.clone()));
        match want {
            None => assert_eq!(r, Ok(()), "{code:?}"),
            Some(w) => {
                let e = r.unwrap_err();
                assert!(e.contains(w), "{code:?}: {e}");
            }
        }
    }
}

/// A binary chunk whose main function is `code`, with `nregs` registers and
/// `nparams` parameters.
fn chunk_with_frame(nregs: u8, nparams: u8, code: Vec<Op>) -> Vec<u8> {
    let mut chunk = Compiler
        .compile(b"local a = 1", "t", Language::Lua)
        .unwrap();
    let p = &mut chunk.protos[0];
    p.code = code;
    p.nregs = nregs;
    p.nparams = nparams;
    p.is_vararg = false;
    p.debug = Default::default();
    format::encode(&chunk, true)
}

/// A register no path has written holds whatever an earlier frame left on
/// the VM stack, so reading one would hand a chunk values it was never
/// given. Full verification rejects every such read.
#[test]
fn registers_are_written_before_they_are_read() {
    let one = |dst| Op::LoadInt { dst, value: 1 };
    let call = |func, nargs, nresults| Op::Call {
        func,
        nargs,
        nresults,
    };
    let cases = [
        // Parameters hold values on entry.
        (1, vec![Op::Return1 { src: 0 }], None),
        (0, vec![one(0), Op::Return { first: 0, count: 2 }], Some(())),
        (0, vec![Op::Return1 { src: 0 }], Some(())),
        (
            0,
            vec![one(0), Op::Move { dst: 1, src: 2 }, Op::Return0],
            Some(()),
        ),
        // Written on one path only.
        (
            1,
            vec![
                Op::JumpIfTrue { src: 0, off: 1 },
                one(1),
                Op::Return1 { src: 1 },
            ],
            Some(()),
        ),
        // Written on both paths.
        (
            1,
            vec![
                Op::JumpIfTrue { src: 0, off: 2 },
                one(1),
                Op::Jump { off: 1 },
                one(1),
                Op::Return1 { src: 1 },
            ],
            None,
        ),
        // A call's results hold values; what its callee left above them doesn't.
        (
            1,
            vec![one(1), call(0, 0, 2), Op::Return { first: 0, count: 2 }],
            None,
        ),
        (
            1,
            vec![
                one(1),
                one(2),
                call(0, 0, 1),
                Op::Return { first: 0, count: 3 },
            ],
            Some(()),
        ),
        (1, vec![call(0, 0, MULTI), Op::Return1 { src: 0 }], Some(())),
        // A numeric loop that never runs leaves its control variable unset.
        (
            3,
            vec![
                Op::ForPrep { base: 0, exit: 1 },
                Op::ForLoop { base: 0, back: -1 },
                Op::Return1 { src: 3 },
            ],
            Some(()),
        ),
        // The stale-register chunk the verifier once accepted.
        (
            0,
            vec![Op::Return {
                first: 1,
                count: 240,
            }],
            Some(()),
        ),
    ];
    let mut iso = isolate();
    iso.set_binary_checks(Some(BinaryChecks::Full));
    for (nparams, code, reject) in cases {
        let r = load(&mut iso, &chunk_with_frame(250, nparams, code.clone()));
        match reject {
            None => assert_eq!(r, Ok(()), "{code:?}"),
            Some(()) => {
                let e = r.unwrap_err();
                assert!(
                    e.contains("read of a register no path has written"),
                    "{code:?}: {e}"
                );
            }
        }
    }
}
