//! Concurrent-client parity: both brokers must preserve reply routing and
//! object ownership while registrations and subscriptions churn.

#![cfg(feature = "testing")]

use std::sync::{mpsc, Arc, Barrier};
use std::thread;
use std::time::Duration;

use rstest::rstest;
use ubus::blobmsg::{lookup as blobmsg_lookup, BlobMsgTable};
use ubus::client::UbusClient;
use ubus::envelope::{find, EnvBuilder, UbusAttr};
use ubus::error::UbusError;
use ubus::server::UbusObject;
use ubus::transport::Transport;
use ubus::wire::{Frame, Header, MsgType, PROTO_VERSION, UBUS_STATUS_NOT_FOUND, UBUS_STATUS_OK};
use ubus_testkit::UnixUbusTransport;
use ubus_testkit::{skip, UbusdHarness};

#[allow(dead_code)]
mod common;
use common::{
    expect_typed_data_then_status_ok, raw_add_object_with_signature, recv_until,
    signature_with_ping_method, OwnerThread,
};

#[derive(Clone, Copy, Debug)]
enum Arm {
    Upstream,
    Rs,
}

struct WorkerKeep {
    // These handles intentionally stay open until the driver's object-set
    // oracle has run; the raw connection owns `conc.w{0,1}`.
    raw: Option<UnixUbusTransport>,
    client: Option<UbusClient<UnixUbusTransport>>,
}

impl WorkerKeep {
    fn has_live_connections(&self) -> bool {
        self.raw.is_some() && self.client.is_some()
    }
}

fn start(arm: Arm) -> Option<UbusdHarness> {
    let start: fn() -> std::io::Result<Option<UbusdHarness>> = match arm {
        Arm::Upstream => UbusdHarness::start_upstream,
        Arm::Rs => UbusdHarness::start_rust,
    };
    match start() {
        Ok(Some(harness)) => Some(harness),
        Ok(None) => {
            skip::or_skip(
                "ubusd_concurrency concurrent_clients_register_invoke_subscribe_churn",
                "broker binary not reachable",
            );
            None
        }
        Err(error) => panic!("broker start: {error:?}"),
    }
}

fn expect_status_ok(transport: &mut UnixUbusTransport, seq: u16, what: &str) {
    let frame = recv_until(transport, Duration::from_secs(5), |frame| {
        frame.hdr.seq == seq && frame.hdr.ty == MsgType::Status
    })
    .unwrap_or_else(|error| panic!("{what}: {error}"));
    assert_eq!(
        find(&frame.payload, UbusAttr::Status)
            .and_then(|attr| attr.as_u32())
            .map(|status| status as i32),
        Some(UBUS_STATUS_OK),
        "{what} STATUS"
    );
}

fn add_anonymous(transport: &mut UnixUbusTransport, seq: u16) -> u32 {
    transport
        .send(&Frame {
            hdr: Header {
                version: PROTO_VERSION,
                ty: MsgType::AddObject,
                seq,
                peer: 0,
            },
            payload: EnvBuilder::new().finish(),
        })
        .expect("send anonymous ADD_OBJECT");
    let frame = recv_until(transport, Duration::from_secs(5), |frame| {
        frame.hdr.seq == seq && frame.hdr.ty == MsgType::Data
    })
    .expect("anonymous ADD_OBJECT DATA");
    let object = find(&frame.payload, UbusAttr::ObjId)
        .and_then(|attr| attr.as_u32())
        .expect("anonymous ADD_OBJECT OBJID");
    expect_status_ok(transport, seq, "anonymous ADD_OBJECT");
    object
}

fn subscribe(transport: &mut UnixUbusTransport, seq: u16, object: u32, target: u32) {
    let mut env = EnvBuilder::new();
    env.push_u32(UbusAttr::ObjId, object);
    env.push_u32(UbusAttr::Target, target);
    transport
        .send(&Frame {
            hdr: Header {
                version: PROTO_VERSION,
                ty: MsgType::Subscribe,
                seq,
                peer: 0,
            },
            payload: env.finish(),
        })
        .expect("send SUBSCRIBE");
    expect_status_ok(transport, seq, "SUBSCRIBE");
}

