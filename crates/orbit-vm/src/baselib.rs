//! The basic library (`lbaselib.c`) and the coroutine library (`lcorolib.c`).
//!
//! They live here rather than in `orbit-stdlib` because they are tied to the
//! interpreter's internals (protected calls, continuations, coroutine switching) and
//! the VM's own tests need them. Stream D can move them once the native API settles.

use crate::error::{LuaResult, Status};
use crate::number;
use crate::state::{Action, KStatus, Vm};
use orbit_core::Value;
use std::io::Write;

/// Opens the basic library into the global table.
pub fn open_base(vm: &mut Vm) {
    let g = vm.new_lib(
        "_G",
        &[
            ("assert", assert),
            ("collectgarbage", collectgarbage),
            ("dofile", dofile),
            ("error", error),
            ("getmetatable", getmetatable),
            ("ipairs", ipairs),
            ("loadfile", loadfile),
            ("load", load),
            ("next", next),
            ("pairs", pairs),
            ("pcall", pcall),
            ("print", print),
            ("warn", warn),
            ("rawequal", rawequal),
            ("rawlen", rawlen),
            ("rawget", rawget),
            ("rawset", rawset),
            ("select", select),
            ("setmetatable", setmetatable),
            ("tonumber", tonumber),
            ("tostring", tostring),
            ("type", type_),
            ("xpcall", xpcall),
        ],
    );
    let v = vm.heap.str_value("Lua 5.5");
    vm.heap.raw_set_name(g, b"_VERSION", v).expect("string key");
    vm.heap
        .raw_set_name(g, b"_G", Value::Table(g))
        .expect("string key");
}

/// Opens the coroutine library as the global `coroutine`.
pub fn open_coroutine(vm: &mut Vm) {
    vm.new_lib(
        "coroutine",
        &[
            ("create", co_create),
            ("resume", co_resume),
            ("running", co_running),
            ("status", co_status),
            ("wrap", co_wrap),
            ("yield", co_yield),
            ("isyieldable", co_isyieldable),
            ("close", co_close),
        ],
    );
}

fn print(vm: &mut Vm) -> LuaResult<Action> {
    let n = vm.top();
    let mut out = Vec::new();
    for i in 1..=n {
        let s = vm.to_display(vm.arg(i))?;
        if i > 1 {
            out.push(b'\t');
        }
        out.extend_from_slice(vm.heap.string(s).as_bytes());
    }
    out.push(b'\n');
    let mut so = std::io::stdout().lock();
    let _ = so.write_all(&out);
    let _ = so.flush();
    Ok(Action::Return(0))
}

fn warn(vm: &mut Vm) -> LuaResult<Action> {
    let n = vm.top();
    vm.check_string(1)?;
    for i in 2..=n {
        vm.check_string(i)?;
    }
    for i in 1..=n {
        let Value::Str(s) = vm.arg(i) else {
            unreachable!()
        };
        let bytes = vm.heap.string(s).as_bytes().to_vec();
        vm.warn(&bytes, i < n);
    }
    Ok(Action::Return(0))
}

/// `b_str2int`: an integer numeral in `base`, surrounded by optional spaces.
fn str_to_int_base(s: &[u8], base: u32) -> Option<i64> {
    let is_space = |c: &u8| b" \x0c\n\r\t\x0b".contains(c);
    let mut i = 0;
    while i < s.len() && is_space(&s[i]) {
        i += 1;
    }
    let mut neg = false;
    if i < s.len() && s[i] == b'-' {
        neg = true;
        i += 1;
    } else if i < s.len() && s[i] == b'+' {
        i += 1;
    }
    if i >= s.len() || !s[i].is_ascii_alphanumeric() {
        return None;
    }
    let mut n: u64 = 0;
    while i < s.len() && s[i].is_ascii_alphanumeric() {
        let c = s[i];
        let digit = if c.is_ascii_digit() {
            u32::from(c - b'0')
        } else {
            u32::from(c.to_ascii_uppercase() - b'A') + 10
        };
        if digit >= base {
            return None;
        }
        n = n
            .wrapping_mul(u64::from(base))
            .wrapping_add(u64::from(digit));
        i += 1;
    }
    while i < s.len() && is_space(&s[i]) {
        i += 1;
    }
    if i != s.len() {
        return None;
    }
    Some(if neg { 0u64.wrapping_sub(n) } else { n } as i64)
}

