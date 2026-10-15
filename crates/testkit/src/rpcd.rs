//! `RpcdHarness` — run upstream `rpcd` against a test broker with its
//! ACL, plugin, configuration, and runtime paths redirected into a temporary
//! fixture directory.

use std::ffi::OsString;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStderr, Command, Stdio};
use std::time::{Duration, Instant};

use crate::path::unique_socket_path;
use crate::ubus_cli::UbusCli;
use crate::ubusd::append_preload;

/// Password (plaintext) both seeded logins accept.
pub const RPCD_TEST_PASSWORD: &str = "test";
/// Username whose login grants only the acl.d `read` scope.
pub const RPCD_LOGIN_READONLY: &str = "ro";
/// Username whose login grants both `read` and `write` scopes.
pub const RPCD_LOGIN_READWRITE: &str = "rw";

// crypt("test", "$1$abcd1234$") — glibc MD5 crypt.
const RPCD_PASSWORD_HASH: &str = "$1$abcd1234$AEHhKyg2d3p9dvDNDtQmA/";

/// Spawned upstream `rpcd` attached to an existing ubus broker. `Drop` kills
/// the daemon and removes all path-redirected fixture files.
pub struct RpcdHarness {
    child: Child,
    stderr: Option<ChildStderr>,
    dir: PathBuf,
    socket: PathBuf,
    reaped: bool,
}

impl RpcdHarness {
    /// Start `rpcd -s <broker_socket>` with a generated login configuration
    /// and the supplied ACL files. `Ok(None)` means no viable `rpcd` binary
    /// was found (or it cannot execute on this host).
    ///
    /// Lookup: `$UBUS_ZERO_RPCD` → `.dep/bin/rpcd` (relative to
    /// `CARGO_MANIFEST_DIR` and one-up) → `rpcd` on `$PATH`.
    pub fn start(broker_socket: &Path, acl_files: &[(&str, &str)]) -> io::Result<Option<Self>> {
        let Some(binary) = locate_rpcd() else {
            return Ok(None);
        };

        let dir = unique_socket_path("rpcd").with_extension("d");
        fs::create_dir(&dir)?;
        let result = Self::start_in_dir(&binary, broker_socket, acl_files, dir.clone());
        if !matches!(result.as_ref(), Ok(Some(_))) {
            let _ = fs::remove_dir_all(&dir);
        }
        result
    }

    fn start_in_dir(
        binary: &OsString,
        broker_socket: &Path,
        acl_files: &[(&str, &str)],
        dir: PathBuf,
    ) -> io::Result<Option<Self>> {
        let acld = dir.join("acl.d");
        let plugins = dir.join("plugins");
        let config = dir.join("config");
        let run = dir.join("run");
        let files = dir.join("files");
        let delta = dir.join("delta");
        let state = dir.join("state");
        let override_dir = dir.join("override");
        for path in [
            &acld,
            &plugins,
            &config,
            &run,
            &files,
            &delta,
            &state,
            &override_dir,
        ] {
            fs::create_dir(path)?;
        }
        for (name, contents) in acl_files {
            fs::write(acld.join(name), contents)?;
        }
        fs::write(config.join("rpcd"), login_config())?;

        // Keep executable script plugins separate from dlopen libraries: the
        // file.so fixture is a shared object, not an executable rpcd plugin.
        let exec_plugins = dir.join("exec-plugins");
        fs::create_dir(&exec_plugins)?;
        if let Some(plugin) = locate_file_plugin(binary) {
            fs::copy(plugin, plugins.join("file.so"))?;
        }

        let path_map = format!(
            "/usr/local/share/rpcd/acl.d={},/usr/local/lib/rpcd={},/usr/local/libexec/rpcd={},/etc/config={},/var/run/rpcd={},/tmp/.uci={},/var/state={},/var/run/uci={}",
            acld.display(),
            plugins.display(),
            exec_plugins.display(),
            config.display(),
            run.display(),
            delta.display(),
            state.display(),
            override_dir.display(),
        );
        let mut preload = std::env::var_os("LD_PRELOAD").unwrap_or_default();
        append_preload(&mut preload, env!("UBUS_TESTKIT_PATH_REDIRECT"));
        append_preload(&mut preload, env!("UBUS_TESTKIT_RPCD_EXEC_ORDER"));

        let mut child = Command::new(binary)
            .arg("-s")
            .arg(broker_socket)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .env("UBUS_TESTKIT_PATH_MAP", path_map)
            .env("UBUS_TESTKIT_RPCD_EXEC_WAIT_EOF", "1")
            .env("LD_PRELOAD", preload)
            .spawn()?;
        let mut stderr = child.stderr.take();

        if let Some(cli) = UbusCli::locate() {
            let deadline = Instant::now() + Duration::from_secs(5);
            while Instant::now() < deadline {
                if let Some(status) = child.try_wait()? {
                    return early_exit(status, &mut stderr);
                }
                if let Ok(output) = cli.run(broker_socket, &["list"]) {
                    let objects = String::from_utf8_lossy(&output.stdout);
                    let ready = ["session", "uci"]
                        .into_iter()
                        .chain(plugins.join("file.so").is_file().then_some("file"))
                        .all(|object| objects.lines().any(|line| line == object));
                    if output.status.success() && ready {
                        return Ok(Some(Self {
                            child,
                            stderr,
                            dir,
                            socket: broker_socket.to_path_buf(),
                            reaped: false,
                        }));
                    }
                }
                std::thread::sleep(Duration::from_millis(10));
            }

            if let Some(status) = child.try_wait()? {
                return early_exit(status, &mut stderr);
            }

            let _ = child.kill();
            let _ = child.wait();
            let stderr = drain_stderr(&mut stderr);
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("rpcd did not register its fixture objects within 5 s; stderr: {stderr}"),
            ));
        }

        // A fresh checkout may have rpcd but no upstream `ubus` CLI. Give
        // rpcd its empirically sufficient registration window in that case;
        // callers will still exercise its session object immediately after.
        std::thread::sleep(Duration::from_millis(800));
        if let Some(status) = child.try_wait()? {
            return early_exit(status, &mut stderr);
        }
        Ok(Some(Self {
            child,
            stderr,
            dir,
            socket: broker_socket.to_path_buf(),
            reaped: false,
        }))
    }

    /// Socket of the broker this daemon is attached to.
    pub fn socket(&self) -> &Path {
        &self.socket
    }

    /// Private configuration directory replacing `/etc/config` in this child.
    /// Seed packages here before invoking the rpcd `uci` object. No host UCI
    /// configuration is consulted or modified.
    pub fn config_dir(&self) -> PathBuf {
        self.dir.join("config")
    }

    /// Private scratch directory for file-plugin operations and ACL fixtures.
    pub fn files_dir(&self) -> PathBuf {
        self.dir.join("files")
    }

    /// Private ACL directory. Login loads these files when it creates a
    /// session, allowing a test to grant access to its freshly allocated paths.
    pub fn acl_dir(&self) -> PathBuf {
        self.dir.join("acl.d")
    }

    /// Whether the exported upstream `file.so` plugin was installed for this
    /// child. Callers must report an unavailable fixture through `skip::or_skip`
    /// so strict compatibility runs cannot silently omit the file scenarios.
    pub fn has_file_plugin(&self) -> bool {
        self.dir.join("plugins/file.so").is_file()
    }

    /// Stop and reap the rpcd child. Dropping the consumed harness then removes
    /// its temporary redirected directories.
    pub fn shutdown(mut self) {
        self.stop();
    }

    fn stop(&mut self) {
        if !self.reaped {
            let _ = self.child.kill();
            let _ = self.child.wait();
            self.reaped = true;
        }
    }
}

