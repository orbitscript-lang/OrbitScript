//! The `std.json` module: JSON text to and from values, over
//! `serde_json` (ADR-12, D13).
//!
//! Reached as `require "std.json"` (`use std::json;`), never as a
//! global, so programs that bring their own `json` module keep it.
//!
//! - `encode(value [, options])` gives the JSON text, raising an error for
//!   anything JSON can't hold. `options.pretty` indents by two spaces;
//!   `options.sort_keys` writes object keys in byte order.
//! - `decode(text)` gives the value, or `nil` and a message.
//! - `null` stands for JSON's `null`, so it survives inside tables.
//! - `object([t])` and `array([t])` mark a table (a new one without `t`)
//!   to encode as an object or an array whatever its keys, which matters
//!   for empty tables. Decoding marks empty objects, so they round-trip.
//!
//! A table encodes as an array when its keys are exactly `1..n`, and
//! otherwise as an object, whose keys must be strings or numbers. Integers
//! and floats keep their subtype both ways (`1` and `1.0`). A metatable's
//! `__tojson` metamethod gives the value to write instead (tables and
//! userdata; a table it gives must be marked with `json.array` or
//! `json.object`, or have a `__tojson` of its own), and a `__jsontype` field (`"object"`, `"array"` or `"null"`) overrides the
//! shape, as the markers and `null` use.

use std::fmt::Write as _;

use orbit::stack::{Native, Raised, Return, Stack, StackResult, Type, upvalue_index};
use serde::de::{self, DeserializeSeed, MapAccess, SeqAccess, Visitor};

/// How deep values nest, both ways: `serde_json`'s own limit for decoding.
const MAX_DEPTH: usize = 128;

/// The upvalues the module's functions share.
const NULL: i32 = upvalue_index(1);
const OBJECT_META: i32 = upvalue_index(2);
const ARRAY_META: i32 = upvalue_index(3);
const NUMBER_META: i32 = upvalue_index(4);
/// How many upvalues each function has.
const UPVALUES: i32 = 4;

macro_rules! natives {
    ($($native:ident $name:literal $f:ident;)*) => {$(
        struct $native;

        impl Native for $native {
            const NAME: &'static str = $name;

            fn call(s: &mut Stack<'_>) -> StackResult<Return> {
                $f(s)
            }
        }
    )*};
}

natives! {
    Encode "json.encode" encode;
    Decode "json.decode" decode;
    MarkObject "json.object" mark_object;
    MarkArray "json.array" mark_array;
    Number "json.number" number;
    ToInt "json.int" to_int;
    ToFloat "json.float" to_float;
    ToString "json.string" to_string;
    ToBool "json.bool" to_bool;
    NumberToString "json.number.__tostring" number_to_string;
    NullToString "json.null.__tostring" null_to_string;
    NullNewIndex "json.null.__newindex" null_new_index;
}

