//! Skip only rpcd's redundant machine-global flush after its real successful
//! fixture-file fsync. A downstream oracle counts forwarding and executes the
//! actual fsync syscall; bytes, mode, errors and outside paths remain observable.
#![cfg(target_os = "linux")]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::Command;

use ubus_testkit::unique_socket_path;

struct Fixture(PathBuf);

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

const ORACLE: &str = r#"
#define _GNU_SOURCE
#include <sys/syscall.h>
#include <unistd.h>
static int file_calls, global_calls;
int fsync(int fd) { file_calls++; return syscall(SYS_fsync, fd); }
void sync(void) { global_calls++; }
int probe_file_calls(void) { return file_calls; }
int probe_global_calls(void) { return global_calls; }
"#;

const PROBE: &str = r#"
#define _GNU_SOURCE
#include <assert.h>
#include <dlfcn.h>
#include <errno.h>
#include <fcntl.h>
#include <string.h>
#include <unistd.h>
int main(int argc, char **argv) {
    assert(argc == 4);
    int enabled = !strcmp(argv[3], "enabled");
    int (*file_calls)(void) = dlsym(RTLD_DEFAULT, "probe_file_calls");
    int (*global_calls)(void) = dlsym(RTLD_DEFAULT, "probe_global_calls");
    assert(file_calls && global_calls);
    sync();
    assert(global_calls() == 1);
    int fd = open(argv[1], O_WRONLY | O_CREAT | O_TRUNC, 0640);
    assert(fd >= 0);
    unsigned char bytes[] = { 0, 1, 2, 3, 255, 254 };
    assert(write(fd, bytes, sizeof(bytes)) == sizeof(bytes));
    assert(fsync(fd) == 0 && file_calls() == 1);
    assert(close(fd) == 0);
    sync();
    assert(global_calls() == (enabled ? 1 : 2));
    // The exemption is consumed by one global sync only.
    sync();
    assert(global_calls() == (enabled ? 2 : 3));
    // A sibling prefix lies outside the configured private root.
    fd = open(argv[2], O_WRONLY | O_CREAT | O_TRUNC, 0640);
    assert(fd >= 0 && write(fd, bytes, sizeof(bytes)) == sizeof(bytes));
    assert(fsync(fd) == 0 && file_calls() == 2 && close(fd) == 0);
    sync();
    assert(global_calls() == (enabled ? 3 : 4));
    // Preserve the actual fsync error and clear any preceding success.
    fd = open(argv[1], O_RDONLY);
    assert(fd >= 0 && fsync(fd) == 0 && file_calls() == 3);
    assert(close(fd) == 0);
    assert(fsync(-1) == -1 && errno == EBADF && file_calls() == 4);
    sync();
    assert(global_calls() == (enabled ? 4 : 5));
    return 0;
}
"#;

#[test]
fn private_file_sync_keeps_fsync_bytes_metadata_errors_and_outside_paths_real() {
    let fixture = Fixture(unique_socket_path("rpcd-sync").with_extension("d"));
    let private = fixture.0.join("private");
    let sibling = fixture.0.join("private-sibling");
    fs::create_dir_all(&private).unwrap();
    fs::create_dir(&sibling).unwrap();
    let cc = std::env::var_os("CC").unwrap_or_else(|| "cc".into());
    let source = fixture.0.join("probe.c");
    let oracle_source = fixture.0.join("oracle.c");
    let binary = fixture.0.join("probe");
    let oracle = fixture.0.join("oracle.so");
    fs::write(&source, PROBE).unwrap();
    fs::write(&oracle_source, ORACLE).unwrap();
    for (input, output, extra) in [
        (&source, &binary, vec!["-ldl"]),
        (&oracle_source, &oracle, vec!["-shared", "-fPIC"]),
    ] {
        let result = Command::new(&cc)
            .args(["-Wall", "-Wextra", "-Werror"])
            .arg(input)
            .arg("-o")
            .arg(output)
            .args(extra)
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
    }
    let file = private.join("binary");
    let outside = sibling.join("binary");
    for enabled in [false, true] {
        let mut command = Command::new(&binary);
        command
            .arg(&file)
            .arg(&outside)
            .arg(if enabled { "enabled" } else { "disabled" })
            .env(
                "LD_PRELOAD",
                format!(
                    "{}:{}",
                    env!("UBUS_TESTKIT_PATH_REDIRECT"),
                    oracle.display()
                ),
            )
            .env_remove("UBUS_TESTKIT_PATH_MAP");
        if enabled {
            command.env(
                "UBUS_TESTKIT_RPCD_SYNC_ROOT",
                fs::canonicalize(&private).unwrap(),
            );
        } else {
            command.env_remove("UBUS_TESTKIT_RPCD_SYNC_ROOT");
        }
        let result = command.output().unwrap();
        assert!(
            result.status.success(),
            "enabled={enabled}, status={}, stderr={}",
            result.status,
            String::from_utf8_lossy(&result.stderr)
        );
        assert_eq!(fs::read(&file).unwrap(), [0, 1, 2, 3, 255, 254]);
        assert_eq!(
            fs::metadata(&file).unwrap().permissions().mode() & 0o777,
            0o640
        );
    }
}
