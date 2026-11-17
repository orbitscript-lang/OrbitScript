//! The command line against the reference `lua`'s observable behaviour
//! (`main.lua` checks the rest once the libraries it uses land).

mod common;

use std::io::Write;
use std::process::{Command, Output, Stdio};

/// Run `orbit` with `args`, `stdin` as its input and a clean environment
/// apart from `env`.
fn orbit(args: &[&str], stdin: &str, env: &[(&str, &str)]) -> Output {
    let mut c = Command::new(common::bin("orbit", env!("CARGO_BIN_EXE_orbit")));
    c.args(args)
        .env_clear()
        // A coverage run's profiles go where the run says (tests/coverage).
        .envs(std::env::var_os("LLVM_PROFILE_FILE").map(|p| ("LLVM_PROFILE_FILE", p)))
        .envs(env.iter().copied())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = c.spawn().unwrap();
    // Options such as `-v` and `-e` end without reading standard input, and
    // the program may already have exited: a closed pipe is no failure.
    match child.stdin.take().unwrap().write_all(stdin.as_bytes()) {
        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => {}
        r => r.unwrap(),
    }
    child.wait_with_output().unwrap()
}

fn out(o: &Output) -> String {
    common::text(&o.stdout)
}

fn err(o: &Output) -> String {
    common::text(&o.stderr)
}

/// Standard input, output and error are text streams on Windows, as the
/// reference interpreter's are: `\n` goes out as `\r\n`, and input reads
/// `\r\n` as `\n` and ends at a Ctrl-Z.
#[test]
#[cfg(windows)]
fn standard_streams_are_text_streams() {
    let o = orbit(
        &[
            "-e",
            "io.write('a\\n') print('b') io.stderr:write('c\\n') io.write(io.read('a'))",
        ],
        "x\r\ny\x1az\r\n",
        &[],
    );
    assert_eq!(o.stdout, b"a\r\nb\r\nx\r\ny");
    assert_eq!(o.stderr, b"c\r\n");
}

/// Scripts are read as text on Windows, ending at a Ctrl-Z, and the
/// program's own messages end lines with `\r\n`, as the reference
/// interpreter's do.
#[test]
#[cfg(windows)]
fn scripts_and_messages_are_text() {
    let o = orbit(&["-"], "print('a')\r\n\x1athis is not Lua\r\n", &[]);
    assert_eq!(o.stdout, b"a\r\n");
    assert!(o.status.success(), "{}", err(&o));
    let o = orbit(&["-e", "error('boom')"], "", &[]);
    assert!(o.stderr.ends_with(b"[C]: in ?\r\n"), "{:?}", o.stderr);
    assert!(!o.stderr.windows(2).any(|w| w[1] == b'\n' && w[0] != b'\r'));
}

#[test]
fn bad_options_print_usage() {
    for (args, msg) in [
        (&["-h"][..], "unrecognized option '-h'"),
        (&["---"], "unrecognized option '---'"),
        (&["-Ex", "--"], "unrecognized option '-Ex'"),
        (&["-iv"], "unrecognized option '-iv'"),
        (&["-e"], "'-e' needs argument"),
        (&["-e", "-l"], "'-e' needs argument"),
        (&["-l"], "'-l' needs argument"),
    ] {
        let o = orbit(args, "", &[]);
        assert_eq!(o.status.code(), Some(1), "{args:?}");
        assert!(err(&o).contains(msg), "{args:?}: {}", err(&o));
        assert!(err(&o).contains("usage: "), "{args:?}");
    }
    let o = orbit(&["--", "-i"], "", &[]);
    assert!(err(&o).contains("cannot open -i"), "{}", err(&o));
}

