//! The parts of a C locale that Lua's behaviour depends on (ADR-13): the
//! decimal point numbers are written and read with, the character classes
//! and case mappings of the string library, and the order strings compare
//! in. Each isolate has its own [`Locale`], plain data that the runtime
//! reads; a [`LocaleProvider`] (in `orbit-locale`, which asks the C
//! library) makes a new one whenever `os.setlocale` changes it. Without a
//! provider only the C locale exists ([`CLocaleProvider`]).

use std::cmp::Ordering;
use std::fmt;
use std::rc::Rc;

use crate::object::Value;
use crate::object::number;

/// The class bits of [`CharTables::classes`], one per pattern class letter.
pub mod class {
    pub const ALPHA: u16 = 1 << 0;
    pub const CNTRL: u16 = 1 << 1;
    pub const DIGIT: u16 = 1 << 2;
    pub const GRAPH: u16 = 1 << 3;
    pub const LOWER: u16 = 1 << 4;
    pub const PUNCT: u16 = 1 << 5;
    pub const SPACE: u16 = 1 << 6;
    pub const UPPER: u16 = 1 << 7;
    pub const ALNUM: u16 = 1 << 8;
    pub const XDIGIT: u16 = 1 << 9;
    /// The zero byte (`%z`), the same in every locale.
    pub const ZERO: u16 = 1 << 10;

    /// The bit of a lower-case pattern class letter (`%a` and so on), or 0.
    pub const fn of_letter(lower: u8) -> u16 {
        match lower {
            b'a' => ALPHA,
            b'c' => CNTRL,
            b'd' => DIGIT,
            b'g' => GRAPH,
            b'l' => LOWER,
            b'p' => PUNCT,
            b's' => SPACE,
            b'u' => UPPER,
            b'w' => ALNUM,
            b'x' => XDIGIT,
            b'z' => ZERO,
            _ => 0,
        }
    }
}

/// For each byte: its classes (C's `isalpha` and friends) and its upper-
/// and lower-case forms (`toupper`, `tolower`).
#[derive(Clone, PartialEq, Eq)]
pub struct CharTables {
    pub classes: [u16; 256],
    pub upper: [u8; 256],
    pub lower: [u8; 256],
}

impl fmt::Debug for CharTables {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("CharTables { .. }")
    }
}

impl CharTables {
    /// The C locale's tables: ASCII only.
    pub fn c() -> CharTables {
        C_TABLES.clone()
    }

    /// Tables from `f`, which gives a byte's classes and its upper- and
    /// lower-case forms. [`class::ZERO`] is set for byte 0 whatever `f`
    /// says.
    pub fn from_fn(mut f: impl FnMut(u8) -> (u16, u8, u8)) -> CharTables {
        let mut t = CharTables {
            classes: [0; 256],
            upper: [0; 256],
            lower: [0; 256],
        };
        for b in 0..=255u8 {
            let (classes, upper, lower) = f(b);
            t.classes[b as usize] = (classes & !class::ZERO) | if b == 0 { class::ZERO } else { 0 };
            t.upper[b as usize] = upper;
            t.lower[b as usize] = lower;
        }
        t
    }

    /// Whether byte `c` is in the classes of `mask`.
    #[inline]
    pub fn is(&self, c: u8, mask: u16) -> bool {
        self.classes[c as usize] & mask != 0
    }
}

/// The C locale's tables, built at compile time.
static C_TABLES: CharTables = {
    let mut t = CharTables {
        classes: [0; 256],
        upper: [0; 256],
        lower: [0; 256],
    };
    let mut i = 0;
    while i < 256 {
        let c = i as u8;
        t.classes[i] = c_classes(c);
        t.upper[i] = c.to_ascii_uppercase();
        t.lower[i] = c.to_ascii_lowercase();
        i += 1;
    }
    t
};