/// Push the module's table.
pub(crate) fn open(s: &mut Stack<'_>) -> StackResult<()> {
    // The shared upvalues: null, then the object and array markers.
    s.create_table(0, 0);
    s.create_table(0, 3);
    s.push_bytes(b"null");
    s.raw_set_field(-2, b"__jsontype")?;
    s.push_native::<NullToString>();
    s.raw_set_field(-2, b"__tostring")?;
    s.push_native::<NullNewIndex>();
    s.raw_set_field(-2, b"__newindex")?;
    s.set_metatable(-2)?;
    for shape in [&b"object"[..], b"array"] {
        s.create_table(0, 1);
        s.push_bytes(shape);
        s.raw_set_field(-2, b"__jsontype")?;
    }
    s.create_table(0, 2);
    s.push_bytes(b"number");
    s.raw_set_field(-2, b"__jsontype")?;
    s.push_native::<NumberToString>();
    s.raw_set_field(-2, b"__tostring")?;
    let ups = s.top() - (UPVALUES - 1);
    s.create_table(0, 6);
    let lib = s.top();
    s.push_copy(ups);
    s.raw_set_field(lib, b"null")?;
    push_closure::<Encode>(s, ups)?;
    s.raw_set_field(lib, b"encode")?;
    push_closure::<Decode>(s, ups)?;
    s.raw_set_field(lib, b"decode")?;
    push_closure::<MarkObject>(s, ups)?;
    s.raw_set_field(lib, b"object")?;
    push_closure::<MarkArray>(s, ups)?;
    s.raw_set_field(lib, b"array")?;
    push_closure::<Number>(s, ups)?;
    s.raw_set_field(lib, b"number")?;
    push_closure::<ToInt>(s, ups)?;
    s.raw_set_field(lib, b"int")?;
    push_closure::<ToFloat>(s, ups)?;
    s.raw_set_field(lib, b"float")?;
    push_closure::<ToString>(s, ups)?;
    s.raw_set_field(lib, b"string")?;
    push_closure::<ToBool>(s, ups)?;
    s.raw_set_field(lib, b"bool")?;
    // Leave only the module.
    s.rotate(ups, 1)?;
    s.set_top(ups)
}

/// Push `N` as a closure over the upvalues starting at `ups`.
fn push_closure<N: Native>(s: &mut Stack<'_>, ups: i32) -> StackResult<()> {
    for i in 0..UPVALUES {
        s.push_copy(ups + i);
    }
    s.push_native_closure::<N>(UPVALUES as usize, 0)
}

fn null_to_string(s: &mut Stack<'_>) -> StackResult<Return> {
    s.push_bytes(b"null");
    Ok(Return::values(1))
}

fn null_new_index(s: &mut Stack<'_>) -> StackResult<Return> {
    Err(s.error("json.null is read-only"))
}

fn mark_object(s: &mut Stack<'_>) -> StackResult<Return> {
    mark(s, OBJECT_META)
}

/// `json.array([t [, n]])`: with `n`, the array has length `n` and its
/// missing elements are written as `null`.
fn mark_array(s: &mut Stack<'_>) -> StackResult<Return> {
    if matches!(s.type_at(2), Type::None | Type::Nil) {
        return mark(s, ARRAY_META);
    }
    let n = s.check_integer(2)?;
    if n < 0 {
        return Err(s.arg_error(2, "length must not be negative"));
    }
    match s.type_at(1) {
        Type::None | Type::Nil => {
            s.set_top(0)?;
            s.create_table(0, 0);
        }
        Type::Table => {
            s.set_top(1)?;
            if s.get_metatable(1) {
                s.raw_get_field(-1, b"__jsontype")?;
                if s.string_at(-1) != Some(b"array") {
                    return Err(s.arg_error(
                        1,
                        "table already has a metatable (set its __jsontype field instead)",
                    ));
                }
                s.set_top(1)?;
            }
        }
        _ => return Err(s.type_error(1, "table")),
    }
    mark_length(s, n)?;
    Ok(Return::values(1))
}

/// Mark the table on top as an array of length `n`, with a metatable of its own.
fn mark_length(s: &mut Stack<'_>, n: i64) -> StackResult<()> {
    s.create_table(0, 2);
    s.push_bytes(b"array");
    s.raw_set_field(-2, b"__jsontype")?;
    s.push_integer(n);
    s.raw_set_field(-2, b"__jsonlength")?;
    s.set_metatable(-2)
}

/// `json.number(numeral)`: a number written exactly as `numeral`, for values
/// a Lua number can't hold (20-digit IDs, decimal amounts).
fn number(s: &mut Stack<'_>) -> StackResult<Return> {
    let text = s.check_string(1)?.to_vec();
    if !is_numeral(&text) {
        return Err(s.arg_error(1, "not a JSON number"));
    }
    s.create_table(1, 0);
    s.push_bytes(&text);
    s.raw_set_index(-2, 1)?;
    s.push_copy(NUMBER_META);
    s.set_metatable(-2)?;
    Ok(Return::values(1))
}