#[test]
fn options_run_in_order_and_fill_arg() {
    let o = orbit(&["-eprint(1)", "-ea=3", "-e", "print(a)"], "", &[]);
    assert_eq!(out(&o), "1\n3\n");
    let o = orbit(
        &[
            "-l",
            "t=table",
            "-lm=math",
            "-e",
            "print(type(t.insert), m.max(1, 2))",
        ],
        "",
        &[],
    );
    assert_eq!(out(&o), "function\t2\n");
    let o = orbit(
        &[
            "-e",
            "print(#arg, arg[0], arg[1], arg[-1], arg[-2])",
            "-",
            "x",
        ],
        "print(...)",
        &[],
    );
    assert_eq!(
        out(&o),
        "1\t-\tx\tprint(#arg, arg[0], arg[1], arg[-1], arg[-2])\t-e\nx\n"
    );
    // The script's arguments are read from `arg` when it runs.
    let o = orbit(&["-e", "arg[1] = 100", "-"], "print(...)", &[]);
    assert_eq!(out(&o), "100\n");
    let o = orbit(&["-e", "arg = 1", "-"], "", &[]);
    assert!(err(&o).ends_with("'arg' is not a table\n"), "{}", err(&o));
}

#[test]
fn standard_input_runs_without_a_script() {
    assert_eq!(out(&orbit(&[], "print(10)", &[])), "10\n");
    assert_eq!(out(&orbit(&["-W"], "print(2)", &[])), "2\n");
    assert_eq!(out(&orbit(&["-e", "x=1"], "print(3)", &[])), "");
    let o = orbit(&["-v"], "print(4)", &[]);
    let release = format!("Lua {}", orbit::LUA.release());
    assert!(out(&o).contains(&release) && out(&o).ends_with("PUC-Rio\n"));
}

#[test]
fn environment() {
    let o = orbit(&["-e", "print(X)"], "", &[("LUA_INIT", "X=3")]);
    assert_eq!(out(&o), "3\n");
    let versioned = orbit_std::versioned_env("LUA_INIT");
    let init = [(versioned.as_str(), "X=10"), ("LUA_INIT", "X=3")];
    assert_eq!(out(&orbit(&["-e", "print(X)"], "", &init)), "10\n");
    let o = orbit(&["-e", ""], "", &[("LUA_INIT", "error('msg')")]);
    assert!(err(&o).contains("LUA_INIT:1: msg"), "{}", err(&o));
    let path = [("LUA_PATH", "xxx"), ("LUA_INIT", "error(10)")];
    let o = orbit(&["-E", "-e", "print(package.path)"], "", &path);
    assert!(
        !out(&o).contains("xxx") && out(&o).contains("lua"),
        "{}",
        out(&o)
    );
    let o = orbit(&["-e", "print(package.path)"], "", &[("LUA_PATH", "a;;b")]);
    assert!(out(&o).starts_with("a;") && out(&o).ends_with(";b\n"));
}

/// On Windows the default paths start in the program's directory, which
/// a path from the environment can name with `!` too, as in the reference
/// build.
#[cfg(windows)]
#[test]
fn windows_paths_are_relative_to_the_program() {
    let exe = common::bin("orbit", env!("CARGO_BIN_EXE_orbit"));
    let lua = orbit::LUA.dotted();
    let o = orbit(&["-e", "print(package.path) print(package.cpath)"], "", &[]);
    // The directory as the system names the program, which may be spelled
    // differently from the path the test started it by.
    let dir = out(&o).split("\\lua\\?.lua;").next().unwrap().to_string();
    let canonical = |p: &std::path::Path| std::fs::canonicalize(p).unwrap();
    assert_eq!(
        canonical(std::path::Path::new(&dir)),
        canonical(exe.parent().unwrap()),
        "{}",
        out(&o)
    );
    assert_eq!(
        out(&o),
        format!(
            "{dir}\\lua\\?.lua;{dir}\\lua\\?\\init.lua;{dir}\\?.lua;{dir}\\?\\init.lua;\
             {dir}\\..\\share\\lua\\{lua}\\?.lua;{dir}\\..\\share\\lua\\{lua}\\?\\init.lua;\
             .\\?.lua;.\\?\\init.lua;{dir}\\..\\share\\orbit\\?.orb;\
             {dir}\\..\\share\\orbit\\?\\init.orb;.\\?.orb;.\\?\\init.orb\n\
             {dir}\\?.dll;{dir}\\..\\lib\\lua\\{lua}\\?.dll;{dir}\\loadall.dll;.\\?.dll\n"
        )
    );
    let o = orbit(
        &["-e", "print(package.cpath)"],
        "",
        &[("LUA_CPATH", "!\\x\\?.dll")],
    );
    assert_eq!(out(&o), format!("{dir}\\x\\?.dll\n"));
}