fn tonumber(vm: &mut Vm) -> LuaResult<Action> {
    if vm.arg(2).is_nil() {
        let v = vm.arg(1);
        let r = match v {
            Value::Int(_) | Value::Float(_) => Some(v),
            Value::Str(s) => number::str_to_number(vm.heap.string(s).as_bytes()),
            _ => None,
        };
        if let Some(r) = r {
            vm.push(r);
            return Ok(Action::Return(1));
        }
        vm.check_any(1)?;
    } else {
        let base = vm.check_integer(2)?;
        let Value::Str(s) = vm.arg(1) else {
            return Err(vm.type_arg_error(1, "string"));
        };
        if !(2..=36).contains(&base) {
            return Err(vm.arg_error(2, "base out of range"));
        }
        if let Some(n) = str_to_int_base(vm.heap.string(s).as_bytes(), base as u32) {
            vm.push(Value::Int(n));
            return Ok(Action::Return(1));
        }
    }
    vm.push(Value::Nil);
    Ok(Action::Return(1))
}

fn error(vm: &mut Vm) -> LuaResult<Action> {
    let level = vm.opt_integer(2, 1)?;
    vm.set_top(1);
    let mut v = vm.arg(1);
    if let Value::Str(s) = v
        && level > 0
    {
        let mut msg = vm.where_(level as usize);
        msg.extend_from_slice(vm.heap.string(s).as_bytes());
        v = vm.heap.str_value(msg);
    }
    Err(vm.raise(v))
}

fn getmetatable(vm: &mut Vm) -> LuaResult<Action> {
    let v = vm.check_any(1)?;
    let r = match vm.heap.metatable(v) {
        None => Value::Nil,
        Some(mt) => match vm.heap.raw_get_name(mt, b"__metatable") {
            Value::Nil => Value::Table(mt),
            p => p,
        },
    };
    vm.push(r);
    Ok(Action::Return(1))
}

fn setmetatable(vm: &mut Vm) -> LuaResult<Action> {
    let mt = vm.arg(2);
    let t = vm.check_table(1)?;
    let mt = match mt {
        Value::Nil => None,
        Value::Table(m) => Some(m),
        _ => return Err(vm.type_arg_error(2, "nil or table")),
    };
    if !vm.meta_field(Value::Table(t), b"__metatable").is_nil() {
        return Err(vm.error("cannot change a protected metatable"));
    }
    vm.heap.set_table_metatable(t, mt);
    vm.set_top(1);
    Ok(Action::Return(1))
}

fn rawequal(vm: &mut Vm) -> LuaResult<Action> {
    let a = vm.check_any(1)?;
    let b = vm.check_any(2)?;
    let r = vm.heap.raw_eq(a, b);
    vm.push(Value::Bool(r));
    Ok(Action::Return(1))
}

fn rawlen(vm: &mut Vm) -> LuaResult<Action> {
    let n = match vm.arg(1) {
        Value::Table(t) => vm.heap.raw_len(t) as i64,
        Value::Str(s) => vm.heap.string(s).len() as i64,
        _ => return Err(vm.type_arg_error(1, "table or string")),
    };
    vm.push(Value::Int(n));
    Ok(Action::Return(1))
}

fn rawget(vm: &mut Vm) -> LuaResult<Action> {
    let t = vm.check_table(1)?;
    let k = vm.check_any(2)?;
    let v = vm.heap.raw_get(t, k);
    vm.push(v);
    Ok(Action::Return(1))
}

fn rawset(vm: &mut Vm) -> LuaResult<Action> {
    let t = vm.check_table(1)?;
    let k = vm.check_any(2)?;
    let v = vm.check_any(3)?;
    if let Err(e) = vm.heap.raw_set(t, k, v) {
        return Err(vm.set_error(e));
    }
    vm.set_top(1);
    Ok(Action::Return(1))
}

