//! The `string` library (manual §6.4). Its functions share the runtime's
//! string internals (substrings, the pattern engine and its capture
//! cache, the locale's character tables), so the runtime keeps them and
//! the library takes each by name ([`Scope::library_function`]).

use orbit::{Error, Scope, Table};

/// The library's fields and the runtime functions behind them.
const FUNCTIONS: [(&str, &str); 17] = [
    ("byte", "string.byte"),
    ("char", "string.char"),
    ("dump", "string.dump"),
    ("find", "string.find"),
    ("format", "string.format"),
    ("gmatch", "string.gmatch"),
    ("gsub", "string.gsub"),
    ("len", "string.len"),
    ("lower", "string.lower"),
    ("match", "string.match"),
    ("pack", "string.pack"),
    ("packsize", "string.packsize"),
    ("rep", "string.rep"),
    ("reverse", "string.reverse"),
    ("sub", "string.sub"),
    ("unpack", "string.unpack"),
    ("upper", "string.upper"),
];

/// The string metatable's arithmetic metamethods (`"10" + 1`) and the
/// runtime functions behind them.
const METAMETHODS: [(&str, &str); 8] = [
    ("__add", "string.__add"),
    ("__sub", "string.__sub"),
    ("__mul", "string.__mul"),
    ("__mod", "string.__mod"),
    ("__pow", "string.__pow"),
    ("__div", "string.__div"),
    ("__idiv", "string.__idiv"),
    ("__unm", "string.__unm"),
];

/// Open the library. Returns the `string` table, and points the shared
/// string metatable at it (`("x"):upper()`), with the arithmetic
/// metamethods, when the base library made that metatable.
pub fn open<'s>(s: &'s Scope<'s>) -> Result<Table<'s>, Error> {
    let t = s.create_table();
    set_fields(s, &t, &FUNCTIONS)?;
    if let Some(mt) = s.create_string("").to_value().metatable() {
        mt.raw_set("__index", t)?;
        set_fields(s, &mt, &METAMETHODS)?;
    }
    Ok(t)
}

/// Set each field of `t` to the runtime function named beside it.
fn set_fields<'s>(s: &'s Scope<'s>, t: &Table<'s>, fields: &[(&str, &str)]) -> Result<(), Error> {
    for &(field, name) in fields {
        if let Some(f) = s.library_function(name) {
            t.raw_set(field, f)?;
        }
    }
    Ok(())
}
