//! End-to-end exercises against the in-memory mock transport. Tests
//! run under std (host toolchain) so std::thread can drive the server
//! and client in parallel — closer to how real ubusd behaves.

use std::thread;

use ubus::blobmsg::{lookup, BlobBuilder, BlobIter, BlobMsgTable};
use ubus::client::UbusClient;
use ubus::envelope::{find, EnvBuilder, UbusAttr};
use ubus::error::UbusError;
use ubus::server::{DispatchOutcome, UbusConnection, UbusObject};
use ubus::transport::{MockTransport, Transport};

/// Wraps `MockTransport` so the client/server handshake loops, which
/// call `wait_recv()` between non-blocking recv polls, actually yield
/// to the broker-shim thread instead of spinning. Without it the
/// `IDLE_WAIT_BUDGET` cap (designed against the real `unix_sock`
/// transport's 50 ms park) trips in microseconds against a no-op
/// `wait_recv` and the loop times out before the broker is scheduled.
struct YieldingMock(MockTransport);

impl YieldingMock {
    fn pair() -> (Self, MockTransport) {
        let (a, b) = MockTransport::pair();
        (Self(a), b)
    }
}

impl Transport for YieldingMock {
    fn send(&mut self, frame: &Frame) -> Result<(), UbusError> {
        self.0.send(frame)
    }
    fn recv(&mut self) -> Result<Option<Frame>, UbusError> {
        self.0.recv()
    }
    fn wait_recv(&mut self) {
        thread::sleep(std::time::Duration::from_micros(100));
    }
}
use ubus::wire::{Frame, MsgType};

/// Bounded poll for the next frame of the requested type. The
/// integration suite drives both sides through `MockTransport` and
/// `thread::yield_now`; without a deadline a regression that fails to
/// emit `want` would hang the whole test binary (and CI) instead of
/// failing with context.
fn wait_for(t: &mut MockTransport, want: MsgType) -> Frame {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        match t.recv().expect("recv ok") {
            Some(f) if f.hdr.ty == want => return f,
            Some(_) => continue,
            None => {
                if std::time::Instant::now() >= deadline {
                    panic!("wait_for({want:?}) timed out after 5s");
                }
                thread::yield_now();
            }
        }
    }
}

/// Stand in for ubusd: HELLO acks with peer=`peer_id`, ADD_OBJECT acks
/// with DATA(ObjId=`object_id`) then STATUS=0.
fn run_fake_broker_register(broker: &mut MockTransport, object_id: u32, peer_id: u32) {
    let hello = wait_for(broker, MsgType::Hello);
    broker
        .send(&Frame::to_peer(
            MsgType::Hello,
            hello.hdr.seq,
            peer_id,
            vec![],
        ))
        .unwrap();
    let add = wait_for(broker, MsgType::AddObject);

    let mut data_env = EnvBuilder::new();
    data_env.push_u32(UbusAttr::ObjId, object_id);
    broker
        .send(&Frame::to_peer(
            MsgType::Data,
            add.hdr.seq,
            add.hdr.peer,
            data_env.finish(),
        ))
        .unwrap();

    let mut status_env = EnvBuilder::new();
    status_env.push_i32(UbusAttr::Status, 0);
    broker
        .send(&Frame::to_peer(
            MsgType::Status,
            add.hdr.seq,
            add.hdr.peer,
            status_env.finish(),
        ))
        .unwrap();
}

#[test]
fn iter_walk_array_payload() {
    let mut b = BlobBuilder::new();
    b.open_array(None);
    for s in &["a", "b", "c"] {
        b.put_str(None, s);
    }
    b.close_array();
    let buf = b.finish();
    let outer = BlobIter::new(&buf).next().unwrap();
    let items: Vec<&str> = BlobIter::new(outer.payload)
        .filter_map(|a| a.as_str())
        .collect();
    assert_eq!(items, vec!["a", "b", "c"]);
}

#[test]
fn register_completes_via_data_then_status() {
    let (server_t, mut broker_t) = YieldingMock::pair();
    let broker = thread::spawn(move || run_fake_broker_register(&mut broker_t, 7, 42));

    let obj = UbusObject::new("example").method("ping", |_| Ok(BlobMsgTable::empty()));
    let _conn = UbusConnection::connect_and_register(server_t, obj).expect("register");

    broker.join().unwrap();
}