fn collectgarbage(vm: &mut Vm) -> LuaResult<Action> {
    const OPTS: &[&str] = &[
        "stop",
        "restart",
        "collect",
        "count",
        "step",
        "isrunning",
        "generational",
        "incremental",
        "param",
    ];
    let opt = vm.check_option(1, Some("collect"), OPTS)?;
    let r = match OPTS[opt] {
        "stop" => {
            vm.heap.gc_mut().stop();
            Value::Int(0)
        }
        "restart" => {
            vm.heap.gc_mut().restart();
            Value::Int(0)
        }
        "collect" => {
            if vm.in_finalizers {
                Value::Nil
            } else {
                vm.full_gc()?;
                Value::Int(0)
            }
        }
        "count" => Value::Float(vm.heap.gc().bytes() as f64 / 1024.0),
        "step" => {
            let n = vm.opt_integer(2, 0)?;
            if vm.in_finalizers {
                Value::Nil
            } else {
                let cycles = vm.heap.gc().cycles();
                let live = vm.st.top;
                let stopped = vm.heap.gc().is_stopped();
                if stopped {
                    vm.heap.gc_mut().restart();
                }
                // A positive size does the work of that many KiB of allocation; zero
                // does one basic step. The collector leaves the step size as its debt
                // when the cycle goes on, so it is capped at the heap size: a huge
                // size must not postpone the next automatic step indefinitely.
                let stepsize = vm.heap.gc().params().stepsize;
                if n > 0 {
                    let kib = usize::try_from(n).unwrap_or(usize::MAX);
                    vm.heap.gc_mut().params_mut().stepsize = kib
                        .saturating_mul(1024)
                        .min(vm.heap.gc().bytes().max(stepsize));
                }
                let r = vm.gc_step(live);
                vm.heap.gc_mut().params_mut().stepsize = stepsize;
                r?;
                if stopped {
                    vm.heap.gc_mut().stop();
                }
                Value::Bool(vm.heap.gc().cycles() != cycles)
            }
        }
        "isrunning" => Value::Bool(!vm.heap.gc().is_stopped()),
        mode @ ("generational" | "incremental") => {
            let old = if vm.gc_generational {
                "generational"
            } else {
                "incremental"
            };
            vm.gc_generational = mode == "generational";
            vm.heap.str_value(old)
        }
        _ => {
            const PARAMS: &[&str] = &[
                "minormul",
                "majorminor",
                "minormajor",
                "pause",
                "stepmul",
                "stepsize",
            ];
            let p = vm.check_option(2, None, PARAMS)?;
            let value = vm.opt_integer(3, -1)?;
            let params = vm.heap.gc_mut().params_mut();
            let old = match PARAMS[p] {
                "pause" => {
                    let old = i64::from(params.pause);
                    if value >= 0 {
                        params.pause = value as u32;
                    }
                    old
                }
                "stepmul" => {
                    let old = i64::from(params.stepmul);
                    if value >= 0 {
                        params.stepmul = value as u32;
                    }
                    old
                }
                "stepsize" => {
                    let old = params.stepsize as i64;
                    if value >= 0 {
                        params.stepsize = value as usize;
                    }
                    old
                }
                _ => 0,
            };
            Value::Int(old)
        }
    };
    vm.push(r);
    Ok(Action::Return(1))
}

fn type_(vm: &mut Vm) -> LuaResult<Action> {
    let v = vm.check_any(1)?;
    let s = vm.heap.str_value(v.type_name());
    vm.push(s);
    Ok(Action::Return(1))
}

fn next(vm: &mut Vm) -> LuaResult<Action> {
    let t = vm.check_table(1)?;
    vm.set_top(2);
    let k = vm.arg(2);
    match vm.heap.next(t, k) {
        Ok(Some((k, v))) => {
            vm.push(k);
            vm.push(v);
            Ok(Action::Return(2))
        }
        Ok(None) => {
            vm.push(Value::Nil);
            Ok(Action::Return(1))
        }
        Err(_) => Err(vm.error_msg("invalid key to 'next'")),
    }
}

fn pairs_cont(_vm: &mut Vm, _s: KStatus, _ctx: isize) -> LuaResult<Action> {
    Ok(Action::Return(4))
}

fn pairs(vm: &mut Vm) -> LuaResult<Action> {
    let t = vm.check_any(1)?;
    let tm = vm.meta_field(t, b"__pairs");
    if tm.is_nil() {
        let f = vm.global_next();
        vm.push(f);
        vm.push(t);
        vm.push(Value::Nil);
        vm.push(Value::Nil);
        Ok(Action::Return(4))
    } else {
        vm.push(tm);
        vm.push(t);
        Ok(Action::Call {
            nargs: 1,
            nresults: Some(4),
            k: pairs_cont,
            ctx: 0,
        })
    }
}

