//! The parts of the debug library (`ldblib.c`) that need the interpreter's internals:
//! `getinfo`, `getlocal`, `setlocal`, `getupvalue`, `setupvalue`, `upvalueid`,
//! `upvaluejoin`, `sethook` and `gethook`. `orbit-stdlib` adds them to its `debug`
//! table through [`FUNCTIONS`].

use crate::error::LuaResult;
use crate::state::{Action, HookEvent, NativeFn, Vm, flag, hook_mask};
use orbit_bytecode::PF_VAHID;
use orbit_core::Value;
use orbit_core::value::{TableRef, ThreadRef};

/// The functions this module provides, by name.
pub const FUNCTIONS: &[(&str, NativeFn)] = &[
    ("getinfo", db_getinfo),
    ("getlocal", db_getlocal),
    ("setlocal", db_setlocal),
    ("getupvalue", db_getupvalue),
    ("setupvalue", db_setupvalue),
    ("upvalueid", db_upvalueid),
    ("upvaluejoin", db_upvaluejoin),
    ("sethook", db_sethook),
    ("gethook", db_gethook),
    ("debug", db_debug),
];

/// Registry key of the table mapping coroutines to their Lua hook functions.
const HOOKKEY: &[u8] = b"_HOOKKEY";

/// What `lua_getinfo` reports about a function or an active frame.
#[derive(Debug, Default)]
pub struct DebugInfo {
    /// The function.
    pub func: Value,
    /// `source`, as given to `load`.
    pub source: Vec<u8>,
    /// `short_src`.
    pub short_src: Vec<u8>,
    /// `linedefined`, or -1 for native functions.
    pub line_defined: i64,
    /// `lastlinedefined`, or -1 for native functions.
    pub last_line_defined: i64,
    /// `"Lua"`, `"C"` or `"main"`.
    pub what: &'static str,
    /// `currentline`, or -1.
    pub current_line: i64,
    /// Number of upvalues.
    pub nups: usize,
    /// Number of fixed parameters.
    pub nparams: usize,
    /// Whether the function is variadic.
    pub is_vararg: bool,
    /// How the function was called (`"global"`, `"method"`, ...) and the name used,
    /// if known.
    pub name: Option<(&'static str, String)>,
    /// Whether the frame was entered by a tail call.
    pub is_tail_call: bool,
    /// Number of `__call` metamethods the call went through.
    pub extra_args: u8,
    /// First value transferred by the hooked call or return, and their number.
    pub transfer: (usize, usize),
    /// Lines with code, for Lua functions.
    pub active_lines: Option<Vec<u32>>,
}

impl Vm {
    /// The frame index of stack level `level` (0 is the running function), if any
    /// (`lua_getstack`).
    pub(crate) fn level_frame(&self, level: i64) -> Option<usize> {
        let n = self.st.frames.len() as i64;
        (0..n).contains(&level).then(|| (n - 1 - level) as usize)
    }

    /// Fills the function part of a [`DebugInfo`] (`funcinfo` and the `u` option).
    fn func_info(&self, f: Value, info: &mut DebugInfo) {
        info.func = f;
        match f {
            Value::LuaFn(cl) => {
                let c = self.heap.get(cl);
                let p = self.heap.get(c.proto);
                info.source = p.source.map_or_else(
                    || b"=?".to_vec(),
                    |s| self.heap.string(s).as_bytes().to_vec(),
                );
                info.line_defined = i64::from(p.info.line_defined);
                info.last_line_defined = i64::from(p.info.last_line_defined);
                info.what = if p.info.line_defined == 0 {
                    "main"
                } else {
                    "Lua"
                };
                info.nups = c.upvals.len();
                info.nparams = usize::from(p.info.num_params);
                info.is_vararg = p.info.is_vararg();
            }
            _ => {
                info.source = b"=[C]".to_vec();
                info.line_defined = -1;
                info.last_line_defined = -1;
                info.what = "C";
                info.nups = match f {
                    Value::NativeFn(c) => self.heap.get(c).upvals.len(),
                    _ => 0,
                };
                info.is_vararg = true;
            }
        }
        info.short_src = orbit_lua_syntax::chunk_id(&info.source);
    }

