//! Shared broker+tapped-wire runners for ubus compatibility matrices.

use std::path::PathBuf;

use crate::golden::Golden;
use crate::ubusd::fault_profile_from_env;
use crate::{canon, replay, skip, CanonTranscript, TapPolicy, UbusdHarness, WireTap};

/// Per-case broker configuration for compatibility runners.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct BrokerOpts {
    pub acl_dir: Option<String>,
    pub env: Vec<(String, String)>,
}

/// Which concrete broker a single tapped capture drives.
#[derive(Clone, Copy, Debug)]
pub enum BrokerArm {
    Upstream,
    Rs,
}

/// Which broker(s) a matrix case drives.
#[derive(Clone, Copy, Debug)]
pub enum MatrixArm {
    Upstream,
    Rs,
    Parity,
    Golden,
}

impl BrokerArm {
    fn label(self) -> &'static str {
        match self {
            Self::Upstream => "upstream",
            Self::Rs => "rs",
        }
    }

    pub(crate) fn start(self, opts: &BrokerOpts) -> std::io::Result<Option<UbusdHarness>> {
        let env: Vec<(&str, &str)> = opts
            .env
            .iter()
            .map(|(key, value)| (key.as_str(), value.as_str()))
            .collect();
        match (self, opts.acl_dir.as_deref()) {
            (Self::Upstream, Some(acl_dir)) => UbusdHarness::start_upstream_with_acl(acl_dir, &env),
            (Self::Upstream, None) => UbusdHarness::start_upstream_with_env(&env),
            (Self::Rs, Some(acl_dir)) => UbusdHarness::start_rust_with_acl(acl_dir, &env),
            (Self::Rs, None) => UbusdHarness::start_rust_with_env(&env),
        }
    }
}

/// Spawn `arm`'s broker, front it with a [`WireTap`], run `body`
/// against the tap's socket path, and return the canonicalized
/// transcript. `None` is a visible skip when the broker binary is not
/// reachable in lenient mode.
pub fn run_tapped(test: &str, arm: BrokerArm, body: &dyn Fn(&str)) -> Option<CanonTranscript> {
    run_tapped_with(test, arm, &BrokerOpts::default(), body)
}

/// [`run_tapped`] with per-broker configuration such as an ACL directory and
/// child-only environment overrides.
pub fn run_tapped_with(
    test: &str,
    arm: BrokerArm,
    opts: &BrokerOpts,
    body: &dyn Fn(&str),
) -> Option<CanonTranscript> {
    let harness = match arm.start(opts) {
        Ok(Some(harness)) => harness,
        Ok(None) => {
            skip::or_skip(
                &format!("{test}({})", arm.label()),
                "broker binary not reachable",
            );
            return None;
        }
        Err(e) => panic!("broker start for {test}({}): {e:?}", arm.label()),
    };
    let policy = TapPolicy {
        fragment_c2b: fault_profile_from_env().and_then(|profile| profile.fragment),
    };
    let tap = WireTap::spawn_with_policy(harness.socket_path(), policy)
        .unwrap_or_else(|e| panic!("spawn tap for {test}({}): {e:?}", arm.label()));
    let tap_path = tap
        .socket_path()
        .to_str()
        .expect("utf-8 tap socket path")
        .to_owned();

    body(&tap_path);

    // The body's transports are dropped by now; wait for the broker to
    // finish closing every tapped conn so teardown frames/EOFs land in
    // the capture deterministically instead of racing pump shutdown. A
    // timeout here is a real teardown race (a scenario body dropped a
    // connection before reading the broker-minted teardown frames), not
    // a flake to swallow — fail loudly with the still-open conns.
    if let Err(pending) = tap.wait_quiescent(std::time::Duration::from_secs(5)) {
        panic!(
            "{test}({}): tap did not reach quiescence within 5s — teardown race, \
             capture would be nondeterministically truncated. Still-open: {}",
            arm.label(),
            pending.join("; "),
        );
    }
    let events = tap.into_events();
    drop(harness);

    let transcript = canon::canonicalize(&events)
        .unwrap_or_else(|e| panic!("canonicalize {test}({}): {e}", arm.label()));
    if matches!(arm, BrokerArm::Upstream) && record_mode() {
        write_golden(test, transcript.clone());
    }
    Some(transcript)
}

