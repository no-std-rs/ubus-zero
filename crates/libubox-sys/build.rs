//! Discover `.dep/bin/` (containing `libubox.so` + `libubus.so`) and
//! `.dep/bin/include/` (libubus.h, libubox/blobmsg.h, ...) relative to
//! the workspace root. When found, emit link directives + size
//! constants probed from a tiny C program. When absent, silent-skip
//! (cargo:warning=) so `cargo check` on a fresh clone still succeeds.
//!
//! Plain `cc` (not the `cc` crate) — matches `crates/testkit/build.rs`,
//! keeps the dep tree minimal for what is essentially a 30-line probe.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=CC");
    println!("cargo:rerun-if-env-changed=AR");
    println!("cargo:rerun-if-env-changed=UBUS_ZERO_LIBUBOX_DIR");
    // Tell rustc the cfg name is intentional so check-cfg doesn't warn.
    println!("cargo::rustc-check-cfg=cfg(have_libubox)");
    println!("cargo::rustc-check-cfg=cfg(have_blobmsg_json)");

    let out_dir: PathBuf = env::var_os("OUT_DIR").expect("OUT_DIR set by cargo").into();

    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("linux") {
        println!("cargo:warning=libubox-sys: non-Linux target, FFI disabled");
        write_fallback_sizes(&out_dir);
        return;
    }

    let manifest_dir: PathBuf = env::var_os("CARGO_MANIFEST_DIR")
        .expect("CARGO_MANIFEST_DIR set by cargo")
        .into();
    let workspace_root = manifest_dir
        .parent()
        .and_then(Path::parent)
        .expect("workspace root")
        .to_path_buf();
    let explicit = env::var_os("UBUS_ZERO_LIBUBOX_DIR").map(PathBuf::from);
    let dep_bin = explicit
        .clone()
        .unwrap_or_else(|| workspace_root.join(".dep/bin"));
    println!("cargo:rerun-if-changed={}", dep_bin.display());
    emit_probe_dir_reruns(&workspace_root);

    let include_dir = if explicit.is_some() {
        Some(dep_bin.join("include"))
    } else {
        locate_include_dir(&workspace_root)
    };
    let Some(include_dir) = include_dir else {
        println!(
            "cargo:warning=libubox-sys: no headers under {} or {} — run tools/ubusd-builder/build.sh; FFI disabled",
            workspace_root.join(".dep/bin/include").display(),
            workspace_root.join(".dep/include").display(),
        );
        write_fallback_sizes(&out_dir);
        return;
    };
    emit_include_reruns(&include_dir);
    if explicit.is_some()
        && (!include_dir.join("libubus.h").is_file()
            || !include_dir.join("libubox/blobmsg.h").is_file())
    {
        panic!("UBUS_ZERO_LIBUBOX_DIR must contain libubus.so, libubox.so and include/libubus.h and include/libubox/blobmsg.h");
    }

    let libubox_so = dep_bin.join("libubox.so");
    let libubus_so = dep_bin.join("libubus.so");
    let blobmsg_json_header = include_dir.join("libubox/blobmsg_json.h");
    let libblobmsg_json_so = dep_bin.join("libblobmsg_json.so");
    println!("cargo:rerun-if-changed={}", libubox_so.display());
    println!("cargo:rerun-if-changed={}", libubus_so.display());
    println!("cargo:rerun-if-changed={}", blobmsg_json_header.display());
    println!("cargo:rerun-if-changed={}", libblobmsg_json_so.display());
    if !libubox_so.is_file() || !libubus_so.is_file() {
        assert!(
            explicit.is_none(),
            "UBUS_ZERO_LIBUBOX_DIR is missing libubus.so or libubox.so"
        );
        println!(
            "cargo:warning=libubox-sys: missing {} or {} — run tools/ubusd-builder/build.sh; FFI disabled",
            libubox_so.display(),
            libubus_so.display(),
        );
        write_fallback_sizes(&out_dir);
        return;
    }

    let Some((blob_buf_size, ubus_context_size, blob_attr_size)) =
        probe_sizes(&include_dir, &out_dir)
    else {
        assert!(
            explicit.is_none(),
            "UBUS_ZERO_LIBUBOX_DIR sizeof probe failed; check headers, target and compiler"
        );
        println!(
            "cargo:warning=libubox-sys: sizeof probe failed — header / compiler mismatch; FFI disabled"
        );
        write_fallback_sizes(&out_dir);
        return;
    };

    write_sizes(&out_dir, blob_buf_size, ubus_context_size, blob_attr_size);
    let have_blobmsg_json = blobmsg_json_header.is_file() && libblobmsg_json_so.is_file();

    if !compile_accessors(&include_dir, &out_dir, have_blobmsg_json) {
        assert!(
            explicit.is_none(),
            "UBUS_ZERO_LIBUBOX_DIR accessor compilation failed"
        );
        println!("cargo:warning=libubox-sys: accessors.c compile failed; FFI disabled");
        write_fallback_sizes(&out_dir);
        return;
    }

    println!("cargo:rustc-link-search=native={}", dep_bin.display());
    // Static archive built by `compile_accessors` lives in OUT_DIR;
    // tell the linker to look there too. `cargo:rustc-link-arg=` only
    // affects this crate's own artifacts (bins/examples/tests), not
    // downstream consumers — a static lib is the portable way to
    // re-export shim symbols across the dependency graph.
    println!("cargo:rustc-link-search=native={}", out_dir.display());
    println!("cargo:rustc-link-lib=static=ubus_zero_libubox_shim");
    println!("cargo:rustc-link-lib=dylib=ubox");
    println!("cargo:rustc-link-lib=dylib=ubus");
    if have_blobmsg_json {
        println!("cargo:rustc-link-lib=dylib=blobmsg_json");
        println!("cargo:rustc-cfg=have_blobmsg_json");
    }
    println!("cargo:rustc-link-arg=-Wl,-rpath,{}", dep_bin.display());
    println!(
        "cargo:rustc-env=UBUS_ZERO_LIBUBOX_DIR={}",
        dep_bin.display()
    );
    println!("cargo:rustc-cfg=have_libubox");
    // Metadata exported via the `links = "ubox"` key: dependent build
    // scripts (notably `crates/testkit/build.rs`) read DEP_UBOX_LIBUBOX
    // and forward `cfg(have_libubox)` into their own crate.
    println!("cargo:libubox=ready");
}

