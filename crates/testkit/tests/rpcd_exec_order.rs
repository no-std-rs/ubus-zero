//! The rpcd-only ordering shim must defer pipe EOF without consuming the real
//! child status or changing file/socket reads. Every process and fd is private.
#![cfg(target_os = "linux")]

use std::fs;
use std::path::PathBuf;
use std::process::Command;

use ubus_testkit::unique_socket_path;

struct Fixture(PathBuf);

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

const PROBE: &str = r#"
#define _GNU_SOURCE
#include <assert.h>
#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/wait.h>
#include <unistd.h>

int main(int argc, char **argv) {
    assert(argc == 3);
    int enabled = !strcmp(argv[2], "enabled");
    int output[2], gate[2];
    assert(pipe(output) == 0 && pipe(gate) == 0);
    pid_t child = fork();
    assert(child >= 0);
    if (!child) {
        close(output[0]);
        close(output[1]);
        close(gate[1]);
        char release;
        assert(read(gate[0], &release, 1) == 1);
        _exit(7);
    }
    close(output[1]);
    close(gate[0]);
    char byte;
    assert(read(output[0], &byte, 0) == 0);
    ssize_t got = read(output[0], &byte, 1);
    assert(enabled ? (got == -1 && errno == EAGAIN) : got == 0);

    // The child is still blocked on its gate. A regular file/socket must
    // nevertheless report EOF normally, not be gated on that child.
    int file = open(argv[1], O_RDONLY);
    assert(file >= 0 && read(file, &byte, 1) == 0 && close(file) == 0);
    int sockets[2];
    assert(socketpair(AF_UNIX, SOCK_STREAM, 0, sockets) == 0);
    assert(close(sockets[1]) == 0);
    assert(read(sockets[0], &byte, 1) == 0 && close(sockets[0]) == 0);

    assert(write(gate[1], "x", 1) == 1 && close(gate[1]) == 0);
    siginfo_t info;
    memset(&info, 0, sizeof(info));
    assert(waitid(P_PID, child, &info, WEXITED | WNOWAIT) == 0);
    assert(info.si_pid == child && info.si_code == CLD_EXITED && info.si_status == 7);
    // Exit status is now ready, but the normal process callback has not
    // reaped it yet. EOF must still wait and WNOWAIT must preserve status.
    got = read(output[0], &byte, 1);
    assert(enabled ? (got == -1 && errno == EAGAIN) : got == 0);
    int status;
    assert(waitpid(child, &status, 0) == child);
    assert(WIFEXITED(status) && WEXITSTATUS(status) == 7);

    // A different exec remains alive. Its status must not gate this pipe:
    // the first child's callback has already recorded its real exit status.
    int other_gate[2];
    assert(pipe2(other_gate, O_CLOEXEC) == 0);
    pid_t other = fork();
    assert(other >= 0);
    if (!other) {
        close(other_gate[1]);
        assert(read(other_gate[0], &byte, 1) == 1);
        _exit(9);
    }
    assert(close(other_gate[0]) == 0);
    assert(read(output[0], &byte, 1) == 0 && close(output[0]) == 0);
    assert(write(other_gate[1], "x", 1) == 1 && close(other_gate[1]) == 0);
    assert(waitpid(other, &status, 0) == other);
    assert(WIFEXITED(status) && WEXITSTATUS(status) == 9);
    return 0;
}
"#;

#[test]
fn pipe_eof_waits_for_real_child_reaping_only_when_enabled() {
    let fixture = Fixture(unique_socket_path("rpcd-exec").with_extension("d"));
    fs::create_dir(&fixture.0).unwrap();
    let source = fixture.0.join("probe.c");
    let binary = fixture.0.join("probe");
    let empty = fixture.0.join("empty");
    fs::write(&source, PROBE).unwrap();
    fs::write(&empty, []).unwrap();
    let cc = std::env::var_os("CC").unwrap_or_else(|| "cc".into());
    let output = Command::new(cc)
        .args(["-Wall", "-Wextra", "-Werror"])
        .arg(&source)
        .arg("-o")
        .arg(&binary)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    for enabled in [false, true] {
        let mut command = Command::new(&binary);
        command
            .arg(&empty)
            .arg(if enabled { "enabled" } else { "disabled" })
            .env("LD_PRELOAD", env!("UBUS_TESTKIT_RPCD_EXEC_ORDER"));
        if enabled {
            command.env("UBUS_TESTKIT_RPCD_EXEC_WAIT_EOF", "1");
        } else {
            command.env_remove("UBUS_TESTKIT_RPCD_EXEC_WAIT_EOF");
        }
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "enabled={enabled}, status={}, stderr={}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
