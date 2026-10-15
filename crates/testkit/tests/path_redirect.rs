//! Exercise the actual libc entry points used by rpcd and libuci in a child.
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
#include <dlfcn.h>
#include <fcntl.h>
#include <limits.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <unistd.h>

int main(int argc, char **argv) {
    assert(argc == 2);
    char actual[PATH_MAX], canonical[PATH_MAX], link[PATH_MAX];
    const char *logical = "/ubus-testkit-fixture";
    char path[PATH_MAX];
    snprintf(path, sizeof(path), "%s/config", logical);
    FILE *f = fopen(path, "w");
    assert(f);
    assert(fputs("private config\n", f) >= 0);
    assert(fclose(f) == 0);
    f = fopen64(path, "r");
    assert(f && fgets(actual, sizeof(actual), f));
    assert(strcmp(actual, "private config\n") == 0);
    assert(fclose(f) == 0);
    assert(chmod(path, 0640) == 0);
    assert(fchmodat(AT_FDCWD, path, 0600, 0) == 0);
    snprintf(actual, sizeof(actual), "%s/config", argv[1]);
    assert(realpath(path, canonical));
    assert(strcmp(actual, canonical) == 0);

    assert(mkdir("/ubus-testkit-fixture/dir", 0700) == 0);
    assert(mkdirat(AT_FDCWD, "/ubus-testkit-fixture/dir/nested", 0700) == 0);
    assert(rmdir("/ubus-testkit-fixture/dir/nested") == 0);
    assert(rmdir("/ubus-testkit-fixture/dir") == 0);

    char temp[] = "/ubus-testkit-fixture/.tmpXXXXXX";
    int fd = mkstemp(temp);
    assert(fd >= 0 && write(fd, "committed\n", 10) == 10);
    assert(close(fd) == 0);
    assert(strncmp(temp, logical, strlen(logical)) == 0);
    assert(chmod(temp, 0600) == 0);
    assert(rename(temp, path) == 0);
    f = fopen(path, "r");
    assert(f && fgets(actual, sizeof(actual), f));
    assert(strcmp(actual, "committed\n") == 0);
    assert(fclose(f) == 0);

    char suffixed[] = "/ubus-testkit-fixture/.tmpXXXXXX.uci";
    fd = mkstemps64(suffixed, 4);
    assert(fd >= 0 && close(fd) == 0);
    assert(renameat(AT_FDCWD, suffixed, AT_FDCWD,
                    "/ubus-testkit-fixture/moved") == 0);
    assert(unlinkat(AT_FDCWD, "/ubus-testkit-fixture/moved", 0) == 0);
    char temp64[] = "/ubus-testkit-fixture/.tmpXXXXXX";
    fd = mkstemp64(temp64);
    assert(fd >= 0 && close(fd) == 0 && unlink(temp64) == 0);

    snprintf(path, sizeof(path), "%s/link", logical);
    ssize_t size = readlink(path, link, sizeof(link) - 1);
    assert(size > 0);
    link[size] = '\0';
    assert(strcmp(link, "config") == 0);
    assert(readlinkat(AT_FDCWD, path, link, sizeof(link)) == size);
    assert(unlink(path) == 0);
    assert(!fopen("/ubus-testkit-fixture-sibling/config", "r"));

    void *plugin = dlopen("/ubus-testkit-fixture/plugin.so", RTLD_NOW);
    assert(plugin);
    int (*answer)(void) = dlsym(plugin, "answer");
    assert(answer && answer() == 42);
    assert(dlopen(NULL, RTLD_NOW));
    assert(dlclose(plugin) == 0);
    return 0;
}
"#;

#[test]
fn rpcd_and_uci_operations_stay_inside_redirected_fixture() {
    let fixture = Fixture(unique_socket_path("redirect").with_extension("d"));
    // A longer actual prefix catches wrappers that copy a rewritten mkstemp
    // template over the caller's much shorter logical buffer.
    let actual = fixture.0.join("isolated-config-and-plugin-directory");
    fs::create_dir_all(&actual).unwrap();
    let source = fixture.0.join("probe.c");
    let binary = fixture.0.join("probe");
    fs::write(&source, PROBE).unwrap();
    let cc = std::env::var_os("CC").unwrap_or_else(|| "cc".into());
    let output = Command::new(&cc)
        .args(["-Wall", "-Wextra", "-Werror"])
        .arg(&source)
        .args(["-o"])
        .arg(&binary)
        .arg("-ldl")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let plugin_source = fixture.0.join("plugin.c");
    fs::write(&plugin_source, "int answer(void) { return 42; }\n").unwrap();
    let output = Command::new(&cc)
        .args(["-shared", "-fPIC"])
        .arg(&plugin_source)
        .arg("-o")
        .arg(actual.join("plugin.so"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    std::os::unix::fs::symlink("config", actual.join("link")).unwrap();
    let output = Command::new(&binary)
        .arg(&actual)
        .env("LD_PRELOAD", env!("UBUS_TESTKIT_PATH_REDIRECT"))
        .env(
            "UBUS_TESTKIT_PATH_MAP",
            format!("/ubus-testkit-fixture={}", actual.display()),
        )
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "status={} stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read_to_string(actual.join("config")).unwrap(),
        "committed\n"
    );
    assert!(!actual.join("moved").exists());
}
