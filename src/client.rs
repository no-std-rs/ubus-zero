//! UBUS client. [`UbusClient::connect`] performs the HELLO handshake
//! at construction; the resulting handle is ready for `lookup` /
//! `invoke`. The `Data` reply attr's outer TABLE wrapper is stripped
//! before returning so callers can `lookup(reply, "key")` directly.

use alloc::{collections::VecDeque, string::String, vec::Vec};

use crate::blobmsg::{BlobIter, BlobMsgTable};
use crate::envelope::{try_find, EnvBuilder, UbusAttr};
use crate::error::UbusError;
use crate::msg::{InboundMsg, OutboundMsg};
use crate::transport::Transport;
use crate::wire::{Frame, MsgType, ObjId, HELLO_NON_HELLO_BUDGET, IDLE_WAIT_BUDGET, MAX_MSG_LEN};

const MAX_PENDING_INVOKES: usize = 256;
// DATA + STATUS for each outstanding invocation, plus the original
// background-message headroom. The independent byte cap still applies.
const MAX_PENDING_MESSAGES: usize = 2 * MAX_PENDING_INVOKES + 256;
const MAX_PENDING_BYTES: usize = MAX_MSG_LEN * 8;

/// An object returned by a broker lookup, including its method signature.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ObjectInfo {
    pub id: u32,
    pub path: String,
    /// Blobmsg table contents describing methods and their argument policies.
    pub signature: Vec<u8>,
}

/// Run the HELLO handshake using `seq` as the outbound HELLO's
/// sequence id; returns the broker-assigned peer id. Accepts the first
/// HELLO regardless of seq (upstream `ubusd` pushes one unsolicited at
/// accept time with `seq=0`).
pub(crate) fn do_hello_handshake<T: Transport>(
    transport: &mut T,
    seq: u16,
) -> Result<u32, UbusError> {
    transport.send(&OutboundMsg::Hello.into_frame(seq))?;
    let mut non_hello = 0u32;
    let mut idle_waits = 0u32;
    loop {
        let frame = match transport.recv()? {
            Some(f) => f,
            None => {
                idle_waits += 1;
                if idle_waits > IDLE_WAIT_BUDGET {
                    return Err(UbusError::Timeout("hello"));
                }
                transport.wait_recv();
                continue;
            }
        };
        if let InboundMsg::Hello { peer, .. } = InboundMsg::parse(&frame)? {
            if peer == 0 {
                return Err(UbusError::Malformed(
                    "HELLO reply assigned local_id=0 (broker convention)",
                ));
            }
            return Ok(peer);
        }
        non_hello += 1;
        if non_hello > HELLO_NON_HELLO_BUDGET {
            return Err(UbusError::Malformed(
                "broker exceeded non-HELLO budget before HELLO",
            ));
        }
    }
}

pub struct UbusClient<T: Transport> {
    transport: T,
    seq: u16,
    /// `None` only on test-seam constructions (`without_hello`); a
    /// real handshake stores `Some(non_zero)`. Avoids the silent
    /// "0 means unknown" overload that production rejects on HELLO.
    peer_id: Option<u32>,
    queued: VecDeque<Frame>,
    queued_bytes: usize,
    pending_invokes: Vec<(u16, u32)>,
}

impl<T: Transport> UbusClient<T> {
    /// Performs HELLO via [`do_hello_handshake`]; returns
    /// `Err(Closed)` if the broker hangs up first or
    /// `Err(Timeout("hello"))` if it stays silent.
    pub fn connect(transport: T) -> Result<Self, UbusError> {
        let mut s = Self {
            transport,
            seq: 1,
            peer_id: None,
            queued: VecDeque::new(),
            queued_bytes: 0,
            pending_invokes: Vec::new(),
        };
        let seq = s.next_seq();
        s.peer_id = Some(do_hello_handshake(&mut s.transport, seq)?);
        Ok(s)
    }

    pub fn peer_id(&self) -> Option<u32> {
        self.peer_id
    }

    /// Low-level transport access. Raw reads bypass invocation tracking; after
    /// consuming a split-phase reply this way, call [`Self::forget_invoke`].
    pub fn transport_mut(&mut self) -> &mut T {
        &mut self.transport
    }

    /// Perform a broker request, delivering every DATA reply to the callback.
    /// Asynchronous messages arriving during the request are retained for
    /// [`Self::recv_frame`]. Accepted DATA resets the idle-wait budget; unrelated
    /// traffic does not. The shared pending queue holds at most 768 frames
    /// and 8 MiB of payload. Transports supply the real-time deadline.
    pub fn request(
        &mut self,
        ty: MsgType,
        peer: u32,
        envelope: EnvBuilder,
        on_data: impl FnMut(&Frame) -> Result<(), UbusError>,
    ) -> Result<(), UbusError> {
        self.request_with_phase(ty, peer, envelope, "request", on_data)
    }

