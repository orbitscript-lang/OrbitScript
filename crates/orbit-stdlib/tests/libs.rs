//! The libraries as Lua sees them, run on the VM. Expected values come from the
//! reference lua5.5; the official suite files (`cargo testkit conformance`) cover
//! far more.

use orbit_core::Value;
use orbit_vm::Vm;
use orbit_vm::baselib::{open_base, open_coroutine};

fn vm() -> Vm {
    let mut vm = Vm::new();
    open_base(&mut vm);
    open_coroutine(&mut vm);
    orbit_stdlib::open_libs(&mut vm);
    vm
}

/// Runs `src` and returns its results as display strings.
fn run(src: &str) -> Result<Vec<String>, String> {
    let mut vm = vm();
    let f = vm
        .load(src.as_bytes(), b"=test")
        .map_err(|e| show(&mut vm, e.value))?;
    vm.push(f);
    match vm.pcall(0, None, 0) {
        Ok(()) => {
            let n = vm.top();
            let mut out = Vec::new();
            for i in 1..=n {
                let v = vm.get(i);
                out.push(show(&mut vm, v));
            }
            Ok(out)
        }
        Err(e) => Err(show(&mut vm, e.value)),
    }
}

fn show(vm: &mut Vm, v: Value) -> String {
    let s = vm.to_display(v).expect("tostring");
    String::from_utf8_lossy(vm.heap.string(s).as_bytes()).into_owned()
}

fn ok(src: &str) -> Vec<String> {
    run(src).unwrap_or_else(|e| panic!("{src}: {e}"))
}