// The scalar conversions give plain values: Lua's own types already decide
// each scalar's JSON type, so `json.int(x)` and `math.tointeger(x)` encode
// the same way, and either is a fine way to say what to write.

/// `json.int(v)`: an integer, a float with an integral value, or a numeral, as an integer.
fn to_int(s: &mut Stack<'_>) -> StackResult<Return> {
    match s.type_at(1) {
        Type::Number | Type::String => {}
        _ => return Err(s.type_error(1, "number")),
    }
    let Some(n) = s.to_integer(1) else {
        return Err(s.arg_error(1, "number has no integer representation"));
    };
    s.push_integer(n);
    Ok(Return::values(1))
}

/// `json.float(v)`: a number or a numeral, as a float.
fn to_float(s: &mut Stack<'_>) -> StackResult<Return> {
    let n = match s.type_at(1) {
        Type::Number | Type::String => s.to_number(1),
        _ => None,
    };
    let Some(n) = n else {
        return Err(s.type_error(1, "number"));
    };
    s.push_number(n);
    Ok(Return::values(1))
}

/// `json.string(v)`: a string, number or boolean, or a value with
/// `__tostring`, as `tostring` writes it.
fn to_string(s: &mut Stack<'_>) -> StackResult<Return> {
    let ok = match s.type_at(1) {
        Type::String => return Ok(Return::values(1)),
        Type::Number | Type::Boolean => true,
        _ if s.get_metatable(1) => {
            s.raw_get_field(-1, b"__tostring")?;
            let has = s.type_at(-1) != Type::Nil;
            s.set_top(1)?;
            has
        }
        _ => false,
    };
    if !ok {
        return Err(s.type_error(1, "string, number or boolean"));
    }
    s.set_top(1)?;
    s.to_display_string(1)?;
    Ok(Return::values(1))
}

/// `json.bool(v)`: `false` for `nil`, `false` and `json.null`, and `true` for
/// anything else, as conditions treat values (so `0` and `""` are `true`).
fn to_bool(s: &mut Stack<'_>) -> StackResult<Return> {
    let b = s.to_boolean(1) && !s.raw_equal(1, NULL);
    s.push_boolean(b);
    Ok(Return::values(1))
}

fn number_to_string(s: &mut Stack<'_>) -> StackResult<Return> {
    s.raw_get_index(1, 1)?;
    Ok(Return::values(1))
}

/// Whether `text` is exactly one JSON number: `-? int frac? exp?`.
fn is_numeral(text: &[u8]) -> bool {
    let mut i = 0;
    let digits = |i: &mut usize| {
        let start = *i;
        while text.get(*i).is_some_and(u8::is_ascii_digit) {
            *i += 1;
        }
        *i > start
    };
    if text.get(i) == Some(&b'-') {
        i += 1;
    }
    match text.get(i) {
        Some(b'0') => i += 1,
        Some(b'1'..=b'9') => {
            digits(&mut i);
        }
        _ => return false,
    }
    if text.get(i) == Some(&b'.') {
        i += 1;
        if !digits(&mut i) {
            return false;
        }
    }
    if matches!(text.get(i), Some(b'e' | b'E')) {
        i += 1;
        if matches!(text.get(i), Some(b'+' | b'-')) {
            i += 1;
        }
        if !digits(&mut i) {
            return false;
        }
    }
    i == text.len()
}

/// Give the table argument (or a new table) the marker metatable `meta`.
fn mark(s: &mut Stack<'_>, meta: i32) -> StackResult<Return> {
    match s.type_at(1) {
        Type::None | Type::Nil => {
            s.set_top(0)?;
            s.create_table(0, 0);
        }
        Type::Table => {
            s.set_top(1)?;
            if s.get_metatable(1) {
                if s.raw_equal(-1, meta) {
                    s.set_top(1)?;
                    return Ok(Return::values(1));
                }
                return Err(s.arg_error(
                    1,
                    "table already has a metatable (set its __jsontype field instead)",
                ));
            }
        }
        _ => return Err(s.type_error(1, "table")),
    }
    s.push_copy(meta);
    s.set_metatable(1)?;
    Ok(Return::values(1))
}