    fn request_with_phase(
        &mut self,
        ty: MsgType,
        peer: u32,
        envelope: EnvBuilder,
        phase: &'static str,
        mut on_data: impl FnMut(&Frame) -> Result<(), UbusError>,
    ) -> Result<(), UbusError> {
        let seq = self.next_seq();
        self.transport
            .send(&Frame::to_peer(ty, seq, peer, envelope.finish()))?;
        let mut idle_waits = 0;
        loop {
            let Some(frame) = self.recv_matching(seq, peer)? else {
                idle_waits += 1;
                if idle_waits > IDLE_WAIT_BUDGET {
                    return Err(UbusError::Timeout(phase));
                }
                self.transport.wait_recv();
                continue;
            };
            if frame.hdr.ty == MsgType::Data {
                on_data(&frame)?;
                idle_waits = 0;
            } else {
                let status = crate::envelope::read_status(&frame.payload)?;
                return if status == 0 {
                    Ok(())
                } else {
                    Err(UbusError::Status(status))
                };
            }
        }
    }

    pub fn recv_frame(&mut self) -> Result<Option<Frame>, UbusError> {
        let frame = if let Some(frame) = self.queued.pop_front() {
            self.queued_bytes -= frame.payload.len();
            Some(frame)
        } else {
            self.transport.recv()?
        };
        if let Some(frame) = &frame {
            if frame.hdr.ty == MsgType::Status {
                self.finish_invoke(frame.hdr.seq, frame.hdr.peer);
            }
        }
        Ok(frame)
    }

    fn is_reply(frame: &Frame, seq: u16, peer: u32) -> bool {
        frame.hdr.seq == seq
            && frame.hdr.peer == peer
            && matches!(frame.hdr.ty, MsgType::Status | MsgType::Data)
    }

    fn recv_matching(&mut self, seq: u16, peer: u32) -> Result<Option<Frame>, UbusError> {
        if let Some(index) = self
            .queued
            .iter()
            .position(|f| Self::is_reply(f, seq, peer))
        {
            let frame = self.queued.remove(index).expect("position exists");
            self.queued_bytes -= frame.payload.len();
            return Ok(Some(frame));
        }
        while let Some(frame) = self.transport.recv()? {
            if Self::is_reply(&frame, seq, peer) {
                return Ok(Some(frame));
            }
            if self.queued.len() >= MAX_PENDING_MESSAGES
                || self.queued_bytes.saturating_add(frame.payload.len()) > MAX_PENDING_BYTES
            {
                return Err(UbusError::Malformed("pending message queue exceeded cap"));
            }
            self.queued_bytes += frame.payload.len();
            self.queued.push_back(frame);
        }
        Ok(None)
    }

    fn finish_invoke(&mut self, seq: u16, peer: u32) {
        if let Some(index) = self
            .pending_invokes
            .iter()
            .position(|entry| *entry == (seq, peer))
        {
            self.pending_invokes.swap_remove(index);
        }
    }

    /// List all objects (`None`) or those matching an ubus lookup pattern.
    pub fn lookup_objects(&mut self, pattern: Option<&str>) -> Result<Vec<ObjectInfo>, UbusError> {
        let mut envelope = EnvBuilder::new();
        if let Some(pattern) = pattern {
            envelope.push_str(UbusAttr::ObjPath, pattern);
        }
        let mut objects = Vec::new();
        self.request(MsgType::Lookup, 0, envelope, |frame| {
            let id = try_find(&frame.payload, UbusAttr::ObjId)?
                .and_then(|a| a.as_u32())
                .ok_or(UbusError::Malformed("lookup reply missing object id"))?;
            let path = try_find(&frame.payload, UbusAttr::ObjPath)?
                .and_then(|a| a.as_str())
                .ok_or(UbusError::Malformed("lookup reply missing object path"))?;
            let signature = try_find(&frame.payload, UbusAttr::Signature)?
                .map(|a| a.payload.to_vec())
                .unwrap_or_default();
            objects.push(ObjectInfo {
                id,
                path: path.into(),
                signature,
            });
            Ok(())
        })?;
        Ok(objects)
    }

    /// Return every reply, rather than just the final DATA message.
    pub fn invoke_many(
        &mut self,
        object: u32,
        method: &str,
        args: &BlobMsgTable,
    ) -> Result<Vec<Vec<u8>>, UbusError> {
        ObjId::new(object).ok_or(UbusError::Malformed("invoke target obj_id is zero"))?;
        let mut envelope = EnvBuilder::new();
        envelope.push_u32(UbusAttr::ObjId, object);
        envelope.push_str(UbusAttr::Method, method);
        envelope.push_data(args.contents());
        let mut replies = Vec::new();
        self.request(MsgType::Invoke, object, envelope, |frame| {
            let data = try_find(&frame.payload, UbusAttr::Data)?
                .ok_or(UbusError::Malformed("invoke reply missing data"))?;
            let mut iter = BlobIter::new(data.payload);
            while iter.try_next()?.is_some() {}
            let size: usize = replies.iter().map(Vec::len).sum();
            if replies.len() >= 256 || size + data.payload.len() > MAX_MSG_LEN * 8 {
                return Err(UbusError::Malformed("invoke replies exceeded cap"));
            }
            replies.push(data.payload.to_vec());
            Ok(())
        })?;
        Ok(replies)
    }