#[test]
fn register_completes_via_status_only() {
    let (server_t, mut broker_t) = YieldingMock::pair();
    let broker = thread::spawn(move || {
        let hello = wait_for(&mut broker_t, MsgType::Hello);
        broker_t
            .send(&Frame::to_peer(MsgType::Hello, hello.hdr.seq, 99, vec![]))
            .unwrap();
        let add = wait_for(&mut broker_t, MsgType::AddObject);
        let mut env = EnvBuilder::new();
        env.push_u32(UbusAttr::ObjId, 11);
        env.push_i32(UbusAttr::Status, 0);
        broker_t
            .send(&Frame::to_peer(
                MsgType::Status,
                add.hdr.seq,
                add.hdr.peer,
                env.finish(),
            ))
            .unwrap();
    });

    let obj = UbusObject::new("example");
    let _conn = UbusConnection::connect_and_register(server_t, obj).expect("register");
    broker.join().unwrap();
}

#[test]
fn register_returns_status_error_on_nonzero() {
    let (server_t, mut broker_t) = YieldingMock::pair();
    let broker = thread::spawn(move || {
        let hello = wait_for(&mut broker_t, MsgType::Hello);
        broker_t
            .send(&Frame::to_peer(MsgType::Hello, hello.hdr.seq, 5, vec![]))
            .unwrap();
        let add = wait_for(&mut broker_t, MsgType::AddObject);
        let mut env = EnvBuilder::new();
        env.push_i32(UbusAttr::Status, -7);
        broker_t
            .send(&Frame::to_peer(
                MsgType::Status,
                add.hdr.seq,
                add.hdr.peer,
                env.finish(),
            ))
            .unwrap();
    });

    let obj = UbusObject::new("example");
    let err = match UbusConnection::connect_and_register(server_t, obj) {
        Err(e) => e,
        Ok(_) => panic!("non-zero status must error"),
    };
    assert!(matches!(err, UbusError::Status(-7)), "got {err:?}");
    broker.join().unwrap();
}

/// HELLO reply with `peer=0` is the broker-as-destination convention,
/// not a valid local id assignment. Surface as Malformed; otherwise a
/// later REMOVE_OBJECT would be addressed at ubusd-as-broker and
/// silently dropped.
#[test]
fn register_rejects_hello_peer_zero() {
    let (server_t, mut broker_t) = YieldingMock::pair();
    let broker = thread::spawn(move || {
        let hello = wait_for(&mut broker_t, MsgType::Hello);
        broker_t
            .send(&Frame::to_peer(MsgType::Hello, hello.hdr.seq, 0, vec![]))
            .unwrap();
    });
    let err = match UbusConnection::connect_and_register(server_t, UbusObject::new("example")) {
        Err(e) => e,
        Ok(_) => panic!("peer=0 must error"),
    };
    assert!(matches!(err, UbusError::Malformed(_)), "got {err:?}");
    broker.join().unwrap();
}

/// STATUS=0 with no ObjId in any preceding DATA or in the STATUS
/// itself is a wire violation that previously would have left
/// `object_id = None` and silently produced a "ghost server" that
/// dispatches no INVOKEs.
#[test]
fn register_rejects_status_zero_without_obj_id() {
    let (server_t, mut broker_t) = YieldingMock::pair();
    let broker = thread::spawn(move || {
        let hello = wait_for(&mut broker_t, MsgType::Hello);
        broker_t
            .send(&Frame::to_peer(MsgType::Hello, hello.hdr.seq, 5, vec![]))
            .unwrap();
        let add = wait_for(&mut broker_t, MsgType::AddObject);
        let mut env = EnvBuilder::new();
        env.push_i32(UbusAttr::Status, 0);
        broker_t
            .send(&Frame::to_peer(
                MsgType::Status,
                add.hdr.seq,
                add.hdr.peer,
                env.finish(),
            ))
            .unwrap();
    });
    let err = match UbusConnection::connect_and_register(server_t, UbusObject::new("example")) {
        Err(e) => e,
        Ok(_) => panic!("missing ObjId must error"),
    };
    assert!(matches!(err, UbusError::Malformed(_)), "got {err:?}");
    broker.join().unwrap();
}

#[test]
fn client_connect_completes_after_hello_ack() {
    let (client_t, mut broker_t) = YieldingMock::pair();
    let broker = thread::spawn(move || {
        let hello = wait_for(&mut broker_t, MsgType::Hello);
        broker_t
            .send(&Frame::to_peer(MsgType::Hello, hello.hdr.seq, 17, vec![]))
            .unwrap();
    });

    let _client = UbusClient::connect(client_t).expect("hello handshake");
    broker.join().unwrap();
}

