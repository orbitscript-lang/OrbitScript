//! Parts of the embedding API the other tests don't reach: each public
//! function is run by some test (tests/coverage checks it).

use std::process::Command;

use orbit::stack::{Stack, StackResult, Status, Type};
use orbit::{
    Chunk, Class, Config, Error, GcMode, Isolate, LocaleMode, Runtime, Scope, Value, native,
};

fn isolate() -> Isolate {
    Runtime::new().new_isolate(Config::default())
}

/// The values `src` returns, as `tostring` shows them.
fn run<'s>(s: &'s Scope<'s>, src: &str) -> Result<Vec<String>, Error> {
    let out = s.load(Chunk::new(src).name("=api"))?.call(())?;
    out.iter()
        .map(|v| Ok(String::from_utf8_lossy(&v.to_display()?).into_owned()))
        .collect()
}

#[test]
fn runtime_version_and_compiler() {
    let rt = Runtime::new();
    assert!(!rt.version().is_empty());
    let compiled = rt
        .compiler()
        .compile_chunk(&Chunk::new("return ... + 1").name("=compiled"))
        .unwrap();
    assert!(
        rt.compiler()
            .compile_chunk(&Chunk::new("return +").name("=bad"))
            .is_err()
    );
    let mut iso = rt.new_isolate(Config::default());
    let r: i64 = iso
        .scope(|s| s.load_compiled(&compiled).call(41)?[0].get())
        .unwrap();
    assert_eq!(r, 42);
}

#[test]
fn config_and_collector_settings() {
    let mut iso = Runtime::new().new_isolate(Config::default().gc_mode(GcMode::Incremental));
    // `collectgarbage` with a mode gives the mode it replaces.
    let was = iso
        .scope(|s| run(s, "return collectgarbage('incremental')"))
        .unwrap();
    assert_eq!(was, ["incremental"]);
    iso.gc().set_mode(GcMode::Generational);
    let was = iso
        .scope(|s| run(s, "return collectgarbage('incremental')"))
        .unwrap();
    assert_eq!(was, ["generational"]);

    iso.gc().set_limit(Some(64 << 20));
    assert_eq!(iso.gc().limit(), Some(64 << 20));
    iso.gc().set_limit(None);
    assert_eq!(iso.gc().limit(), None);

    iso.gc().set_stress(true);
    let n = iso
        .scope(|s| run(s, "local t = {} for i = 1, 50 do t[i] = {i} end return #t"))
        .unwrap();
    iso.gc().set_stress(false);
    assert_eq!(n, ["50"]);

    assert_eq!(iso.locale_mode(), LocaleMode::Isolate);
    iso.set_locale_mode(LocaleMode::Process);
    assert_eq!(iso.locale_mode(), LocaleMode::Process);
    iso.set_locale_mode(LocaleMode::Isolate);

    iso.exec(Chunk::new("answer = 42")).unwrap();
    let g = iso.globals();
    let answer: i64 = iso.scope(|s| g.get(s)?.get("answer")).unwrap();
    assert_eq!(answer, 42);
}

#[test]
fn numbers_in_the_isolates_locale() {
    let mut iso = isolate();
    iso.scope(|s| {
        assert_eq!(&*s.decimal_point(), b".");
        assert!(matches!(s.parse_number(b" 0x10 "), Some(Value::Int(16))));
        assert!(matches!(s.parse_number(b"2.5"), Some(Value::Float(f)) if f == 2.5));
        assert!(s.parse_number(b"2.5x").is_none());
        Ok::<_, Error>(())
    })
    .unwrap();
}

#[test]
fn values_and_strings() {
    let mut iso = isolate();
    iso.scope(|s| {
        let out = s
            .load(Chunk::new(
                "local t = setmetatable({}, {__eq = function() return true end})
                 return t, setmetatable({}, getmetatable(t)), 'héllo', '', \
                 coroutine.create(function() end), '\\xff'",
            ))?
            .call(())?;
        let (a, b) = (out[0], out[1]);
        let ud = Value::Userdata(s.create_userdata(1u8));
        // `__eq` makes them equal, but not raw-equal.
        assert!(!a.raw_eq(&b));
        assert!(a.raw_eq(&a));
        assert!(Value::Int(1).raw_eq(&Value::Int(1)));
        assert!(a.metatable().is_some());
        assert!(Value::Int(1).metatable().is_none());
        // Strings share a metatable, with `__index` the string library.
        assert!(out[2].metatable().is_some());

        let text = out[2].as_str().unwrap();
        assert_eq!(text.len(), 6);
        assert!(!text.is_empty());
        assert_eq!(text.to_str()?, "héllo");
        assert_eq!(&*text.to_orbit_string(), "héllo".as_bytes());
        assert!(out[3].as_str().unwrap().is_empty());
        assert!(out[5].as_str().unwrap().to_str().is_err());
        assert!(Value::Int(1).as_str().is_none());

        assert!(out[4].as_thread().is_some());
        assert!(out[2].as_thread().is_none());
        assert!(ud.as_userdata().is_some());
        assert!(out[4].as_userdata().is_none());

        // A pinned value outlives the scope; a plain one needs no pin.
        assert!(Value::Int(1).pin().is_none());
        let pinned = out[2].pin().unwrap();
        assert_eq!(pinned.get(s)?.as_str().unwrap().to_str()?, "héllo");
        Ok::<_, Error>(())
    })
    .unwrap();
}