fn ipairs_aux(vm: &mut Vm) -> LuaResult<Action> {
    let i = vm.check_integer(2)?.wrapping_add(1);
    vm.push(Value::Int(i));
    let t = vm.arg(1);
    if let Value::Table(h) = t {
        let v = vm.heap.raw_get_int(h, i);
        if !v.is_nil() {
            vm.push(v);
            return Ok(Action::Return(2));
        }
    }
    let v = vm.index_value(t, Value::Int(i))?;
    vm.push(v);
    Ok(Action::Return(if v.is_nil() { 1 } else { 2 }))
}

fn ipairs(vm: &mut Vm) -> LuaResult<Action> {
    let t = vm.check_any(1)?;
    let f = vm.ipairs_aux_fn();
    vm.push(f);
    vm.push(t);
    vm.push(Value::Int(0));
    Ok(Action::Return(3))
}

/// Finishes `load`: sets the environment upvalue, or returns `fail, msg`.
fn load_result(vm: &mut Vm, r: LuaResult<Value>, env: Option<Value>) -> LuaResult<Action> {
    match r {
        Ok(f) => {
            if let (Some(env), Value::LuaFn(cl)) = (env, f)
                && let Some(&uv) = vm.heap.get(cl).upvals.first()
            {
                vm.heap
                    .update(uv, |u| *u = orbit_core::object::UpVal::Closed(env));
            }
            vm.push(f);
            Ok(Action::Return(1))
        }
        Err(e) if e.status == Status::Syntax || e.status == Status::Runtime => {
            vm.push(Value::Nil);
            vm.push(e.value);
            Ok(Action::Return(2))
        }
        Err(e) => Err(e),
    }
}

/// The `mode` argument of `load`/`loadfile` (`getMode`): raises for an invalid mode.
fn get_mode(vm: &mut Vm, arg: usize) -> LuaResult<Option<Vec<u8>>> {
    let Some(mode) = vm.opt_string(arg)? else {
        return Ok(None);
    };
    let mode = vm.heap.string(mode).as_bytes().to_vec();
    if mode.contains(&b'B') {
        return Err(vm.arg_error(arg, "invalid mode"));
    }
    Ok(Some(mode))
}

/// Loads a chunk. Errors are load failures (`fail, msg` for `load`).
fn load_chunk(vm: &mut Vm, src: &[u8], chunkname: &[u8], mode: Option<&[u8]>) -> LuaResult<Value> {
    if let Some(mode) = mode {
        let binary = src.first() == Some(&0x1b);
        let (want, kind) = if binary {
            (b'b', "binary")
        } else {
            (b't', "text")
        };
        if !mode.contains(&want) {
            let m = String::from_utf8_lossy(mode).into_owned();
            let msg = format!("attempt to load a {kind} chunk (mode is '{m}')");
            return Err(crate::Error {
                value: vm.heap.str_value(msg),
                status: Status::Syntax,
            });
        }
    }
    if src.first() == Some(&0x1b) {
        return vm.undump(src, chunkname);
    }
    vm.load_with_env(src, chunkname, Some(Value::Table(vm.globals)))
}

fn load(vm: &mut Vm) -> LuaResult<Action> {
    let mode = get_mode(vm, 3)?;
    let env = (vm.top() >= 4).then(|| vm.arg(4));
    let chunk = vm.arg(1);
    let (src, chunkname) = match chunk {
        Value::Str(s) => {
            let src = vm.heap.string(s).as_bytes().to_vec();
            let name = match vm.opt_string(2)? {
                Some(n) => vm.heap.string(n).as_bytes().to_vec(),
                None => src.clone(),
            };
            (src, name)
        }
        Value::Int(_) | Value::Float(_) => {
            let s = vm.check_string(1)?;
            let src = vm.heap.string(s).as_bytes().to_vec();
            let name = match vm.opt_string(2)? {
                Some(n) => vm.heap.string(n).as_bytes().to_vec(),
                None => src.clone(),
            };
            (src, name)
        }
        _ => {
            let name = match vm.opt_string(2)? {
                Some(n) => vm.heap.string(n).as_bytes().to_vec(),
                None => b"=(load)".to_vec(),
            };
            vm.check_function(1)?;
            let mut src = Vec::new();
            loop {
                vm.check_stack(2)?;
                let f = vm.arg(1);
                vm.push(f);
                if let Err(e) = vm.pcall(0, Some(1), 0) {
                    vm.push(Value::Nil);
                    vm.push(e.value);
                    return Ok(Action::Return(2));
                }
                match vm.pop() {
                    Value::Nil => break,
                    Value::Str(s) => {
                        let piece = vm.heap.string(s).as_bytes();
                        if piece.is_empty() {
                            break;
                        }
                        src.extend_from_slice(piece);
                    }
                    Value::Int(_) | Value::Float(_) => {
                        // lua_isstring accepts numbers.
                    }
                    _ => {
                        let e = vm.error("reader function must return a string");
                        vm.push(Value::Nil);
                        vm.push(e.value);
                        return Ok(Action::Return(2));
                    }
                }
            }
            (src, name)
        }
    };
    let r = load_chunk(vm, &src, &chunkname, mode.as_deref());
    load_result(vm, r, env)
}

