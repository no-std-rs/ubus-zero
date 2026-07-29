use std::io::{Read, Write};
use std::time::Duration;

use ubus::envelope::EnvBuilder;
use ubus::wire::{Codec, Frame, Header, MsgType, HEADER_SIZE, PROTO_VERSION};
use ubus_testkit::{broker_matrix, paired_peer_test, RawPeer, TransportPair, UbusdHarness};

#[test]
fn raw_peer_path_is_unique_across_calls() {
    let a = RawPeer::bind().unwrap();
    let b = RawPeer::bind().unwrap();
    assert_ne!(a.path(), b.path());
}

#[test]
fn raw_peer_drop_unlinks_socket() {
    let path = {
        let p = RawPeer::bind().unwrap();
        p.path().to_path_buf()
    };
    assert!(
        !path.exists(),
        "RawPeer drop should unlink {}",
        path.display()
    );
}

#[test]
fn transport_pair_round_trips_bytes() {
    use rustix::fd::AsFd;
    use rustix::io::Errno;
    use rustix::net::{RecvFlags, SendFlags};

    let pair = TransportPair::new().unwrap();
    let payload = b"hello-ubus";
    rustix::net::send(pair.a().as_fd(), payload, SendFlags::empty()).unwrap();

    let mut buf = [0u8; 64];
    let mut got = 0;
    for _ in 0..1024 {
        match rustix::net::recv(pair.b().as_fd(), &mut buf, RecvFlags::empty()) {
            Ok((n, _)) => {
                got = n;
                break;
            }
            Err(Errno::AGAIN) => std::thread::sleep(std::time::Duration::from_micros(100)),
            Err(e) => panic!("recv: {e:?}"),
        }
    }
    assert_eq!(&buf[..got], payload);
}

#[test]
fn transport_pair_split_yields_paired_fds() {
    use rustix::fd::AsFd;
    use rustix::io::Errno;
    use rustix::net::{RecvFlags, SendFlags};

    let (left, right) = TransportPair::new().unwrap().split();
    rustix::net::send(left.as_fd(), b"x", SendFlags::empty()).unwrap();
    let mut buf = [0u8; 1];
    for _ in 0..1024 {
        match rustix::net::recv(right.as_fd(), &mut buf, RecvFlags::empty()) {
            Ok(_) => break,
            Err(Errno::AGAIN) => std::thread::sleep(std::time::Duration::from_micros(100)),
            Err(e) => panic!("recv after split: {e:?}"),
        }
    }
    assert_eq!(&buf, b"x");
}

#[test]
fn ubusd_harness_returns_none_when_binary_missing() {
    match UbusdHarness::start_upstream() {
        Ok(None) => {
            eprintln!("skip: ubusd not on PATH");
        }
        Ok(Some(h)) => {
            std::os::unix::net::UnixStream::connect(h.socket_path())
                .expect("ubusd socket should be connectable");
        }
        Err(e) => panic!("UbusdHarness::start_upstream: {e:?}"),
    }
}

#[test]
fn ubusd_rs_harness_returns_none_when_binary_missing() {
    match UbusdHarness::start_rust() {
        Ok(None) => {
            eprintln!("skip: ubusd-rs binary not reachable");
        }
        Ok(Some(h)) => {
            std::os::unix::net::UnixStream::connect(h.socket_path())
                .expect("ubusd-rs socket should be connectable");
        }
        Err(e) => panic!("UbusdHarness::start_rust: {e:?}"),
    }
}

#[test]
fn raw_peer_accepts_a_client_connection() {
    let peer = RawPeer::bind().unwrap();
    let path = peer.path().to_path_buf();

    let mut client = std::os::unix::net::UnixStream::connect(&path).unwrap();
    let mut server = peer.accept().unwrap();

    client.write_all(b"ping").unwrap();
    let mut buf = [0u8; 4];
    server.read_exact(&mut buf).unwrap();
    assert_eq!(&buf, b"ping");
}

struct DummyTransport(std::os::unix::net::UnixStream);

impl DummyTransport {
    fn connect(path: &str) -> std::io::Result<Self> {
        std::os::unix::net::UnixStream::connect(path).map(Self)
    }

