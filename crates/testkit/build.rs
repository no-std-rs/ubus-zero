//! Compile the LD_PRELOAD shims. Paths are exposed to the consumer
//! crate via env vars that `env!` resolves at compile time under
//! `cfg(target_os = "linux")`.

use std::env;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=src/uid_spoof.c");
    println!("cargo:rerun-if-changed=src/nss_spoof.c");
    println!("cargo:rerun-if-changed=src/fault.c");
    println!("cargo:rerun-if-changed=src/stat_spoof.c");
    println!("cargo:rerun-if-changed=src/path_redirect.c");
    println!("cargo:rerun-if-changed=src/rpcd_exec_order.c");
    println!("cargo:rerun-if-env-changed=CC");
    // Tell rustc the cfg name is intentional so check-cfg doesn't warn.
    println!("cargo::rustc-check-cfg=cfg(have_libubox)");

    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("linux") {
        // `struct ucred` is Linux-only; skip the build. Consumer
        // gates the runtime wiring on `cfg(target_os = "linux")`.
        println!("cargo:warning=ubus-testkit LD_PRELOAD shims skipped: non-Linux target");
        return;
    }

    let out_dir: PathBuf = env::var_os("OUT_DIR").expect("OUT_DIR set by cargo").into();
    let uid_spoof = build_shim("src/uid_spoof.c", "libubus_testkit_uid_spoof.so", &out_dir);
    let nss_spoof = build_shim("src/nss_spoof.c", "libubus_testkit_nss_spoof.so", &out_dir);
    let fault = build_shim("src/fault.c", "libubus_testkit_fault.so", &out_dir);
    let stat_spoof = build_shim(
        "src/stat_spoof.c",
        "libubus_testkit_stat_spoof.so",
        &out_dir,
    );
    let path_redirect = build_shim(
        "src/path_redirect.c",
        "libubus_testkit_path_redirect.so",
        &out_dir,
    );
    let rpcd_exec_order = build_shim(
        "src/rpcd_exec_order.c",
        "libubus_testkit_rpcd_exec_order.so",
        &out_dir,
    );

    println!(
        "cargo:rustc-env=UBUS_TESTKIT_UID_SPOOF={}",
        uid_spoof.display()
    );
    println!(
        "cargo:rustc-env=UBUS_TESTKIT_NSS_SPOOF={}",
        nss_spoof.display()
    );
    println!("cargo:rustc-env=UBUS_TESTKIT_FAULT_SO={}", fault.display());
    println!(
        "cargo:rustc-env=UBUS_TESTKIT_STAT_SPOOF={}",
        stat_spoof.display()
    );
    println!(
        "cargo:rustc-env=UBUS_TESTKIT_PATH_REDIRECT={}",
        path_redirect.display()
    );
    println!(
        "cargo:rustc-env=UBUS_TESTKIT_RPCD_EXEC_ORDER={}",
        rpcd_exec_order.display()
    );

    // libubox-sys uses `links = "ubox"` and emits `cargo:libubox=ready`
    // when its build script located `.dep/bin/{libubox,libubus}.so` plus
    // headers. Cargo surfaces that to dependent build scripts as
    // `DEP_UBOX_LIBUBOX`. Forward as `cfg(have_libubox)` so this crate
    // can compile its `LibuboxClient` module conditionally.
    println!("cargo:rerun-if-env-changed=DEP_UBOX_LIBUBOX");
    if env::var("DEP_UBOX_LIBUBOX").as_deref() == Ok("ready") {
        println!("cargo:rustc-cfg=have_libubox");
    }

    embed_libubox_rpath();
}

/// Mirror of the root `build.rs::embed_libubox_rpath`: libubox-sys's
/// `cargo:rustc-link-arg=-Wl,-rpath,...` only applies to that crate's
/// own artifacts, so any ubus-testkit test binary that actually calls
/// libubox/libblobmsg_json FFI (retaining the DT_NEEDED entries that
/// `--as-needed` would otherwise drop) needs the rpath re-emitted here
/// or it exits 127 at load time outside `LD_LIBRARY_PATH` wrappers.
fn embed_libubox_rpath() {
    let manifest_dir = env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR set by cargo");
    let dep_bin = std::path::Path::new(&manifest_dir).join("../../.dep/bin");
    println!("cargo:rerun-if-changed={}", dep_bin.display());
    if dep_bin.join("libubus.so").is_file() && dep_bin.join("libubox.so").is_file() {
        println!(
            "cargo:rustc-link-arg-tests=-Wl,-rpath,{}",
            dep_bin.display()
        );
    }
}

fn build_shim(src: &str, so_name: &str, out_dir: &std::path::Path) -> PathBuf {
    let so_path = out_dir.join(so_name);
    // Another Cargo invocation can rebuild this OUT_DIR while a parallel
    // suite loads a shim. Publish a complete new inode atomically: compiling
    // directly over the live .so can truncate it or corrupt existing mappings.
    let staged = out_dir.join(format!(".{so_name}.{}.tmp", std::process::id()));
    let cc = env::var("CC").unwrap_or_else(|_| "cc".into());
    let status = Command::new(&cc)
        .args([
            "-shared",
            "-fPIC",
            "-O2",
            "-Wall",
            "-Wextra",
            "-fvisibility=default",
            src,
            "-o",
        ])
        .arg(&staged)
        .arg("-ldl")
        .status()
        .unwrap_or_else(|e| panic!("invoking {cc:?} for {src} shim failed: {e}"));
    if !status.success() {
        let _ = std::fs::remove_file(&staged);
        panic!("{src} shim build failed (cc exited {status})");
    }
    std::fs::rename(&staged, &so_path).expect("publish complete LD_PRELOAD shim");
    so_path
}
