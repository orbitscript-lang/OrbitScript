//! Errors raised by library functions.

use std::fmt;

/// An error raised by a library function, in the two shapes `lauxlib` raises them.
///
/// Messages are bytes because they can quote parts of Lua strings, which need not be
/// UTF-8. The VM turns them into Lua error values: a [`LibError::Message`] is used as is
/// (`luaL_error` adds no position to errors raised by C functions), and a
/// [`LibError::Arg`] becomes `bad argument #arg to 'name' (msg)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LibError {
    /// `luaL_error`: a complete message.
    Message(Vec<u8>),
    /// `luaL_argerror`: a problem with argument number `arg` (1-based, as Lua counts).
    Arg {
        /// The argument's position in the Lua call.
        arg: usize,
        /// The explanation that goes in parentheses.
        msg: Vec<u8>,
    },
}

impl LibError {
    pub(crate) fn msg(m: impl Into<Vec<u8>>) -> Self {
        LibError::Message(m.into())
    }

    pub(crate) fn arg(arg: usize, msg: impl Into<Vec<u8>>) -> Self {
        LibError::Arg {
            arg,
            msg: msg.into(),
        }
    }
}

impl fmt::Display for LibError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LibError::Message(m) => f.write_str(&String::from_utf8_lossy(m)),
            LibError::Arg { arg, msg } => {
                write!(f, "bad argument #{arg} ({})", String::from_utf8_lossy(msg))
            }
        }
    }
}

impl std::error::Error for LibError {}
