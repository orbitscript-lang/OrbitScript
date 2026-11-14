//! The command line: file names that are not UTF-8.

#[cfg(unix)]
#[test]
fn file_names_are_bytes() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;
    let dir = std::env::temp_dir().join(format!("orbit-fmt-args-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join(OsStr::from_bytes(b"\xff.orb"));
    // Some file systems (APFS) refuse names that are not UTF-8; then the
    // name still has to reach the "cannot read" error without a panic.
    let created = std::fs::write(&file, "let _x = 1;\n").is_ok();
    let o = std::process::Command::new(env!("CARGO_BIN_EXE_orbit-fmt"))
        .arg("--check")
        .arg(&file)
        .output()
        .unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(
        o.status.code(),
        Some(if created { 0 } else { 2 }),
        "{}{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    );
}
