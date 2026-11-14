//! The command line: file names that are not UTF-8.

#[cfg(unix)]
#[test]
fn file_names_are_bytes() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;
    let dir = std::env::temp_dir().join(format!("orbit-check-args-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join(OsStr::from_bytes(b"\xff.orb"));
    std::fs::write(&file, "let _x = 1;\n").unwrap();
    let o = std::process::Command::new(env!("CARGO_BIN_EXE_orbit-check"))
        .arg(&file)
        .output()
        .unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        o.status.success(),
        "{}{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    );
}