impl Drop for RpcdHarness {
    fn drop(&mut self) {
        if !self.reaped {
            // A daemon that leaves while its harness is still in scope is
            // unexpected; retain its diagnostics instead of silently treating
            // it as an ordinary shutdown.
            match self.child.try_wait() {
                Ok(Some(status)) => {
                    let stderr = drain_stderr(&mut self.stderr);
                    eprintln!("rpcd exited mid-test ({status}); stderr: {stderr}");
                    self.reaped = true;
                }
                _ => self.stop(),
            }
        }
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn login_config() -> String {
    format!(
        "config login\n\
\toption username '{RPCD_LOGIN_READONLY}'\n\
\toption password '{RPCD_PASSWORD_HASH}'\n\
\tlist read '*'\n\n\
config login\n\
\toption username '{RPCD_LOGIN_READWRITE}'\n\
\toption password '{RPCD_PASSWORD_HASH}'\n\
\tlist read '*'\n\
\tlist write '*'\n"
    )
}

fn early_exit(
    status: std::process::ExitStatus,
    stderr: &mut Option<ChildStderr>,
) -> io::Result<Option<RpcdHarness>> {
    let stderr_text = drain_stderr(stderr);
    let exec_format_error = stderr_text.contains("Exec format error")
        || stderr_text.contains("cannot execute binary file");
    if status.code() == Some(126) || exec_format_error {
        return Ok(None);
    }
    Err(io::Error::other(format!(
        "rpcd exited early ({status}); stderr: {stderr_text}"
    )))
}

fn drain_stderr(handle: &mut Option<ChildStderr>) -> String {
    let Some(mut stderr) = handle.take() else {
        return String::from("<unavailable>");
    };
    let mut buf = String::new();
    if let Err(e) = stderr.read_to_string(&mut buf) {
        buf.push_str(&format!("\n<stderr read failed: {e}>"));
    }
    if buf.is_empty() {
        String::from("<empty>")
    } else {
        buf
    }
}

fn locate_rpcd() -> Option<OsString> {
    if let Some(path) = std::env::var_os("UBUS_ZERO_RPCD") {
        if !path.is_empty() && Path::new(&path).is_file() {
            return Some(path);
        }
    }
    for relative in [".dep/bin/rpcd", "../.dep/bin/rpcd", "../../.dep/bin/rpcd"] {
        let candidate = Path::new(env!("CARGO_MANIFEST_DIR")).join(relative);
        if candidate.is_file() {
            return Some(candidate.into_os_string());
        }
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join("rpcd"))
        .find(|candidate| candidate.is_file())
        .map(PathBuf::into_os_string)
}

fn locate_file_plugin(binary: &OsString) -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("UBUS_ZERO_RPCD_PLUGINS") {
        let candidate = PathBuf::from(dir).join("file.so");
        return candidate.is_file().then_some(candidate);
    }
    let adjacent = Path::new(binary).parent()?.join("rpcd-plugins/file.so");
    if adjacent.is_file() {
        return Some(adjacent);
    }
    [
        ".dep/bin/rpcd-plugins/file.so",
        "../.dep/bin/rpcd-plugins/file.so",
        "../../.dep/bin/rpcd-plugins/file.so",
    ]
    .into_iter()
    .map(|relative| Path::new(env!("CARGO_MANIFEST_DIR")).join(relative))
    .find(|candidate| candidate.is_file())
}
