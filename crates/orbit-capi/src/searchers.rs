//! The C searchers and `package.loadlib`: loading C modules for
//! `require` (ADR-8). `orbit-std` adds `package.cpath` and the Lua
//! searcher; [`install_searchers`] adds these after it.

use orbit::stack::{Native, REGISTRY_INDEX, Return, Stack, StackResult, Type};

use orbit_std::{DIRSEP, search_path};

type R = StackResult<Return>;

/// Loads `sym` from the C library at `path` and pushes the function, or
/// `true` when `sym` is `"*"` (link the library's symbols globally only).
/// On failure, pushes nothing and gives the message and where it failed
/// (`"open"` or `"init"`).
pub type LoadFn = fn(&mut Stack<'_>, path: &[u8], sym: &[u8]) -> Result<(), (String, &'static str)>;

/// The dynamic loader `package.loadlib` and the C searchers use, an
/// isolate extension ([`Stack::extension`]). `None` (the default): no
/// dynamic loading.
#[derive(Debug, Default)]
pub struct DynLoader {
    pub load: Option<LoadFn>,
    /// The C module search path when the host has locked it ([`lock_cpath`]).
    locked_cpath: Option<Vec<u8>>,
}

fn dyn_loader(s: &mut Stack<'_>) -> Option<LoadFn> {
    s.extension(|d: &mut DynLoader| d.load)
}

/// Lock the C module search path to `cpath` (ADR-8). The C searchers then
/// search `cpath` whatever scripts set `package.cpath` to, and
/// `package.loadlib` only opens files those searchers could open: a path
/// one of `cpath`'s templates produces with `?` replaced by a module name.
/// Module names reach the templates with `.` turned into the directory
/// separator, so the text in place of `?` holds no `.`, and a path can't
/// climb out of a template's directory with `..`.
pub fn lock_cpath(s: &mut Stack<'_>, cpath: &[u8]) {
    s.extension(|d: &mut DynLoader| d.locked_cpath = Some(cpath.to_vec()));
}