    pub fn ping(&mut self) -> Result<(), UbusError> {
        self.request(MsgType::Ping, 0, EnvBuilder::new(), |_| Ok(()))
    }

    /// Create the anonymous object required by event listeners/subscribers.
    pub fn add_anonymous_object(&mut self) -> Result<u32, UbusError> {
        let mut object = None;
        self.request(MsgType::AddObject, 0, EnvBuilder::new(), |frame| {
            object = try_find(&frame.payload, UbusAttr::ObjId)?.and_then(|a| a.as_u32());
            Ok(())
        })?;
        object
            .filter(|id| *id != 0)
            .ok_or(UbusError::Malformed("add_object reply missing id"))
    }

    pub fn register_event(&mut self, object: u32, pattern: &str) -> Result<(), UbusError> {
        let args = BlobMsgTable::build(|b| {
            b.put_i32(Some("object"), object as i32);
            b.put_str(Some("pattern"), pattern);
        });
        self.invoke_many(1, "register", &args).map(|_| ())
    }

    pub fn send_event(&mut self, name: &str, data: &BlobMsgTable) -> Result<(), UbusError> {
        let args = BlobMsgTable::build(|b| {
            b.put_str(Some("id"), name);
            b.put_table(Some("data"), data);
        });
        self.invoke_many(1, "send", &args).map(|_| ())
    }

    pub fn subscribe(&mut self, subscriber: u32, target: u32) -> Result<(), UbusError> {
        self.subscription(MsgType::Subscribe, subscriber, target)
    }

    pub fn unsubscribe(&mut self, subscriber: u32, target: u32) -> Result<(), UbusError> {
        self.subscription(MsgType::Unsubscribe, subscriber, target)
    }

    fn subscription(&mut self, ty: MsgType, subscriber: u32, target: u32) -> Result<(), UbusError> {
        let mut envelope = EnvBuilder::new();
        envelope.push_u32(UbusAttr::ObjId, subscriber);
        envelope.push_u32(UbusAttr::Target, target);
        self.request(ty, 0, envelope, |_| Ok(()))
    }

    pub fn monitor(&mut self, enabled: bool) -> Result<(), UbusError> {
        self.invoke_many(
            3,
            if enabled { "add" } else { "remove" },
            &BlobMsgTable::empty(),
        )
        .map(|_| ())
    }

    /// Acknowledge an asynchronous INVOKE or NOTIFY delivered to this client.
    pub fn acknowledge(&mut self, frame: &Frame, status: i32) -> Result<(), UbusError> {
        let mut envelope = EnvBuilder::new();
        if let Some(id) = try_find(&frame.payload, UbusAttr::ObjId)?.and_then(|a| a.as_u32()) {
            envelope.push_u32(UbusAttr::ObjId, id);
        }
        envelope.push_i32(UbusAttr::Status, status);
        self.transport.send(&Frame::to_peer(
            MsgType::Status,
            frame.hdr.seq,
            frame.hdr.peer,
            envelope.finish(),
        ))
    }

    pub fn lookup(&mut self, object: &str) -> Result<u32, UbusError> {
        let mut found_id = None;
        let mut data_frame_was_corrupt = false;
        let mut envelope = EnvBuilder::new();
        envelope.push_str(UbusAttr::ObjPath, object);
        self.request_with_phase(MsgType::Lookup, 0, envelope, "lookup", |frame| {
            match InboundMsg::parse(frame)? {
                InboundMsg::Data {
                    obj_id: Some(id), ..
                } => found_id = Some(id),
                InboundMsg::Data { obj_id: None, .. } => data_frame_was_corrupt = true,
                _ => {}
            }
            Ok(())
        })?;
        found_id.ok_or(UbusError::Malformed(if data_frame_was_corrupt {
            "LOOKUP DATA frame missing ObjId attr"
        } else {
            "LOOKUP succeeded without ObjId in DATA"
        }))
    }

    /// Start an invocation and return its sequence for [`Self::recv_invoke_reply`].
    /// At most 256 invocations may be outstanding. Receiving its STATUS through
    /// [`Self::recv_frame`] also retires it. Raw transport reads bypass tracking;
    /// use [`Self::forget_invoke`] after manually consuming those replies.
    pub fn send_invoke(
        &mut self,
        object_id: u32,
        method: &str,
        args: &BlobMsgTable,
    ) -> Result<u16, UbusError> {
        let obj_id =
            ObjId::new(object_id).ok_or(UbusError::Malformed("invoke target obj_id is zero"))?;
        if self.pending_invokes.len() >= MAX_PENDING_INVOKES {
            return Err(UbusError::Malformed("pending invocation limit exceeded"));
        }
        let seq = self.next_seq();
        self.transport.send(
            &OutboundMsg::Invoke {
                obj_id,
                method,
                args,
            }
            .into_frame(seq),
        )?;
        self.pending_invokes.push((seq, object_id));
        Ok(seq)
    }

