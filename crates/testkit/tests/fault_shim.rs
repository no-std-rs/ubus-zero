//! Faults target accepted client sockets, preserving signals and fixture I/O.
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
#include <string.h>
#include <sys/socket.h>
#include <sys/uio.h>
#include <unistd.h>

static ssize_t transmit(int operation, int fd) {
    struct iovec iov[] = {{ .iov_base = "abc", .iov_len = 3 },
                          { .iov_base = "def", .iov_len = 3 }};
    struct msghdr message = { .msg_iov = iov, .msg_iovlen = 2 };
    switch (operation) {
    case 0: return write(fd, "abcdef", 6);
    case 1: return send(fd, "abcdef", 6, 0);
    case 2: return writev(fd, iov, 2);
    case 3: return sendmsg(fd, &message, 0);
    default: assert(0); return -1;
    }
}

static void drain(int fd, size_t expected) {
    char bytes[6];
    size_t received = 0;
    while (received < expected) {
        ssize_t n = read(fd, bytes + received, expected - received);
        assert(n > 0);
        received += n;
    }
    assert(!memcmp(bytes, "abcdef", expected));
}

static void mark_client(int fd) {
    struct ucred credentials;
    socklen_t size = sizeof(credentials);
    assert(getsockopt(fd, SOL_SOCKET, SO_PEERCRED, &credentials, &size) == 0);
}

static void unaffected(int sockets[2], int pipes[2], int file) {
    for (int operation = 0; operation < 4; operation++) {
        assert(transmit(operation, sockets[0]) == 6);
        drain(sockets[1], 6);
    }
    // These include the self-pipe write and stderr writev entry points.
    for (int operation = 0; operation <= 2; operation += 2) {
        assert(transmit(operation, pipes[1]) == 6);
        drain(pipes[0], 6);
        assert(transmit(operation, file) == 6);
    }
}

int main(int argc, char **argv) {
    assert(argc == 3);
    int interrupt = !strcmp(argv[2], "eintr");
    int cap = !strcmp(argv[2], "cap");
    int stall = !strcmp(argv[2], "stall");
    assert(interrupt || cap || stall);
    int marked[2], ordinary[2], pipes[2];
    assert(socketpair(AF_UNIX, SOCK_STREAM, 0, marked) == 0);
    assert(socketpair(AF_UNIX, SOCK_STREAM, 0, ordinary) == 0);
    assert(pipe(pipes) == 0);
    int file = open(argv[1], O_CREAT | O_RDWR | O_TRUNC, 0600);
    assert(file >= 0);
    mark_client(marked[0]);

    for (int operation = 0; operation < 4; operation++) {
        unaffected(ordinary, pipes, file);
        if (stall && operation != 0) {
            errno = 0;
            assert(transmit(operation, marked[0]) == -1 && errno == EAGAIN);
            continue;
        }
        size_t expected = cap ? 2 : stall ? 3 : 6;
        assert(transmit(operation, marked[0]) == (ssize_t) expected);
        drain(marked[1], expected);
        unaffected(ordinary, pipes, file);
        errno = 0;
        if (interrupt) {
            // Unmarked operations must not advance the every-second counter.
            assert(transmit(operation, marked[0]) == -1 && errno == EINTR);
        } else if (stall) {
            assert(transmit(operation, marked[0]) == -1 && errno == EAGAIN);
        }
    }

    // close must clear both the client marker and exhausted stall budget.
    int reused = marked[0];
    assert(close(reused) == 0);
    int replacement = open(argv[1], O_WRONLY | O_APPEND);
    assert(replacement == reused);
    assert(transmit(0, replacement) == 6);
    assert(transmit(2, replacement) == 6);
    assert(close(replacement) == 0);
    int recycled[2];
    assert(socketpair(AF_UNIX, SOCK_STREAM, 0, recycled) == 0);
    assert(recycled[0] == reused);
    for (int operation = 0; operation < 4; operation++) {
        assert(transmit(operation, recycled[0]) == 6);
        drain(recycled[1], 6);
    }
    mark_client(recycled[0]);
    if (interrupt) {
        assert(transmit(0, recycled[0]) == 6);
        drain(recycled[1], 6);
        errno = 0;
        assert(transmit(0, recycled[0]) == -1 && errno == EINTR);
    } else if (cap) {
        assert(transmit(0, recycled[0]) == 2);
        drain(recycled[1], 2);
    } else {
        // The second accepted client is not the configured stall target.
        assert(transmit(0, recycled[0]) == 6);
        drain(recycled[1], 6);
    }
    return 0;
}
"#;

#[test]
fn tx_faults_only_affect_marked_client_sockets_and_clear_on_close() {
    let fixture = Fixture(unique_socket_path("fault-scope").with_extension("d"));
    fs::create_dir(&fixture.0).unwrap();
    let source = fixture.0.join("probe.c");
    let binary = fixture.0.join("probe");
    fs::write(&source, PROBE).unwrap();
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
    for profile in ["eintr", "cap", "stall"] {
        let mut command = Command::new(&binary);
        command
            .arg(fixture.0.join(profile))
            .arg(profile)
            .env("LD_PRELOAD", env!("UBUS_TESTKIT_FAULT_SO"));
        for control in [
            "UBUS_TESTKIT_EINTR_EVERY",
            "UBUS_TESTKIT_MAX_WRITE",
            "UBUS_TESTKIT_RX_EINTR_EVERY",
            "UBUS_TESTKIT_TX_STALL_CONN",
            "UBUS_TESTKIT_TX_STALL_AFTER_BYTES",
        ] {
            command.env_remove(control);
        }
        match profile {
            "eintr" => {
                command.env("UBUS_TESTKIT_EINTR_EVERY", "2");
            }
            "cap" => {
                command.env("UBUS_TESTKIT_MAX_WRITE", "2");
            }
            "stall" => {
                command
                    .env("UBUS_TESTKIT_TX_STALL_CONN", "0")
                    .env("UBUS_TESTKIT_TX_STALL_AFTER_BYTES", "3");
            }
            _ => unreachable!(),
        }
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "profile={profile} status={} stderr={}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