fn unsubscribe(transport: &mut UnixUbusTransport, seq: u16, object: u32, target: u32) {
    let mut env = EnvBuilder::new();
    env.push_u32(UbusAttr::ObjId, object);
    env.push_u32(UbusAttr::Target, target);
    transport
        .send(&Frame {
            hdr: Header {
                version: PROTO_VERSION,
                ty: MsgType::Unsubscribe,
                seq,
                peer: 0,
            },
            payload: env.finish(),
        })
        .expect("send UNSUBSCRIBE");
    expect_status_ok(transport, seq, "UNSUBSCRIBE");
}

fn remove_object(transport: &mut UnixUbusTransport, seq: u16, object: u32) {
    let mut env = EnvBuilder::new();
    env.push_u32(UbusAttr::ObjId, object);
    transport
        .send(&Frame {
            hdr: Header {
                version: PROTO_VERSION,
                ty: MsgType::RemoveObject,
                seq,
                peer: 0,
            },
            payload: env.finish(),
        })
        .expect("send REMOVE_OBJECT");
    let (removed, _) = expect_typed_data_then_status_ok(transport, seq, "REMOVE_OBJECT");
    assert_eq!(removed, object, "REMOVE_OBJECT DATA obj id");
}

fn worker(path: String, worker: usize, barrier: Arc<Barrier>) -> Result<WorkerKeep, String> {
    let client_transport = UnixUbusTransport::connect(&path)
        .map_err(|error| format!("w{worker}: client connect: {error:?}"))?;
    let mut client = UbusClient::connect(client_transport)
        .map_err(|error| format!("w{worker}: client hello: {error:?}"))?;
    let echo = client
        .lookup("conc.echo")
        .map_err(|error| format!("w{worker}: lookup conc.echo: {error:?}"))?;

    let mut raw = UnixUbusTransport::connect(&path)
        .map_err(|error| format!("w{worker}: raw connect: {error:?}"))?;
    let name = format!("conc.w{worker}");
    let (owned, _) =
        raw_add_object_with_signature(&mut raw, 10, &name, &signature_with_ping_method());
    let anonymous = add_anonymous(&mut raw, 11);

    barrier.wait();

    for iteration in 0..25 {
        let tag = format!("w{worker}-{iteration}");
        let args = BlobMsgTable::build(|builder| builder.put_str(Some("tag"), &tag));
        let reply = client
            .invoke(echo, "echo", &args)
            .map_err(|error| format!("w{worker}: echo {iteration}: {error:?}"))?;
        if blobmsg_lookup(&reply, "tag").and_then(|attr| attr.as_str()) != Some(tag.as_str()) {
            return Err(format!(
                "w{worker}: echo {iteration} reply was routed incorrectly"
            ));
        }
    }

    for iteration in 0..5 {
        let sibling = format!("conc.w{}", (worker + 1) % 4);
        let target = client
            .lookup(&sibling)
            .map_err(|error| format!("w{worker}: lookup {sibling}: {error:?}"))?;
        let seq = 100 + (iteration as u16 * 2);
        subscribe(&mut raw, seq, anonymous, target);
        unsubscribe(&mut raw, seq + 1, anonymous, target);
    }

    // Second rendezvous: workers 2/3 tear their objects down next, and a
    // sibling still inside its churn loop would otherwise race that
    // removal into a NOT_FOUND on lookup/subscribe/unsubscribe. All
    // churn completes before any removal starts.
    barrier.wait();

    if worker >= 2 {
        remove_object(&mut raw, 200, owned);
        drop(raw);
        drop(client);
        Ok(WorkerKeep {
            raw: None,
            client: None,
        })
    } else {
        Ok(WorkerKeep {
            raw: Some(raw),
            client: Some(client),
        })
    }
}

