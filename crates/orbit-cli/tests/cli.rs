//! The command line against the reference `lua`'s observable behaviour
//! (`main.lua` checks the rest once the libraries it uses land).

use std::io::Write;
use std::process::{Command, Output, Stdio};

/// Run `orbit` with `args`, `stdin` as its input and a clean environment
/// apart from `env`.
fn orbit(args: &[&str], stdin: &str, env: &[(&str, &str)]) -> Output {
    let mut c = Command::new(env!("CARGO_BIN_EXE_orbit"));
    c.args(args)
        .env_clear()
        .envs(env.iter().copied())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = c.spawn().unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(stdin.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

fn out(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn err(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
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
    assert!(out(&o).contains("Lua 5.5.1") && out(&o).ends_with("PUC-Rio\n"));
}

#[test]
fn environment() {
    let o = orbit(&["-e", "print(X)"], "", &[("LUA_INIT", "X=3")]);
    assert_eq!(out(&o), "3\n");
    let init = [("LUA_INIT_5_5", "X=10"), ("LUA_INIT", "X=3")];
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
fn interactive() {
    let prompts = "_PROMPT='' _PROMPT2=''";
    let o = orbit(&["-e", prompts, "-i"], "(6*2-6)\na =\n10\nprint(a)\na", &[]);
    assert!(out(&o).ends_with("6\n10\n10\n\n"), "{}", out(&o));
    let o = orbit(&["-i"], "a.\n", &[]);
    assert!(err(&o).contains("near <eof>"), "{}", err(&o));
    assert!(out(&o).ends_with("> >> > \n"), "{}", out(&o));
    let o = orbit(&["-i"], "  \t local x\nlocal1 = 10\nlocal1 + 3\n", &[]);
    assert_eq!(err(&o).matches("warning: locals do not survive").count(), 1);
    assert!(out(&o).contains("13\n"));
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
    let mut child = Command::new(env!("CARGO_BIN_EXE_orbit"))
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
    assert_eq!(line.as_deref(), Ok("ready\n"));
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

/// Without the `c-modules` feature, `package.loadlib` gives Lua's result for
/// a build without dynamic libraries.
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
