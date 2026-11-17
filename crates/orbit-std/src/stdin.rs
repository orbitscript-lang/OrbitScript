//! Standard input for everything that reads it besides `io`: `loadfile()`
//! and `dofile()` without a name, `debug.debug`, and the `orbit` program's
//! `-` and interactive mode. It is the C library's `stdin`, the stream
//! behind `io.stdin` (ADR-15), so a script's `io.read` and these readers
//! never split one input between two buffers.

use std::io::{self, Read};

use orbit::LineEndings;

/// Standard input as the isolate with `line_endings` reads it: the C
/// library's `stdin`, or Rust's where `io` doesn't use that (a build
/// without `io`, or a `Binary` isolate on Windows, whose `io.stdin` is a
/// binary copy of its own).
#[derive(Debug)]
pub struct StandardInput {
    #[cfg(feature = "io")]
    c: bool,
}

/// Standard input for an isolate with `line_endings` ([`StandardInput`]).
pub fn stdin(line_endings: LineEndings) -> StandardInput {
    let _ = line_endings;
    StandardInput {
        #[cfg(feature = "io")]
        c: !cfg!(windows) || line_endings.translates(),
    }
}

impl StandardInput {
    /// Read through the next `\n`, which is kept, appending to `line`.
    /// Gives how many bytes it read: 0 at the end of the input.
    pub fn read_line(&mut self, line: &mut Vec<u8>) -> io::Result<usize> {
        #[cfg(feature = "io")]
        if self.c {
            return c_stdin(|f| {
                let at = line.len();
                f.read_line(true, line)?;
                Ok(line.len() - at)
            });
        }
        io::BufRead::read_until(&mut io::stdin().lock(), b'\n', line)
    }
}

impl Read for StandardInput {
    /// One byte at a time from the C library's stream, so a read returns
    /// as soon as a terminal has a key, never waiting for a whole buffer.
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        #[cfg(feature = "io")]
        if self.c {
            if buf.is_empty() {
                return Ok(0);
            }
            return c_stdin(|f| match f.getc() {
                Some(b) => {
                    buf[0] = b;
                    Ok(1)
                }
                None => Ok(0),
            });
        }
        io::stdin().read(buf)
    }

    fn read_to_end(&mut self, buf: &mut Vec<u8>) -> io::Result<usize> {
        #[cfg(feature = "io")]
        if self.c {
            return c_stdin(|f| {
                let at = buf.len();
                f.read_into(usize::MAX, buf)?;
                Ok(buf.len() - at)
            });
        }
        io::stdin().read_to_end(buf)
    }
}

/// Run `f` on the C library's `stdin`, locked, and report the error the
/// stream has afterwards.
#[cfg(feature = "io")]
fn c_stdin<R>(f: impl FnOnce(&mut crate::stdio::Locked<'_>) -> io::Result<R>) -> io::Result<R> {
    use crate::stdio::{CFile, Std};
    let Some(mut file) = CFile::standard(Std::In) else {
        return Err(crate::crt::system_error(libc::EBADF));
    };
    let mut locked = file.lock();
    locked.clear_error();
    let r = f(&mut locked)?;
    match locked.error() {
        Some(e) => Err(e),
        None => Ok(r),
    }
}