/// Reads a file for `loadfile`/`dofile`; `None` means standard input.
fn read_source(vm: &mut Vm, fname: Option<&[u8]>) -> LuaResult<(Vec<u8>, Vec<u8>)> {
    use std::io::Read;
    match fname {
        None => {
            let mut src = Vec::new();
            let _ = std::io::stdin().read_to_end(&mut src);
            Ok((src, b"=stdin".to_vec()))
        }
        Some(name) => {
            let path = String::from_utf8_lossy(name).into_owned();
            match std::fs::read(&path) {
                Ok(src) => {
                    let mut chunkname = b"@".to_vec();
                    chunkname.extend_from_slice(name);
                    Ok((src, chunkname))
                }
                Err(e) => {
                    let msg = format!("cannot open {path}: {}", io_error_text(&e));
                    Err(crate::Error {
                        value: vm.heap.str_value(msg),
                        status: Status::Runtime,
                    })
                }
            }
        }
    }
}

/// The C library's text for an I/O error (`strerror`), without Rust's
/// " (os error N)" suffix.
pub fn io_error_text(e: &std::io::Error) -> String {
    let s = e.to_string();
    match s.find(" (os error") {
        Some(i) => s[..i].to_string(),
        None => s,
    }
}

/// `luaL_loadfilex`: loads the file `fname` (standard input for `None`) as a text or
/// binary chunk, skipping a first line starting with `#`. `mode` is as for `load`.
pub fn load_file(vm: &mut Vm, fname: Option<&[u8]>, mode: Option<&[u8]>) -> LuaResult<Value> {
    let (src, chunkname) = read_source(vm, fname)?;
    let mut body = orbit_lua_syntax::skip_file_prefix(&src);
    // A text chunk keeps the comment's newline, so line numbers stay right; a binary
    // chunk after a comment starts on the next line.
    if body.starts_with(b"\n\x1b") {
        body = &body[1..];
    }
    let body = body.to_vec();
    load_chunk(vm, &body, &chunkname, mode)
}

fn loadfile(vm: &mut Vm) -> LuaResult<Action> {
    let fname = vm
        .opt_string(1)?
        .map(|s| vm.heap.string(s).as_bytes().to_vec());
    let mode = get_mode(vm, 2)?;
    let env = (vm.top() >= 3).then(|| vm.arg(3));
    let r = load_file(vm, fname.as_deref(), mode.as_deref());
    load_result(vm, r, env)
}

fn dofile_cont(vm: &mut Vm, _s: KStatus, _ctx: isize) -> LuaResult<Action> {
    Ok(Action::Return(vm.top() - 1))
}

fn dofile(vm: &mut Vm) -> LuaResult<Action> {
    let fname = vm
        .opt_string(1)?
        .map(|s| vm.heap.string(s).as_bytes().to_vec());
    vm.set_top(1);
    let f = load_file(vm, fname.as_deref(), Some(b"bt"))?;
    vm.push(f);
    Ok(Action::Call {
        nargs: 0,
        nresults: None,
        k: dofile_cont,
        ctx: 0,
    })
}