#[test]
fn a_message_handler_sees_the_error() {
    let mut iso = isolate();
    iso.scope(|s| {
        let f = s.load(Chunk::new("error('boom', 0)"))?;
        let handler = s.load(Chunk::new("return 'handled: ' .. ..."))?;
        let e = f.call_handled((), handler).unwrap_err();
        assert_eq!(e.to_string(), "handled: boom");
        let ok = s.load(Chunk::new("return 7"))?.call_handled((), handler)?;
        assert_eq!(ok[0].get::<i64>()?, 7);
        Ok::<_, Error>(())
    })
    .unwrap();
}

struct Account {
    balance: i64,
}

#[test]
fn class_functions_metamethods_and_borrows() {
    let mut iso = isolate();
    iso.scope(|s| {
        let mut methods = None;
        let class: Class<Account> = s.define_class("Account", |c| {
            c.function("opening", |call| call.ret(100))?;
            c.metamethod("__len", |call| {
                let u = call.arg(0).as_userdata().unwrap();
                let n = u.borrow::<Account>()?.balance;
                call.ret(n)
            })?;
            assert!(c.metamethod("__gc", |call| call.ret(())).is_err());
            methods = Some(c.methods());
            Ok(())
        })?;
        s.globals().set("Account", methods.unwrap())?;
        let u = class.instance(s, Account { balance: 5 })?;
        u.borrow_mut::<Account>()?.balance += 10;
        assert!(u.borrow_mut::<String>().is_err());
        s.globals().set("acct", u)?;
        assert_eq!(run(s, "return Account.opening(), #acct")?, ["100", "15"]);
        Ok::<_, Error>(())
    })
    .unwrap();
}

#[test]
fn host_function_calls() {
    let mut iso = isolate();
    iso.scope(|s| {
        let g = s.globals();
        g.set(
            "count",
            s.create_function(|call| call.ret(call.nargs() as i64)),
        )?;
        g.set(
            "positive",
            s.create_function(|call| {
                let n: i64 = call.check(0)?;
                if n <= 0 {
                    return Err(call.arg_error(0, "not positive"));
                }
                call.ret(n)
            }),
        )?;
        g.set(
            "via",
            s.create_function(|call| {
                let f = call.check(0)?;
                call.tail_call(f, (call.arg(1), call.arg(2)))
            }),
        )?;
        g.set(
            "trace",
            s.create_function(|call| call.ret(call.traceback(1))),
        )?;
        g.set("oom", s.create_function(|_| Err(Error::memory())))?;
        let out = run(
            s,
            "local _, e = pcall(positive, -1)
             local _, m = pcall(oom)
             local function where() return trace() end
             return count(1, nil, 3), e, via(math.max, 3, 9), m, where()",
        )?;
        assert_eq!(out[0], "3");
        assert_eq!(out[1], "bad argument #1 to 'positive' (not positive)");
        assert_eq!(out[2], "9");
        assert_eq!(out[3], "not enough memory");
        assert!(out[4].starts_with("stack traceback:"), "{}", out[4]);
        assert!(out[4].contains("where"), "{}", out[4]);
        Ok::<_, Error>(())
    })
    .unwrap();
}

/// `Call::exit` ends the process, so it runs in a child: this test binary
/// again, running only this test with `ORBIT_TEST_EXIT` set.
#[test]
fn host_exit_ends_the_process() {
    if std::env::var_os("ORBIT_TEST_EXIT").is_some() {
        let mut iso = isolate();
        iso.before_exit(|| println!("before exit"));
        let _ = iso.scope(|s| {
            let g = s.globals();
            g.set("quit", s.create_function(|call| call.exit(7, true)))?;
            g.set(
                "note",
                s.create_function(|call| {
                    println!("{}", call.check::<String>(0)?);
                    call.ret(())
                }),
            )?;
            run(
                s,
                "setmetatable({}, {__gc = function() note('closed') end}) quit()",
            )
        });
        unreachable!("quit ends the process");
    }
    let out = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "host_exit_ends_the_process", "--nocapture"])
        .env("ORBIT_TEST_EXIT", "1")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(out.status.code(), Some(7), "{stdout}");
    assert!(stdout.contains("closed\nbefore exit"), "{stdout}");
}

/// The stack API's codes and threads.
#[native(name = "probe")]
fn probe(s: &mut Stack<'_>) -> StackResult<(bool, bool)> {
    let main = s.main_thread();
    let live = s.thread_is_live(main);
    let running_is_main = s.running_thread() == main;
    Ok((live, running_is_main))
}

#[native(name = "no_memory")]
fn no_memory(s: &mut Stack<'_>) -> StackResult<()> {
    Err(s.memory_error())
}

#[test]
fn stack_codes_threads_and_errors() {
    assert_eq!(Type::from_code(-1), Some(Type::None));
    assert_eq!(Type::from_code(8), Some(Type::Thread));
    assert_eq!(Type::from_code(9), None);
    assert_eq!(Status::from_code(0), Some(Status::Ok));
    assert_eq!(Status::from_code(5), Some(Status::Handler));
    assert_eq!(Status::from_code(6), None);

    let mut iso = isolate();
    let main = iso.control().main_thread();
    iso.scope(|s| {
        s.globals().set("probe", s.create_native::<probe>())?;
        s.globals()
            .set("no_memory", s.create_native::<no_memory>())?;
        let out = run(
            s,
            "local a, b = probe()
             local c, d = coroutine.wrap(probe)()
             return a, b, c, d, select(2, pcall(no_memory))",
        )?;
        assert_eq!(out, ["true", "true", "true", "false", "not enough memory"]);
        Ok::<_, Error>(())
    })
    .unwrap();
    // The isolate's view of the main thread is the natives' view.
    assert_eq!(iso.control().main_thread(), main);
}
