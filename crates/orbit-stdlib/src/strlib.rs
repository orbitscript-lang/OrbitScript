//! The string library (`lstrlib.c`) on top of the pure [`crate::pattern`],
//! [`crate::format`] and [`crate::pack`] modules.

use std::borrow::Cow;

use orbit_core::Value;
use orbit_core::value::{StrRef, Type};
use orbit_vm::number::{self, Arith};
use orbit_vm::{Action, LuaResult, Vm, value_addr};

use crate::format::{FormatArgs, Literal};
use crate::pack::{PackArgs, Unpacked};
use crate::pattern::{self, Capture, Captures, GMatch, GsubMatch, Replacement};
use crate::{AnyError, LibError, raise};

/// `MAX_SIZE`: the largest string length.
const MAX_SIZE: usize = i64::MAX as usize;

/// Opens the string library and sets the metatable of strings.
pub fn open_string(vm: &mut Vm) {
    let gmatch_aux = vm.register("gmatch_aux", gmatch_aux);
    let t = vm.new_lib(
        "string",
        &[
            ("byte", str_byte),
            ("char", str_char),
            ("dump", str_dump),
            ("find", str_find),
            ("format", str_format),
            ("gsub", str_gsub),
            ("len", str_len),
            ("lower", str_lower),
            ("match", str_match),
            ("rep", str_rep),
            ("reverse", str_reverse),
            ("sub", str_sub),
            ("upper", str_upper),
            ("pack", str_pack),
            ("packsize", str_packsize),
            ("unpack", str_unpack),
        ],
    );
    // gmatch keeps its iterator function as an upvalue.
    let gmatch = vm.native_closure("gmatch", gmatch, vec![gmatch_aux]);
    vm.heap
        .raw_set_name(t, b"gmatch", gmatch)
        .expect("string key");

    let mt = vm.heap.new_table();
    let arith: [(&'static str, orbit_vm::NativeFn); 8] = [
        ("__add", arith_add),
        ("__sub", arith_sub),
        ("__mul", arith_mul),
        ("__mod", arith_mod),
        ("__pow", arith_pow),
        ("__div", arith_div),
        ("__idiv", arith_idiv),
        ("__unm", arith_unm),
    ];
    for (name, f) in arith {
        let v = vm.register(name, f);
        vm.heap
            .raw_set_name(mt, name.as_bytes(), v)
            .expect("string key");
    }
    vm.heap
        .raw_set_name(mt, b"__index", Value::Table(t))
        .expect("string key");
    vm.heap.set_type_metatable(Type::String, Some(mt));
}

fn bytes(vm: &Vm, s: StrRef) -> &[u8] {
    vm.heap.string(s).as_bytes()
}

fn ret_str(vm: &mut Vm, b: impl AsRef<[u8]>) -> LuaResult<Action> {
    let v = vm.heap.str_value(b);
    vm.push(v);
    Ok(Action::Return(1))
}

/// `posrelatI`
fn posrelat(pos: i64, len: usize) -> usize {
    pattern::posrelat_start(pos, len)
}

/// `getendpos`
fn endpos(vm: &mut Vm, arg: usize, def: i64, len: usize) -> LuaResult<usize> {
    let pos = vm.opt_integer(arg, def)?;
    Ok(if pos > len as i64 {
        len
    } else if pos >= 0 {
        pos as usize
    } else if pos.unsigned_abs() > len as u64 {
        0
    } else {
        len - pos.unsigned_abs() as usize + 1
    })
}

fn str_len(vm: &mut Vm) -> LuaResult<Action> {
    let s = vm.check_string(1)?;
    let n = bytes(vm, s).len() as i64;
    vm.push(Value::Int(n));
    Ok(Action::Return(1))
}

fn str_sub(vm: &mut Vm) -> LuaResult<Action> {
    let s = vm.check_string(1)?;
    let l = bytes(vm, s).len();
    let start = posrelat(vm.check_integer(2)?, l);
    let end = endpos(vm, 3, -1, l)?;
    if start <= end {
        if start == 1 && end == l {
            vm.push(Value::Str(s));
            return Ok(Action::Return(1));
        }
        let sub = bytes(vm, s)[start - 1..end].to_vec();
        ret_str(vm, sub)
    } else {
        ret_str(vm, b"")
    }
}

fn str_reverse(vm: &mut Vm) -> LuaResult<Action> {
    let s = vm.check_string(1)?;
    let mut b = bytes(vm, s).to_vec();
    b.reverse();
    ret_str(vm, b)
}

fn str_lower(vm: &mut Vm) -> LuaResult<Action> {
    let s = vm.check_string(1)?;
    let b = bytes(vm, s).to_ascii_lowercase();
    ret_str(vm, b)
}

fn str_upper(vm: &mut Vm) -> LuaResult<Action> {
    let s = vm.check_string(1)?;
    let b = bytes(vm, s).to_ascii_uppercase();
    ret_str(vm, b)
}

fn str_rep(vm: &mut Vm) -> LuaResult<Action> {
    let s = vm.check_string(1)?;
    let n = vm.check_integer(2)?;
    let sep = vm.opt_string(3)?;
    let len = bytes(vm, s).len();
    let lsep = sep.map_or(0, |p| bytes(vm, p).len());
    if n <= 0 || (len | lsep) == 0 {
        return ret_str(vm, b"");
    }
    if len > MAX_SIZE - lsep || (len + lsep) as i64 > i64::MAX / n {
        return Err(vm.error("resulting string too large"));
    }
    let total = n as usize * (len + lsep) - lsep;
    let mut out = Vec::new();
    if out.try_reserve_exact(total).is_err() {
        return Err(vm.error("not enough memory"));
    }
    let sb = bytes(vm, s);
    let sepb: &[u8] = sep.map_or(&[], |p| vm.heap.string(p).as_bytes());
    for i in 0..n {
        out.extend_from_slice(sb);
        if i + 1 < n {
            out.extend_from_slice(sepb);
        }
    }
    ret_str(vm, out)
}

fn str_byte(vm: &mut Vm) -> LuaResult<Action> {
    let s = vm.check_string(1)?;
    let l = bytes(vm, s).len();
    let pi = vm.opt_integer(2, 1)?;
    let posi = posrelat(pi, l);
    let pose = endpos(vm, 3, pi, l)?;
    if posi > pose {
        return Ok(Action::Return(0));
    }
    if pose - posi >= i32::MAX as usize {
        return Err(vm.error("string slice too long"));
    }
    let n = pose - posi + 1;
    if vm.check_stack(n).is_err() {
        return Err(vm.error("stack overflow (string slice too long)"));
    }
    for i in 0..n {
        let c = bytes(vm, s)[posi + i - 1];
        vm.push(Value::Int(i64::from(c)));
    }
    Ok(Action::Return(n))
}

fn str_char(vm: &mut Vm) -> LuaResult<Action> {
    let n = vm.top();
    let mut out = Vec::with_capacity(n);
    for i in 1..=n {
        let c = vm.check_integer(i)? as u64;
        if c > 255 {
            return Err(vm.arg_error(i, "value out of range"));
        }
        out.push(c as u8);
    }
    ret_str(vm, out)
}

fn str_dump(vm: &mut Vm) -> LuaResult<Action> {
    let f = vm.arg(1);
    if !matches!(f, Value::LuaFn(_)) {
        return Err(vm.arg_error(1, "Lua function expected"));
    }
    let strip = vm.arg(2).is_truthy();
    match vm.dump(f, strip) {
        Some(b) => ret_str(vm, b),
        None => Err(vm.error("unable to dump given function")),
    }
}

// ---- metamethods -------------------------------------------------------------

/// `tonum`: a number, or a string that converts to one.
fn tonum(vm: &Vm, v: Value) -> Option<Value> {
    match v {
        Value::Int(_) | Value::Float(_) => Some(v),
        Value::Str(s) => number::str_to_number(bytes(vm, s)),
        _ => None,
    }
}

fn arith(vm: &mut Vm, op: Arith, mtname: &'static str) -> LuaResult<Action> {
    let (a, b) = (vm.arg(1), vm.arg(2));
    if let (Some(x), Some(y)) = (tonum(vm, a), tonum(vm, b)) {
        let (x, y) = if op == Arith::Unm { (x, x) } else { (x, y) };
        let v = match number::raw_arith(op, x, y) {
            Some(Ok(v)) => v,
            Some(Err(e)) => return Err(vm.rt_error(e.message())),
            None => unreachable!("numbers"),
        };
        vm.push(v);
        return Ok(Action::Return(1));
    }
    // trymt
    let tm = if matches!(b, Value::Str(_)) {
        Value::Nil
    } else {
        vm.meta_field(b, mtname.as_bytes())
    };
    if tm.is_nil() {
        let (t1, t2) = (a.type_name(), b.type_name());
        let op = &mtname[2..];
        return Err(vm.error(format!("attempt to {op} a '{t1}' with a '{t2}'")));
    }
    let r = vm.call_value(tm, &[a, b])?;
    vm.push(r);
    Ok(Action::Return(1))
}

fn arith_add(vm: &mut Vm) -> LuaResult<Action> {
    arith(vm, Arith::Add, "__add")
}
fn arith_sub(vm: &mut Vm) -> LuaResult<Action> {
    arith(vm, Arith::Sub, "__sub")
}
fn arith_mul(vm: &mut Vm) -> LuaResult<Action> {
    arith(vm, Arith::Mul, "__mul")
}
fn arith_mod(vm: &mut Vm) -> LuaResult<Action> {
    arith(vm, Arith::Mod, "__mod")
}
fn arith_pow(vm: &mut Vm) -> LuaResult<Action> {
    arith(vm, Arith::Pow, "__pow")
}
fn arith_div(vm: &mut Vm) -> LuaResult<Action> {
    arith(vm, Arith::Div, "__div")
}
fn arith_idiv(vm: &mut Vm) -> LuaResult<Action> {
    arith(vm, Arith::IDiv, "__idiv")
}
fn arith_unm(vm: &mut Vm) -> LuaResult<Action> {
    arith(vm, Arith::Unm, "__unm")
}

// ---- pattern matching --------------------------------------------------------

fn push_capture(vm: &mut Vm, s: StrRef, c: &Capture) {
    let v = match *c {
        Capture::Str { start, end } => {
            let b = bytes(vm, s)[start..end].to_vec();
            vm.heap.str_value(b)
        }
        Capture::Position(p) => Value::Int(p),
    };
    vm.push(v);
}

fn push_captures(vm: &mut Vm, s: StrRef, caps: &Captures) -> LuaResult<usize> {
    if vm.check_stack(caps.len()).is_err() {
        return Err(vm.error("stack overflow (too many captures)"));
    }
    for c in caps.as_slice() {
        push_capture(vm, s, c);
    }
    Ok(caps.len())
}

fn find_aux(vm: &mut Vm, find: bool) -> LuaResult<Action> {
    let s = vm.check_string(1)?;
    let p = vm.check_string(2)?;
    let init = vm.opt_integer(3, 1)?;
    let r = if find {
        let plain = vm.arg(4).is_truthy();
        pattern::find(bytes(vm, s), bytes(vm, p), init, plain)
            .map(|f| f.map(|f| (Some((f.start as i64 + 1, f.end as i64)), f.captures)))
    } else {
        pattern::match_(bytes(vm, s), bytes(vm, p), init).map(|c| c.map(|c| (None, c)))
    };
    match r {
        Err(e) => Err(raise(vm, e)),
        Ok(None) => {
            vm.push(Value::Nil);
            Ok(Action::Return(1))
        }
        Ok(Some((pos, caps))) => {
            let mut n = 0;
            if let Some((a, b)) = pos {
                vm.push(Value::Int(a));
                vm.push(Value::Int(b));
                n = 2;
            }
            n += push_captures(vm, s, &caps)?;
            Ok(Action::Return(n))
        }
    }
}

fn str_find(vm: &mut Vm) -> LuaResult<Action> {
    find_aux(vm, true)
}

fn str_match(vm: &mut Vm) -> LuaResult<Action> {
    find_aux(vm, false)
}

fn gmatch(vm: &mut Vm) -> LuaResult<Action> {
    let s = vm.check_string(1)?;
    let p = vm.check_string(2)?;
    let init = vm.opt_integer(3, 1)?;
    let (src, _) = GMatch::new(bytes(vm, s).len(), init).state();
    let aux = vm.upvalue(1);
    let f = vm.native_closure_of(
        aux,
        vec![
            Value::Str(s),
            Value::Str(p),
            Value::Int(src as i64),
            Value::Nil,
        ],
    );
    vm.push(f);
    Ok(Action::Return(1))
}

fn gmatch_aux(vm: &mut Vm) -> LuaResult<Action> {
    let (Value::Str(s), Value::Str(p), Value::Int(src)) =
        (vm.upvalue(1), vm.upvalue(2), vm.upvalue(3))
    else {
        unreachable!("gmatch state")
    };
    let last = match vm.upvalue(4) {
        Value::Int(e) => Some(e as usize),
        _ => None,
    };
    let mut g = GMatch::from_state(src as usize, last);
    let r = g.next(bytes(vm, s), bytes(vm, p));
    let (src, last) = g.state();
    vm.set_upvalue(3, Value::Int(src as i64));
    vm.set_upvalue(4, last.map_or(Value::Nil, |e| Value::Int(e as i64)));
    match r {
        Err(e) => Err(raise(vm, e)),
        Ok(None) => Ok(Action::Return(0)),
        Ok(Some(caps)) => {
            let n = push_captures(vm, s, &caps)?;
            Ok(Action::Return(n))
        }
    }
}

fn str_gsub(vm: &mut Vm) -> LuaResult<Action> {
    let s = vm.check_string(1)?;
    let p = vm.check_string(2)?;
    let repl = vm.arg(3);
    let src_len = bytes(vm, s).len();
    let max = Some(vm.opt_integer(4, src_len as i64 + 1)?);
    let r = match repl {
        Value::Str(_) | Value::Int(_) | Value::Float(_) => {
            let r = vm.check_string(3)?;
            pattern::gsub::<LibError>(
                bytes(vm, s),
                bytes(vm, p),
                Replacement::Str(bytes(vm, r)),
                max,
            )
            .map_err(AnyError::Lib)
        }
        Value::Table(_) => gsub_call(vm, s, p, repl, max, false),
        v if v.is_function() => gsub_call(vm, s, p, repl, max, true),
        _ => {
            return Err(vm.type_arg_error(3, "string/function/table"));
        }
    };
    match r {
        Err(e) => Err(e.into_error(vm)),
        Ok((out, n)) => {
            let v = match out {
                None => Value::Str(s),
                Some(b) => vm.heap.str_value(b),
            };
            vm.push(v);
            vm.push(Value::Int(n));
            Ok(Action::Return(2))
        }
    }
}

/// `gsub` with a function or table replacement (`add_value`).
fn gsub_call(
    vm: &mut Vm,
    s: StrRef,
    p: StrRef,
    repl: Value,
    max: Option<i64>,
    is_fn: bool,
) -> Result<(Option<Vec<u8>>, i64), AnyError> {
    // The callback needs the VM, so the matcher works on copies.
    let src = bytes(vm, s).to_vec();
    let pat = bytes(vm, p).to_vec();
    let mut f = |m: &GsubMatch<'_, '_>, out: &mut Vec<u8>| -> Result<bool, AnyError> {
        let v = if is_fn {
            let caps = m.captures()?;
            vm.check_stack(caps.len() + 1)?;
            vm.push(repl);
            for c in caps.as_slice() {
                push_capture(vm, s, c);
            }
            vm.call(caps.len(), Some(1))?;
            vm.pop()
        } else {
            let c = m.first()?;
            push_capture(vm, s, &c);
            let k = vm.pop();
            vm.index_value(repl, k)?
        };
        match v {
            Value::Nil | Value::Bool(false) => Ok(false),
            Value::Str(r) => {
                out.extend_from_slice(bytes(vm, r));
                Ok(true)
            }
            Value::Int(_) | Value::Float(_) => {
                out.extend_from_slice(number::fmt_number(v).as_bytes());
                Ok(true)
            }
            v => {
                let t = vm.type_name_of(v);
                Err(vm
                    .error(format!("invalid replacement value (a {t})"))
                    .into())
            }
        }
    };
    pattern::gsub(&src, &pat, Replacement::Call(&mut f), max)
}

// ---- format ------------------------------------------------------------------

/// `string.format`'s view of the VM's arguments.
struct VmArgs<'v>(&'v mut Vm);

