#![cfg(all(target_os = "linux", feature = "signal"))]

use rustix::{fd::AsFd, io::Errno};
use std::collections::HashSet;
use ubus::{
    envelope::{find, EnvBuilder, UbusAttr},
    signal::{SignalQueue, SubmitError},
    unix::{SendProgress, StartError, StreamSender},
    wire::{Codec, Frame, MsgType},
};

fn socket_pair() -> (rustix::fd::OwnedFd, rustix::fd::OwnedFd) {
    rustix::net::socketpair(
        rustix::net::AddressFamily::UNIX,
        rustix::net::SocketType::STREAM,
        rustix::net::SocketFlags::NONBLOCK | rustix::net::SocketFlags::CLOEXEC,
        None,
    )
    .unwrap()
}

fn frame(size: usize) -> Vec<u8> {
    let mut env = EnvBuilder::new();
    env.push_data(&vec![0x55; size]);
    Codec::write_frame(&Frame::to_broker(MsgType::Ping, 10, env.finish()))
}

#[test]
fn concurrent_submissions_never_interleave_and_maximum_frame_fits() {
    let mut queue = SignalQueue::new().unwrap();
    let (sender, mut receiver) = queue.split();
    std::thread::scope(|scope| {
        for id in 1..=4 {
            scope.spawn(move || {
                for sequence in 0..200 {
                    let mut env = EnvBuilder::new();
                    env.push_u32(UbusAttr::ObjId, id);
                    env.push_str(UbusAttr::Method, "concurrent");
                    let bytes = Codec::write_frame(&Frame::to_broker(
                        MsgType::Ping,
                        sequence,
                        env.finish(),
                    ));
                    loop {
                        match sender.try_submit(&bytes) {
                            Ok(()) => break,
                            Err(SubmitError::Full | SubmitError::Interrupted) => {
                                std::thread::yield_now()
                            }
                            result => panic!("submission failed: {result:?}"),
                        }
                    }
                }
            });
        }
        let mut seen = HashSet::new();
        let mut buffered = Vec::new();
        let mut chunk = [0; 13];
        while seen.len() != 800 {
            let n = receiver.read(&mut chunk).unwrap();
            if n == 0 {
                std::thread::yield_now();
            }
            buffered.extend_from_slice(&chunk[..n]);
            while buffered.len() >= 12 {
                match Codec::read_frame_borrowed(&buffered) {
                    Ok((frame, consumed)) => {
                        let id = find(frame.payload, UbusAttr::ObjId)
                            .unwrap()
                            .as_u32()
                            .unwrap();
                        assert!((1..=4).contains(&id));
                        assert!(frame.hdr.seq < 200);
                        assert_eq!(
                            find(frame.payload, UbusAttr::Method).unwrap().as_str(),
                            Some("concurrent")
                        );
                        assert!(seen.insert((id, frame.hdr.seq)));
                        buffered.drain(..consumed);
                    }
                    Err(ubus::UbusError::Short { .. }) => break,
                    result => panic!("corrupt/interleaved frame: {result:?}"),
                }
            }
        }
        assert!(buffered.is_empty());
    });
    let bytes = frame(4080);
    assert_eq!(bytes.len(), 4096);
    sender.try_submit(&bytes).unwrap();
    let mut received = [0; 4096];
    assert_eq!(receiver.read(&mut received).unwrap(), 4096);
    assert_eq!(received.as_slice(), bytes);
}

#[test]
fn stream_sender_resumes_exactly_after_partial_writes_and_backpressure() {
    let (stream, peer) = socket_pair();
    rustix::net::sockopt::set_socket_send_buffer_size(&stream, 4096).unwrap();
    let bytes = frame(128 * 1024);
    let mut sender = StreamSender::new(stream.as_fd());
    let mut pending = sender.start(&bytes).unwrap();
    assert_eq!(pending.advance().unwrap(), SendProgress::Pending);
    assert_eq!(pending.advance(), Err(Errno::AGAIN));
    let mut received = Vec::new();
    let mut chunk = [0; 4096];
    let mut complete = false;
    while !complete || received.len() != bytes.len() {
        if !complete {
            match pending.advance() {
                Ok(SendProgress::Complete) => complete = true,
                Ok(SendProgress::Pending) | Err(Errno::AGAIN | Errno::INTR) => {}
                result => panic!("send failed: {result:?}"),
            }
        }
        match rustix::io::read(&peer, &mut chunk) {
            Ok(n) if n > 0 => received.extend_from_slice(&chunk[..n]),
            Err(Errno::AGAIN) => {}
            result => panic!("read failed: {result:?}"),
        }
    }
    assert_eq!(received, bytes);
    assert_eq!(pending.advance().unwrap(), SendProgress::Complete);
    drop(pending);
    assert!(sender.start(&frame(0)).is_ok());
}

#[test]
fn abandoning_partial_send_blocks_next_frame_and_closed_peer_never_sigpipes() {
    let (stream, peer) = socket_pair();
    rustix::net::sockopt::set_socket_send_buffer_size(&stream, 4096).unwrap();
    let bytes = frame(128 * 1024);
    let mut sender = StreamSender::new(stream.as_fd());
    drop(sender.start(&bytes).unwrap()); // No bytes sent: cancellation is safe.
    let mut pending = sender.start(&bytes).unwrap();
    assert_eq!(pending.advance().unwrap(), SendProgress::Pending);
    drop(pending);
    assert!(matches!(
        sender.start(&bytes),
        Err(StartError::AbandonedFrame)
    ));
    drop(peer);

    let (stream, peer) = socket_pair();
    drop(peer);
    let mut sender = StreamSender::new(stream.as_fd());
    let mut pending = sender.start(&bytes).unwrap();
    assert_eq!(pending.advance(), Err(Errno::PIPE));
}