// ---- encoding -------------------------------------------------------------------------------

fn encode(s: &mut Stack<'_>) -> StackResult<Return> {
    if s.type_at(1) == Type::None {
        return Err(s.arg_error(1, "value expected"));
    }
    let mut e = Encoder {
        out: Vec::new(),
        pretty: false,
        sort_keys: false,
        open: Vec::new(),
    };
    match s.type_at(2) {
        Type::None | Type::Nil => {}
        Type::Table => {
            s.raw_get_field(2, b"pretty")?;
            e.pretty = s.to_boolean(-1);
            s.raw_get_field(2, b"sort_keys")?;
            e.sort_keys = s.to_boolean(-1);
        }
        _ => return Err(s.type_error(2, "table")),
    }
    s.set_top(1)?;
    match e.value(s, 1) {
        Ok(()) => {
            s.push_bytes(&e.out);
            Ok(Return::values(1))
        }
        Err(Fail::Raised(r)) => Err(r),
        Err(Fail::Bad { message, path }) => {
            let mut at = String::new();
            for seg in path.iter().rev() {
                at.push_str(seg);
            }
            let message = if at.is_empty() {
                message
            } else {
                format!("{message} at value{at}")
            };
            Err(s.error(message))
        }
    }
}

/// Why encoding stopped.
enum Fail {
    /// A stack operation raised (out of memory).
    Raised(Raised),
    /// The value has no JSON form; `path` is where, innermost first.
    Bad { message: String, path: Vec<String> },
}

impl From<Raised> for Fail {
    fn from(r: Raised) -> Fail {
        Fail::Raised(r)
    }
}

fn bad(message: impl Into<String>) -> Fail {
    Fail::Bad {
        message: message.into(),
        path: Vec::new(),
    }
}

/// Add a path segment to a failure coming up from inside a table.
fn within(r: Result<(), Fail>, seg: impl FnOnce() -> String) -> Result<(), Fail> {
    r.map_err(|f| match f {
        Fail::Bad { message, mut path } => {
            path.push(seg());
            Fail::Bad { message, path }
        }
        raised => raised,
    })
}

/// The shape a metatable asks for.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Shape {
    Any,
    Object,
    /// With the length `json.array(t, n)` gave, if any.
    Array(Option<i64>),
    Null,
    /// `json.number`: the numeral in `[1]`.
    Number,
}

struct Encoder {
    out: Vec<u8>,
    pretty: bool,
    sort_keys: bool,
    /// The tables being written, outermost first, to refuse cycles.
    open: Vec<u64>,
}

impl Encoder {
    /// Write the value at absolute index `idx`.
    fn value(&mut self, s: &mut Stack<'_>, idx: i32) -> Result<(), Fail> {
        match s.type_at(idx) {
            Type::None | Type::Nil => self.out.extend_from_slice(b"null"),
            Type::Boolean => {
                let b = s.to_boolean(idx);
                self.out
                    .extend_from_slice(if b { b"true" } else { b"false" });
            }
            Type::Number => {
                if s.is_integer(idx) {
                    let n = s.to_integer(idx).expect("an integer");
                    let _ = write_int(&mut self.out, n);
                } else {
                    let f = s.to_number(idx).expect("a number");
                    self.float(f)?;
                }
            }
            Type::String => {
                let bytes = s.string_at(idx).expect("a string");
                write_string(&mut self.out, bytes)?;
            }
            Type::Table => {
                if !self.hook(s, idx)? {
                    self.table(s, idx)?;
                }
            }
            Type::Userdata if self.hook(s, idx)? => {}
            t => return Err(bad(format!("cannot encode a {}", t.name()))),
        }
        Ok(())
    }