impl FormatArgs for VmArgs<'_> {
    type Error = AnyError;

    fn top(&self) -> usize {
        self.0.top()
    }

    fn integer(&mut self, arg: usize) -> Result<i64, AnyError> {
        Ok(self.0.check_integer(arg)?)
    }

    fn number(&mut self, arg: usize) -> Result<f64, AnyError> {
        let v = self.0.check_number(arg)?;
        Ok(number::to_float_ns(v).unwrap_or(0.0))
    }

    fn tostring(&mut self, arg: usize) -> Result<Cow<'_, [u8]>, AnyError> {
        let v = self.0.arg(arg);
        let s = self.0.to_display(v)?;
        Ok(Cow::Borrowed(self.0.heap.string(s).as_bytes()))
    }

    fn literal(&mut self, arg: usize) -> Literal<'_> {
        match self.0.arg(arg) {
            Value::Str(s) => Literal::Str(self.0.heap.string(s).as_bytes()),
            Value::Int(i) => Literal::Int(i),
            Value::Float(f) => Literal::Float(f),
            Value::Nil | Value::Bool(_) => Literal::NilOrBool,
            _ => Literal::Other,
        }
    }

    fn pointer(&mut self, arg: usize) -> Option<usize> {
        match value_addr(&self.0.heap, self.0.arg(arg)) {
            0 => None,
            p => Some(p as usize),
        }
    }
}

