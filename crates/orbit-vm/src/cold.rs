//! The slow and rare halves of instructions.
//!
//! The dispatch loop in `run()` is one large function, and every line in it
//! competes for the same machine registers. So `run()` keeps only each
//! instruction's common case: decoding, a type check and a few operations
//! on registers. Everything bigger or rarer (allocation, cache misses,
//! varargs, concatenation, error messages) is a method here, marked
//! `#[inline(never)]` so it costs the loop one call instead of its body.
//! New instructions follow the same split: their fast path goes in `run()`,
//! the rest here. That keeps the loop's size, and the hot instructions'
//! register allocation, independent of how many instructions there are.

use orbit_bytecode::{CaptureSource, MULTI};
use orbit_core::table::{self, Key};
use orbit_core::{Cell, Closure, FieldCache, Gc, LuaStr, Table, Value};

use crate::interp::Flow;
use crate::ops::{self, Exc};
use crate::{LuaError, Vm};

impl Vm {
    /// Raise an instruction's error: add its position, then resume the
    /// nearest protected call or return the error.
    #[cold]
    #[inline(never)]
    pub(crate) fn raise(&mut self, x: Exc, stop: usize) -> Result<Flow, LuaError> {
        let e = self.make_error(x, stop);
        self.unwind(e, stop)
    }

    /// `NewTable`.
    #[inline(never)]
    pub(crate) fn op_new_table(&mut self, array_hint: usize, hash_hint: usize) -> Value {
        Value::Table(self.heap.alloc(Table::with_capacity(array_hint, hash_hint)))
    }

    /// `CellNew`: a fresh cell holding `v` in cell slot `slot`.
    #[inline(never)]
    pub(crate) fn op_cell_new(&mut self, slot: usize, v: Value) {
        let c = self.heap.alloc(Cell { value: v });
        self.rt.stack.cells[slot] = c;
    }

    /// `Closure`: instantiate child prototype `proto` of `parent`.
    #[inline(never)]
    pub(crate) fn op_closure(
        &mut self,
        parent: Gc<Closure>,
        cell_base: usize,
        proto: usize,
    ) -> Value {
        let parent = self.heap.get(parent);
        let child = self.heap.get(parent.proto).children[proto];
        let captures: Box<[Gc<Cell>]> = self
            .heap
            .get(child)
            .captures
            .iter()
            .map(|c| match *c {
                CaptureSource::Cell(i) => self.rt.stack.cells[cell_base + i as usize],
                CaptureSource::Capture(i) => parent.captures[i as usize],
                CaptureSource::Env => parent.captures[0],
            })
            .collect();
        let code = self.heap.get(child).code.clone();
        Value::Closure(self.heap.alloc(Closure {
            proto: child,
            code,
            captures,
        }))
    }

    /// `Concat` of the `n` values at stack index `from`.
    #[inline(never)]
    pub(crate) fn op_concat(&mut self, from: usize, n: usize) -> Result<Value, Exc> {
        let regs = &self.rt.stack.regs;
        let mut buf = Vec::new();
        let mut bad = None;
        for (i, &v) in regs[from..from + n].iter().enumerate() {
            if !ops::concat_piece(&self.heap, v, &mut buf) {
                bad = Some(i);
            }
        }
        if let Some(mut j) = bad {
            let strnum = |v: Value| matches!(v, Value::Str(_) | Value::Int(_) | Value::Float(_));
            if j == n - 1 && n >= 2 && !strnum(regs[from + n - 2]) {
                j -= 1;
            }
            let v = regs[from + j];
            return Err(Exc::Rt(
                format!("attempt to concatenate a {} value", v.type_name()),
                Some(j as u8),
            ));
        }
        Ok(Value::Str(
            self.rt.strings.new_str_owned(&mut self.heap, buf),
        ))
    }

    /// `SetList`: store the `n` values at stack index `from` into `t`,
    /// starting at index `start`.
    #[inline(never)]
    pub(crate) fn op_set_list(&mut self, t: Gc<Table>, from: usize, n: usize, start: i64) {
        let regs = &self.rt.stack.regs;
        self.heap.update(t, |tab| {
            for (i, &v) in regs[from..from + n].iter().enumerate() {
                tab.set_int(start + i as i64, v);
            }
        });
    }