fn emit_probe_dir_reruns(workspace_root: &Path) {
    // Cargo caches failed build-script probes as aggressively as
    // successful ones. Track both candidate header directories so a
    // later `tools/ubusd-builder/build.sh` population of `.dep/`
    // invalidates the fallback cfg before tests can skip-pass.
    for rel in [".dep/bin/include", ".dep/include"] {
        println!(
            "cargo:rerun-if-changed={}",
            workspace_root.join(rel).display()
        );
    }
}

fn emit_include_reruns(include_dir: &Path) {
    for rel in ["libubus.h", "libubox/blob.h", "libubox/blobmsg.h"] {
        println!("cargo:rerun-if-changed={}", include_dir.join(rel).display());
    }
}

fn locate_include_dir(workspace_root: &Path) -> Option<PathBuf> {
    // The Dockerfile copies headers into /export/include/ and
    // `--output type=local,dest=.dep/bin` extracts them to
    // `.dep/bin/include/`. Older layouts may have used `.dep/include/`
    // — try both.
    for rel in [".dep/bin/include", ".dep/include"] {
        let candidate = workspace_root.join(rel);
        if candidate.join("libubus.h").is_file() && candidate.join("libubox/blobmsg.h").is_file() {
            return Some(candidate);
        }
    }
    None
}

