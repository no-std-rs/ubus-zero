//! Canonical wire transcripts for cross-broker byte diffing.
//!
//! Broker-minted ids are intentionally unstable across implementations.
//! This module rewrites only those ids to transcript-local first-seen
//! numbers, leaving all other frame bytes byte-for-byte comparable.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use ubus::blob::{align4, EXT_FLAG, ID_MASK, ID_SHIFT, LEN_MASK};
use ubus::blobmsg::BlobType;
use ubus::envelope::{MonitorAttr, UbusAttr};
use ubus::error::UbusError;
use ubus::wire::{Codec, Frame, MsgType};

use crate::wire_tap::{TapDirection, TapEvent, TapEventKind};

/// Canonicalized transcript of one tapped session: per-(conn,
/// direction) decoded frame streams with broker-minted ids rewritten
/// to a stable first-appearance numbering shared across the transcript.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CanonTranscript {
    streams: BTreeMap<StreamKey, Vec<Frame>>,
    events: Vec<CanonEvent>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CanonEvent {
    pub conn: usize,
    pub dir: TapDirection,
    pub kind: CanonEventKind,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CanonEventKind {
    Frame(Frame),
    Eof,
}

impl CanonTranscript {
    pub fn frames(&self, conn: usize, dir: TapDirection) -> &[Frame] {
        self.streams
            .get(&StreamKey::new(conn, dir))
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    pub fn events(&self) -> &[CanonEvent] {
        &self.events
    }
}

pub fn from_canonical_events(events: Vec<CanonEvent>) -> CanonTranscript {
    let mut streams: BTreeMap<StreamKey, Vec<Frame>> = BTreeMap::new();
    for ev in &events {
        if let CanonEventKind::Frame(frame) = &ev.kind {
            streams
                .entry(StreamKey::new(ev.conn, ev.dir))
                .or_default()
                .push(frame.clone());
        }
    }
    CanonTranscript { streams, events }
}

#[derive(Debug)]
pub enum CanonError {
    Decode {
        conn: usize,
        dir: TapDirection,
        offset: usize,
        source: UbusError,
        context: String,
    },
    Envelope {
        conn: usize,
        dir: TapDirection,
        frame_index: usize,
        payload_offset: usize,
        detail: &'static str,
        context: String,
    },
}

impl fmt::Display for CanonError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CanonError::Decode {
                conn,
                dir,
                offset,
                source,
                context,
            } => write!(
                f,
                "canon decode error conn={conn} dir={dir:?} offset={offset}: {source}; context={context}"
            ),
            CanonError::Envelope {
                conn,
                dir,
                frame_index,
                payload_offset,
                detail,
                context,
            } => write!(
                f,
                "canon envelope error conn={conn} dir={dir:?} frame={frame_index} payload_offset={payload_offset}: {detail}; context={context}"
            ),
        }
    }
}

impl std::error::Error for CanonError {}

/// Canonicalize a tapped session.
pub fn canonicalize(events: &[TapEvent]) -> Result<CanonTranscript, CanonError> {
    let streams = collect_streams(events);
    let mut decoded = BTreeMap::new();
    for (key, bytes) in &streams {
        decoded.insert(*key, decode_stream(*key, bytes)?);
    }

    let mut ids = IdMap::default();
    scan_ids_in_arrival_order(events, &decoded, &mut ids)?;

    let mut canon_streams = BTreeMap::new();
    for (key, frames) in &decoded {
        let mut out = Vec::with_capacity(frames.len());
        for (frame_index, decoded_frame) in frames.iter().enumerate() {
            out.push(canonical_frame(
                *key,
                frame_index,
                &decoded_frame.frame,
                &ids,
            )?);
        }
        canon_streams.insert(*key, out);
    }

    let canon_events = canonical_events_in_arrival_order(events, &decoded, &ids)?;

    Ok(CanonTranscript {
        streams: canon_streams,
        events: canon_events,
    })
}

/// None when canonical per-(conn, dir) event streams, including EOF
/// positions within each stream, are identical. Cross-connection
/// global interleaving is intentionally ignored because the kernel and
/// tap pumps may legally schedule independent connections differently.
pub fn diff_events(a: &CanonTranscript, b: &CanonTranscript) -> Option<String> {
    let a_streams = event_streams(a);
    let b_streams = event_streams(b);
    if a_streams == b_streams {
        return None;
    }

    let a_keys: BTreeSet<_> = a_streams.keys().copied().collect();
    let b_keys: BTreeSet<_> = b_streams.keys().copied().collect();
    if a_keys != b_keys {
        let mut s = String::new();
        s.push_str("event stream sets differ\n");
        s.push_str(&format!("left:  {:?}\n", a_keys));
        s.push_str(&format!("right: {:?}\n", b_keys));
        return Some(s);
    }

    let keys: BTreeSet<_> = a_streams.keys().chain(b_streams.keys()).copied().collect();
    for key in keys {
        let left = a_streams.get(&key).map(Vec::as_slice).unwrap_or(&[]);
        let right = b_streams.get(&key).map(Vec::as_slice).unwrap_or(&[]);
        if left == right {
            continue;
        }

        let mut s = String::new();
        s.push_str(&format!(
            "event stream mismatch conn={} dir={:?}\n",
            key.conn, key.dir
        ));
        let shared = left.len().min(right.len());
        if let Some(i) = (0..shared).find(|&i| left[i] != right[i]) {
            s.push_str(&format!("first mismatching stream event index {i}\n"));
            s.push_str(&render_stream_event("left", &left[i]));
            s.push_str(&render_stream_event("right", &right[i]));
        } else {
            s.push_str(&format!(
                "length mismatch: left {} event(s), right {} event(s)\n",
                left.len(),
                right.len()
            ));
            for (i, event) in left[shared..].iter().enumerate() {
                s.push_str(&format!("extra left stream event index {}\n", shared + i));
                s.push_str(&render_stream_event("left", event));
            }
            for (i, event) in right[shared..].iter().enumerate() {
                s.push_str(&format!("extra right stream event index {}\n", shared + i));
                s.push_str(&render_stream_event("right", event));
            }
        }
        return Some(s);
    }

    None
}