/// One byte's classes in the C locale.
const fn c_classes(c: u8) -> u16 {
    const fn bit(on: bool, bit: u16) -> u16 {
        if on { bit } else { 0 }
    }
    bit(c.is_ascii_alphabetic(), class::ALPHA)
        | bit(c.is_ascii_control(), class::CNTRL)
        | bit(c.is_ascii_digit(), class::DIGIT)
        | bit(c.is_ascii_graphic(), class::GRAPH)
        | bit(c.is_ascii_lowercase(), class::LOWER)
        | bit(c.is_ascii_punctuation(), class::PUNCT)
        | bit(
            matches!(c, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r'),
            class::SPACE,
        )
        | bit(c.is_ascii_uppercase(), class::UPPER)
        | bit(c.is_ascii_alphanumeric(), class::ALNUM)
        | bit(c.is_ascii_hexdigit(), class::XDIGIT)
        | bit(c == 0, class::ZERO)
}

/// String order for a locale whose collation isn't byte order (C's
/// `strcoll`).
pub trait Collate: fmt::Debug {
    fn compare(&self, a: &[u8], b: &[u8]) -> Ordering;
}

/// An isolate's locale as the runtime reads it. Cheap to clone.
#[derive(Clone)]
pub struct Locale {
    /// What numbers use for a decimal point: `.` in C, `,` in many
    /// locales, sometimes more than one byte (U+066B in `ps_AF`).
    decimal_point: Rc<[u8]>,
    chars: Rc<CharTables>,
    /// Whether `chars` are the C locale's, so case mapping can stay ASCII.
    c_chars: bool,
    /// `None` for byte order.
    collate: Option<Rc<dyn Collate>>,
}

impl fmt::Debug for Locale {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Locale")
            .field("c_chars", &self.c_chars)
            .field("collated", &self.collate.is_some())
            .finish_non_exhaustive()
    }
}

impl Default for Locale {
    fn default() -> Locale {
        Locale::c()
    }
}

impl Locale {
    /// The C locale.
    pub fn c() -> Locale {
        thread_local! {
            static C: Locale = Locale {
                decimal_point: Rc::from(&b"."[..]),
                chars: Rc::new(CharTables::c()),
                c_chars: true,
                collate: None,
            };
        }
        C.with(Locale::clone)
    }

    /// A locale from its parts. An empty decimal point means `.`.
    pub fn new(
        decimal_point: &[u8],
        chars: Rc<CharTables>,
        collate: Option<Rc<dyn Collate>>,
    ) -> Locale {
        let decimal_point = if decimal_point.is_empty() {
            &b"."[..]
        } else {
            decimal_point
        };
        Locale {
            decimal_point: Rc::from(decimal_point),
            c_chars: *chars == C_TABLES,
            chars,
            collate,
        }
    }

    pub fn decimal_point(&self) -> &[u8] {
        &self.decimal_point
    }

    pub fn chars(&self) -> &CharTables {
        &self.chars
    }

    /// The character tables, shared.
    pub fn chars_rc(&self) -> Rc<CharTables> {
        Rc::clone(&self.chars)
    }

    /// Whether the character classes and case mappings are the C locale's
    /// (ASCII).
    #[inline]
    pub fn c_chars(&self) -> bool {
        self.c_chars
    }

    /// `string.upper` of `s`, appended to `out`.
    pub fn upper(&self, s: &[u8], out: &mut Vec<u8>) {
        if self.c_chars {
            out.extend(s.iter().map(u8::to_ascii_uppercase));
        } else {
            out.extend(s.iter().map(|&c| self.chars.upper[c as usize]));
        }
    }

    /// `string.lower` of `s`, appended to `out`.
    pub fn lower(&self, s: &[u8], out: &mut Vec<u8>) {
        if self.c_chars {
            out.extend(s.iter().map(u8::to_ascii_lowercase));
        } else {
            out.extend(s.iter().map(|&c| self.chars.lower[c as usize]));
        }
    }

    /// Whether `.` is the decimal point, so numbers read and print as in C.
    #[inline]
    pub fn c_numbers(&self) -> bool {
        *self.decimal_point == *b"."
    }

    /// Replace the first `.` written after `start` with the decimal point:
    /// for a float formatted the C way, as C's `printf` would format it in
    /// this locale. A field width counts the point as one character, as
    /// glibc's `printf` does, since the padding was done with `.`.
    #[inline]
    pub fn localize_float(&self, out: &mut Vec<u8>, start: usize) {
        if self.c_numbers() {
            return;
        }
        self.localize_slow(out, start);
    }

    #[cold]
    #[inline(never)]
    fn localize_slow(&self, out: &mut Vec<u8>, start: usize) {
        if let Some(at) = out[start..].iter().position(|&b| b == b'.') {
            let at = start + at;
            out.splice(at..at + 1, self.decimal_point.iter().copied());
        }
    }

    /// [`Locale::localize_float`] for a `String`. Returns it unchanged in C.
    pub fn localize_string(&self, s: String) -> Vec<u8> {
        let mut out = s.into_bytes();
        self.localize_float(&mut out, 0);
        out
    }

