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
#[cfg(feature = "io")]
mod io;
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
        Ok(())
    })
}

/// Make `lib` the global `name` and what `require(name)` returns.
#[cfg(any(feature = "io", feature = "os"))]
fn publish<'s>(s: &'s Scope<'s>, name: &str, lib: orbit::Table<'s>) -> Result<(), Error> {
    s.globals().raw_set(name, lib)?;
    s.register_module(name, lib);
    Ok(())
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