    /// Write what the `__tojson` metamethod of the value at `idx` gives, if it has
    /// one; false if it hasn't.
    fn hook(&mut self, s: &mut Stack<'_>, idx: i32) -> Result<bool, Fail> {
        if !s.get_metatable(idx) {
            return Ok(false);
        }
        let base = s.top() - 1;
        s.raw_get_field(-1, b"__tojson")?;
        if s.type_at(-1) != Type::Function {
            s.set_top(base)?;
            return Ok(false);
        }
        // The value stays open while its replacement is written, so a hook that
        // gives back the value itself, or a table holding it, is a cycle.
        let id = s
            .identity(idx)
            .expect("a table or userdata has an identity");
        if self.open.contains(&id) {
            return Err(bad("cannot encode a table that contains itself"));
        }
        if self.open.len() >= MAX_DEPTH {
            return Err(bad(format!(
                "cannot encode tables nested over {MAX_DEPTH} deep"
            )));
        }
        s.push_copy(idx);
        s.call(1, 1)?;
        // A table must say its shape, so the hook reads as what it writes.
        if s.type_at(-1) == Type::Table && shape(s, -1)? == Shape::Any && !has_hook(s, -1)? {
            return Err(bad(
                "__tojson returned a table that is not json.array(...) or json.object(...)",
            ));
        }
        self.open.push(id);
        let r = self.value(s, s.top());
        self.open.pop();
        r?;
        s.set_top(base)?;
        Ok(true)
    }

    fn float(&mut self, f: f64) -> Result<(), Fail> {
        if !f.is_finite() {
            return Err(bad(format!("cannot encode {}", float_name(f))));
        }
        // serde_json writes the shortest form that reads back the same,
        // always with a `.` or an exponent, so `1.0` stays a float.
        serde_json::to_writer(&mut self.out, &f).expect("writing to a Vec");
        Ok(())
    }

    fn table(&mut self, s: &mut Stack<'_>, t: i32) -> Result<(), Fail> {
        let shape = shape(s, t)?;
        match shape {
            Shape::Null => {
                self.out.extend_from_slice(b"null");
                return Ok(());
            }
            Shape::Number => {
                s.raw_get_index(t, 1)?;
                match s.string_at(-1) {
                    Some(n) if is_numeral(n) => self.out.extend_from_slice(n),
                    _ => return Err(bad("cannot encode a json.number that holds no numeral")),
                }
                s.set_top(-2)?;
                return Ok(());
            }
            _ => {}
        }
        let id = s.identity(t).expect("a table has an identity");
        if self.open.contains(&id) {
            return Err(bad("cannot encode a table that contains itself"));
        }
        if self.open.len() >= MAX_DEPTH {
            return Err(bad(format!(
                "cannot encode tables nested over {MAX_DEPTH} deep"
            )));
        }
        self.open.push(id);
        let base = s.top();
        let r = self.table_body(s, t, shape);
        self.open.pop();
        if !matches!(r, Err(Fail::Raised(_))) {
            s.set_top(base)?;
        }
        r
    }

