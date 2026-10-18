//! The C searchers and `package.loadlib`: loading C modules for
//! `require` (ADR-8). `orbit-std` adds `package.cpath` and the Lua
//! searcher; [`install_searchers`] adds these after it.

use orbit_runtime::object::table as tbl;
use orbit_runtime::object::{Gc, Table, Value};
use orbit_runtime::vm::{LuaError, NativeCtx, NativeReturn, Vm};

use orbit_std::{DIRSEP, search_path};

type R = Result<NativeReturn, LuaError>;

/// Loads `sym` from the C library at `path`: the function, or `true` when
/// `sym` is `"*"` (link the library's symbols globally only). On failure,
/// the message and where it failed (`"open"` or `"init"`).
pub type LoadFn =
    fn(&mut NativeCtx<'_>, path: &[u8], sym: &[u8]) -> Result<Value, (String, &'static str)>;

/// The dynamic loader `package.loadlib` and the C searchers use, kept in
/// [`orbit_runtime::vm::Runtime::ext`]. `None` (the default): no dynamic loading.
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

/// Registry key of the `package` table `require` was opened with.
const PACKAGE: &[u8] = b"_PACKAGE";

fn str_arg(ctx: &mut NativeCtx<'_>, i: usize) -> Result<Vec<u8>, LuaError> {
    let s = ctx.check_str(i)?;
    Ok(ctx.bytes(s).to_vec())
}

/// The `package` table `require` was opened with, if any.
fn package_table(ctx: &mut NativeCtx<'_>) -> Option<Gc<Table>> {
    let k = ctx.new_str(PACKAGE);
    match tbl::get(&ctx.vm.heap, ctx.vm.rt.registry, k) {
        Value::Table(p) => Some(p),
        _ => None,
    }
}

/// `package.cpath`, which must be a string (numbers convert).
fn cpath_field(ctx: &mut NativeCtx<'_>) -> Result<Vec<u8>, LuaError> {
    let v = match package_table(ctx) {
        Some(p) => {
            let k = ctx.new_str(b"cpath");
            tbl::get(&ctx.vm.heap, p, k)
        }
        None => Value::Nil,
    };
    match v {
        Value::Str(s) => Ok(ctx.bytes(s).to_vec()),
        v @ (Value::Int(_) | Value::Float(_)) => {
            let mut out = Vec::new();
            ctx.display(v, &mut out);
            Ok(out)
        }
        _ => Err(ctx.error("'package.cpath' must be a string")),
    }
}

/// Return one message string from a searcher.
fn ret_msg(ctx: &mut NativeCtx<'_>, msg: Vec<u8>) -> R {
    let m = ctx.new_str_owned(msg);
    ctx.ret1(m)
}

/// The error for a module file that was found but can't be loaded.
fn load_error(ctx: &mut NativeCtx<'_>, name: &[u8], file: &[u8], why: &str) -> LuaError {
    ctx.error(orbit_std::load_error(name, file, why))
}

/// Add the C searchers after the ones there (preload, and `orbit-std`'s
/// Lua searcher), replace `package.loadlib`, and lock the C module path to
/// the current `package.cpath` when `lock`. Does nothing unless the
/// `package` library is open.
pub(crate) fn install_searchers(ctx: &mut NativeCtx<'_>, lock: bool) {
    let Some(pkg) = package_table(ctx) else {
        return;
    };
    if lock && let Ok(cpath) = cpath_field(ctx) {
        lock_cpath(ctx.vm, &cpath);
    }
    let k = ctx.new_str(b"loadlib");
    let f = ctx.native("package.loadlib", loadlib);
    let _ = tbl::set(&mut ctx.vm.heap, pkg, k, f);
    let k = ctx.new_str(b"searchers");
    let Value::Table(searchers) = tbl::get(&ctx.vm.heap, pkg, k) else {
        return;
    };
    for (name, f) in [
        (
            "package.searchers[3]",
            search_c as orbit_runtime::vm::NativeFn,
        ),
        ("package.searchers[4]", search_croot),
    ] {
        let mut n = 1;
        while !tbl::get(&ctx.vm.heap, searchers, Value::Int(n)).is_nil() {
            n += 1;
        }
        let f = ctx.native(name, f);
        let _ = tbl::set(&mut ctx.vm.heap, searchers, Value::Int(n), f);
    }
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

/// The C searchers: search `cpath` like `lua`, then load the library's
/// `luaopen_` function through the [`DynLoader`]. Without one, a library
/// that's found can't be loaded. `croot` is the all-in-one searcher: a
/// library without the function only adds a message.
fn search_c_for(ctx: &mut NativeCtx<'_>, name: &[u8], file_name: &[u8], croot: bool) -> R {
    let cpath = match locked_cpath(ctx) {
        Some(cpath) => cpath,
        None => cpath_field(ctx)?,
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
