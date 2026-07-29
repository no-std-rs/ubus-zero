//! Broker process lifecycle parity pins: signals and socket-file cleanup.

#![cfg(feature = "testing")]

use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use rstest::rstest;
use rustix::process::Signal;
use ubus::blobmsg::{lookup as blob_lookup, BlobMsgTable, BlobValue};
use ubus::envelope::{find, EnvBuilder, UbusAttr};
use ubus::error::UbusError;
use ubus::transport::Transport;
use ubus::wire::{Frame, Header, MsgType, PROTO_VERSION, UBUS_STATUS_OK};
use ubus_testkit::UnixUbusTransport;
use ubus_testkit::{skip, unique_socket_path, UbusdHarness};

#[allow(dead_code)]
mod common;
use common::recv_until;

#[derive(Clone, Copy, Debug)]
enum Arm {
    Upstream,
    Rs,
}

const TEST_USER: &str = "testuser";
const TEST_PEERCRED: &str = "1000:1000:1";
const TEST_NSS_USERS: &str = "1000:testuser,0:root";
const TEST_NSS_GROUPS: &str = "1000:testgroup,0:root";

fn start(arm: Arm) -> Option<UbusdHarness> {
    let start: fn() -> std::io::Result<Option<UbusdHarness>> = match arm {
        Arm::Upstream => UbusdHarness::start_upstream,
        Arm::Rs => UbusdHarness::start_rust,
    };
    match start() {
        Ok(Some(harness)) => Some(harness),
        Ok(None) => {
            skip::or_skip("ubusd_lifecycle", "broker binary not reachable");
            None
        }
        Err(e) => panic!("broker start: {e:?}"),
    }
}

struct AclDir {
    dir: PathBuf,
}

impl AclDir {
    fn one_file(contents: &str) -> Self {
        let dir = unique_socket_path("lifecycle-acl").with_extension("d");
        std::fs::create_dir_all(&dir).expect("create ACL dir");
        std::fs::write(dir.join("grant.json"), contents).expect("write ACL file");
        Self { dir }
    }
    fn path(&self) -> &Path {
        &self.dir
    }
}

impl Drop for AclDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn start_with_acl(arm: Arm, acl: &AclDir) -> Option<UbusdHarness> {
    start_with_acl_env(arm, acl, &[])
}

fn start_with_acl_env(arm: Arm, acl: &AclDir, env: &[(&str, &str)]) -> Option<UbusdHarness> {
    let path = acl.path().to_str().expect("utf-8 ACL dir");
    let result = match arm {
        Arm::Upstream => UbusdHarness::start_upstream_with_acl(path, env),
        Arm::Rs => UbusdHarness::start_rust_with_acl(path, env),
    };
    match result {
        Ok(Some(harness)) => Some(harness),
        Ok(None) => {
            skip::or_skip("ubusd_lifecycle", "broker binary not reachable");
            None
        }
        Err(error) => panic!("broker with ACL start: {error:?}"),
    }
}

fn client_observes_eof(transport: &mut UnixUbusTransport, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        match transport.recv() {
            Err(UbusError::Closed) => return,
            Ok(Some(frame)) => panic!("unexpected frame after broker shutdown: {frame:?}"),
            Ok(None) => std::thread::sleep(Duration::from_millis(2)),
            Err(e) => panic!("client recv after broker shutdown: {e:?}"),
        }
    }
    panic!("client did not observe broker EOF within {timeout:?}");
}

fn signal_unlinks_socket_and_closes_clients(arm: Arm, signal: Signal) {
    let Some(mut harness) = start(arm) else {
        return;
    };
    let path = harness.socket_path().to_str().expect("utf-8 socket path");
    let mut transport = UnixUbusTransport::connect(path).expect("connect client");
    recv_until(&mut transport, Duration::from_secs(5), |frame| {
        frame.hdr.ty == MsgType::Hello
    })
    .expect("HELLO before signal");

    harness
        .send_signal(signal)
        .expect("signal broker by saved PID");
    let status = harness
        .wait_exit(Duration::from_secs(5))
        .expect("broker exits after signal");
    assert!(status.success(), "broker signal exit status: {status}");
    assert!(
        !harness.socket_path().exists(),
        "broker itself must unlink its socket before harness Drop"
    );
    client_observes_eof(&mut transport, Duration::from_secs(5));
}

#[rstest]
#[case::upstream(Arm::Upstream)]
#[case::rs(Arm::Rs)]
fn sigterm_unlinks_socket_and_closes_clients(#[case] arm: Arm) {
    signal_unlinks_socket_and_closes_clients(arm, Signal::TERM);
}