/// None when transcripts are canonically identical; Some(rendered
/// human-readable diff) otherwise.
pub fn diff(a: &CanonTranscript, b: &CanonTranscript) -> Option<String> {
    let a_conns = conn_set(a);
    let b_conns = conn_set(b);
    if a_conns != b_conns {
        let mut s = String::new();
        s.push_str("connection sets differ\n");
        s.push_str(&format!("left:  {:?}\n", a_conns));
        s.push_str(&format!("right: {:?}\n", b_conns));
        return Some(s);
    }

    let keys: BTreeSet<_> = a.streams.keys().chain(b.streams.keys()).copied().collect();
    for key in keys {
        let left = a.streams.get(&key).map(Vec::as_slice).unwrap_or(&[]);
        let right = b.streams.get(&key).map(Vec::as_slice).unwrap_or(&[]);
        if left == right {
            continue;
        }

        let mut s = String::new();
        s.push_str(&format!(
            "stream mismatch conn={} dir={:?}\n",
            key.conn, key.dir
        ));
        let shared = left.len().min(right.len());
        if let Some(i) = (0..shared).find(|&i| left[i] != right[i]) {
            s.push_str(&format!("first mismatching frame index {i}\n"));
            s.push_str(&render_frame("left", &left[i]));
            s.push_str(&render_frame("right", &right[i]));
        } else {
            s.push_str(&format!(
                "length mismatch: left {} frame(s), right {} frame(s)\n",
                left.len(),
                right.len()
            ));
            for (i, frame) in left[shared..].iter().enumerate() {
                s.push_str(&format!("extra left frame index {}\n", shared + i));
                s.push_str(&render_frame("left", frame));
            }
            for (i, frame) in right[shared..].iter().enumerate() {
                s.push_str(&format!("extra right frame index {}\n", shared + i));
                s.push_str(&render_frame("right", frame));
            }
        }
        return Some(s);
    }

    None
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
struct StreamKey {
    conn: usize,
    dir: DirKey,
}

impl StreamKey {
    fn new(conn: usize, dir: TapDirection) -> Self {
        Self {
            conn,
            dir: DirKey::from(dir),
        }
    }
}

#[derive(Clone, Copy, Eq, PartialEq, Ord, PartialOrd)]
enum DirKey {
    ClientToBroker,
    BrokerToClient,
}

impl From<TapDirection> for DirKey {
    fn from(dir: TapDirection) -> Self {
        match dir {
            TapDirection::ClientToBroker => Self::ClientToBroker,
            TapDirection::BrokerToClient => Self::BrokerToClient,
        }
    }
}

impl fmt::Debug for DirKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DirKey::ClientToBroker => f.write_str("ClientToBroker"),
            DirKey::BrokerToClient => f.write_str("BrokerToClient"),
        }
    }
}

impl From<DirKey> for TapDirection {
    fn from(dir: DirKey) -> Self {
        match dir {
            DirKey::ClientToBroker => Self::ClientToBroker,
            DirKey::BrokerToClient => Self::BrokerToClient,
        }
    }
}

#[derive(Debug)]
struct DecodedFrame {
    end: usize,
    frame: Frame,
}

#[derive(Default)]
struct IdMap {
    next: u32,
    by_raw: BTreeMap<u32, u32>,
}

impl IdMap {
    fn observe(&mut self, raw: u32) {
        // `0` (broker) and the reserved event, ACL, and monitor system object
        // ids (identical on both brokers) are fixed system constants, not
        // broker-minted ids — they never take a sequential slot.
        if raw == 0
            || raw == EVENT_OBJECT_ID
            || raw == ACL_OBJECT_ID
            || raw == MONITOR_OBJECT_ID
            || self.by_raw.contains_key(&raw)
        {
            return;
        }
        let canon = self.next + 1;
        self.next = canon;
        self.by_raw.insert(raw, canon);
    }

    fn map_existing(&self, raw: u32) -> u32 {
        match raw {
            0 => 0,
            // The event object maps to a reserved high sentinel, *not*
            // into the sequential range (which starts at 1). Sharing a
            // value with canon `#1` would make the golden replay VM
            // conflate the event object with the first broker-minted id.
            EVENT_OBJECT_ID => EVENT_OBJECT_CANON,
            ACL_OBJECT_ID => ACL_OBJECT_CANON,
            MONITOR_OBJECT_ID => MONITOR_OBJECT_CANON,
            _ => *self
                .by_raw
                .get(&raw)
                .expect("id rewrite reached an unobserved nonzero id"),
        }
    }
}

fn collect_streams(events: &[TapEvent]) -> BTreeMap<StreamKey, Vec<u8>> {
    let mut streams = BTreeMap::new();
    for ev in events {
        let TapEventKind::Bytes(bytes) = &ev.kind else {
            continue;
        };
        streams
            .entry(StreamKey::new(ev.conn, ev.dir))
            .or_insert_with(Vec::new)
            .extend_from_slice(bytes);
    }
    streams
}

fn decode_stream(key: StreamKey, bytes: &[u8]) -> Result<Vec<DecodedFrame>, CanonError> {
    let mut frames = Vec::new();
    let mut offset = 0usize;
    while offset < bytes.len() {
        match Codec::read_frame(&bytes[offset..]) {
            Ok((frame, consumed)) => {
                offset += consumed;
                frames.push(DecodedFrame { end: offset, frame });
            }
            Err(source) => {
                return Err(CanonError::Decode {
                    conn: key.conn,
                    dir: key.dir.into(),
                    offset,
                    source,
                    context: hex_context(&bytes[offset..]),
                });
            }
        }
    }
    Ok(frames)
}

fn scan_ids_in_arrival_order(
    events: &[TapEvent],
    decoded: &BTreeMap<StreamKey, Vec<DecodedFrame>>,
    ids: &mut IdMap,
) -> Result<(), CanonError> {
    let mut seen_bytes: BTreeMap<StreamKey, usize> = BTreeMap::new();
    let mut next_frame: BTreeMap<StreamKey, usize> = BTreeMap::new();

    for ev in events {
        let TapEventKind::Bytes(bytes) = &ev.kind else {
            continue;
        };
        let key = StreamKey::new(ev.conn, ev.dir);
        let after = seen_bytes.get(&key).copied().unwrap_or(0) + bytes.len();
        seen_bytes.insert(key, after);

        let frames = decoded
            .get(&key)
            .expect("decoded streams were built from the same tap events");
        let index = next_frame.entry(key).or_insert(0);
        while *index < frames.len() && frames[*index].end <= after {
            observe_frame_ids(key, *index, &frames[*index].frame, ids)?;
            *index += 1;
        }
    }

    Ok(())
}

fn canonical_events_in_arrival_order(
    events: &[TapEvent],
    decoded: &BTreeMap<StreamKey, Vec<DecodedFrame>>,
    ids: &IdMap,
) -> Result<Vec<CanonEvent>, CanonError> {
    let mut out = Vec::new();
    let mut seen_bytes: BTreeMap<StreamKey, usize> = BTreeMap::new();
    let mut next_frame: BTreeMap<StreamKey, usize> = BTreeMap::new();

    for ev in events {
        let key = StreamKey::new(ev.conn, ev.dir);
        match &ev.kind {
            TapEventKind::Bytes(bytes) => {
                let after = seen_bytes.get(&key).copied().unwrap_or(0) + bytes.len();
                seen_bytes.insert(key, after);

                let frames = decoded
                    .get(&key)
                    .expect("decoded streams were built from the same tap events");
                let index = next_frame.entry(key).or_insert(0);
                while *index < frames.len() && frames[*index].end <= after {
                    out.push(CanonEvent {
                        conn: ev.conn,
                        dir: ev.dir,
                        kind: CanonEventKind::Frame(canonical_frame(
                            key,
                            *index,
                            &frames[*index].frame,
                            ids,
                        )?),
                    });
                    *index += 1;
                }
            }
            TapEventKind::Eof => out.push(CanonEvent {
                conn: ev.conn,
                dir: ev.dir,
                kind: CanonEventKind::Eof,
            }),
        }
    }

    Ok(out)
}

