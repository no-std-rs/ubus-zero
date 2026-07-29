//! Byte-level cross-check: drive an identical LOOKUP exchange against
//! both broker flavours through the shared tapped runner, then assert
//! the canonical transcripts match. Where `ubusd_handshake.rs`
//! compares each end's decoded view, this pins the actual wire bytes
//! — the surface where a stray peer-id or a spurious DATA frame hides.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

use ubus::envelope::EnvBuilder;
use ubus::error::UbusError;
use ubus::wire::{Codec, Frame, Header, MsgType, HEADER_SIZE, PROTO_VERSION};
use ubus_testkit::{canon, run_tapped, BrokerArm};

fn lookup_exchange_body(path: &str) {
    let mut client = UnixStream::connect(path).expect("connect through tap");
    client
        .set_read_timeout(Some(Duration::from_millis(50)))
        .expect("set read timeout");

    let mut reader = FrameReader::default();

    // 1. Unsolicited broker HELLO.
    reader
        .read_matching(&mut client, Duration::from_secs(2), |f| {
            f.hdr.ty == MsgType::Hello
        })
        .expect("unsolicited HELLO");

    // 2. Raw list-all LOOKUP: empty envelope, peer=0, seq=1 — same
    //    shape as `ubusd_handshake.rs`'s raw bodies.
    let lookup = Frame {
        hdr: Header {
            version: PROTO_VERSION,
            ty: MsgType::Lookup,
            seq: 1,
            peer: 0,
        },
        payload: EnvBuilder::new().finish(),
    };
    client
        .write_all(&Codec::write_frame(&lookup))
        .expect("send LOOKUP");

    // 3. STATUS reply on seq=1 (empty broker → STATUS only, no DATA).
    reader
        .read_matching(&mut client, Duration::from_secs(2), |f| {
            f.hdr.seq == 1 && f.hdr.ty == MsgType::Status
        })
        .expect("LOOKUP STATUS reply");
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

#[test]
fn lookup_exchange_frames_match_across_brokers() {
    let test = "wire_parity lookup_exchange_frames_match_across_brokers";
    let Some(upstream) = run_tapped(test, BrokerArm::Upstream, &lookup_exchange_body) else {
        return;
    };
    let Some(rs) = run_tapped(test, BrokerArm::Rs, &lookup_exchange_body) else {
        return;
    };
    if let Some(diff) = canon::diff(&upstream, &rs) {
        panic!("{test}: canonical transcript divergence between upstream and rs\n{diff}");
    }
}

/// Minimal streaming frame reader over a raw `UnixStream`: buffers
/// bytes until [`Codec::read_frame`] yields a frame matching `pred`.
#[derive(Default)]
struct FrameReader {
    buf: Vec<u8>,
}

impl FrameReader {
    fn read_matching(
        &mut self,
        stream: &mut UnixStream,
        timeout: Duration,
        pred: impl Fn(&Frame) -> bool,
    ) -> Option<Frame> {
        let deadline = Instant::now() + timeout;
        loop {
            while self.buf.len() >= HEADER_SIZE {
                match Codec::read_frame(&self.buf) {
                    Ok((frame, consumed)) => {
                        self.buf.drain(..consumed);
                        if pred(&frame) {
                            return Some(frame);
                        }
                    }
                    Err(UbusError::Short { .. }) => break,
                    Err(e) => panic!("frame decode: {e:?}; hex: {}", hex(&self.buf)),
                }
            }
            if Instant::now() >= deadline {
                return None;
            }
            let mut tmp = [0u8; 512];
            match stream.read(&mut tmp) {
                Ok(0) => return None,
                Ok(n) => self.buf.extend_from_slice(&tmp[..n]),
                Err(e)
                    if e.kind() == std::io::ErrorKind::WouldBlock
                        || e.kind() == std::io::ErrorKind::TimedOut =>
                {
                    continue
                }
                Err(e) => panic!("stream read: {e:?}"),
            }
        }
    }
}
