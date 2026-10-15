//! Keep old /proc and new SO_PEERGROUPS reference brokers on the same fixture.
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
#include <string.h>
#include <sys/socket.h>
#include <sys/syscall.h>
#include <unistd.h>

int main(int argc, char **argv) {
    assert(argc == 2);
    int sockets[2];
    assert(socketpair(AF_UNIX, SOCK_STREAM, 0, sockets) == 0);
    struct ucred real_cred, fixture_cred;
    socklen_t length = sizeof(real_cred);
    assert(syscall(SYS_getsockopt, sockets[0], SOL_SOCKET, SO_PEERCRED, &real_cred, &length) == 0);
    length = sizeof(fixture_cred);
    assert(getsockopt(sockets[0], SOL_SOCKET, SO_PEERCRED, &fixture_cred, &length) == 0);
    assert(length == sizeof(real_cred) && !memcmp(&real_cred, &fixture_cred, length));
    int type;
    length = sizeof(type);
    assert(getsockopt(sockets[0], SOL_SOCKET, SO_TYPE, &type, &length) == 0 && type == SOCK_STREAM);

    gid_t groups[80];
    if (!strcmp(argv[1], "unset")) {
        gid_t actual[80];
        socklen_t actual_length = sizeof(actual);
        int actual_status = syscall(SYS_getsockopt, sockets[0], SOL_SOCKET, SO_PEERGROUPS, actual, &actual_length);
        int actual_errno = errno;
        length = sizeof(groups);
        int status = getsockopt(sockets[0], SOL_SOCKET, SO_PEERGROUPS, groups, &length);
        assert(status == actual_status && length == actual_length);
        assert(status ? errno == actual_errno : !memcmp(groups, actual, length));
        return 0;
    }
    if (!strcmp(argv[1], "failed")) {
        length = sizeof(groups);
        assert(getsockopt(sockets[0], SOL_SOCKET, SO_PEERGROUPS, groups, &length) == -1 && errno == ENODATA);
        assert(length == sizeof(groups));
        return 0;
    }
    size_t count = !strcmp(argv[1], "empty") ? 0 : !strcmp(argv[1], "many") ? 65 : 3;
    socklen_t required = count * sizeof(gid_t);
    memset(groups, 0xa5, sizeof(groups));
    if (required) {
        length = required - 1;
        assert(getsockopt(sockets[0], SOL_SOCKET, SO_PEERGROUPS, groups, &length) == -1 && errno == ERANGE);
        assert(length == required && groups[0] == (gid_t)0xa5a5a5a5);
        length = 0;
        assert(getsockopt(sockets[0], SOL_SOCKET, SO_PEERGROUPS, NULL, &length) == -1 && errno == ERANGE);
        assert(length == required);
        length = sizeof(groups);
        assert(getsockopt(sockets[0], SOL_SOCKET, SO_PEERGROUPS, NULL, &length) == -1 && errno == EFAULT);
        assert(length == sizeof(groups));
    } else {
        length = 0;
        assert(getsockopt(sockets[0], SOL_SOCKET, SO_PEERGROUPS, NULL, &length) == 0 && !length);
    }
    length = sizeof(groups);
    assert(getsockopt(sockets[0], SOL_SOCKET, SO_PEERGROUPS, groups, &length) == 0 && length == required);
    for (size_t i = 0; i < count; i++) {
        gid_t expected = count == 65 ? (i == 64 ? 3000 : 2000) : (i == 0 ? 2000 : 3000);
        assert(groups[i] == expected);
    }
    if (count == 65) {
        length = 64 * sizeof(gid_t);
        assert(getsockopt(sockets[0], SOL_SOCKET, SO_PEERGROUPS, groups, &length) == -1 && errno == ERANGE);
        assert(length == 65 * sizeof(gid_t));
    }
    assert(getsockopt(sockets[0], SOL_SOCKET, SO_PEERGROUPS, groups, NULL) == -1 && errno == EFAULT);
    length = sizeof(groups);
    assert(getsockopt(-1, SOL_SOCKET, SO_PEERGROUPS, groups, &length) == -1 && errno == EBADF);
    return 0;
}
"#;

#[test]
fn peer_groups_fixture_preserves_lengths_errors_and_other_socket_options() {
    let fixture = Fixture(unique_socket_path("peer-groups").with_extension("d"));
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
    let many = format!("{}3000", "2000 ".repeat(64));
    for (profile, extra) in [
        ("unset", None),
        ("empty", Some("")),
        ("prefix", Some("2000 3000 3000 invalid 9000")),
        ("many", Some(many.as_str())),
        ("failed", Some("2000")),
    ] {
        let mut command = Command::new(&binary);
        command
            .arg(profile)
            .env("LD_PRELOAD", env!("UBUS_TESTKIT_NSS_SPOOF"))
            .env_remove("UBUS_TESTKIT_NSS_EXTRA_GIDS")
            .env_remove("UBUS_TESTKIT_NSS_PROC_FAIL");
        if let Some(extra) = extra {
            command.env("UBUS_TESTKIT_NSS_EXTRA_GIDS", extra);
        }
        if profile == "failed" {
            command.env("UBUS_TESTKIT_NSS_PROC_FAIL", "1");
        }
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "profile={profile}, status={} stderr={}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