    fn table_body(&mut self, s: &mut Stack<'_>, t: i32, shape: Shape) -> Result<(), Fail> {
        // Count the keys and see whether they are all positive integers.
        let (mut count, mut max, mut sequence) = (0_i64, 0_i64, true);
        s.push_nil();
        while s.next(t)? {
            count += 1;
            match s.to_integer(-2) {
                Some(k) if s.is_integer(-2) && k > 0 => max = max.max(k),
                _ => sequence = false,
            }
            s.set_top(-2)?;
        }
        // A marked array may have holes, written as `null`, up to its length.
        let length = match shape {
            Shape::Array(_) if !sequence => {
                return Err(bad(
                    "cannot encode a table marked as an array whose keys aren't positive integers",
                ));
            }
            Shape::Array(Some(n)) if max > n => {
                return Err(bad(format!(
                    "cannot encode an array of length {n} with an element at [{max}]"
                )));
            }
            Shape::Array(n) => Some(n.unwrap_or(max)),
            Shape::Object => None,
            _ => (sequence && max == count).then_some(count),
        };
        let depth = self.open.len();
        if let Some(length) = length {
            let count = length;
            self.out.push(b'[');
            for i in 1..=count {
                self.separator(depth, i == 1);
                s.raw_get_index(t, i)?;
                let v = s.top();
                within(self.value(s, v), || format!("[{i}]"))?;
                s.set_top(-2)?;
            }
            self.close(depth, count == 0, b']');
        } else {
            self.out.push(b'{');
            if self.sort_keys {
                self.sorted_members(s, t, depth)?;
            } else {
                let mut first = true;
                s.push_nil();
                while s.next(t)? {
                    self.member(s, -2, -1, depth, first)?;
                    first = false;
                    s.set_top(-2)?;
                }
            }
            self.close(depth, count == 0, b'}');
        }
        Ok(())
    }

    /// The object's members in the byte order of their keys as written.
    fn sorted_members(&mut self, s: &mut Stack<'_>, t: i32, depth: usize) -> Result<(), Fail> {
        // Keep a copy of each key in the window, below the one `next` uses.
        let first_key = s.top() + 1;
        s.push_nil();
        while s.next(t)? {
            s.set_top(-2)?;
            s.push_copy(-1);
        }
        let keys = s.top() - first_key + 1;
        let mut order = Vec::with_capacity(keys as usize);
        for k in first_key..first_key + keys {
            order.push((key_text(s, k)?, k));
        }
        order.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        for (n, (text, k)) in order.into_iter().enumerate() {
            self.separator(depth, n == 0);
            write_string(&mut self.out, text.as_bytes())?;
            self.out
                .extend_from_slice(if self.pretty { b": " } else { b":" });
            s.push_copy(k);
            s.raw_get(t)?;
            let v = s.top();
            within(self.value(s, v), || segment(&text))?;
            s.set_top(-2)?;
        }
        Ok(())
    }

    /// Write the member whose key and value are at `k` and `v`.
    fn member(
        &mut self,
        s: &mut Stack<'_>,
        k: i32,
        v: i32,
        depth: usize,
        first: bool,
    ) -> Result<(), Fail> {
        let (k, v) = (s.abs_index(k), s.abs_index(v));
        let text = key_text(s, k)?;
        self.separator(depth, first);
        write_string(&mut self.out, text.as_bytes())?;
        self.out
            .extend_from_slice(if self.pretty { b": " } else { b":" });
        within(self.value(s, v), || segment(&text))
    }

    /// Before an element: a comma unless it's the first, then the newline
    /// and indent when pretty.
    fn separator(&mut self, depth: usize, first: bool) {
        if !first {
            self.out.push(b',');
        }
        if self.pretty {
            self.newline(depth);
        }
    }

    fn close(&mut self, depth: usize, empty: bool, bracket: u8) {
        if self.pretty && !empty {
            self.newline(depth - 1);
        }
        self.out.push(bracket);
    }

    fn newline(&mut self, depth: usize) {
        self.out.push(b'\n');
        self.out.resize(self.out.len() + 2 * depth, b' ');
    }
}

/// Whether the value at `idx` has a `__tojson` function of its own.
fn has_hook(s: &mut Stack<'_>, idx: i32) -> StackResult<bool> {
    if !s.get_metatable(idx) {
        return Ok(false);
    }
    s.raw_get_field(-1, b"__tojson")?;
    let hook = s.type_at(-1) == Type::Function;
    s.set_top(-3)?;
    Ok(hook)
}

