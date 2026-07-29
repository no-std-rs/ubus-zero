//! `UciCli` — drive the upstream `uci` CLI binary as a reference
//! parser and exporter for OpenWrt UCI configuration files.

use std::ffi::OsString;
use std::io;
use std::path::Path;
use std::process::{Command, Output};

/// A section emitted by `uci export`, preserving its original order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UciSection {
    pub kind: String,
    pub name: Option<String>,
    pub entries: Vec<(String, UciEntry)>,
}

/// An entry emitted by `uci export`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UciEntry {
    Option(String),
    List(Vec<String>),
}

/// Located `uci` binary. `Drop`-free: it just remembers the resolved
/// path and shells out per [`UciCli::export`].
pub struct UciCli {
    binary: OsString,
}

impl UciCli {
    /// Resolve the `uci` CLI. `None` when no binary is found — callers
    /// can silent-skip reference checks on a fresh clone that hasn't run
    /// `tools/ubusd-builder/build.sh`.
    ///
    /// Lookup: `$UBUS_ZERO_UCI` → `.dep/bin/uci` (rel. to
    /// `CARGO_MANIFEST_DIR` and one-up) → `uci` on `$PATH`.
    pub fn locate() -> Option<Self> {
        locate_uci_cli(&StdEnv).map(|binary| Self { binary })
    }

    /// Run `<bin> -c <confdir> export <package>` and collect its
    /// `Output`. The rpath=$ORIGIN in the built binary resolves its .so
    /// deps next to it, so no `LD_LIBRARY_PATH` juggling is needed here.
    pub fn export(&self, confdir: &Path, package: &str) -> io::Result<Output> {
        Command::new(&self.binary)
            .arg("-c")
            .arg(confdir)
            .arg("export")
            .arg(package)
            .output()
    }

