//! SCM_RIGHTS behavior cannot traverse WireTap, so these are deliberately
//! direct-harness, two-arm behavioral probes.  A pipe round-trip proves that
//! the relayed descriptor works without depending on its process-local number.

#![cfg(feature = "testing")]

use std::path::Path;
#[cfg(target_os = "linux")]
use std::sync::mpsc;
use std::time::Duration;

use rstest::rstest;
use rustix::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd};
use rustix::fs::{fcntl_getfl, fcntl_setfl, OFlags};
use rustix::io::Errno;
use ubus::blobmsg::BlobBuilder;
use ubus::envelope::{find, EnvBuilder, UbusAttr};
use ubus::wire::{
    Codec, Frame, Header, MsgType, PROTO_VERSION, UBUS_STATUS_NOT_FOUND, UBUS_STATUS_OK,
    UBUS_STATUS_PERMISSION_DENIED,
};
use ubus_testkit::{pipe, skip, unique_socket_path, BrokerArm, FdPeer, UbusdHarness};
#[cfg(target_os = "linux")]
use ubus_testkit::{LibuboxClient, LibuboxHost};

#[allow(dead_code)]
mod common;

const RECV: Duration = Duration::from_secs(5);

enum PipeMarker {
    Marker(u8),
    Eof,
    Open,
}

fn fd_send(peer: &FdPeer, frame: &Frame, fd: Option<BorrowedFd<'_>>) {
    peer.send_frame_with_fd(&Codec::write_frame(frame), fd)
        .expect("send frame");
}

fn fd_hello(peer: &FdPeer) -> u32 {
    let (hello, fd) = peer.recv_full_frame_with_fd(RECV).expect("HELLO");
    assert_eq!(hello.hdr.ty, MsgType::Hello);
    assert!(fd.is_none(), "HELLO must not carry an fd");
    assert_ne!(hello.hdr.peer, 0, "HELLO peer id");
    hello.hdr.peer
}

fn signature_with_ping_method() -> Vec<u8> {
    let mut sig = BlobBuilder::new();
    sig.open_table(Some("ping"));
    sig.close_table();
    sig.finish()
}

fn fd_register_typed(peer: &FdPeer, seq: u16, path: &str) -> u32 {
    let mut env = EnvBuilder::new();
    env.push_str(UbusAttr::ObjPath, path);
    env.push_signature(&signature_with_ping_method());
    fd_send(
        peer,
        &Frame {
            hdr: Header {
                version: PROTO_VERSION,
                ty: MsgType::AddObject,
                seq,
                peer: 0,
            },
            payload: env.finish(),
        },
        None,
    );
    let (data, fd) = peer.recv_full_frame_with_fd(RECV).expect("ADD_OBJECT DATA");
    assert_eq!(data.hdr.ty, MsgType::Data);
    assert_eq!(data.hdr.seq, seq);
    assert!(fd.is_none(), "ADD_OBJECT DATA fd");
    let obj = find(&data.payload, UbusAttr::ObjId)
        .and_then(|attr| attr.as_u32())
        .expect("ADD_OBJECT OBJID");
    let (status, fd) = peer
        .recv_full_frame_with_fd(RECV)
        .expect("ADD_OBJECT STATUS");
    assert_eq!(status.hdr.ty, MsgType::Status);
    assert_eq!(status.hdr.seq, seq);
    assert_eq!(status_value(&status), UBUS_STATUS_OK);
    assert!(fd.is_none(), "ADD_OBJECT STATUS fd");
    obj
}

fn fd_register_anonymous(peer: &FdPeer, seq: u16) -> u32 {
    fd_send(
        peer,
        &Frame {
            hdr: Header {
                version: PROTO_VERSION,
                ty: MsgType::AddObject,
                seq,
                peer: 0,
            },
            payload: EnvBuilder::new().finish(),
        },
        None,
    );
    let (data, fd) = peer
        .recv_full_frame_with_fd(RECV)
        .expect("anonymous ADD_OBJECT DATA");
    assert_eq!(data.hdr.ty, MsgType::Data);
    assert_eq!(data.hdr.seq, seq);
    assert!(fd.is_none(), "anonymous ADD_OBJECT DATA fd");
    let obj = find(&data.payload, UbusAttr::ObjId)
        .and_then(|attr| attr.as_u32())
        .expect("anonymous ADD_OBJECT OBJID");
    let (status, fd) = peer
        .recv_full_frame_with_fd(RECV)
        .expect("anonymous ADD_OBJECT STATUS");
    assert_eq!(status.hdr.ty, MsgType::Status);
    assert_eq!(status.hdr.seq, seq);
    assert_eq!(status_value(&status), UBUS_STATUS_OK);
    assert!(fd.is_none(), "anonymous ADD_OBJECT STATUS fd");
    obj
}

fn invoke(seq: u16, obj: u32, method: &str, data: &[u8]) -> Frame {
    let mut env = EnvBuilder::new();
    env.push_u32(UbusAttr::ObjId, obj);
    env.push_str(UbusAttr::Method, method);
    env.push_data(data);
    Frame {
        hdr: Header {
            version: PROTO_VERSION,
            ty: MsgType::Invoke,
            seq,
            peer: obj,
        },
        payload: env.finish(),
    }
}

fn status(seq: u16, peer: u32, value: i32) -> Frame {
    let mut env = EnvBuilder::new();
    env.push_u32(UbusAttr::Status, value as u32);
    Frame {
        hdr: Header {
            version: PROTO_VERSION,
            ty: MsgType::Status,
            seq,
            peer,
        },
        payload: env.finish(),
    }
}

/// Owner-side terminal STATUS for a forwarded INVOKE: addressed at the
/// caller peer the broker stamped into the forwarded frame, carrying the
/// ObjId attr `ubusd_handle_response` requires.
fn owner_status_reply(forwarded: &Frame, obj: u32) -> Frame {
    let mut env = EnvBuilder::new();
    env.push_u32(UbusAttr::ObjId, obj);
    env.push_u32(UbusAttr::Status, UBUS_STATUS_OK as u32);
    Frame {
        hdr: Header {
            version: PROTO_VERSION,
            ty: MsgType::Status,
            seq: forwarded.hdr.seq,
            peer: forwarded.hdr.peer,
        },
        payload: env.finish(),
    }
}

