//! Golden transcript replay against the in-tree ubus broker.

use std::collections::{BTreeMap, VecDeque};
use std::io::{Read, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use ubus::envelope::{iter_attrs, unwrap as unwrap_envelope, MonitorAttr};
use ubus::error::UbusError;
use ubus::wire::{Codec, Frame, MsgType, HEADER_SIZE};

use crate::canon::{self, CanonEvent, CanonEventKind, CanonTranscript};
use crate::compat::{BrokerArm, BrokerOpts};
use crate::golden::Golden;
use crate::wire_tap::TapDirection;
use crate::{skip, TapPolicy, WireTap};

const STEP_TIMEOUT: Duration = Duration::from_secs(5);

pub fn replay_golden(test: &str, golden: &Golden, opts: &BrokerOpts) -> Option<CanonTranscript> {
    let harness = match BrokerArm::Rs.start(opts) {
        Ok(Some(harness)) => harness,
        Ok(None) => {
            skip::or_skip(test, "rs broker binary not reachable");
            return None;
        }
        Err(e) => panic!("broker start for {test}(golden): {e:?}"),
    };
    let policy = TapPolicy {
        fragment_c2b: crate::ubusd::fault_profile_from_env().and_then(|profile| profile.fragment),
    };
    let tap = WireTap::spawn_with_policy(harness.socket_path(), policy)
        .unwrap_or_else(|e| panic!("spawn tap for {test}(golden): {e:?}"));
    let tap_path = tap.socket_path().to_owned();

    let mut vm = ReplayVm::new(tap_path, &golden.transcript);
    for ev in golden.transcript.events() {
        vm.apply(test, ev);
    }
    vm.assert_finished(test);
    drop(vm);

    let _events = tap.into_events();
    drop(harness);

    Some(golden.transcript.clone())
}

struct ReplayVm {
    tap_path: PathBuf,
    conns: BTreeMap<usize, ReplayConn>,
    canon_to_live: BTreeMap<u32, u32>,
    expected_b2c: BTreeMap<usize, VecDeque<B2cExpected>>,
    remaining_eofs: usize,
    events: Vec<CanonEvent>,
    next_event: usize,
}

impl ReplayVm {
    fn new(tap_path: PathBuf, transcript: &CanonTranscript) -> Self {
        Self {
            tap_path,
            conns: BTreeMap::new(),
            canon_to_live: BTreeMap::new(),
            expected_b2c: expected_b2c(transcript),
            remaining_eofs: transcript
                .events()
                .iter()
                .filter(|ev| matches!(ev.kind, CanonEventKind::Eof))
                .count(),
            events: transcript.events().to_vec(),
            next_event: 0,
        }
    }

    fn apply(&mut self, test: &str, ev: &CanonEvent) {
        assert_eq!(
            self.events.get(self.next_event),
            Some(ev),
            "{test}: replay event order diverged from transcript"
        );
        match (&ev.dir, &ev.kind) {
            (TapDirection::ClientToBroker, CanonEventKind::Frame(frame)) => {
                let frame = self.substitute_bound_ids(test, ev.conn, frame);
                let conn = self.conn(ev.conn);
                conn.stream
                    .write_all(&Codec::write_frame(&frame))
                    .unwrap_or_else(|e| panic!("{test}: replay c2b write conn={}: {e:?}", ev.conn));
                conn.stream
                    .flush()
                    .unwrap_or_else(|e| panic!("{test}: replay c2b flush conn={}: {e:?}", ev.conn));
            }
            (TapDirection::BrokerToClient, CanonEventKind::Frame(expected)) => {
                let expected = self.pop_expected_b2c_frame(test, ev.conn, expected);
                self.connect_hello_mirror_cause(&expected);
                let live = {
                    let conn = self.conn(ev.conn);
                    conn.reader
                        .read_next(&mut conn.stream, STEP_TIMEOUT)
                        .unwrap_or_else(|e| panic!("{test}: replay b2c read conn={}: {e}", ev.conn))
                };
                self.match_and_bind(test, ev.conn, &expected, &live);
            }
            (TapDirection::ClientToBroker, CanonEventKind::Eof) => {
                let conn = self.conn(ev.conn);
                let _ = conn.stream.shutdown(Shutdown::Write);
                self.observe_eof(test, ev.conn, ev.dir);
            }
            (TapDirection::BrokerToClient, CanonEventKind::Eof) => {
                self.pop_expected_b2c_eof(test, ev.conn);
                let conn = self.conn(ev.conn);
                conn.reader
                    .read_to_eof(&mut conn.stream, STEP_TIMEOUT)
                    .unwrap_or_else(|e| panic!("{test}: replay b2c eof conn={}: {e}", ev.conn));
                self.observe_eof(test, ev.conn, ev.dir);
            }
        }
        self.next_event += 1;
    }

    fn pop_expected_b2c_frame(&mut self, test: &str, conn: usize, event_frame: &Frame) -> Frame {
        let queue = self.expected_b2c.get_mut(&conn).unwrap_or_else(|| {
            panic!("{test}: replay b2c conn={conn}: no expected broker-to-client queue")
        });
        match queue.pop_front() {
            Some(B2cExpected::Frame(frame)) => {
                assert_eq!(
                    &frame, event_frame,
                    "{test}: replay b2c conn={conn}: recorded event does not match queue front"
                );
                frame
            }
            Some(B2cExpected::Eof) => {
                panic!("{test}: replay b2c conn={conn}: expected EOF before frame")
            }
            None => panic!("{test}: replay b2c conn={conn}: expected queue already drained"),
        }
    }

    fn pop_expected_b2c_eof(&mut self, test: &str, conn: usize) {
        let queue = self.expected_b2c.get_mut(&conn).unwrap_or_else(|| {
            panic!("{test}: replay b2c conn={conn}: no expected broker-to-client queue")
        });
        match queue.pop_front() {
            Some(B2cExpected::Eof) => {}
            Some(B2cExpected::Frame(_)) => {
                panic!("{test}: replay b2c conn={conn}: expected frame before EOF")
            }
            None => panic!("{test}: replay b2c conn={conn}: expected queue already drained"),
        }
    }

    fn observe_eof(&mut self, test: &str, conn: usize, dir: TapDirection) {
        self.remaining_eofs = self.remaining_eofs.checked_sub(1).unwrap_or_else(|| {
            panic!("{test}: replay conn={conn} dir={dir:?}: observed unexpected EOF")
        });
    }

    fn assert_finished(&self, test: &str) {
        for (conn, queue) in &self.expected_b2c {
            if !queue.is_empty() {
                panic!(
                    "{test}: replay b2c conn={conn}: {} expected event(s) not observed",
                    queue.len()
                );
            }
        }
        if self.remaining_eofs != 0 {
            panic!(
                "{test}: replay finished with {} recorded EOF event(s) unobserved",
                self.remaining_eofs
            );
        }
    }

    fn conn(&mut self, conn: usize) -> &mut ReplayConn {
        self.conns
            .entry(conn)
            .or_insert_with(|| ReplayConn::connect(&self.tap_path, conn))
    }

    /// A monitor's outbound HELLO record is caused by accepting another
    /// connection, not a preceding client-to-broker frame. If tap ordering
    /// places that mirror before the new connection's own HELLO, connect the
    /// latter before attempting the mirror read so replay preserves causality.
    fn connect_hello_mirror_cause(&mut self, expected: &Frame) {
        let Some(client_id) = monitor_hello_client(expected) else {
            return;
        };
        if self.canon_to_live.contains_key(&client_id) {
            return;
        }
        let cause_conn = self
            .events
            .iter()
            .enumerate()
            .skip(self.next_event + 1)
            .find_map(|(index, ev)| match (&ev.dir, &ev.kind) {
                (TapDirection::BrokerToClient, CanonEventKind::Frame(frame))
                    if frame.hdr.ty == MsgType::Hello
                        && frame.hdr.peer == client_id
                        && !self.events[..index]
                            .iter()
                            .any(|prior| prior.conn == ev.conn) =>
                {
                    Some(ev.conn)
                }
                _ => None,
            });
        if let Some(conn) = cause_conn {
            self.conn(conn);
        }
    }

    fn substitute_bound_ids(&mut self, test: &str, conn: usize, frame: &Frame) -> Frame {
        let mut frame = frame.clone();
        if frame.hdr.peer == canon::EVENT_OBJECT_CANON {
            // The event object is a fixed system id, not a broker-minted
            // one: send its live literal, never a bound placeholder.
            frame.hdr.peer = canon::EVENT_OBJECT_ID;
        } else if frame.hdr.peer == canon::MONITOR_OBJECT_CANON {
            // Like the event object, the monitor object is a fixed system id.
            frame.hdr.peer = canon::MONITOR_OBJECT_ID;
        } else if frame.hdr.peer == canon::ACL_OBJECT_CANON {
            // Like the event object, the ACL object is a fixed system id.
            frame.hdr.peer = canon::ACL_OBJECT_ID;
        } else if frame.hdr.peer != 0 {
            frame.hdr.peer = self.live_id_for_c2b(frame.hdr.peer);
        }
        canon::try_visit_id_fields_in_payload(
            frame.hdr.ty,
            &mut frame.payload,
            |canon_id, bytes| {
                if canon_id == 0 {
                    return;
                }
                if canon_id == canon::EVENT_OBJECT_CANON {
                    bytes.copy_from_slice(&canon::EVENT_OBJECT_ID.to_be_bytes());
                    return;
                }
                if canon_id == canon::MONITOR_OBJECT_CANON {
                    bytes.copy_from_slice(&canon::MONITOR_OBJECT_ID.to_be_bytes());
                    return;
                }
                if canon_id == canon::ACL_OBJECT_CANON {
                    bytes.copy_from_slice(&canon::ACL_OBJECT_ID.to_be_bytes());
                    return;
                }
                let live = self.live_id_for_c2b(canon_id);
                bytes.copy_from_slice(&live.to_be_bytes());
            },
        )
        .unwrap_or_else(|detail| {
            panic!("{test}: replay c2b id substitution conn={conn}: {detail}")
        });
        frame
    }

    fn live_id_for_c2b(&mut self, canon_id: u32) -> u32 {
        if let Some(live_id) = self.canon_to_live.get(&canon_id) {
            return *live_id;
        }
        let live_id = self.mint_live_id(canon_id);
        self.canon_to_live.insert(canon_id, live_id);
        live_id
    }

    fn mint_live_id(&self, canon_id: u32) -> u32 {
        let mut candidate = canon_id;
        loop {
            if candidate > canon::MONITOR_OBJECT_ID
                && !self.canon_to_live.values().any(|live| *live == candidate)
            {
                return candidate;
            }
            candidate = candidate.wrapping_add(1);
            if candidate == 0 {
                candidate = 1;
            }
        }
    }

    fn match_and_bind(&mut self, test: &str, conn: usize, expected: &Frame, live: &Frame) {
        let mut normalized = live.clone();
        if expected.hdr.peer != 0 {
            self.bind_id(test, conn, expected.hdr.peer, live.hdr.peer);
            normalized.hdr.peer = expected.hdr.peer;
        }

        let mut expected_ids = Vec::new();
        let mut expected_payload = expected.payload.clone();
        canon::try_visit_id_fields_in_payload(
            expected.hdr.ty,
            &mut expected_payload,
            |canon_id, _| {
                expected_ids.push(canon_id);
            },
        )
        .unwrap_or_else(|detail| panic!("{test}: expected golden payload invalid: {detail}"));

        let mut next_expected = expected_ids.into_iter();
        canon::try_visit_id_fields_in_payload(
            live.hdr.ty,
            &mut normalized.payload,
            |live_id, bytes| {
                let canon_id = next_expected.next().unwrap_or_else(|| {
                    panic!("{test}: replay b2c conn={conn}: live frame has extra id field")
                });
                if canon_id != 0 {
                    self.bind_id(test, conn, canon_id, live_id);
                    bytes.copy_from_slice(&canon_id.to_be_bytes());
                }
            },
        )
        .unwrap_or_else(|detail| {
            panic!("{test}: replay b2c conn={conn}: live payload invalid: {detail}")
        });
        if let Some(canon_id) = next_expected.next() {
            panic!(
                "{test}: replay b2c conn={conn}: live frame missing canonical id field {canon_id}"
            );
        }

        if &normalized != expected {
            let expected_hex = hex(&Codec::write_frame(expected));
            let live_hex = hex(&Codec::write_frame(live));
            panic!(
                "{test}: replay b2c structural mismatch conn={conn}\nexpected: hdr={:?} bytes={expected_hex}\ngot:      hdr={:?} bytes={live_hex}",
                expected.hdr, live.hdr
            );
        }
    }

    fn bind_id(&mut self, test: &str, conn: usize, canon_id: u32, live_id: u32) {
        if live_id == 0 {
            panic!("{test}: replay b2c conn={conn}: canonical id {canon_id} matched live id 0");
        }
        match self.canon_to_live.get(&canon_id) {
            Some(bound) if *bound == live_id => {}
            Some(bound) => panic!(
                "{test}: replay b2c conn={conn}: canonical id {canon_id} was bound to {bound}, got {live_id}"
            ),
            None => {
                self.canon_to_live.insert(canon_id, live_id);
            }
        }
    }
}

fn monitor_hello_client(frame: &Frame) -> Option<u32> {
    if frame.hdr.ty != MsgType::Monitor {
        return None;
    }
    let inner = unwrap_envelope(&frame.payload).ok()?;
    let attr = |id| {
        iter_attrs(inner)
            .find(|attr| attr.id == id as u8)
            .map(|attr| attr.payload)
    };
    let client = u32::from_be_bytes(attr(MonitorAttr::Client)?.try_into().ok()?);
    let ty = u32::from_be_bytes(attr(MonitorAttr::Type)?.try_into().ok()?);
    let send = attr(MonitorAttr::Send)?;
    (ty == MsgType::Hello as u32 && send.first().is_some_and(|byte| *byte != 0)).then_some(client)
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum B2cExpected {
    Frame(Frame),
    Eof,
}

fn expected_b2c(transcript: &CanonTranscript) -> BTreeMap<usize, VecDeque<B2cExpected>> {
    let mut queues: BTreeMap<usize, VecDeque<B2cExpected>> = BTreeMap::new();
    for ev in transcript.events() {
        if ev.dir != TapDirection::BrokerToClient {
            continue;
        }
        let expected = match &ev.kind {
            CanonEventKind::Frame(frame) => B2cExpected::Frame(frame.clone()),
            CanonEventKind::Eof => B2cExpected::Eof,
        };
        queues.entry(ev.conn).or_default().push_back(expected);
    }
    queues
}

struct ReplayConn {
    stream: UnixStream,
    reader: FrameReader,
}

impl ReplayConn {
    fn connect(path: &Path, conn: usize) -> Self {
        let stream = UnixStream::connect(path)
            .unwrap_or_else(|e| panic!("replay connect conn={conn}: {e:?}"));
        stream
            .set_read_timeout(Some(Duration::from_millis(50)))
            .unwrap_or_else(|e| panic!("replay set read timeout conn={conn}: {e:?}"));
        Self {
            stream,
            reader: FrameReader::default(),
        }
    }
}

#[derive(Default)]
struct FrameReader {
    buf: Vec<u8>,
}

impl FrameReader {
    fn read_next(&mut self, stream: &mut UnixStream, timeout: Duration) -> Result<Frame, String> {
        let deadline = Instant::now() + timeout;
        loop {
            if self.buf.len() >= HEADER_SIZE {
                match Codec::read_frame(&self.buf) {
                    Ok((frame, consumed)) => {
                        self.buf.drain(..consumed);
                        return Ok(frame);
                    }
                    Err(UbusError::Short { .. }) => {}
                    Err(e) => return Err(format!("frame decode: {e:?}; hex: {}", hex(&self.buf))),
                }
            }
            if Instant::now() >= deadline {
                return Err(format!("timeout after {timeout:?}"));
            }
            if !self.read_more(stream)? {
                return Err("EOF before next frame".to_owned());
            }
        }
    }

    fn read_to_eof(&mut self, stream: &mut UnixStream, timeout: Duration) -> Result<(), String> {
        let deadline = Instant::now() + timeout;
        loop {
            if !self.buf.is_empty() {
                return Err(format!(
                    "unexpected bytes before EOF; hex: {}",
                    hex(&self.buf)
                ));
            }
            if Instant::now() >= deadline {
                return Err(format!("timeout after {timeout:?}"));
            }
            match self.read_more(stream) {
                Ok(true) => {}
                Ok(false) => return Ok(()),
                Err(e) => return Err(e),
            }
        }
    }

    fn read_more(&mut self, stream: &mut UnixStream) -> Result<bool, String> {
        let mut tmp = [0u8; 512];
        match stream.read(&mut tmp) {
            Ok(0) => Ok(false),
            Ok(n) => {
                self.buf.extend_from_slice(&tmp[..n]);
                Ok(true)
            }
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                Ok(true)
            }
            Err(e) => Err(format!("stream read: {e:?}")),
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
    use std::path::PathBuf;

    use ubus::envelope::{find, EnvBuilder, MonitorAttr, UbusAttr};
    use ubus::wire::{Frame, Header, MsgType, PROTO_VERSION};

    use super::*;

    fn status_frame() -> Frame {
        let mut env = EnvBuilder::new();
        env.push_u32(UbusAttr::Status, 0);
        Frame {
            hdr: Header {
                version: PROTO_VERSION,
                ty: MsgType::Status,
                seq: 7,
                peer: 1,
            },
            payload: env.finish(),
        }
    }

    #[test]
    fn eof_rejects_extra_frame_after_expected_reply() {
        let (mut reader_stream, mut writer) = UnixStream::pair().unwrap();
        let expected = status_frame();
        let bytes = Codec::write_frame(&expected);
        writer
            .write_all(&[bytes.as_slice(), bytes.as_slice()].concat())
            .unwrap();
        writer.shutdown(Shutdown::Write).unwrap();

        let mut reader = FrameReader::default();
        assert_eq!(
            reader.read_next(&mut reader_stream, STEP_TIMEOUT).unwrap(),
            expected
        );
        let error = reader
            .read_to_eof(&mut reader_stream, STEP_TIMEOUT)
            .unwrap_err();
        assert!(error.contains("unexpected bytes before EOF"), "{error}");
    }

    #[test]
    fn eof_rejects_new_trailing_junk_after_expected_reply() {
        let (mut reader_stream, mut writer) = UnixStream::pair().unwrap();
        let expected = status_frame();
        writer.write_all(&Codec::write_frame(&expected)).unwrap();
        let mut reader = FrameReader::default();
        assert_eq!(
            reader.read_next(&mut reader_stream, STEP_TIMEOUT).unwrap(),
            expected
        );

        // These bytes arrive only after the expected frame was consumed, so
        // checking the existing buffer alone cannot detect this divergence.
        writer.write_all(&[0xde, 0xad]).unwrap();
        writer.shutdown(Shutdown::Write).unwrap();
        let error = reader
            .read_to_eof(&mut reader_stream, STEP_TIMEOUT)
            .unwrap_err();
        assert!(error.contains("dead"), "{error}");
    }

    #[test]
    fn eof_accepts_clean_close_after_expected_reply() {
        let (mut reader_stream, mut writer) = UnixStream::pair().unwrap();
        let expected = status_frame();
        writer.write_all(&Codec::write_frame(&expected)).unwrap();
        writer.shutdown(Shutdown::Write).unwrap();
        let mut reader = FrameReader::default();
        assert_eq!(
            reader.read_next(&mut reader_stream, STEP_TIMEOUT).unwrap(),
            expected
        );
        reader
            .read_to_eof(&mut reader_stream, STEP_TIMEOUT)
            .unwrap();
    }

    #[test]
    fn c2b_substitution_mints_unbound_client_invented_ids() {
        let transcript = canon::from_canonical_events(Vec::new());
        let mut vm = ReplayVm::new(PathBuf::from("/unused"), &transcript);
        vm.canon_to_live.insert(1, 2);

        let mut env = EnvBuilder::new();
        env.push_u32(UbusAttr::ObjId, 2);
        let frame = Frame {
            hdr: Header {
                version: PROTO_VERSION,
                ty: MsgType::RemoveObject,
                seq: 9,
                peer: 2,
            },
            payload: env.finish(),
        };

        let substituted = vm.substitute_bound_ids("mint-test", 0, &frame);
        assert_eq!(substituted.hdr.peer, 4);
        assert_eq!(
            find(&substituted.payload, UbusAttr::ObjId).and_then(|attr| attr.as_u32()),
            Some(4)
        );
        assert_eq!(vm.canon_to_live.get(&2), Some(&4));
    }

    #[test]
    fn c2b_substitution_sends_monitor_sentinel_as_system_id() {
        let transcript = canon::from_canonical_events(Vec::new());
        let mut vm = ReplayVm::new(PathBuf::from("/unused"), &transcript);
        let mut env = EnvBuilder::new();
        env.push_u32(UbusAttr::ObjId, canon::MONITOR_OBJECT_CANON);
        let frame = Frame {
            hdr: Header {
                version: PROTO_VERSION,
                ty: MsgType::RemoveObject,
                seq: 9,
                peer: canon::MONITOR_OBJECT_CANON,
            },
            payload: env.finish(),
        };

        let substituted = vm.substitute_bound_ids("monitor-sentinel", 0, &frame);
        assert_eq!(substituted.hdr.peer, canon::MONITOR_OBJECT_ID);
        assert_eq!(
            find(&substituted.payload, UbusAttr::ObjId).and_then(|attr| attr.as_u32()),
            Some(canon::MONITOR_OBJECT_ID)
        );
        for canon_id in [0, 1, 2, 3] {
            assert_eq!(vm.mint_live_id(canon_id), 4);
        }
    }

    #[test]
    fn c2b_substitution_sends_acl_sentinel_as_system_id() {
        let transcript = canon::from_canonical_events(Vec::new());
        let mut vm = ReplayVm::new(PathBuf::from("/unused"), &transcript);
        let mut env = EnvBuilder::new();
        env.push_u32(UbusAttr::ObjId, canon::ACL_OBJECT_CANON);
        let frame = Frame {
            hdr: Header {
                version: PROTO_VERSION,
                ty: MsgType::RemoveObject,
                seq: 9,
                peer: canon::ACL_OBJECT_CANON,
            },
            payload: env.finish(),
        };

        let substituted = vm.substitute_bound_ids("acl-sentinel", 0, &frame);
        assert_eq!(substituted.hdr.peer, canon::ACL_OBJECT_ID);
        assert_eq!(
            find(&substituted.payload, UbusAttr::ObjId).and_then(|attr| attr.as_u32()),
            Some(canon::ACL_OBJECT_ID)
        );
    }

    #[test]
    fn monitor_hello_client_requires_an_outbound_hello() {
        let mut env = EnvBuilder::new();
        env.push_u32_id(MonitorAttr::Client as u8, 9);
        env.push_u32_id(MonitorAttr::Type as u8, MsgType::Hello as u32);
        env.push_u8_id(MonitorAttr::Send as u8, 1);
        let hello = Frame {
            hdr: Header {
                version: PROTO_VERSION,
                ty: MsgType::Monitor,
                seq: 1,
                peer: 0,
            },
            payload: env.finish(),
        };
        assert_eq!(monitor_hello_client(&hello), Some(9));

        let mut inbound = hello.clone();
        let mut env = EnvBuilder::new();
        env.push_u32_id(MonitorAttr::Client as u8, 9);
        env.push_u32_id(MonitorAttr::Type as u8, MsgType::Hello as u32);
        env.push_u8_id(MonitorAttr::Send as u8, 0);
        inbound.payload = env.finish();
        assert_eq!(monitor_hello_client(&inbound), None);
    }
}