#[test]
fn lookup_returns_obj_id_from_data_then_status() {
    let (client_t, mut broker_t) = YieldingMock::pair();
    let broker = thread::spawn(move || {
        let hello = wait_for(&mut broker_t, MsgType::Hello);
        broker_t
            .send(&Frame::to_peer(MsgType::Hello, hello.hdr.seq, 17, vec![]))
            .unwrap();

        let lookup = wait_for(&mut broker_t, MsgType::Lookup);
        assert_eq!(
            find(&lookup.payload, UbusAttr::ObjPath).and_then(|a| a.as_str()),
            Some("network.interface.wwan")
        );

        let mut data_env = EnvBuilder::new();
        data_env.push_u32(UbusAttr::ObjId, 77);
        broker_t
            .send(&Frame::to_peer(
                MsgType::Data,
                lookup.hdr.seq,
                lookup.hdr.peer,
                data_env.finish(),
            ))
            .unwrap();

        let mut status_env = EnvBuilder::new();
        status_env.push_i32(UbusAttr::Status, 0);
        broker_t
            .send(&Frame::to_peer(
                MsgType::Status,
                lookup.hdr.seq,
                lookup.hdr.peer,
                status_env.finish(),
            ))
            .unwrap();
    });

    let mut client = UbusClient::connect(client_t).expect("hello handshake");
    let obj_id = client.lookup("network.interface.wwan").expect("lookup ok");
    assert_eq!(obj_id, 77);
    broker.join().unwrap();
}

#[test]
fn lookup_surfaces_nonzero_status() {
    let (client_t, mut broker_t) = YieldingMock::pair();
    let broker = thread::spawn(move || {
        let hello = wait_for(&mut broker_t, MsgType::Hello);
        broker_t
            .send(&Frame::to_peer(MsgType::Hello, hello.hdr.seq, 17, vec![]))
            .unwrap();

        let lookup = wait_for(&mut broker_t, MsgType::Lookup);
        let mut status_env = EnvBuilder::new();
        status_env.push_i32(UbusAttr::Status, -5);
        broker_t
            .send(&Frame::to_peer(
                MsgType::Status,
                lookup.hdr.seq,
                lookup.hdr.peer,
                status_env.finish(),
            ))
            .unwrap();
    });

    let mut client = UbusClient::connect(client_t).expect("hello handshake");
    let err = client
        .lookup("network.interface.wwan")
        .expect_err("non-zero status must surface");
    assert!(matches!(err, UbusError::Status(-5)), "got {err:?}");
    broker.join().unwrap();
}

#[test]
fn register_signature_includes_method_names() {
    let (server_t, mut broker_t) = YieldingMock::pair();
    let captured = std::sync::Arc::new(std::sync::Mutex::new(None));
    let captured_for_thread = captured.clone();

    let broker = thread::spawn(move || {
        let hello = wait_for(&mut broker_t, MsgType::Hello);
        broker_t
            .send(&Frame::to_peer(MsgType::Hello, hello.hdr.seq, 1, vec![]))
            .unwrap();
        let add = wait_for(&mut broker_t, MsgType::AddObject);
        let sig_attr = find(&add.payload, UbusAttr::Signature)
            .map(|a| a.as_blobmsg().to_vec())
            .expect("signature attr present");
        *captured_for_thread.lock().unwrap() = Some(sig_attr);
        let mut env = EnvBuilder::new();
        env.push_u32(UbusAttr::ObjId, 1);
        env.push_i32(UbusAttr::Status, 0);
        broker_t
            .send(&Frame::to_peer(
                MsgType::Status,
                add.hdr.seq,
                add.hdr.peer,
                env.finish(),
            ))
            .unwrap();
    });

    let obj = UbusObject::new("example")
        .method("alpha", |_| Ok(BlobMsgTable::empty()))
        .method("beta", |_| Ok(BlobMsgTable::empty()));
    let _conn = UbusConnection::connect_and_register(server_t, obj).unwrap();
    broker.join().unwrap();

    let sig = captured.lock().unwrap().clone().expect("captured");
    let names: Vec<String> = BlobIter::new(&sig)
        .filter_map(|a| a.name.map(String::from))
        .collect();
    assert_eq!(names, vec!["alpha", "beta"]);
}

#[test]
fn unregister_emits_remove_object_for_registered_id() {
    let (server_t, mut broker_t) = YieldingMock::pair();
    let broker = thread::spawn(move || {
        let hello = wait_for(&mut broker_t, MsgType::Hello);
        broker_t
            .send(&Frame::to_peer(MsgType::Hello, hello.hdr.seq, 4, vec![]))
            .unwrap();

        let add = wait_for(&mut broker_t, MsgType::AddObject);
        let mut env = EnvBuilder::new();
        env.push_u32(UbusAttr::ObjId, 55);
        env.push_i32(UbusAttr::Status, 0);
        broker_t
            .send(&Frame::to_peer(
                MsgType::Status,
                add.hdr.seq,
                add.hdr.peer,
                env.finish(),
            ))
            .unwrap();

        let remove = wait_for(&mut broker_t, MsgType::RemoveObject);
        assert_eq!(remove.hdr.peer, 0, "REMOVE_OBJECT must go to broker");
        assert_eq!(
            find(&remove.payload, UbusAttr::ObjId).and_then(|a| a.as_u32()),
            Some(55)
        );
    });

    let obj = UbusObject::new("example");
    let mut conn = UbusConnection::connect_and_register(server_t, obj).expect("register");
    conn.unregister().expect("unregister");
    broker.join().unwrap();
}

