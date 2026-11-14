//! `io.popen` and `os.execute` hand the command to the shell unchanged:
//! `/bin/sh -c` elsewhere, and on Windows `cmd /C` with the command line
//! verbatim, quotes included, as the C library's `system` and `_popen` do.

#![cfg(all(feature = "io", feature = "os"))]

use orbit::{Chunk, Config, Error, Isolate, Runtime};

fn isolate() -> Isolate {
    let mut iso = Runtime::new().new_isolate(Config::default());
    orbit_std::install(&mut iso, &orbit_std::Options::default()).unwrap();
    iso
}

fn run(src: &str) -> Vec<String> {
    let mut iso = isolate();
    let out = iso.scope(|s| {
        let out = s.load(Chunk::new(src).name("=t"))?.call(())?;
        out.iter()
            .map(|v| Ok(String::from_utf8_lossy(&v.to_display()?).into_owned()))
            .collect::<Result<Vec<_>, Error>>()
    });
    match out {
        Ok(v) => v,
        Err(e) => panic!("script failed: {e:?}"),
    }
}

#[test]
fn popen_passes_quotes_through() {
    let out = run(r#"
        local p = assert(io.popen('echo "a  b"'))
        local s = p:read("a")
        p:close()
        return (s:gsub("%s+$", ""))
    "#);
    // `cmd`'s `echo` prints its argument as written, quotes and all; the
    // shell elsewhere removes them.
    let want = if cfg!(windows) { "\"a  b\"" } else { "a  b" };
    assert_eq!(out, [want]);
}

#[test]
fn execute_passes_quotes_through() {
    let cmd = if cfg!(windows) {
        r#"if "x"=="x" (exit 3) else (exit 4)"#
    } else {
        r#"if [ "x" = "x" ]; then exit 3; else exit 4; fi"#
    };
    let out = run(&format!("return os.execute([==[{cmd}]==])"));
    assert_eq!(out, ["nil", "exit", "3"]);
}