    /// `Vararg`: copy the frame's extra arguments (the `nvarargs` values
    /// below `base`) to register `dst`, `count` of them or all with `MULTI`.
    #[inline(never)]
    pub(crate) fn op_vararg(&mut self, base: usize, nvarargs: usize, dst: usize, count: u8) {
        let from = base - nvarargs;
        let to = base + dst;
        let regs = &mut self.rt.stack.regs;
        if count == MULTI {
            if regs.len() < to + nvarargs {
                regs.resize(to + nvarargs, Value::Nil);
            }
            regs.copy_within(from..from + nvarargs, to);
            self.rt.top = to + nvarargs;
        } else {
            for i in 0..count as usize {
                regs[to + i] = if i < nvarargs {
                    regs[from + i]
                } else {
                    Value::Nil
                };
            }
        }
    }

    /// `VarargTable`: `{n = count, ...}` of the frame's extra arguments.
    #[inline(never)]
    pub(crate) fn op_vararg_table(&mut self, base: usize, nvarargs: usize) -> Value {
        let t = self.heap.alloc(Table::with_capacity(nvarargs, 1));
        for i in 0..nvarargs {
            let v = self.rt.stack.regs[base - nvarargs + i];
            table::set_int(&mut self.heap, t, i as i64 + 1, v);
        }
        let n = self.rt.strings.new_str(&mut self.heap, b"n");
        let key = Key::str(&self.heap, n);
        table::set_key(&mut self.heap, t, &key, Value::Int(nvarargs as i64));
        Value::Table(t)
    }

    /// `VarargGet`: what the vararg table would hold at `key`.
    #[inline(never)]
    pub(crate) fn op_vararg_get(&self, base: usize, nvarargs: usize, key: Value) -> Value {
        let i = match key {
            Value::Int(i) => i,
            Value::Float(x) => match orbit_core::number::float_to_int(x) {
                Some(i) => i,
                None => return Value::Nil,
            },
            Value::Str(s) if self.heap.get(s).as_bytes() == b"n" => {
                return Value::Int(nvarargs as i64);
            }
            _ => return Value::Nil,
        };
        if (1..=nvarargs as i64).contains(&i) {
            self.rt.stack.regs[base - nvarargs + i as usize - 1]
        } else {
            Value::Nil
        }
    }

    /// `VarargUnpack`: like `Vararg`, from the vararg table `table`.
    #[inline(never)]
    pub(crate) fn op_vararg_unpack(
        &mut self,
        base: usize,
        table: Value,
        dst: usize,
        count: u8,
    ) -> Result<(), Exc> {
        let Value::Table(t) = table else {
            unreachable!("the vararg table is a read-only local")
        };
        let key = self.rt.strings.new_str(&mut self.heap, b"n");
        let n = match self.heap.get(t).get_str(&self.heap, key) {
            Value::Int(n) if (0..1 << 30).contains(&n) => n as usize,
            _ => return Err(Exc::Rt("vararg table has no proper 'n'".into(), None)),
        };
        let to = base + dst;
        let count = if count == MULTI {
            if to + n > self.rt.limits.max_stack {
                return Err(Exc::Rt("stack overflow".into(), None));
            }
            let regs = &mut self.rt.stack.regs;
            if regs.len() < to + n {
                regs.resize(to + n, Value::Nil);
            }
            self.rt.top = to + n;
            n
        } else {
            count as usize
        };
        let tab = self.heap.get(t);
        for i in 0..count {
            self.rt.stack.regs[to + i] = if i < n {
                tab.get_int(i as i64 + 1)
            } else {
                Value::Nil
            };
        }
        Ok(())
    }

