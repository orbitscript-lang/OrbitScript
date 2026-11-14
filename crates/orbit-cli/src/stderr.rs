//! The programs' own messages on standard error (errors, tracebacks,
//! usage), written as a text stream the way the reference interpreter's
//! are: with `\r\n` line ends on Windows (`orbit::LineEndings`).

use std::io::Write;

/// Write `bytes` to standard error as a text stream.
pub fn write(bytes: &[u8]) {
    let err = std::io::stderr().lock();
    let _ = if orbit::LineEndings::Native.translates() {
        orbit::text::TextWriter::new(err).write_all(bytes)
    } else {
        let mut err = err;
        err.write_all(bytes)
    };
}

/// `eprint!` through [`write`].
macro_rules! err_print {
    ($($arg:tt)*) => {
        $crate::stderr::write(format!($($arg)*).as_bytes())
    };
}

/// `eprintln!` through [`write`].
macro_rules! err_println {
    ($($arg:tt)*) => {
        $crate::stderr::write(format!("{}\n", format_args!($($arg)*)).as_bytes())
    };
}

pub(crate) use {err_print, err_println};