#[test]
fn string_library() {
    assert_eq!(
        ok(r##"return ("hello world"):gsub("o", "0")"##),
        ["hell0 w0rld", "2"]
    );
    assert_eq!(
        ok(r##"return string.gsub("abc", "%w", {a = 1, b = false})"##),
        ["1bc", "3"]
    );
    assert_eq!(
        ok(r##"return string.gsub("abc", "(%w)", function (c) return c:upper() .. "." end)"##),
        ["A.B.C.", "3"]
    );
    assert_eq!(
        ok(
            r##"local t = {} for k, v in ("a=1, b=2"):gmatch("(%w+)=(%w+)") do t[#t+1] = k .. v end
              return table.concat(t, ";")"##
        ),
        ["a1;b2"]
    );
    assert_eq!(
        ok(r##"return string.format("%5.2f|%q|%s", 3.14159, "a\0", {} ~= nil)"##),
        [" 3.14|\"a\\0\"|true"]
    );
    assert_eq!(
        ok(r##"return "10" + 5, "3" * "4", 10 .. """##),
        ["15", "12", "10"]
    );
    assert_eq!(
        run(r##"return "a" + 1"##).unwrap_err(),
        "test:1: attempt to add a 'string' with a 'number'"
    );
    assert_eq!(
        ok(r##"return string.unpack("<i4", string.pack("<i4", -7))"##),
        ["-7", "5"]
    );
    assert_eq!(
        run(r##"return string.rep("x", -1), ("x"):byte(10), string.char(72, 105)"##).unwrap(),
        ["", "nil", "Hi"]
    );
}

#[test]
fn table_library() {
    assert_eq!(
        ok(
            r##"local t = {5, 2, 8, 1} table.sort(t, function (a, b) return a > b end)
              return table.concat(t, ",")"##
        ),
        ["8,5,2,1"]
    );
    assert_eq!(
        ok(
            r##"local t = {1, 2, 3} table.insert(t, 1, 0) table.remove(t)
              return table.concat(t, ","), select("#", table.unpack({1, nil, 3}, 1, 3))"##
        ),
        ["0,1,2", "3"]
    );
    assert_eq!(
        run(r##"table.sort({1, 2, 3, 4, 5, 6, 7, 8, 9, 10}, function () return true end)"##)
            .unwrap_err(),
        "test:1: invalid order function for sorting"
    );
    // __index and __len are respected.
    assert_eq!(
        ok(
            r##"local p = setmetatable({}, {__index = function (_, i) return i * 10 end,
                                         __len = function () return 3 end})
              return table.concat(p, " ")"##
        ),
        ["10 20 30"]
    );
}

#[test]
fn math_library() {
    assert_eq!(
        ok("return math.floor(3.7), math.max(1, 5, 3), math.type(1), math.type(1.0), math.pi"),
        ["3", "5", "integer", "float", "3.1415926535897931"]
    );
    assert_eq!(
        ok(
            r##"return math.tointeger("8"), math.tointeger(3.5), math.ult(1, -1), math.fmod(-7, 3)"##
        ),
        ["8", "nil", "true", "-1"]
    );
    // Seeded sequences are the reference's.
    assert_eq!(
        ok("math.randomseed(42) return math.random(1, 100), math.random(1, 100), math.random(0)"),
        ["50", "76", "-3807604385970496171"]
    );
}

#[test]
fn utf8_library() {
    assert_eq!(
        ok(
            r##"return utf8.char(72, 8364), utf8.len("h€"), utf8.codepoint("€"), utf8.offset("a€b", 3)"##
        ),
        ["H€", "2", "8364", "5", "5"]
    );
    assert_eq!(
        ok(
            r##"local t = {} for p, c in utf8.codes("a€") do t[#t+1] = p .. ":" .. c end
              return table.concat(t, " "), utf8.len("\xff")"##
        ),
        ["1:97 2:8364", "nil", "1"]
    );
}

#[test]
fn os_library() {
    assert_eq!(
        ok(r##"return os.date("!%Y-%m-%d %H:%M:%S %j %a %b", 0),
                     os.time{year = 2020, month = 1, day = 1, hour = 0},
                     os.date("!*t", 86400).wday"##),
        ["1970-01-01 00:00:00 001 Thu Jan", "1577836800", "6"]
    );
    assert_eq!(
        ok(
            r##"local t = {year = 2021, month = 14, day = 0} os.time(t) return t.year, t.month, t.day"##
        ),
        ["2022", "1", "31"]
    );
    assert_eq!(
        run(r##"os.date("%Ez")"##).unwrap_err(),
        "test:1: bad argument #1 to 'date' (invalid conversion specifier '%Ez')"
    );
}

#[test]
fn io_library() {
    assert_eq!(
        ok(r##"local f = io.tmpfile()
              f:write("12 0x10 abc\n", 3.5, "\nlast")
              f:seek("set")
              local a, b = f:read("n", "n")
              local rest = f:read("l")
              local lines = {}
              for l in f:lines("L") do lines[#lines+1] = l end
              f:close()
              return a, b, rest, table.concat(lines, "|"), io.type(f)"##),
        ["12", "16", " abc", "3.5\n|last", "closed file"]
    );
    assert_eq!(
        ok(r##"local name = os.tmpname()
              local f = assert(io.open(name, "w"))
              f:write("x")
              assert(f:close())
              local r = io.open(name):read("a")
              assert(os.remove(name))
              return r, io.open(name) == nil"##),
        ["x", "true"]
    );
    // A write to a read-only file fails at once, not when the buffer is flushed.
    assert_eq!(
        ok(r##"local name = os.tmpname()
              local f = io.open(name, "r")
              local r = {f:write("abc", 1)}
              f:close()
              os.remove(name)
              return table.unpack(r)"##),
        ["nil", "Bad file descriptor", "9", "0"]
    );
    assert_eq!(
        ok(r##"local f, msg, code = io.open("/nonexistent/file") return f, msg, code"##),
        ["nil", "/nonexistent/file: No such file or directory", "2"]
    );
}

#[test]
fn package_library() {
    assert_eq!(
        ok(r##"package.preload.m = function (...) return {...} end
              local m, extra = require "m"
              return m[1], m[2], extra, require "m" == m, package.loaded.string == string"##),
        ["m", ":preload:", ":preload:", "true", "true"]
    );
    let e = run(r##"package.path = "" package.cpath = "" require "nope""##).unwrap_err();
    assert_eq!(
        e,
        "test:1: module 'nope' not found:\n\tno field package.preload['nope']\n\tno file ''\n\tno file ''"
    );
    assert_eq!(
        ok(r##"return package.loadlib("x", "y")"##),
        [
            "nil",
            "dynamic libraries not enabled; check your Lua installation",
            "absent"
        ]
    );
}
