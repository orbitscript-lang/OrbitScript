//! The package library (`loadlib.c`): `require` and `package`.
//!
//! Lua modules load from `package.path`. OrbitScript does not load native (C)
//! libraries, so it behaves like a reference build without dynamic library support:
//! `package.loadlib` fails with `"absent"`, and a file found on `package.cpath` fails
//! to load with the reference's message.

use orbit_core::Value;
use orbit_core::value::TableRef;
use orbit_vm::{Action, LuaResult, Status, Vm};

use crate::oslib::os_path;

const PRELOAD_TABLE: &[u8] = b"_PRELOAD";
const PATH_SEP: u8 = b';';
const PATH_MARK: &[u8] = b"?";
const DIRSEP: &[u8] = b"/";
const DLMSG: &str = "dynamic libraries not enabled; check your Lua installation";

const PATH_DEFAULT: &str = "/usr/local/share/lua/5.5/?.lua;/usr/local/share/lua/5.5/?/init.lua;\
/usr/local/lib/lua/5.5/?.lua;/usr/local/lib/lua/5.5/?/init.lua;./?.lua;./?/init.lua";
const CPATH_DEFAULT: &str = "/usr/local/lib/lua/5.5/?.so;/usr/local/lib/lua/5.5/loadall.so;./?.so";

/// Opens the package library and the global `require`.
pub fn open_package(vm: &mut Vm) {
    let pkg = vm.new_lib(
        "package",
        &[("loadlib", ll_loadlib), ("searchpath", ll_searchpath)],
    );
    let searchers = vm.heap.new_table();
    let fns: [(&'static str, orbit_vm::NativeFn); 4] = [
        ("searcher_preload", searcher_preload),
        ("searcher_Lua", searcher_lua),
        ("searcher_C", searcher_c),
        ("searcher_Croot", searcher_croot),
    ];
    for (i, (name, f)) in fns.into_iter().enumerate() {
        let c = vm.native_closure(name, f, vec![Value::Table(pkg)]);
        vm.heap
            .raw_set_int(searchers, i as i64 + 1, c)
            .expect("integer key");
    }
    set_field(vm, pkg, b"searchers", Value::Table(searchers));
    let path = env_path("LUA_PATH", PATH_DEFAULT);
    let path = vm.heap.str_value(path);
    set_field(vm, pkg, b"path", path);
    let cpath = env_path("LUA_CPATH", CPATH_DEFAULT);
    let cpath = vm.heap.str_value(cpath);
    set_field(vm, pkg, b"cpath", cpath);
    let config = vm.heap.str_value("/\n;\n?\n!\n-\n");
    set_field(vm, pkg, b"config", config);
    let loaded = vm.loaded_table();
    set_field(vm, pkg, b"loaded", Value::Table(loaded));
    let preload = preload_table(vm);
    set_field(vm, pkg, b"preload", Value::Table(preload));
    let require = vm.native_closure("require", ll_require, vec![Value::Table(pkg)]);
    let g = vm.globals();
    set_field(vm, g, b"require", require);
}

fn set_field(vm: &mut Vm, t: TableRef, name: &[u8], v: Value) {
    vm.heap.raw_set_name(t, name, v).expect("string key");
}

/// `registry._PRELOAD`, created on first use.
fn preload_table(vm: &mut Vm) -> TableRef {
    let reg = vm.registry();
    match vm.heap.raw_get_name(reg, PRELOAD_TABLE) {
        Value::Table(t) => t,
        _ => {
            let t = vm.heap.new_table();
            set_field(vm, reg, PRELOAD_TABLE, Value::Table(t));
            t
        }
    }
}

/// The value of `LUA_PATH_5_5` or `LUA_PATH` (for `var` = `"LUA_PATH"`), with `;;`
/// replaced by the default path, or the default.
fn env_path(var: &str, dft: &str) -> Vec<u8> {
    let val = std::env::var_os(format!("{var}_5_5")).or_else(|| std::env::var_os(var));
    let Some(val) = val else {
        return dft.as_bytes().to_vec();
    };
    let path = val.into_encoded_bytes();
    let Some(mark) = path.windows(2).position(|w| w == b";;") else {
        return path;
    };
    let mut out = Vec::new();
    if mark > 0 {
        out.extend_from_slice(&path[..mark]);
        out.push(PATH_SEP);
    }
    out.extend_from_slice(dft.as_bytes());
    if mark + 2 < path.len() {
        out.push(PATH_SEP);
        out.extend_from_slice(&path[mark + 2..]);
    }
    out
}

/// Replaces every `pat` in `s` by `rep` (`luaL_gsub`).
fn replace(s: &[u8], pat: &[u8], rep: &[u8]) -> Vec<u8> {
    if pat.is_empty() {
        return s.to_vec();
    }
    let mut out = Vec::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
        if s[i..].starts_with(pat) {
            out.extend_from_slice(rep);
            i += pat.len();
        } else {
            out.push(s[i]);
            i += 1;
        }
    }
    out
}