fn status_value(frame: &Frame) -> i32 {
    find(&frame.payload, UbusAttr::Status)
        .and_then(|attr| attr.as_u32())
        .map(|value| value as i32)
        .expect("STATUS value")
}

fn read_marker(read_end: &OwnedFd, timeout: Duration) -> PipeMarker {
    use rustix::event::{poll, PollFd, PollFlags, Timespec};

    let timeout = Timespec::try_from(timeout).expect("pipe deadline");
    let mut fds = [PollFd::new(read_end, PollFlags::IN)];
    loop {
        match poll(&mut fds, Some(&timeout)) {
            Ok(0) => return PipeMarker::Open,
            Ok(_) => break,
            Err(Errno::INTR) => continue,
            Err(e) => panic!("pipe poll: {e}"),
        }
    }
    let flags = fcntl_getfl(read_end).expect("get pipe flags");
    fcntl_setfl(read_end, flags | OFlags::NONBLOCK).expect("set pipe nonblocking");
    let mut byte = [0_u8; 1];
    match rustix::io::read(read_end, &mut byte) {
        Ok(0) => PipeMarker::Eof,
        Ok(1) => PipeMarker::Marker(byte[0]),
        Ok(n) => panic!("unexpected pipe read length {n}"),
        Err(Errno::AGAIN) => PipeMarker::Open,
        Err(e) => panic!("pipe read: {e}"),
    }
}

fn start(arm: BrokerArm) -> Option<UbusdHarness> {
    let start: fn() -> std::io::Result<Option<UbusdHarness>> = match arm {
        BrokerArm::Upstream => UbusdHarness::start_upstream,
        BrokerArm::Rs => UbusdHarness::start_rust,
    };
    match start() {
        Ok(Some(harness)) => Some(harness),
        Ok(None) => {
            skip::or_skip("ubusd_fd_passing", "broker binary not reachable");
            None
        }
        Err(e) => panic!("broker start: {e:?}"),
    }
}

fn peers(path: &Path, object: &str) -> (FdPeer, u32, FdPeer, u32) {
    let owner = FdPeer::connect(path).expect("owner connect");
    let _owner_peer = fd_hello(&owner);
    let obj = fd_register_typed(&owner, 1, object);
    let caller = FdPeer::connect(path).expect("caller connect");
    let caller_peer = fd_hello(&caller);
    (owner, obj, caller, caller_peer)
}

#[rstest]
#[case::upstream(BrokerArm::Upstream)]
#[case::rs(BrokerArm::Rs)]
fn invoke_fd_relays_scm_rights_to_owner(#[case] arm: BrokerArm) {
    let Some(harness) = start(arm) else { return };
    let (owner, obj, caller, _caller_peer) = peers(harness.socket_path(), "fd.forward");
    let (read_end, write_end) = pipe().expect("pipe");
    fd_send(
        &caller,
        &invoke(10, obj, "ping", &[]),
        Some(write_end.as_fd()),
    );
    drop(write_end);

    let (forwarded, received) = owner
        .recv_full_frame_with_fd(RECV)
        .expect("forwarded INVOKE");
    assert_eq!(forwarded.hdr.ty, MsgType::Invoke);
    let received = received.expect("forwarded INVOKE SCM_RIGHTS");
    rustix::io::write(&received, b"f").expect("write relayed fd");
    assert!(matches!(
        read_marker(&read_end, RECV),
        PipeMarker::Marker(b'f')
    ));
    fd_send(&owner, &status(10, obj, UBUS_STATUS_OK), None);
}

#[rstest]
#[case::upstream(BrokerArm::Upstream)]
#[case::rs(BrokerArm::Rs)]
fn reply_fd_relays_back_to_caller(#[case] arm: BrokerArm) {
    let Some(harness) = start(arm) else { return };
    let (owner, obj, caller, _caller_peer) = peers(harness.socket_path(), "fd.reply");
    fd_send(&caller, &invoke(11, obj, "ping", &[]), None);
    let (forwarded, received) = owner
        .recv_full_frame_with_fd(RECV)
        .expect("forwarded INVOKE");
    assert_eq!(forwarded.hdr.ty, MsgType::Invoke);
    assert!(received.is_none());
    // The broker stamps the caller's peer id into the forwarded INVOKE;
    // owner replies must be addressed there and carry the ObjId attr so
    // `forward_owner_response` validates ownership and routes them back.
    let caller_peer = forwarded.hdr.peer;

    // A DATA reply carrying an fd is dropped by the broker (probed:
    // reply-direction fd rides the terminal STATUS, not DATA). Send DATA
    // separately (well-formed: ObjId + Data attr) so the caller receives
    // it before the terminal STATUS.
    let mut data_env = EnvBuilder::new();
    data_env.push_u32(UbusAttr::ObjId, obj);
    data_env.push_data(&EnvBuilder::new().finish());
    fd_send(
        &owner,
        &Frame {
            hdr: Header {
                version: PROTO_VERSION,
                ty: MsgType::Data,
                seq: 11,
                peer: caller_peer,
            },
            payload: data_env.finish(),
        },
        None,
    );
    let (data, data_fd) = caller.recv_full_frame_with_fd(RECV).expect("reply DATA");
    assert_eq!(data.hdr.ty, MsgType::Data);
    assert!(data_fd.is_none());

    let (read_end, write_end) = pipe().expect("pipe");
    let mut status_env = EnvBuilder::new();
    status_env.push_u32(UbusAttr::ObjId, obj);
    status_env.push_u32(UbusAttr::Status, UBUS_STATUS_OK as u32);
    fd_send(
        &owner,
        &Frame {
            hdr: Header {
                version: PROTO_VERSION,
                ty: MsgType::Status,
                seq: 11,
                peer: caller_peer,
            },
            payload: status_env.finish(),
        },
        Some(write_end.as_fd()),
    );
    drop(write_end);
    let (reply, received) = caller.recv_full_frame_with_fd(RECV).expect("reply STATUS");
    assert_eq!(reply.hdr.ty, MsgType::Status);
    assert_eq!(status_value(&reply), UBUS_STATUS_OK);
    let received = received.expect("STATUS SCM_RIGHTS");
    rustix::io::write(&received, b"r").expect("write relayed reply fd");
    assert!(matches!(
        read_marker(&read_end, RECV),
        PipeMarker::Marker(b'r')
    ));
}

