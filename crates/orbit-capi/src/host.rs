//! SPIKE (liblua): states created from C (`lua_newstate`), for programs
//! that embed the runtime through the C API instead of the Rust one.

use orbit_vm::Vm;

fn compile_lua(src: &[u8], chunk_name: &str) -> Result<orbit_vm::op::UnlinkedChunk, String> {
    let options = orbit_lua_syntax::ParseOptions::default();
    let hir = orbit_lua_syntax::to_hir(chunk_name, src, &options)
        .map_err(|d| d.to_lua_message(chunk_name))?;
    orbit_compiler::compile(&hir).map_err(|e| e.to_string())
}

fn compile_orb(src: &[u8], chunk_name: &str) -> Result<orbit_vm::op::UnlinkedChunk, String> {
    let options = orbit_orb_syntax::ParseOptions::default();
    let hir = orbit_orb_syntax::to_hir(chunk_name, src, &options)
        .map_err(|d| d.to_message(chunk_name))?;
    orbit_compiler::compile(&hir).map_err(|e| e.to_string())
}

/// A VM with both front ends and no libraries open, as `lua_newstate`
/// gives one.
pub(crate) fn new_vm() -> Vm {
    let mut vm = Vm::new();
    vm.rt.compile = Some(compile_lua);
    vm.rt.compile_orbitscript = Some(compile_orb);
    vm
}

/// `luaL_openselectedlibs`: the spike opens every library.
/// `registry.LUA_NOENV` set (as `lua -E` does) makes `package` ignore
/// the environment.
pub(crate) fn open_libs(vm: &mut Vm) {
    let k = vm.new_str(b"LUA_NOENV");
    let noenv = orbit_vm::orbit_core::table::get(&vm.heap, vm.rt.registry, k);
    let options = orbit_stdlib::Options {
        ignore_env: !noenv.is_falsy(),
        ..Default::default()
    };
    orbit_stdlib::open_libs_with(vm, options);
}
