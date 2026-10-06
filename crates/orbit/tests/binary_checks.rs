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
