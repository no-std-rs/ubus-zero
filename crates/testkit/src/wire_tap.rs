//! `WireTap` — a recording man-in-the-middle proxy for a ubus broker
//! socket. It binds a fresh tmp socket, forwards every byte verbatim
//! between a test client and the real broker, and appends a [`TapEvent`]
//! for each chunk or EOF in each direction. Tests connect *through* the tap and
//! then assert on the captured byte stream — which lets them compare the
//! upstream `ubusd` and the in-tree `ubusd-rs` broker at the *wire*
//! level (types, seqs, peers, payload bytes), not just each end's
//! decoded view.
//!
//! Std-only (plus `rustix` via [`unique_socket_path`], the testkit
//! convention); no async, no extra deps. Shutdown is cooperative: pump
//! threads poll a flag under a short read timeout, and [`WireTap::drop`]
//! sets the flag, connect-to-self to unblock `accept`, then joins.

use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use ubus::wire::{Codec, Frame};

use crate::path::unique_socket_path;

/// Which way a captured byte chunk was travelling.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TapDirection {
    /// Bytes the test client wrote toward the broker.
    ClientToBroker,
    /// Bytes the broker wrote back toward the client.
    BrokerToClient,
}

/// One forwarded event. `conn` is a 0-based index assigned in accept
/// order so multiple concurrent connections stay separable.
#[derive(Clone, Debug)]
pub struct TapEvent {
    pub conn: usize,
    pub dir: TapDirection,
    pub kind: TapEventKind,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TapEventKind {
    Bytes(Vec<u8>),
    Eof,
}

/// Optional forwarding behavior for a tap. Policies affect only how
/// bytes are written to the peer; captured [`TapEventKind::Bytes`]
/// chunks remain the original reads so canonical transcripts stay
/// comparable across fault profiles.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct TapPolicy {
    pub fragment_c2b: Option<usize>,
}

/// Poll interval for the pump threads' blocking reads — short enough
/// that Drop-time shutdown is prompt, long enough to avoid busy-spin.
const PUMP_POLL: Duration = Duration::from_millis(50);

pub struct WireTap {
    path: PathBuf,
    log: Arc<Mutex<Vec<TapEvent>>>,
    shutdown: Arc<AtomicBool>,
    accept_handle: Option<JoinHandle<()>>,
    pump_handles: Arc<Mutex<Vec<JoinHandle<()>>>>,
}

impl WireTap {
    /// Bind a fresh `tap` socket in front of `broker_socket` and start
    /// the accept loop. Test clients connect to [`WireTap::socket_path`].
    pub fn spawn(broker_socket: &Path) -> io::Result<WireTap> {
        Self::spawn_with_policy(broker_socket, TapPolicy::default())
    }

    /// Like [`WireTap::spawn`], but applies a forwarding policy.
    pub fn spawn_with_policy(broker_socket: &Path, policy: TapPolicy) -> io::Result<WireTap> {
        let path = unique_socket_path("tap");
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path)?;

        let log: Arc<Mutex<Vec<TapEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let shutdown = Arc::new(AtomicBool::new(false));
        let pump_handles: Arc<Mutex<Vec<JoinHandle<()>>>> = Arc::new(Mutex::new(Vec::new()));
        let broker_path = broker_socket.to_path_buf();

        let accept_handle = {
            let log = log.clone();
            let shutdown = shutdown.clone();
            let pump_handles = pump_handles.clone();
            std::thread::spawn(move || {
                accept_loop(listener, &broker_path, policy, log, shutdown, pump_handles);
            })
        };

