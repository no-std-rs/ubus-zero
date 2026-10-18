//! Real libubus clients decode provider errors through both broker implementations.
#![cfg(all(feature = "testing", target_os = "linux"))]

use rstest::rstest;
use ubus::server::UbusObject;
use ubus::wire::{UBUS_STATUS_METHOD_NOT_FOUND, UBUS_STATUS_UNKNOWN_ERROR};
use ubus::UbusError;
use ubus_testkit::{skip, BrokerArm, LibuboxClient, UbusdHarness};
#[allow(dead_code)]
mod common;
use common::OwnerThread;

#[rstest]
#[case::upstream(BrokerArm::Upstream)]
#[case::rust(BrokerArm::Rs)]
fn c_client_receives_standard_provider_statuses(#[case] arm: BrokerArm) {
    let harness = match arm {
        BrokerArm::Upstream => UbusdHarness::start_upstream(),
        BrokerArm::Rs => UbusdHarness::start_rust(),
    }
    .unwrap();
    let Some(harness) = harness else {
        skip::or_skip("provider statuses", "broker fixture not available");
        return;
    };
    let owner = OwnerThread::spawn(harness.socket_path().to_str().unwrap().into(), || {
        UbusObject::new("audit.provider.status").method("fail", |_| {
            Err(UbusError::Method("downstream failure".into()))
        })
    });
    owner.wait_ready();
    let Some(mut client) = LibuboxClient::connect(harness.socket_path()).unwrap() else {
        skip::or_skip("provider statuses", "libubus fixture not available");
        return;
    };
    for (method, expected) in [
        ("missing", UBUS_STATUS_METHOD_NOT_FOUND),
        ("fail", UBUS_STATUS_UNKNOWN_ERROR),
    ] {
        let error = client.invoke("audit.provider.status", method).unwrap_err();
        // The fixture exposes libubus's numeric return in this diagnostic.
        assert_eq!(
            error.to_string(),
            format!("ubus_invoke(audit.provider.status.{method}) returned {expected}")
        );
    }
    owner.shutdown();
}
