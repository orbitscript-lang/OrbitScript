//! States created from C (`lua_newstate`, ADR-10), for programs that
//! embed the runtime through the C API instead of the Rust one.

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