fn canonical_frame(
    key: StreamKey,
    frame_index: usize,
    frame: &Frame,
    ids: &IdMap,
) -> Result<Frame, CanonError> {
    let mut frame = frame.clone();
    if frame.hdr.peer != 0 {
        frame.hdr.peer = ids.map_existing(frame.hdr.peer);
    }
    rewrite_id_attrs(key, frame_index, frame.hdr.ty, &mut frame.payload, ids)?;
    Ok(frame)
}

fn observe_frame_ids(
    key: StreamKey,
    frame_index: usize,
    frame: &Frame,
    ids: &mut IdMap,
) -> Result<(), CanonError> {
    ids.observe(frame.hdr.peer);
    let mut payload = frame.payload.clone();
    visit_id_attrs(key, frame_index, frame.hdr.ty, &mut payload, |raw, _| {
        ids.observe(raw);
    })
}

fn rewrite_id_attrs(
    key: StreamKey,
    frame_index: usize,
    ty: MsgType,
    payload: &mut [u8],
    ids: &IdMap,
) -> Result<(), CanonError> {
    visit_id_attrs(key, frame_index, ty, payload, |raw, bytes| {
        let canon = ids.map_existing(raw);
        bytes.copy_from_slice(&canon.to_be_bytes());
    })
}

/// The builtin `ubus.event` system object id (`UBUS_SYSTEM_OBJECT_EVENT`),
/// the live value both brokers use. `register` / `send` INVOKEs are
/// addressed here, and the golden replay VM sends it back verbatim.
pub(crate) const EVENT_OBJECT_ID: u32 = 1;

/// Canonical placeholder for [`EVENT_OBJECT_ID`]. A reserved sentinel far
/// above the sequential canon range so it can never collide with a
/// broker-minted id's placeholder (`#1`, `#2`, …). The replay VM maps it
/// back to [`EVENT_OBJECT_ID`] when driving the live broker.
pub(crate) const EVENT_OBJECT_CANON: u32 = 0xffff_ffff;

/// The builtin `ubus` ACL system object id (`UBUS_SYSTEM_OBJECT_ACL`),
/// the live value both brokers use.
pub(crate) const ACL_OBJECT_ID: u32 = 2;

/// Canonical placeholder for [`ACL_OBJECT_ID`]. A reserved sentinel far
/// above the sequential canon range so it can never collide with a
/// broker-minted id's placeholder (`#1`, `#2`, …). The replay VM maps it
/// back to [`ACL_OBJECT_ID`] when driving the live broker.
pub(crate) const ACL_OBJECT_CANON: u32 = 0xffff_fffd;

/// The builtin `ubus.monitor` system object id (`UBUS_SYSTEM_OBJECT_MONITOR`),
/// the live value both brokers use.
pub(crate) const MONITOR_OBJECT_ID: u32 = 3;

/// Canonical placeholder for [`MONITOR_OBJECT_ID`]. A reserved sentinel far
/// above the sequential canon range so it can never collide with a
/// broker-minted id's placeholder (`#1`, `#2`, …). The replay VM maps it
/// back to [`MONITOR_OBJECT_ID`] when driving the live broker.
pub(crate) const MONITOR_OBJECT_CANON: u32 = 0xffff_fffe;

/// When a frame's `Data` blobmsg carries a broker-minted object id in a
/// named INT32 field, returns that field's name so it can be
/// canonicalized like any envelope-level id. Two cases arise:
///
/// * broker-minted `ubus.object.add`/`remove` fan-out carries the changed
///   object's id in an `id` field (gated on the reserved event name);
/// * a `ubus listen` `register` INVOKE echoes the listener's own object
///   id in an `object` field (gated on the reserved method name *and* the
///   frame being addressed to the event object, so a user object that
///   happens to expose a `register` method is never mis-mapped).
///
/// Everything else — a user's `ubus send` payload, arbitrary INVOKE args —
/// stays byte-literal: the gate is the reserved name, never the field
/// name alone. Best-effort: a malformed envelope yields `None` and the
/// caller's own walk surfaces the error.
fn event_blobmsg_id_field_in_stream(payload: &[u8]) -> Option<&'static str> {
    let method = bare_attr(payload, UbusAttr::Method as u8).and_then(attr_as_str)?;
    match method {
        "ubus.object.add" | "ubus.object.remove" => Some("id"),
        "register" if stream_obj_id_is_event(payload) => Some("object"),
        _ => None,
    }
}

fn stream_obj_id_is_event(payload: &[u8]) -> bool {
    // Accepts the raw event object id (record time, walking the live
    // wire) and its canonical sentinel (replay time, walking a golden).
    matches!(
        bare_attr(payload, UbusAttr::ObjId as u8).and_then(attr_as_u32),
        Some(EVENT_OBJECT_ID) | Some(EVENT_OBJECT_CANON)
    )
}

fn bare_attr(payload: &[u8], wanted_id: u8) -> Option<&[u8]> {
    let mut cursor = 0usize;
    while cursor < payload.len() {
        if payload.len() - cursor < 4 {
            return None;
        }
        let id_len = u32::from_be_bytes(payload[cursor..cursor + 4].try_into().unwrap());
        let attr_id = ((id_len & ID_MASK) >> ID_SHIFT) as u8;
        let attr_total = (id_len & LEN_MASK) as usize;
        if attr_total < 4 || cursor + attr_total > payload.len() {
            return None;
        }
        if attr_id == wanted_id {
            return Some(&payload[cursor + 4..cursor + attr_total]);
        }
        let consumed = align4(attr_total);
        if cursor + consumed > payload.len() {
            return None;
        }
        cursor += consumed;
    }
    None
}

fn attr_as_str(payload: &[u8]) -> Option<&str> {
    let end = payload.iter().position(|&b| b == 0)?;
    std::str::from_utf8(&payload[..end]).ok()
}

fn attr_as_u32(payload: &[u8]) -> Option<u32> {
    payload
        .get(..4)
        .map(|bytes| u32::from_be_bytes(bytes.try_into().unwrap()))
}

