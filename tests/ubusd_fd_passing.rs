//! SCM_RIGHTS behavior cannot traverse WireTap, so these are deliberately
//! direct-harness, two-arm behavioral probes.  A pipe round-trip proves that
//! the relayed descriptor works without depending on its process-local number.

#![cfg(feature = "testing")]

use std::path::Path;
use std::process::Command;
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
const SETTLE: Duration = Duration::from_millis(25);

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
    // ADD_OBJECT's DATA and STATUS may be adjacent in one recvmsg.  We only
    // need DATA's object id here; a separately received STATUS is harmless.
    let (data, fd) = peer.recv_full_frame_with_fd(RECV).expect("ADD_OBJECT DATA");
    assert_eq!(data.hdr.ty, MsgType::Data);
    assert_eq!(data.hdr.seq, seq);
    assert!(fd.is_none(), "ADD_OBJECT DATA fd");
    let obj = find(&data.payload, UbusAttr::ObjId)
        .and_then(|attr| attr.as_u32())
        .expect("ADD_OBJECT OBJID");
    // recv_full_frame_with_fd intentionally discards bytes after its decoded
    // frame. If STATUS arrived separately, consume it before the owner starts
    // receiving forwarded INVOKEs.
    match peer.recv_full_frame_with_fd(Duration::from_millis(100)) {
        Ok((status, fd)) => {
            assert_eq!(status.hdr.ty, MsgType::Status);
            assert_eq!(status.hdr.seq, seq);
            assert_eq!(status_value(&status), UBUS_STATUS_OK);
            assert!(fd.is_none(), "ADD_OBJECT STATUS fd");
        }
        Err(e)
            if e.kind() == std::io::ErrorKind::TimedOut
                || e.kind() == std::io::ErrorKind::WouldBlock => {}
        Err(e) => panic!("ADD_OBJECT STATUS: {e}"),
    }
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
    // Match `fd_register_typed`: the helper that decoded DATA may already
    // have discarded an adjacent STATUS from the same recvmsg.
    match peer.recv_full_frame_with_fd(Duration::from_millis(100)) {
        Ok((status, fd)) => {
            assert_eq!(status.hdr.ty, MsgType::Status);
            assert_eq!(status.hdr.seq, seq);
            assert_eq!(status_value(&status), UBUS_STATUS_OK);
            assert!(fd.is_none(), "anonymous ADD_OBJECT STATUS fd");
        }
        Err(e)
            if e.kind() == std::io::ErrorKind::TimedOut
                || e.kind() == std::io::ErrorKind::WouldBlock => {}
        Err(e) => panic!("anonymous ADD_OBJECT STATUS: {e}"),
    }
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

fn read_marker(read_end: &OwnedFd, settle: Duration) -> PipeMarker {
    std::thread::sleep(settle);
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
        read_marker(&read_end, SETTLE),
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
    // it; `recv_full_frame_with_fd` never has to retain a following STATUS
    // from the same recvmsg.
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
        read_marker(&read_end, SETTLE),
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
        read_marker(&read_end, SETTLE),
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
        read_marker(&read_end, SETTLE),
        PipeMarker::Marker(b's')
    ));
    host.shutdown().expect("libubox host shutdown");
}

/// Upstream forwards an fd on a NO_REPLY NOTIFY fan-out to its sole subscriber.
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
        matches!(read_marker(&read_end, SETTLE), PipeMarker::Eof),
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
        read_marker(&first_read, SETTLE),
        PipeMarker::Marker(b'1')
    ));
    assert!(matches!(read_marker(&second_read, SETTLE), PipeMarker::Eof));
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
    assert!(matches!(read_marker(&read_end, SETTLE), PipeMarker::Eof));
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
    assert!(matches!(read_marker(&read_end, SETTLE), PipeMarker::Eof));
}

fn current_uid() -> u32 {
    Command::new("id")
        .arg("-u")
        .output()
        .ok()
        .and_then(|out| String::from_utf8(out.stdout).ok())
        .and_then(|out| out.trim().parse().ok())
        .unwrap_or(0)
}

#[rstest]
#[case::upstream(BrokerArm::Upstream)]
#[case::rs(BrokerArm::Rs)]
fn fd_on_acl_denied_invoke_is_closed(#[case] arm: BrokerArm) {
    if current_uid() == 0 {
        skip::or_skip(
            "ubusd_fd_passing fd_on_acl_denied_invoke_is_closed",
            "no resolvable non-root identity (running as root?)",
        );
    }
    let acl = unique_socket_path("fd-acl").with_extension("d");
    std::fs::create_dir_all(&acl).expect("create acl dir");
    std::fs::write(acl.join("deny.json"), r#"{"user":"example-no-such-user"}"#).expect("write acl");
    let acl_path = acl.to_str().expect("utf-8 acl path");
    // Owner is root and may publish; caller is a non-root identity which has
    // no access grant. The shim assigns identities by accepted-peer order.
    let creds = "0:0:1,0:0:1,1000:1000:1";
    let started = match arm {
        BrokerArm::Upstream => {
            UbusdHarness::start_upstream_with_acl(acl_path, &[("UBUS_TESTKIT_PEERCRED_SEQ", creds)])
        }
        BrokerArm::Rs => {
            UbusdHarness::start_rust_with_acl(acl_path, &[("UBUS_TESTKIT_PEERCRED_SEQ", creds)])
        }
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
    assert!(matches!(read_marker(&read_end, SETTLE), PipeMarker::Eof));
    let _ = std::fs::remove_dir_all(acl);
}