/// A libubus caller's fd reaches an in-tree owner on the forwarded INVOKE.
#[cfg(target_os = "linux")]
#[rstest]
#[case::upstream(BrokerArm::Upstream)]
#[case::rs(BrokerArm::Rs)]
fn libubox_invoke_fd_relays_to_intree_owner(#[case] arm: BrokerArm) {
    let Some(harness) = start(arm) else { return };
    let owner = FdPeer::connect(harness.socket_path()).expect("owner connect");
    let _owner_peer = fd_hello(&owner);
    let obj = fd_register_typed(&owner, 20, "fd.libubox.invoke");
    let (read_end, write_end) = pipe().expect("pipe");
    let path = harness.socket_path().to_owned();
    let (ready_tx, ready_rx) = mpsc::sync_channel(1);
    let caller = std::thread::spawn(move || -> std::io::Result<Vec<u8>> {
        let Some(mut caller) = LibuboxClient::connect(&path)? else {
            let _ = ready_tx.send(false);
            return Ok(Vec::new());
        };
        let _ = ready_tx.send(true);
        // Ownership of write_end transfers into libubus, which closes any
        // fd it sends (libubus-io.c ubus_send_msg).
        caller.invoke_with_fd("fd.libubox.invoke", "ping", write_end)
    });
    if !ready_rx.recv().expect("libubox caller readiness") {
        caller
            .join()
            .expect("libubox caller thread")
            .expect("libubox skip");
        skip::or_skip(
            "ubusd_fd_passing libubox_invoke_fd_relays_to_intree_owner",
            "libubox FFI artifacts not reachable",
        );
        return;
    }

    let (forwarded, received) = owner
        .recv_full_frame_with_fd(RECV)
        .expect("libubox forwarded INVOKE");
    assert_eq!(forwarded.hdr.ty, MsgType::Invoke);
    let received = received.expect("libubox forwarded INVOKE SCM_RIGHTS");
    rustix::io::write(&received, b"l").expect("write relayed libubox fd");
    assert!(matches!(
        read_marker(&read_end, RECV),
        PipeMarker::Marker(b'l')
    ));
    // Reply like the other owner fixtures: hdr.peer = the caller peer the
    // broker stamped into the forwarded INVOKE, payload ObjId + Status —
    // ubusd_handle_response drops STATUS frames missing either.
    let mut status_env = EnvBuilder::new();
    status_env.push_u32(UbusAttr::ObjId, obj);
    status_env.push_u32(UbusAttr::Status, UBUS_STATUS_OK as u32);
    fd_send(
        &owner,
        &Frame {
            hdr: Header {
                version: PROTO_VERSION,
                ty: MsgType::Status,
                seq: forwarded.hdr.seq,
                peer: forwarded.hdr.peer,
            },
            payload: status_env.finish(),
        },
        None,
    );
    caller
        .join()
        .expect("libubox caller thread")
        .expect("libubox invoke with fd");
}

/// A libubus handler's `ubus_request_set_fd` reply rides the terminal STATUS.
#[cfg(target_os = "linux")]
#[rstest]
#[case::upstream(BrokerArm::Upstream)]
#[case::rs(BrokerArm::Rs)]
fn libubox_request_set_fd_reaches_intree_caller(#[case] arm: BrokerArm) {
    let Some(harness) = start(arm) else { return };
    let Some(host) = LibuboxHost::connect(harness.socket_path()).expect("libubox host connect")
    else {
        skip::or_skip(
            "ubusd_fd_passing libubox_request_set_fd_reaches_intree_caller",
            "libubox FFI artifacts not reachable",
        );
        return;
    };
    let (read_end, write_end) = pipe().expect("pipe");
    let mut reply_fd = Some(write_end);
    let object = host
        .add_object_with_fd_reply("fd.libubox.reply", &["ping"], move |caller_fd, _, _| {
            assert!(caller_fd.is_none(), "raw caller did not attach an fd");
            (UBUS_STATUS_OK, None, reply_fd.take())
        })
        .expect("add libubox reply-fd object");

    let caller = FdPeer::connect(harness.socket_path()).expect("caller connect");
    let _caller_peer = fd_hello(&caller);
    fd_send(&caller, &invoke(21, object.id(), "ping", &[]), None);
    let (reply, received) = caller
        .recv_full_frame_with_fd(RECV)
        .expect("libubox reply STATUS");
    assert_eq!(reply.hdr.ty, MsgType::Status);
    assert_eq!(status_value(&reply), UBUS_STATUS_OK);
    let received = received.expect("libubox STATUS SCM_RIGHTS");
    rustix::io::write(&received, b"s").expect("write relayed reply fd");
    assert!(matches!(
        read_marker(&read_end, RECV),
        PipeMarker::Marker(b's')
    ));
    host.shutdown().expect("libubox host shutdown");
}