    /// Release local tracking for a split-phase invocation. This does not
    /// cancel the remote operation or remove buffered replies. Use it after
    /// handling a reply through the raw transport, or to abandon a request.
    pub fn forget_invoke(&mut self, seq: u16) -> bool {
        if let Some(index) = self.pending_invokes.iter().position(|entry| entry.0 == seq) {
            self.pending_invokes.swap_remove(index);
            true
        } else {
            false
        }
    }

    /// On STATUS=0 returns the *contents* of the handler's reply
    /// TABLE (outer wrapper stripped). Returns `Err(Status)` for
    /// non-zero STATUS and `Err(Malformed)` if the broker claimed
    /// success but DATA was missing or unparseable. `seq` must come from
    /// [`Self::send_invoke`] on this client. Any result, including an error,
    /// retires that invocation; other messages remain buffered in arrival order.
    pub fn recv_invoke_reply(&mut self, seq: u16) -> Result<Vec<u8>, UbusError> {
        let peer = self
            .pending_invokes
            .iter()
            .find(|entry| entry.0 == seq)
            .map(|entry| entry.1)
            .ok_or(UbusError::Malformed("invoke sequence is not pending"))?;
        let result = self.wait_invoke_reply(seq, peer);
        self.finish_invoke(seq, peer);
        result
    }

    fn wait_invoke_reply(&mut self, seq: u16, peer: u32) -> Result<Vec<u8>, UbusError> {
        let mut data: Option<Vec<u8>> = None;
        let mut data_frame_was_corrupt = false;
        let mut idle_waits = 0u32;
        loop {
            let frame = match self.recv_matching(seq, peer)? {
                Some(f) => f,
                None => {
                    idle_waits += 1;
                    if idle_waits > IDLE_WAIT_BUDGET {
                        return Err(UbusError::Timeout("invoke"));
                    }
                    self.transport.wait_recv();
                    continue;
                }
            };
            match InboundMsg::parse(&frame)? {
                InboundMsg::Data {
                    data: Some(contents),
                    ..
                } => {
                    // libubox convention: UBUS_ATTR_DATA carries the
                    // contents of the reply blobmsg directly (i.e. the
                    // bytes a `blob_data(reply_msg)` call would yield),
                    // not a wrapped TABLE. Validate they parse as a
                    // sequence of attrs so a corrupted reply surfaces
                    // as `Malformed` rather than as silently-truncated
                    // empty bytes downstream.
                    let mut it = BlobIter::new(contents);
                    while it.try_next()?.is_some() {}
                    data = Some(contents.to_vec());
                    idle_waits = 0;
                }
                InboundMsg::Data { data: None, .. } => data_frame_was_corrupt = true,
                InboundMsg::Status { status, .. } => {
                    if status != 0 {
                        return Err(UbusError::Status(status));
                    }
                    if data_frame_was_corrupt {
                        return Err(UbusError::Malformed("DATA frame missing Data attr"));
                    }
                    return Ok(data.unwrap_or_default());
                }
                _ => continue,
            }
        }
    }

    pub fn invoke(
        &mut self,
        object_id: u32,
        method: &str,
        args: &BlobMsgTable,
    ) -> Result<Vec<u8>, UbusError> {
        let seq = self.send_invoke(object_id, method, args)?;
        self.recv_invoke_reply(seq)
    }

    fn next_seq(&mut self) -> u16 {
        // Both sets are bounded well below the 16-bit sequence space.
        loop {
            let seq = self.seq;
            self.seq = self.seq.wrapping_add(1);
            let busy = self.pending_invokes.iter().any(|entry| entry.0 == seq)
                || self.queued.iter().any(|frame| {
                    frame.hdr.seq == seq && matches!(frame.hdr.ty, MsgType::Data | MsgType::Status)
                });
            if !busy {
                return seq;
            }
        }
    }
}