fn readable(name: &[u8]) -> bool {
    std::fs::File::open(os_path(name)).is_ok()
}

/// Looks for `name` in `path`: the first readable file, or the "no file ..." message.
fn searchpath(name: &[u8], path: &[u8], sep: &[u8], dirsep: &[u8]) -> Result<Vec<u8>, Vec<u8>> {
    // (The reference tests only the separator's first byte before replacing it all.)
    let name = match sep.first() {
        Some(&c) if name.contains(&c) => replace(name, sep, dirsep),
        _ => name.to_vec(),
    };
    let pathname = replace(path, PATH_MARK, &name);
    for filename in pathname.split(|&c| c == PATH_SEP) {
        if !filename.is_empty() && readable(filename) {
            return Ok(filename.to_vec());
        }
    }
    let mut msg = b"no file '".to_vec();
    msg.extend(replace(&pathname, b";", b"'\n\tno file '"));
    msg.push(b'\'');
    Err(msg)
}

fn string_arg(vm: &mut Vm, arg: usize) -> LuaResult<Vec<u8>> {
    let s = vm.check_string(arg)?;
    Ok(vm.heap.string(s).as_bytes().to_vec())
}

fn opt_string_arg(vm: &mut Vm, arg: usize, dft: &[u8]) -> LuaResult<Vec<u8>> {
    Ok(match vm.opt_string(arg)? {
        Some(s) => vm.heap.string(s).as_bytes().to_vec(),
        None => dft.to_vec(),
    })
}

fn ll_searchpath(vm: &mut Vm) -> LuaResult<Action> {
    let name = string_arg(vm, 1)?;
    let path = string_arg(vm, 2)?;
    let sep = opt_string_arg(vm, 3, b".")?;
    let dirsep = opt_string_arg(vm, 4, DIRSEP)?;
    match searchpath(&name, &path, &sep, &dirsep) {
        Ok(f) => {
            let v = vm.heap.str_value(f);
            vm.push(v);
            Ok(Action::Return(1))
        }
        Err(msg) => {
            let v = vm.heap.str_value(msg);
            vm.push(Value::Nil);
            vm.push(v);
            Ok(Action::Return(2))
        }
    }
}

fn ll_loadlib(vm: &mut Vm) -> LuaResult<Action> {
    string_arg(vm, 1)?;
    string_arg(vm, 2)?;
    let msg = vm.heap.str_value(DLMSG);
    let place = vm.heap.str_value("absent");
    vm.push(Value::Nil);
    vm.push(msg);
    vm.push(place);
    Ok(Action::Return(3))
}

/// Searches `package[pname]` (the upvalue) for `name`.
fn findfile(vm: &mut Vm, name: &[u8], pname: &str) -> LuaResult<Result<Vec<u8>, Vec<u8>>> {
    let pkg = vm.upvalue(1);
    let key = vm.heap.str_value(pname);
    let path = match vm.index_value(pkg, key)? {
        Value::Str(s) => vm.heap.string(s).as_bytes().to_vec(),
        v @ (Value::Int(_) | Value::Float(_)) => orbit_vm::number::fmt_number(v).into_bytes(),
        _ => return Err(vm.error(format!("'package.{pname}' must be a string"))),
    };
    Ok(searchpath(name, &path, b".", DIRSEP))
}

/// Returns the loader and file name, or raises the reference's load error.
fn checkload(
    vm: &mut Vm,
    r: Result<Value, Vec<u8>>,
    name: &[u8],
    filename: &[u8],
) -> LuaResult<Action> {
    match r {
        Ok(f) => {
            let fname = vm.heap.str_value(filename);
            vm.push(f);
            vm.push(fname);
            Ok(Action::Return(2))
        }
        Err(msg) => {
            let mut m = b"error loading module '".to_vec();
            m.extend_from_slice(name);
            m.extend_from_slice(b"' from file '");
            m.extend_from_slice(filename);
            m.extend_from_slice(b"':\n\t");
            m.extend_from_slice(&msg);
            Err(vm.error(m))
        }
    }
}

/// `luaL_loadfilex(L, filename, "bt")`, returning the error message on failure.
fn load_file(vm: &mut Vm, filename: &[u8]) -> LuaResult<Result<Value, Vec<u8>>> {
    let src = match std::fs::read(os_path(filename)) {
        Ok(src) => src,
        Err(e) => {
            let mut m = b"cannot open ".to_vec();
            m.extend_from_slice(filename);
            m.extend_from_slice(b": ");
            m.extend_from_slice(orbit_vm::baselib::io_error_text(&e).as_bytes());
            return Ok(Err(m));
        }
    };
    let mut chunkname = b"@".to_vec();
    chunkname.extend_from_slice(filename);
    let body = orbit_vm::skip_file_prefix(&src);
    if body.first() == Some(&0x1b) {
        let mut m = orbit_lua_syntax::chunk_id(&chunkname);
        m.extend_from_slice(b": binary chunks are not supported");
        return Ok(Err(m));
    }
    match vm.load(body, &chunkname) {
        Ok(f) => Ok(Ok(f)),
        Err(e) if e.status == Status::Syntax => Ok(Err(value_bytes(vm, e.value))),
        Err(e) => Err(e),
    }
}