    /// Lines of `f` that have code (`collectvalidlines`).
    fn active_lines(&self, f: Value) -> Option<Vec<u32>> {
        let Value::LuaFn(cl) = f else { return None };
        let p = self.heap.get(self.heap.get(cl).proto);
        // A vararg function's first instruction (VARARGPREP) is not a line of its own.
        let skip = usize::from(p.info.is_vararg());
        Some(p.info.lines.iter().skip(skip).copied().collect())
    }

    /// Information about function `f` (`lua_getinfo` with `>`).
    pub fn function_info(&self, f: Value) -> DebugInfo {
        let mut info = DebugInfo {
            current_line: -1,
            ..DebugInfo::default()
        };
        self.func_info(f, &mut info);
        info.active_lines = self.active_lines(f);
        info
    }

    /// Information about the function at stack level `level` of the running
    /// coroutine, or `None` if there is no such level.
    pub fn level_info(&self, level: i64) -> Option<DebugInfo> {
        let fi = self.level_frame(level)?;
        let fr = self.st.frames[fi];
        let f = self.st.stack[fr.func];
        let mut info = DebugInfo::default();
        self.func_info(f, &mut info);
        info.current_line = if fr.is_lua() {
            self.frame_line(&fr)
        } else {
            -1
        };
        info.name = self.func_name_for_frame(fi);
        info.is_tail_call = fr.flags & flag::TAIL != 0;
        info.extra_args = fr.extra_args;
        if fr.flags & flag::HOOKED != 0 {
            info.transfer = self.st.hook.transfer;
        }
        info.active_lines = self.active_lines(f);
        Some(info)
    }

    /// The name and stack slot of local `n` of frame `fi` (`luaG_findlocal`).
    fn find_local(&self, fi: usize, n: i64) -> Option<(String, usize)> {
        let fr = self.st.frames[fi];
        let base = fr.func + 1;
        let mut name = None;
        if fr.is_lua() {
            let p = self.frame_proto(&fr)?;
            if n < 0 {
                // Extra arguments of a vararg function.
                let nextra = fr.nextraargs as i64;
                if p.info.flags & PF_VAHID != 0 && n >= -nextra {
                    let pos = (fr.func as i64 - nextra - (n + 1)) as usize;
                    return Some(("(vararg)".to_string(), pos));
                }
                return None;
            }
            let pc = fr.pc.saturating_sub(1) as u32;
            name = u32::try_from(n)
                .ok()
                .and_then(|n| p.info.local_name(n, pc))
                .map(str::to_string);
        }
        let name = match name {
            Some(name) => name,
            None => {
                let limit = if fi + 1 == self.st.frames.len() {
                    self.st.top
                } else {
                    self.st.frames[fi + 1].func
                };
                if n > 0 && (limit as i64 - base as i64) >= n {
                    if fr.is_lua() {
                        "(temporary)".to_string()
                    } else {
                        "(C temporary)".to_string()
                    }
                } else {
                    return None;
                }
            }
        };
        Some((name, base + n as usize - 1))
    }

    /// Local variable `n` of the function at stack level `level`: its name and value
    /// (`lua_getlocal`). `Err(())` means there is no such level.
    #[allow(clippy::result_unit_err)]
    pub fn get_local(&self, level: i64, n: i64) -> Result<Option<(String, Value)>, ()> {
        let fi = self.level_frame(level).ok_or(())?;
        Ok(self
            .find_local(fi, n)
            .map(|(name, pos)| (name, self.st.stack[pos])))
    }

