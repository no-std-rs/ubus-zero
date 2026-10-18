//! Client dispatch behavior exercised against both broker implementations.
#![cfg(feature = "testing")]

use rstest::rstest;
use std::time::Duration;
use ubus::blobmsg::lookup;
use ubus::client::UbusClient;
use ubus::envelope::{find, UbusAttr};
use ubus::server::UbusObject;
use ubus::{BlobMsgTable, UbusError};
use ubus_testkit::{skip, BrokerArm, UbusdHarness, UnixUbusTransport};
#[allow(dead_code)]
mod common;
use common::OwnerThread;

fn broker(arm: BrokerArm) -> Option<UbusdHarness> {
    let harness = match arm {
        BrokerArm::Upstream => UbusdHarness::start_upstream(),
        BrokerArm::Rs => UbusdHarness::start_rust(),
    }
    .unwrap();
    if harness.is_none() {
        skip::or_skip("client reply dispatch", "broker fixture not available");
    }
    harness
}

fn connect(harness: &UbusdHarness) -> UbusClient<UnixUbusTransport> {
    UbusClient::connect(UnixUbusTransport::connect(harness.socket_path()).unwrap()).unwrap()
}

#[rstest]
#[case::upstream(BrokerArm::Upstream)]
#[case::rust(BrokerArm::Rs)]
fn pipelined_invocations_can_be_received_in_reverse_order(#[case] arm: BrokerArm) {
    let Some(harness) = broker(arm) else { return };
    let owner = OwnerThread::spawn_with_deadline(
        harness.socket_path().to_str().unwrap().into(),
        Duration::from_secs(30),
        || {
            UbusObject::new("audit.dispatch").method("echo", |args| {
                let value = lookup(args, "value").unwrap().as_i32().unwrap();
                Ok(BlobMsgTable::build(|b| b.put_i32(Some("value"), value)))
            })
        },
    );
    owner.wait_ready();
    let mut client = connect(&harness);
    let object = client.lookup("audit.dispatch").unwrap();
    let first = client
        .send_invoke(
            object,
            "echo",
            &BlobMsgTable::build(|b| b.put_i32(Some("value"), 1)),
        )
        .unwrap();
    let second = client
        .send_invoke(
            object,
            "echo",
            &BlobMsgTable::build(|b| b.put_i32(Some("value"), 2)),
        )
        .unwrap();
    for (seq, value) in [(second, 2), (first, 1)] {
        let reply = client.recv_invoke_reply(seq).unwrap();
        assert_eq!(lookup(&reply, "value").unwrap().as_i32(), Some(value));
    }
    owner.shutdown();
}

#[rstest]
#[case::upstream(BrokerArm::Upstream)]
#[case::rust(BrokerArm::Rs)]
fn lookup_preserves_a_preceding_event(#[case] arm: BrokerArm) {
    let Some(harness) = broker(arm) else { return };
    let mut listener = connect(&harness);
    let object = listener.add_anonymous_object().unwrap();
    listener
        .register_event(object, "audit.dispatch.event")
        .unwrap();
    let mut sender = connect(&harness);
    sender
        .send_event("audit.dispatch.event", &BlobMsgTable::empty())
        .unwrap();
    assert!(matches!(
        listener.lookup("audit.no.such.object"),
        Err(UbusError::Status(4))
    ));
    // A redundant HELLO status may precede the event in the preserved queue.
    let mut found = false;
    for _ in 0..4 {
        if let Some(frame) = listener.recv_frame().unwrap() {
            if find(&frame.payload, UbusAttr::Method).and_then(|a| a.as_str())
                == Some("audit.dispatch.event")
            {
                found = true;
                break;
            }
        }
    }
    assert!(found, "lookup discarded the listener's event");
}