fn value_bytes(vm: &Vm, v: Value) -> Vec<u8> {
    match v {
        Value::Str(s) => vm.heap.string(s).as_bytes().to_vec(),
        _ => Vec::new(),
    }
}

fn push_msg(vm: &mut Vm, msg: Vec<u8>) -> LuaResult<Action> {
    let v = vm.heap.str_value(msg);
    vm.push(v);
    Ok(Action::Return(1))
}

fn searcher_lua(vm: &mut Vm) -> LuaResult<Action> {
    let name = string_arg(vm, 1)?;
    let filename = match findfile(vm, &name, "path")? {
        Ok(f) => f,
        Err(msg) => return push_msg(vm, msg),
    };
    let r = load_file(vm, &filename)?;
    checkload(vm, r, &name, &filename)
}

fn searcher_c(vm: &mut Vm) -> LuaResult<Action> {
    let name = string_arg(vm, 1)?;
    let filename = match findfile(vm, &name, "cpath")? {
        Ok(f) => f,
        Err(msg) => return push_msg(vm, msg),
    };
    checkload(vm, Err(DLMSG.as_bytes().to_vec()), &name, &filename)
}

fn searcher_croot(vm: &mut Vm) -> LuaResult<Action> {
    let name = string_arg(vm, 1)?;
    let Some(dot) = name.iter().position(|&c| c == b'.') else {
        return Ok(Action::Return(0));
    };
    let filename = match findfile(vm, &name[..dot], "cpath")? {
        Ok(f) => f,
        Err(msg) => return push_msg(vm, msg),
    };
    checkload(vm, Err(DLMSG.as_bytes().to_vec()), &name, &filename)
}

fn searcher_preload(vm: &mut Vm) -> LuaResult<Action> {
    let name = string_arg(vm, 1)?;
    let preload = preload_table(vm);
    let key = vm.heap.str_value(&name);
    let v = vm.index_value(Value::Table(preload), key)?;
    if v.is_nil() {
        let mut m = b"no field package.preload['".to_vec();
        m.extend_from_slice(&name);
        m.extend_from_slice(b"']");
        return push_msg(vm, m);
    }
    let tag = vm.heap.str_value(":preload:");
    vm.push(v);
    vm.push(tag);
    Ok(Action::Return(2))
}

/// Finds a loader for `name`: returns it with its extra value.
fn findloader(vm: &mut Vm, name: &[u8], namev: Value) -> LuaResult<(Value, Value)> {
    let pkg = vm.upvalue(1);
    let key = vm.heap.str_value("searchers");
    let Value::Table(searchers) = vm.index_value(pkg, key)? else {
        return Err(vm.error("'package.searchers' must be a table"));
    };
    let mut msg = b"\n\t".to_vec();
    for i in 1.. {
        let s = vm.heap.raw_get_int(searchers, i);
        if s.is_nil() {
            msg.truncate(msg.len() - 2);
            let mut m = b"module '".to_vec();
            m.extend_from_slice(name);
            m.extend_from_slice(b"' not found:");
            m.extend_from_slice(&msg);
            return Err(vm.error(m));
        }
        vm.check_stack(2)?;
        vm.push(s);
        vm.push(namev);
        vm.call(1, Some(2))?;
        let extra = vm.pop();
        let f = vm.pop();
        if f.is_function() {
            return Ok((f, extra));
        }
        match f {
            Value::Str(s) => msg.extend_from_slice(vm.heap.string(s).as_bytes()),
            Value::Int(_) | Value::Float(_) => {
                msg.extend_from_slice(orbit_vm::number::fmt_number(f).as_bytes())
            }
            _ => continue,
        }
        msg.extend_from_slice(b"\n\t");
    }
    unreachable!()
}

fn ll_require(vm: &mut Vm) -> LuaResult<Action> {
    let s = vm.check_string(1)?;
    let name = vm.heap.string(s).as_bytes().to_vec();
    let namev = Value::Str(s);
    vm.set_top(1);
    let loaded = Value::Table(vm.loaded_table());
    let v = vm.index_value(loaded, namev)?;
    if v.is_truthy() {
        vm.push(v);
        return Ok(Action::Return(1));
    }
    let (loader, data) = findloader(vm, &name, namev)?;
    // Keep the loader data reachable while the loader runs.
    vm.push(data);
    vm.check_stack(3)?;
    vm.push(loader);
    vm.push(namev);
    vm.push(data);
    vm.call(2, Some(1))?;
    let r = vm.pop();
    if !r.is_nil() {
        vm.set_table(loaded, namev, r)?;
    }
    let mut result = vm.index_value(loaded, namev)?;
    if result.is_nil() {
        result = Value::Bool(true);
        vm.set_table(loaded, namev, result)?;
    }
    vm.push(result);
    vm.push(data);
    Ok(Action::Return(2))
}