/// Upstream discards a NO_REPLY NOTIFY fd before fan-out to its subscriber.
#[rstest]
#[case::upstream(BrokerArm::Upstream)]
#[case::rs(BrokerArm::Rs)]
fn fd_on_notify_fanout_follows_upstream(#[case] arm: BrokerArm) {
    let Some(harness) = start(arm) else { return };
    let publisher = FdPeer::connect(harness.socket_path()).expect("publisher connect");
    let _publisher_peer = fd_hello(&publisher);
    let publisher_obj = fd_register_typed(&publisher, 30, "fd.notify");
    let subscriber = FdPeer::connect(harness.socket_path()).expect("subscriber connect");
    let _subscriber_peer = fd_hello(&subscriber);
    let subscriber_obj = fd_register_anonymous(&subscriber, 31);

    let mut subscribe = EnvBuilder::new();
    subscribe.push_u32(UbusAttr::ObjId, subscriber_obj);
    subscribe.push_u32(UbusAttr::Target, publisher_obj);
    fd_send(
        &subscriber,
        &Frame {
            hdr: Header {
                version: PROTO_VERSION,
                ty: MsgType::Subscribe,
                seq: 32,
                peer: publisher_obj,
            },
            payload: subscribe.finish(),
        },
        None,
    );
    let (active, active_fd) = publisher
        .recv_full_frame_with_fd(RECV)
        .expect("publisher Active=1");
    assert_eq!(active.hdr.ty, MsgType::Notify);
    assert!(active_fd.is_none(), "Active=1 fd");
    let (sub_status, sub_fd) = subscriber
        .recv_full_frame_with_fd(RECV)
        .expect("SUBSCRIBE STATUS");
    assert_eq!(sub_status.hdr.ty, MsgType::Status);
    assert_eq!(status_value(&sub_status), UBUS_STATUS_OK);
    assert!(sub_fd.is_none(), "SUBSCRIBE STATUS fd");

    let (read_end, write_end) = pipe().expect("pipe");
    let mut notify = EnvBuilder::new();
    notify.push_u32(UbusAttr::ObjId, publisher_obj);
    notify.push_str(UbusAttr::Method, "fd-event");
    notify.push_u8(UbusAttr::NoReply, 1);
    publisher
        .send_frame_with_fds(
            &Codec::write_frame(&Frame {
                hdr: Header {
                    version: PROTO_VERSION,
                    ty: MsgType::Notify,
                    seq: 33,
                    peer: publisher_obj,
                },
                payload: notify.finish(),
            }),
            &[write_end.as_raw_fd()],
        )
        .expect("send fd NOTIFY");
    drop(write_end);

    // Upstream closes the fd at receive time for every frame type except
    // STATUS and INVOKE (ubusd_proto_receive_message: ubus_msg_close_fd),
    // so a NOTIFY-borne fd is DROPPED — the fan-out arrives bare and the
    // pipe's read end reports EOF once the broker's copy is closed.
    let (fanout, received) = subscriber
        .recv_full_frame_with_fd(RECV)
        .expect("subscriber fd fan-out");
    assert_eq!(fanout.hdr.ty, MsgType::Invoke);
    assert!(
        received.is_none(),
        "NOTIFY fan-out must not carry the publisher's fd"
    );
    assert!(
        matches!(read_marker(&read_end, RECV), PipeMarker::Eof),
        "broker must close its copy of the NOTIFY fd"
    );
}

/// Upstream's one-slot recvmsg ancillary buffer keeps only fd[0] from one cmsg.
#[rstest]
#[case::upstream(BrokerArm::Upstream)]
#[case::rs(BrokerArm::Rs)]
fn second_fd_in_one_sendmsg_is_dropped(#[case] arm: BrokerArm) {
    let Some(harness) = start(arm) else { return };
    let (owner, obj, caller, _caller_peer) = peers(harness.socket_path(), "fd.two-rights");
    let (first_read, first_write) = pipe().expect("first pipe");
    let (second_read, second_write) = pipe().expect("second pipe");
    caller
        .send_frame_with_fds(
            &Codec::write_frame(&invoke(40, obj, "ping", &[])),
            &[first_write.as_raw_fd(), second_write.as_raw_fd()],
        )
        .expect("send INVOKE with two SCM_RIGHTS fds");
    drop(first_write);
    drop(second_write);

    let (forwarded, received) = owner
        .recv_full_frame_with_fd(RECV)
        .expect("forwarded two-fd INVOKE");
    assert_eq!(forwarded.hdr.ty, MsgType::Invoke);
    let received = received.expect("first forwarded SCM_RIGHTS fd");
    rustix::io::write(&received, b"1").expect("write first relayed fd");
    assert!(matches!(
        read_marker(&first_read, RECV),
        PipeMarker::Marker(b'1')
    ));
    assert!(matches!(read_marker(&second_read, RECV), PipeMarker::Eof));
    fd_send(&owner, &owner_status_reply(&forwarded, obj), None);
    let (first_status, first_status_fd) = caller
        .recv_full_frame_with_fd(RECV)
        .expect("two-fd INVOKE STATUS");
    assert_eq!(first_status.hdr.ty, MsgType::Status);
    assert_eq!(status_value(&first_status), UBUS_STATUS_OK);
    assert!(first_status_fd.is_none());

    fd_send(&caller, &invoke(41, obj, "ping", &[]), None);
    let (follow_up, follow_up_fd) = owner
        .recv_full_frame_with_fd(RECV)
        .expect("follow-up INVOKE");
    assert_eq!(follow_up.hdr.ty, MsgType::Invoke);
    assert!(follow_up_fd.is_none(), "follow-up INVOKE fd");
    fd_send(&owner, &owner_status_reply(&follow_up, obj), None);
    let (follow_up_status, follow_up_status_fd) = caller
        .recv_full_frame_with_fd(RECV)
        .expect("follow-up STATUS");
    assert_eq!(follow_up_status.hdr.ty, MsgType::Status);
    assert_eq!(status_value(&follow_up_status), UBUS_STATUS_OK);
    assert!(follow_up_status_fd.is_none());
}

#[rstest]
#[case::upstream(BrokerArm::Upstream)]
#[case::rs(BrokerArm::Rs)]
fn fd_on_unroutable_invoke_is_closed(#[case] arm: BrokerArm) {
    let Some(harness) = start(arm) else { return };
    let caller = FdPeer::connect(harness.socket_path()).expect("caller connect");
    let _caller_peer = fd_hello(&caller);
    let (read_end, write_end) = pipe().expect("pipe");
    fd_send(
        &caller,
        &invoke(12, 0x00ff_fffe, "ping", &[]),
        Some(write_end.as_fd()),
    );
    drop(write_end);
    let (reply, received) = caller.recv_full_frame_with_fd(RECV).expect("NOT_FOUND");
    assert_eq!(reply.hdr.ty, MsgType::Status);
    assert_eq!(status_value(&reply), UBUS_STATUS_NOT_FOUND);
    assert!(received.is_none());
    assert!(matches!(read_marker(&read_end, RECV), PipeMarker::Eof));
}

#[rstest]
#[case::upstream(BrokerArm::Upstream)]
#[case::rs(BrokerArm::Rs)]
fn fd_on_nonforwarding_message_is_closed(#[case] arm: BrokerArm) {
    let Some(harness) = start(arm) else { return };
    let caller = FdPeer::connect(harness.socket_path()).expect("caller connect");
    let _caller_peer = fd_hello(&caller);
    let (read_end, write_end) = pipe().expect("pipe");
    fd_send(
        &caller,
        &Frame {
            hdr: Header {
                version: PROTO_VERSION,
                ty: MsgType::Ping,
                seq: 13,
                peer: 0,
            },
            payload: EnvBuilder::new().finish(),
        },
        Some(write_end.as_fd()),
    );
    drop(write_end);
    let (data, received) = caller.recv_full_frame_with_fd(RECV).expect("PING DATA");
    assert_eq!(data.hdr.ty, MsgType::Data);
    assert!(received.is_none());
    assert!(matches!(read_marker(&read_end, RECV), PipeMarker::Eof));
}

