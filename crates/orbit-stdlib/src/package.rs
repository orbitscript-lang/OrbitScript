//! `require` and the `package` library
//! ([design](https://github.com/tymlipari/OrbitScript/blob/main/docs/design/chunks-io-os-cli.md#modules-require-and-package)).
//!
//! `package.loaded` and `package.preload` are also kept in the registry
//! (`_LOADED`, `_PRELOAD`), and the `package` table itself (`_PACKAGE`), so
//! `require` keeps working when scripts replace those fields, as in Lua.
//! Dynamic loading comes from a [`DynLoader`] a host installs (the
//! `orbit-capi` crate, behind the `c-modules` feature). Without one,
//! `package.loadlib` reports `"absent"` and the C searchers only search.

use orbit_vm::orbit_core::table as tbl;
use orbit_vm::orbit_core::{Gc, Table, Value};
use orbit_vm::{CallOutcome, LuaError, NativeCtx, NativeFn, NativeReturn, Vm};

use crate::load::{Mode, load_chunk, read_chunk_file};
use crate::{LOADED, registry_table};

type R = Result<NativeReturn, LuaError>;

/// Loads `sym` from the C library at `path`: the function, or `true` when
/// `sym` is `"*"` (link the library's symbols globally only). On failure,
/// the message and where it failed (`"open"` or `"init"`).
pub type LoadFn =
    fn(&mut NativeCtx<'_>, path: &[u8], sym: &[u8]) -> Result<Value, (String, &'static str)>;

/// The dynamic loader `package.loadlib` and the C searchers use, kept in
/// [`orbit_vm::Runtime::ext`]. `None` (the default): no dynamic loading.
#[derive(Debug, Default)]
pub struct DynLoader {
    pub load: Option<LoadFn>,
    /// The C module search path when the host has locked it ([`lock_cpath`]).
    locked_cpath: Option<Vec<u8>>,
}

fn dyn_loader(ctx: &mut NativeCtx<'_>) -> Option<LoadFn> {
    ctx.vm.rt.ext.get_or_insert_with(DynLoader::default).load
}

/// Lock the C module search path to `cpath` (ADR-8). The C searchers then
/// search `cpath` whatever scripts set `package.cpath` to, and
/// `package.loadlib` only opens files those searchers could open: a path
/// one of `cpath`'s templates produces with `?` replaced by a module name.
/// Module names reach the templates with `.` turned into the directory
/// separator, so the text in place of `?` holds no `.`, and a path can't
/// climb out of a template's directory with `..`.
pub fn lock_cpath(vm: &mut Vm, cpath: &[u8]) {
    vm.rt
        .ext
        .get_or_insert_with(DynLoader::default)
        .locked_cpath = Some(cpath.to_vec());
}

/// The locked C module search path, if any.
fn locked_cpath(ctx: &mut NativeCtx<'_>) -> Option<Vec<u8>> {
    ctx.vm
        .rt
        .ext
        .get::<DynLoader>()
        .and_then(|d| d.locked_cpath.clone())
}

/// Whether some template in `cpath` gives `file` with each `?` replaced by
/// the same non-empty text holding no `.`.
fn cpath_allows(cpath: &[u8], file: &[u8]) -> bool {
    cpath.split(|&b| b == b';').any(|template| {
        let marks = template.iter().filter(|&&b| b == b'?').count();
        if marks == 0 {
            return !template.is_empty() && template == file;
        }
        let fixed = template.len() - marks;
        let Some(rest) = file.len().checked_sub(fixed) else {
            return false;
        };
        if rest == 0 || rest % marks != 0 {
            return false;
        }
        let first = template.iter().position(|&b| b == b'?').unwrap_or(0);
        let name = &file[first..first + rest / marks];
        if name.contains(&b'.') {
            return false;
        }
        let mut built = Vec::with_capacity(file.len());
        for &b in template {
            if b == b'?' {
                built.extend_from_slice(name);
            } else {
                built.push(b);
            }
        }
        built == file
    })
}

