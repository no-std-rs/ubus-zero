//! Raw-stream framing assertions also run against the pinned C broker.
#![cfg(feature = "testing")]

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

use rstest::rstest;
use ubus::envelope::EnvBuilder;
use ubus::wire::{Codec, Frame, MsgType};
use ubus_testkit::{skip, BrokerArm, UbusdHarness};

#[rstest]
#[case::upstream(BrokerArm::Upstream)]
#[case::rust(BrokerArm::Rs)]
fn unaligned_ping_does_not_wait_for_padding_or_consume_the_next_header(#[case] arm: BrokerArm) {
    let harness = match arm {
        BrokerArm::Upstream => UbusdHarness::start_upstream(),
        BrokerArm::Rs => UbusdHarness::start_rust(),
    };
    let Some(harness) = harness.expect("start broker") else {
        skip::or_skip("unaligned stream framing", "broker fixture not available");
        return;
    };
    let mut stream = UnixStream::connect(harness.socket_path()).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    stream
        .set_write_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let mut hello = [0; 12];
    stream.read_exact(&mut hello).unwrap();

    // A final one-byte UNSPEC attribute has raw length 5. Its alignment
    // bytes are not part of the outer blob's declared length (9).
    let payload = vec![0, 0, 0, 9, 14, 0, 0, 5, 0x71];
    let first = Frame::to_broker(MsgType::Ping, 71, payload);
    let second = Frame::to_broker(MsgType::Ping, 72, EnvBuilder::new().finish());
    let mut pipeline = Codec::write_frame(&first);
    pipeline.extend_from_slice(&Codec::write_frame(&second));
    stream.write_all(&pipeline).unwrap();

    for expected in [&first, &second] {
        let mut header = [0; 8];
        stream.read_exact(&mut header).unwrap();
        assert_eq!(header[1], MsgType::Data as u8);
        assert_eq!(&header[2..4], &expected.hdr.seq.to_be_bytes());
        let mut echo = vec![0; expected.payload.len()];
        stream.read_exact(&mut echo).unwrap();
        assert_eq!(echo, expected.payload);
        // Read the immediately following header at the raw-length boundary;
        // this assertion deliberately does not use the Rust decoder.
        let mut status = [0; 20];
        stream.read_exact(&mut status).unwrap();
        assert_eq!(status[1], MsgType::Status as u8);
        assert_eq!(&status[2..4], &expected.hdr.seq.to_be_bytes());
        assert_eq!(&status[8..], &[0, 0, 0, 12, 1, 0, 0, 8, 0, 0, 0, 0]);
    }
}