    /// Sets local variable `n` of the function at stack level `level`, returning its
    /// name (`lua_setlocal`). `Err(())` means there is no such level.
    #[allow(clippy::result_unit_err)]
    pub fn set_local(&mut self, level: i64, n: i64, v: Value) -> Result<Option<String>, ()> {
        let fi = self.level_frame(level).ok_or(())?;
        Ok(self.find_local(fi, n).map(|(name, pos)| {
            self.st.stack[pos] = v;
            name
        }))
    }

    /// Upvalue `n` (1-based) of function `f`: its name (`""` for native closures) and
    /// value (`lua_getupvalue`).
    pub fn get_upvalue(&self, f: Value, n: i64) -> Option<(String, Value)> {
        let i = usize::try_from(n).ok()?.checked_sub(1)?;
        match f {
            Value::LuaFn(cl) => {
                let c = self.heap.get(cl);
                let uv = *c.upvals.get(i)?;
                let p = self.heap.get(c.proto);
                let name = match p.info.upvalues.get(i) {
                    Some(d) if !d.name.is_empty() => d.name.to_string(),
                    _ => "(no name)".to_string(),
                };
                Some((name, self.upval_get(uv)))
            }
            Value::NativeFn(c) => {
                let v = *self.heap.get(c).upvals.get(i)?;
                Some((String::new(), v))
            }
            _ => None,
        }
    }

    /// Sets upvalue `n` (1-based) of function `f`, returning its name
    /// (`lua_setupvalue`).
    pub fn set_upvalue_of(&mut self, f: Value, n: i64, v: Value) -> Option<String> {
        let (name, _) = self.get_upvalue(f, n)?;
        let i = n as usize - 1;
        match f {
            Value::LuaFn(cl) => {
                let uv = self.heap.get(cl).upvals[i];
                self.upval_set(uv, v);
            }
            Value::NativeFn(c) => self.heap.update(c, |c| c.upvals[i] = v),
            _ => unreachable!(),
        }
        Some(name)
    }

    /// An identifier for upvalue `n` of function `f`, equal for closures sharing it
    /// (`lua_upvalueid`).
    pub fn upvalue_id(&self, f: Value, n: i64) -> Option<u64> {
        let i = usize::try_from(n).ok()?.checked_sub(1)?;
        match f {
            Value::LuaFn(cl) => {
                let uv = *self.heap.get(cl).upvals.get(i)?;
                Some(0x5500_0000_0000 | (8 << 36) | (u64::from(uv.index()) << 4))
            }
            Value::NativeFn(c) => {
                self.heap.get(c).upvals.get(i)?;
                Some(0x5600_0000_0000 | (u64::from(c.index()) << 8) | i as u64)
            }
            _ => None,
        }
    }

