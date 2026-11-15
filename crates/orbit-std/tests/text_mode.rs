//! Text streams on Windows: `io.open` without `b`, `io.lines` and pipes
//! translate line endings and end input at a Ctrl-Z, as the C library's
//! text streams do; `b` streams and `LineEndings::Binary` isolates don't.
//! The expected bytes are the reference interpreter's on Windows.

#![cfg(all(windows, feature = "io", feature = "os"))]

use orbit::{Chunk, Config, Error, Isolate, LineEndings, Runtime};

fn isolate(line_endings: LineEndings) -> Isolate {
    let config = Config::default().line_endings(line_endings);
    let mut iso = Runtime::new().new_isolate(config);
    orbit_std::install(&mut iso, &orbit_std::Options::default()).unwrap();
    iso
}

/// Run `src` with `path` (a fresh temporary file name) as `...`, and give
/// its results as bytes.
fn run(line_endings: LineEndings, src: &str) -> Vec<Vec<u8>> {
    let path = std::env::temp_dir().join(format!(
        "orbit-text-mode-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let mut iso = isolate(line_endings);
    let out = iso.scope(|s| {
        let f = s.load(Chunk::new(src).name("=t"))?;
        let out = f.call((path.to_str().unwrap(),))?;
        out.iter()
            .map(|v| Ok(v.to_display()?.to_vec()))
            .collect::<Result<Vec<_>, Error>>()
    });
    let _ = std::fs::remove_file(&path);
    match out {
        Ok(v) => v,
        Err(e) => panic!("script failed: {e:?}"),
    }
}

const HELPERS: &str = r#"
    local path = ...
    local function put(bytes) local f = assert(io.open(path, "wb")) f:write(bytes) f:close() end
    local function get() local f = assert(io.open(path, "rb")) local s = f:read("a") f:close() return s end
"#;

fn script(body: &str) -> String {
    format!("{HELPERS}\n{body}")
}

#[test]
fn writes() {
    let r = run(
        LineEndings::Native,
        &script(
            r#"
            local f = assert(io.open(path, "w")) f:write("a\nb\r\nc\rd\26e\n")
            local pos = f:seek("cur") f:close()
            local text = get()
            f = assert(io.open(path, "wb")) f:write("a\n") f:close()
            local binary = get()
            put("x\26") f = assert(io.open(path, "a+")) f:write("y\n") f:close()
            local appended = get()
            put("x\26") f = assert(io.open(path, "a")) f:write("y\n") f:close()
            return text, pos, binary, appended, get()
            "#,
        ),
    );
    let want: [&[u8]; 5] = [
        b"a\r\nb\r\r\nc\rd\x1ae\r\n",
        b"14",
        b"a\n",
        b"xy\r\n",
        b"x\x1ay\r\n",
    ];
    assert_eq!(r, want);
}

#[test]
fn reads() {
    let r = run(
        LineEndings::Native,
        &script(
            r#"
            put("l1\r\nl2\n\r\nl3\rX\26after\r\nend")
            local f = assert(io.open(path))
            local a, again, size = f:read("a"), f:read("a"), f:seek("end")
            f:close()
            local lines = {}
            for l in io.lines(path) do lines[#lines + 1] = l end
            f = assert(io.open(path, "rb"))
            local raw = f:read("a") f:close()
            return a, again, size, table.concat(lines, ","), raw
            "#,
        ),
    );
    let want: [&[u8]; 5] = [
        b"l1\nl2\n\nl3\rX",
        b"",
        b"24",
        b"l1,l2,,l3\rX",
        b"l1\r\nl2\n\r\nl3\rX\x1aafter\r\nend",
    ];
    assert_eq!(r, want);
}

#[test]
fn pipes() {
    let r = run(
        LineEndings::Native,
        r#"
        local p = assert(io.popen("echo hi")) local s = p:read("a") p:close()
        return s
        "#,
    );
    assert_eq!(r, [b"hi\n"]);
}

#[test]
fn binary_isolates_keep_bytes() {
    let r = run(
        LineEndings::Binary,
        &script(
            r#"
            local f = assert(io.open(path, "w")) f:write("a\n") f:close()
            local written = get()
            put("b\r\n\26c")
            f = assert(io.open(path)) local read = f:read("a") f:close()
            local p = assert(io.popen("echo hi")) local piped = p:read("a") p:close()
            return written, read, piped
            "#,
        ),
    );
    let want: [&[u8]; 3] = [b"a\n", b"b\r\n\x1ac", b"hi\r\n"];
    assert_eq!(r, want);
}

#[test]
fn scripts_are_read_as_text() {
    let r = run(
        LineEndings::Native,
        &script(
            r#"
            put("return 1\26 this is not Lua")
            local ended = loadfile(path)()
            put("return 'a\26b'")
            local _, unfinished = loadfile(path)
            put("local x = 2\r\nreturn [[a\r\r\nb]], x")
            local long, x = dofile(path)
            put(string.dump(function() return "q\r\n\26" end))
            local binary = loadfile(path)()
            put('#!shebang\r\n' .. string.dump(function() return 7 end))
            local shebang = loadfile(path)()
            return ended, unfinished:match("unfinished string") ~= nil, long, x, binary, shebang
            "#,
        ),
    );
    let want: [&[u8]; 6] = [b"1", b"true", b"a\nb", b"2", b"q\r\n\x1a", b"7"];
    assert_eq!(r, want);
}
