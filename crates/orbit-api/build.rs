//! The `shared` backend links liborbit like any shared library. Cargo
//! can't order that build before this one, so the library is built first
//! (`cargo testkit package` does both): this script points the linker at
//! ORBIT_LIB_DIR, or else at the profile directory this build writes to,
//! where `cargo build -p orbit-lib` left `liborbit_lib.so`.
fn main() {
    println!("cargo:rerun-if-env-changed=ORBIT_LIB_DIR");
    if std::env::var_os("CARGO_FEATURE_SHARED").is_none() {
        return;
    }
    let dir = match std::env::var("ORBIT_LIB_DIR") {
        Ok(d) => std::path::PathBuf::from(d),
        // OUT_DIR is <target>/<profile>/build/<pkg>-<hash>/out.
        Err(_) => {
            let out = std::path::PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR"));
            match out.ancestors().nth(3) {
                Some(d) => d.to_path_buf(),
                None => return,
            }
        }
    };
    println!("cargo:rustc-link-search=native={}", dir.display());
}