fn assert(vm: &mut Vm) -> LuaResult<Action> {
    if vm.arg(1).is_truthy() {
        return Ok(Action::Return(vm.top()));
    }
    vm.check_any(1)?;
    // As `error(msg)`: a string message gets the caller's position.
    let msg = if vm.top() >= 2 {
        vm.arg(2)
    } else {
        vm.heap.str_value("assertion failed!")
    };
    if let Value::Str(s) = msg {
        let mut full = vm.where_(1);
        full.extend_from_slice(vm.heap.string(s).as_bytes());
        let v = vm.heap.str_value(full);
        return Err(vm.raise(v));
    }
    Err(vm.raise(msg))
}

fn select(vm: &mut Vm) -> LuaResult<Action> {
    let n = vm.top() as i64;
    if let Value::Str(s) = vm.arg(1)
        && vm.heap.string(s).as_bytes().first() == Some(&b'#')
    {
        vm.push(Value::Int(n - 1));
        return Ok(Action::Return(1));
    }
    let mut i = vm.check_integer(1)?;
    if i < 0 {
        i += n;
    } else if i > n {
        i = n;
    }
    if i < 1 {
        return Err(vm.arg_error(1, "index out of range"));
    }
    Ok(Action::Return((n - i) as usize))
}

fn finish_pcall(vm: &mut Vm, status: KStatus, extra: isize) -> LuaResult<Action> {
    match status {
        KStatus::Error(_) => {
            let e = vm.arg(vm.top());
            vm.push(Value::Bool(false));
            vm.push(e);
            Ok(Action::Return(2))
        }
        _ => Ok(Action::Return(vm.top() - extra as usize)),
    }
}

fn pcall(vm: &mut Vm) -> LuaResult<Action> {
    vm.check_any(1)?;
    vm.insert(1, Value::Bool(true));
    Ok(Action::PCall {
        nargs: vm.top() - 2,
        nresults: None,
        handler: 0,
        k: finish_pcall,
        ctx: 0,
    })
}

fn xpcall(vm: &mut Vm) -> LuaResult<Action> {
    let n = vm.top();
    vm.check_function(2)?;
    let f = vm.arg(1);
    vm.insert(3, f);
    vm.insert(3, Value::Bool(true));
    Ok(Action::PCall {
        nargs: n - 2,
        nresults: None,
        handler: 2,
        k: finish_pcall,
        ctx: 2,
    })
}

fn tostring(vm: &mut Vm) -> LuaResult<Action> {
    let v = vm.check_any(1)?;
    let s = vm.to_display(v)?;
    vm.push(Value::Str(s));
    Ok(Action::Return(1))
}

// ---- coroutines ----------------------------------------------------------------

fn get_co(vm: &mut Vm, arg: usize) -> LuaResult<orbit_core::value::ThreadRef> {
    match vm.arg(arg) {
        Value::Thread(t) => Ok(t),
        _ => Err(vm.type_arg_error(arg, "thread")),
    }
}

fn co_create(vm: &mut Vm) -> LuaResult<Action> {
    let f = vm.check_function(1)?;
    let co = vm.new_thread(f);
    vm.push(Value::Thread(co));
    Ok(Action::Return(1))
}

fn co_resume(vm: &mut Vm) -> LuaResult<Action> {
    let co = get_co(vm, 1)?;
    let nargs = vm.top() - 1;
    match vm.resume(co, nargs) {
        Ok(r) => {
            let n = r.count();
            vm.insert(vm.top() - n + 1, Value::Bool(true));
            Ok(Action::Return(n + 1))
        }
        Err(e) => {
            vm.push(Value::Bool(false));
            vm.push(e.value);
            Ok(Action::Return(2))
        }
    }
}

fn co_wrap_aux(vm: &mut Vm) -> LuaResult<Action> {
    let Value::Thread(co) = vm.upvalue(1) else {
        unreachable!("wrap closure without a coroutine")
    };
    let nargs = vm.top();
    match vm.resume(co, nargs) {
        Ok(r) => Ok(Action::Return(r.count())),
        Err(mut e) => {
            if vm.thread_failed(co)
                && let Some(e2) = vm.close_thread(co)
            {
                e = e2;
            }
            if let Value::Str(s) = e.value {
                let mut msg = vm.where_(1);
                msg.extend_from_slice(vm.heap.string(s).as_bytes());
                e.value = vm.heap.str_value(msg);
            }
            Err(e)
        }
    }
}

fn co_wrap(vm: &mut Vm) -> LuaResult<Action> {
    let f = vm.check_function(1)?;
    let co = vm.new_thread(f);
    let aux = vm.wrap_aux_fn();
    let c = vm.native_closure_of(aux, vec![Value::Thread(co)]);
    vm.push(c);
    Ok(Action::Return(1))
}