#[test]
fn error_objects() {
    let o = orbit(&["-e", "error{}"], "", &[]);
    assert!(err(&o).contains(": (error object is a table value)\nstack traceback:"));
    let o = orbit(
        &[
            "-e",
            "error(setmetatable({}, {__tostring = function() return 'TS' end}))",
        ],
        "",
        &[],
    );
    assert!(err(&o).ends_with(": TS\n"), "{}", err(&o));
    let o = orbit(&["-e", "a"], "", &[]);
    assert!(err(&o).contains("syntax error"), "{}", err(&o));
}

#[test]
#[cfg(feature = "repl")]
fn interactive() {
    let prompts = "_PROMPT='' _PROMPT2=''";
    let o = orbit(&["-e", prompts, "-i"], "(6*2-6)\na =\n10\nprint(a)\na", &[]);
    assert!(out(&o).ends_with("6\n10\n10\n\n"), "{}", out(&o));
    let o = orbit(&["-i"], "a.\n", &[]);
    assert!(err(&o).contains("near <eof>"), "{}", err(&o));
    assert!(out(&o).ends_with("> >> > \n"), "{}", out(&o));
    let o = orbit(&["-i"], "  \t local x\nlocal1 = 10\nlocal1 + 3\n", &[]);
    // Lua 5.4's interpreter has no such warning.
    let warnings = if orbit::LUA.is_5_4() { 0 } else { 1 };
    assert_eq!(
        err(&o).matches("warning: locals do not survive").count(),
        warnings
    );
    assert!(out(&o).contains("13\n"));
    if orbit::LUA.is_5_4() {
        // Lua 5.4's interpreter reads `=expr` as `return expr`.
        let o = orbit(&["-e", prompts, "-i"], "=1+1\n= [[a\nb]]\n", &[]);
        assert!(out(&o).ends_with("2\na\nb\n\n"), "{}", out(&o));
    }
    let o = orbit(&["-e", "print=nil", "-i"], "10\n", &[]);
    assert!(err(&o).contains("error calling 'print'"), "{}", err(&o));
    let o = orbit(&["-i"], "error('boom')\nprint(1)\n", &[]);
    assert!(
        err(&o).starts_with("stdin:1: boom\nstack traceback:"),
        "{}",
        err(&o)
    );
    assert!(out(&o).contains("1\n"));
}

#[test]
fn files_are_flushed_at_exit() {
    let dir = std::env::temp_dir().join(format!("orbit-cli-test-{}", std::process::id()));
    let code = format!("io.output({:?}); io.write('alo')", dir.to_string_lossy());
    let o = orbit(&["-e", &code], "", &[]);
    assert!(o.status.success(), "{}", err(&o));
    assert_eq!(std::fs::read_to_string(&dir).unwrap(), "alo");
    let _ = std::fs::remove_file(&dir);
}

#[test]
fn print_output_reaches_a_pipe_at_once() {
    use std::io::{BufRead, BufReader};
    // The script keeps running after printing, so the line can only arrive
    // in time if `print` flushed it.
    let mut child = Command::new(common::bin("orbit", env!("CARGO_BIN_EXE_orbit")))
        .args([
            "-e",
            "print('ready') local t = os.time() while os.time() - t < 20 do end",
        ])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut line = String::new();
        let _ = BufReader::new(stdout).read_line(&mut line);
        let _ = tx.send(line);
    });
    let line = rx.recv_timeout(std::time::Duration::from_secs(10));
    let _ = child.kill();
    let _ = child.wait();
    let line = line.map(|l| common::text(l.as_bytes()));
    assert_eq!(line.as_deref(), Ok("ready\n"));
}