/// The three-way runner for rstest cases. Single-broker arms still
/// canonicalize the tap capture, while parity runs the body against
/// both fresh brokers and asserts canonical transcript equality.
pub fn run_matrix_case(test: &str, arm: MatrixArm, body: &dyn Fn(&str)) {
    run_matrix_case_with(test, arm, &BrokerOpts::default(), body)
}

/// [`run_matrix_case`] with per-broker configuration. Golden replay uses the
/// same options as the recorded scenario, including a pinned NSS identity for
/// ACL cases.
pub fn run_matrix_case_with(test: &str, arm: MatrixArm, opts: &BrokerOpts, body: &dyn Fn(&str)) {
    match arm {
        MatrixArm::Upstream => {
            let Some(upstream) = run_tapped_with(test, BrokerArm::Upstream, opts, body) else {
                return;
            };
            if !record_mode() {
                if let Some(golden) = load_golden(test) {
                    if let Some(diff) = canon::diff_events(&golden.transcript, &upstream) {
                        panic!(
                            "{test}: golden drift — pinned revs changed? re-run tools/ubus-record.sh\n{diff}"
                        );
                    }
                }
            }
        }
        MatrixArm::Rs => {
            let _ = run_tapped_with(test, BrokerArm::Rs, opts, body);
        }
        MatrixArm::Parity => {
            let Some(upstream) = run_tapped_with(test, BrokerArm::Upstream, opts, body) else {
                return;
            };
            let Some(rs) = run_tapped_with(test, BrokerArm::Rs, opts, body) else {
                return;
            };
            if let Some(diff) = canon::diff(&upstream, &rs) {
                panic!("{test}: canonical transcript divergence between upstream and rs\n{diff}");
            }
        }
        MatrixArm::Golden => {
            let Some(golden) = load_golden(test) else {
                skip::or_skip(test, "golden transcript not recorded");
                return;
            };
            let _ = replay::replay_golden(test, &golden, opts);
        }
    }
}

fn record_mode() -> bool {
    std::env::var("UBUS_ZERO_GOLDEN").as_deref() == Ok("record")
}

fn write_golden(test: &str, transcript: CanonTranscript) {
    let (ubus_rev, libubox_rev) = pinned_revs_from_env();
    let golden = Golden::from_transcript(test, ubus_rev, libubox_rev, transcript);
    let path = golden_path(test);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .unwrap_or_else(|e| panic!("create golden dir {}: {e:?}", parent.display()));
    }
    std::fs::write(&path, golden.serialize())
        .unwrap_or_else(|e| panic!("write golden {}: {e:?}", path.display()));
}

fn load_golden(test: &str) -> Option<Golden> {
    let path = golden_path(test);
    if !path.exists() {
        return None;
    }
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("read golden {}: {e:?}", path.display()));
    Some(Golden::parse(&text).unwrap_or_else(|e| panic!("parse golden {}: {e}", path.display())))
}

fn golden_path(test: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("goldens")
        .join(format!("{}.golden", slug(test)))
}

fn slug(test: &str) -> String {
    test.split_whitespace().collect::<Vec<_>>().join("_")
}

fn pinned_revs_from_env() -> (String, String) {
    let Some(raw) = std::env::var("UBUS_ZERO_REVS").ok() else {
        return ("unknown".to_owned(), "unknown".to_owned());
    };
    let mut parts = raw.split_whitespace();
    let ubus = parts.next().unwrap_or("unknown").to_owned();
    let libubox = parts.next().unwrap_or("unknown").to_owned();
    (ubus, libubox)
}