/// The defaults the reference `lua5.5` build uses, so scripts see the same
/// strings, with OrbitScript modules after Lua's: the Lua searcher loads
/// whatever file `package.path` finds, picking the compiler by the `.orb`
/// name.
const DEFAULT_PATH: &str = "/usr/local/share/lua/5.5/?.lua;/usr/local/share/lua/5.5/?/init.lua;\
/usr/local/lib/lua/5.5/?.lua;/usr/local/lib/lua/5.5/?/init.lua;./?.lua;./?/init.lua;\
/usr/local/share/orbit/?.orb;/usr/local/share/orbit/?/init.orb;./?.orb;./?/init.orb";
const DEFAULT_CPATH: &str = "/usr/local/lib/lua/5.5/?.so;/usr/local/lib/lua/5.5/loadall.so;./?.so";

const DIRSEP: &str = if cfg!(windows) { "\\" } else { "/" };

/// Registry keys.
const PRELOAD: &[u8] = b"_PRELOAD";
const PACKAGE: &[u8] = b"_PACKAGE";

const FUNCTIONS: &[(&str, NativeFn)] = &[
    ("package.loadlib", loadlib),
    ("package.searchpath", searchpath),
];

const SEARCHERS: &[(&str, NativeFn)] = &[
    ("package.searchers[1]", search_preload),
    ("package.searchers[2]", search_lua),
    ("package.searchers[3]", search_c),
    ("package.searchers[4]", search_croot),
];

fn reg_get(ctx: &mut NativeCtx<'_>, key: &[u8]) -> Value {
    let k = ctx.new_str(key);
    tbl::get(&ctx.vm.heap, ctx.vm.rt.registry, k)
}

/// A search path from the environment (`LUA_PATH_5_5`, then `LUA_PATH`),
/// with `;;` replaced by the default, or the default.
fn env_path(versioned: &str, plain: &str, default: &str, ignore_env: bool) -> String {
    let v = if ignore_env {
        None
    } else {
        std::env::var(versioned)
            .ok()
            .or_else(|| std::env::var(plain).ok())
    };
    let Some(v) = v else {
        return default.to_string();
    };
    match v.find(";;") {
        None => v,
        Some(i) => {
            let (pre, post) = (&v[..i], &v[i + 2..]);
            let mut out = String::from(pre);
            if !pre.is_empty() {
                out.push(';');
            }
            out.push_str(default);
            if !post.is_empty() {
                out.push(';');
                out.push_str(post);
            }
            out
        }
    }
}

/// Open `package` and the global `require`.
pub(crate) fn open(vm: &mut Vm, ignore_env: bool, lock: bool) {
    let pkg = crate::open_lib(vm, "package", FUNCTIONS);
    vm.pin(Value::Table(pkg));
    let set = |vm: &mut Vm, k: &str, v: Value| {
        let k = vm.new_str(k.as_bytes());
        vm.table_set(pkg, k, v);
    };
    let path = env_path("LUA_PATH_5_5", "LUA_PATH", DEFAULT_PATH, ignore_env);
    let v = vm.new_str(path.as_bytes());
    set(vm, "path", v);
    let cpath = env_path("LUA_CPATH_5_5", "LUA_CPATH", DEFAULT_CPATH, ignore_env);
    if lock {
        lock_cpath(vm, cpath.as_bytes());
    }
    let v = vm.new_str(cpath.as_bytes());
    set(vm, "cpath", v);
    let config = format!("{DIRSEP}\n;\n?\n!\n-\n");
    let v = vm.new_str(config.as_bytes());
    set(vm, "config", v);
    let loaded = registry_table(vm, LOADED);
    set(vm, "loaded", Value::Table(loaded));
    let preload = registry_table(vm, PRELOAD);
    set(vm, "preload", Value::Table(preload));
    let searchers = vm.new_table();
    set(vm, "searchers", Value::Table(searchers));
    for (i, &(name, f)) in SEARCHERS.iter().enumerate() {
        let fv = vm.register_native(name, f);
        vm.table_set(searchers, Value::Int(i as i64 + 1), fv);
    }
    let k = vm.new_str(PACKAGE);
    let reg = vm.rt.registry;
    vm.table_set(reg, k, Value::Table(pkg));
    let req = vm.register_native("require", require);
    vm.set_global("require", req);
    vm.unpin(Value::Table(pkg));
}