/// Walk the blobmsg attr stream that is the inner payload of a `Data`
/// attr and invoke `f(raw, &mut value_bytes)` on the 4-byte big-endian
/// value of the named INT32 field `field`, if present. Best-effort: the
/// enclosing envelope walk already bounds-checked the `Data` attr, and
/// broker-minted / listener-echoed payloads are well-formed by
/// construction, so a malformed blobmsg simply stops the walk. Both
/// `visit_id_attrs` and [`try_visit_id_fields_in_payload`] route through
/// this so the golden recorder and the replay VM visit the same id field
/// in the same order.
fn visit_named_blobmsg_id(contents: &mut [u8], field: &str, f: &mut impl FnMut(u32, &mut [u8])) {
    let field = field.as_bytes();
    let total = contents.len();
    let mut cursor = 0usize;
    while cursor + 4 <= total {
        let id_len = u32::from_be_bytes([
            contents[cursor],
            contents[cursor + 1],
            contents[cursor + 2],
            contents[cursor + 3],
        ]);
        if id_len & EXT_FLAG == 0 {
            return;
        }
        let ty = ((id_len & ID_MASK) >> ID_SHIFT) as u8;
        let attr_total = (id_len & LEN_MASK) as usize;
        if attr_total < 6 || cursor + attr_total > total {
            return;
        }
        let name_len = u16::from_be_bytes([contents[cursor + 4], contents[cursor + 5]]) as usize;
        let name_field = 2 + name_len + 1;
        let hdr = 4 + name_field + ((4 - (name_field & 3)) & 3);
        if hdr > attr_total {
            return;
        }
        if ty == BlobType::Int32 as u8
            && name_len == field.len()
            && cursor + 6 + name_len <= cursor + attr_total
            && &contents[cursor + 6..cursor + 6 + name_len] == field
        {
            let value_off = cursor + hdr;
            if value_off + 4 <= cursor + attr_total {
                let raw = u32::from_be_bytes([
                    contents[value_off],
                    contents[value_off + 1],
                    contents[value_off + 2],
                    contents[value_off + 3],
                ]);
                f(raw, &mut contents[value_off..value_off + 4]);
            }
        }
        let consumed = align4(attr_total);
        if cursor + consumed > total {
            return;
        }
        cursor += consumed;
    }
}

fn visit_id_attrs(
    key: StreamKey,
    frame_index: usize,
    ty: MsgType,
    payload: &mut [u8],
    f: impl FnMut(u32, &mut [u8]),
) -> Result<(), CanonError> {
    match visit_frame_id_fields(ty, payload, f) {
        Ok(()) => Ok(()),
        Err((payload_offset, detail)) => {
            envelope_err(key, frame_index, payload_offset, detail, payload)
        }
    }
}

/// Visit all broker-minted id fields in an envelope. MONITOR uses its own
/// outer attribute id space; its DATA value is a bare normal UBUS attr stream.
fn visit_frame_id_fields(
    ty: MsgType,
    payload: &mut [u8],
    mut f: impl FnMut(u32, &mut [u8]),
) -> Result<(), (usize, &'static str)> {
    if payload.is_empty() {
        return Ok(());
    }
    if payload.len() < 4 {
        return Err((0, "short outer envelope"));
    }
    let outer = u32::from_be_bytes(payload[..4].try_into().unwrap());
    let total = (outer & LEN_MASK) as usize;
    if total < 4 || total > payload.len() {
        return Err((0, "bad outer envelope length"));
    }
    let inner = &mut payload[4..total];
    if ty == MsgType::Monitor {
        visit_monitor_attr_stream(inner, 4, &mut f)
    } else {
        visit_ubus_attr_stream(inner, 4, &mut f)
    }
}

fn visit_ubus_attr_stream(
    payload: &mut [u8],
    payload_offset: usize,
    f: &mut impl FnMut(u32, &mut [u8]),
) -> Result<(), (usize, &'static str)> {
    let id_field = event_blobmsg_id_field_in_stream(payload);
    let total = payload.len();
    let mut cursor = 0usize;
    while cursor < total {
        if total - cursor < 4 {
            return Err((payload_offset + cursor, "short attr header"));
        }
        let id_len = u32::from_be_bytes([
            payload[cursor],
            payload[cursor + 1],
            payload[cursor + 2],
            payload[cursor + 3],
        ]);
        let attr_id = ((id_len & ID_MASK) >> ID_SHIFT) as u8;
        let attr_total = (id_len & LEN_MASK) as usize;
        if attr_total < 4 {
            return Err((payload_offset + cursor, "attr length below header"));
        }
        if cursor + attr_total > total {
            return Err((payload_offset + cursor, "attr length exceeds envelope"));
        }

        if attr_id == UbusAttr::ObjId as u8
            || attr_id == UbusAttr::ObjType as u8
            || attr_id == UbusAttr::Target as u8
        {
            visit_u32_id(payload, payload_offset + cursor, cursor, attr_total, f)?;
        } else if attr_id == UbusAttr::Subscribers as u8 {
            visit_subscriber_ids(
                &mut payload[cursor + 4..cursor + attr_total],
                payload_offset + cursor + 4,
                f,
            )?;
        } else if let Some(field) = id_field.filter(|_| attr_id == UbusAttr::Data as u8) {
            visit_named_blobmsg_id(&mut payload[cursor + 4..cursor + attr_total], field, f);
        }

        let consumed = align4(attr_total);
        if cursor + consumed > total {
            return Err((payload_offset + cursor, "attr padding exceeds envelope"));
        }
        cursor += consumed;
    }
    Ok(())
}

fn visit_monitor_attr_stream(
    payload: &mut [u8],
    payload_offset: usize,
    f: &mut impl FnMut(u32, &mut [u8]),
) -> Result<(), (usize, &'static str)> {
    let total = payload.len();
    let mut cursor = 0usize;
    while cursor < total {
        if total - cursor < 4 {
            return Err((payload_offset + cursor, "short attr header"));
        }
        let id_len = u32::from_be_bytes(payload[cursor..cursor + 4].try_into().unwrap());
        let attr_id = ((id_len & ID_MASK) >> ID_SHIFT) as u8;
        let attr_total = (id_len & LEN_MASK) as usize;
        if attr_total < 4 {
            return Err((payload_offset + cursor, "attr length below header"));
        }
        if cursor + attr_total > total {
            return Err((payload_offset + cursor, "attr length exceeds envelope"));
        }

        if attr_id == MonitorAttr::Client as u8 || attr_id == MonitorAttr::Peer as u8 {
            visit_u32_id(payload, payload_offset + cursor, cursor, attr_total, f)?;
        } else if attr_id == MonitorAttr::Data as u8 {
            visit_ubus_attr_stream(
                &mut payload[cursor + 4..cursor + attr_total],
                payload_offset + cursor + 4,
                f,
            )?;
        }

        let consumed = align4(attr_total);
        if cursor + consumed > total {
            return Err((payload_offset + cursor, "attr padding exceeds envelope"));
        }
        cursor += consumed;
    }
    Ok(())
}

fn visit_u32_id(
    payload: &mut [u8],
    payload_offset: usize,
    cursor: usize,
    attr_total: usize,
    f: &mut impl FnMut(u32, &mut [u8]),
) -> Result<(), (usize, &'static str)> {
    if attr_total < 8 {
        return Err((payload_offset, "short u32 id attr"));
    }
    let value_offset = cursor + 4;
    let raw = u32::from_be_bytes(payload[value_offset..value_offset + 4].try_into().unwrap());
    f(raw, &mut payload[value_offset..value_offset + 4]);
    Ok(())
}