    /// `ForPrep` on the loop state at stack index `at`. Returns whether the
    /// loop runs at all.
    #[inline(never)]
    pub(crate) fn op_for_prep(&mut self, at: usize) -> Result<bool, Exc> {
        let r = &mut self.rt.stack.regs[at..at + 4];
        let (init, limit, step) = (r[0], r[1], r[2]);
        if let (Value::Int(i), Value::Int(s)) = (init, step) {
            if s == 0 {
                return Err(Exc::Rt("'for' step is zero".into(), None));
            }
            return match ops::for_limit(i, limit, s) {
                Err(_) => Err(for_error("limit", limit)),
                Ok(None) => Ok(false),
                Ok(Some(lim)) => {
                    // The number of iterations left replaces the limit.
                    let count = if s > 0 {
                        (lim as u64).wrapping_sub(i as u64) / s as u64
                    } else {
                        (i as u64).wrapping_sub(lim as u64) / ((-(s + 1)) as u64 + 1)
                    };
                    r[1] = Value::Int(count as i64);
                    r[3] = Value::Int(i);
                    Ok(true)
                }
            };
        }
        let Some(lim) = limit.as_f64() else {
            return Err(for_error("limit", limit));
        };
        let Some(st) = step.as_f64() else {
            return Err(for_error("step", step));
        };
        let Some(start) = init.as_f64() else {
            return Err(for_error("initial", init));
        };
        if st == 0.0 {
            return Err(Exc::Rt("'for' step is zero".into(), None));
        }
        let runs = if st > 0.0 { start <= lim } else { lim <= start };
        if runs {
            r[0] = Value::Float(start);
            r[1] = Value::Float(lim);
            r[2] = Value::Float(st);
            r[3] = Value::Float(start);
        }
        Ok(runs)
    }

    /// `Len` of `v`.
    #[inline(never)]
    pub(crate) fn op_len(&self, v: Value) -> Result<Value, Exc> {
        match v {
            Value::Str(s) => Ok(Value::Int(self.heap.get(s).len() as i64)),
            Value::Table(t) => Ok(Value::Int(self.heap.get(t).border())),
            _ => Err(Exc::Rt(
                format!("attempt to get length of a {} value", v.type_name()),
                Some(0),
            )),
        }
    }

    /// The value of string key `k` in `t` when the inline cache missed;
    /// refills the cache.
    #[inline(never)]
    pub(crate) fn get_str_miss(&self, t: Gc<Table>, k: Gc<LuaStr>, cache: &FieldCache) -> Value {
        let tab = self.heap.get(t);
        match tab.find_str(&self.heap, k) {
            Some(slot) => {
                cache.fill(slot);
                tab.slot_value(slot)
            }
            None => Value::Nil,
        }
    }

    /// `t[k] = v` for string key `k` when the inline cache missed; refills
    /// the cache.
    #[inline(never)]
    pub(crate) fn set_str_miss(
        &mut self,
        t: Gc<Table>,
        k: Gc<LuaStr>,
        v: Value,
        cache: &FieldCache,
    ) {
        let key = Key::str(&self.heap, k);
        if let Some(slot) = table::set_key(&mut self.heap, t, &key, v) {
            cache.fill(slot);
        }
    }
}

/// Indexing something that isn't a table.
#[cold]
#[inline(never)]
pub(crate) fn index_error(v: Value) -> Exc {
    Exc::Rt(
        format!("attempt to index a {} value", v.type_name()),
        Some(0),
    )
}

/// `global x = v` found `x` already set.
#[cold]
#[inline(never)]
pub(crate) fn already_defined(name: &[u8]) -> Exc {
    Exc::Rt(
        format!("global '{}' already defined", String::from_utf8_lossy(name)),
        None,
    )
}

/// `_ENV` isn't a table.
#[cold]
#[inline(never)]
pub(crate) fn env_error(v: Value) -> Exc {
    Exc::Rt(
        format!(
            "attempt to index a {} value (upvalue '_ENV')",
            v.type_name()
        ),
        None,
    )
}

#[cold]
fn for_error(what: &str, v: Value) -> Exc {
    let what = if what == "initial" {
        "initial value"
    } else {
        what
    };
    Exc::Rt(
        format!("bad 'for' {what} (number expected, got {})", v.type_name()),
        None,
    )
}
