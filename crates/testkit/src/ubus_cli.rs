//! `UbusCli` — drive the upstream `ubus` CLI binary as a reference
//! client against a running broker. The CLI links libubus +
//! blobmsg_json, so it exercises the full JSON → blobmsg args → wire →
//! reply blobmsg → JSON round-trip that operators and OpenWrt scripts
//! actually run; our own client never touches that libubox JSON layer.

use std::io;
use std::path::Path;
use std::process::{Child, Command, Output, Stdio};

/// Located `ubus` binary. `Drop`-free: it just remembers the resolved
/// path and shells out per [`UbusCli::run`].
pub struct UbusCli {
    binary: std::ffi::OsString,
}

impl UbusCli {
    /// Resolve the `ubus` CLI. `None` when no binary is found — callers
    /// silent-skip, mirroring [`crate::UbusdHarness::start_upstream`] on
    /// a fresh clone that hasn't run `tools/ubusd-builder/build.sh`.
    ///
    /// Lookup: `$UBUS_ZERO_CLI` → `.dep/bin/ubus` (rel. to
    /// `CARGO_MANIFEST_DIR` and one-up) → `ubus` on `$PATH`.
    pub fn locate() -> Option<Self> {
        locate_ubus_cli(&StdEnv).map(|binary| Self { binary })
    }

    /// Run `<bin> -s <socket> <args...>` and collect its `Output`. The
    /// rpath=$ORIGIN in the built binary resolves its .so deps next to
    /// it, so no `LD_LIBRARY_PATH` juggling is needed here.
    pub fn run(&self, socket: &Path, args: &[&str]) -> io::Result<Output> {
        Command::new(&self.binary)
            .arg("-s")
            .arg(socket)
            .args(args)
            .output()
    }

    /// Spawn `<bin> -s <socket> <args...>` as a background child with
    /// stdout/stderr piped, for long-running verbs like `listen`. The
    /// caller owns the [`Child`] and must terminate it by PID
    /// (`Child::kill`) — never `pkill -f`, which would match and kill the
    /// invoking test shell.
    pub fn spawn(&self, socket: &Path, args: &[&str]) -> io::Result<Child> {
        Command::new(&self.binary)
            .arg("-s")
            .arg(socket)
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
    }
}

/// Env + filesystem facade so [`locate_ubus_cli`] is testable without
/// mutating process-global state (parallel tests would race on
/// `set_var`). Mirrors `ubusd::LocateEnv` but scoped to the CLI's
/// smaller lookup surface.
trait CliLocateEnv {
    fn ubus_zero_cli(&self) -> Option<std::ffi::OsString>;
    fn manifest_dir(&self) -> Option<std::ffi::OsString>;
    fn path(&self) -> Option<std::ffi::OsString>;
    fn is_file(&self, p: &Path) -> bool;
}

struct StdEnv;

impl CliLocateEnv for StdEnv {
    fn ubus_zero_cli(&self) -> Option<std::ffi::OsString> {
        std::env::var_os("UBUS_ZERO_CLI")
    }
    fn manifest_dir(&self) -> Option<std::ffi::OsString> {
        // `env!` (not `var_os`): pin the value into the binary so
        // discovery works even outside `cargo test`.
        Some(std::ffi::OsString::from(env!("CARGO_MANIFEST_DIR")))
    }
    fn path(&self) -> Option<std::ffi::OsString> {
        std::env::var_os("PATH")
    }
    fn is_file(&self, p: &Path) -> bool {
        p.is_file()
    }
}

fn locate_ubus_cli<E: CliLocateEnv>(env: &E) -> Option<std::ffi::OsString> {
    if let Some(p) = env.ubus_zero_cli() {
        if !p.is_empty() {
            let candidate = Path::new(&p);
            if env.is_file(candidate) {
                return Some(p);
            }
        }
    }
    // CARGO_MANIFEST_DIR is `crates/testkit/`; the workspace root is two
    // `..` components up. Keep the older fallbacks for standalone use.
    if let Some(manifest) = env.manifest_dir() {
        for relative in [".dep/bin/ubus", "../.dep/bin/ubus", "../../.dep/bin/ubus"] {
            let candidate = Path::new(&manifest).join(relative);
            if env.is_file(&candidate) {
                return Some(candidate.into_os_string());
            }
        }
    }
    let path = env.path()?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join("ubus");
        if env.is_file(&candidate) {
            return Some(candidate.into_os_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::ffi::OsString;
    use std::path::PathBuf;

    struct FakeEnv {
        cli: Option<OsString>,
        manifest: Option<OsString>,
        path: Option<OsString>,
        existing: HashSet<PathBuf>,
    }

    impl CliLocateEnv for FakeEnv {
        fn ubus_zero_cli(&self) -> Option<OsString> {
            self.cli.clone()
        }
        fn manifest_dir(&self) -> Option<OsString> {
            self.manifest.clone()
        }
        fn path(&self) -> Option<OsString> {
            self.path.clone()
        }
        fn is_file(&self, p: &Path) -> bool {
            self.existing.contains(p)
        }
    }

    fn env(
        cli: Option<&str>,
        manifest: Option<&str>,
        path: Option<&str>,
        existing: &[&str],
    ) -> FakeEnv {
        FakeEnv {
            cli: cli.map(OsString::from),
            manifest: manifest.map(OsString::from),
            path: path.map(OsString::from),
            existing: existing.iter().map(PathBuf::from).collect(),
        }
    }

    #[test]
    fn ubus_zero_cli_wins_over_dep_bin() {
        let e = env(
            Some("/explicit/ubus"),
            Some("/repo/crates/testkit"),
            None,
            &["/explicit/ubus", "/repo/crates/testkit/.dep/bin/ubus"],
        );
        assert_eq!(locate_ubus_cli(&e), Some(OsString::from("/explicit/ubus")));
    }

    #[test]
    fn ubus_zero_cli_pointing_at_missing_file_falls_through() {
        let e = env(
            Some("/missing/ubus"),
            Some("/repo/crates/testkit"),
            None,
            &["/repo/crates/testkit/.dep/bin/ubus"],
        );
        assert_eq!(
            locate_ubus_cli(&e),
            Some(OsString::from("/repo/crates/testkit/.dep/bin/ubus")),
        );
    }

    #[test]
    fn empty_ubus_zero_cli_falls_through_to_path() {
        let e = env(
            Some(""),
            None,
            Some("/usr/local/bin:/usr/bin"),
            &["/usr/bin/ubus"],
        );
        assert_eq!(locate_ubus_cli(&e), Some(OsString::from("/usr/bin/ubus")));
    }

    #[test]
    fn dep_bin_two_up_is_resolved_from_nested_workspace_member() {
        let e = env(
            None,
            Some("/repo/crates/testkit"),
            None,
            &["/repo/crates/testkit/../../.dep/bin/ubus"],
        );
        assert_eq!(
            locate_ubus_cli(&e),
            Some(OsString::from("/repo/crates/testkit/../../.dep/bin/ubus")),
        );
    }

    #[test]
    fn returns_none_when_nothing_is_reachable() {
        let e = env(None, Some("/repo"), Some("/usr/bin"), &[]);
        assert_eq!(locate_ubus_cli(&e), None);
    }
}