    fn echo(&mut self, byte: u8) -> std::io::Result<u8> {
        self.0.write_all(&[byte])?;
        let mut got = [0u8; 1];
        self.0.read_exact(&mut got)?;
        Ok(got[0])
    }
}

fn echo_round_trip(peer: std::os::unix::net::UnixStream, transport: &mut DummyTransport, byte: u8) {
    // `scope` joins the worker before returning, so a panic in the
    // echo thread surfaces as a test failure and a hung worker can't
    // outlive the test and bleed into the next one.
    std::thread::scope(|s| {
        s.spawn(move || {
            let mut peer = peer;
            let mut buf = [0u8; 1];
            if peer.read_exact(&mut buf).is_ok() {
                let _ = peer.write_all(&buf);
            }
        });
        let echoed = transport.echo(byte).expect("echo");
        assert_eq!(echoed, byte);
    });
}

/// HELLO round-trip that works against all three matrix arms.
///
/// `_raw`: `peer` is the listener's accepted other end of `transport`'s
/// stream, so the peer-thread reads our HELLO bytes off the wire and
/// writes a hand-rolled HELLO reply back. `transport.recv` then sees
/// the bytes we wrote.
///
/// `_upstream` / `_rs`: `peer` is a separate idle connection to the
/// broker (the macro just `connect`s it; we never send HELLO on it).
/// Both brokers greet every accept with an unsolicited HELLO, so
/// `peer.read` returns the broker's HELLO bytes immediately rather
/// than timing out — the peer-thread parses it as HELLO and writes a
/// hand-rolled HELLO reply, which the broker then treats as a
/// redundant client HELLO and answers privately on `peer`. The main
/// test thread is unaffected; only the HELLO sent over `transport`
/// matters for the assertion below.
fn hello_round_trip(peer: std::os::unix::net::UnixStream, transport: &mut DummyTransport) {
    peer.set_read_timeout(Some(Duration::from_millis(250)))
        .unwrap();
    transport
        .0
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();

    let request = Frame {
        hdr: Header {
            version: PROTO_VERSION,
            ty: MsgType::Hello,
            seq: 1,
            peer: 0,
        },
        // Client HELLO carries the 4-byte empty envelope so upstream
        // ubusd's stream decoder doesn't desync on a back-to-back
        // frame after this one. See `ubus::wire` codec docs.
        payload: EnvBuilder::new().finish(),
    };

    std::thread::scope(|s| {
        s.spawn(move || {
            let mut peer = peer;
            let mut buf = [0u8; 256];
            // The peer side either receives our HELLO bytes (raw arm)
            // or the broker's unsolicited HELLO (broker arms); either
            // way the thread exits and the scope joins. Errors are
            // intentionally ignored — a stray timeout / decode failure
            // here is not the assertion under test.
            let n = match peer.read(&mut buf) {
                Ok(n) if n >= HEADER_SIZE => n,
                _ => return,
            };
            let Ok((frame, _)) = Codec::read_frame(&buf[..n]) else {
                return;
            };
            if !matches!(frame.hdr.ty, MsgType::Hello) {
                return;
            }
            let reply = Frame {
                hdr: Header {
                    version: PROTO_VERSION,
                    ty: MsgType::Hello,
                    seq: frame.hdr.seq,
                    peer: 1025,
                },
                // Mirror the well-formed wire shape from the request.
                payload: EnvBuilder::new().finish(),
            };
            let _ = peer.write_all(&Codec::write_frame(&reply));
        });

        transport
            .0
            .write_all(&Codec::write_frame(&request))
            .unwrap();
        let mut buf = [0u8; HEADER_SIZE];
        transport
            .0
            .read_exact(&mut buf)
            .expect("HELLO reply header");
        let header = Header::decode(&buf).expect("decode HELLO header");
        assert_eq!(
            header.ty,
            MsgType::Hello,
            "expected HELLO reply, got {header:?}"
        );
    });
}

broker_matrix!(
    fn macro_emits_arm(peer, transport: crate::DummyTransport) {
        crate::hello_round_trip(peer, &mut transport);
    }
);

paired_peer_test!(
    fn paired_macro_emits_named_test(peer, transport: crate::DummyTransport) {
        crate::echo_round_trip(peer, &mut transport, 0x99);
    }
);
