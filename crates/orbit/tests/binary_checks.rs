//! `load` checks binary chunks at the level the build and `ORBIT_VERIFY`
//! (or `checks::set_level`) choose.

use orbit::checks::set_level;
use orbit::{Lua, Value};
use orbit_compiler::bytecode::verify::Level;
use orbit_compiler::bytecode::{MULTI, Op, format};
use orbit_lua_syntax::ParseOptions;

/// A binary chunk whose main function is `code`.
fn chunk_with(code: Vec<Op>) -> Vec<u8> {
    let mut chunk = orbit::compile(b"local a = 1", "t", &ParseOptions::default()).unwrap();
    chunk.protos[0].code = code;
    chunk.protos[0].debug = Default::default();
    format::encode(&chunk, true)
}

/// Whether `load` accepts `bytes`, and its message if not.
fn load(lua: &mut Lua, bytes: &[u8]) -> Result<(), String> {
    let s = lua.vm().new_str(bytes);
    lua.vm().set_global("B", s);
    let r = lua.exec(b"return load(B)", "t").unwrap();
    match r[..] {
        [Value::Closure(_)] => Ok(()),
        [Value::Nil, m] => Err(String::from_utf8_lossy(&lua.vm().to_display(m)).into_owned()),
        _ => panic!("unexpected load results"),
    }
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
    let mut lua = Lua::new();

    set_level(lua.vm(), Some(Level::Full));
    let e = load(&mut lua, &flow).unwrap_err();
    assert!(e.contains("open operand without an open producer"), "{e}");
    let e = load(&mut lua, &structure).unwrap_err();
    assert!(e.contains("bad binary format"), "{e}");

    set_level(lua.vm(), Some(Level::Structure));
    assert_eq!(load(&mut lua, &flow), Ok(()));
    assert!(load(&mut lua, &structure).is_err());

    set_level(lua.vm(), None);
    assert_eq!(load(&mut lua, &structure), Ok(()));
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
    let mut lua = Lua::new();
    set_level(lua.vm(), Some(Level::Full));
    for (code, want) in cases {
        let r = load(&mut lua, &chunk_with(code.clone()));
        match want {
            None => assert_eq!(r, Ok(()), "{code:?}"),
            Some(w) => {
                let e = r.unwrap_err();
                assert!(e.contains(w), "{code:?}: {e}");
            }
        }
    }
}