fn loadlib(ctx: &mut NativeCtx<'_>) -> R {
    let path = ctx.check_str(0)?;
    let sym = ctx.check_str(1)?;
    if let Some(load) = dyn_loader(ctx) {
        let (path, sym) = (ctx.bytes(path).to_vec(), ctx.bytes(sym).to_vec());
        if let Some(cpath) = locked_cpath(ctx)
            && !cpath_allows(&cpath, &path)
        {
            let msg = ctx.new_str(b"library path not allowed: the C module path is locked");
            let when = ctx.new_str(b"open");
            ctx.push(Value::Nil);
            ctx.push(msg);
            ctx.push(when);
            return Ok(NativeReturn::Return(3));
        }
        return match load(ctx, &path, &sym) {
            Ok(f) => ctx.ret1(f),
            Err((msg, when)) => {
                let msg = ctx.new_str(msg.as_bytes());
                let when = ctx.new_str(when.as_bytes());
                ctx.push(Value::Nil);
                ctx.push(msg);
                ctx.push(when);
                Ok(NativeReturn::Return(3))
            }
        };
    }
    let msg = ctx.new_str(b"dynamic libraries not enabled; check your Lua installation");
    let when = ctx.new_str(b"absent");
    ctx.push(Value::Nil);
    ctx.push(msg);
    ctx.push(when);
    Ok(NativeReturn::Return(3))
}

/// Look for `name` along `path`: each `;`-separated template with `?`
/// replaced by `name` (after replacing `sep` in `name` with `rep`). Returns
/// the first readable file, or the `no file '...'` lines.
fn search_path(name: &[u8], path: &[u8], sep: &[u8], rep: &[u8]) -> Result<Vec<u8>, Vec<u8>> {
    let name = if sep.is_empty() {
        name.to_vec()
    } else {
        replace(name, sep, rep)
    };
    let mut msg = Vec::new();
    for template in path.split(|&b| b == b';') {
        let file = replace(template, b"?", &name);
        if std::fs::File::open(crate::os_path(&file)).is_ok() && !file.contains(&0) {
            return Ok(file);
        }
        if !msg.is_empty() {
            msg.extend_from_slice(b"\n\t");
        }
        msg.extend_from_slice(b"no file '");
        msg.extend_from_slice(&file);
        msg.push(b'\'');
    }
    Err(msg)
}

fn replace(s: &[u8], from: &[u8], to: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
        if s[i..].starts_with(from) {
            out.extend_from_slice(to);
            i += from.len();
        } else {
            out.push(s[i]);
            i += 1;
        }
    }
    out
}

fn str_arg(ctx: &mut NativeCtx<'_>, i: usize) -> Result<Vec<u8>, LuaError> {
    let s = ctx.check_str(i)?;
    Ok(ctx.bytes(s).to_vec())
}

fn searchpath(ctx: &mut NativeCtx<'_>) -> R {
    // lua5.5 checks the path first.
    let path = str_arg(ctx, 1)?;
    let name = str_arg(ctx, 0)?;
    let sep = if ctx.arg(2).is_nil() {
        b".".to_vec()
    } else {
        str_arg(ctx, 2)?
    };
    let rep = if ctx.arg(3).is_nil() {
        DIRSEP.as_bytes().to_vec()
    } else {
        str_arg(ctx, 3)?
    };
    match search_path(&name, &path, &sep, &rep) {
        Ok(file) => {
            let f = ctx.new_str_owned(file);
            ctx.ret1(f)
        }
        Err(msg) => {
            let m = ctx.new_str_owned(msg);
            ctx.push(Value::Nil);
            ctx.push(m);
            Ok(NativeReturn::Return(2))
        }
    }
}

