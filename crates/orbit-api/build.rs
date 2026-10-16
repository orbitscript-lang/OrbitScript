//! SPIKE: the `table` and `stabby` projections link their library like any
//! shared library (DT_NEEDED). ORBIT_LIB_DIR says where it is at build time.
fn main() {
    println!("cargo:rerun-if-env-changed=ORBIT_LIB_DIR");
    let table = std::env::var_os("CARGO_FEATURE_TABLE").is_some();
    let stabby = std::env::var_os("CARGO_FEATURE_STABBY").is_some();
    if table || stabby {
        if let Ok(dir) = std::env::var("ORBIT_LIB_DIR") {
            println!("cargo:rustc-link-search=native={dir}");
        }
    }
    if table {
        println!("cargo:rustc-link-lib=dylib=orbit_table");
    }
}