        Ok(WireTap {
            path,
            log,
            shutdown,
            accept_handle: Some(accept_handle),
            pump_handles,
        })
    }

    /// Socket the test client should connect to.
    pub fn socket_path(&self) -> &Path {
        &self.path
    }

    /// Snapshot clone of every captured event in arrival order.
    pub fn events(&self) -> Vec<TapEvent> {
        self.log.lock().expect("tap log poisoned").clone()
    }

    /// Wait until every connection the tap has observed reached EOF in
    /// BOTH directions, or `timeout` elapses. Brokers close their side
    /// after a client EOF, but that close races the caller's
    /// [`WireTap::into_events`] pump shutdown — without this barrier,
    /// trailing broker-side EOFs (and the teardown frames before them)
    /// are captured nondeterministically.
    ///
    /// `Ok(())` on quiescence (including a tap that observed nothing —
    /// there is nothing to drain). On timeout, `Err` lists the
    /// still-open connections per direction so a teardown-race surfaces
    /// loudly instead of silently truncating the capture. Early-returns
    /// the instant EOF quiescence is reached, so a green run never waits
    /// the full `timeout`.
    pub fn wait_quiescent(&self, timeout: Duration) -> Result<(), Vec<String>> {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            let pending = self.pending_conns();
            if pending.is_empty() {
                return Ok(());
            }
            if std::time::Instant::now() >= deadline {
                return Err(pending);
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// Connections the tap has seen open that have not yet reached EOF in
    /// both directions, rendered for a diagnostic. Empty when every
    /// observed connection is fully closed (or none were observed).
    fn pending_conns(&self) -> Vec<String> {
        let log = self.log.lock().expect("tap log poisoned");
        let mut conns: std::collections::BTreeMap<usize, (bool, bool)> =
            std::collections::BTreeMap::new();
        for ev in log.iter() {
            let entry = conns.entry(ev.conn).or_insert((false, false));
            if matches!(ev.kind, TapEventKind::Eof) {
                match ev.dir {
                    TapDirection::ClientToBroker => entry.0 = true,
                    TapDirection::BrokerToClient => entry.1 = true,
                }
            }
        }
        conns
            .into_iter()
            .filter(|(_, (c2b, b2c))| !(*c2b && *b2c))
            .map(|(conn, (c2b, b2c))| {
                format!("conn {conn}: client→broker EOF={c2b}, broker→client EOF={b2c}")
            })
            .collect()
    }

    /// Shut the tap down, join every pump, and return the complete log.
    pub fn into_events(mut self) -> Vec<TapEvent> {
        self.shutdown_and_join();
        let events = self.log.lock().expect("tap log poisoned").clone();
        drop(self);
        events
    }

    /// Concatenate `dir`'s byte events per connection (in first-seen order)
    /// and decode them with [`Codec`]. Panics with a hex dump on a
    /// decode error: a tap that can't decode what a broker sent is
    /// itself a finding.
    pub fn frames(&self, dir: TapDirection) -> Vec<Frame> {
        let mut out = Vec::new();
        for stream in self.dir_streams(dir) {
            decode_stream(&stream, &mut out);
        }
        out
    }

    /// Like [`WireTap::frames`] but tolerant of a trailing partial frame:
    /// decode every complete frame and stop each connection's stream at
    /// the first incomplete one instead of panicking. Safe to poll while
    /// the broker is mid-write — e.g. a readiness barrier that watches
    /// for a specific frame to land. A genuinely malformed (non-`Short`)
    /// decode still panics: that is a real finding, not a partial read.
    pub fn frames_lenient(&self, dir: TapDirection) -> Vec<Frame> {
        let mut out = Vec::new();
        for stream in self.dir_streams(dir) {
            decode_stream_lenient(&stream, &mut out);
        }
        out
    }

    /// Concatenated per-connection byte streams for `dir`, in first-seen
    /// connection order.
    fn dir_streams(&self, dir: TapDirection) -> Vec<Vec<u8>> {
        let events = self.log.lock().expect("tap log poisoned");
        let mut conns: Vec<usize> = Vec::new();
        let mut streams: Vec<Vec<u8>> = Vec::new();
        for ev in events.iter().filter(|e| e.dir == dir) {
            let TapEventKind::Bytes(bytes) = &ev.kind else {
                continue;
            };
            let idx = match conns.iter().position(|c| *c == ev.conn) {
                Some(i) => i,
                None => {
                    conns.push(ev.conn);
                    streams.push(Vec::new());
                    conns.len() - 1
                }
            };
            streams[idx].extend_from_slice(bytes);
        }
        streams
    }
}

impl Drop for WireTap {
    fn drop(&mut self) {
        self.shutdown_and_join();
    }
}

impl WireTap {
    fn shutdown_and_join(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        // Unblock the parked `accept()` — the loop re-checks the flag
        // and breaks before servicing this dummy connection.
        let _ = UnixStream::connect(&self.path);
        if let Some(h) = self.accept_handle.take() {
            let _ = h.join();
        }
        // Accept loop is joined, so no more handles get pushed.
        let handles =
            std::mem::take(&mut *self.pump_handles.lock().expect("pump handles poisoned"));
        for h in handles {
            let _ = h.join();
        }
        let _ = std::fs::remove_file(&self.path);
    }
}

fn accept_loop(
    listener: UnixListener,
    broker_path: &Path,
    policy: TapPolicy,
    log: Arc<Mutex<Vec<TapEvent>>>,
    shutdown: Arc<AtomicBool>,
    pump_handles: Arc<Mutex<Vec<JoinHandle<()>>>>,
) {
    let mut next_conn = 0usize;
    loop {
        let client = match listener.accept() {
            Ok((stream, _)) => stream,
            Err(_) => break,
        };
        if shutdown.load(Ordering::SeqCst) {
            break;
        }
        let broker = match UnixStream::connect(broker_path) {
            Ok(b) => b,
            // Broker gone mid-test: skip this connection rather than
            // tearing down the whole tap.
            Err(_) => continue,
        };
        let conn = next_conn;
        next_conn += 1;

        // Split each socket: one fd reads, its clone writes. Read
        // timeouts on the two reading fds let the pumps poll `shutdown`.
        let client_r = match client.try_clone() {
            Ok(c) => c,
            Err(_) => continue,
        };
        let broker_r = match broker.try_clone() {
            Ok(b) => b,
            Err(_) => continue,
        };
        let _ = client_r.set_read_timeout(Some(PUMP_POLL));
        let _ = broker_r.set_read_timeout(Some(PUMP_POLL));

        let c2b = spawn_pump(
            client_r,
            broker,
            conn,
            TapDirection::ClientToBroker,
            policy.fragment_c2b,
            log.clone(),
            shutdown.clone(),
        );
        let b2c = spawn_pump(
            broker_r,
            client,
            conn,
            TapDirection::BrokerToClient,
            None,
            log.clone(),
            shutdown.clone(),
        );
        pump_handles
            .lock()
            .expect("pump handles poisoned")
            .extend([c2b, b2c]);
    }
}

fn spawn_pump(
    mut src: UnixStream,
    mut dst: UnixStream,
    conn: usize,
    dir: TapDirection,
    fragment: Option<usize>,
    log: Arc<Mutex<Vec<TapEvent>>>,
    shutdown: Arc<AtomicBool>,
) -> JoinHandle<()> {
    std::thread::spawn(move || {
        pump_loop(&mut src, &mut dst, conn, dir, fragment, &log, &shutdown);
        // Propagate the close so the opposite-direction pump also exits.
        let _ = dst.shutdown(Shutdown::Write);
        let _ = src.shutdown(Shutdown::Read);
    })
}

fn pump_loop(
    src: &mut impl Read,
    dst: &mut impl Write,
    conn: usize,
    dir: TapDirection,
    fragment: Option<usize>,
    log: &Arc<Mutex<Vec<TapEvent>>>,
    shutdown: &AtomicBool,
) {
    let mut buf = [0u8; 4096];
    let mut forwarding = true;
    loop {
        match src.read(&mut buf) {
            Ok(0) => {
                log_eof(log, conn, dir);
                break;
            }
            Ok(n) => {
                // Record before forwarding so a captured chunk is
                // never observed by the peer before the log has it.
                log.lock().expect("tap log poisoned").push(TapEvent {
                    conn,
                    dir,
                    kind: TapEventKind::Bytes(buf[..n].to_vec()),
                });
                if forwarding && forward(dst, &buf[..n], fragment).is_err() {
                    forwarding = false;
                }
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                if shutdown.load(Ordering::SeqCst) {
                    break;
                }
                continue;
            }
            Err(e) if e.kind() == io::ErrorKind::TimedOut => {
                if shutdown.load(Ordering::SeqCst) {
                    break;
                }
                continue;
            }
            Err(e) if e.kind() == io::ErrorKind::ConnectionReset => {
                log_eof(log, conn, dir);
                break;
            }
            Err(_) => break,
        }
    }
}

fn forward(dst: &mut impl Write, bytes: &[u8], fragment: Option<usize>) -> io::Result<()> {
    match fragment {
        Some(n) => {
            debug_assert!(n > 0);
            for chunk in bytes.chunks(n) {
                dst.write_all(chunk)?;
                dst.flush()?;
            }
        }
        None => {
            dst.write_all(bytes)?;
            dst.flush()?;
        }
    }
    Ok(())
}

fn log_eof(log: &Arc<Mutex<Vec<TapEvent>>>, conn: usize, dir: TapDirection) {
    log.lock().expect("tap log poisoned").push(TapEvent {
        conn,
        dir,
        kind: TapEventKind::Eof,
    });
}

fn decode_stream(bytes: &[u8], out: &mut Vec<Frame>) {
    let mut off = 0;
    while off < bytes.len() {
        match Codec::read_frame(&bytes[off..]) {
            Ok((frame, consumed)) => {
                debug_assert!(consumed > 0);
                out.push(frame);
                off += consumed;
            }
            Err(e) => panic!(
                "wire_tap: decode error {e:?} at offset {off} of {} bytes; hex: {}",
                bytes.len(),
                hex(&bytes[off..]),
            ),
        }
    }
}

/// Decode complete frames from `bytes`, stopping at the first incomplete
/// (`Short`) one instead of panicking. A non-`Short` decode error still
/// panics, matching [`decode_stream`] — a corrupt frame is a finding.
fn decode_stream_lenient(bytes: &[u8], out: &mut Vec<Frame>) {
    let mut off = 0;
    while off < bytes.len() {
        match Codec::read_frame(&bytes[off..]) {
            Ok((frame, consumed)) => {
                debug_assert!(consumed > 0);
                out.push(frame);
                off += consumed;
            }
            Err(ubus::error::UbusError::Short { .. }) => return,
            Err(e) => panic!(
                "wire_tap: decode error {e:?} at offset {off} of {} bytes; hex: {}",
                bytes.len(),
                hex(&bytes[off..]),
            ),
        }
    }
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::RawPeer;

    // 14-byte markers so the fake broker can `read_exact` a known count.
    const B2C: &[u8] = b"BROKER->CLIENT";
    const C2B: &[u8] = b"CLIENT->BROKER";

    #[test]
    fn tap_forwards_and_records_both_directions() {
        let broker = RawPeer::bind_labelled("tapbroker").expect("bind fake broker");
        let broker_path = broker.path().to_owned();

        // Fake broker: greet with a known blob, then echo back what the
        // client sent (join guarantees the C2B pump has recorded it).
        let bh = std::thread::spawn(move || {
            let mut s = broker.accept().expect("broker accept");
            s.write_all(B2C).expect("broker write");
            s.flush().expect("broker flush");
            let mut got = [0u8; C2B.len()];
            s.read_exact(&mut got).expect("broker read");
            got.to_vec()
        });

        let tap = WireTap::spawn(&broker_path).expect("spawn tap");
        let mut client = UnixStream::connect(tap.socket_path()).expect("client connect");

        let mut greet = [0u8; B2C.len()];
        client.read_exact(&mut greet).expect("client read greeting");
        assert_eq!(&greet, B2C);

        client.write_all(C2B).expect("client write");
        client.flush().expect("client flush");

        let echoed = bh.join().expect("broker thread");
        assert_eq!(echoed, C2B);

        drop(client);

        let events = tap.events();
        let c2b: Vec<u8> = events
            .iter()
            .filter(|e| e.dir == TapDirection::ClientToBroker)
            .filter_map(|e| match &e.kind {
                TapEventKind::Bytes(bytes) => Some(bytes.as_slice()),
                TapEventKind::Eof => None,
            })
            .flat_map(|bytes| bytes.iter().copied())
            .collect();
        let b2c: Vec<u8> = events
            .iter()
            .filter(|e| e.dir == TapDirection::BrokerToClient)
            .filter_map(|e| match &e.kind {
                TapEventKind::Bytes(bytes) => Some(bytes.as_slice()),
                TapEventKind::Eof => None,
            })
            .flat_map(|bytes| bytes.iter().copied())
            .collect();
        assert_eq!(c2b, C2B, "client->broker capture");
        assert_eq!(b2c, B2C, "broker->client capture");
        assert!(
            events.iter().all(|e| e.conn == 0),
            "single connection should be conn 0"
        );
    }

    #[test]
    fn pump_keeps_observing_after_destination_write_failure() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let shutdown = Arc::new(AtomicBool::new(false));
        let mut src = ScriptedRead::new([
            ReadStep::Bytes(b"first"),
            ReadStep::Bytes(b"second"),
            ReadStep::Eof,
        ]);
        let mut dst = FailingWrite;

        pump_loop(
            &mut src,
            &mut dst,
            0,
            TapDirection::BrokerToClient,
            None,
            &log,
            &shutdown,
        );

        let events = log.lock().expect("tap log").clone();
        let bytes: Vec<u8> = events
            .iter()
            .filter_map(|e| match &e.kind {
                TapEventKind::Bytes(bytes) => Some(bytes.as_slice()),
                TapEventKind::Eof => None,
            })
            .flat_map(|bytes| bytes.iter().copied())
            .collect();
        assert_eq!(bytes, b"firstsecond");
        assert!(
            events.iter().any(|e| matches!(e.kind, TapEventKind::Eof)),
            "pump must record EOF after destination write failure"
        );
    }

    #[test]
    fn pump_treats_source_connection_reset_as_eof() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let shutdown = Arc::new(AtomicBool::new(false));
        let mut src = ScriptedRead::new([ReadStep::Reset]);
        let mut dst = Vec::new();

        pump_loop(
            &mut src,
            &mut dst,
            0,
            TapDirection::ClientToBroker,
            None,
            &log,
            &shutdown,
        );

        let events = log.lock().expect("tap log");
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0].kind, TapEventKind::Eof));
    }

    #[test]
    fn fragment_policy_keeps_log_chunks_original() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let shutdown = Arc::new(AtomicBool::new(false));
        let mut src = ScriptedRead::new([ReadStep::Bytes(b"abcdef"), ReadStep::Eof]);
        let mut dst = Vec::new();

        pump_loop(
            &mut src,
            &mut dst,
            0,
            TapDirection::ClientToBroker,
            Some(2),
            &log,
            &shutdown,
        );

        assert_eq!(dst, b"abcdef");
        let events = log.lock().expect("tap log");
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].kind, TapEventKind::Bytes(b"abcdef".to_vec()));
        assert!(matches!(events[1].kind, TapEventKind::Eof));
    }

    enum ReadStep {
        Bytes(&'static [u8]),
        Eof,
        Reset,
    }

    struct ScriptedRead {
        steps: std::collections::VecDeque<ReadStep>,
    }

    impl ScriptedRead {
        fn new(steps: impl IntoIterator<Item = ReadStep>) -> Self {
            Self {
                steps: steps.into_iter().collect(),
            }
        }
    }

    impl Read for ScriptedRead {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            match self.steps.pop_front().unwrap_or(ReadStep::Eof) {
                ReadStep::Bytes(bytes) => {
                    buf[..bytes.len()].copy_from_slice(bytes);
                    Ok(bytes.len())
                }
                ReadStep::Eof => Ok(0),
                ReadStep::Reset => Err(io::Error::new(
                    io::ErrorKind::ConnectionReset,
                    "scripted reset",
                )),
            }
        }
    }

    struct FailingWrite;

    impl Write for FailingWrite {
        fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
            Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "scripted write failure",
            ))
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
}