    /// Makes upvalue `n1` of Lua function `f1` refer to upvalue `n2` of Lua function
    /// `f2` (`lua_upvaluejoin`). Both must exist.
    pub fn upvalue_join(&mut self, f1: Value, n1: i64, f2: Value, n2: i64) {
        let (Value::LuaFn(c1), Value::LuaFn(c2)) = (f1, f2) else {
            panic!("Lua functions expected")
        };
        let uv = self.heap.get(c2).upvals[n2 as usize - 1];
        self.heap.update(c1, |c| c.upvals[n1 as usize - 1] = uv);
    }
}

/// The optional thread argument of most debug functions (`getthread`): the thread
/// and the offset of the remaining arguments.
fn get_thread(vm: &Vm) -> (ThreadRef, usize) {
    match vm.arg(1) {
        Value::Thread(t) => (t, 1),
        _ => (vm.current_thread(), 0),
    }
}

fn set_field(vm: &mut Vm, t: TableRef, k: &str, v: Value) {
    vm.heap
        .raw_set_name(t, k.as_bytes(), v)
        .expect("string key");
}

fn db_getinfo(vm: &mut Vm) -> LuaResult<Action> {
    let (co, arg) = get_thread(vm);
    let options = match vm.opt_string(arg + 2)? {
        Some(s) => vm.heap.string(s).as_bytes().to_vec(),
        None => b"flnSrtu".to_vec(),
    };
    if options.first() == Some(&b'>') {
        return Err(vm.arg_error(arg + 2, "invalid option '>'"));
    }
    let target = vm.arg(arg + 1);
    let info = if target.is_function() {
        vm.function_info(target)
    } else {
        let level = vm.check_integer(arg + 1)?;
        match vm.with_thread(co, |vm| vm.level_info(level)) {
            Some(info) => info,
            None => {
                vm.push(Value::Nil);
                return Ok(Action::Return(1));
            }
        }
    };
    if !options.iter().all(|c| b"SlutnrLf".contains(c)) {
        return Err(vm.arg_error(arg + 2, "invalid option"));
    }
    let t = vm.heap.new_table();
    vm.push(Value::Table(t));
    let has = |c: u8| options.contains(&c);
    if has(b'S') {
        let v = vm.heap.str_value(&info.source);
        set_field(vm, t, "source", v);
        let v = vm.heap.str_value(&info.short_src);
        set_field(vm, t, "short_src", v);
        set_field(vm, t, "linedefined", Value::Int(info.line_defined));
        set_field(vm, t, "lastlinedefined", Value::Int(info.last_line_defined));
        let v = vm.heap.str_value(info.what);
        set_field(vm, t, "what", v);
    }
    if has(b'l') {
        set_field(vm, t, "currentline", Value::Int(info.current_line));
    }
    if has(b'u') {
        set_field(vm, t, "nups", Value::Int(info.nups as i64));
        set_field(vm, t, "nparams", Value::Int(info.nparams as i64));
        set_field(vm, t, "isvararg", Value::Bool(info.is_vararg));
    }
    if has(b'n') {
        let (namewhat, name) = match &info.name {
            Some((kind, name)) => (*kind, vm.heap.str_value(name)),
            None => ("", Value::Nil),
        };
        set_field(vm, t, "name", name);
        let v = vm.heap.str_value(namewhat);
        set_field(vm, t, "namewhat", v);
    }
    if has(b'r') {
        set_field(vm, t, "ftransfer", Value::Int(info.transfer.0 as i64));
        set_field(vm, t, "ntransfer", Value::Int(info.transfer.1 as i64));
    }
    if has(b't') {
        set_field(vm, t, "istailcall", Value::Bool(info.is_tail_call));
        set_field(vm, t, "extraargs", Value::Int(i64::from(info.extra_args)));
    }
    if has(b'L')
        && let Some(lines) = &info.active_lines
    {
        let lt = vm.heap.new_table();
        set_field(vm, t, "activelines", Value::Table(lt));
        for &l in lines {
            let _ = vm.heap.raw_set_int(lt, i64::from(l), Value::Bool(true));
        }
    }
    if has(b'f') {
        set_field(vm, t, "func", info.func);
    }
    Ok(Action::Return(1))
}

fn db_getlocal(vm: &mut Vm) -> LuaResult<Action> {
    let (co, arg) = get_thread(vm);
    let nvar = vm.check_integer(arg + 2)?;
    let f = vm.arg(arg + 1);
    if f.is_function() {
        // Parameter names of a function that is not running.
        let name = match f {
            Value::LuaFn(cl) => {
                let p = vm.heap.get(vm.heap.get(cl).proto);
                u32::try_from(nvar)
                    .ok()
                    .and_then(|n| p.info.local_name(n, 0))
                    .map(str::to_string)
            }
            _ => None,
        };
        let v = name.map_or(Value::Nil, |n| vm.heap.str_value(n));
        vm.push(v);
        return Ok(Action::Return(1));
    }
    let level = vm.check_integer(arg + 1)?;
    match vm.with_thread(co, |vm| vm.get_local(level, nvar)) {
        Err(()) => Err(vm.arg_error(arg + 1, "level out of range")),
        Ok(Some((name, v))) => {
            let name = vm.heap.str_value(name);
            vm.push(name);
            vm.push(v);
            Ok(Action::Return(2))
        }
        Ok(None) => {
            vm.push(Value::Nil);
            Ok(Action::Return(1))
        }
    }
}

fn db_setlocal(vm: &mut Vm) -> LuaResult<Action> {
    let (co, arg) = get_thread(vm);
    let level = vm.check_integer(arg + 1)?;
    let nvar = vm.check_integer(arg + 2)?;
    if vm.with_thread(co, |vm| vm.level_frame(level)).is_none() {
        return Err(vm.arg_error(arg + 1, "level out of range"));
    }
    let v = vm.check_any(arg + 3)?;
    let name = vm
        .with_thread(co, |vm| vm.set_local(level, nvar, v))
        .ok()
        .flatten();
    let name = name.map_or(Value::Nil, |n| vm.heap.str_value(n));
    vm.push(name);
    Ok(Action::Return(1))
}

fn db_getupvalue(vm: &mut Vm) -> LuaResult<Action> {
    let n = vm.check_integer(2)?;
    let f = vm.check_function(1)?;
    match vm.get_upvalue(f, n) {
        Some((name, v)) => {
            let name = vm.heap.str_value(name);
            vm.push(name);
            vm.push(v);
            Ok(Action::Return(2))
        }
        None => Ok(Action::Return(0)),
    }
}

fn db_setupvalue(vm: &mut Vm) -> LuaResult<Action> {
    let v = vm.check_any(3)?;
    let n = vm.check_integer(2)?;
    let f = vm.check_function(1)?;
    match vm.set_upvalue_of(f, n, v) {
        Some(name) => {
            let name = vm.heap.str_value(name);
            vm.push(name);
            Ok(Action::Return(1))
        }
        None => Ok(Action::Return(0)),
    }
}

/// `checkupval`: the id of upvalue `argnup` of function `argf`.
fn check_upval(vm: &mut Vm, argf: usize, argnup: usize) -> LuaResult<(Value, i64, Option<u64>)> {
    let n = vm.check_integer(argnup)?;
    let f = vm.check_function(argf)?;
    Ok((f, n, vm.upvalue_id(f, n)))
}

fn db_upvalueid(vm: &mut Vm) -> LuaResult<Action> {
    let (_, _, id) = check_upval(vm, 1, 2)?;
    vm.push(id.map_or(Value::Nil, Value::LightUserData));
    Ok(Action::Return(1))
}

fn db_upvaluejoin(vm: &mut Vm) -> LuaResult<Action> {
    let (f1, n1, id1) = check_upval(vm, 1, 2)?;
    if id1.is_none() {
        return Err(vm.arg_error(2, "invalid upvalue index"));
    }
    let (f2, n2, id2) = check_upval(vm, 3, 4)?;
    if id2.is_none() {
        return Err(vm.arg_error(4, "invalid upvalue index"));
    }
    if !matches!(f1, Value::LuaFn(_)) {
        return Err(vm.arg_error(1, "Lua function expected"));
    }
    if !matches!(f2, Value::LuaFn(_)) {
        return Err(vm.arg_error(3, "Lua function expected"));
    }
    vm.upvalue_join(f1, n1, f2, n2);
    Ok(Action::Return(0))
}

/// The hook `debug.sethook` installs: calls the Lua function registered for the
/// running coroutine with the event name and line (`hookf`).
fn hookf(vm: &mut Vm, event: HookEvent, line: i64) -> LuaResult<()> {
    let Value::Table(ht) = vm.heap.raw_get_name(vm.registry(), HOOKKEY) else {
        return Ok(());
    };
    let f = vm.heap.raw_get(ht, Value::Thread(vm.current_thread()));
    if !f.is_function() {
        return Ok(());
    }
    vm.push(f);
    let name = vm.heap.str_value(event.name());
    vm.push(name);
    vm.push(if line >= 0 {
        Value::Int(line)
    } else {
        Value::Nil
    });
    vm.call(2, Some(0))
}

fn db_sethook(vm: &mut Vm) -> LuaResult<Action> {
    let (co, arg) = get_thread(vm);
    let (func, mask, count) = if vm.arg(arg + 1).is_nil() {
        vm.set_top(arg + 1);
        (None, 0, 0)
    } else {
        let smask = vm.check_string(arg + 2)?;
        vm.check_function(arg + 1)?;
        let count = vm.opt_integer(arg + 3, 0)?;
        let smask = vm.heap.string(smask).as_bytes();
        let mut mask = 0;
        for (c, bit) in [
            (b'c', hook_mask::CALL),
            (b'r', hook_mask::RET),
            (b'l', hook_mask::LINE),
        ] {
            if smask.contains(&c) {
                mask |= bit;
            }
        }
        if count > 0 {
            mask |= hook_mask::COUNT;
        }
        let count = u32::try_from(count.max(0)).unwrap_or(u32::MAX);
        (Some(hookf as crate::state::HookFn), mask, count)
    };
    let ht = match vm.heap.raw_get_name(vm.registry(), HOOKKEY) {
        Value::Table(t) => t,
        _ => {
            let t = vm.heap.new_table();
            let k = vm.heap.str_value("k");
            set_field(vm, t, "__mode", k);
            vm.heap.set_table_metatable(t, Some(t));
            let reg = vm.registry();
            set_field(vm, reg, "_HOOKKEY", Value::Table(t));
            t
        }
    };
    let f = vm.arg(arg + 1);
    vm.heap
        .raw_set(ht, Value::Thread(co), f)
        .expect("thread key");
    vm.set_thread_hook(co, func, mask, count);
    Ok(Action::Return(0))
}

fn db_gethook(vm: &mut Vm) -> LuaResult<Action> {
    let (co, _) = get_thread(vm);
    let (hook, mask, count) = vm.thread_hook(co);
    let Some(hook) = hook else {
        vm.push(Value::Nil);
        return Ok(Action::Return(1));
    };
    if std::ptr::fn_addr_eq(hook, hookf as crate::state::HookFn) {
        let f = match vm.heap.raw_get_name(vm.registry(), HOOKKEY) {
            Value::Table(ht) => vm.heap.raw_get(ht, Value::Thread(co)),
            _ => Value::Nil,
        };
        vm.push(f);
    } else {
        let v = vm.heap.str_value("external hook");
        vm.push(v);
    }
    let mut smask = String::new();
    for (c, bit) in [
        ('c', hook_mask::CALL),
        ('r', hook_mask::RET),
        ('l', hook_mask::LINE),
    ] {
        if mask & bit != 0 {
            smask.push(c);
        }
    }
    let smask = vm.heap.str_value(smask);
    vm.push(smask);
    vm.push(Value::Int(i64::from(count)));
    Ok(Action::Return(3))
}

/// `debug.debug`: runs each line read from standard input until one reads `cont`,
/// printing errors to standard error.
fn db_debug(vm: &mut Vm) -> LuaResult<Action> {
    use std::io::{BufRead, Write};
    loop {
        let mut e = std::io::stderr().lock();
        let _ = e.write_all(b"lua_debug> ");
        let _ = e.flush();
        drop(e);
        let mut line = Vec::new();
        match std::io::stdin().lock().read_until(b'\n', &mut line) {
            Ok(0) | Err(_) => return Ok(Action::Return(0)),
            Ok(_) if line == b"cont\n" => return Ok(Action::Return(0)),
            Ok(_) => {}
        }
        let base = vm.top();
        let r = vm.load(&line, b"=(debug command)").and_then(|f| {
            vm.push(f);
            vm.pcall(0, Some(0), 0)
        });
        if let Err(err) = r {
            let msg = match vm.to_display(err.value) {
                Ok(s) => vm.heap.string(s).as_bytes().to_vec(),
                Err(_) => b"?".to_vec(),
            };
            let mut e = std::io::stderr().lock();
            let _ = e.write_all(&msg);
            let _ = e.write_all(b"\n");
            let _ = e.flush();
        }
        vm.set_top(base);
    }
}
