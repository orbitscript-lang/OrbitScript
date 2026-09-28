//! The UTF-8 library (`lutf8lib.c`).

use orbit_vm::{Action, LuaResult, Vm};

use orbit_core::Value;

const MAXUNICODE: u32 = 0x10FFFF;
const MAXUTF: u32 = 0x7FFF_FFFF;
const MSG_INVALID: &str = "invalid UTF-8 code";

/// The pattern that matches one UTF-8 character.
const CHARPATTERN: &[u8] = b"[\0-\x7F\xC2-\xFD][\x80-\xBF]*";

/// Opens the utf8 library.
pub fn open_utf8(vm: &mut Vm) {
    let t = vm.new_lib(
        "utf8",
        &[
            ("offset", byteoffset),
            ("codepoint", codepoint),
            ("char", utfchar),
            ("len", utflen),
        ],
    );
    // `codes` keeps its two iterators as upvalues, registered once.
    let strict = vm.register("for iterator", iter_auxstrict);
    let lax = vm.register("for iterator", iter_auxlax);
    let codes = vm.native_closure("codes", iter_codes, vec![strict, lax]);
    vm.heap
        .raw_set_name(t, b"codes", codes)
        .expect("string key");
    let p = vm.heap.str_value(CHARPATTERN);
    vm.heap
        .raw_set_name(t, b"charpattern", p)
        .expect("string key");
}

/// Byte `i` of `s`, or 0 past its end (the reference reads the terminating `'\0'`).
fn at(s: &[u8], i: usize) -> u8 {
    s.get(i).copied().unwrap_or(0)
}

fn iscont(c: u8) -> bool {
    c & 0xC0 == 0x80
}

fn u_posrelat(pos: i64, len: usize) -> i64 {
    if pos >= 0 {
        pos
    } else if pos.unsigned_abs() > len as u64 {
        0
    } else {
        len as i64 + pos + 1
    }
}

/// Decodes the sequence at `i`, returning the code point and the index after it.
fn decode(s: &[u8], i: usize, strict: bool) -> Option<(u32, usize)> {
    const LIMITS: [u32; 6] = [u32::MAX, 0x80, 0x800, 0x10000, 0x200000, 0x4000000];
    let mut c = at(s, i) as u32;
    let mut res: u32 = 0;
    let mut count = 0;
    if c < 0x80 {
        res = c;
    } else if c >= 0xfe {
        return None;
    } else {
        while c & 0x40 != 0 {
            count += 1;
            let cc = at(s, i + count);
            if !iscont(cc) {
                return None;
            }
            res = (res << 6) | (cc & 0x3F) as u32;
            c <<= 1;
        }
        res |= (c & 0x7F) << (count * 5);
        if res > MAXUTF || res < LIMITS[count] {
            return None;
        }
    }
    if strict && (res > MAXUNICODE || (0xD800..=0xDFFF).contains(&res)) {
        return None;
    }
    Some((res, i + count + 1))
}

/// Encodes `x` (at most [`MAXUTF`]) as in `luaO_utf8esc`.
pub(crate) fn utf8_encode(mut x: u32, out: &mut Vec<u8>) {
    if x < 0x80 {
        out.push(x as u8);
        return;
    }
    let mut buf = [0u8; 8];
    let mut n = 1;
    let mut mfb: u32 = 0x3f;
    loop {
        buf[8 - n] = 0x80 | (x & 0x3f) as u8;
        n += 1;
        x >>= 6;
        mfb >>= 1;
        if x <= mfb {
            break;
        }
    }
    buf[8 - n] = ((!mfb << 1) | x) as u8;
    out.extend_from_slice(&buf[8 - n..]);
}

fn string_arg(vm: &mut Vm, arg: usize) -> LuaResult<Vec<u8>> {
    let s = vm.check_string(arg)?;
    Ok(vm.heap.string(s).as_bytes().to_vec())
}

fn utflen(vm: &mut Vm) -> LuaResult<Action> {
    let s = string_arg(vm, 1)?;
    let len = s.len() as i64;
    let mut posi = u_posrelat(vm.opt_integer(2, 1)?, s.len());
    let mut posj = u_posrelat(vm.opt_integer(3, -1)?, s.len());
    let lax = vm.arg(4).is_truthy();
    if !(1 <= posi && posi - 1 <= len) {
        return Err(vm.arg_error(2, "initial position out of bounds"));
    }
    posi -= 1;
    posj -= 1;
    if posj >= len {
        return Err(vm.arg_error(3, "final position out of bounds"));
    }
    let mut n = 0;
    while posi <= posj {
        match decode(&s, posi as usize, !lax) {
            Some((_, next)) => posi = next as i64,
            None => {
                vm.push(Value::Nil);
                vm.push(Value::Int(posi + 1));
                return Ok(Action::Return(2));
            }
        }
        n += 1;
    }
    vm.push(Value::Int(n));
    Ok(Action::Return(1))
}

