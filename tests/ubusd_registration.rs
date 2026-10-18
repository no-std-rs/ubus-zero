//! Registration/lifecycle contracts checked against the pinned C broker.
#![cfg(feature = "testing")]

use std::time::Duration;

use rstest::rstest;
use ubus::envelope::{find, read_status, EnvBuilder, UbusAttr};
use ubus::transport::Transport;
use ubus::wire::{Frame, MsgType};
use ubus_testkit::{skip, BrokerArm, UbusdHarness, UnixUbusTransport};

#[allow(dead_code)]
mod common;
use common::{raw_add_object_with_signature, recv_until, signature_with_ping_method};

fn broker(arm: BrokerArm) -> Option<UbusdHarness> {
    let result = match arm {
        BrokerArm::Upstream => UbusdHarness::start_upstream(),
        BrokerArm::Rs => UbusdHarness::start_rust(),
    }
    .expect("start broker");
    if result.is_none() {
        skip::or_skip("registration lifecycle", "broker fixture not available");
    }
    result
}

fn connect(harness: &UbusdHarness) -> UnixUbusTransport {
    let mut transport = UnixUbusTransport::connect(harness.socket_path()).unwrap();
    recv_until(&mut transport, Duration::from_secs(2), |f| {
        f.hdr.ty == MsgType::Hello
    })
    .unwrap();
    transport
}

fn request(
    transport: &mut UnixUbusTransport,
    ty: MsgType,
    seq: u16,
    env: EnvBuilder,
) -> (Vec<Frame>, i32) {
    transport
        .send(&Frame::to_broker(ty, seq, env.finish()))
        .unwrap();
    let mut data = Vec::new();
    loop {
        let frame = recv_until(transport, Duration::from_secs(2), |f| {
            f.hdr.seq == seq && matches!(f.hdr.ty, MsgType::Data | MsgType::Status)
        })
        .unwrap();
        if frame.hdr.ty == MsgType::Status {
            return (data, read_status(&frame.payload).unwrap());
        }
        data.push(frame);
    }
}

fn remove(transport: &mut UnixUbusTransport, seq: u16, id: u32) {
    let mut env = EnvBuilder::new();
    env.push_u32(UbusAttr::ObjId, id);
    assert_eq!(request(transport, MsgType::RemoveObject, seq, env).1, 0);
}

#[rstest]
#[case::upstream(BrokerArm::Upstream)]
#[case::rust(BrokerArm::Rs)]
fn shared_object_types_keep_their_signature_until_the_last_object_is_removed(
    #[case] arm: BrokerArm,
) {
    let Some(harness) = broker(arm) else { return };
    let mut owner = connect(&harness);
    let signature = signature_with_ping_method();
    let (original, type_id) =
        raw_add_object_with_signature(&mut owner, 10, "audit.type.original", &signature);
    let mut env = EnvBuilder::new();
    env.push_str(UbusAttr::ObjPath, "audit.type.shared");
    env.push_u32(UbusAttr::ObjType, type_id);
    let (data, status) = request(&mut owner, MsgType::AddObject, 11, env);
    assert_eq!(status, 0);
    assert_eq!(data.len(), 1);
    // C only returns OBJTYPE when SIGNATURE was included in ADD_OBJECT.
    assert!(find(&data[0].payload, UbusAttr::ObjType).is_none());
    let shared = find(&data[0].payload, UbusAttr::ObjId)
        .unwrap()
        .as_u32()
        .unwrap();
    remove(&mut owner, 12, original);

    let mut env = EnvBuilder::new();
    env.push_str(UbusAttr::ObjPath, "audit.type.shared");
    let (data, status) = request(&mut owner, MsgType::Lookup, 13, env);
    assert_eq!(status, 0);
    assert_eq!(data.len(), 1);
    assert_eq!(
        find(&data[0].payload, UbusAttr::ObjType).unwrap().as_u32(),
        Some(type_id)
    );
    assert_eq!(
        find(&data[0].payload, UbusAttr::Signature).unwrap().payload,
        signature
    );

    // Pathless objects may share types too; OBJTYPE takes priority over a
    // different supplied signature, and the response echoes the shared ID.
    let mut env = EnvBuilder::new();
    env.push_u32(UbusAttr::ObjType, type_id);
    env.push_signature(&[]);
    let (data, status) = request(&mut owner, MsgType::AddObject, 14, env);
    assert_eq!(status, 0);
    assert_eq!(
        find(&data[0].payload, UbusAttr::ObjType).unwrap().as_u32(),
        Some(type_id)
    );
    let anonymous = find(&data[0].payload, UbusAttr::ObjId)
        .unwrap()
        .as_u32()
        .unwrap();
    remove(&mut owner, 15, shared);
    remove(&mut owner, 16, anonymous);

    // A missing/expired type suppresses SIGNATURE rather than creating a new
    // type. This also proves removal reclaimed the type's registry entry.
    for (seq, id) in [(17, type_id), (18, 0), (19, u32::MAX)] {
        let mut env = EnvBuilder::new();
        env.push_u32(UbusAttr::ObjType, id);
        env.push_signature(&signature);
        let (data, status) = request(&mut owner, MsgType::AddObject, seq, env);
        assert_eq!(status, 0);
        assert!(find(&data[0].payload, UbusAttr::ObjType).is_none());
    }
}

#[rstest]
#[case::upstream_disconnect(BrokerArm::Upstream, false)]
#[case::rust_disconnect(BrokerArm::Rs, false)]
#[case::upstream_remove(BrokerArm::Upstream, true)]
#[case::rust_remove(BrokerArm::Rs, true)]
fn unsubscribing_one_sibling_keeps_cleanup_for_the_other(
    #[case] arm: BrokerArm,
    #[case] remove_sibling: bool,
) {
    let Some(harness) = broker(arm) else { return };
    let mut publisher = connect(&harness);
    let (target, _) = raw_add_object_with_signature(
        &mut publisher,
        30,
        "audit.subscriptions",
        &signature_with_ping_method(),
    );
    let mut subscriber = connect(&harness);
    let mut objects = Vec::new();
    for seq in [31, 32] {
        let (data, status) = request(&mut subscriber, MsgType::AddObject, seq, EnvBuilder::new());
        assert_eq!(status, 0);
        objects.push(
            find(&data[0].payload, UbusAttr::ObjId)
                .unwrap()
                .as_u32()
                .unwrap(),
        );
    }
    for (seq, object) in [(33, objects[0]), (34, objects[1])] {
        let mut env = EnvBuilder::new();
        env.push_u32(UbusAttr::ObjId, object);
        env.push_u32(UbusAttr::Target, target);
        assert_eq!(request(&mut subscriber, MsgType::Subscribe, seq, env).1, 0);
    }
    recv_until(&mut publisher, Duration::from_secs(2), |f| {
        f.hdr.ty == MsgType::Notify
            && find(&f.payload, UbusAttr::Active).map(|a| a.payload) == Some(&[1][..])
    })
    .unwrap();
    let mut env = EnvBuilder::new();
    env.push_u32(UbusAttr::ObjId, objects[0]);
    env.push_u32(UbusAttr::Target, target);
    assert_eq!(request(&mut subscriber, MsgType::Unsubscribe, 35, env).1, 0);
    if remove_sibling {
        remove(&mut subscriber, 36, objects[1]);
    }
    drop(subscriber);
    recv_until(&mut publisher, Duration::from_secs(2), |f| {
        f.hdr.ty == MsgType::Notify
            && find(&f.payload, UbusAttr::Active).map(|a| a.payload) == Some(&[0][..])
    })
    .expect("last sibling removal must notify publisher");
}
