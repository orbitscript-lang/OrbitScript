//! [`Error`]: why loading, calling or converting something failed.

use std::fmt;

use crate::pin::Pinned;
use crate::value::Value;

/// What kind of failure an [`Error`] is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    /// A chunk failed to compile or decode (`chunk:line: message`).
    Syntax,
    /// A script raised an error, an operation failed, or a host function
    /// returned one.
    Runtime,
    /// The isolate ran out of memory, or past its memory limit.
    Memory,
    /// [`Interrupt::request`](crate::Interrupt::request) stopped the code.
    Interrupted,
}

/// Why something failed. Errors that come from a script keep the script's
/// error value ([`Error::value`]), whatever its type.
pub struct Error {
    kind: ErrorKind,
    message: String,
    traceback: Option<String>,
    value: Option<Pinned<Value<'static>>>,
}

impl Error {
    /// A runtime error whose value is the string `message`, unchanged. A
    /// host function that wants Lua's position prefix uses
    /// [`Call::error`](crate::Call::error) instead.
    pub fn runtime(message: impl Into<String>) -> Error {
        Error::new(ErrorKind::Runtime, message.into())
    }

    /// Lua's `not enough memory`, which scripts can catch like any error
    /// (a host function that can't allocate raises it).
    pub fn memory() -> Error {
        Error::new(
            ErrorKind::Memory,
            orbit_runtime::object::NoMemory::MESSAGE.to_owned(),
        )
    }

    /// A syntax error with `message` (`chunk:line: text`).
    pub fn syntax(message: impl Into<String>) -> Error {
        Error::new(ErrorKind::Syntax, message.into())
    }

    pub(crate) fn new(kind: ErrorKind, message: String) -> Error {
        Error {
            kind,
            message,
            traceback: None,
            value: None,
        }
    }

    pub(crate) fn with_value(mut self, value: Pinned<Value<'static>>) -> Error {
        self.value = Some(value);
        self
    }

    pub(crate) fn with_traceback(mut self, traceback: Option<String>) -> Error {
        self.traceback = traceback;
        self
    }

    pub fn kind(&self) -> ErrorKind {
        self.kind
    }

    /// The message, as the standalone interpreter prints it.
    pub fn message(&self) -> &str {
        &self.message
    }

    /// A traceback, for errors from [`Function::call_traced`](crate::Function::call_traced).
    pub fn traceback(&self) -> Option<&str> {
        self.traceback.as_deref()
    }

    /// The error value the script raised, when it came from a script.
    pub fn value(&self) -> Option<&Pinned<Value<'static>>> {
        self.value.as_ref()
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl fmt::Debug for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Error")
            .field("kind", &self.kind)
            .field("message", &self.message)
            .field("traceback", &self.traceback)
            .finish_non_exhaustive()
    }
}

impl std::error::Error for Error {}