#[rstest]
#[case::upstream(BrokerArm::Upstream)]
#[case::rs(BrokerArm::Rs)]
fn fd_on_acl_denied_invoke_is_closed(#[case] arm: BrokerArm) {
    let acl = unique_socket_path("fd-acl").with_extension("d");
    std::fs::create_dir_all(&acl).expect("create acl dir");
    std::fs::write(acl.join("deny.json"), r#"{"user":"example-no-such-user"}"#).expect("write acl");
    let acl_path = acl.to_str().expect("utf-8 acl path");
    // Owner is root and may publish; caller is a non-root identity which has
    // no access grant. The shim assigns identities by accepted-peer order.
    let creds = "0:0:1,0:0:1,1000:1000:1";
    let nss = [
        ("UBUS_TESTKIT_PEERCRED_SEQ", creds),
        ("UBUS_TESTKIT_NSS_USERS", "1000:fd-denied"),
        ("UBUS_TESTKIT_NSS_GROUPS", "1000:fd-denied"),
    ];
    let started = match arm {
        BrokerArm::Upstream => UbusdHarness::start_upstream_with_acl(acl_path, &nss),
        BrokerArm::Rs => UbusdHarness::start_rust_with_acl(acl_path, &nss),
    };
    let harness = match started {
        Ok(Some(harness)) => harness,
        Ok(None) => {
            skip::or_skip("ubusd_fd_passing", "broker binary not reachable");
            return;
        }
        Err(e) => panic!("broker start: {e:?}"),
    };
    let (_owner, obj, caller, _caller_peer) = peers(harness.socket_path(), "fd.acl");
    let (read_end, write_end) = pipe().expect("pipe");
    fd_send(
        &caller,
        &invoke(14, obj, "ping", &[]),
        Some(write_end.as_fd()),
    );
    drop(write_end);
    let (reply, received) = caller
        .recv_full_frame_with_fd(RECV)
        .expect("PERMISSION_DENIED");
    assert_eq!(reply.hdr.ty, MsgType::Status);
    assert_eq!(status_value(&reply), UBUS_STATUS_PERMISSION_DENIED);
    assert!(received.is_none());
    assert!(matches!(read_marker(&read_end, RECV), PipeMarker::Eof));
    let _ = std::fs::remove_dir_all(acl);
}

fn assert_fd_marker(received: Option<OwnedFd>, read_end: &OwnedFd, marker: u8) {
    let received = received.expect("forwarded SCM_RIGHTS");
    let flags = rustix::io::fcntl_getfd(&received).expect("descriptor flags");
    assert!(flags.contains(rustix::io::FdFlags::CLOEXEC));
    rustix::io::write(&received, &[marker]).expect("write forwarded pipe");
    assert!(matches!(read_marker(read_end, RECV), PipeMarker::Marker(value) if value == marker));
    drop(received);
    assert!(
        matches!(read_marker(read_end, RECV), PipeMarker::Eof),
        "broker must release its forwarded descriptor copy"
    );
}

fn expect_ok(peer: &FdPeer, seq: u16) {
    let (reply, fd) = peer.recv_full_frame_with_fd(RECV).expect("terminal STATUS");
    assert_eq!(reply.hdr.ty, MsgType::Status);
    assert_eq!(reply.hdr.seq, seq);
    assert_eq!(status_value(&reply), UBUS_STATUS_OK);
    assert!(fd.is_none());
}

fn owner_data_reply(forwarded: &Frame, obj: u32, data: &[u8]) -> Frame {
    let mut env = EnvBuilder::new();
    env.push_u32(UbusAttr::ObjId, obj);
    env.push_data(data);
    Frame::to_peer(
        MsgType::Data,
        forwarded.hdr.seq,
        forwarded.hdr.peer,
        env.finish(),
    )
}

/// An fd received with an incomplete header remains attached through all
/// subsequent header/body reads, rather than closing or moving to a successor.
#[rstest]
#[case::upstream(BrokerArm::Upstream)]
#[case::rs(BrokerArm::Rs)]
fn fragmented_invoke_retains_fd(
    #[case] arm: BrokerArm,
    #[values(1, 7, 8, 11, 12, 24)] split: usize,
) {
    let Some(harness) = start(arm) else { return };
    let (owner, obj, caller, _) = peers(harness.socket_path(), "fd.fragmented");
    let bytes = Codec::write_frame(&invoke(60, obj, "ping", &[]));
    let (read_end, write_end) = pipe().expect("pipe");
    caller
        .send_frame_with_fd(&bytes[..split], Some(write_end.as_fd()))
        .expect("first fragment");
    drop(write_end);
    // A negative socket read is an observable partial-frame assertion; the
    // descriptor must remain alive while no complete request can be forwarded.
    let error = owner
        .recv_full_frame_with_fd(Duration::from_millis(10))
        .unwrap_err();
    assert!(matches!(
        error.kind(),
        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
    ));
    caller
        .send_frame_with_fd(&bytes[split..], None)
        .expect("remaining frame");
    let (forwarded, received) = owner
        .recv_full_frame_with_fd(RECV)
        .expect("fragmented INVOKE");
    assert_eq!(forwarded.hdr.seq, 60);
    assert_fd_marker(received, &read_end, b'f');
    fd_send(&owner, &owner_status_reply(&forwarded, obj), None);
    expect_ok(&caller, 60);
}

/// The ancillary window is the eight-byte UBUS header plus the four-byte
/// outer blob header. An fd attached inside the body must be discarded.
#[rstest]
#[case::upstream(BrokerArm::Upstream)]
#[case::rs(BrokerArm::Rs)]
fn invoke_fd_at_header_body_boundary(
    #[case] arm: BrokerArm,
    #[values(1, 7, 8, 11, 12, 20)] offset: usize,
) {
    let Some(harness) = start(arm) else { return };
    let (owner, obj, caller, _) = peers(harness.socket_path(), "fd.window");
    let bytes = Codec::write_frame(&invoke(61, obj, "ping", &[]));
    let (read_end, write_end) = pipe().expect("pipe");
    caller
        .send_frame_with_fd(&bytes[..offset], None)
        .expect("plain prefix");
    caller
        .send_frame_with_fd(&bytes[offset..], Some(write_end.as_fd()))
        .expect("ancillary suffix");
    drop(write_end);
    let (forwarded, received) = owner.recv_full_frame_with_fd(RECV).expect("window INVOKE");
    assert_eq!(forwarded.hdr.seq, 61);
    if offset < 12 {
        assert_fd_marker(received, &read_end, b'w');
    } else {
        assert!(
            received.is_none(),
            "body-only fd at offset {offset} must not be relayed"
        );
        assert!(matches!(read_marker(&read_end, RECV), PipeMarker::Eof));
    }
    fd_send(&owner, &owner_status_reply(&forwarded, obj), None);
    expect_ok(&caller, 61);
}

/// A plain request queued immediately before an fd-bearing request must not
/// steal its SCM_RIGHTS, even if one recvmsg could read bytes from both writes.
#[rstest]
#[case::upstream(BrokerArm::Upstream)]
#[case::rs(BrokerArm::Rs)]
fn pipelined_plain_and_fd_invokes_keep_descriptor_association(#[case] arm: BrokerArm) {
    let Some(harness) = start(arm) else { return };
    let (owner, obj, caller, _) = peers(harness.socket_path(), "fd.pipeline");
    let mut pipes = Vec::new();
    for index in 0..16_u16 {
        let seq = 80 + index;
        let (read_end, write_end) = pipe().expect("pipeline pipe");
        fd_send(
            &caller,
            &invoke(seq, obj, "ping", &[]),
            (index % 3 != 0).then_some(write_end.as_fd()),
        );
        drop(write_end);
        pipes.push(read_end);
    }
    for (index, read_end) in pipes.iter().enumerate() {
        let (forwarded, received) = owner
            .recv_full_frame_with_fd(RECV)
            .expect("pipelined INVOKE");
        assert_eq!(forwarded.hdr.ty, MsgType::Invoke);
        assert_eq!(forwarded.hdr.seq, 80 + index as u16);
        if index % 3 != 0 {
            assert_fd_marker(received, read_end, index as u8);
        } else {
            assert!(
                received.is_none(),
                "plain request {index} stole a successor's fd"
            );
            assert!(matches!(read_marker(read_end, RECV), PipeMarker::Eof));
        }
        fd_send(&owner, &owner_status_reply(&forwarded, obj), None);
    }
    for seq in 80..96 {
        expect_ok(&caller, seq);
    }
}

#[rstest]
#[case::upstream(BrokerArm::Upstream)]
#[case::rs(BrokerArm::Rs)]
fn adjacent_data_and_fd_status_keep_reply_association(#[case] arm: BrokerArm) {
    let Some(harness) = start(arm) else { return };
    let (owner, obj, caller, _) = peers(harness.socket_path(), "fd.adjacent-replies");
    fd_send(&caller, &invoke(100, obj, "ping", &[]), None);
    let (forwarded, _) = owner.recv_full_frame_with_fd(RECV).expect("INVOKE");
    let (read_end, write_end) = pipe().expect("reply pipe");
    fd_send(&owner, &owner_data_reply(&forwarded, obj, &[]), None);
    fd_send(
        &owner,
        &owner_status_reply(&forwarded, obj),
        Some(write_end.as_fd()),
    );
    drop(write_end);
    let (data, received) = caller.recv_full_frame_with_fd(RECV).expect("adjacent DATA");
    assert_eq!(data.hdr.ty, MsgType::Data);
    assert_eq!(data.hdr.seq, 100);
    assert!(received.is_none(), "DATA stole terminal STATUS fd");
    let (reply, received) = caller
        .recv_full_frame_with_fd(RECV)
        .expect("adjacent STATUS");
    assert_eq!(reply.hdr.ty, MsgType::Status);
    assert_eq!(status_value(&reply), UBUS_STATUS_OK);
    assert_fd_marker(received, &read_end, b'r');
}

#[rstest]
#[case::upstream(BrokerArm::Upstream)]
#[case::rs(BrokerArm::Rs)]
fn descriptor_on_data_reply_is_closed_without_dropping_data(#[case] arm: BrokerArm) {
    let Some(harness) = start(arm) else { return };
    let (owner, obj, caller, _) = peers(harness.socket_path(), "fd.data-close");
    fd_send(&caller, &invoke(101, obj, "ping", &[]), None);
    let (forwarded, _) = owner.recv_full_frame_with_fd(RECV).expect("INVOKE");
    let (read_end, write_end) = pipe().expect("DATA pipe");
    fd_send(
        &owner,
        &owner_data_reply(&forwarded, obj, &[]),
        Some(write_end.as_fd()),
    );
    drop(write_end);
    let (data, received) = caller
        .recv_full_frame_with_fd(RECV)
        .expect("DATA without fd");
    assert_eq!(data.hdr.ty, MsgType::Data);
    assert!(received.is_none(), "DATA descriptors are not forwarded");
    assert!(matches!(read_marker(&read_end, RECV), PipeMarker::Eof));
    fd_send(&owner, &owner_status_reply(&forwarded, obj), None);
    expect_ok(&caller, 101);
}

#[rstest]
#[case::upstream(BrokerArm::Upstream)]
#[case::rs(BrokerArm::Rs)]
fn descriptor_on_forged_owner_response_is_closed(#[case] arm: BrokerArm) {
    let Some(harness) = start(arm) else { return };
    let (owner, obj, caller, _) = peers(harness.socket_path(), "fd.forged-response");
    fd_send(&caller, &invoke(102, obj, "ping", &[]), None);
    let (forwarded, _) = owner.recv_full_frame_with_fd(RECV).expect("INVOKE");
    let forger = FdPeer::connect(harness.socket_path()).expect("forger connect");
    fd_hello(&forger);
    let (read_end, write_end) = pipe().expect("forged pipe");
    fd_send(
        &forger,
        &owner_status_reply(&forwarded, obj),
        Some(write_end.as_fd()),
    );
    drop(write_end);
    // Observe disposal through pipe EOF; no timing guess or parent env change.
    assert!(matches!(read_marker(&read_end, RECV), PipeMarker::Eof));
    fd_send(&owner, &owner_status_reply(&forwarded, obj), None);
    expect_ok(&caller, 102);
}

#[rstest]
#[case::upstream(BrokerArm::Upstream)]
#[case::rs(BrokerArm::Rs)]
fn disconnect_with_incomplete_fd_frame_closes_descriptor(
    #[case] arm: BrokerArm,
    #[values(1, 7, 8, 11, 12, 24)] prefix: usize,
) {
    let Some(harness) = start(arm) else { return };
    let (_owner, obj, caller, _) = peers(harness.socket_path(), "fd.partial-close");
    let (read_end, write_end) = pipe().expect("partial pipe");
    let bytes = Codec::write_frame(&invoke(103, obj, "ping", &[]));
    caller
        .send_frame_with_fd(&bytes[..prefix], Some(write_end.as_fd()))
        .expect("partial INVOKE");
    drop(write_end);
    drop(caller);
    assert!(matches!(read_marker(&read_end, RECV), PipeMarker::Eof));
    let fresh = FdPeer::connect(harness.socket_path()).expect("fresh peer");
    fd_hello(&fresh);
}

#[rstest]
#[case::upstream(BrokerArm::Upstream)]
#[case::rs(BrokerArm::Rs)]
fn malformed_fd_frame_closes_descriptor(
    #[case] arm: BrokerArm,
    #[values(0_u32, 3, 0x00ff_ffff)] length: u32,
) {
    let Some(harness) = start(arm) else { return };
    let caller = FdPeer::connect(harness.socket_path()).expect("caller connect");
    fd_hello(&caller);
    let mut bytes = Header {
        version: PROTO_VERSION,
        ty: MsgType::Invoke,
        seq: 104,
        peer: 0,
    }
    .encode()
    .to_vec();
    bytes.extend_from_slice(&length.to_be_bytes());
    let (read_end, write_end) = pipe().expect("malformed pipe");
    caller
        .send_frame_with_fd(&bytes, Some(write_end.as_fd()))
        .expect("malformed fd frame");
    drop(write_end);
    assert!(matches!(read_marker(&read_end, RECV), PipeMarker::Eof));
    assert_eq!(
        caller.recv_full_frame_with_fd(RECV).unwrap_err().kind(),
        std::io::ErrorKind::UnexpectedEof
    );
    let fresh = FdPeer::connect(harness.socket_path()).expect("fresh peer");
    fd_hello(&fresh);
}

/// Exercise independent SCM_RIGHTS routes concurrently in the same daemon.
/// Markers identify each pipe and each request, catching cross-peer fd reuse.
#[rstest]
#[case::upstream(BrokerArm::Upstream)]
#[case::rs(BrokerArm::Rs)]
fn concurrent_descriptor_routes_do_not_cross_peers(#[case] arm: BrokerArm) {
    let Some(harness) = start(arm) else { return };
    let path = harness.socket_path().to_owned();
    std::thread::scope(|scope| {
        let mut workers = Vec::new();
        for lane in 0..8_u8 {
            let path = &path;
            workers.push(scope.spawn(move || {
                let (owner, obj, caller, _) = peers(path, &format!("fd.concurrent.{lane}"));
                for index in 0..16_u16 {
                    let seq = 200 + index;
                    let marker = lane.wrapping_mul(16) + index as u8;
                    let (read_end, write_end) = pipe().expect("concurrent pipe");
                    fd_send(
                        &caller,
                        &invoke(seq, obj, "ping", &[]),
                        Some(write_end.as_fd()),
                    );
                    drop(write_end);
                    let (forwarded, received) = owner
                        .recv_full_frame_with_fd(RECV)
                        .expect("concurrent INVOKE");
                    assert_eq!(forwarded.hdr.seq, seq);
                    assert_fd_marker(received, &read_end, marker);
                    fd_send(&owner, &owner_status_reply(&forwarded, obj), None);
                    expect_ok(&caller, seq);
                }
            }));
        }
        for worker in workers {
            worker.join().expect("concurrent descriptor route");
        }
    });
}

#[rstest]
#[case::upstream(BrokerArm::Upstream)]
#[case::rs(BrokerArm::Rs)]
fn fragmented_terminal_status_retains_reply_fd(
    #[case] arm: BrokerArm,
    #[values(1, 7, 8, 11, 12, 20)] split: usize,
) {
    let Some(harness) = start(arm) else { return };
    let (owner, obj, caller, _) = peers(harness.socket_path(), "fd.fragmented-status");
    fd_send(&caller, &invoke(105, obj, "ping", &[]), None);
    let (forwarded, _) = owner.recv_full_frame_with_fd(RECV).expect("INVOKE");
    let bytes = Codec::write_frame(&owner_status_reply(&forwarded, obj));
    let (read_end, write_end) = pipe().expect("fragmented reply pipe");
    owner
        .send_frame_with_fd(&bytes[..split], Some(write_end.as_fd()))
        .expect("reply first fragment");
    drop(write_end);
    let error = caller
        .recv_full_frame_with_fd(Duration::from_millis(10))
        .unwrap_err();
    assert!(matches!(
        error.kind(),
        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
    ));
    owner
        .send_frame_with_fd(&bytes[split..], None)
        .expect("remaining reply");
    let (reply, received) = caller
        .recv_full_frame_with_fd(RECV)
        .expect("fragmented reply STATUS");
    assert_eq!(reply.hdr.ty, MsgType::Status);
    assert_eq!(reply.hdr.seq, 105);
    assert_eq!(status_value(&reply), UBUS_STATUS_OK);
    assert_fd_marker(received, &read_end, b's');
}

#[rstest]
#[case::upstream(BrokerArm::Upstream)]
#[case::rs(BrokerArm::Rs)]
fn fd_on_invalid_owner_response_is_closed(
    #[case] arm: BrokerArm,
    #[values("missing-object", "missing-status", "missing-caller")] invalid: &str,
) {
    let Some(harness) = start(arm) else { return };
    let (owner, obj, caller, _) = peers(harness.socket_path(), "fd.invalid-status");
    fd_send(&caller, &invoke(106, obj, "ping", &[]), None);
    let (forwarded, _) = owner.recv_full_frame_with_fd(RECV).expect("INVOKE");
    let mut env = EnvBuilder::new();
    if invalid != "missing-object" {
        env.push_u32(UbusAttr::ObjId, obj);
    }
    if invalid != "missing-status" {
        env.push_u32(UbusAttr::Status, UBUS_STATUS_OK as u32);
    }
    let reply = Frame::to_peer(
        MsgType::Status,
        106,
        if invalid == "missing-caller" {
            0
        } else {
            forwarded.hdr.peer
        },
        env.finish(),
    );
    let (read_end, write_end) = pipe().expect("invalid STATUS pipe");
    fd_send(&owner, &reply, Some(write_end.as_fd()));
    drop(write_end);
    assert!(matches!(read_marker(&read_end, RECV), PipeMarker::Eof));
    // Invalid replies must neither satisfy nor disconnect the real caller.
    fd_send(&owner, &owner_status_reply(&forwarded, obj), None);
    expect_ok(&caller, 106);
}

#[rstest]
#[case::upstream(BrokerArm::Upstream)]
#[case::rs(BrokerArm::Rs)]
fn non_relay_message_types_close_received_descriptors(
    #[case] arm: BrokerArm,
    #[values(
        MsgType::Hello,
        MsgType::Data,
        MsgType::Lookup,
        MsgType::AddObject,
        MsgType::RemoveObject,
        MsgType::Subscribe,
        MsgType::Unsubscribe,
        MsgType::Notify,
        MsgType::Monitor
    )]
    ty: MsgType,
) {
    let Some(harness) = start(arm) else { return };
    let caller = FdPeer::connect(harness.socket_path()).expect("caller connect");
    fd_hello(&caller);
    let (read_end, write_end) = pipe().expect("non-relay pipe");
    fd_send(
        &caller,
        &Frame::to_broker(ty, 107, EnvBuilder::new().finish()),
        Some(write_end.as_fd()),
    );
    drop(write_end);
    assert!(
        matches!(read_marker(&read_end, RECV), PipeMarker::Eof),
        "{ty:?} must close a descriptor rather than retaining it"
    );
    let fresh = FdPeer::connect(harness.socket_path()).expect("fresh peer");
    fd_hello(&fresh);
}

