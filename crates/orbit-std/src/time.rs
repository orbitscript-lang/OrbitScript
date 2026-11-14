//! `os.date`'s format strings, checked in safe Rust before any of them
//! reaches C: the C library formats one conversion at a time
//! ([`crate::ctime`]), so a script's format never does.

/// Is `spec` (what follows a `%`) a conversion `os.date` accepts, and how
/// many bytes does it take? The C99 conversions plus the `E` and `O`
/// modifiers POSIX allows.
fn conversion_len(spec: &[u8]) -> Option<usize> {
    match spec {
        [b'E', c, ..] if b"cCxXyY".contains(c) => Some(2),
        [b'O', c, ..] if b"deHImMSuUVwWy".contains(c) => Some(2),
        [c, ..] if b"aAbBcCdDeFgGhHIjmMnprRStTuUVwWxXyYzZ%".contains(c) => Some(1),
        _ => None,
    }
}

/// Does conversion `spec` depend on the time category's locale (names,
/// the `%c` formats, the `E` and `O` alternatives)?
pub fn is_localized(spec: &[u8]) -> bool {
    spec.len() == 2 || b"aAbBhpcxXr".contains(&spec[0])
}

/// Format `fmt`, with `convert` writing each conversion (`spec`, what
/// follows its `%`, already checked) onto the output, or return the bad
/// conversion: the format from that `%` to its end, which is what the
/// error message shows.
pub fn format(
    fmt: &[u8],
    mut convert: impl FnMut(&[u8], &mut Vec<u8>),
) -> Result<Vec<u8>, Vec<u8>> {
    let mut out = Vec::with_capacity(fmt.len());
    let mut i = 0;
    while i < fmt.len() {
        if fmt[i] != b'%' {
            out.push(fmt[i]);
            i += 1;
            continue;
        }
        let spec = &fmt[i + 1..];
        let Some(n) = conversion_len(spec) else {
            return Err(fmt[i..].to_vec());
        };
        convert(&spec[..n], &mut out);
        i += 1 + n;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each conversion, as `[spec]`.
    fn specs(fmt: &[u8]) -> Result<Vec<u8>, Vec<u8>> {
        format(fmt, |spec, out| {
            out.push(b'[');
            out.extend_from_slice(spec);
            out.push(b']');
        })
    }

    #[test]
    fn conversions_are_passed_one_at_a_time() {
        assert_eq!(
            specs(b"a%Y-%m %Ec%Od%%z"),
            Ok(b"a[Y]-[m] [Ec][Od][%]z".to_vec())
        );
        assert_eq!(specs(b""), Ok(Vec::new()));
    }

    #[test]
    fn bad_conversions_report_the_rest() {
        assert_eq!(specs(b"ab%Qxy"), Err(b"%Qxy".to_vec()));
        assert_eq!(specs(b"%"), Err(b"%".to_vec()));
        assert_eq!(specs(b"%Ea"), Err(b"%Ea".to_vec()));
        assert_eq!(specs(b"%O"), Err(b"%O".to_vec()));
        assert_eq!(specs(b"%9"), Err(b"%9".to_vec()));
        // GNU's flags and extra conversions are not C's.
        assert_eq!(specs(b"%-d"), Err(b"%-d".to_vec()));
        assert_eq!(specs(b"%k"), Err(b"%k".to_vec()));
        assert_eq!(specs(b"%s"), Err(b"%s".to_vec()));
    }

    #[test]
    fn localized_conversions() {
        for s in [
            &b"a"[..],
            b"B",
            b"h",
            b"p",
            b"c",
            b"x",
            b"X",
            b"r",
            b"Ey",
            b"OH",
        ] {
            assert!(is_localized(s), "{s:?}");
        }
        for s in [&b"Y"[..], b"d", b"Z", b"z", b"%", b"T", b"D"] {
            assert!(!is_localized(s), "{s:?}");
        }
    }
}