    /// `tostring` of a number in this locale.
    pub fn fmt_number(&self, out: &mut Vec<u8>, v: Value) -> Option<()> {
        match v {
            Value::Int(i) => number::push_int(out, i),
            Value::Float(f) => {
                let start = out.len();
                let mut s = String::new();
                number::fmt_float(&mut s, f);
                out.extend_from_slice(s.as_bytes());
                self.localize_float(out, start);
            }
            _ => return None,
        }
        Some(())
    }

    /// Read a number as `tonumber` does in this locale. The decimal point
    /// is the locale's; a `.` is still read when the point is one byte, as
    /// lua5.5 retries with `.` replaced by the point's first byte.
    #[inline]
    pub fn str_to_number(&self, s: &[u8]) -> Option<Value> {
        if self.c_numbers() {
            return number::str_to_number(s);
        }
        self.str_to_number_slow(s)
    }

    #[cold]
    #[inline(never)]
    fn str_to_number_slow(&self, s: &[u8]) -> Option<Value> {
        let dp = &self.decimal_point[..];
        let has_dot = s.contains(&b'.');
        if !has_dot {
            // The locale's point becomes `.`; with none, it reads as in C.
            return number::str_to_number(&replace(s, dp, b"."));
        }
        if dp.len() == 1 && !contains(s, dp) {
            // The retry: `.` stands for the point.
            return number::str_to_number(s);
        }
        None
    }

    /// Compare two strings in this locale's order.
    #[inline]
    pub fn compare(&self, a: &[u8], b: &[u8]) -> Ordering {
        match &self.collate {
            None => a.cmp(b),
            Some(c) => c.compare(a, b),
        }
    }

    /// Whether strings compare byte by byte.
    #[inline]
    pub fn byte_order(&self) -> bool {
        self.collate.is_none()
    }
}

fn contains(s: &[u8], pat: &[u8]) -> bool {
    s.windows(pat.len()).any(|w| w == pat)
}

/// `s` with every `from` replaced by `to`.
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

/// A locale category, as `os.setlocale` names them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Category {
    All,
    Collate,
    Ctype,
    Monetary,
    Numeric,
    Time,
}

impl Category {
    /// The category `os.setlocale` calls `name`.
    pub fn from_name(name: &[u8]) -> Option<Category> {
        Some(match name {
            b"all" => Category::All,
            b"collate" => Category::Collate,
            b"ctype" => Category::Ctype,
            b"monetary" => Category::Monetary,
            b"numeric" => Category::Numeric,
            b"time" => Category::Time,
            _ => return None,
        })
    }
}

/// Whose locale an isolate's scripts set and read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum LocaleMode {
    /// The isolate's own: `os.setlocale` changes this isolate only. The
    /// default for Rust hosts.
    #[default]
    Isolate,
    /// The process's: `os.setlocale` calls the C library's `setlocale`, as
    /// PUC Lua does, and the isolate follows changes the host makes. The
    /// default for states made through the Lua C API.
    Process,
}

/// A broken-down time for the locale-dependent `os.date` conversions:
/// C's `struct tm`, with a full year and 1-based month and year day.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TimeParts {
    pub year: i64,
    /// 1 to 12.
    pub month: i64,
    /// 1 to 31.
    pub day: i64,
    pub hour: i64,
    pub min: i64,
    pub sec: i64,
    /// 0 (Sunday) to 6.
    pub wday: i64,
    /// 1 to 366.
    pub yday: i64,
    pub isdst: bool,
}

/// Where an isolate's locales come from: `os.setlocale` and the
/// locale-dependent `os.date` conversions go through it.
pub trait LocaleProvider: fmt::Debug {
    /// `os.setlocale(name, category)`: with a name, switch `category` to
    /// it; without, ask. The locale's name, or `None` if it isn't
    /// available (and nothing changed).
    fn set(&mut self, category: Category, name: Option<&[u8]>) -> Option<Vec<u8>>;

    /// The current locale, for the runtime.
    fn locale(&self) -> Locale;

    fn mode(&self) -> LocaleMode;

    /// Switch modes, keeping the locale scripts see: a switch to
    /// [`LocaleMode::Process`] takes on the process's locale, one to
    /// [`LocaleMode::Isolate`] keeps it as the isolate's own.
    fn set_mode(&mut self, mode: LocaleMode);

    /// In [`LocaleMode::Process`], take on changes the host made to the
    /// process's locale since the last call; true if there were any.
    fn sync(&mut self) -> bool {
        false
    }

    /// The pattern a `%c`, `%x`, `%X` or `%r` conversion (`spec`, without
    /// its `%`, also `Ec`, `Ex` or `EX`) expands to in the time category's
    /// locale, or `None` to format it the C way (or with
    /// [`LocaleProvider::time_text`]).
    fn time_pattern(&self, _spec: &[u8]) -> Option<Vec<u8>> {
        None
    }

