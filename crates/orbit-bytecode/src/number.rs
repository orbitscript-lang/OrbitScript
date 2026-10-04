//! Lua numbers and the arithmetic rules between them, in one place.
//!
//! A Lua number is an integer or a float. Two integers give an integer
//! (wrapping on overflow), except `/` and `^`, which always give a float.
//! Any other mix converts the integer to a float. `//` and `%` floor, and
//! with two integers they fail on a zero divisor. The compiler's constant
//! folding, the VM's fast paths and its slow path all use these methods, so
//! they agree by construction. Checked against the reference `lua5.5`.
//!
//! It lives in `orbit-bytecode` because that is the one crate the compiler
//! and the runtime share; `orbit_core::number` re-exports it.

/// A Lua number: an integer or a float.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Number {
    Int(i64),
    Float(f64),
}

/// A binary arithmetic operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArithOp {
    Add,
    Sub,
    Mul,
    Div,
    IDiv,
    Mod,
    Pow,
}

/// Why integer arithmetic failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArithError {
    /// `n // 0` with integers.
    DivideByZero,
    /// `n % 0` with integers.
    ModuloByZero,
}

impl ArithError {
    /// The reference interpreter's message for this error.
    pub fn message(self) -> &'static str {
        match self {
            ArithError::DivideByZero => "attempt to divide by zero",
            ArithError::ModuloByZero => "attempt to perform 'n%0'",
        }
    }
}

impl Number {
    /// The value as a float.
    #[inline(always)]
    pub fn to_f64(self) -> f64 {
        match self {
            Number::Int(i) => i as f64,
            Number::Float(f) => f,
        }
    }

    /// `self op rhs`. With a constant `op` this inlines down to the one case.
    #[inline(always)]
    pub fn arith(self, op: ArithOp, rhs: Number) -> Result<Number, ArithError> {
        use Number::{Float, Int};
        Ok(match (self, rhs) {
            (Int(a), Int(b)) => match op {
                ArithOp::Add => Int(a.wrapping_add(b)),
                ArithOp::Sub => Int(a.wrapping_sub(b)),
                ArithOp::Mul => Int(a.wrapping_mul(b)),
                ArithOp::Div => Float(a as f64 / b as f64),
                ArithOp::IDiv => Int(int_idiv(a, b).ok_or(ArithError::DivideByZero)?),
                ArithOp::Mod => Int(int_mod(a, b).ok_or(ArithError::ModuloByZero)?),
                ArithOp::Pow => Float((a as f64).powf(b as f64)),
            },
            (a, b) => Float(float_arith(op, a.to_f64(), b.to_f64())),
        })
    }
}

impl core::ops::Neg for Number {
    type Output = Number;

    /// `-self`.
    #[inline(always)]
    fn neg(self) -> Number {
        match self {
            Number::Int(i) => Number::Int(i.wrapping_neg()),
            Number::Float(f) => Number::Float(-f),
        }
    }
}

/// `a op b` on floats.
#[inline(always)]
pub fn float_arith(op: ArithOp, a: f64, b: f64) -> f64 {
    match op {
        ArithOp::Add => a + b,
        ArithOp::Sub => a - b,
        ArithOp::Mul => a * b,
        ArithOp::Div => a / b,
        ArithOp::IDiv => float_idiv(a, b),
        ArithOp::Mod => float_mod(a, b),
        ArithOp::Pow => a.powf(b),
    }
}

/// Floor division of integers. `None` when dividing by zero.
#[inline]
pub fn int_idiv(a: i64, b: i64) -> Option<i64> {
    match b {
        0 => None,
        -1 => Some(a.wrapping_neg()),
        _ => {
            let q = a / b;
            Some(if a % b != 0 && (a ^ b) < 0 { q - 1 } else { q })
        }
    }
}

/// Floored modulo of integers. `None` when dividing by zero.
#[inline]
pub fn int_mod(a: i64, b: i64) -> Option<i64> {
    match b {
        0 => None,
        -1 => Some(0),
        _ => {
            let r = a % b;
            Some(if r != 0 && (r ^ b) < 0 { r + b } else { r })
        }
    }
}

/// Floored modulo of floats.
#[inline]
pub fn float_mod(a: f64, b: f64) -> f64 {
    let r = a % b;
    if r != 0.0 && (r < 0.0) != (b < 0.0) {
        r + b
    } else {
        r
    }
}

/// Floor division of floats.
#[inline]
pub fn float_idiv(a: f64, b: f64) -> f64 {
    (a / b).floor()
}

#[cfg(test)]
mod tests {
    use super::Number::{Float, Int};
    use super::*;

    #[test]
    fn integers_stay_integers_except_div_and_pow() {
        assert_eq!(Int(7).arith(ArithOp::Add, Int(2)), Ok(Int(9)));
        assert_eq!(Int(i64::MAX).arith(ArithOp::Add, Int(1)), Ok(Int(i64::MIN)));
        assert_eq!(Int(7).arith(ArithOp::Div, Int(2)), Ok(Float(3.5)));
        assert_eq!(Int(2).arith(ArithOp::Pow, Int(3)), Ok(Float(8.0)));
        assert_eq!(Int(-7).arith(ArithOp::IDiv, Int(2)), Ok(Int(-4)));
        assert_eq!(Int(-7).arith(ArithOp::Mod, Int(2)), Ok(Int(1)));
        assert_eq!(
            Int(i64::MIN).arith(ArithOp::IDiv, Int(-1)),
            Ok(Int(i64::MIN))
        );
    }

    #[test]
    fn mixed_operands_become_floats() {
        assert_eq!(Int(7).arith(ArithOp::Mul, Float(0.5)), Ok(Float(3.5)));
        assert_eq!(Float(-7.0).arith(ArithOp::Mod, Int(2)), Ok(Float(1.0)));
        assert_eq!(
            Int(7).arith(ArithOp::IDiv, Float(0.0)),
            Ok(Float(f64::INFINITY))
        );
    }

    #[test]
    fn integer_division_by_zero_fails() {
        assert_eq!(
            Int(1).arith(ArithOp::IDiv, Int(0)),
            Err(ArithError::DivideByZero)
        );
        assert_eq!(
            Int(1).arith(ArithOp::Mod, Int(0)),
            Err(ArithError::ModuloByZero)
        );
    }
}
