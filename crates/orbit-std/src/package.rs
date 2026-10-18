//! `require` finding files: `package.path` and `package.cpath`,
//! `package.searchpath`, and the Lua searcher
//! ([design](https://github.com/tymlipari/OrbitScript/blob/main/docs/design/chunks-io-os-cli.md#modules-require-and-package)).
//!
//! The runtime's `package` library keeps `require`, `package.loaded`,
//! `package.preload` and the preload searcher; this adds the searcher that
//! reads files. The C searchers and `package.loadlib` come from
//! `orbit-capi`, which installs itself after this library.

use orbit::{Call, Chunk, Error, Ret, Scope, Table, Value};

use crate::base::read_chunk_file;
use crate::sys::os_path;

/// The defaults the reference `lua5.5` build uses, so scripts see the same
/// strings, with OrbitScript modules after Lua's: the Lua searcher loads
/// whatever file `package.path` finds, picking the compiler by the `.orb`
/// name.
const DEFAULT_PATH: &str = "/usr/local/share/lua/5.5/?.lua;/usr/local/share/lua/5.5/?/init.lua;\
/usr/local/lib/lua/5.5/?.lua;/usr/local/lib/lua/5.5/?/init.lua;./?.lua;./?/init.lua;\
/usr/local/share/orbit/?.orb;/usr/local/share/orbit/?/init.orb;./?.orb;./?/init.orb";
const DEFAULT_CPATH: &str = "/usr/local/lib/lua/5.5/?.so;/usr/local/lib/lua/5.5/loadall.so;./?.so";

/// The directory separator, which `require` puts in place of dots.
pub const DIRSEP: &str = if cfg!(windows) { "\\" } else { "/" };

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

/// Look for `name` along `path`: each `;`-separated template with `?`
/// replaced by `name` (after replacing `sep` in `name` with `rep`). Returns
/// the first readable file, or the `no file '...'` lines.
pub fn search_path(name: &[u8], path: &[u8], sep: &[u8], rep: &[u8]) -> Result<Vec<u8>, Vec<u8>> {
    let name = if sep.is_empty() {
        name.to_vec()
    } else {
        replace(name, sep, rep)
    };
    let mut msg = Vec::new();
    for template in path.split(|&b| b == b';') {
        let file = replace(template, b"?", &name);
        if std::fs::File::open(os_path(&file)).is_ok() && !file.contains(&0) {
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

fn searchpath<'s>(c: &'s Call<'s>) -> Result<Ret, Error> {
    // lua5.5 checks the path first.
    let path: Vec<u8> = c.check(1)?;
    let name: Vec<u8> = c.check(0)?;
    let sep = c
        .opt::<Option<Vec<u8>>>(2, None)?
        .unwrap_or_else(|| b".".to_vec());
    let rep = c
        .opt::<Option<Vec<u8>>>(3, None)?
        .unwrap_or_else(|| DIRSEP.as_bytes().to_vec());
    match search_path(&name, &path, &sep, &rep) {
        Ok(file) => c.ret(&file[..]),
        Err(msg) => c.ret((Value::Nil, &msg[..])),
    }
}

/// `package.path` or `package.cpath` of the `package` table `require` was
/// opened with (the registry's `_PACKAGE`), which must be a string
/// (numbers convert).
pub fn path_field<'s>(s: &'s Scope<'s>, field: &str) -> Result<Vec<u8>, String> {
    let v = match s.registry().raw_get::<Option<Table>>("_PACKAGE") {
        Ok(Some(p)) => p.raw_get::<Value>(field).unwrap_or(Value::Nil),
        _ => Value::Nil,
    };
    match v {
        Value::Str(_) | Value::Int(_) | Value::Float(_) => {
            v.get::<Vec<u8>>().map_err(|e| e.message().to_owned())
        }
        _ => Err(format!("'package.{field}' must be a string")),
    }
}

/// The error for a module file that was found but can't be loaded.
pub fn load_error(name: &[u8], file: &[u8], why: &str) -> String {
    format!(
        "error loading module '{}' from file '{}':\n\t{why}",
        String::from_utf8_lossy(name),
        String::from_utf8_lossy(file)
    )
}

/// The Lua searcher. It also finds `.orb` modules: they are on
/// `package.path` (`DEFAULT_PATH`), and the loader picks the compiler by the
/// file name.
fn search_lua<'s>(c: &'s Call<'s>) -> Result<Ret, Error> {
    let name: Vec<u8> = c.check(0)?;
    let path = path_field(c, "path").map_err(|msg| c.error(msg))?;
    let file = match search_path(&name, &path, b".", DIRSEP.as_bytes()) {
        Ok(f) => f,
        Err(msg) => return c.ret(&msg[..]),
    };
    let loaded = read_chunk_file(Some(&file)).and_then(|(bytes, source)| {
        c.load(Chunk::new(&bytes).name(&source))
            .map_err(|e| e.message().to_owned())
    });
    match loaded {
        Ok(f) => c.ret((f, &file[..])),
        Err(why) => Err(c.error(load_error(&name, &file, &why))),
    }
}

/// `package.loadlib` when no C modules are installed.
fn loadlib_absent<'s>(c: &'s Call<'s>) -> Result<Ret, Error> {
    c.check::<Vec<u8>>(0)?;
    c.check::<Vec<u8>>(1)?;
    c.ret((
        Value::Nil,
        "dynamic libraries not enabled; check your Lua installation",
        "absent",
    ))
}

/// Set `package.path` and `package.cpath` (from the environment unless
/// `ignore_env`), and add `package.searchpath`, `package.loadlib` (which
/// reports C modules absent until `orbit-capi` replaces it) and the Lua
/// searcher. Does nothing without the `package` library.
pub fn open<'s>(s: &'s Scope<'s>, ignore_env: bool) -> Result<(), Error> {
    let Some(pkg) = s.registry().raw_get::<Option<Table>>("_PACKAGE")? else {
        return Ok(());
    };
    let path = env_path("LUA_PATH_5_5", "LUA_PATH", DEFAULT_PATH, ignore_env);
    pkg.raw_set("path", path)?;
    let cpath = env_path("LUA_CPATH_5_5", "LUA_CPATH", DEFAULT_CPATH, ignore_env);
    pkg.raw_set("cpath", cpath)?;
    pkg.raw_set("searchpath", s.create_function(searchpath))?;
    pkg.raw_set("loadlib", s.create_function(loadlib_absent))?;
    if let Some(searchers) = pkg.raw_get::<Option<Table>>("searchers")? {
        searchers.push(s.create_function(search_lua))?;
    }
    Ok(())
}