/// The shape the metatable of the table at `t` asks for.
fn shape(s: &mut Stack<'_>, t: i32) -> StackResult<Shape> {
    if !s.get_metatable(t) {
        return Ok(Shape::Any);
    }
    s.raw_get_field(-1, b"__jsontype")?;
    let shape = match s.string_at(-1) {
        Some(b"object") => Shape::Object,
        Some(b"array") => {
            s.raw_get_field(-2, b"__jsonlength")?;
            let n = s.is_integer(-1).then(|| s.to_integer(-1)).flatten();
            s.set_top(-2)?;
            Shape::Array(n.filter(|&n| n >= 0))
        }
        Some(b"null") => Shape::Null,
        Some(b"number") => Shape::Number,
        _ => Shape::Any,
    };
    s.set_top(-3)?;
    Ok(shape)
}

/// The object key the key at `k` becomes.
fn key_text(s: &mut Stack<'_>, k: i32) -> Result<String, Fail> {
    match s.type_at(k) {
        Type::String => {
            let bytes = s.string_at(k).expect("a string");
            match std::str::from_utf8(bytes) {
                Ok(text) => Ok(text.to_owned()),
                Err(_) => Err(bad("cannot encode a key that isn't valid UTF-8")),
            }
        }
        Type::Number if s.is_integer(k) => Ok(s.to_integer(k).expect("an integer").to_string()),
        Type::Number => {
            let f = s.to_number(k).expect("a number");
            if !f.is_finite() {
                return Err(bad(format!("cannot encode a key {}", float_name(f))));
            }
            Ok(serde_json::to_string(&f).expect("a finite float"))
        }
        t => Err(bad(format!("cannot encode a key of type {}", t.name()))),
    }
}

/// A path segment for key `text`: `.name` when it reads as a name.
fn segment(text: &str) -> String {
    let name = text
        .bytes()
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == b'_')
        && text.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_');
    if name {
        format!(".{text}")
    } else {
        let mut quoted = String::from("[");
        let _ = write!(quoted, "{text:?}");
        quoted.push(']');
        quoted
    }
}

fn float_name(f: f64) -> &'static str {
    if f.is_nan() {
        "NaN"
    } else if f > 0.0 {
        "inf"
    } else {
        "-inf"
    }
}

fn write_int(out: &mut Vec<u8>, n: i64) -> std::io::Result<()> {
    use std::io::Write as _;
    write!(out, "{n}")
}

fn write_string(out: &mut Vec<u8>, bytes: &[u8]) -> Result<(), Fail> {
    let Ok(text) = std::str::from_utf8(bytes) else {
        return Err(bad("cannot encode a string that isn't valid UTF-8"));
    };
    serde_json::to_writer(out, text).expect("writing to a Vec");
    Ok(())
}

// ---- decoding -------------------------------------------------------------------------------

fn decode(s: &mut Stack<'_>) -> StackResult<Return> {
    let text = s.check_string(1)?.to_vec();
    let keep_null = match s.type_at(2) {
        Type::None | Type::Nil => true,
        Type::Table => {
            s.raw_get_field(2, b"keep_null")?;
            s.type_at(-1) == Type::Nil || s.to_boolean(-1)
        }
        _ => return Err(s.type_error(2, "table")),
    };
    s.set_top(1)?;
    let mut raised = None;
    let mut de = serde_json::Deserializer::from_slice(&text);
    let r = Build {
        s: &mut *s,
        raised: &mut raised,
        keep_null,
    }
    .deserialize(&mut de)
    .and_then(|()| de.end());
    if let Some(r) = raised {
        return Err(r);
    }
    match r {
        Ok(()) => Ok(Return::values(1)),
        Err(e) => {
            s.set_top(1)?;
            s.push_nil();
            s.push_bytes(e.to_string().as_bytes());
            Ok(Return::values(2))
        }
    }
}

/// Pushes the value it reads. A stack operation that raises (out of
/// memory) is kept in `raised` and stops the parse.
struct Build<'a, 'b> {
    s: &'a mut Stack<'b>,
    raised: &'a mut Option<Raised>,
    /// JSON `null` is `json.null`; otherwise it is `nil`.
    keep_null: bool,
}

