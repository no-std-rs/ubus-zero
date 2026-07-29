use std::env;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    embed_libubox_rpath();
}

/// libubox-sys's link args do not propagate to this crate's test binary.
/// `--as-needed` retains the NEEDED entries only when a test calls the FFI,
/// so re-emit the rpath here. Generic `rustc-link-arg` (not `-tests`) because
/// the parity tests live in the LIB unit-test binary, which the `-tests`
/// variant (integration-test targets only) does not cover; the extra rpath
/// on the dev-only broker bin is benign.
fn embed_libubox_rpath() {
    let manifest_dir = env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR set by cargo");
    let dep_bin = std::path::Path::new(&manifest_dir).join("../../.dep/bin");
    println!("cargo:rerun-if-changed={}", dep_bin.display());
    if dep_bin.join("libubus.so").is_file() && dep_bin.join("libubox.so").is_file() {
        println!("cargo:rustc-link-arg=-Wl,-rpath,{}", dep_bin.display());
    }
}