/// A field of the `package` table (the one `require` was opened with).
fn package_field(ctx: &mut NativeCtx<'_>, field: &[u8]) -> Value {
    match reg_get(ctx, PACKAGE) {
        Value::Table(p) => {
            let k = ctx.new_str(field);
            tbl::get(&ctx.vm.heap, p, k)
        }
        _ => Value::Nil,
    }
}

/// `package.path` or `package.cpath`, which must be a string (numbers
/// convert).
fn path_field(ctx: &mut NativeCtx<'_>, field: &str) -> Result<Vec<u8>, LuaError> {
    match package_field(ctx, field.as_bytes()) {
        Value::Str(s) => Ok(ctx.bytes(s).to_vec()),
        v @ (Value::Int(_) | Value::Float(_)) => {
            let mut out = Vec::new();
            ctx.display(v, &mut out);
            Ok(out)
        }
        _ => Err(ctx.error(format!("'package.{field}' must be a string"))),
    }
}

/// Return one message string from a searcher.
fn ret_msg(ctx: &mut NativeCtx<'_>, msg: Vec<u8>) -> R {
    let m = ctx.new_str_owned(msg);
    ctx.ret1(m)
}

fn search_preload(ctx: &mut NativeCtx<'_>) -> R {
    let name = str_arg(ctx, 0)?;
    let f = match reg_get(ctx, PRELOAD) {
        Value::Table(t) => {
            let k = ctx.new_str(&name);
            tbl::get(&ctx.vm.heap, t, k)
        }
        _ => return Err(ctx.error("'package.preload' must be a table")),
    };
    if f.is_nil() {
        let mut msg = b"no field package.preload['".to_vec();
        msg.extend_from_slice(&name);
        msg.extend_from_slice(b"']");
        return ret_msg(ctx, msg);
    }
    let data = ctx.new_str(b":preload:");
    ctx.push(f);
    ctx.push(data);
    Ok(NativeReturn::Return(2))
}

/// The error for a module file that was found but can't be loaded.
fn load_error(ctx: &mut NativeCtx<'_>, name: &[u8], file: &[u8], why: &str) -> LuaError {
    let msg = format!(
        "error loading module '{}' from file '{}':\n\t{why}",
        String::from_utf8_lossy(name),
        String::from_utf8_lossy(file)
    );
    ctx.error(msg)
}

/// The Lua searcher. It also finds `.orb` modules: they are on
/// `package.path` (`DEFAULT_PATH`), and the loader picks the compiler by the
/// file name.
fn search_lua(ctx: &mut NativeCtx<'_>) -> R {
    let name = str_arg(ctx, 0)?;
    let path = path_field(ctx, "path")?;
    let file = match search_path(&name, &path, b".", DIRSEP.as_bytes()) {
        Ok(f) => f,
        Err(msg) => return ret_msg(ctx, msg),
    };
    let globals = Value::Table(ctx.vm.rt.globals);
    let loaded = read_chunk_file(Some(&file))
        .and_then(|(bytes, source)| load_chunk(ctx, &bytes, &source, &Mode(b"bt"), globals));
    match loaded {
        Ok(f) => {
            ctx.push(f);
            let data = ctx.new_str_owned(file);
            ctx.push(data);
            Ok(NativeReturn::Return(2))
        }
        Err(why) => Err(load_error(ctx, &name, &file, &why)),
    }
}