impl<'b> Build<'_, 'b> {
    fn again(&mut self) -> Build<'_, 'b> {
        Build {
            s: &mut *self.s,
            raised: &mut *self.raised,
            keep_null: self.keep_null,
        }
    }

    fn check<E: de::Error>(&mut self, r: StackResult<()>) -> Result<(), E> {
        r.map_err(|e| {
            *self.raised = Some(e);
            E::custom("out of memory")
        })
    }
}

impl<'de> DeserializeSeed<'de> for Build<'_, '_> {
    type Value = ();

    fn deserialize<D: de::Deserializer<'de>>(self, d: D) -> Result<(), D::Error> {
        d.deserialize_any(self)
    }
}

impl<'de> Visitor<'de> for Build<'_, '_> {
    type Value = ();

    fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("a JSON value")
    }

    fn visit_unit<E: de::Error>(self) -> Result<(), E> {
        if self.keep_null {
            self.s.push_copy(NULL);
        } else {
            self.s.push_nil();
        }
        Ok(())
    }

    fn visit_bool<E: de::Error>(self, b: bool) -> Result<(), E> {
        self.s.push_boolean(b);
        Ok(())
    }

    fn visit_i64<E: de::Error>(self, n: i64) -> Result<(), E> {
        self.s.push_integer(n);
        Ok(())
    }

    fn visit_u64<E: de::Error>(self, n: u64) -> Result<(), E> {
        // Past the integers, the nearest float, as Lua reads such numerals.
        match i64::try_from(n) {
            Ok(n) => self.s.push_integer(n),
            Err(_) => self.s.push_number(n as f64),
        }
        Ok(())
    }

    fn visit_f64<E: de::Error>(self, f: f64) -> Result<(), E> {
        self.s.push_number(f);
        Ok(())
    }

    fn visit_str<E: de::Error>(self, text: &str) -> Result<(), E> {
        self.s.push_bytes(text.as_bytes());
        Ok(())
    }

    fn visit_seq<A: SeqAccess<'de>>(mut self, mut seq: A) -> Result<(), A::Error> {
        self.s.create_table(seq.size_hint().unwrap_or(0) as i32, 0);
        let (mut i, mut holes) = (0, false);
        while seq.next_element_seed(self.again())?.is_some() {
            i += 1;
            holes |= self.s.type_at(-1) == Type::Nil;
            let r = self.s.raw_set_index(-2, i);
            self.check(r)?;
        }
        if holes {
            // Keep the length, as json.array(t, n) does, so it encodes back
            // with its nulls.
            let r = mark_length(self.s, i);
            self.check(r)?;
        }
        Ok(())
    }

    fn visit_map<A: MapAccess<'de>>(mut self, mut map: A) -> Result<(), A::Error> {
        self.s.create_table(0, map.size_hint().unwrap_or(0) as i32);
        let mut empty = true;
        while map.next_key_seed(Key { s: &mut *self.s })?.is_some() {
            empty = false;
            map.next_value_seed(self.again())?;
            let r = self.s.raw_set(-3);
            self.check(r)?;
        }
        if empty {
            // Mark it, so it encodes as `{}` again.
            self.s.push_copy(OBJECT_META);
            let r = self.s.set_metatable(-2);
            self.check(r)?;
        }
        Ok(())
    }
}

/// Pushes an object key as a string.
struct Key<'a, 'b> {
    s: &'a mut Stack<'b>,
}

impl<'de> DeserializeSeed<'de> for Key<'_, '_> {
    type Value = ();

    fn deserialize<D: de::Deserializer<'de>>(self, d: D) -> Result<(), D::Error> {
        d.deserialize_str(self)
    }
}

impl<'de> Visitor<'de> for Key<'_, '_> {
    type Value = ();

    fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("a string key")
    }

    fn visit_str<E: de::Error>(self, text: &str) -> Result<(), E> {
        self.s.push_bytes(text.as_bytes());
        Ok(())
    }
}
