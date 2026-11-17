//! `io`'s file handles are `luaL_Stream` blocks over C streams (ADR-15).
//! Only the handles `io` made are files to it while no C code can run: a
//! script can give any userdata the `FILE*` metatable, but that makes no
//! file of it.

#![cfg(feature = "io")]

use orbit::stack::{Return, Stack, StackResult};
use orbit::{Chunk, Config, Runtime};

/// `block()`: a 16-byte userdata block, as C would make a `luaL_Stream`.
struct Block;
impl orbit::stack::Native for Block {
    const NAME: &'static str = "block";
    fn call(s: &mut Stack<'_>) -> StackResult<Return> {
        s.new_userdata(16, 0)?;
        Ok(Return::values(1))
    }
}

/// `hosted()`: a 16-byte block carrying a host value that isn't a file's.
struct Hosted;
impl orbit::stack::Native for Hosted {
    const NAME: &'static str = "hosted";
    fn call(s: &mut Stack<'_>) -> StackResult<Return> {
        s.new_userdata_with_host(16, 0, 7u32)?;
        Ok(Return::values(1))
    }
}

fn run(src: &str) {
    let mut iso = Runtime::new().new_isolate(Config::default());
    orbit_std::install(&mut iso, &orbit_std::Options::default()).unwrap();
    iso.scope(|s| {
        let g = s.globals();
        g.set("block", s.create_native::<Block>())?;
        g.set("hosted", s.create_native::<Hosted>())?;
        s.load(Chunk::new(src).name("=t"))?.call(())?;
        Ok::<_, orbit::Error>(())
    })
    .unwrap_or_else(|e| panic!("{e}"));
}

#[test]
fn only_handles_io_made_are_files() {
    run(r#"
        local mt = getmetatable(io.stdout)
        for _, u in ipairs { block(), hosted() } do
          debug.setmetatable(u, mt)
          assert(io.type(u) == nil)
          local ok, err = pcall(io.stdout.write, u, "x")
          assert(not ok and err:find("FILE%* expected"), err)
          assert(not pcall(io.close, u))
          assert(not pcall(io.output, u))
        end
        local t = setmetatable({}, mt)
        assert(io.type(t) == nil and not pcall(io.stdout.read, t))
    "#);
}

#[test]
fn a_handle_with_another_metatable_is_no_file() {
    run(r#"
        local f = io.tmpfile()
        local mt = debug.getmetatable(f)
        debug.setmetatable(f, {})
        assert(io.type(f) == nil)
        assert(not pcall(mt.__index.write, f, "x"))
        debug.setmetatable(f, mt)
        assert(io.type(f) == "file")
        assert(f:write("x"):seek("set") == 0 and f:read("a") == "x")
        f:close()
        assert(io.type(f) == "closed file")
    "#);
}

#[test]
fn handles_have_no_user_values() {
    run(r#"
        local f = io.tmpfile()
        assert(debug.setuservalue(f, 1) == nil and debug.getuservalue(f, 1) == nil)
        assert(debug.setuservalue(io.stdin, 10) == nil)
        f:close()
    "#);
}