/// The locked C module search path, if any.
fn locked_cpath(s: &mut Stack<'_>) -> Option<Vec<u8>> {
    s.extension(|d: &mut DynLoader| d.locked_cpath.clone())
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

/// Push the `package` table `require` was opened with; false, pushing
/// nothing, if there is none.
fn push_package(s: &mut Stack<'_>) -> StackResult<bool> {
    if s.raw_get_field(REGISTRY_INDEX, PACKAGE)? == Type::Table {
        return Ok(true);
    }
    s.set_top(-2)?;
    Ok(false)
}

/// `package.cpath`, which must be a string (numbers convert).
fn cpath_field(s: &mut Stack<'_>) -> StackResult<Vec<u8>> {
    let top = s.top();
    if push_package(s)? {
        s.raw_get_field(-1, b"cpath")?;
    } else {
        s.push_nil();
    }
    let v = match s.type_at(-1) {
        Type::String | Type::Number => Some(s.display(-1)),
        _ => None,
    };
    s.set_top(top)?;
    v.ok_or_else(|| s.error("'package.cpath' must be a string"))
}

/// Return one message string from a searcher.
fn ret_msg(s: &mut Stack<'_>, msg: &[u8]) -> R {
    s.push_bytes(msg);
    Ok(Return::values(1))
}

/// Return `nil, msg, when`, as `package.loadlib` fails.
fn ret_fail(s: &mut Stack<'_>, msg: &[u8], when: &[u8]) -> R {
    s.push_nil();
    s.push_bytes(msg);
    s.push_bytes(when);
    Ok(Return::values(3))
}

/// The error for a module file that was found but can't be loaded.
fn load_error(s: &mut Stack<'_>, name: &[u8], file: &[u8], why: &str) -> orbit::stack::Raised {
    s.error(orbit_std::load_error(name, file, why))
}

/// Add the C searchers after the ones there (preload, and `orbit-std`'s
/// Lua searcher), replace `package.loadlib`, and lock the C module path to
/// the current `package.cpath` when `lock`. Does nothing unless the
/// `package` library is open.
pub(crate) fn install_searchers(s: &mut Stack<'_>, lock: bool) {
    let top = s.top();
    let _ = install_in(s, lock);
    let _ = s.set_top(top);
}

fn install_in(s: &mut Stack<'_>, lock: bool) -> StackResult<()> {
    if !push_package(s)? {
        return Ok(());
    }
    if lock && let Ok(cpath) = cpath_field(s) {
        lock_cpath(s, &cpath);
    }
    // [package]
    s.push_native::<LoadLib>();
    s.raw_set_field(-2, b"loadlib")?;
    if s.raw_get_field(-1, b"searchers")? != Type::Table {
        return Ok(());
    }
    // [package, searchers]
    for which in 0..2 {
        let mut n = 1;
        while s.raw_get_index(-1, n)? != Type::Nil {
            s.set_top(-2)?;
            n += 1;
        }
        s.set_top(-2)?;
        if which == 0 {
            s.push_native::<SearchC>();
        } else {
            s.push_native::<SearchCRoot>();
        }
        s.raw_set_index(-2, n)?;
    }
    Ok(())
}

struct LoadLib;

impl Native for LoadLib {
    const NAME: &'static str = "package.loadlib";

    fn call(s: &mut Stack<'_>) -> R {
        let path = s.check_string(1)?.to_vec();
        let sym = s.check_string(2)?.to_vec();
        let Some(load) = dyn_loader(s) else {
            return ret_fail(
                s,
                b"dynamic libraries not enabled; check your Lua installation",
                b"absent",
            );
        };
        if let Some(cpath) = locked_cpath(s)
            && !cpath_allows(&cpath, &path)
        {
            return ret_fail(
                s,
                b"library path not allowed: the C module path is locked",
                b"open",
            );
        }
        match load(s, &path, &sym) {
            Ok(()) => Ok(Return::values(1)),
            Err((msg, when)) => ret_fail(s, msg.as_bytes(), when.as_bytes()),
        }
    }
}

/// The C searchers: search `cpath` like `lua`, then load the library's
/// `luaopen_` function through the [`DynLoader`]. Without one, a library
/// that's found can't be loaded. `croot` is the all-in-one searcher: a
/// library without the function only adds a message.
fn search_c_for(s: &mut Stack<'_>, name: &[u8], file_name: &[u8], croot: bool) -> R {
    let cpath = match locked_cpath(s) {
        Some(cpath) => cpath,
        None => cpath_field(s)?,
    };
    let file = match search_path(file_name, &cpath, b".", DIRSEP.as_bytes()) {
        Ok(file) => file,
        Err(msg) => return ret_msg(s, &msg),
    };
    let Some(load) = dyn_loader(s) else {
        return Err(load_error(
            s,
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
    match load(s, &file, &sym) {
        Ok(()) => {
            s.push_bytes(&file);
            Ok(Return::values(2))
        }
        Err((_, "init")) if croot => {
            let mut msg = b"no module '".to_vec();
            msg.extend_from_slice(name);
            msg.extend_from_slice(b"' in file '");
            msg.extend_from_slice(&file);
            msg.push(b'\'');
            ret_msg(s, &msg)
        }
        Err((why, _)) => Err(load_error(s, name, &file, &why)),
    }
}

struct SearchC;

impl Native for SearchC {
    const NAME: &'static str = "package.searchers[3]";

    fn call(s: &mut Stack<'_>) -> R {
        let name = s.check_string(1)?.to_vec();
        search_c_for(s, &name, &name, false)
    }
}

struct SearchCRoot;

impl Native for SearchCRoot {
    const NAME: &'static str = "package.searchers[4]";

    fn call(s: &mut Stack<'_>) -> R {
        let name = s.check_string(1)?.to_vec();
        let Some(dot) = name.iter().position(|&b| b == b'.') else {
            return Ok(Return::values(0));
        };
        let root = name[..dot].to_vec();
        search_c_for(s, &name, &root, true)
    }
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
