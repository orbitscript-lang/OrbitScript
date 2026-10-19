//! DAP's framing: a `Content-Length` header, a blank line, then that many
//! bytes of JSON.

use std::io::{self, BufRead, Write};

use serde_json::Value;

/// The next message, or `None` at the end of the stream.
pub(crate) fn read(r: &mut impl BufRead) -> io::Result<Option<Value>> {
    let mut length = None;
    let mut line = String::new();
    loop {
        line.clear();
        if r.read_line(&mut line)? == 0 {
            return Ok(None);
        }
        let header = line.trim_end_matches(['\r', '\n']);
        if header.is_empty() {
            if length.is_some() {
                break;
            }
            continue;
        }
        if let Some((name, value)) = header.split_once(':')
            && name.trim().eq_ignore_ascii_case("content-length")
        {
            length = value.trim().parse::<usize>().ok();
        }
    }
    let mut body = vec![0; length.unwrap_or(0)];
    r.read_exact(&mut body)?;
    serde_json::from_slice(&body)
        .map(Some)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

/// Write `msg` with its header.
pub(crate) fn write(w: &mut dyn Write, msg: &Value) -> io::Result<()> {
    let body = serde_json::to_vec(msg)?;
    write!(w, "Content-Length: {}\r\n\r\n", body.len())?;
    w.write_all(&body)?;
    w.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_round_trip() {
        let mut out = Vec::new();
        write(&mut out, &serde_json::json!({"seq": 1, "type": "request"})).unwrap();
        write(&mut out, &serde_json::json!({"seq": 2, "type": "event"})).unwrap();
        let mut r = &out[..];
        assert_eq!(read(&mut r).unwrap().unwrap()["seq"], 1);
        assert_eq!(read(&mut r).unwrap().unwrap()["seq"], 2);
        assert!(read(&mut r).unwrap().is_none());
    }
}
