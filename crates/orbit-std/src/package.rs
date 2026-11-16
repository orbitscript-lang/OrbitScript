//! `require` and the `package` library: `package.loaded`,
//! `package.preload`, `package.path` and `package.cpath`,
//! `package.searchpath`, and the preload and Lua searchers
//! ([design](https://github.com/orbitscript-lang/OrbitScript/blob/main/docs/design/chunks-io-os-cli.md#modules-require-and-package)).
//!
//! `package.loaded` and `package.preload` are also kept in the registry
//! (`_LOADED`, `_PRELOAD`), and the `package` table itself (`_PACKAGE`), so
//! `require` keeps working when scripts replace those fields, as in Lua.
//! The C searchers and `package.loadlib` come from `orbit-capi`, which
//! installs itself after this library.

use orbit::stack::{
    Any, Continuation, Either, Nil, Outcome, REGISTRY_INDEX, Return, Stack, StackExt, StackResult,
    Status, Type,
};
use orbit::{Chunk, Error, OrbitStr, OrbitString, Scope, Table, Value, native};

use crate::base::read_chunk_file;
use crate::sys::os_path;

/// The defaults the reference `lua5.5` build uses (`lua5.4`'s in a 5.4
/// build, ADR-14), so scripts see the same strings, with OrbitScript modules
/// after Lua's: the Lua searcher loads whatever file `package.path` finds,
/// picking the compiler by the `.orb` name.
#[cfg(not(windows))]
const DEFAULT_PATH: &str = if orbit::LUA.is_5_4() {
    "/usr/local/share/lua/5.4/?.lua;/usr/local/share/lua/5.4/?/init.lua;\
/usr/local/lib/lua/5.4/?.lua;/usr/local/lib/lua/5.4/?/init.lua;./?.lua;./?/init.lua;\
/usr/local/share/orbit/?.orb;/usr/local/share/orbit/?/init.orb;./?.orb;./?/init.orb"
} else {
    "/usr/local/share/lua/5.5/?.lua;/usr/local/share/lua/5.5/?/init.lua;\
/usr/local/lib/lua/5.5/?.lua;/usr/local/lib/lua/5.5/?/init.lua;./?.lua;./?/init.lua;\
/usr/local/share/orbit/?.orb;/usr/local/share/orbit/?/init.orb;./?.orb;./?/init.orb"
};
#[cfg(not(windows))]
const DEFAULT_CPATH: &str = if orbit::LUA.is_5_4() {
    "/usr/local/lib/lua/5.4/?.so;/usr/local/lib/lua/5.4/loadall.so;./?.so"
} else {
    "/usr/local/lib/lua/5.5/?.so;/usr/local/lib/lua/5.5/loadall.so;./?.so"
};

/// On Windows the reference build's defaults are relative to the program's
/// directory, which [`EXEC_DIR`] stands for, with OrbitScript's modules in
/// `share\orbit` beside Lua's `share\lua`.
#[cfg(windows)]
const DEFAULT_PATH: &str = if orbit::LUA.is_5_4() {
    "!\\lua\\?.lua;!\\lua\\?\\init.lua;!\\?.lua;!\\?\\init.lua;\
!\\..\\share\\lua\\5.4\\?.lua;!\\..\\share\\lua\\5.4\\?\\init.lua;.\\?.lua;.\\?\\init.lua;\
!\\..\\share\\orbit\\?.orb;!\\..\\share\\orbit\\?\\init.orb;.\\?.orb;.\\?\\init.orb"
} else {
    "!\\lua\\?.lua;!\\lua\\?\\init.lua;!\\?.lua;!\\?\\init.lua;\
!\\..\\share\\lua\\5.5\\?.lua;!\\..\\share\\lua\\5.5\\?\\init.lua;.\\?.lua;.\\?\\init.lua;\
!\\..\\share\\orbit\\?.orb;!\\..\\share\\orbit\\?\\init.orb;.\\?.orb;.\\?\\init.orb"
};
#[cfg(windows)]
const DEFAULT_CPATH: &str = if orbit::LUA.is_5_4() {
    "!\\?.dll;!\\..\\lib\\lua\\5.4\\?.dll;!\\loadall.dll;.\\?.dll"
} else {
    "!\\?.dll;!\\..\\lib\\lua\\5.5\\?.dll;!\\loadall.dll;.\\?.dll"
};

/// What a Windows search path writes for the program's directory: every
/// one in `package.path` and `package.cpath`, the environment's included,
/// becomes that directory, as in the reference build.
#[cfg(windows)]
const EXEC_DIR: &str = "!";

/// `path` with [`EXEC_DIR`] replaced by the directory of the running
/// program. A path stays as it is when that directory is unknown.
#[cfg(windows)]
fn with_exec_dir(path: String) -> String {
    let dir = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|d| d.to_string_lossy().into_owned()));
    match dir {
        Some(dir) => path.replace(EXEC_DIR, &dir),
        None => path,
    }
}