fn codepoint(vm: &mut Vm) -> LuaResult<Action> {
    let s = string_arg(vm, 1)?;
    let posi = u_posrelat(vm.opt_integer(2, 1)?, s.len());
    let pose = u_posrelat(vm.opt_integer(3, posi)?, s.len());
    let lax = vm.arg(4).is_truthy();
    if posi < 1 {
        return Err(vm.arg_error(2, "out of bounds"));
    }
    if pose > s.len() as i64 {
        return Err(vm.arg_error(3, "out of bounds"));
    }
    if posi > pose {
        return Ok(Action::Return(0));
    }
    if pose - posi >= i32::MAX as i64 {
        return Err(vm.error("string slice too long"));
    }
    let mut i = (posi - 1) as usize;
    let end = pose as usize;
    let mut codes = Vec::new();
    while i < end {
        match decode(&s, i, !lax) {
            Some((c, next)) => {
                codes.push(c);
                i = next;
            }
            None => return Err(vm.error(MSG_INVALID)),
        }
    }
    vm.check_stack(codes.len())?;
    for &c in &codes {
        vm.push(Value::Int(c as i64));
    }
    Ok(Action::Return(codes.len()))
}

fn utfchar(vm: &mut Vm) -> LuaResult<Action> {
    let n = vm.top();
    let mut out = Vec::new();
    for i in 1..=n {
        let code = vm.check_integer(i)? as u64;
        if code > MAXUTF as u64 {
            return Err(vm.arg_error(i, "value out of range"));
        }
        utf8_encode(code as u32, &mut out);
    }
    let v = vm.heap.str_value(out);
    vm.push(v);
    Ok(Action::Return(1))
}

fn byteoffset(vm: &mut Vm) -> LuaResult<Action> {
    let s = string_arg(vm, 1)?;
    let len = s.len() as i64;
    let mut n = vm.check_integer(2)?;
    let def = if n >= 0 { 1 } else { len + 1 };
    let mut posi = u_posrelat(vm.opt_integer(3, def)?, s.len());
    if !(1 <= posi && posi - 1 <= len) {
        return Err(vm.arg_error(3, "position out of bounds"));
    }
    posi -= 1;
    let cont = |p: i64| iscont(at(&s, p as usize));
    if n == 0 {
        while posi > 0 && cont(posi) {
            posi -= 1;
        }
    } else {
        if cont(posi) {
            return Err(vm.error("initial position is a continuation byte"));
        }
        if n < 0 {
            while n < 0 && posi > 0 {
                loop {
                    posi -= 1;
                    if !(posi > 0 && cont(posi)) {
                        break;
                    }
                }
                n += 1;
            }
        } else {
            n -= 1;
            while n > 0 && posi < len {
                loop {
                    posi += 1;
                    if !cont(posi) {
                        break;
                    }
                }
                n -= 1;
            }
        }
    }
    if n != 0 {
        vm.push(Value::Nil);
        return Ok(Action::Return(1));
    }
    vm.push(Value::Int(posi + 1));
    if at(&s, posi as usize) & 0x80 != 0 {
        if cont(posi) {
            return Err(vm.error("initial position is a continuation byte"));
        }
        while cont(posi + 1) {
            posi += 1;
        }
    }
    vm.push(Value::Int(posi + 1));
    Ok(Action::Return(2))
}

fn iter_aux(vm: &mut Vm, strict: bool) -> LuaResult<Action> {
    let s = string_arg(vm, 1)?;
    let len = s.len() as u64;
    let mut n = match vm.arg(2) {
        Value::Int(i) => i as u64,
        Value::Float(f) => f as i64 as u64,
        _ => 0,
    };
    if n < len {
        while iscont(at(&s, n as usize)) {
            n += 1;
        }
    }
    if n >= len {
        return Ok(Action::Return(0));
    }
    match decode(&s, n as usize, strict) {
        Some((code, next)) if !iscont(at(&s, next)) => {
            vm.push(Value::Int(n as i64 + 1));
            vm.push(Value::Int(code as i64));
            Ok(Action::Return(2))
        }
        _ => Err(vm.error(MSG_INVALID)),
    }
}

fn iter_auxstrict(vm: &mut Vm) -> LuaResult<Action> {
    iter_aux(vm, true)
}

fn iter_auxlax(vm: &mut Vm) -> LuaResult<Action> {
    iter_aux(vm, false)
}

fn iter_codes(vm: &mut Vm) -> LuaResult<Action> {
    let lax = vm.arg(2).is_truthy();
    let s = string_arg(vm, 1)?;
    if iscont(at(&s, 0)) {
        return Err(vm.arg_error(1, MSG_INVALID));
    }
    let f = vm.upvalue(if lax { 2 } else { 1 });
    let sv = vm.arg(1);
    let sv = match sv {
        Value::Str(_) => sv,
        _ => vm.heap.str_value(&s),
    };
    vm.push(f);
    vm.push(sv);
    vm.push(Value::Int(0));
    Ok(Action::Return(3))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_matches_reference() {
        let mut v = Vec::new();
        utf8_encode(0x7FFF_FFFF, &mut v);
        assert_eq!(v, b"\xFD\xBF\xBF\xBF\xBF\xBF");
        v.clear();
        utf8_encode(0x20AC, &mut v);
        assert_eq!(v, "€".as_bytes());
        assert_eq!(decode("€".as_bytes(), 0, true), Some((0x20AC, 3)));
    }
}
