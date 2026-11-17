//! The `table` library (manual §6.6). Its functions work on a table's
//! parts directly, so the runtime keeps them and the library takes each
//! by name ([`Scope::library_function`]).

use orbit::{Error, Scope, Table};

/// The library's fields and the runtime functions behind them.
/// `create` is Lua 5.5's: a 5.4 build's runtime doesn't have it.
const FUNCTIONS: [(&str, &str); 8] = [
    ("concat", "table.concat"),
    ("create", "table.create"),
    ("insert", "table.insert"),
    ("move", "table.move"),
    ("pack", "table.pack"),
    ("remove", "table.remove"),
    ("sort", "table.sort"),
    ("unpack", "table.unpack"),
];

/// Open the library. Returns the `table` table.
pub fn open<'s>(s: &'s Scope<'s>) -> Result<Table<'s>, Error> {
    let t = s.create_table();
    for (field, name) in FUNCTIONS {
        if let Some(f) = s.library_function(name) {
            t.raw_set(field, f)?;
        }
    }
    Ok(t)
}
