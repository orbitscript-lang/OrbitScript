//! Light userdata through the embedding API: opaque tokens with identity
//! semantics.

use orbit::Lua;
use orbit_vm::orbit_core::{LightUserData, Value};
use orbit_vm::{LuaError, NativeCtx, NativeReturn};

fn token(ctx: &mut NativeCtx<'_>) -> Result<NativeReturn, LuaError> {
    let n = ctx.check_int(0)?;
    ctx.ret1(Value::LightUserData(LightUserData::new(n as u64)))
}

fn needs_table(ctx: &mut NativeCtx<'_>) -> Result<NativeReturn, LuaError> {
    ctx.check_table(0)?;
    Ok(NativeReturn::Return(0))
}

#[test]
fn light_userdata_compares_by_identity() {
    let mut lua = Lua::new();
    let vm = lua.vm();
    let f = vm.register_native("token", token);
    vm.set_global("token", f);
    let f = vm.register_native("needs_table", needs_table);
    vm.set_global("needs_table", f);
    let out = lua
        .exec(
            b"local a, b, c = token(1), token(1), token(2)
              local t = {[a] = 'one'}
              local ok, msg = pcall(needs_table, a)
              return type(a), a == b, a == c, t[b], t[c], msg,
                     tostring(a) == tostring(b), rawequal(a, b)",
            "t",
        )
        .unwrap();
    let vm = lua.vm();
    let text: Vec<String> = out
        .iter()
        .map(|&v| String::from_utf8_lossy(&vm.to_display(v)).into_owned())
        .collect();
    assert_eq!(
        text,
        [
            "userdata",
            "true",
            "false",
            "one",
            "nil",
            "bad argument #1 to 'needs_table' (table expected, got light userdata)",
            "true",
            "true",
        ]
    );
}
