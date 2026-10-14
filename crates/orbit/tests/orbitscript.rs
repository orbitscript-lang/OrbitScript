//! OrbitScript files through `loadfile`, `dofile` and `require`, next to Lua
//! code in the same VM.

use std::path::PathBuf;

use orbit::{Lua, Value};

/// A fresh directory under the target directory, holding `files`.
fn dir_with(test: &str, files: &[(&str, &str)]) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("orbitscript-{test}"));
    let _ = std::fs::remove_dir_all(&dir);
    for (name, text) in files {
        let path = dir.join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }
    dir
}

fn exec_str(lua: &mut Lua, src: &str) -> Vec<String> {
    let values = lua.exec(src.as_bytes(), "=test").unwrap();
    values
        .iter()
        .map(|v| String::from_utf8_lossy(&lua.vm().to_display(*v)).into_owned())
        .collect()
}

#[test]
fn require_finds_orb_modules() {
    let dir = dir_with(
        "require",
        &[
            (
                "geo/vec.orb",
                "pub struct Vec2 { x, y }\n\
                 impl Vec2 {\n\
                     pub fn new(x, y) { Vec2 { x, y } }\n\
                     pub fn len(self) { math::sqrt(self.x * self.x + self.y * self.y) }\n\
                 }\n",
            ),
            (
                "app.orb",
                "use geo::vec::{Vec2};\n\
                 use helper;\n\
                 pub fn run() { f\"{Vec2::new(3, 4).len()} {helper::twice(21)}\" }\n",
            ),
            (
                "helper.lua",
                "return { twice = function(x) return x * 2 end }",
            ),
        ],
    );
    let mut lua = Lua::new();
    let d = dir.display();
    let out = exec_str(
        &mut lua,
        &format!(
            "package.path = '{d}/?.lua;{d}/?.orb'
             local app = require('app')
             return app.run(), #package.searchers, package.loaded['geo.vec'].Vec2.__name"
        ),
    );
    assert_eq!(out, ["5.0 42", "4", "Vec2"]);
}

#[test]
fn orb_modules_are_on_package_path() {
    let mut lua = Lua::new();
    let out = exec_str(
        &mut lua,
        "local default = package.path:find('./?.orb', 1, true) ~= nil
         package.path = './?.lua;./?.orb' package.cpath = './?.so'
         local ok, msg = pcall(require, 'nope')
         return default, msg",
    );
    assert_eq!(
        out,
        [
            "true",
            "module 'nope' not found:\n\tno field package.preload['nope']\n\t\
             no file './nope.lua'\n\tno file './nope.orb'\n\tno file './nope.so'"
        ]
    );
}

#[test]
fn loadfile_and_dofile_pick_the_language_by_name() {
    let dir = dir_with(
        "loadfile",
        &[
            ("a.orb", "let x = [1, 2, 3];\nx[#x] ~ \"!\"\n"),
            ("b.orb", "let y = 1;\ny = 2;\n"),
            ("c.lua", "return 'lua'"),
        ],
    );
    let mut lua = Lua::new();
    let d = dir.display();
    let out = exec_str(
        &mut lua,
        &format!(
            "local f = assert(loadfile('{d}/a.orb'))
             local g, err = loadfile('{d}/b.orb')
             return f(), dofile('{d}/c.lua'), tostring(g), err"
        ),
    );
    assert_eq!(out[..3], ["3!", "lua", "nil"]);
    assert!(
        out[3].ends_with(
            "b.orb:2: cannot assign twice to immutable variable `y` (declare it with `let mut`)"
        ),
        "{}",
        out[3]
    );
}

#[test]
fn load_orbitscript_and_lua_share_globals() {
    let mut lua = Lua::new();
    lua.exec(b"shared = { n = 41 }", "=lua").unwrap();
    let f = lua
        .load_orbitscript(b"extern shared;\nshared.n += 1;\nshared.n", "orb")
        .unwrap();
    let r = lua.call(f, &[]).unwrap();
    assert!(matches!(r[0], Value::Int(42)));
    let err = lua.load_orbitscript(b"let = 1;", "orb").unwrap_err();
    assert_eq!(err.to_string(), "orb:1: expected a pattern, found `=`");
}