/// The C searchers: search `cpath` like `lua`, then load the library's
/// `luaopen_` function through the [`DynLoader`]. Without one, a library
/// that's found can't be loaded. `croot` is the all-in-one searcher: a
/// library without the function only adds a message.
fn search_c_for(ctx: &mut NativeCtx<'_>, name: &[u8], file_name: &[u8], croot: bool) -> R {
    let cpath = match locked_cpath(ctx) {
        Some(cpath) => cpath,
        None => path_field(ctx, "cpath")?,
    };
    let file = match search_path(file_name, &cpath, b".", DIRSEP.as_bytes()) {
        Ok(file) => file,
        Err(msg) => return ret_msg(ctx, msg),
    };
    let Some(load) = dyn_loader(ctx) else {
        return Err(load_error(
            ctx,
            name,
            &file,
            "dynamic libraries not enabled; check your Lua installation",
        ));
    };
    // `luaopen_` and the name with dots as underscores, without any suffix
    // from a hyphen on (`a.b-v2` opens `luaopen_a_b`).
    let base = name.split(|&b| b == b'-').next().unwrap_or(name);
    let mut sym = b"luaopen_".to_vec();
    sym.extend(base.iter().map(|&b| if b == b'.' { b'_' } else { b }));
    match load(ctx, &file, &sym) {
        Ok(f) => {
            ctx.push(f);
            let data = ctx.new_str_owned(file);
            ctx.push(data);
            Ok(NativeReturn::Return(2))
        }
        Err((_, "init")) if croot => {
            let mut msg = b"no module '".to_vec();
            msg.extend_from_slice(name);
            msg.extend_from_slice(b"' in file '");
            msg.extend_from_slice(&file);
            msg.push(b'\'');
            ret_msg(ctx, msg)
        }
        Err((why, _)) => Err(load_error(ctx, name, &file, &why)),
    }
}

fn search_c(ctx: &mut NativeCtx<'_>) -> R {
    let name = str_arg(ctx, 0)?;
    search_c_for(ctx, &name, &name, false)
}

fn search_croot(ctx: &mut NativeCtx<'_>) -> R {
    let name = str_arg(ctx, 0)?;
    let Some(dot) = name.iter().position(|&b| b == b'.') else {
        return Ok(NativeReturn::Return(0));
    };
    let root = name[..dot].to_vec();
    search_c_for(ctx, &name, &root, true)
}

// `require`'s window while it runs searchers and the loader: the name, the
// searchers table, the next searcher's index and the messages so far; then
// the loader's extra value once a searcher succeeds.
const NAME: usize = 0;
const SEARCHERS_AT: usize = 1;
const NEXT: usize = 2;
const MSGS: usize = 3;
const EXTRA: usize = 4;

fn loaded_table(ctx: &mut NativeCtx<'_>) -> Result<Gc<Table>, LuaError> {
    match reg_get(ctx, LOADED) {
        Value::Table(t) => Ok(t),
        _ => Err(ctx.error("'package.loaded' must be a table")),
    }
}

fn require(ctx: &mut NativeCtx<'_>) -> R {
    let s = ctx.check_str(0)?;
    let loaded = loaded_table(ctx)?;
    let v = tbl::get(&ctx.vm.heap, loaded, Value::Str(s));
    if !v.is_falsy() {
        return ctx.ret1(v);
    }
    let Value::Table(searchers) = package_field(ctx, b"searchers") else {
        return Err(ctx.error("'package.searchers' must be a table"));
    };
    ctx.truncate(1);
    ctx.push(Value::Table(searchers));
    ctx.push(Value::Int(1));
    let empty = ctx.new_str(b"");
    ctx.push(empty);
    next_searcher(ctx)
}

/// Call the next searcher.
fn next_searcher(ctx: &mut NativeCtx<'_>) -> R {
    let (Value::Table(searchers), Value::Int(i)) = (ctx.arg(SEARCHERS_AT), ctx.arg(NEXT)) else {
        unreachable!("require's window")
    };
    let f = tbl::get(&ctx.vm.heap, searchers, Value::Int(i));
    if f.is_nil() {
        let Value::Str(name) = ctx.arg(NAME) else {
            unreachable!("require's window")
        };
        let Value::Str(msgs) = ctx.arg(MSGS) else {
            unreachable!("require's window")
        };
        let mut msg = b"module '".to_vec();
        msg.extend_from_slice(ctx.bytes(name));
        msg.extend_from_slice(b"' not found:");
        msg.extend_from_slice(ctx.bytes(msgs));
        let text = String::from_utf8_lossy(&msg).into_owned();
        let where_ = ctx.where_(1);
        let m = ctx.new_str(format!("{where_}{text}").as_bytes());
        return Err(ctx.error_value(m));
    }
    ctx.set(NEXT, Value::Int(i + 1));
    ctx.truncate(MSGS + 1);
    ctx.push(ctx.arg(NAME));
    Ok(NativeReturn::CallThen {
        f,
        nargs: 1,
        k: searcher_done,
        protected: false,
    })
}