fn lookup_not_found(client: &mut UbusClient<UnixUbusTransport>, object: &str) {
    match client.lookup(object) {
        Err(UbusError::Status(status)) if status == UBUS_STATUS_NOT_FOUND => {}
        result => panic!("{object} must be absent, got {result:?}"),
    }
}

#[rstest]
#[case::upstream(Arm::Upstream)]
#[case::rs(Arm::Rs)]
fn concurrent_clients_register_invoke_subscribe_churn(#[case] arm: Arm) {
    let Some(harness) = start(arm) else {
        return;
    };
    let path = harness
        .socket_path()
        .to_str()
        .expect("utf-8 socket")
        .to_owned();
    let owner = OwnerThread::spawn_with_deadline(path.clone(), Duration::from_secs(15), || {
        UbusObject::new("conc.echo").method("echo", |args| {
            let tag = blobmsg_lookup(args, "tag")
                .and_then(|attr| attr.as_str())
                .expect("echo tag")
                .to_owned();
            Ok(BlobMsgTable::build(|builder| {
                builder.put_str(Some("tag"), &tag);
            }))
        })
    });
    owner.wait_ready();

    let barrier = Arc::new(Barrier::new(4));
    let mut workers = Vec::new();
    for worker_id in 0..4 {
        let (result_tx, result_rx) = mpsc::channel();
        let worker_path = path.clone();
        let worker_barrier = Arc::clone(&barrier);
        let handle = thread::spawn(move || {
            result_tx
                .send(worker(worker_path, worker_id, worker_barrier))
                .expect("driver receives worker result");
        });
        workers.push((handle, result_rx));
    }

    let mut kept = Vec::new();
    let mut worker_errors = Vec::new();
    for (worker_id, (_, result_rx)) in workers.iter().enumerate() {
        match result_rx.recv_timeout(Duration::from_secs(10)) {
            Ok(Ok(connections)) => kept.push(connections),
            Ok(Err(error)) => worker_errors.push(error),
            Err(error) => worker_errors.push(format!("w{worker_id}: result timeout: {error}")),
        }
    }
    for (handle, _) in workers {
        handle.join().expect("worker thread panicked");
    }
    assert!(
        worker_errors.is_empty(),
        "worker failures: {worker_errors:?}"
    );
    assert_eq!(
        kept.iter()
            .filter(|connections| connections.has_live_connections())
            .count(),
        2,
        "only w0 and w1 retain connections"
    );

    let mut liveness = UnixUbusTransport::connect(&path).expect("liveness connect");
    liveness
        .send(&Frame::to_peer(
            MsgType::Ping,
            9999,
            0,
            EnvBuilder::new().finish(),
        ))
        .expect("send liveness PING");
    recv_until(&mut liveness, Duration::from_secs(5), |frame| {
        frame.hdr.ty == MsgType::Data && frame.hdr.seq == 9999
    })
    .expect("PING DATA echo");

    let transport = UnixUbusTransport::connect(&path).expect("oracle connect");
    let mut oracle = UbusClient::connect(transport).expect("oracle hello");
    let echo = oracle.lookup("conc.echo").expect("conc.echo survives");
    oracle.lookup("conc.w0").expect("conc.w0 survives");
    oracle.lookup("conc.w1").expect("conc.w1 survives");
    lookup_not_found(&mut oracle, "conc.w2");
    lookup_not_found(&mut oracle, "conc.w3");
    let args = BlobMsgTable::build(|builder| builder.put_str(Some("tag"), "final"));
    let reply = oracle.invoke(echo, "echo", &args).expect("final echo");
    assert_eq!(
        blobmsg_lookup(&reply, "tag").and_then(|attr| attr.as_str()),
        Some("final"),
        "final echo reply"
    );

    drop(kept);
    owner.shutdown();
}