/// Full INVOKE handler round-trip. The cli/srv `MockTransport` pair
/// is direct (no broker simulator in between), so the test bypasses
/// the broker handshake via `with_object_id` / `without_hello`. The
/// dispatch loop and reply-stripping logic are what's under test.
#[test]
fn invoke_round_trip_via_typed_layer() {
    let (cli_t, srv_t) = YieldingMock::pair();

    let server = thread::spawn(move || {
        let obj = UbusObject::new("example").method("status_serving_cell", |_args: &[u8]| {
            Ok(BlobMsgTable::build(|b| {
                b.put_str(Some("plmn"), "26201");
                b.put_i32(Some("tac"), 0x1234);
                b.put_str(Some("act"), "lte");
            }))
        });
        let mut srv = UbusConnection::with_object_id(srv_t, obj, 7);
        loop {
            match srv.poll_one().expect("poll_one") {
                DispatchOutcome::Handled => break,
                DispatchOutcome::Idle => thread::yield_now(),
                other => panic!("unexpected: {other:?}"),
            }
        }
    });

    let mut cli = UbusClient::without_hello(cli_t);
    let reply = cli
        .invoke(7, "status_serving_cell", &BlobMsgTable::empty())
        .unwrap();
    assert_eq!(
        lookup(&reply, "plmn").and_then(|a| a.as_str()),
        Some("26201")
    );
    assert_eq!(lookup(&reply, "tac").and_then(|a| a.as_i32()), Some(0x1234));
    assert_eq!(lookup(&reply, "act").and_then(|a| a.as_str()), Some("lte"));
    server.join().unwrap();
}

#[test]
fn invoke_surfaces_handler_error_status() {
    let (cli_t, srv_t) = YieldingMock::pair();
    let server = thread::spawn(move || {
        let obj =
            UbusObject::new("example").method("fail", |_| Err(UbusError::Malformed("intentional")));
        let mut srv = UbusConnection::with_object_id(srv_t, obj, 7);
        loop {
            match srv.poll_one().expect("poll_one") {
                DispatchOutcome::HandlerErr(_) => break,
                DispatchOutcome::Idle => thread::yield_now(),
                other => panic!("unexpected: {other:?}"),
            }
        }
    });

    let mut cli = UbusClient::without_hello(cli_t);
    let err = cli
        .invoke(7, "fail", &BlobMsgTable::empty())
        .expect_err("handler error");
    // Sign round-trips: handler Err → server STATUS=-3 → client surfaces
    // Status(-3) (i32 → u32 → i32 must preserve sign).
    assert!(matches!(err, UbusError::Status(-3)), "got {err:?}");
    server.join().unwrap();
}

#[test]
fn invoke_surfaces_unknown_method_status_minus_two() {
    let (cli_t, srv_t) = YieldingMock::pair();
    let server = thread::spawn(move || {
        let obj = UbusObject::new("example").method("known", |_| Ok(BlobMsgTable::empty()));
        let mut srv = UbusConnection::with_object_id(srv_t, obj, 7);
        loop {
            match srv.poll_one().expect("poll_one") {
                DispatchOutcome::Handled => break,
                DispatchOutcome::Idle => thread::yield_now(),
                other => panic!("unexpected: {other:?}"),
            }
        }
    });

    let mut cli = UbusClient::without_hello(cli_t);
    let err = cli
        .invoke(7, "unknown", &BlobMsgTable::empty())
        .expect_err("unknown method");
    assert!(matches!(err, UbusError::Status(-2)), "got {err:?}");
    server.join().unwrap();
}

#[test]
fn invoke_with_empty_handler_reply_returns_empty_vec() {
    let (cli_t, srv_t) = YieldingMock::pair();
    let server = thread::spawn(move || {
        // Handler returns empty TABLE; client strips the outer wrapper
        // and the inner attr stream is empty.
        let obj = UbusObject::new("example").method("noop", |_| Ok(BlobMsgTable::empty()));
        let mut srv = UbusConnection::with_object_id(srv_t, obj, 7);
        loop {
            match srv.poll_one().expect("poll_one") {
                DispatchOutcome::Handled => break,
                DispatchOutcome::Idle => thread::yield_now(),
                other => panic!("unexpected: {other:?}"),
            }
        }
    });

    let mut cli = UbusClient::without_hello(cli_t);
    let reply = cli
        .invoke(7, "noop", &BlobMsgTable::empty())
        .expect("noop ok");
    assert!(
        reply.is_empty(),
        "expected empty reply, got {} bytes",
        reply.len()
    );
    server.join().unwrap();
}