#[rstest]
#[case::upstream(Arm::Upstream)]
#[case::rs(Arm::Rs)]
fn sigint_unlinks_socket_like_sigterm(#[case] arm: Arm) {
    signal_unlinks_socket_and_closes_clients(arm, Signal::INT);
}

#[rstest]
#[case::upstream(Arm::Upstream)]
#[case::rs(Arm::Rs)]
fn socket_file_mode_is_0666(#[case] arm: Arm) {
    let Some(harness) = start(arm) else {
        return;
    };
    let mode = std::fs::metadata(harness.socket_path())
        .expect("socket metadata")
        .mode()
        & 0o7777;
    assert_eq!(mode, 0o666, "socket mode");
}

#[rstest]
#[case::upstream(Arm::Upstream)]
#[case::rs(Arm::Rs)]
fn acl_sighup_reload_broadcasts_sequence_event(#[case] arm: Arm) {
    let acl = AclDir::one_file(r#"{"user":"root"}"#);
    let Some(harness) = start_with_acl(arm, &acl) else {
        return;
    };
    let path = harness.socket_path().to_str().expect("utf-8 socket");
    let mut transport = UnixUbusTransport::connect(path).expect("connect");
    recv_until(&mut transport, Duration::from_secs(5), |frame| {
        frame.hdr.ty == MsgType::Hello
    })
    .expect("HELLO");

    transport
        .send(&Frame {
            hdr: Header {
                version: PROTO_VERSION,
                ty: MsgType::AddObject,
                seq: 1,
                peer: 0,
            },
            payload: EnvBuilder::new().finish(),
        })
        .expect("add listener object");
    let listener = recv_until(&mut transport, Duration::from_secs(5), |frame| {
        frame.hdr.ty == MsgType::Data && frame.hdr.seq == 1
    })
    .expect("listener DATA");
    let listener = find(&listener.payload, UbusAttr::ObjId)
        .and_then(|attr| attr.as_u32())
        .expect("listener id");
    recv_until(&mut transport, Duration::from_secs(5), |frame| {
        frame.hdr.ty == MsgType::Status && frame.hdr.seq == 1
    })
    .expect("listener status");

    let registration = BlobMsgTable::build(|builder| {
        builder.put_i32(Some("object"), listener as i32);
        builder.put_str(Some("pattern"), "ubus.acl.sequence");
    });
    let mut register = EnvBuilder::new();
    register.push_u32(UbusAttr::ObjId, 1);
    register.push_str(UbusAttr::Method, "register");
    register.push_data(registration.contents());
    transport
        .send(&Frame {
            hdr: Header {
                version: PROTO_VERSION,
                ty: MsgType::Invoke,
                seq: 2,
                peer: 1,
            },
            payload: register.finish(),
        })
        .expect("register listener");
    let registered = recv_until(&mut transport, Duration::from_secs(5), |frame| {
        frame.hdr.ty == MsgType::Status && frame.hdr.seq == 2
    })
    .expect("register status");
    assert_eq!(
        find(&registered.payload, UbusAttr::Status)
            .and_then(|attr| attr.as_u32())
            .map(|v| v as i32),
        Some(UBUS_STATUS_OK)
    );

    harness.send_signal(Signal::HUP).expect("SIGHUP broker");
    let event = recv_until(&mut transport, Duration::from_secs(5), |frame| {
        frame.hdr.ty == MsgType::Invoke
            && find(&frame.payload, UbusAttr::Method).and_then(|attr| attr.as_str())
                == Some("ubus.acl.sequence")
    })
    .expect("ACL sequence event");
    assert_eq!(event.hdr.seq, 1);
    assert_eq!(event.hdr.peer, 0);
    assert_eq!(
        find(&event.payload, UbusAttr::ObjId).and_then(|attr| attr.as_u32()),
        Some(listener)
    );
    let payload = find(&event.payload, UbusAttr::Data)
        .expect("event data")
        .payload;
    assert_eq!(
        blob_lookup(payload, "sequence").and_then(|attr| attr.value().ok()),
        Some(BlobValue::Int32(2))
    );

    let mut query = EnvBuilder::new();
    query.push_u32(UbusAttr::ObjId, 2);
    query.push_str(UbusAttr::Method, "query");
    transport
        .send(&Frame {
            hdr: Header {
                version: PROTO_VERSION,
                ty: MsgType::Invoke,
                seq: 3,
                peer: 2,
            },
            payload: query.finish(),
        })
        .expect("query ACL sequence");
    let data = recv_until(&mut transport, Duration::from_secs(5), |frame| {
        frame.hdr.ty == MsgType::Data && frame.hdr.seq == 3
    })
    .expect("ACL query DATA");
    let payload = find(&data.payload, UbusAttr::Data)
        .expect("query data")
        .payload;
    assert_eq!(
        blob_lookup(payload, "seq").and_then(|attr| attr.value().ok()),
        Some(BlobValue::Int32(2))
    );
}

#[rstest]
#[case::upstream(Arm::Upstream)]
#[case::rs(Arm::Rs)]
fn acl_sighup_with_emptied_dir_keeps_grants_and_sequence(#[case] arm: Arm) {
    let acl = AclDir::one_file(&format!(r#"{{"user":"{TEST_USER}","publish":["keep.*"]}}"#));
    let env = [
        ("UBUS_TESTKIT_PEERCRED", TEST_PEERCRED),
        ("UBUS_TESTKIT_NSS_USERS", TEST_NSS_USERS),
        ("UBUS_TESTKIT_NSS_GROUPS", TEST_NSS_GROUPS),
    ];
    let Some(harness) = start_with_acl_env(arm, &acl, &env) else {
        return;
    };
    let path = harness.socket_path().to_str().expect("utf-8 socket");
    let mut serial = UnixUbusTransport::connect(path).expect("connect serial client");
    recv_until(&mut serial, Duration::from_secs(5), |frame| {
        frame.hdr.ty == MsgType::Hello
    })
    .expect("serial HELLO");

    std::fs::remove_file(acl.path().join("grant.json")).expect("empty ACL directory");
    harness.send_signal(Signal::HUP).expect("SIGHUP broker");
    serial
        .send(&Frame {
            hdr: Header {
                version: PROTO_VERSION,
                ty: MsgType::Ping,
                seq: 10,
                peer: 0,
            },
            payload: EnvBuilder::new().finish(),
        })
        .expect("PING after SIGHUP");
    recv_until(&mut serial, Duration::from_secs(5), |frame| {
        frame.hdr.ty == MsgType::Data && frame.hdr.seq == 10
    })
    .expect("PING DATA");
    recv_until(&mut serial, Duration::from_secs(5), |frame| {
        frame.hdr.ty == MsgType::Status && frame.hdr.seq == 10
    })
    .expect("PING STATUS");

    let mut client = UnixUbusTransport::connect(path).expect("connect post-HUP client");
    recv_until(&mut client, Duration::from_secs(5), |frame| {
        frame.hdr.ty == MsgType::Hello
    })
    .expect("post-HUP HELLO");
    let mut add = EnvBuilder::new();
    add.push_str(UbusAttr::ObjPath, "keep.two");
    client
        .send(&Frame {
            hdr: Header {
                version: PROTO_VERSION,
                ty: MsgType::AddObject,
                seq: 11,
                peer: 0,
            },
            payload: add.finish(),
        })
        .expect("post-HUP named ADD_OBJECT");
    recv_until(&mut client, Duration::from_secs(5), |frame| {
        frame.hdr.ty == MsgType::Data && frame.hdr.seq == 11
    })
    .expect("post-HUP ADD_OBJECT DATA");
    let added = recv_until(&mut client, Duration::from_secs(5), |frame| {
        frame.hdr.ty == MsgType::Status && frame.hdr.seq == 11
    })
    .expect("post-HUP ADD_OBJECT STATUS");
    assert_eq!(
        find(&added.payload, UbusAttr::Status)
            .and_then(|attr| attr.as_u32())
            .map(|value| value as i32),
        Some(UBUS_STATUS_OK)
    );

    let mut query = EnvBuilder::new();
    query.push_u32(UbusAttr::ObjId, 2);
    query.push_str(UbusAttr::Method, "query");
    client
        .send(&Frame {
            hdr: Header {
                version: PROTO_VERSION,
                ty: MsgType::Invoke,
                seq: 12,
                peer: 2,
            },
            payload: query.finish(),
        })
        .expect("post-HUP ACL query");
    let query = recv_until(&mut client, Duration::from_secs(5), |frame| {
        frame.hdr.ty == MsgType::Data && frame.hdr.seq == 12
    })
    .expect("post-HUP ACL query DATA");
    let query = find(&query.payload, UbusAttr::Data)
        .expect("ACL query data")
        .payload;
    assert_eq!(
        blob_lookup(query, "seq").and_then(|attr| attr.value().ok()),
        Some(BlobValue::Int32(1))
    );
}