fn str_format(vm: &mut Vm) -> LuaResult<Action> {
    let f = vm.check_string(1)?;
    let fmt = bytes(vm, f).to_vec();
    match crate::format::format(&fmt, &mut VmArgs(vm)) {
        Ok(out) => ret_str(vm, out),
        Err(e) => Err(e.into_error(vm)),
    }
}

// ---- pack --------------------------------------------------------------------

impl PackArgs for VmArgs<'_> {
    type Error = AnyError;

    fn integer(&mut self, arg: usize) -> Result<i64, AnyError> {
        Ok(self.0.check_integer(arg)?)
    }

    fn number(&mut self, arg: usize) -> Result<f64, AnyError> {
        FormatArgs::number(self, arg)
    }

    fn string(&mut self, arg: usize) -> Result<Cow<'_, [u8]>, AnyError> {
        let s = self.0.check_string(arg)?;
        Ok(Cow::Borrowed(self.0.heap.string(s).as_bytes()))
    }
}

fn str_pack(vm: &mut Vm) -> LuaResult<Action> {
    let f = vm.check_string(1)?;
    let fmt = bytes(vm, f).to_vec();
    match crate::pack::pack(&fmt, &mut VmArgs(vm)) {
        Ok(out) => ret_str(vm, out),
        Err(e) => Err(e.into_error(vm)),
    }
}