impl<T: Transport> UbusClient<T> {
    /// Bypass the HELLO handshake. Use only from tests driving
    /// `MockTransport` directly with a paired server fixture. Kept
    /// public so integration tests can exercise client/server
    /// round-trips without a broker thread in between.
    #[doc(hidden)]
    pub fn without_hello(transport: T) -> Self {
        Self {
            transport,
            seq: 1,
            peer_id: None,
            queued: VecDeque::new(),
            queued_bytes: 0,
            pending_invokes: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::envelope::{EnvBuilder, UbusAttr};
    use crate::transport::MockTransport;
    use crate::wire::{Frame, MsgType};

    #[test]
    fn invoke_many_retains_every_reply_and_racing_events() {
        let (transport, mut peer) = MockTransport::pair();
        let mut client = UbusClient::without_hello(transport);
        let event = Frame::to_peer(MsgType::Invoke, 1, 0, EnvBuilder::new().finish());
        peer.send(&event).unwrap();
        for n in [1, 2] {
            let data = BlobMsgTable::build(|b| b.put_i32(Some("n"), n));
            let mut envelope = EnvBuilder::new();
            envelope.push_data(data.contents());
            peer.send(&Frame::to_peer(MsgType::Data, 1, 42, envelope.finish()))
                .unwrap();
        }
        let mut envelope = EnvBuilder::new();
        envelope.push_i32(UbusAttr::Status, 0);
        peer.send(&Frame::to_peer(MsgType::Status, 1, 42, envelope.finish()))
            .unwrap();
        let replies = client
            .invoke_many(42, "method", &BlobMsgTable::empty())
            .unwrap();
        assert_eq!(replies.len(), 2);
        assert_eq!(
            crate::blobmsg::lookup(&replies[0], "n").unwrap().as_i32(),
            Some(1)
        );
        assert_eq!(
            crate::blobmsg::lookup(&replies[1], "n").unwrap().as_i32(),
            Some(2)
        );
        assert_eq!(client.recv_frame().unwrap(), Some(event));
    }

    fn data_reply(seq: u16, peer: u32, value: i32) -> Frame {
        let data = BlobMsgTable::build(|b| b.put_i32(Some("value"), value));
        let mut env = EnvBuilder::new();
        env.push_data(data.contents());
        Frame::to_peer(MsgType::Data, seq, peer, env.finish())
    }

    fn status_reply(seq: u16, peer: u32, status: i32) -> Frame {
        let mut env = EnvBuilder::new();
        env.push_i32(UbusAttr::Status, status);
        Frame::to_peer(MsgType::Status, seq, peer, env.finish())
    }

    #[test]
    fn invoke_and_invoke_many_accept_the_same_policy_valid_status() {
        for invalid in [&[6][..], &[0, 0, 0, 6, 0]] {
            let (transport, mut peer) = MockTransport::pair();
            let mut client = UbusClient::without_hello(transport);
            let mut envelope = EnvBuilder::new();
            envelope.push_i32(UbusAttr::Status, 0);
            envelope.push_bytes_id(UbusAttr::Status as u8, invalid);
            let payload = envelope.finish();
            peer.send(&Frame::to_peer(MsgType::Status, 1, 42, payload.clone()))
                .unwrap();
            assert_eq!(
                client.invoke(42, "method", &BlobMsgTable::empty()).unwrap(),
                Vec::<u8>::new()
            );
            assert!(!client.forget_invoke(1));
            peer.send(&Frame::to_peer(MsgType::Status, 2, 42, payload))
                .unwrap();
            assert!(client
                .invoke_many(42, "method", &BlobMsgTable::empty())
                .unwrap()
                .is_empty());
        }
    }

    #[test]
    fn request_does_not_accept_a_matching_sequence_from_another_object() {
        let (transport, mut peer) = MockTransport::pair();
        let mut client = UbusClient::without_hello(transport);
        let unrelated = status_reply(1, 99, 6);
        peer.send(&unrelated).unwrap();
        peer.send(&data_reply(1, 42, 7)).unwrap();
        peer.send(&status_reply(1, 42, 0)).unwrap();
        let replies = client
            .invoke_many(42, "method", &BlobMsgTable::empty())
            .unwrap();
        assert_eq!(replies.len(), 1);
        assert_eq!(client.recv_frame().unwrap(), Some(unrelated));
    }

    #[test]
    fn lookup_preserves_an_event_with_the_same_sequence() {
        let (transport, mut peer) = MockTransport::pair();
        let mut client = UbusClient::without_hello(transport);
        let event = OutboundMsg::Invoke {
            obj_id: ObjId::new(77).unwrap(),
            method: "event",
            args: &BlobMsgTable::empty(),
        }
        .into_frame(1);
        peer.send(&event).unwrap();
        let mut env = EnvBuilder::new();
        env.push_u32(UbusAttr::ObjId, 42);
        peer.send(&Frame::to_broker(MsgType::Data, 1, env.finish()))
            .unwrap();
        peer.send(&status_reply(1, 0, 0)).unwrap();
        assert_eq!(client.lookup("example").unwrap(), 42);
        assert_eq!(client.recv_frame().unwrap(), Some(event));
    }

    #[test]
    fn invoke_replies_can_be_received_in_reverse_order() {
        let (transport, mut peer) = MockTransport::pair();
        let mut client = UbusClient::without_hello(transport);
        let first = client
            .send_invoke(42, "method", &BlobMsgTable::empty())
            .unwrap();
        let second = client
            .send_invoke(43, "method", &BlobMsgTable::empty())
            .unwrap();
        for (seq, object, value) in [(first, 42, 1), (second, 43, 2)] {
            peer.send(&data_reply(seq, object, value)).unwrap();
            peer.send(&status_reply(seq, object, 0)).unwrap();
        }
        for (seq, expected) in [(second, 2), (first, 1)] {
            let bytes = client.recv_invoke_reply(seq).unwrap();
            assert_eq!(
                crate::blobmsg::lookup(&bytes, "value").unwrap().as_i32(),
                Some(expected)
            );
        }
    }

    #[test]
    fn invocation_target_is_checked_and_raw_status_retires_only_its_request() {
        let (transport, mut peer) = MockTransport::pair();
        let mut client = UbusClient::without_hello(transport);
        let seq = client
            .send_invoke(42, "method", &BlobMsgTable::empty())
            .unwrap();
        let unrelated = status_reply(seq, 99, 0);
        peer.send(&unrelated).unwrap();
        assert_eq!(client.recv_frame().unwrap(), Some(unrelated));
        peer.send(&data_reply(seq, 99, 99)).unwrap();
        peer.send(&data_reply(seq, 42, 7)).unwrap();
        peer.send(&status_reply(seq, 42, 0)).unwrap();
        let bytes = client.recv_invoke_reply(seq).unwrap();
        assert_eq!(
            crate::blobmsg::lookup(&bytes, "value").unwrap().as_i32(),
            Some(7)
        );
        assert_eq!(client.recv_frame().unwrap(), Some(data_reply(seq, 99, 99)));
        assert!(matches!(
            client.recv_invoke_reply(seq),
            Err(UbusError::Malformed("invoke sequence is not pending"))
        ));
    }

    #[test]
    fn the_full_invocation_window_can_complete_in_reverse_order() {
        for background in [0, 256] {
            let (transport, mut peer) = MockTransport::pair();
            let mut client = UbusClient::without_hello(transport);
            let seqs: Vec<_> = (0..MAX_PENDING_INVOKES)
                .map(|_| {
                    client
                        .send_invoke(42, "method", &BlobMsgTable::empty())
                        .unwrap()
                })
                .collect();
            let event = Frame::to_broker(MsgType::Notify, 0, EnvBuilder::new().finish());
            for _ in 0..background {
                peer.send(&event).unwrap();
            }
            for (n, seq) in seqs.iter().enumerate() {
                peer.send(&data_reply(*seq, 42, n as i32)).unwrap();
                peer.send(&status_reply(*seq, 42, 0)).unwrap();
            }
            if background != 0 {
                // An ordinary synchronous request can run while the entire
                // invocation window and background traffic are buffered.
                peer.send(&status_reply(MAX_PENDING_INVOKES as u16 + 1, 0, 0))
                    .unwrap();
                client.ping().unwrap();
            }
            for (n, seq) in seqs.iter().enumerate().rev() {
                let data = client.recv_invoke_reply(*seq).unwrap();
                assert_eq!(
                    crate::blobmsg::lookup(&data, "value").unwrap().as_i32(),
                    Some(n as i32)
                );
            }
            for _ in 0..background {
                assert_eq!(client.recv_frame().unwrap(), Some(event.clone()));
            }
            assert!(client.recv_frame().unwrap().is_none());
        }
    }

    #[test]
    fn pending_invocations_are_bounded_and_raw_completion_frees_a_slot() {
        let (transport, mut peer) = MockTransport::pair();
        let mut client = UbusClient::without_hello(transport);
        let first = client
            .send_invoke(42, "method", &BlobMsgTable::empty())
            .unwrap();
        for _ in 1..MAX_PENDING_INVOKES {
            client
                .send_invoke(42, "method", &BlobMsgTable::empty())
                .unwrap();
        }
        assert!(matches!(
            client.send_invoke(42, "method", &BlobMsgTable::empty()),
            Err(UbusError::Malformed("pending invocation limit exceeded"))
        ));
        for _ in 0..MAX_PENDING_INVOKES {
            assert!(peer.recv().unwrap().is_some());
        }
        assert!(peer.recv().unwrap().is_none(), "rejected call was not sent");
        peer.send(&status_reply(first, 42, 0)).unwrap();
        assert!(client.recv_frame().unwrap().is_some());
        client
            .send_invoke(42, "method", &BlobMsgTable::empty())
            .unwrap();
    }

    #[test]
    fn receive_errors_retire_invocations_and_raw_users_can_release_tracking() {
        let (transport, _peer) = MockTransport::pair();
        let mut client = UbusClient::without_hello(transport);
        let seq = client
            .send_invoke(42, "method", &BlobMsgTable::empty())
            .unwrap();
        assert!(matches!(
            client.recv_invoke_reply(seq),
            Err(UbusError::Timeout("invoke"))
        ));
        assert!(!client.forget_invoke(seq));
        let seq = client
            .send_invoke(42, "method", &BlobMsgTable::empty())
            .unwrap();
        assert!(client.forget_invoke(seq));
        assert!(!client.forget_invoke(seq));
    }

    #[test]
    fn sequence_wrap_skips_outstanding_and_buffered_replies() {
        let (transport, mut peer) = MockTransport::pair();
        let mut client = UbusClient::without_hello(transport);
        let first = client
            .send_invoke(42, "method", &BlobMsgTable::empty())
            .unwrap();
        assert_eq!(first, 1);
        // Buffer a reply with seq=2 while completing a request with seq=3.
        client.seq = 3;
        peer.send(&status_reply(2, 99, 0)).unwrap();
        peer.send(&status_reply(3, 0, 0)).unwrap();
        client.ping().unwrap();
        client.seq = u16::MAX;
        assert_eq!(
            client
                .send_invoke(42, "method", &BlobMsgTable::empty())
                .unwrap(),
            u16::MAX
        );
        assert_eq!(
            client
                .send_invoke(42, "method", &BlobMsgTable::empty())
                .unwrap(),
            0
        );
        assert_eq!(
            client
                .send_invoke(42, "method", &BlobMsgTable::empty())
                .unwrap(),
            3
        );
    }

    /// One idle read between each complete frame models a slowly delivered
    /// multi-DATA response without depending on kernel scheduling.
    struct IntermittentReplies {
        frames: VecDeque<Frame>,
        idle: bool,
    }

    impl Transport for IntermittentReplies {
        fn send(&mut self, _: &Frame) -> Result<(), UbusError> {
            Ok(())
        }

        fn recv(&mut self) -> Result<Option<Frame>, UbusError> {
            self.idle = !self.idle;
            Ok(if self.idle {
                None
            } else {
                self.frames.pop_front()
            })
        }
    }

    fn intermittent_data(seq: u16) -> IntermittentReplies {
        IntermittentReplies {
            frames: (0..=IDLE_WAIT_BUDGET)
                .map(|_| Frame::to_peer(MsgType::Data, seq, 0, EnvBuilder::new().finish()))
                .collect(),
            idle: false,
        }
    }

    #[test]
    fn request_idle_budget_resets_when_response_data_makes_progress() {
        let mut transport = intermittent_data(1);
        let mut status = EnvBuilder::new();
        status.push_i32(UbusAttr::Status, 0);
        transport
            .frames
            .push_back(Frame::to_peer(MsgType::Status, 1, 0, status.finish()));
        let mut client = UbusClient::without_hello(transport);
        let mut received = 0;
        client
            .request(MsgType::Lookup, 0, EnvBuilder::new(), |_| {
                received += 1;
                Ok(())
            })
            .expect("a progressing response must finish despite many inter-frame waits");
        assert_eq!(received, IDLE_WAIT_BUDGET + 1);
    }

    #[test]
    fn split_phase_invoke_idle_budget_resets_on_valid_data() {
        let mut transport = IntermittentReplies {
            frames: (0..=IDLE_WAIT_BUDGET)
                .map(|n| data_reply(1, 42, n as i32))
                .collect(),
            idle: false,
        };
        transport.frames.push_back(status_reply(1, 42, 0));
        let mut client = UbusClient::without_hello(transport);
        let seq = client
            .send_invoke(42, "method", &BlobMsgTable::empty())
            .unwrap();
        let data = client.recv_invoke_reply(seq).unwrap();
        assert_eq!(
            crate::blobmsg::lookup(&data, "value").unwrap().as_i32(),
            Some(IDLE_WAIT_BUDGET as i32)
        );
    }

    #[test]
    fn request_still_times_out_on_silence_or_unrelated_data() {
        for seq in [1, 99] {
            let mut client = UbusClient::without_hello(intermittent_data(seq));
            assert!(matches!(
                client.request(MsgType::Lookup, 0, EnvBuilder::new(), |_| Ok(())),
                Err(UbusError::Timeout("request"))
            ));
        }
    }

    #[test]
    fn pending_queue_is_bounded() {
        let (transport, mut peer) = MockTransport::pair();
        let mut client = UbusClient::without_hello(transport);
        for _ in 0..=MAX_PENDING_MESSAGES {
            peer.send(&Frame::to_peer(
                MsgType::Notify,
                99,
                0,
                EnvBuilder::new().finish(),
            ))
            .unwrap();
        }
        assert!(matches!(
            client.ping(),
            Err(UbusError::Malformed("pending message queue exceeded cap"))
        ));
    }

    fn client_skip_hello(transport: MockTransport) -> UbusClient<MockTransport> {
        UbusClient::without_hello(transport)
    }

    /// A DATA frame missing its `Data` attr followed by STATUS=0 must
    /// surface as `Malformed` rather than masquerading as `Ok(empty)`.
    #[test]
    fn recv_invoke_reply_status_zero_after_corrupt_data_returns_malformed() {
        let (cli_t, mut srv_t) = MockTransport::pair();
        let mut cli = client_skip_hello(cli_t);
        let args = BlobMsgTable::empty();
        let seq = cli.send_invoke(7, "ping", &args).unwrap();
        // First DATA: envelope parses but no Data attr.
        let mut bad = EnvBuilder::new();
        bad.push_u32(UbusAttr::ObjId, 0);
        srv_t
            .send(&Frame::to_peer(MsgType::Data, seq, 7, bad.finish()))
            .unwrap();
        // Then STATUS=0 — broker claims success.
        let mut ok = EnvBuilder::new();
        ok.push_i32(UbusAttr::Status, 0);
        srv_t
            .send(&Frame::to_peer(MsgType::Status, seq, 7, ok.finish()))
            .unwrap();
        let err = cli.recv_invoke_reply(seq).expect_err("must surface");
        assert!(matches!(err, UbusError::Malformed(_)), "got {err:?}");
    }

    /// LOOKUP with STATUS=0 but no ObjId in the preceding DATA is a
    /// broker wire violation — surface as `Malformed`, not as a
    /// "not found" that collapses wire bugs with absence of the obj.
    #[test]
    fn lookup_status_zero_without_obj_id_is_malformed() {
        let (cli_t, mut srv_t) = MockTransport::pair();
        let mut cli = client_skip_hello(cli_t);
        // We can't easily intercept the lookup's seq without sending
        // first, so use a fresh seq counter trick: send the canned
        // DATA + STATUS=0 in advance so they're queued, then call
        // lookup which will use seq=1.
        // First: prep an empty DATA without ObjId + STATUS=0.
        let env_data = EnvBuilder::new().finish();
        srv_t
            .send(&Frame::to_peer(MsgType::Data, 1, 0, env_data))
            .unwrap();
        let mut env_status = EnvBuilder::new();
        env_status.push_i32(UbusAttr::Status, 0);
        srv_t
            .send(&Frame::to_peer(MsgType::Status, 1, 0, env_status.finish()))
            .unwrap();
        let err = cli.lookup("anything").expect_err("must surface");
        assert!(matches!(err, UbusError::Malformed(_)), "got {err:?}");
    }

    fn queue_hello(srv_t: &mut MockTransport, seq: u16, peer: u32) {
        srv_t
            .send(&Frame::to_peer(
                MsgType::Hello,
                seq,
                peer,
                alloc::vec::Vec::new(),
            ))
            .unwrap();
    }

    #[test]
    fn connect_accepts_unsolicited_hello_with_seq_zero() {
        let (cli_t, mut srv_t) = MockTransport::pair();
        queue_hello(&mut srv_t, 0, 1025);
        let cli = UbusClient::connect(cli_t).expect("HELLO accepted");
        assert_eq!(cli.peer_id(), Some(1025));
    }

    #[test]
    fn connect_accepts_hello_with_arbitrary_nonzero_seq() {
        let (cli_t, mut srv_t) = MockTransport::pair();
        queue_hello(&mut srv_t, 42, 4096);
        let cli = UbusClient::connect(cli_t).expect("HELLO accepted");
        assert_eq!(cli.peer_id(), Some(4096));
    }

    #[test]
    fn connect_rejects_hello_with_peer_zero() {
        let (cli_t, mut srv_t) = MockTransport::pair();
        queue_hello(&mut srv_t, 0, 0);
        let err = UbusClient::connect(cli_t).err().expect("connect must fail");
        assert!(matches!(err, UbusError::Malformed(_)), "got {err:?}");
    }

    #[test]
    fn connect_gives_up_after_too_many_non_hello_frames() {
        let (cli_t, mut srv_t) = MockTransport::pair();
        let env = EnvBuilder::new().finish();
        for i in 0..(HELLO_NON_HELLO_BUDGET + 1) {
            srv_t
                .send(&Frame::to_peer(MsgType::Ping, i as u16, 0, env.clone()))
                .unwrap();
        }
        let err = UbusClient::connect(cli_t).err().expect("connect must fail");
        assert!(matches!(err, UbusError::Malformed(_)), "got {err:?}");
    }

    /// Pin the budget *lower* bound: exactly `HELLO_NON_HELLO_BUDGET`
    /// non-HELLO frames followed by a valid HELLO must succeed. Guards
    /// against an off-by-one regression (`>` → `>=`) that would silently
    /// pass the upper-bound test.
    #[test]
    fn connect_accepts_hello_at_exact_non_hello_budget() {
        let (cli_t, mut srv_t) = MockTransport::pair();
        let env = EnvBuilder::new().finish();
        for i in 0..HELLO_NON_HELLO_BUDGET {
            srv_t
                .send(&Frame::to_peer(MsgType::Ping, i as u16, 0, env.clone()))
                .unwrap();
        }
        queue_hello(&mut srv_t, 0, 1025);
        let cli = UbusClient::connect(cli_t).expect("HELLO accepted at budget");
        assert_eq!(cli.peer_id(), Some(1025));
    }

    /// Silent broker: peer accepts the connection but sends nothing.
    /// `MockTransport`'s default `wait_recv` is a no-op, so the loop
    /// burns through `IDLE_WAIT_BUDGET + 1` iterations and surfaces
    /// `Timeout("hello")` rather than parking forever.
    #[test]
    fn connect_times_out_when_broker_is_silent() {
        let (cli_t, _srv_t) = MockTransport::pair();
        let err = UbusClient::connect(cli_t).err().expect("connect must fail");
        assert!(matches!(err, UbusError::Timeout("hello")), "got {err:?}");
    }
}