fn visit_subscriber_ids(
    payload: &mut [u8],
    payload_offset: usize,
    f: &mut impl FnMut(u32, &mut [u8]),
) -> Result<(), (usize, &'static str)> {
    let total = payload.len();
    let mut cursor = 0usize;
    while cursor < total {
        if total - cursor < 4 {
            return Err((payload_offset + cursor, "short Subscribers attr header"));
        }
        let id_len = u32::from_be_bytes(payload[cursor..cursor + 4].try_into().unwrap());
        let attr_total = (id_len & LEN_MASK) as usize;
        if attr_total < 8 {
            return Err((payload_offset + cursor, "short Subscribers u32 attr"));
        }
        if cursor + attr_total > total {
            return Err((
                payload_offset + cursor,
                "Subscribers attr length exceeds container",
            ));
        }
        let value_offset = cursor + 4;
        let raw = u32::from_be_bytes(payload[value_offset..value_offset + 4].try_into().unwrap());
        f(raw, &mut payload[value_offset..value_offset + 4]);

        let consumed = align4(attr_total);
        if cursor + consumed > total {
            return Err((
                payload_offset + cursor,
                "Subscribers attr padding exceeds container",
            ));
        }
        cursor += consumed;
    }
    Ok(())
}

fn envelope_err<T>(
    key: StreamKey,
    frame_index: usize,
    payload_offset: usize,
    detail: &'static str,
    payload: &[u8],
) -> Result<T, CanonError> {
    Err(CanonError::Envelope {
        conn: key.conn,
        dir: key.dir.into(),
        frame_index,
        payload_offset,
        detail,
        context: hex_context(&payload[payload_offset.min(payload.len())..]),
    })
}

fn conn_set(transcript: &CanonTranscript) -> BTreeSet<usize> {
    transcript.streams.keys().map(|k| k.conn).collect()
}

fn event_streams(transcript: &CanonTranscript) -> BTreeMap<StreamKey, Vec<CanonEventKind>> {
    let mut streams: BTreeMap<StreamKey, Vec<CanonEventKind>> = BTreeMap::new();
    for ev in &transcript.events {
        streams
            .entry(StreamKey::new(ev.conn, ev.dir))
            .or_default()
            .push(ev.kind.clone());
    }
    streams
}

fn render_frame(label: &str, frame: &Frame) -> String {
    format!(
        "{label}: hdr={:?} attrs=[{}] payload={}\n",
        frame.hdr,
        attr_summary(&frame.payload),
        hex(&frame.payload)
    )
}

fn render_stream_event(label: &str, event: &CanonEventKind) -> String {
    match event {
        CanonEventKind::Frame(frame) => format!(
            "{label}: kind=bytes hdr={:?} attrs=[{}] payload={}\n",
            frame.hdr,
            attr_summary(&frame.payload),
            hex(&frame.payload)
        ),
        CanonEventKind::Eof => format!("{label}: kind=eof\n"),
    }
}

pub(crate) fn try_visit_id_fields_in_payload(
    ty: MsgType,
    payload: &mut [u8],
    f: impl FnMut(u32, &mut [u8]),
) -> Result<(), &'static str> {
    visit_frame_id_fields(ty, payload, f).map_err(|(_, detail)| detail)
}

fn attr_summary(payload: &[u8]) -> String {
    if payload.is_empty() {
        return "empty".to_owned();
    }
    if payload.len() < 4 {
        return "malformed(short outer envelope)".to_owned();
    }

    let outer = u32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]);
    let total = (outer & LEN_MASK) as usize;
    if total < 4 || total > payload.len() {
        return "malformed(bad outer envelope length)".to_owned();
    }

    let mut cursor = 4usize;
    let mut out = Vec::new();
    while cursor < total {
        if total - cursor < 4 {
            out.push("malformed(short attr header)".to_owned());
            break;
        }
        let id_len = u32::from_be_bytes([
            payload[cursor],
            payload[cursor + 1],
            payload[cursor + 2],
            payload[cursor + 3],
        ]);
        let id = ((id_len & ID_MASK) >> ID_SHIFT) as u8;
        let attr_total = (id_len & LEN_MASK) as usize;
        if attr_total < 4 || cursor + attr_total > total {
            out.push(format!("{}=malformed", attr_name(id)));
            break;
        }

        let value = &payload[cursor + 4..cursor + attr_total];
        out.push(format_attr(id, value));
        let consumed = align4(attr_total);
        if cursor + consumed > total {
            out.push("malformed(attr padding exceeds envelope)".to_owned());
            break;
        }
        cursor += consumed;
    }
    out.join(", ")
}

fn format_attr(id: u8, value: &[u8]) -> String {
    match UbusAttr::from_u8(id) {
        Some(UbusAttr::ObjId | UbusAttr::ObjType | UbusAttr::Status) if value.len() >= 4 => {
            let n = u32::from_be_bytes([value[0], value[1], value[2], value[3]]);
            format!("{}={n}", attr_name(id))
        }
        Some(UbusAttr::ObjPath | UbusAttr::Method) => {
            let rendered = value
                .iter()
                .position(|&b| b == 0)
                .and_then(|end| std::str::from_utf8(&value[..end]).ok())
                .unwrap_or("<non-utf8>");
            format!("{}={rendered:?}", attr_name(id))
        }
        Some(UbusAttr::Subscribers) => format_subscribers(value),
        _ => format!("{}({}B)", attr_name(id), value.len()),
    }
}

fn format_subscribers(value: &[u8]) -> String {
    let mut cursor = 0usize;
    let mut ids = Vec::new();
    while cursor < value.len() {
        if value.len() - cursor < 4 {
            return "Subscribers=malformed".to_owned();
        }
        let id_len = u32::from_be_bytes([
            value[cursor],
            value[cursor + 1],
            value[cursor + 2],
            value[cursor + 3],
        ]);
        let attr_total = (id_len & LEN_MASK) as usize;
        if attr_total < 8 || cursor + attr_total > value.len() {
            return "Subscribers=malformed".to_owned();
        }
        let value_offset = cursor + 4;
        ids.push(u32::from_be_bytes([
            value[value_offset],
            value[value_offset + 1],
            value[value_offset + 2],
            value[value_offset + 3],
        ]));
        let consumed = align4(attr_total);
        if cursor + consumed > value.len() {
            return "Subscribers=malformed".to_owned();
        }
        cursor += consumed;
    }
    format!("Subscribers={ids:?}")
}

fn attr_name(id: u8) -> &'static str {
    match UbusAttr::from_u8(id) {
        Some(UbusAttr::Status) => "Status",
        Some(UbusAttr::ObjPath) => "ObjPath",
        Some(UbusAttr::ObjId) => "ObjId",
        Some(UbusAttr::Method) => "Method",
        Some(UbusAttr::ObjType) => "ObjType",
        Some(UbusAttr::Signature) => "Signature",
        Some(UbusAttr::Data) => "Data",
        Some(UbusAttr::Target) => "Target",
        Some(UbusAttr::Active) => "Active",
        Some(UbusAttr::NoReply) => "NoReply",
        Some(UbusAttr::Subscribers) => "Subscribers",
        Some(UbusAttr::User) => "User",
        Some(UbusAttr::Group) => "Group",
        None => "Unknown",
    }
}

