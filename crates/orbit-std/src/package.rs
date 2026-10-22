//! `require` finding files: `package.path` and `package.cpath`,
//! `package.searchpath`, and the Lua searcher
//! ([design](https://github.com/tymlipari/OrbitScript/blob/main/docs/design/chunks-io-os-cli.md#modules-require-and-package)).
//!
//! The runtime's `package` library keeps `require`, `package.loaded`,
//! `package.preload` and the preload searcher; this adds the searcher that
//! reads files. The C searchers and `package.loadlib` come from
//! `orbit-capi`, which installs itself after this library.

use orbit::stack::{Any, Either, Nil, Stack, StackResult, Status, Type};
use orbit::{Chunk, Error, OrbitString, Scope, Table, Value, native};

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

/// `nil` and the `no file` lines.
type NotFound = (Nil, Vec<u8>);

#[native(name = "package.searchpath")]
fn searchpath(s: &mut Stack<'_>) -> StackResult<Either<Vec<u8>, NotFound>> {
    // By hand rather than as parameters: lua5.5 checks the path first.
    let path = s.check_string(2)?.to_vec();
    let name = s.check_string(1)?.to_vec();
    let sep = opt_string(s, 3)?.unwrap_or_else(|| b".".to_vec());
    let rep = opt_string(s, 4)?.unwrap_or_else(|| DIRSEP.as_bytes().to_vec());
    Ok(match search_path(&name, &path, &sep, &rep) {
        Ok(file) => Either::Left(file),
        Err(msg) => Either::Right((Nil, msg)),
    })
}

fn opt_string(s: &mut Stack<'_>, i: i32) -> StackResult<Option<Vec<u8>>> {
    match s.type_at(i) {
        Type::None | Type::Nil => Ok(None),
        _ => Ok(Some(s.check_string(i)?.to_vec())),
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
pub fn load_error(name: &[u8], file: &[u8], why: &[u8]) -> OrbitString {
    let mut msg = OrbitString::from("error loading module '");
    msg.push(name);
    msg.push("' from file '");
    msg.push(file);
    msg.push("':\n\t");
    msg.push(why);
    msg
}

/// A searcher's loader (the function at its index) and the file it read.
type Loaded = (Any, Vec<u8>);

/// The Lua searcher. It also finds `.orb` modules: they are on
/// `package.path` (`DEFAULT_PATH`), and the loader picks the compiler by the
/// file name.
#[native(name = "package.searchers[2]")]
fn search_lua(s: &mut Stack<'_>, name: Vec<u8>) -> Result<Either<Vec<u8>, Loaded>, OrbitString> {
    let path = s.with_scope(|sc| path_field(sc, "path"))?;
    let file = match search_path(&name, &path, b".", DIRSEP.as_bytes()) {
        Ok(f) => f,
        Err(msg) => return Ok(Either::Left(msg)),
    };
    let (bytes, source) =
        read_chunk_file(Some(&file)).map_err(|why| load_error(&name, &file, &why))?;
    match s.load_chunk(&Chunk::new(&bytes).name(&source), 0) {
        Status::Ok => Ok(Either::Right((Any(s.top()), file))),
        _ => {
            let why = s.string_at(-1).unwrap_or_default().to_vec();
            Err(load_error(&name, &file, &why))
        }
    }
}

/// `package.loadlib` when no C modules are installed. Not registered as
/// `package.loadlib`: a native's name identifies it, and `orbit-capi`'s
/// working one has that name.
#[native(name = "loadlib")]
fn loadlib_absent(_lib: &[u8], _func: &[u8]) -> (Nil, &'static str, &'static str) {
    (
        Nil,
        "dynamic libraries not enabled; check your Lua installation",
        "absent",
    )
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
    pkg.raw_set("searchpath", s.create_native::<searchpath>())?;
    pkg.raw_set("loadlib", s.create_native::<loadlib_absent>())?;
    if let Some(searchers) = pkg.raw_get::<Option<Table>>("searchers")? {
        searchers.push(s.create_native::<search_lua>())?;
    }
    Ok(())
}