    /// Export a package and normalize the reference output into sections.
    pub fn export_sections(
        &self,
        confdir: &Path,
        package: &str,
    ) -> Result<Vec<UciSection>, String> {
        let output = self
            .export(confdir, package)
            .map_err(|err| format!("uci export {package:?} failed to run: {err}"))?;
        if !output.status.success() {
            return Err(format!(
                "uci export {package:?} failed with {}: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            ));
        }
        let stdout = String::from_utf8(output.stdout)
            .map_err(|err| format!("uci export {package:?} produced non-UTF-8 stdout: {err}"))?;
        parse_uci_export(&stdout)
    }
}

/// Env + filesystem facade so [`locate_uci_cli`] is testable without
/// mutating process-global state (parallel tests would race on
/// `set_var`). Mirrors `ubusd::LocateEnv` but scoped to the CLI's
/// smaller lookup surface.
trait UciLocateEnv {
    fn ubus_zero_uci(&self) -> Option<OsString>;
    fn manifest_dir(&self) -> Option<OsString>;
    fn path(&self) -> Option<OsString>;
    fn is_file(&self, p: &Path) -> bool;
}

struct StdEnv;

impl UciLocateEnv for StdEnv {
    fn ubus_zero_uci(&self) -> Option<OsString> {
        std::env::var_os("UBUS_ZERO_UCI")
    }
    fn manifest_dir(&self) -> Option<OsString> {
        // `env!` (not `var_os`): pin the value into the binary so
        // discovery works even outside `cargo test`.
        Some(OsString::from(env!("CARGO_MANIFEST_DIR")))
    }
    fn path(&self) -> Option<OsString> {
        std::env::var_os("PATH")
    }
    fn is_file(&self, p: &Path) -> bool {
        p.is_file()
    }
}

fn locate_uci_cli<E: UciLocateEnv>(env: &E) -> Option<OsString> {
    if let Some(p) = env.ubus_zero_uci() {
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
        for relative in [".dep/bin/uci", "../.dep/bin/uci", "../../.dep/bin/uci"] {
            let candidate = Path::new(&manifest).join(relative);
            if env.is_file(&candidate) {
                return Some(candidate.into_os_string());
            }
        }
    }
    let path = env.path()?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join("uci");
        if env.is_file(&candidate) {
            return Some(candidate.into_os_string());
        }
    }
    None
}

/// Normalize the strict, single-quoted dialect emitted by `uci export`.
pub fn parse_uci_export(text: &str) -> Result<Vec<UciSection>, String> {
    let mut sections = Vec::new();

    for (line_index, line) in text.lines().enumerate() {
        let line_number = line_index + 1;
        let trimmed = line.trim_start();
        if trimmed.is_empty() || trimmed.starts_with("package ") {
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("config ") {
            let (kind, rest) = unquoted_token(rest, line_number, "section kind")?;
            let rest = rest.trim_start();
            let name = if rest.is_empty() {
                None
            } else {
                let (name, remainder) = quoted_token(rest, line_number)?;
                if !remainder.trim().is_empty() {
                    return Err(format!("line {line_number}: unexpected text after config"));
                }
                Some(name)
            };
            sections.push(UciSection {
                kind: kind.to_owned(),
                name,
                entries: Vec::new(),
            });
            continue;
        }

        let (is_list, rest) = if let Some(rest) = trimmed.strip_prefix("option ") {
            (false, rest)
        } else if let Some(rest) = trimmed.strip_prefix("list ") {
            (true, rest)
        } else {
            return Err(format!("line {line_number}: unexpected export line"));
        };
        let section = sections
            .last_mut()
            .ok_or_else(|| format!("line {line_number}: entry outside a config section"))?;
        let (key, rest) = unquoted_token(rest, line_number, "entry key")?;
        let value_start = rest.trim_start();
        if value_start.len() == rest.len() {
            return Err(format!("line {line_number}: missing entry value"));
        }
        let (value, remainder) = quoted_token(value_start, line_number)?;
        if !remainder.trim().is_empty() {
            return Err(format!(
                "line {line_number}: unexpected text after entry value"
            ));
        }

        if let Some((_, entry)) = section.entries.iter_mut().find(|(name, _)| name == key) {
            match (entry, is_list) {
                (UciEntry::List(values), true) => values.push(value),
                (UciEntry::List(_), false) | (UciEntry::Option(_), true) => {
                    return Err(format!(
                        "line {line_number}: option/list collision for {key:?}"
                    ));
                }
                (UciEntry::Option(_), false) => {
                    return Err(format!("line {line_number}: duplicate option {key:?}"));
                }
            }
        } else if is_list {
            section
                .entries
                .push((key.to_owned(), UciEntry::List(vec![value])));
        } else {
            section
                .entries
                .push((key.to_owned(), UciEntry::Option(value)));
        }
    }

    Ok(sections)
}

fn unquoted_token<'a>(
    text: &'a str,
    line_number: usize,
    description: &str,
) -> Result<(&'a str, &'a str), String> {
    let split_at = text.find(char::is_whitespace).unwrap_or(text.len());
    if split_at == 0 {
        return Err(format!("line {line_number}: missing {description}"));
    }
    Ok((&text[..split_at], &text[split_at..]))
}

fn quoted_token(text: &str, line_number: usize) -> Result<(String, &str), String> {
    let Some(value) = text.strip_prefix('\'') else {
        return Err(format!("line {line_number}: expected single-quoted value"));
    };
    let mut unquoted = String::new();
    let mut index = 0;
    while index < value.len() {
        let remainder = &value[index..];
        if remainder.starts_with("'\\''") {
            unquoted.push('\'');
            index += 4;
        } else if let Some(remainder) = remainder.strip_prefix('\'') {
            return Ok((unquoted, remainder));
        } else {
            let ch = remainder
                .chars()
                .next()
                .expect("remainder is non-empty while parsing quote");
            unquoted.push(ch);
            index += ch.len_utf8();
        }
    }
    Err(format!(
        "line {line_number}: unterminated single-quoted value"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::path::PathBuf;

    struct FakeEnv {
        cli: Option<OsString>,
        manifest: Option<OsString>,
        path: Option<OsString>,
        existing: HashSet<PathBuf>,
    }

    impl UciLocateEnv for FakeEnv {
        fn ubus_zero_uci(&self) -> Option<OsString> {
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
    fn ubus_zero_uci_wins_over_dep_bin() {
        let e = env(
            Some("/explicit/uci"),
            Some("/repo/crates/testkit"),
            None,
            &["/explicit/uci", "/repo/crates/testkit/.dep/bin/uci"],
        );
        assert_eq!(locate_uci_cli(&e), Some(OsString::from("/explicit/uci")));
    }

    #[test]
    fn ubus_zero_uci_pointing_at_missing_file_falls_through() {
        let e = env(
            Some("/missing/uci"),
            Some("/repo/crates/testkit"),
            None,
            &["/repo/crates/testkit/.dep/bin/uci"],
        );
        assert_eq!(
            locate_uci_cli(&e),
            Some(OsString::from("/repo/crates/testkit/.dep/bin/uci")),
        );
    }

    #[test]
    fn empty_ubus_zero_uci_falls_through_to_path() {
        let e = env(
            Some(""),
            None,
            Some("/usr/local/bin:/usr/bin"),
            &["/usr/bin/uci"],
        );
        assert_eq!(locate_uci_cli(&e), Some(OsString::from("/usr/bin/uci")));
    }

    #[test]
    fn dep_bin_two_up_is_resolved_from_nested_workspace_member() {
        let e = env(
            None,
            Some("/repo/crates/testkit"),
            None,
            &["/repo/crates/testkit/../../.dep/bin/uci"],
        );
        assert_eq!(
            locate_uci_cli(&e),
            Some(OsString::from("/repo/crates/testkit/../../.dep/bin/uci")),
        );
    }

    #[test]
    fn returns_none_when_nothing_is_reachable() {
        let e = env(None, Some("/repo"), Some("/usr/bin"), &[]);
        assert_eq!(locate_uci_cli(&e), None);
    }

    const FIXTURE_A: &str = r#"package example

config daemon 'main'
	option state_dir '/var/lib/example'

config modem
	option device '/dev/ttyUSB2'
	option apn 'internet'
	list override_pre_pdp 'AT+X'
	list override_pre_pdp 'AT+Y'

"#;

    const FIXTURE_B: &str = r#"package example

config daemon 'main'
	option state_dir '/var/lib/example'
	option ubus_socket '/var/run/ubus/ubus.sock'

config modem 'modem0'
	option device '/dev/ttyUSB2'
	option apn 'o'\''brien'
	list override_pre_pdp 'AT+X=1,"q"'

config modem
	option reconcile '1'
	list override_post_register 'AT+Z'

"#;

    #[test]
    fn parses_fixture_a() {
        let sections = parse_uci_export(FIXTURE_A).unwrap();
        assert_eq!(sections.len(), 2);
        assert_eq!(sections[0].kind, "daemon");
        assert_eq!(sections[0].name.as_deref(), Some("main"));
        assert!(matches!(
            sections[0].entries.as_slice(),
            [(key, UciEntry::Option(value))] if key == "state_dir" && value == "/var/lib/example"
        ));

        assert_eq!(sections[1].kind, "modem");
        assert_eq!(sections[1].name, None);
        assert!(matches!(
            sections[1].entries.as_slice(),
            [
                (device, UciEntry::Option(device_value)),
                (apn, UciEntry::Option(apn_value)),
                (overrides, UciEntry::List(values)),
            ] if device == "device" && device_value == "/dev/ttyUSB2"
                && apn == "apn" && apn_value == "internet"
                && overrides == "override_pre_pdp"
                && values.iter().map(String::as_str).eq(["AT+X", "AT+Y"])
        ));
    }

    #[test]
    fn parses_fixture_b() {
        let sections = parse_uci_export(FIXTURE_B).unwrap();
        assert_eq!(sections.len(), 3);
        assert_eq!(sections[1].kind, "modem");
        assert_eq!(sections[1].name.as_deref(), Some("modem0"));
        assert!(matches!(
            sections[1].entries.as_slice(),
            [
                (_, UciEntry::Option(_)),
                (apn, UciEntry::Option(value)),
                (_, UciEntry::List(_)),
            ] if apn == "apn" && value == "o'brien"
        ));
        assert_eq!(sections[2].kind, "modem");
        assert_eq!(sections[2].name, None);
        assert!(matches!(
            sections[2].entries.as_slice(),
            [
                (reconcile, UciEntry::Option(value)),
                (overrides, UciEntry::List(values)),
            ] if reconcile == "reconcile" && value == "1"
                && overrides == "override_post_register"
                && values.iter().map(String::as_str).eq(["AT+Z"])
        ));
    }

    #[test]
    fn rejects_option_before_config() {
        assert!(parse_uci_export("\toption apn 'internet'\n").is_err());
    }

    #[test]
    fn rejects_unterminated_quote() {
        assert!(parse_uci_export("config modem 'unterminated\n").is_err());
    }
}