fn hex_context(bytes: &[u8]) -> String {
    let n = bytes.len().min(64);
    let mut s = hex(&bytes[..n]);
    if bytes.len() > n {
        s.push_str("...");
    }
    s
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
    use ubus::blobmsg::BlobIter;
    use ubus::envelope::{iter_attrs, unwrap, EnvBuilder, MonitorAttr};
    use ubus::wire::{Header, MsgType, PROTO_VERSION};

    fn frame(ty: MsgType, seq: u16, peer: u32, payload: Vec<u8>) -> Frame {
        Frame {
            hdr: Header {
                version: PROTO_VERSION,
                ty,
                seq,
                peer,
            },
            payload,
        }
    }

    fn hello(peer: u32) -> Frame {
        frame(MsgType::Hello, 0, peer, EnvBuilder::new().finish())
    }

    fn status(seq: u16, peer: u32, value: i32) -> Frame {
        let mut env = EnvBuilder::new();
        env.push_i32(UbusAttr::Status, value);
        frame(MsgType::Status, seq, peer, env.finish())
    }

    fn data(seq: u16, peer: u32, obj_id: u32, obj_type: u32) -> Frame {
        let mut env = EnvBuilder::new();
        env.push_u32(UbusAttr::ObjId, obj_id);
        env.push_str(UbusAttr::ObjPath, "svc");
        env.push_u32(UbusAttr::ObjType, obj_type);
        frame(MsgType::Data, seq, peer, env.finish())
    }

    fn notify_ack(seq: u16, publisher_obj: u32, subscribers: &[u32]) -> Frame {
        let mut env = EnvBuilder::new();
        env.push_u32(UbusAttr::ObjId, publisher_obj);
        env.push_container(UbusAttr::Subscribers, |inner| {
            for subscriber in subscribers {
                inner.push_u32_raw(0, *subscriber);
            }
        });
        env.push_i32(UbusAttr::Status, 0);
        frame(MsgType::Status, seq, publisher_obj, env.finish())
    }

    fn add_object(seq: u16) -> Frame {
        let mut env = EnvBuilder::new();
        env.push_str(UbusAttr::ObjPath, "svc");
        frame(MsgType::AddObject, seq, 0, env.finish())
    }

    fn invoke(seq: u16, obj_id: u32) -> Frame {
        let mut env = EnvBuilder::new();
        env.push_u32(UbusAttr::ObjId, obj_id);
        env.push_str(UbusAttr::Method, "ping");
        frame(MsgType::Invoke, seq, obj_id, env.finish())
    }

    fn monitor(client: u32, peer: u32, seq: u32, ty: u32, send: u8, data: &[u8]) -> Frame {
        let mut env = EnvBuilder::new();
        env.push_u32_id(MonitorAttr::Client as u8, client);
        env.push_u32_id(MonitorAttr::Peer as u8, peer);
        env.push_u32_id(MonitorAttr::Seq as u8, seq);
        env.push_u32_id(MonitorAttr::Type as u8, ty);
        env.push_u8_id(MonitorAttr::Send as u8, send);
        env.push_bytes_id(MonitorAttr::Data as u8, data);
        frame(MsgType::Monitor, 0, 0, env.finish())
    }

    fn monitor_attr(frame: &Frame, id: MonitorAttr) -> &[u8] {
        iter_attrs(unwrap(&frame.payload).unwrap())
            .find(|attr| attr.id == id as u8)
            .unwrap()
            .payload
    }

    fn events(frames: &[(TapDirection, Frame)]) -> Vec<TapEvent> {
        let mut events = Vec::new();
        for (dir, frame) in frames {
            let bytes = Codec::write_frame(frame);
            let split1 = bytes.len().min(3);
            let split2 = bytes.len().min(11);
            for chunk in [&bytes[..split1], &bytes[split1..split2], &bytes[split2..]] {
                if !chunk.is_empty() {
                    events.push(TapEvent {
                        conn: 0,
                        dir: *dir,
                        kind: TapEventKind::Bytes(chunk.to_vec()),
                    });
                }
            }
        }
        events
    }

    fn env_obj_id(frame: &Frame) -> u32 {
        let mut payload = frame.payload.clone();
        let mut found = None;
        visit_id_attrs(
            StreamKey::new(0, TapDirection::BrokerToClient),
            0,
            frame.hdr.ty,
            &mut payload,
            |raw, _| {
                if found.is_none() {
                    found = Some(raw);
                }
            },
        )
        .unwrap();
        found.unwrap()
    }

    fn first_subscriber_id(frame: &Frame) -> u32 {
        let subscribers =
            ubus::envelope::find(&frame.payload, UbusAttr::Subscribers).expect("Subscribers attr");
        let nested = ubus::blob::BlobAttr::parse(subscribers.payload)
            .expect("nested subscriber attr")
            .0;
        u32::from_be_bytes([
            nested.payload[0],
            nested.payload[1],
            nested.payload[2],
            nested.payload[3],
        ])
    }

    #[test]
    fn monitor_ids_canonicalize_without_rewriting_literals() {
        let inner = data(7, 4, 4, 5).payload;
        let transcript = canonicalize(&events(&[(
            TapDirection::BrokerToClient,
            monitor(1025, 4, 17, MsgType::Data as u32, 1, &inner[4..]),
        )]))
        .unwrap();
        let frame = &transcript.frames(0, TapDirection::BrokerToClient)[0];

        assert_eq!(monitor_attr(frame, MonitorAttr::Client), 1u32.to_be_bytes());
        assert_eq!(monitor_attr(frame, MonitorAttr::Peer), 2u32.to_be_bytes());
        assert_eq!(monitor_attr(frame, MonitorAttr::Seq), 17u32.to_be_bytes());
        assert_eq!(
            monitor_attr(frame, MonitorAttr::Type),
            (MsgType::Data as u32).to_be_bytes()
        );
        assert_eq!(monitor_attr(frame, MonitorAttr::Send), &[1]);

        let mirrored = monitor_attr(frame, MonitorAttr::Data);
        let mut wrapped = vec![0; 4];
        wrapped.extend_from_slice(mirrored);
        let wrapped_len = wrapped.len() as u32;
        wrapped[..4].copy_from_slice(&wrapped_len.to_be_bytes());
        assert_eq!(
            ubus::envelope::find(&wrapped, UbusAttr::ObjId).and_then(|attr| attr.as_u32()),
            Some(2)
        );
        assert_eq!(
            ubus::envelope::find(&wrapped, UbusAttr::ObjType).and_then(|attr| attr.as_u32()),
            Some(3)
        );
    }

    #[test]
    fn monitor_seq_collision_stays_literal() {
        let transcript = canonicalize(&events(&[(
            TapDirection::BrokerToClient,
            monitor(0x77aa_0001, 0, 0x77aa_0001, MsgType::Ping as u32, 0, &[]),
        )]))
        .unwrap();
        let frame = &transcript.frames(0, TapDirection::BrokerToClient)[0];
        assert_eq!(monitor_attr(frame, MonitorAttr::Client), 1u32.to_be_bytes());
        assert_eq!(
            monitor_attr(frame, MonitorAttr::Seq),
            0x77aa_0001u32.to_be_bytes()
        );
    }

    #[test]
    fn monitor_data_applies_register_blobmsg_gate() {
        let data = ubus::blobmsg::BlobMsgTable::build(|b| {
            b.put_i32(Some("object"), 4);
        });
        let mut inner = EnvBuilder::new();
        inner.push_u32(UbusAttr::ObjId, EVENT_OBJECT_ID);
        inner.push_str(UbusAttr::Method, "register");
        inner.push_data(data.contents());
        let inner = inner.finish();
        let transcript = canonicalize(&events(&[(
            TapDirection::BrokerToClient,
            monitor(1025, 0, 1, MsgType::Invoke as u32, 0, &inner[4..]),
        )]))
        .unwrap();
        let frame = &transcript.frames(0, TapDirection::BrokerToClient)[0];
        let mirrored = monitor_attr(frame, MonitorAttr::Data);
        let data = iter_attrs(mirrored)
            .find(|attr| attr.id == UbusAttr::Data as u8)
            .unwrap()
            .payload;
        let object = BlobIter::new(data).next().unwrap().as_i32().unwrap();
        assert_eq!(object, 2);
    }

    #[test]
    fn id_map_reserves_monitor_object_id() {
        let mut ids = IdMap::default();
        ids.observe(MONITOR_OBJECT_ID);
        ids.observe(4);
        assert_eq!(ids.map_existing(MONITOR_OBJECT_ID), MONITOR_OBJECT_CANON);
        assert_eq!(ids.map_existing(4), 1);
    }

    #[test]
    fn id_map_reserves_acl_object_id() {
        let mut ids = IdMap::default();
        ids.observe(ACL_OBJECT_ID);
        ids.observe(4);
        assert_eq!(ids.map_existing(ACL_OBJECT_ID), ACL_OBJECT_CANON);
        assert_eq!(ids.map_existing(4), 1);
    }

    #[test]
    fn different_raw_ids_canonicalize_equal() {
        let upstream = events(&[
            (TapDirection::BrokerToClient, hello(0x8000_0042)),
            (TapDirection::ClientToBroker, add_object(1)),
            (
                TapDirection::BrokerToClient,
                data(1, 0x8000_0042, 0x77aa_0001, 0x77aa_0002),
            ),
            (TapDirection::BrokerToClient, status(1, 0x8000_0042, 0)),
        ]);
        // rs peers start at 1025 (never 1, which is the reserved event
        // object id the canonicalizer holds fixed).
        let rs = events(&[
            (TapDirection::BrokerToClient, hello(1025)),
            (TapDirection::ClientToBroker, add_object(1)),
            (TapDirection::BrokerToClient, data(1, 1025, 4, 5)),
            (TapDirection::BrokerToClient, status(1, 1025, 0)),
        ]);

        let upstream = canonicalize(&upstream).unwrap();
        let rs = canonicalize(&rs).unwrap();
        assert_eq!(diff(&upstream, &rs), None);
    }

    #[test]
    fn client_echoed_id_maps_through_shared_bijection() {
        let transcript = canonicalize(&events(&[
            (TapDirection::BrokerToClient, hello(0x8000_0042)),
            (
                TapDirection::BrokerToClient,
                data(1, 0x8000_0042, 0x77aa_0001, 0x77aa_0002),
            ),
            (TapDirection::ClientToBroker, invoke(2, 0x77aa_0001)),
        ]))
        .unwrap();

        let b2c = transcript.frames(0, TapDirection::BrokerToClient);
        let c2b = transcript.frames(0, TapDirection::ClientToBroker);
        assert_eq!(b2c[1].payload, data(1, 1, 2, 3).payload);
        assert_eq!(c2b[0].hdr.peer, 2);
        assert_eq!(env_obj_id(&c2b[0]), 2);
    }

    #[test]
    fn subscribers_container_ids_canonicalize_equal() {
        let upstream = canonicalize(&events(&[
            (TapDirection::BrokerToClient, hello(0x8000_0042)),
            (
                TapDirection::BrokerToClient,
                notify_ack(7, 0x77aa_0001, &[0x77aa_0002]),
            ),
        ]))
        .unwrap();
        let rs = canonicalize(&events(&[
            (TapDirection::BrokerToClient, hello(1025)),
            (TapDirection::BrokerToClient, notify_ack(7, 4, &[5])),
        ]))
        .unwrap();

        assert_eq!(diff(&upstream, &rs), None);
    }

    #[test]
    fn subscriber_nested_id_reuses_existing_obj_mapping() {
        let transcript = canonicalize(&events(&[
            (TapDirection::BrokerToClient, hello(0x8000_0042)),
            (
                TapDirection::BrokerToClient,
                data(1, 0x8000_0042, 0x77aa_0001, 0x77aa_0002),
            ),
            (
                TapDirection::BrokerToClient,
                notify_ack(7, 0x77aa_0002, &[0x77aa_0001]),
            ),
        ]))
        .unwrap();

        let b2c = transcript.frames(0, TapDirection::BrokerToClient);
        assert_eq!(env_obj_id(&b2c[1]), 2);
        assert_eq!(first_subscriber_id(&b2c[2]), 2);
    }

    #[test]
    fn target_attr_maps_through_shared_bijection() {
        // SUBSCRIBE's Target carries a broker-minted object id; two
        // transcripts with different raw target ids (upstream random vs
        // rs sequential) must canonicalize equal.
        fn subscribe(sub_obj: u32, target: u32) -> Frame {
            let mut env = EnvBuilder::new();
            env.push_u32(UbusAttr::ObjId, sub_obj);
            env.push_u32(UbusAttr::Target, target);
            frame(MsgType::Subscribe, 9, target, env.finish())
        }
        let upstream = canonicalize(&events(&[
            (
                TapDirection::BrokerToClient,
                data(1, 0x8000_0042, 0x77aa_0001, 0x77aa_0002),
            ),
            (
                TapDirection::ClientToBroker,
                subscribe(0x77aa_0001, 0x94a7_5be8),
            ),
        ]))
        .unwrap();
        let rs = canonicalize(&events(&[
            (TapDirection::BrokerToClient, data(1, 1025, 4, 5)),
            (TapDirection::ClientToBroker, subscribe(4, 6)),
        ]))
        .unwrap();
        assert_eq!(diff(&upstream, &rs), None);
    }

    #[test]
    fn divergent_status_yields_rendered_diff() {
        let ok = canonicalize(&events(&[
            (TapDirection::BrokerToClient, hello(1)),
            (TapDirection::BrokerToClient, status(1, 1, 0)),
        ]))
        .unwrap();
        let bad = canonicalize(&events(&[
            (TapDirection::BrokerToClient, hello(1)),
            (TapDirection::BrokerToClient, status(1, 1, -3)),
        ]))
        .unwrap();

        let rendered = diff(&ok, &bad).unwrap();
        assert!(rendered.contains("conn=0"));
        assert!(rendered.contains("dir=BrokerToClient"));
        assert!(rendered.contains("first mismatching frame index 1"));
        assert!(rendered.contains(&hex(&status(1, 1, 0).payload)));
        assert!(rendered.contains(&hex(&status(1, 1, -3).payload)));
    }

    #[test]
    fn attr_order_divergence_is_not_masked() {
        let mut a_env = EnvBuilder::new();
        a_env.push_u32(UbusAttr::ObjId, 42);
        a_env.push_str(UbusAttr::Method, "ping");
        let mut b_env = EnvBuilder::new();
        b_env.push_str(UbusAttr::Method, "ping");
        b_env.push_u32(UbusAttr::ObjId, 42);

        let a = canonicalize(&events(&[(
            TapDirection::ClientToBroker,
            frame(MsgType::Invoke, 1, 42, a_env.finish()),
        )]))
        .unwrap();
        let b = canonicalize(&events(&[(
            TapDirection::ClientToBroker,
            frame(MsgType::Invoke, 1, 42, b_env.finish()),
        )]))
        .unwrap();

        assert!(diff(&a, &b).is_some());
    }

    #[test]
    fn truncated_stream_is_a_canon_error() {
        let mut bytes = Codec::write_frame(&hello(1));
        bytes.pop();
        let err = canonicalize(&[TapEvent {
            conn: 0,
            dir: TapDirection::BrokerToClient,
            kind: TapEventKind::Bytes(bytes),
        }])
        .unwrap_err();
        assert!(matches!(err, CanonError::Decode { .. }));
    }

    #[test]
    fn eof_events_do_not_disturb_frame_decoding() {
        let mut events = events(&[
            (TapDirection::BrokerToClient, hello(9)),
            (TapDirection::ClientToBroker, add_object(4)),
        ]);
        events.insert(
            1,
            TapEvent {
                conn: 0,
                dir: TapDirection::BrokerToClient,
                kind: TapEventKind::Eof,
            },
        );

        let transcript = canonicalize(&events).unwrap();
        assert_eq!(
            transcript.frames(0, TapDirection::BrokerToClient),
            &[hello(1)]
        );
        assert_eq!(
            transcript.frames(0, TapDirection::ClientToBroker),
            &[add_object(4)]
        );
        assert!(
            transcript
                .events()
                .iter()
                .any(|ev| matches!(ev.kind, CanonEventKind::Eof)),
            "EOF marker should be preserved in canonical event stream"
        );
    }

    #[test]
    fn zero_peer_never_mapped() {
        let transcript =
            canonicalize(&events(&[(TapDirection::BrokerToClient, hello(0))])).unwrap();
        assert_eq!(
            transcript.frames(0, TapDirection::BrokerToClient)[0]
                .hdr
                .peer,
            0
        );
    }

    /// A broker-minted `ubus.object.add` fan-out carries the changed
    /// object's id twice: the envelope ObjId (the listener's own object)
    /// and an `id` INT32 inside the `Data` blobmsg. Both must map through
    /// the shared bijection so upstream's random ids and rs's small
    /// sequential ids canonicalize identically.
    fn object_add_event(listener_obj: u32, added_obj: u32) -> Frame {
        let data = ubus::blobmsg::BlobMsgTable::build(|b| {
            b.put_i32(Some("id"), added_obj as i32);
            b.put_str(Some("path"), "example.svc");
        });
        let mut env = EnvBuilder::new();
        env.push_u32(UbusAttr::ObjId, listener_obj);
        env.push_str(UbusAttr::Method, "ubus.object.add");
        env.push_data(data.contents());
        frame(MsgType::Invoke, 3, 0, env.finish())
    }

    #[test]
    fn object_event_payload_id_canonicalizes_equal() {
        let upstream = canonicalize(&events(&[
            (TapDirection::BrokerToClient, hello(0x8000_0042)),
            (
                TapDirection::BrokerToClient,
                data(1, 0x8000_0042, 0x77aa_0001, 0x77aa_0002),
            ),
            (
                TapDirection::BrokerToClient,
                object_add_event(0x77aa_0001, 0x77aa_0003),
            ),
        ]))
        .unwrap();
        let rs = canonicalize(&events(&[
            (TapDirection::BrokerToClient, hello(1025)),
            (TapDirection::BrokerToClient, data(1, 1025, 4, 5)),
            (TapDirection::BrokerToClient, object_add_event(4, 6)),
        ]))
        .unwrap();
        assert_eq!(diff(&upstream, &rs), None);
    }

    #[test]
    fn non_object_event_payload_id_is_not_canonicalized() {
        // A user `ubus send` event whose payload happens to have an `id`
        // field: that id is opaque user data, not a broker object id, so
        // it must stay byte-literal. Two transcripts differing only in
        // that value must therefore NOT canonicalize equal — proving the
        // mapping is gated on the event name, not the field name.
        fn user_event(listener_obj: u32, id_val: i32) -> Frame {
            let data = ubus::blobmsg::BlobMsgTable::build(|b| {
                b.put_i32(Some("id"), id_val);
            });
            let mut env = EnvBuilder::new();
            env.push_u32(UbusAttr::ObjId, listener_obj);
            env.push_str(UbusAttr::Method, "example.probe.event");
            env.push_data(data.contents());
            frame(MsgType::Invoke, 3, 0, env.finish())
        }
        let a = canonicalize(&events(&[
            (TapDirection::BrokerToClient, hello(1)),
            (TapDirection::BrokerToClient, user_event(1, 5)),
        ]))
        .unwrap();
        let b = canonicalize(&events(&[
            (TapDirection::BrokerToClient, hello(1)),
            (TapDirection::BrokerToClient, user_event(1, 6)),
        ]))
        .unwrap();
        assert!(
            diff(&a, &b).is_some(),
            "user-event payload id must not be canonicalized"
        );
    }

    #[test]
    fn register_object_field_canonicalizes_equal() {
        // A `ubus listen` register INVOKE echoes the listener's own
        // broker-minted object id in an `object` INT32 field. The event
        // object id (1) it addresses is distinct from any real peer/obj
        // id on both brokers, so the rs hello peer must not be 1 here.
        fn register(listener_obj: u32) -> Frame {
            let data = ubus::blobmsg::BlobMsgTable::build(|b| {
                b.put_i32(Some("object"), listener_obj as i32);
                b.put_str(Some("pattern"), "*");
            });
            let mut env = EnvBuilder::new();
            env.push_u32(UbusAttr::ObjId, 1);
            env.push_str(UbusAttr::Method, "register");
            env.push_data(data.contents());
            frame(MsgType::Invoke, 5, 1, env.finish())
        }
        let upstream = canonicalize(&events(&[
            (TapDirection::BrokerToClient, hello(0x8000_0042)),
            (
                TapDirection::BrokerToClient,
                data(1, 0x8000_0042, 0x77aa_0001, 0x77aa_0002),
            ),
            (TapDirection::ClientToBroker, register(0x77aa_0001)),
        ]))
        .unwrap();
        let rs = canonicalize(&events(&[
            (TapDirection::BrokerToClient, hello(1025)),
            (TapDirection::BrokerToClient, data(1, 1025, 4, 5)),
            (TapDirection::ClientToBroker, register(4)),
        ]))
        .unwrap();
        assert_eq!(diff(&upstream, &rs), None);
    }
}