    /// The text of a locale-dependent conversion (`spec` is `a`, `A`, `b`,
    /// `B`, `h`, `p`, `c`, `x`, `X`, `r`, or one with an `E` or `O`
    /// modifier) in the time category's locale, or `None` to format it the
    /// C way.
    fn time_text(&self, _spec: &[u8], _tm: &TimeParts) -> Option<Vec<u8>> {
        None
    }

    /// Native code (a C module, `ffi`) is about to run on this thread:
    /// make the isolate's locale the thread's, if scripts changed it.
    /// Calls nest; each is matched by [`LocaleProvider::leave_native`].
    fn enter_native(&mut self) {}

    /// Native code returned: undo the matching
    /// [`LocaleProvider::enter_native`].
    fn leave_native(&mut self) {}
}

/// The provider when none is installed: only the C locale exists.
/// Asking for it by any of its names (`"C"`, `"POSIX"`, `""`) gives `"C"`;
/// anything else isn't available.
#[derive(Debug, Default)]
pub struct CLocaleProvider {
    mode: LocaleMode,
}

impl LocaleProvider for CLocaleProvider {
    fn set(&mut self, _category: Category, name: Option<&[u8]>) -> Option<Vec<u8>> {
        matches!(name, None | Some(b"" | b"C" | b"POSIX")).then(|| b"C".to_vec())
    }

    fn locale(&self) -> Locale {
        Locale::c()
    }

    fn mode(&self) -> LocaleMode {
        self.mode
    }

    fn set_mode(&mut self, mode: LocaleMode) {
        self.mode = mode;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn comma() -> Locale {
        Locale::new(b",", Rc::new(CharTables::c()), None)
    }

    fn show(v: Option<Value>) -> String {
        match v {
            Some(Value::Int(i)) => format!("{i}"),
            Some(Value::Float(f)) => format!("{f:?}"),
            _ => "nil".into(),
        }
    }

    #[test]
    fn reading_numbers() {
        let c = Locale::c();
        let pt = comma();
        let ps = Locale::new("٫".as_bytes(), Rc::new(CharTables::c()), None);
        // As lua5.5 reads them under C, pt_BR and ps_AF.
        let cases: [(&str, &str, &str, &str); 8] = [
            ("3.4", "3.4", "3.4", "nil"),
            ("3,4", "nil", "3.4", "nil"),
            ("3٫5", "nil", "nil", "3.5"),
            ("0x1,8", "nil", "1.5", "nil"),
            ("3,0", "nil", "3.0", "nil"),
            ("1,5.0", "nil", "nil", "nil"),
            ("10", "10", "10", "10"),
            (" 1e1 ", "10.0", "10.0", "10.0"),
        ];
        for (s, want_c, want_pt, want_ps) in cases {
            assert_eq!(show(c.str_to_number(s.as_bytes())), want_c, "C {s}");
            assert_eq!(show(pt.str_to_number(s.as_bytes())), want_pt, "pt {s}");
            assert_eq!(show(ps.str_to_number(s.as_bytes())), want_ps, "ps {s}");
        }
    }

    #[test]
    fn writing_numbers() {
        let mut out = Vec::new();
        comma().fmt_number(&mut out, Value::Float(1234567.0));
        assert_eq!(out, b"1234567,0");
        out.clear();
        comma().fmt_number(&mut out, Value::Int(-12));
        assert_eq!(out, b"-12");
        let ps = Locale::new("٫".as_bytes(), Rc::new(CharTables::c()), None);
        let mut out = b"[   3.5]".to_vec();
        ps.localize_float(&mut out, 1);
        assert_eq!(out, "[   3٫5]".as_bytes());
    }

    #[test]
    fn c_provider() {
        let mut p = CLocaleProvider::default();
        assert_eq!(p.set(Category::All, Some(b"POSIX")), Some(b"C".to_vec()));
        assert_eq!(p.set(Category::Numeric, Some(b"pt_BR")), None);
        assert_eq!(p.set(Category::Time, None), Some(b"C".to_vec()));
    }

    #[test]
    fn c_tables() {
        let t = CharTables::c();
        assert!(t.is(b'a', class::ALPHA | class::LOWER));
        assert!(!t.is(0xe1, class::ALPHA));
        assert!(t.is(0, class::ZERO) && !t.is(b'0', class::ZERO));
        assert_eq!(
            (t.upper[b'q' as usize], t.lower[b'Q' as usize]),
            (b'Q', b'q')
        );
    }
}