fn probe_sizes(include_dir: &Path, out_dir: &Path) -> Option<(usize, usize, usize)> {
    let probe_src = out_dir.join("size_probe.c");
    let probe_bin = out_dir.join("size_probe");
    fs::write(
        &probe_src,
        r#"#include <stdio.h>
#include <libubox/blob.h>
#include <libubox/blobmsg.h>
#include <libubus.h>

int main(void) {
    printf("%zu %zu %zu\n",
        sizeof(struct blob_buf),
        sizeof(struct ubus_context),
        sizeof(struct blob_attr));
    return 0;
}
"#,
    )
    .ok()?;

    let cc = env::var("CC").unwrap_or_else(|_| "cc".into());
    let status = Command::new(&cc)
        .args(["-O0", "-I"])
        .arg(include_dir)
        .arg(&probe_src)
        .arg("-o")
        .arg(&probe_bin)
        .status()
        .ok()?;
    if !status.success() {
        return None;
    }
    let out = Command::new(&probe_bin).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?;
    let mut it = s.split_whitespace();
    let blob_buf = it.next()?.parse().ok()?;
    let ubus_ctx = it.next()?.parse().ok()?;
    let blob_attr = it.next()?.parse().ok()?;
    Some((blob_buf, ubus_ctx, blob_attr))
}

fn compile_accessors(include_dir: &Path, out_dir: &Path, have_blobmsg_json: bool) -> bool {
    // The shim re-exports libubox's `static inline` accessors and owns the
    // intrusive server-role libubus structs that Rust intentionally keeps
    // opaque. Compile both translation units with PIC, then archive their
    // objects so downstream crates resolve every shim symbol consistently.
    let src_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let sources = [
        (src_dir.join("accessors.c"), out_dir.join("accessors.o")),
        (src_dir.join("server_shim.c"), out_dir.join("server_shim.o")),
    ];
    let cc = env::var("CC").unwrap_or_else(|_| "cc".into());
    for (src, obj) in &sources {
        println!("cargo:rerun-if-changed={}", src.display());
        let mut command = Command::new(&cc);
        command
            .args(["-O2", "-fPIC", "-Wall", "-Wextra", "-c"])
            .arg("-I")
            .arg(include_dir);
        if have_blobmsg_json {
            command.arg("-DUBUS_ZERO_HAVE_BLOBMSG_JSON");
        }
        let cc_status = match command.arg(src).arg("-o").arg(obj).status() {
            Ok(s) => s,
            Err(_) => return false,
        };
        if !cc_status.success() {
            return false;
        }
    }
    let archive = out_dir.join("libubus_zero_libubox_shim.a");
    let _ = fs::remove_file(&archive);
    let ar = env::var("AR").unwrap_or_else(|_| "ar".into());
    match Command::new(&ar)
        .arg("crs")
        .arg(&archive)
        .args(sources.iter().map(|(_, obj)| obj))
        .status()
    {
        Ok(s) => s.success(),
        Err(_) => false,
    }
}

fn write_sizes(out_dir: &Path, blob_buf: usize, ubus_ctx: usize, blob_attr: usize) {
    let body = format!(
        "// generated by build.rs from a sizeof() probe against the headers\n\
         pub const BLOB_BUF_SIZE: usize = {blob_buf};\n\
         pub const UBUS_CONTEXT_SIZE: usize = {ubus_ctx};\n\
         pub const BLOB_ATTR_SIZE: usize = {blob_attr};\n"
    );
    fs::write(out_dir.join("sizes.rs"), body).expect("write sizes.rs");
}

fn write_fallback_sizes(out_dir: &Path) {
    // Values are unreachable when `have_libubox` is unset (the FFI
    // module is cfg'd out), but the include!() in lib.rs still needs
    // these constants to exist for `cargo check` to type-check.
    let body = "pub const BLOB_BUF_SIZE: usize = 0;\n\
                pub const UBUS_CONTEXT_SIZE: usize = 0;\n\
                pub const BLOB_ATTR_SIZE: usize = 0;\n";
    fs::write(out_dir.join("sizes.rs"), body).expect("write sizes.rs");
}
