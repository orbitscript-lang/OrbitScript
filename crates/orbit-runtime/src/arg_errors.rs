//! Lua's argument errors: the one place their text is written.
//!
//! Every way of raising one (a native's context, the `orbit` crate's
//! `Stack` and `Call`, typed `#[native]` parameters, the OrbitScript
//! built-ins' `argerror` and `typeerror` intrinsics, the Lua C API's
//! `luaL_argerror` and `luaL_typeerror`) ends in [`bad_argument`], and the
//! messages more than one of them raise are the constants and functions
//! here. A message only one library raises (`position out of bounds`)
//! stays with that library.

use crate::object::{OrbitStr, OrbitString};

/// A float argument (or operand) where an integer is needed has no exact
/// integer value. Bitwise operators raise it too, with the operand's name
/// put after "number" (`number (local 'x') has no integer representation`).
pub const NO_INTEGER_REP: &str = "number has no integer representation";

/// A required argument is absent (`luaL_checkany`).
pub const VALUE_EXPECTED: &str = "value expected";

/// The type name a type error gives an absent argument.
pub const NO_VALUE: &str = "no value";

/// The message of a type error: `number expected, got nil`.
pub fn type_mismatch(expected: &str, got: &str) -> String {
    format!("{expected} expected, got {got}")
}

/// `bad argument #n to 'name' (msg)` for argument `n` (1-based, counting
/// `self`), or `calling 'name' on bad self (msg)` when a method call's
/// `self` is the bad one. The caller adds the position.
pub(crate) fn bad_argument(name: &[u8], method: bool, n: usize, msg: OrbitStr<'_>) -> OrbitString {
    let mut text = OrbitString::new();
    if method && n == 1 {
        text.push("calling '");
        text.push(name);
        text.push("' on bad self (");
    } else {
        let n = if method { n - 1 } else { n };
        text.push(format!("bad argument #{n} to '"));
        text.push(name);
        text.push("' (");
    }
    text.push(msg);
    text.push(")");
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(name: &str, method: bool, n: usize, msg: &str) -> String {
        bad_argument(name.as_bytes(), method, n, msg.into())
            .to_string_lossy()
            .into_owned()
    }

    #[test]
    fn frames_the_message() {
        assert_eq!(
            text("f", false, 2, VALUE_EXPECTED),
            "bad argument #2 to 'f' (value expected)"
        );
        // A method call doesn't count `self`, and a bad `self` says so.
        assert_eq!(
            text("m", true, 2, &type_mismatch("number", NO_VALUE)),
            "bad argument #1 to 'm' (number expected, got no value)"
        );
        assert_eq!(
            text("m", true, 1, NO_INTEGER_REP),
            "calling 'm' on bad self (number has no integer representation)"
        );
    }
}