fn str_packsize(vm: &mut Vm) -> LuaResult<Action> {
    let f = vm.check_string(1)?;
    match crate::pack::packsize(bytes(vm, f)) {
        Ok(n) => {
            vm.push(Value::Int(n));
            Ok(Action::Return(1))
        }
        Err(e) => Err(raise(vm, e)),
    }
}

fn str_unpack(vm: &mut Vm) -> LuaResult<Action> {
    let f = vm.check_string(1)?;
    let d = vm.check_string(2)?;
    let init = vm.opt_integer(3, 1)?;
    let (values, next) = match crate::pack::unpack(bytes(vm, f), bytes(vm, d), init) {
        Ok(r) => r,
        Err(e) => return Err(raise(vm, e)),
    };
    if vm.check_stack(values.len() + 1).is_err() {
        return Err(vm.error("stack overflow (too many results)"));
    }
    for v in &values {
        let v = match *v {
            Unpacked::Int(i) => Value::Int(i),
            Unpacked::Float(x) => Value::Float(x),
            Unpacked::Str { start, end } => {
                let b = bytes(vm, d)[start..end].to_vec();
                vm.heap.str_value(b)
            }
        };
        vm.push(v);
    }
    vm.push(Value::Int(next));
    Ok(Action::Return(values.len() + 1))
}
