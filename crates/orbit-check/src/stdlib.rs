//! The bundled stubs: the prelude, which lists the names every module can read
//! without declaring them, and one stub per standard library module. They describe what the
//! runtime's libraries do; `tests/stubs.rs` checks them against the runtime and the front end.

/// Where the bundled stubs appear to live, in paths and diagnostics. No real file has it.
pub(crate) const ROOT: &str = "<builtin>";

pub(crate) const PRELUDE: &str = include_str!("../stubs/prelude.orbi");

/// Each module's `use` path, dotted, and its stub.
pub(crate) const MODULES: &[(&str, &str)] = &[
    ("coroutine", include_str!("../stubs/coroutine.orbi")),
    ("debug", include_str!("../stubs/debug.orbi")),
    ("io", include_str!("../stubs/io.orbi")),
    ("math", include_str!("../stubs/math.orbi")),
    ("os", include_str!("../stubs/os.orbi")),
    ("package", include_str!("../stubs/package.orbi")),
    ("std.ffi", include_str!("../stubs/std/ffi.orbi")),
    ("std.json", include_str!("../stubs/std/json.orbi")),
    ("string", include_str!("../stubs/string.orbi")),
    ("table", include_str!("../stubs/table.orbi")),
    ("utf8", include_str!("../stubs/utf8.orbi")),
];
