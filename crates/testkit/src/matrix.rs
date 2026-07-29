//! `broker_matrix!` — declare one test body, get one `#[test]` per
//! peer fixture in the matrix (raw / upstream / rs).
//!
//! The transport type is passed as a `path` so testkit doesn't have
//! to depend on `ubus_testkit::UnixUbusTransport`.

/// Internal — emit the shared paired-peer setup. Public so the
/// surface macros below can call it across crate boundaries; not
/// part of the user-facing API.
#[macro_export]
#[doc(hidden)]
macro_rules! __paired_peer_setup {
    ($peer:ident, $transport:ident: $tpath:path, $body:block) => {{
        let __peer_listener = $crate::RawPeer::bind().expect("ubus-testkit: bind raw peer");
        let __path: &str = __peer_listener
            .path()
            .to_str()
            .expect("ubus-testkit: socket path is utf-8");
        let mut $transport = <$tpath>::connect(__path).expect("ubus-testkit: connect transport");
        let mut $peer = __peer_listener
            .accept()
            .expect("ubus-testkit: accept raw peer");
        let _ = &mut $transport;
        let _ = &mut $peer;
        $body
    }};
}

/// Internal — emit the broker-backed setup. `$start` is one of
/// `UbusdHarness::start_upstream` / `start_rust`; both return
/// `io::Result<Option<UbusdHarness>>` and `Ok(None)` triggers a
/// visible skip (or a strict-mode panic) through [`crate::skip`].
///
/// Real brokers don't echo frames, so the `$peer` binding is a
/// second `UnixStream::connect` — bodies that only touch
/// `$transport` ignore it; bodies that `peer.read(...)` see the
/// broker-side bytes for that auxiliary connection.
#[macro_export]
#[doc(hidden)]
macro_rules! __broker_peer_setup {
    ($start:path, $skip_label:expr, $peer:ident, $transport:ident: $tpath:path, $body:block) => {{
        let __harness = match $start() {
            Ok(Some(h)) => h,
            // Keep the std-only stderr/env handling inside testkit;
            // these macros expand in no_std + alloc test crates.
            Ok(None) => {
                $crate::skip::or_skip($skip_label, "broker binary not reachable");
                return;
            }
            Err(e) => panic!("{}: {:?}", $skip_label, e),
        };
        let __path: &str = __harness
            .socket_path()
            .to_str()
            .expect("ubus-testkit: socket path is utf-8");
        let mut $transport = <$tpath>::connect(__path).expect("ubus-testkit: connect transport");
        let mut $peer = std::os::unix::net::UnixStream::connect(__path)
            .expect("ubus-testkit: peer connect to broker");
        let _ = &mut $transport;
        let _ = &mut $peer;
        $body
    }};
}

/// Emit `<name>_raw` for [`crate::RawPeer`], plus `<name>_upstream`
/// and `<name>_rs` for the two [`crate::UbusdHarness`] flavours.
/// Each broker arm runs in default `cargo test` and reports a visible
/// skip (Ok(None) → `SKIP ...` then early `return`) when its binary is
/// unreachable; strict mode turns that skip into a panic.
///
#[macro_export]
macro_rules! broker_matrix {
    (fn $name:ident($peer:ident, $transport:ident: $tpath:path) $body:block) => {
        $crate::__private::paste! {
            #[test]
            fn [<$name _raw>]() {
                $crate::__paired_peer_setup!($peer, $transport: $tpath, $body);
            }

            #[test]
            fn [<$name _upstream>]() {
                $crate::__broker_peer_setup!(
                    $crate::UbusdHarness::start_upstream,
                    concat!(stringify!($name), "_upstream"),
                    $peer, $transport: $tpath, $body
                );
            }

            #[test]
            fn [<$name _rs>]() {
                $crate::__broker_peer_setup!(
                    $crate::UbusdHarness::start_rust,
                    concat!(stringify!($name), "_rs"),
                    $peer, $transport: $tpath, $body
                );
            }
        }
    };
}

/// Raw-peer-only: byte-mirror tests can't run against real brokers
/// (brokers consume frames rather than echoing them).
#[macro_export]
macro_rules! paired_peer_test {
    (fn $name:ident($peer:ident, $transport:ident: $tpath:path) $body:block) => {
        #[test]
        fn $name() {
            $crate::__paired_peer_setup!($peer, $transport: $tpath, $body);
        }
    };
}