fn searcher_done(ctx: &mut NativeCtx<'_>, outcome: CallOutcome) -> R {
    let CallOutcome::Ok(n) = outcome else {
        unreachable!("unprotected call")
    };
    let first = ctx.len() - n;
    let r0 = if n > 0 { ctx.arg(first) } else { Value::Nil };
    let r1 = if n > 1 {
        ctx.arg(first + 1)
    } else {
        Value::Nil
    };
    ctx.truncate(MSGS + 1);
    match r0 {
        Value::Closure(_) | Value::Native(_) => {
            // [.., extra, name, extra] -> loader(name, extra)
            ctx.push(r1);
            ctx.push(ctx.arg(NAME));
            ctx.push(r1);
            Ok(NativeReturn::CallThen {
                f: r0,
                nargs: 2,
                k: loader_done,
                protected: false,
            })
        }
        Value::Str(_) | Value::Int(_) | Value::Float(_) => {
            let Value::Str(msgs) = ctx.arg(MSGS) else {
                unreachable!("require's window")
            };
            let mut all = ctx.bytes(msgs).to_vec();
            all.extend_from_slice(b"\n\t");
            ctx.display(r0, &mut all);
            let m = ctx.new_str_owned(all);
            ctx.set(MSGS, m);
            next_searcher(ctx)
        }
        _ => next_searcher(ctx),
    }
}

fn loader_done(ctx: &mut NativeCtx<'_>, outcome: CallOutcome) -> R {
    let CallOutcome::Ok(n) = outcome else {
        unreachable!("unprotected call")
    };
    let first = ctx.len() - n;
    let result = if n > 0 { ctx.arg(first) } else { Value::Nil };
    let name = ctx.arg(NAME);
    let loaded = loaded_table(ctx)?;
    if !result.is_nil() {
        tbl::set(&mut ctx.vm.heap, loaded, name, result).expect("string keys are valid");
    }
    let mut v = tbl::get(&ctx.vm.heap, loaded, name);
    if v.is_nil() {
        v = Value::Bool(true);
        tbl::set(&mut ctx.vm.heap, loaded, name, v).expect("string keys are valid");
    }
    let extra = ctx.arg(EXTRA);
    ctx.truncate(0);
    ctx.push(v);
    ctx.push(extra);
    Ok(NativeReturn::Return(2))
}

#[cfg(test)]
mod tests {
    use super::cpath_allows;

    const CPATH: &[u8] = b"/usr/lib/lua/?.so;/opt/?/core.so;./?.so;/fixed/all.so";

    #[test]
    fn locked_cpath_allows_what_the_searchers_could_open() {
        assert!(cpath_allows(CPATH, b"/usr/lib/lua/lfs.so"));
        assert!(cpath_allows(CPATH, b"/usr/lib/lua/socket/core.so"));
        assert!(cpath_allows(CPATH, b"/opt/x/core.so"));
        assert!(cpath_allows(CPATH, b"./cjson.so"));
        assert!(cpath_allows(CPATH, b"/fixed/all.so"));
    }

    #[test]
    fn locked_cpath_refuses_other_files() {
        assert!(!cpath_allows(CPATH, b"/usr/lib/lua/../../libc.so"));
        assert!(!cpath_allows(CPATH, b"/usr/lib/lua/.so"));
        assert!(!cpath_allows(CPATH, b"/usr/lib/lua/lfs.so.6"));
        assert!(!cpath_allows(CPATH, b"/opt/a/b.so"));
        assert!(!cpath_allows(CPATH, b"/lib/libc.so.6"));
        assert!(!cpath_allows(CPATH, b"/fixed/all.so2"));
        assert!(!cpath_allows(b"", b""));
        // Every `?` takes the same text.
        assert!(cpath_allows(b"/m/?/?.so", b"/m/a/a.so"));
        assert!(!cpath_allows(b"/m/?/?.so", b"/m/a/b.so"));
    }
}