/// Descriptor-free receive batches can span many complete frames plus a
/// partial header. A following SCM_RIGHTS must still bind only to its INVOKE.
#[rstest]
#[case::upstream(BrokerArm::Upstream)]
#[case::rs(BrokerArm::Rs)]
fn fd_after_large_plain_batch_keeps_its_frame(
    #[case] arm: BrokerArm,
    #[values(0_u16, 1, 64, 127)] plain: u16,
) {
    run_fd_after_plain_batch(arm, plain);
}

// The pinned upstream broker copies a queued external-buffer message without
// transferring its fd ownership, closing a queued descriptor prematurely. Keep
// that upstream bug out of the parity contract while verifying Rust's queue.
#[test]
fn queued_descriptor_after_large_plain_batch_remains_usable() {
    run_fd_after_plain_batch(BrokerArm::Rs, 1024);
}

fn run_fd_after_plain_batch(arm: BrokerArm, plain: u16) {
    let Some(harness) = start(arm) else { return };
    let (owner, obj, caller, _) = peers(harness.socket_path(), "fd.plain-batch");
    let mut batch = Vec::new();
    for index in 0..plain {
        batch.extend_from_slice(&Codec::write_frame(&invoke(400 + index, obj, "ping", &[])));
    }
    caller
        .send_frame_with_fd(&batch, None)
        .expect("plain batch");
    let (read_end, write_end) = pipe().expect("batched pipe");
    fd_send(
        &caller,
        &invoke(400 + plain, obj, "ping", &[]),
        Some(write_end.as_fd()),
    );
    drop(write_end);
    for index in 0..=plain {
        let (forwarded, received) = owner.recv_full_frame_with_fd(RECV).expect("batched INVOKE");
        assert_eq!(forwarded.hdr.ty, MsgType::Invoke);
        assert_eq!(forwarded.hdr.seq, 400 + index);
        if index == plain {
            assert_fd_marker(received, &read_end, b'b');
        } else {
            assert!(
                received.is_none(),
                "plain request {index} must not steal the trailing fd"
            );
        }
        fd_send(&owner, &owner_status_reply(&forwarded, obj), None);
    }
    for index in 0..=plain {
        expect_ok(&caller, 400 + index);
    }
}

