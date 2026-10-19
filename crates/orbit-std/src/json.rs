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
//! `__jsontype` field (`"object"`, `"array"` or `"null"`) overrides the
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
    let ups = s.top() - 2;
    s.create_table(0, 5);
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
    // Leave only the module.
    s.rotate(ups, 1)?;
    s.set_top(ups)
}

/// Push `N` as a closure over the three upvalues starting at `ups`.
fn push_closure<N: Native>(s: &mut Stack<'_>, ups: i32) -> StackResult<()> {
    for i in 0..3 {
        s.push_copy(ups + i);
    }
    s.push_native_closure::<N>(3, 0)
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

fn mark_array(s: &mut Stack<'_>) -> StackResult<Return> {
    mark(s, ARRAY_META)
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
    Array,
    Null,
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
            Type::Table => self.table(s, idx)?,
            t => return Err(bad(format!("cannot encode a {}", t.name()))),
        }
        Ok(())
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
        if shape == Shape::Null {
            self.out.extend_from_slice(b"null");
            return Ok(());
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
        // Count the keys and see whether they are exactly 1..n.
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
        let sequence = sequence && max == count;
        let array = match shape {
            Shape::Array if !sequence => {
                return Err(bad(
                    "cannot encode a table marked as an array whose keys aren't 1..n",
                ));
            }
            Shape::Array => true,
            Shape::Object => false,
            _ => sequence,
        };
        let depth = self.open.len();
        if array {
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

/// The shape the metatable of the table at `t` asks for.
fn shape(s: &mut Stack<'_>, t: i32) -> StackResult<Shape> {
    if !s.get_metatable(t) {
        return Ok(Shape::Any);
    }
    s.raw_get_field(-1, b"__jsontype")?;
    let shape = match s.string_at(-1) {
        Some(b"object") => Shape::Object,
        Some(b"array") => Shape::Array,
        Some(b"null") => Shape::Null,
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
    s.set_top(1)?;
    let mut raised = None;
    let mut de = serde_json::Deserializer::from_slice(&text);
    let r = Build {
        s: &mut *s,
        raised: &mut raised,
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
}

impl<'b> Build<'_, 'b> {
    fn again(&mut self) -> Build<'_, 'b> {
        Build {
            s: &mut *self.s,
            raised: &mut *self.raised,
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
        self.s.push_copy(NULL);
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
        let mut i = 0;
        while seq.next_element_seed(self.again())?.is_some() {
            i += 1;
            let r = self.s.raw_set_index(-2, i);
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