#[cfg(not(windows))]
fn with_exec_dir(path: String) -> String {
    path
}

/// `LUA_PATH_5_5` and the like: `name` with the build's version suffix
/// (`_5_4` in a 5.4 build), as Lua's `LUA_VERSUFFIX` makes it.
pub fn versioned_env(name: &str) -> String {
    format!("{name}_{}", orbit::LUA.underscored())
}

/// The directory separator, which `require` puts in place of dots.
pub const DIRSEP: &str = if cfg!(windows) { "\\" } else { "/" };

/// A search path from the environment (`LUA_PATH_5_5` or `_5_4`, then `LUA_PATH`),
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
pub fn search_path(
    name: &[u8],
    path: &[u8],
    sep: &[u8],
    rep: &[u8],
) -> Result<OrbitString, OrbitString> {
    let name = if sep.is_empty() {
        name.to_vec()
    } else {
        replace(name, sep, rep)
    };
    let mut msg = OrbitString::new();
    for template in path.split(|&b| b == b';') {
        let file = replace(template, b"?", &name);
        if std::fs::File::open(os_path(&file)).is_ok() && !file.contains(&0) {
            return Ok(file.into());
        }
        if !msg.is_empty() {
            msg.push("\n\t");
        }
        msg.push("no file '");
        msg.push(&file);
        msg.push("'");
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
type NotFound = (Nil, OrbitString);

#[native(name = "package.searchpath")]
fn searchpath(s: &mut Stack<'_>) -> StackResult<Either<OrbitString, NotFound>> {
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
type Loaded = (Any, OrbitString);

/// The Lua searcher. It also finds `.orb` modules: they are on
/// `package.path` (`DEFAULT_PATH`), and the loader picks the compiler by the
/// file name.
#[native(name = "package.searchers[2]")]
fn search_lua(
    s: &mut Stack<'_>,
    name: OrbitString,
) -> Result<Either<OrbitString, Loaded>, OrbitString> {
    let path = s.with_scope(|sc| path_field(sc, "path"))?;
    let file = match search_path(&name, &path, b".", DIRSEP.as_bytes()) {
        Ok(f) => f,
        Err(msg) => return Ok(Either::Left(msg)),
    };
    let (bytes, source) = read_chunk_file(Some(&file), s.line_endings())
        .map_err(|why| load_error(&name, &file, &why))?;
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
fn loadlib_absent(_lib: OrbitStr<'_>, _func: OrbitStr<'_>) -> (Nil, &'static str, &'static str) {
    (
        Nil,
        "dynamic libraries not enabled; check your Lua installation",
        "absent",
    )
}

/// Registry keys.
const LOADED: &str = "_LOADED";
const PRELOAD: &str = "_PRELOAD";
const PACKAGE: &str = "_PACKAGE";

/// The registry table under `key`, made the first time.
fn registry_table<'s>(s: &'s Scope<'s>, key: &str) -> Result<Table<'s>, Error> {
    let reg = s.registry();
    if let Some(t) = reg.raw_get::<Option<Table>>(key)? {
        return Ok(t);
    }
    let t = s.create_table();
    reg.raw_set(key, t)?;
    Ok(t)
}

/// Open the `package` library and the global `require`, with
/// `package.path` and `package.cpath` from the environment unless
/// `ignore_env`. Returns the `package` table.
pub fn open<'s>(s: &'s Scope<'s>, ignore_env: bool) -> Result<Table<'s>, Error> {
    let pkg = s.create_table();
    pkg.raw_set("config", format!("{DIRSEP}\n;\n?\n!\n-\n"))?;
    pkg.raw_set("loaded", registry_table(s, LOADED)?)?;
    pkg.raw_set("preload", registry_table(s, PRELOAD)?)?;
    let searchers = s.create_table();
    searchers.push(s.create_native::<search_preload>())?;
    searchers.push(s.create_native::<search_lua>())?;
    pkg.raw_set("searchers", searchers)?;
    let path = env_path(
        &versioned_env("LUA_PATH"),
        "LUA_PATH",
        DEFAULT_PATH,
        ignore_env,
    );
    pkg.raw_set("path", with_exec_dir(path))?;
    let cpath = env_path(
        &versioned_env("LUA_CPATH"),
        "LUA_CPATH",
        DEFAULT_CPATH,
        ignore_env,
    );
    pkg.raw_set("cpath", with_exec_dir(cpath))?;
    pkg.add_native::<searchpath>()?;
    pkg.add_native::<loadlib_absent>()?;
    s.registry().raw_set(PACKAGE, pkg)?;
    s.globals()
        .raw_set("require", s.create_native::<require>())?;
    Ok(pkg)
}

// Nameless in messages, as the reference's searchers are. Both lookups
// are ordinary indexing, so `__index` on `package.preload` applies.
#[native(name = "?")]
fn search_preload(s: &mut Stack<'_>) -> StackResult<Return> {
    s.check_string(1)?;
    s.get_field(REGISTRY_INDEX, PRELOAD)?;
    s.push_copy(1);
    if s.get_table(2)? == Type::Nil {
        let name = s.string_at(1).map_or(&b""[..], |n| n.as_bytes());
        let msg = [&b"no field package.preload['"[..], name, b"']"].concat();
        s.push_bytes(msg.as_slice());
        return Ok(Return::values(1));
    }
    s.push_bytes(":preload:");
    Ok(Return::values(2))
}

// `require`'s window while it runs searchers and the loader: the name, the
// searchers table, the next searcher's index and the messages so far; then
// the loader's extra value once a searcher succeeds.
const NAME: i32 = 1;
const SEARCHERS: i32 = 2;
const NEXT: i32 = 3;
const MSGS: i32 = 4;
const EXTRA: i32 = 5;

/// Push `package.loaded` (the registry's), which must be a table.
fn push_loaded(s: &mut Stack<'_>) -> StackResult<()> {
    if s.raw_get_field(REGISTRY_INDEX, LOADED)? != Type::Table {
        return Err(s.error("'package.loaded' must be a table"));
    }
    Ok(())
}

#[native(name = "require")]
fn require(s: &mut Stack<'_>) -> StackResult<Return> {
    s.check_string(NAME)?;
    s.set_top(NAME)?;
    push_loaded(s)?;
    s.push_copy(NAME);
    s.raw_get(2)?;
    if s.to_boolean(3) {
        return Ok(Return::values(1));
    }
    s.set_top(NAME)?;
    // The `searchers` of the `package` table `require` was opened with.
    let found = s.raw_get_field(REGISTRY_INDEX, PACKAGE)? == Type::Table
        && s.raw_get_field(2, "searchers")? == Type::Table;
    if !found {
        return Err(s.error("'package.searchers' must be a table"));
    }
    s.copy(3, SEARCHERS)?;
    s.set_top(SEARCHERS)?;
    s.push_integer(1);
    s.push_bytes("");
    next_searcher(s)
}

/// Call the next searcher.
fn next_searcher(s: &mut Stack<'_>) -> StackResult<Return> {
    let i = s.to_integer(NEXT).unwrap_or(1);
    s.set_top(MSGS)?;
    if s.raw_get_index(SEARCHERS, i)? == Type::Nil {
        let mut msg = OrbitString::from("module '");
        msg.push(s.string_at(NAME).unwrap_or_default());
        msg.push("' not found:");
        msg.push(s.string_at(MSGS).unwrap_or_default());
        return Err(s.error(&msg));
    }
    s.push_integer(i + 1);
    s.copy(-1, NEXT)?;
    s.set_top(-2)?;
    s.push_copy(NAME);
    Ok(Return::call_then::<SearcherDone>(1, false))
}

struct SearcherDone;

impl Continuation for SearcherDone {
    fn resume(s: &mut Stack<'_>, outcome: Outcome) -> StackResult<Return> {
        let Outcome::Ok(n) = outcome else {
            unreachable!("an unprotected call")
        };
        if n < 2 {
            // The loader and its extra value, nil when missing.
            for _ in n..2 {
                s.push_nil();
            }
        }
        let first = s.top() - n.max(2) as i32 + 1;
        match s.type_at(first) {
            Type::Function => {
                // [.., results] -> [.., extra, loader, name, extra]
                s.push_copy(first + 1);
                s.push_copy(first);
                s.push_copy(NAME);
                s.push_copy(first + 1);
                s.rotate(EXTRA, 4)?;
                s.set_top(EXTRA + 3)?;
                Ok(Return::call_then::<LoaderDone>(2, false))
            }
            Type::String | Type::Number => {
                let mut all = OrbitString::from(s.string_at(MSGS).unwrap_or_default());
                all.push("\n\t");
                all.push(&s.display(first));
                s.set_top(MSGS)?;
                s.push_bytes(&all);
                s.copy(-1, MSGS)?;
                next_searcher(s)
            }
            _ => next_searcher(s),
        }
    }
}

struct LoaderDone;

impl Continuation for LoaderDone {
    fn resume(s: &mut Stack<'_>, outcome: Outcome) -> StackResult<Return> {
        let Outcome::Ok(n) = outcome else {
            unreachable!("an unprotected call")
        };
        if n == 0 {
            s.push_nil();
        }
        let result = s.top() - n.max(1) as i32 + 1;
        push_loaded(s)?;
        let loaded = s.top();
        if s.type_at(result) != Type::Nil {
            s.push_copy(NAME);
            s.push_copy(result);
            s.raw_set(loaded)?;
        }
        s.push_copy(NAME);
        if s.raw_get(loaded)? == Type::Nil {
            s.set_top(-2)?;
            s.push_boolean(true);
            s.push_copy(NAME);
            s.push_copy(-2);
            s.raw_set(loaded)?;
        }
        s.push_copy(EXTRA);
        Ok(Return::values(2))
    }
}