#[rstest]
#[case::upstream(BrokerArm::Upstream)]
#[case::rs(BrokerArm::Rs)]
fn descriptor_in_large_invoke_body_is_closed(
    #[case] arm: BrokerArm,
    #[values(4095, 4096, 4097, 65535)] offset: usize,
) {
    let Some(harness) = start(arm) else { return };
    let (owner, obj, caller, _) = peers(harness.socket_path(), "fd.large-body");
    let frame = invoke(1500, obj, "ping", &vec![b'x'; 64 * 1024]);
    let bytes = Codec::write_frame(&frame);
    caller
        .send_frame_with_fd(&bytes[..offset], None)
        .expect("large plain prefix");
    let (read_end, write_end) = pipe().expect("large-body pipe");
    caller
        .send_frame_with_fd(&bytes[offset..offset + 1], Some(write_end.as_fd()))
        .expect("body descriptor byte");
    drop(write_end);
    // Pipe EOF observes the broker consuming the ancillary byte. Only then
    // queue the remaining body: upstream's edge-triggered body reader needs a
    // new readiness event after a short read ending at an ancillary barrier.
    assert!(matches!(read_marker(&read_end, RECV), PipeMarker::Eof));
    caller
        .send_frame_with_fd(&bytes[offset + 1..], None)
        .expect("remaining large body");
    let (forwarded, received) = owner.recv_full_frame_with_fd(RECV).expect("large INVOKE");
    assert_eq!(forwarded.hdr.ty, MsgType::Invoke);
    assert_eq!(forwarded.hdr.seq, 1500);
    assert!(
        received.is_none(),
        "fd inside a large body must not bind to the frame header"
    );
    assert!(matches!(read_marker(&read_end, RECV), PipeMarker::Eof));
    fd_send(&owner, &owner_status_reply(&forwarded, obj), None);
    expect_ok(&caller, 1500);
}
