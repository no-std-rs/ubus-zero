fn main() {
    let manifest_dir =
        std::env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR set by Cargo");
    let dep_bin = std::path::Path::new(&manifest_dir).join(".dep/bin");

    println!("cargo:rerun-if-changed={}", dep_bin.display());
    if dep_bin.join("libubus.so").is_file() && dep_bin.join("libubox.so").is_file() {
        // Root integration tests call the compatibility FFI through
        // ubus-testkit. Linker arguments emitted by libubox-sys apply only to
        // that dependency's own artifacts, so root-owned test executables need
        // the fixture-library runpath repeated here.
        println!(
            "cargo:rustc-link-arg-tests=-Wl,-rpath,{}",
            dep_bin.display()
        );
    }
}
