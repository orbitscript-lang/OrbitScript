//! The standard library's I/O-facing parts, built on the public `orbit` API
//! only (ADR-11): `print`, `loadfile`, `dofile`, `debug.debug`, the `io`
//! and `os` libraries, and `require` finding files on `package.path`.
//!
//! The runtime reads no files and touches no process state; a host that
//! wants scripts to do so installs this crate:
//!
//! ```
//! let rt = orbit::Runtime::new();
//! let mut iso = rt.new_isolate(orbit::Config::default());
//! orbit_std::install(&mut iso, &orbit_std::Options::default()).unwrap();
//! iso.exec(orbit::Chunk::new("assert(io.write and os.time and print)")).unwrap();
//! ```
//!
//! C modules (`package.cpath`'s searchers, `package.loadlib`) come from
//! `orbit-capi`, which builds on this crate.

mod base;
#[cfg(windows)]
mod crt;
#[cfg(feature = "io")]
mod io;
#[cfg(feature = "json")]
mod json;
#[cfg(feature = "os")]
mod os;
mod package;
mod sys;
#[cfg(feature = "os")]
mod time;
#[cfg(feature = "os")]
mod tz;

use orbit::{Error, Isolate, Scope};

pub use base::{STDIN, read_chunk_file};
pub use package::{DIRSEP, load_error, path_field, search_path};
pub use sys::os_error_text;

/// What [`install`] gives scripts. The default is everything this build
/// has, as `lua` gives.
#[derive(Debug, Clone)]
pub struct Options {
    /// `print`, `loadfile`, `dofile`, `debug.debug`, and the Lua searcher.
    pub base: bool,
    /// The `io` library (feature `io`).
    pub io: bool,
    /// The `os` library (feature `os`).
    pub os: bool,
    /// The `std.json` module (feature `json`), reached through `require`
    /// only.
    pub json: bool,
    /// Ignore environment variables (`LUA_PATH`, `LUA_CPATH` and their
    /// versioned forms), as `lua -E` does.
    pub ignore_env: bool,
}

impl Default for Options {
    fn default() -> Options {
        Options {
            base: true,
            io: cfg!(feature = "io"),
            os: cfg!(feature = "os"),
            json: cfg!(feature = "json"),
            ignore_env: false,
        }
    }
}

/// Install the libraries `options` asks for in `iso`: as globals, and in
/// `package.loaded` when the `package` library is open.
pub fn install(iso: &mut Isolate, options: &Options) -> Result<(), Error> {
    iso.scope(|s| {
        if options.base {
            open_base(s)?;
            open_package(s, options.ignore_env)?;
            open_debug(s)?;
        }
        #[cfg(feature = "io")]
        if options.io {
            let t = open_io(s)?;
            publish(s, "io", t)?;
        }
        #[cfg(feature = "os")]
        if options.os {
            let t = open_os(s)?;
            publish(s, "os", t)?;
        }
        Ok::<(), Error>(())
    })?;
    #[cfg(feature = "json")]
    if options.json {
        let r = iso.stack(|s| {
            open_json(s)?;
            set_loaded(s, b"std.json")
        });
        // Only running out of memory under a heap limit can fail, and a new
        // isolate's limit leaves room for one small library.
        debug_assert!(r.is_ok(), "installing std.json raised {r:?}");
    }
    Ok(())
}

/// Push the `std.json` module's table: `encode`, `decode`, `null`,
/// `object` and `array`. [`install`] makes it what `require "std.json"`
/// returns.
#[cfg(feature = "json")]
pub fn open_json(s: &mut orbit::stack::Stack<'_>) -> orbit::stack::StackResult<()> {
    json::open(s)
}

/// `package.loaded[name] =` the value on top, popped.
#[cfg(feature = "json")]
fn set_loaded(s: &mut orbit::stack::Stack<'_>, name: &[u8]) -> orbit::stack::StackResult<()> {
    use orbit::stack::{REGISTRY_INDEX, Type};
    if s.raw_get_field(REGISTRY_INDEX, b"_LOADED")? != Type::Table {
        s.set_top(-2)?;
        s.create_table(0, 1);
        s.push_copy(-1);
        s.raw_set_field(REGISTRY_INDEX, b"_LOADED")?;
    }
    s.rotate(-2, 1)?;
    s.raw_set_field(-2, name)?;
    s.set_top(-2)
}

/// Make `lib` the global `name` and what `require(name)` returns.
#[cfg(any(feature = "io", feature = "os"))]
fn publish<'s>(s: &'s Scope<'s>, name: &str, lib: orbit::Table<'s>) -> Result<(), Error> {
    s.globals().raw_set(name, lib)?;
    s.register_module(name, lib);
    Ok(())
}

/// Give scripts the `ffi` library (the `ffi` global and
/// `package.loaded.ffi`): calling C functions in any shared library the
/// process can load, and reading and writing memory through pointers. That
/// is as much power as a C module has, so a host enables it only for
/// scripts it trusts that far (ADR-9). Calling it again does nothing.
#[cfg(feature = "ffi")]
pub fn install_ffi(iso: &mut Isolate) {
    let r = iso.stack(orbit_ffi::install);
    // Only running out of memory under a heap limit can fail, and a new
    // isolate's limit leaves room for one small library.
    debug_assert!(r.is_ok(), "installing ffi raised {r:?}");
}

/// Add `print`, `loadfile` and `dofile` to the global table.
pub fn open_base<'s>(s: &'s Scope<'s>) -> Result<(), Error> {
    base::open(s)
}

/// Set `package.path` and `package.cpath` (from the environment unless
/// `ignore_env`) and add `package.searchpath`, a `package.loadlib` that
/// reports C modules absent (until `orbit-capi` replaces it), and the Lua
/// searcher. Does nothing unless the `package` library is open.
pub fn open_package<'s>(s: &'s Scope<'s>, ignore_env: bool) -> Result<(), Error> {
    package::open(s, ignore_env)
}

/// Add `debug.debug` to the `debug` library, if it is open.
pub fn open_debug<'s>(s: &'s Scope<'s>) -> Result<(), Error> {
    base::open_debug(s)
}

/// The `io` library's table, with the `FILE*` class registered. Open
/// files are flushed if a script ends the process (`os.exit`).
#[cfg(feature = "io")]
pub fn open_io<'s>(s: &'s Scope<'s>) -> Result<orbit::Table<'s>, Error> {
    io::open(s)
}

/// The `os` library's table.
#[cfg(feature = "os")]
pub fn open_os<'s>(s: &'s Scope<'s>) -> Result<orbit::Table<'s>, Error> {
    os::open(s)
}

/// Flush every open file's buffer, as the C library does at exit (standard
/// output is the isolate's to flush: [`Isolate::flush`]).
#[cfg(feature = "io")]
pub fn flush_files() {
    io::flush_all();
}
