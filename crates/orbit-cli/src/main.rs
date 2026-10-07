//! `orbit`: command-line interpreter for Lua 5.5 and OrbitScript.
//!
//! `orbit [--dump-ast | --dump-hir | --dump-bytecode] script.lua [args...]`
//! runs a script like the reference `lua` does: `arg` holds the script name
//! and arguments, `...` the arguments, and an uncaught error prints
//! `<program>: <message>` plus a traceback to stderr with exit status 1.
//! `ORBIT_GC_STRESS=1` collects at nearly every safe point.

use std::process::ExitCode;

use orbit::{Error, Lua, Value};
use orbit_lua_syntax::ParseOptions;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let prog = args.first().cloned().unwrap_or_else(|| "orbit".into());
    let mut i = 1;
    let mut dump = None;
    while i < args.len() && args[i].starts_with("--dump-") {
        dump = Some(args[i].clone());
        i += 1;
    }
    if i < args.len() && (args[i] == "-v" || args[i] == "--version") {
        println!("OrbitScript {}", orbit::VERSION);
        return ExitCode::SUCCESS;
    }
    let Some(script) = args.get(i) else {
        eprintln!("usage: {prog} [--dump-ast|--dump-hir|--dump-bytecode] script.lua [args...]");
        return ExitCode::FAILURE;
    };
    if let Some(what) = dump {
        let src = match std::fs::read(script) {
            Ok(s) => s,
            Err(_) => {
                eprintln!("{prog}: cannot open {script}");
                return ExitCode::FAILURE;
            }
        };
        let options = ParseOptions {
            skip_comment_line: true,
            ..ParseOptions::default()
        };
        return dump_stage(&prog, &what, &src, script, &options);
    }

    let mut lua = Lua::new();
    if std::env::var_os("ORBIT_GC_STRESS").is_some_and(|v| v != "0") {
        lua.vm().heap.set_stress(true);
    }
    // Read the script as `loadfile` does (byte-order mark, `#` first line).
    let (src, source) = match orbit::read_chunk_file(Some(script.as_bytes())) {
        Ok(r) => r,
        Err(m) => {
            eprintln!("{prog}: {m}");
            return ExitCode::FAILURE;
        }
    };
    let name = orbit::chunk_id(&source);
    let script_args = &args[i + 1..];
    let f = match lua.load(&src, &name) {
        Ok(f) => f,
        Err(e) => return fail(&mut lua, &prog, e),
    };
    lua.vm().pin(f);
    set_arg_table(lua.vm(), &prog, &args[i..]);
    let mut call_args = Vec::with_capacity(script_args.len());
    for a in script_args {
        let v = lua.vm().new_str(a.as_bytes());
        lua.vm().pin(v);
        call_args.push(v);
    }
    let result = lua.call(f, &call_args);
    let _ = lua.vm().flush();
    match result {
        Ok(_) => ExitCode::SUCCESS,
        Err(e) => fail(&mut lua, &prog, e),
    }
}

fn set_arg_table(vm: &mut orbit::Vm, prog: &str, script_and_args: &[String]) {
    let t = vm.new_table();
    vm.pin(Value::Table(t));
    let p = vm.new_str(prog.as_bytes());
    vm.table_set(t, Value::Int(-1), p);
    for (n, a) in script_and_args.iter().enumerate() {
        let v = vm.new_str(a.as_bytes());
        vm.table_set(t, Value::Int(n as i64), v);
    }
    vm.set_global("arg", Value::Table(t));
    vm.unpin(Value::Table(t));
}

fn fail(lua: &mut Lua, prog: &str, e: Error) -> ExitCode {
    let _ = lua.vm().flush();
    match e {
        Error::Syntax(m) => eprintln!("{prog}: {m}"),
        Error::Runtime { message, traceback } => {
            eprintln!("{prog}: {message}");
            if let Some(tb) = traceback {
                eprintln!("{tb}");
            }
        }
    }
    ExitCode::FAILURE
}

fn dump_stage(prog: &str, what: &str, src: &[u8], name: &str, options: &ParseOptions) -> ExitCode {
    use std::io::Write;
    let out: Result<Vec<u8>, String> = match what {
        "--dump-ast" => orbit_lua_syntax::parse_with(name, src, options)
            .map(|ast| orbit_lua_syntax::unparse(&ast))
            .map_err(|d| d.to_lua_message(name)),
        "--dump-hir" => orbit_lua_syntax::to_hir(name, src, options)
            .map(|h| orbit_hir::print(&h).into_bytes())
            .map_err(|d| d.to_lua_message(name)),
        "--dump-bytecode" => orbit::compile(src, name, options)
            .map(|c| orbit_compiler::disassemble(&c).into_bytes())
            .map_err(|e| e.to_string()),
        other => Err(format!("unknown option '{other}'")),
    };
    match out {
        Ok(bytes) => {
            let _ = std::io::stdout().write_all(&bytes);
            ExitCode::SUCCESS
        }
        Err(m) => {
            eprintln!("{prog}: {m}");
            ExitCode::FAILURE
        }
    }
}