/// The first `SIGSEGV` or `SIGBUS` ends `orbit`, as it ends `lua`, even
/// one sent with `kill()`. Rust's runtime catches both to report stack
/// overflows, and without `orbit_std::default_fault_signals` a signal that
/// isn't a fault only reset its handler, so `orbit` carried on (and so did
/// the processes a script forked).
#[test]
#[cfg(unix)]
fn first_fault_signal_ends_orbit() {
    use std::os::unix::process::ExitStatusExt;
    use std::time::{Duration, Instant};
    for (name, number) in [
        ("SEGV", 11),
        ("BUS", if cfg!(target_os = "linux") { 7 } else { 10 }),
    ] {
        let mut child = Command::new(common::bin("orbit", env!("CARGO_BIN_EXE_orbit")))
            .args(["-e", "io.write('ready\\n') io.flush() while true do end"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut ready = [0u8; 6];
        std::io::Read::read_exact(child.stdout.as_mut().unwrap(), &mut ready).unwrap();
        let pid = child.id().to_string();
        let sent = Command::new("kill")
            .args([&format!("-{name}"), &pid])
            .status();
        assert!(sent.is_ok_and(|s| s.success()), "kill -{name} {pid} failed");
        let deadline = Instant::now() + Duration::from_secs(10);
        let status = loop {
            if let Some(s) = child.try_wait().unwrap() {
                break Some(s);
            }
            if Instant::now() > deadline {
                break None;
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        if status.is_none() {
            let _ = child.kill();
            let _ = child.wait();
        }
        let status = status.unwrap_or_else(|| panic!("orbit survived SIG{name}"));
        assert_eq!(status.signal(), Some(number), "SIG{name}: {status:?}");
    }
}

/// A script writing to a pipe whose reader has gone ends with SIGPIPE, as
/// `lua` does (`orbit script | head -1`). Rust's runtime ignores SIGPIPE,
/// and `print` ignores write errors, so before `signals::default_pipe_signal` such
/// a script ran forever.
#[test]
#[cfg(unix)]
fn writing_to_a_closed_pipe_ends_orbit() {
    use std::io::{BufRead, BufReader};
    use std::os::unix::process::ExitStatusExt;
    use std::time::{Duration, Instant};
    let mut child = Command::new(common::bin("orbit", env!("CARGO_BIN_EXE_orbit")))
        .args(["-e", "while true do print(1) end"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut line = String::new();
    BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    assert_eq!(line, "1\n");
    // The reader is dropped: the pipe has no reader left.
    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(s) = child.try_wait().unwrap() {
            break Some(s);
        }
        if Instant::now() > deadline {
            break None;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    if status.is_none() {
        let _ = child.kill();
        let _ = child.wait();
    }
    let status = status.expect("orbit kept running with no reader");
    assert_eq!(status.signal(), Some(13), "{status:?}");
}

/// One Ctrl-C can't stop a long library call, which never reaches an
/// interrupt check; a second ends the process, as it ends `lua`. This is
/// what `main.lua`'s "long unbreakable search" checks, but its `.*b` search
/// ends at once in our matcher, so the manifest disables those lines.
#[test]
#[cfg(unix)]
fn second_ctrl_c_ends_a_long_library_call() {
    use std::io::{BufRead, BufReader};
    use std::time::{Duration, Instant};
    // Quadratic in our matcher: every 'a' is a start that scans to the end.
    // Seconds per 100,000 bytes in a release build, so this runs for minutes.
    let mut child = Command::new(common::bin("orbit", env!("CARGO_BIN_EXE_orbit")))
        .args([
            "-e",
            "print(15) string.find(string.rep('a', 1000000), 'a.*b') print('finished')",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let pid = child.id().to_string();
    let int = || {
        let ok = Command::new("kill").args(["-INT", &pid]).status();
        assert!(ok.is_ok_and(|s| s.success()), "kill -INT {pid} failed");
    };
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let mut line = String::new();
    stdout.read_line(&mut line).unwrap();
    assert_eq!(line, "15\n");

    std::thread::sleep(Duration::from_millis(300));
    int();
    std::thread::sleep(Duration::from_millis(500));
    let still_running = child.try_wait().unwrap().is_none();
    if still_running {
        int();
    }
    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(s) = child.try_wait().unwrap() {
            break Some(s);
        }
        if Instant::now() > deadline {
            break None;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    if status.is_none() {
        let _ = child.kill();
        let _ = child.wait();
    }
    // Exit status 130 here means the first Ctrl-C ended orbit; a success,
    // that the search finished and a slower call is needed.
    assert!(still_running, "orbit exited after one Ctrl-C: {status:?}");
    let status = status.expect("a second Ctrl-C didn't end orbit");
    assert_eq!(status.code(), Some(130));
}

#[test]
fn memory_limit_from_the_environment() {
    let script = "local t = {} \
                  print(pcall(function () for i = 1, math.huge do t[i] = {} end end)) \
                  print(collectgarbage('count') < 1100)";
    for limit in ["1M", "1m", "1048576"] {
        let o = orbit(&["-"], script, &[("ORBIT_MEMORY_LIMIT", limit)]);
        assert_eq!(
            out(&o),
            "false\tnot enough memory\ntrue\n",
            "{limit}: {}",
            err(&o)
        );
    }
    let o = orbit(&["-"], "", &[("ORBIT_MEMORY_LIMIT", "lots")]);
    assert!(!o.status.success());
    assert!(err(&o).contains("ORBIT_MEMORY_LIMIT"), "{}", err(&o));
}

/// `ORBIT_VERIFY=none` lowers the checks on binary chunks, unless `-E`.
#[test]
fn verification_level_from_the_environment() {
    use orbit_compiler::bytecode::{Op, format};
    // Fails the structure check: a register past the frame.
    let mut chunk = orbit::Compiler
        .compile(b"local a = 1", "t", orbit::Language::Lua)
        .unwrap();
    chunk.protos[0].code = vec![Op::Return1 { src: 200 }];
    chunk.protos[0].debug = Default::default();
    let path = std::env::temp_dir().join(format!("orbit-verify-{}.out", std::process::id()));
    std::fs::write(&path, format::encode(&chunk, true)).unwrap();
    let script = format!(
        "local f = io.open({:?}, 'rb') print(type(load(f:read('a')))) f:close()",
        path.to_str().unwrap()
    );
    let checked = orbit(&["-e", &script], "", &[]);
    let unchecked = orbit(&["-e", &script], "", &[("ORBIT_VERIFY", "none")]);
    let ignored = orbit(&["-E", "-e", &script], "", &[("ORBIT_VERIFY", "none")]);
    std::fs::remove_file(&path).unwrap();
    assert_eq!(out(&checked), "nil\n", "{}", err(&checked));
    assert_eq!(out(&unchecked), "function\n", "{}", err(&unchecked));
    assert_eq!(out(&ignored), "nil\n", "{}", err(&ignored));
}

/// Without the `c-modules` feature, `package.loadlib` gives Lua's result for
/// a build without dynamic libraries. (A shared build has liborbit's.)
#[test]
#[cfg(not(feature = "c-modules"))]
fn loadlib_is_absent_without_c_modules() {
    let o = orbit(
        &["-e", "print(select(3, package.loadlib('x.so', 'f')))"],
        "",
        &[],
    );
    assert_eq!(out(&o), "absent\n");
}

/// Without the `repl` feature, `-i` is an unknown option.
#[test]
#[cfg(not(feature = "repl"))]
fn no_interactive_mode() {
    let out = Command::new(common::bin("orbit", env!("CARGO_BIN_EXE_orbit")))
        .arg("-i")
        .output()
        .unwrap();
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("unrecognized option '-i'"), "{err}");
    assert!(!err.contains("interactive"), "{err}");
}

#[cfg(feature = "ffi")]
#[test]
fn ffi_is_on_unless_no_ffi() {
    let o = orbit(&["-e", "print(type(require 'std.ffi'), ffi)"], "", &[]);
    assert_eq!(out(&o), "table\tnil\n");
    let o = orbit(
        &[
            "--no-ffi",
            "-e",
            "print(package.loaded['std.ffi'], (pcall(require, 'std.ffi')))",
        ],
        "",
        &[],
    );
    assert_eq!(out(&o), "nil\tfalse\n");
}

#[test]
fn usage_lists_no_ffi() {
    let o = orbit(&["-h"], "", &[]);
    assert!(
        err(&o).contains("--no-ffi  leave out the 'std.ffi' module"),
        "{}",
        err(&o)
    );
}

/// Arguments that are not UTF-8 reach the script as the bytes given, as
/// in `lua`, in `arg`, in `...`, in `-e` code and as the script's name.
#[cfg(unix)]
#[test]
fn arguments_are_bytes() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;
    let dir = std::env::temp_dir().join(format!("orbit-args-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let code = "io.write(arg[0]:match('[^/]*$'), '\\t', tostring(arg[1] == '\\xff\\xfe'), '\\t', \
                select('#', ...), '\\t', tostring((...) == '\\xff\\xfe'), '\\t', tostring(x == '\\xfa'), '\\n')";
    // Some file systems (APFS) refuse names that are not UTF-8.
    let mut name: &[u8] = b"s\xff.lua";
    if std::fs::write(dir.join(OsStr::from_bytes(name)), code).is_err() {
        name = b"s.lua";
        std::fs::write(dir.join(OsStr::from_bytes(name)), code).unwrap();
    }
    let o = Command::new(env!("CARGO_BIN_EXE_orbit"))
        .arg("-e")
        .arg(OsStr::from_bytes(b"x='\xfa'"))
        .arg(dir.join(OsStr::from_bytes(name)))
        .arg(OsStr::from_bytes(b"\xff\xfe"))
        .output()
        .unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    assert!(o.status.success(), "{}", err(&o));
    let mut expected = name.to_vec();
    expected.extend_from_slice(b"\ttrue\t1\ttrue\ttrue\n");
    assert_eq!(o.stdout, expected, "{}", out(&o));

    // An unknown option is reported with its bytes.
    let o = Command::new(env!("CARGO_BIN_EXE_orbit"))
        .arg(OsStr::from_bytes(b"-\xff"))
        .output()
        .unwrap();
    assert_eq!(o.status.code(), Some(1));
    let needle = b"unrecognized option '-\xff'";
    assert!(
        o.stderr.windows(needle.len()).any(|w| w == needle),
        "{}",
        err(&o)
    );
}

/// On Windows, opening a directory or a name ending in `/` fails with the
/// errno lua5.5 built with the C runtime reports.
#[cfg(windows)]
#[test]
fn windows_trailing_slash_errors() {
    let o = Command::new(env!("CARGO_BIN_EXE_orbit"))
        .arg("-e")
        .arg(
            "for _, c in ipairs{{'/', 'w'}, {'./', 'w'}, {'./', 'r'}, {'.', 'w'}, \
             {'orbit-no-such-dir/', 'w'}, {'orbit-no-such-dir/', 'r'}} do \
             print(c[1], c[2], select(3, io.open(c[1], c[2]))) end",
        )
        .output()
        .unwrap();
    assert!(o.status.success(), "{}", err(&o));
    assert_eq!(
        out(&o).replace('\r', ""),
        "/\tw\t13\n./\tw\t13\n./\tr\t13\n.\tw\t13\n\
         orbit-no-such-dir/\tw\t22\norbit-no-such-dir/\tr\t2\n"
    );
}