fn co_yield(vm: &mut Vm) -> LuaResult<Action> {
    Ok(Action::Yield {
        n: vm.top(),
        k: None,
    })
}

fn co_status(vm: &mut Vm) -> LuaResult<Action> {
    let co = get_co(vm, 1)?;
    let s = vm.thread_status(co).name();
    let v = vm.heap.str_value(s);
    vm.push(v);
    Ok(Action::Return(1))
}

fn co_isyieldable(vm: &mut Vm) -> LuaResult<Action> {
    let r = if vm.top() == 0 {
        vm.is_yieldable()
    } else {
        let co = get_co(vm, 1)?;
        vm.thread_is_yieldable(co)
    };
    vm.push(Value::Bool(r));
    Ok(Action::Return(1))
}

fn co_running(vm: &mut Vm) -> LuaResult<Action> {
    let co = vm.current_thread();
    let main = co == vm.main_thread();
    vm.push(Value::Thread(co));
    vm.push(Value::Bool(main));
    Ok(Action::Return(2))
}

fn co_close(vm: &mut Vm) -> LuaResult<Action> {
    let co = if vm.top() == 0 {
        vm.current_thread()
    } else {
        get_co(vm, 1)?
    };
    use crate::coro::CoroutineStatus as S;
    match vm.thread_status(co) {
        S::Dead | S::Suspended => match vm.close_thread(co) {
            None => {
                vm.push(Value::Bool(true));
                Ok(Action::Return(1))
            }
            Some(e) => {
                vm.push(Value::Bool(false));
                vm.push(e.value);
                Ok(Action::Return(2))
            }
        },
        S::Normal => Err(vm.error("cannot close a normal coroutine")),
        S::Running => {
            if co == vm.main_thread() {
                Err(vm.error("cannot close main thread"))
            } else {
                Err(vm.close_running())
            }
        }
    }
}

impl Vm {
    /// The `next` function registered by the basic library, for `pairs`.
    fn global_next(&mut self) -> Value {
        self.lib_fn("next", next)
    }

    fn ipairs_aux_fn(&mut self) -> Value {
        self.lib_fn("ipairs_aux", ipairs_aux)
    }

    fn wrap_aux_fn(&mut self) -> Value {
        self.lib_fn("wrap", co_wrap_aux)
    }

    /// A registered native function, registering it on first use.
    fn lib_fn(&mut self, name: &'static str, f: crate::NativeFn) -> Value {
        if let Some(i) = self
            .natives
            .iter()
            .position(|e| std::ptr::fn_addr_eq(e.f, f))
        {
            return Value::LightNative(orbit_core::NativeId(i as u32));
        }
        self.register(name, f)
    }

    /// Whether coroutine `co` stopped with an error.
    fn thread_failed(&self, co: orbit_core::value::ThreadRef) -> bool {
        co != self.cur && self.saved_status(co) == crate::state::CoStatus::Failed
    }

    /// `t[k]` with metamethods, from a native function (`lua_gettable`).
    pub fn index_value(&mut self, t: Value, k: Value) -> LuaResult<Value> {
        match self.finish_get(t, k, crate::debug::Operand::None)? {
            crate::meta::Get::Value(v) => Ok(v),
            crate::meta::Get::Call(tm, t) => {
                self.check_stack(3)?;
                self.push(tm);
                self.push(t);
                self.push(k);
                self.call(2, Some(1))?;
                Ok(self.pop())
            }
        }
    }

    /// Picks argument `arg` from a list of option names (`luaL_checkoption`).
    pub fn check_option(
        &mut self,
        arg: usize,
        default: Option<&str>,
        opts: &[&str],
    ) -> LuaResult<usize> {
        let name = match (self.arg(arg), default) {
            (Value::Nil, Some(d)) => d.as_bytes().to_vec(),
            _ => {
                let s = self.check_string(arg)?;
                self.heap.string(s).as_bytes().to_vec()
            }
        };
        match opts.iter().position(|o| o.as_bytes() == name.as_slice()) {
            Some(i) => Ok(i),
            None => {
                let n = String::from_utf8_lossy(&name).into_owned();
                Err(self.arg_error(arg, &format!("invalid option '{n}'")))
            }
        }
    }
}
