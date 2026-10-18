//! Object provider: HELLO + ADD_OBJECT registration on construction
//! ([`UbusConnection::connect_and_register`]) followed by
//! [`UbusConnection::poll_one`] to dispatch INVOKE frames. Handlers
//! receive the inner blobmsg payload (the `Data` attr's contents) so
//! they don't see the envelope.

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;

use crate::blobmsg::{BlobBuilder, BlobIter, BlobMsgTable};
use crate::client::do_hello_handshake;
use crate::error::UbusError;
use crate::msg::{InboundMsg, OutboundMsg};
use crate::transport::Transport;
use crate::wire::{
    MsgType, ObjId, PeerId, IDLE_WAIT_BUDGET, UBUS_STATUS_INVALID_ARGUMENT,
    UBUS_STATUS_METHOD_NOT_FOUND, UBUS_STATUS_UNKNOWN_ERROR,
};

/// Receives the *contents* of the INVOKE's outer TABLE wrapper so the
/// handler can `lookup(args, "key")` directly. Returns the reply
/// TABLE — use [`BlobMsgTable::build`] to construct it.
pub type MethodHandler = Box<dyn FnMut(&[u8]) -> Result<BlobMsgTable, UbusError> + 'static>;

pub struct Method {
    pub name: String,
    pub handler: MethodHandler,
}

impl Method {
    pub fn new<F>(name: impl Into<String>, handler: F) -> Self
    where
        F: FnMut(&[u8]) -> Result<BlobMsgTable, UbusError> + 'static,
    {
        Self {
            name: name.into(),
            handler: Box::new(handler),
        }
    }
}

pub struct UbusObject {
    pub name: String,
    pub methods: Vec<Method>,
}

impl UbusObject {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            methods: Vec::new(),
        }
    }

    pub fn method<F>(mut self, name: impl Into<String>, handler: F) -> Self
    where
        F: FnMut(&[u8]) -> Result<BlobMsgTable, UbusError> + 'static,
    {
        self.methods.push(Method::new(name, handler));
        self
    }
}

/// Result of a single [`UbusConnection::poll_one`] tick. Lets the
/// caller distinguish "no frame available" from "frame ignored",
/// "INVOKE handled successfully", "handler returned an error" (already
/// reported as UNKNOWN_ERROR (9) to the peer; surfaced here so the caller can
/// log via injected telemetry), and "INVOKE was malformed" (already
/// reported as INVALID_ARGUMENT (2), but surfaced for the same reason).
#[derive(Debug)]
pub enum DispatchOutcome {
    /// Transport had no frame queued.
    Idle,
    /// INVOKE dispatched successfully; reply already sent.
    Handled,
    /// Handler returned an error; UNKNOWN_ERROR (9) already sent. Inner error
    /// is the handler's error so callers can record it.
    HandlerErr(UbusError),
    /// Frame was an INVOKE but malformed (no Method attr, corrupt
    /// envelope, etc.); INVALID_ARGUMENT (2) already sent.
    Malformed(UbusError),
    /// Frame's MsgType wasn't INVOKE (PING, STATUS, etc.) — accepted
    /// and consumed without a reply.
    Ignored(MsgType),
}

pub struct UbusConnection<T: Transport> {
    transport: T,
    object: UbusObject,
    /// Assigned by ubusd's ADD_OBJECT reply. Required for incoming
    /// INVOKE matching; set by `connect_and_register`.
    object_id: u32,
    /// Broker-assigned local id. `None` on test-seam constructions
    /// (`with_object_id`); a real handshake stores `Some(non_zero)`.
    peer_id: Option<u32>,
    /// Monotonic seq counter for outbound frames; protects against
    /// reply collisions if `register`'s seqs were ever reused.
    seq: u16,
}

impl<T: Transport> UbusConnection<T> {
    /// Open a connection, perform HELLO + ADD_OBJECT, and return a
    /// ready-to-poll connection. Returns `Err(Malformed)` if the
    /// broker omits a peer id on HELLO or an object id on ADD_OBJECT.
    pub fn connect_and_register(transport: T, object: UbusObject) -> Result<Self, UbusError> {
        let mut conn = Self {
            transport,
            object,
            object_id: 0,
            peer_id: None,
            seq: 1,
        };
        let seq = conn.next_seq();
        conn.peer_id = Some(do_hello_handshake(&mut conn.transport, seq)?);
        conn.do_add_object()?;
        Ok(conn)
    }

    pub fn peer_id(&self) -> Option<u32> {
        self.peer_id
    }

    /// Object id assigned by ADD_OBJECT, or `0` on test-seam
    /// constructions (`with_object_id`) that bypass the handshake.
    pub fn object_id(&self) -> u32 {
        self.object_id
    }

    /// Raw transport access for tests. Callers must not consume frames
    /// `poll_one` expects or reuse seqs from the internal counter.
    #[doc(hidden)]
    pub fn transport_mut(&mut self) -> &mut T {
        &mut self.transport
    }

    fn do_add_object(&mut self) -> Result<(), UbusError> {
        // SIGNATURE-only ADD_OBJECT: ubusd derives a fresh OBJTYPE.
        // Sending OBJTYPE=0 would make it look up type 0 (none) and the
        // request would fail silently.
        let mut sig = BlobBuilder::new();
        for m in &self.object.methods {
            sig.open_table(Some(&m.name));
            sig.close_table();
        }
        let sig_buf = sig.finish();

        let seq = self.next_seq();
        self.transport.send(
            &OutboundMsg::AddObject {
                path: &self.object.name,
                signature: &sig_buf,
            }
            .into_frame(seq),
        )?;

        // ubusd may send the assigned OBJID in a DATA before the closing
        // STATUS=0, or directly in the STATUS — accept both.
        let mut object_id: Option<u32> = None;
        let mut idle_waits = 0u32;
        loop {
            let frame = match self.transport.recv()? {
                Some(f) => f,
                None => {
                    idle_waits += 1;
                    if idle_waits > IDLE_WAIT_BUDGET {
                        return Err(UbusError::Timeout("add_object"));
                    }
                    self.transport.wait_recv();
                    continue;
                }
            };
            if frame.hdr.seq != seq
                || frame.hdr.peer != 0
                || !matches!(frame.hdr.ty, MsgType::Data | MsgType::Status)
            {
                continue;
            }
            match InboundMsg::parse(&frame)? {
                InboundMsg::Data {
                    obj_id: Some(id), ..
                } => object_id = Some(id),
                InboundMsg::Status { status, peer, .. } => {
                    if status != 0 {
                        return Err(UbusError::Status(status));
                    }
                    // STATUS may carry the OBJID inline if it wasn't
                    // emitted as a preceding DATA.
                    if object_id.is_none() {
                        if let Some(id) = inline_obj_id(&frame.payload) {
                            object_id = Some(id);
                        }
                    }
                    self.object_id = object_id.ok_or(UbusError::Malformed(
                        "ADD_OBJECT reply missing ObjId after STATUS=0",
                    ))?;
                    let _ = peer;
                    return Ok(());
                }
                _ => continue,
            }
        }
    }

    pub fn unregister(&mut self) -> Result<(), UbusError> {
        let obj_id = ObjId::new(self.object_id).ok_or(UbusError::NotRegistered)?;
        let seq = self.next_seq();
        self.transport
            .send(&OutboundMsg::RemoveObject { obj_id }.into_frame(seq))
    }

    /// Remove this object and wait for the broker's acknowledgement before
    /// closing the transport. This also lets it consume preceding replies.
    pub fn unregister_and_wait(&mut self) -> Result<(), UbusError> {
        let obj_id = ObjId::new(self.object_id).ok_or(UbusError::NotRegistered)?;
        let seq = self.next_seq();
        self.transport
            .send(&OutboundMsg::RemoveObject { obj_id }.into_frame(seq))?;
        let mut idle_waits = 0;
        loop {
            let Some(frame) = self.transport.recv()? else {
                idle_waits += 1;
                if idle_waits > IDLE_WAIT_BUDGET {
                    return Err(UbusError::Timeout("remove_object"));
                }
                self.transport.wait_recv();
                continue;
            };
            if frame.hdr.seq != seq || frame.hdr.peer != 0 || frame.hdr.ty != MsgType::Status {
                continue;
            }
            let status = crate::envelope::read_status(&frame.payload)?;
            if status != 0 {
                return Err(UbusError::Status(status));
            }
            self.object_id = 0;
            return Ok(());
        }
    }

    /// Process at most one queued frame. Returns a [`DispatchOutcome`]
    /// describing what happened — callers should record `HandlerErr` /
    /// `Malformed` via injected telemetry if they care to surface
    /// dispatch problems beyond the wire-level INVALID_ARGUMENT/UNKNOWN_ERROR already
    /// sent to the peer.
    pub fn poll_one(&mut self) -> Result<DispatchOutcome, UbusError> {
        let frame = match self.transport.recv()? {
            Some(f) => f,
            None => return Ok(DispatchOutcome::Idle),
        };
        // Guard upfront so dispatch_invoke can't emit a DATA frame and
        // then fail the trailing STATUS, leaving the peer with a
        // partial reply.
        if frame.hdr.ty == MsgType::Invoke && self.object_id == 0 {
            return Err(UbusError::NotRegistered);
        }
        let parsed = match InboundMsg::parse(&frame) {
            Ok(p) => p,
            Err(e) => {
                // Couldn't even decode the envelope — only INVOKE has
                // a meaningful "reply INVALID_ARGUMENT" path; anything else, drop.
                if frame.hdr.ty == MsgType::Invoke {
                    self.send_status(frame.hdr.seq, frame.hdr.peer, UBUS_STATUS_INVALID_ARGUMENT)?;
                    return Ok(DispatchOutcome::Malformed(e));
                }
                return Ok(DispatchOutcome::Ignored(frame.hdr.ty));
            }
        };
        match parsed {
            InboundMsg::Invoke {
                seq,
                peer,
                obj_id,
                method,
                data,
            } => {
                if obj_id.get() != self.object_id {
                    self.send_status(seq, peer, UBUS_STATUS_INVALID_ARGUMENT)?;
                    return Ok(DispatchOutcome::Malformed(UbusError::Malformed(
                        "INVOKE addressed to different ObjId",
                    )));
                }
                self.dispatch_invoke(seq, peer, method, data)
            }
            InboundMsg::Hello { .. }
            | InboundMsg::Status { .. }
            | InboundMsg::Data { .. }
            | InboundMsg::Ping { .. }
            | InboundMsg::Other { .. } => Ok(DispatchOutcome::Ignored(frame.hdr.ty)),
        }
    }

    fn dispatch_invoke(
        &mut self,
        seq: u16,
        peer: u32,
        method: &str,
        data: Option<&[u8]>,
    ) -> Result<DispatchOutcome, UbusError> {
        let args = match data {
            Some(blobmsg_contents) => match validate_data_args(blobmsg_contents) {
                Ok(args) => args,
                Err(e) => {
                    self.send_status(seq, peer, UBUS_STATUS_INVALID_ARGUMENT)?;
                    return Ok(DispatchOutcome::Malformed(e));
                }
            },
            None => &[],
        };
        let handler = match self.object.methods.iter_mut().find(|m| m.name == method) {
            Some(h) => h,
            None => {
                self.send_status(seq, peer, UBUS_STATUS_METHOD_NOT_FOUND)?;
                return Ok(DispatchOutcome::Handled);
            }
        };
        match (handler.handler)(args) {
            Ok(reply) => {
                self.send_data(seq, peer, &reply)?;
                self.send_status(seq, peer, 0)?;
                Ok(DispatchOutcome::Handled)
            }
            Err(e) => {
                self.send_status(seq, peer, UBUS_STATUS_UNKNOWN_ERROR)?;
                Ok(DispatchOutcome::HandlerErr(e))
            }
        }
    }

    fn send_data(&mut self, seq: u16, peer: u32, reply: &BlobMsgTable) -> Result<(), UbusError> {
        let peer = PeerId::new(peer).ok_or(UbusError::Malformed(
            "INVOKE caller peer-id is zero (broker would silently drop reply)",
        ))?;
        let obj_id = ObjId::new(self.object_id).ok_or(UbusError::NotRegistered)?;
        self.transport.send(
            &OutboundMsg::DataReply {
                peer,
                obj_id: Some(obj_id),
                data: reply,
            }
            .into_frame(seq),
        )
    }

    fn send_status(&mut self, seq: u16, peer: u32, status: i32) -> Result<(), UbusError> {
        let peer = PeerId::new(peer).ok_or(UbusError::Malformed(
            "INVOKE caller peer-id is zero (broker would silently drop STATUS)",
        ))?;
        let obj_id = ObjId::new(self.object_id).ok_or(UbusError::NotRegistered)?;
        self.transport.send(
            &OutboundMsg::Status {
                peer,
                obj_id: Some(obj_id),
                status,
            }
            .into_frame(seq),
        )
    }

    fn next_seq(&mut self) -> u16 {
        let s = self.seq;
        self.seq = self.seq.wrapping_add(1);
        s
    }
}

impl<T: Transport> UbusConnection<T> {
    /// Bypass the broker handshake, setting `object_id` directly. Use
    /// only from tests that drive INVOKE dispatch against a paired
    /// `MockTransport` without a real ubusd in between. Kept public so
    /// integration tests can exercise direct client/server dispatch
    /// without spinning a broker shim.
    #[doc(hidden)]
    pub fn with_object_id(transport: T, object: UbusObject, object_id: u32) -> Self {
        Self {
            transport,
            object,
            object_id,
            peer_id: None,
            seq: 100,
        }
    }
}

/// Walk the envelope manually for the case where `InboundMsg::Status`
/// also carried an `ObjId` attr (the broker's STATUS-only reply path).
fn inline_obj_id(frame_payload: &[u8]) -> Option<u32> {
    crate::envelope::find(frame_payload, crate::envelope::UbusAttr::ObjId).and_then(|a| a.as_u32())
}

/// Validate `UBUS_ATTR_DATA` payload as a sequence of well-formed
/// blobmsg attrs (the libubox convention: `blob_put(DATA, blob_data(msg),
/// blob_len(msg))` puts the *contents* of the args TABLE — without the
/// outer wrapper — directly under DATA). Empty payload is `(NULL, 0)`'s
/// no-args shape and is fine. Any TLV violation surfaces as `Malformed`
/// so the dispatcher replies `INVALID_ARGUMENT (2)` instead of handing garbage to
/// a handler.
fn validate_data_args(blobmsg_contents: &[u8]) -> Result<&[u8], UbusError> {
    let mut it = BlobIter::new(blobmsg_contents);
    while it.try_next()?.is_some() {}
    Ok(blobmsg_contents)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn acknowledged_removal_marks_the_object_unregistered() {
        let (transport, mut peer) = crate::transport::MockTransport::pair();
        let mut server = UbusConnection::with_object_id(transport, UbusObject::new("example"), 7);
        let mut envelope = crate::envelope::EnvBuilder::new();
        envelope.push_i32(crate::envelope::UbusAttr::Status, 0);
        peer.send(&crate::wire::Frame::to_peer(
            crate::wire::MsgType::Status,
            server.seq,
            0,
            envelope.finish(),
        ))
        .unwrap();
        server.unregister_and_wait().unwrap();
        assert_eq!(server.object_id(), 0);
        assert!(matches!(
            server.unregister_and_wait(),
            Err(UbusError::NotRegistered)
        ));
    }
    #[test]
    fn registration_ignores_replies_from_an_object_instead_of_the_broker() {
        let (transport, mut peer) = MockTransport::pair();
        queue_hello(&mut peer, 0, 1025);
        for (source, object) in [(42, 99), (0, 7)] {
            let mut env = EnvBuilder::new();
            env.push_u32(UbusAttr::ObjId, object);
            peer.send(&Frame::to_peer(MsgType::Data, 2, source, env.finish()))
                .unwrap();
            let mut env = EnvBuilder::new();
            env.push_i32(UbusAttr::Status, 0);
            peer.send(&Frame::to_peer(MsgType::Status, 2, source, env.finish()))
                .unwrap();
        }
        let server =
            UbusConnection::connect_and_register(transport, UbusObject::new("example")).unwrap();
        assert_eq!(server.object_id(), 7);
    }

    #[test]
    fn acknowledged_removal_ignores_an_unrelated_objects_status() {
        let (transport, mut peer) = MockTransport::pair();
        let mut server = UbusConnection::with_object_id(transport, UbusObject::new("example"), 7);
        for (source, status) in [(42, 6), (0, 0)] {
            let mut env = EnvBuilder::new();
            env.push_i32(UbusAttr::Status, status);
            peer.send(&Frame::to_peer(
                MsgType::Status,
                server.seq,
                source,
                env.finish(),
            ))
            .unwrap();
        }
        server.unregister_and_wait().unwrap();
        assert_eq!(server.object_id(), 0);
    }

    use crate::blobmsg::lookup;
    use crate::envelope::{find, EnvBuilder, UbusAttr};
    use crate::transport::MockTransport;
    use crate::wire::Frame;

    fn invoke_frame(method: &str, blobmsg_args: Option<&BlobMsgTable>) -> Frame {
        let mut env = EnvBuilder::new();
        env.push_u32(UbusAttr::ObjId, 7);
        env.push_str(UbusAttr::Method, method);
        // DATA carries blobmsg contents directly (libubox convention);
        // an empty/no-args call is a zero-length DATA payload — same
        // shape as `ubus_invoke_async_fd`'s `blob_put(DATA, NULL, 0)`.
        env.push_data(blobmsg_args.map(|t| t.contents()).unwrap_or(&[]));
        Frame::to_peer(MsgType::Invoke, 1, 7, env.finish())
    }

    /// Build a `UbusConnection` whose `object_id` is preset to `7`,
    /// bypassing the broker handshake so unit tests can drive INVOKE
    /// dispatch directly.
    fn server_with_obj(transport: MockTransport, obj: UbusObject) -> UbusConnection<MockTransport> {
        UbusConnection::with_object_id(transport, obj, 7)
    }

    #[test]
    fn dispatch_calls_handler_and_replies_status_zero() {
        let (a, mut b) = MockTransport::pair();
        let obj = UbusObject::new("example").method("ping", |_args| {
            Ok(BlobMsgTable::build(|bb| {
                bb.put_str(Some("pong"), "yes");
            }))
        });
        let mut srv = server_with_obj(a, obj);
        b.send(&invoke_frame("ping", None)).unwrap();

        match srv.poll_one().unwrap() {
            DispatchOutcome::Handled => {}
            other => panic!("expected Handled, got {other:?}"),
        }

        let data = b.recv().unwrap().expect("data frame");
        assert_eq!(data.hdr.ty, MsgType::Data);
        // DATA payload is blobmsg contents directly (libubox shape),
        // no outer TABLE wrapper to strip.
        let blobmsg = find(&data.payload, UbusAttr::Data)
            .map(|a| a.as_blobmsg().to_vec())
            .unwrap();
        assert_eq!(lookup(&blobmsg, "pong").unwrap().as_str(), Some("yes"));

        let status = b.recv().unwrap().expect("status frame");
        assert_eq!(status.hdr.ty, MsgType::Status);
        let s = find(&status.payload, UbusAttr::Status).and_then(|a| a.as_u32());
        assert_eq!(s, Some(0));
        let oid = find(&status.payload, UbusAttr::ObjId).and_then(|a| a.as_u32());
        assert_eq!(oid, Some(7), "STATUS must carry the responder's ObjId");
    }

    #[test]
    fn dispatch_unknown_method_returns_method_not_found() {
        let (a, mut b) = MockTransport::pair();
        let obj = UbusObject::new("example").method("known", |_| Ok(BlobMsgTable::empty()));
        let mut srv = server_with_obj(a, obj);
        b.send(&invoke_frame("unknown", None)).unwrap();
        assert!(matches!(srv.poll_one().unwrap(), DispatchOutcome::Handled));
        let status = b.recv().unwrap().expect("status frame");
        let s = find(&status.payload, UbusAttr::Status)
            .and_then(|a| a.as_u32())
            .map(|u| u as i32);
        assert_eq!(s, Some(crate::wire::UBUS_STATUS_METHOD_NOT_FOUND));
    }

    #[test]
    fn dispatch_invoke_without_data_attr_treats_args_as_empty() {
        let (a, mut b) = MockTransport::pair();
        let obj = UbusObject::new("example").method("ping", |args| {
            assert!(args.is_empty(), "missing Data attr must map to empty args");
            Ok(BlobMsgTable::empty())
        });
        let mut srv = server_with_obj(a, obj);

        let mut env = EnvBuilder::new();
        env.push_u32(UbusAttr::ObjId, 7);
        env.push_str(UbusAttr::Method, "ping");
        b.send(&Frame::to_peer(MsgType::Invoke, 1, 7, env.finish()))
            .unwrap();

        assert!(matches!(srv.poll_one().unwrap(), DispatchOutcome::Handled));
        let _data = b.recv().unwrap().expect("data frame");
        let status = b.recv().unwrap().expect("status frame");
        let s = find(&status.payload, UbusAttr::Status)
            .and_then(|a| a.as_u32())
            .map(|u| u as i32);
        assert_eq!(s, Some(0));
    }

    #[test]
    fn poll_one_returns_idle_when_no_frame() {
        let (a, _b) = MockTransport::pair();
        let mut srv = server_with_obj(a, UbusObject::new("example"));
        assert!(matches!(srv.poll_one().unwrap(), DispatchOutcome::Idle));
    }

    /// Corrupt-envelope INVOKE replies `INVALID_ARGUMENT` (malformed) and surfaces
    /// `DispatchOutcome::Malformed` so the caller can log via injected
    /// telemetry.
    #[test]
    fn dispatch_corrupt_envelope_replies_invalid_argument() {
        let (a, mut b) = MockTransport::pair();
        let obj = UbusObject::new("example").method("ping", |_| Ok(BlobMsgTable::empty()));
        let mut srv = server_with_obj(a, obj);
        b.send(&Frame::to_peer(MsgType::Invoke, 9, 7, Vec::new()))
            .unwrap();
        match srv.poll_one().unwrap() {
            DispatchOutcome::Malformed(_) => {}
            other => panic!("expected Malformed, got {other:?}"),
        }
        let status = b.recv().unwrap().expect("status frame");
        let s = find(&status.payload, UbusAttr::Status)
            .and_then(|a| a.as_u32())
            .map(|u| u as i32);
        assert_eq!(s, Some(UBUS_STATUS_INVALID_ARGUMENT));
    }

    /// INVOKE envelope without `Method` must reply `INVALID_ARGUMENT` and surface
    /// Malformed — distinct from "method name doesn't match".
    #[test]
    fn dispatch_missing_method_attr_replies_invalid_argument() {
        let (a, mut b) = MockTransport::pair();
        let obj = UbusObject::new("example").method("ping", |_| Ok(BlobMsgTable::empty()));
        let mut srv = server_with_obj(a, obj);
        let mut env = EnvBuilder::new();
        env.push_u32(UbusAttr::ObjId, 7);
        // No push_str(Method, _).
        b.send(&Frame::to_peer(MsgType::Invoke, 9, 7, env.finish()))
            .unwrap();
        match srv.poll_one().unwrap() {
            DispatchOutcome::Malformed(_) => {}
            other => panic!("expected Malformed, got {other:?}"),
        }
        let status = b.recv().unwrap().expect("status frame");
        let s = find(&status.payload, UbusAttr::Status)
            .and_then(|a| a.as_u32())
            .map(|u| u as i32);
        assert_eq!(s, Some(UBUS_STATUS_INVALID_ARGUMENT));
    }

    /// INVOKE with a Data attr whose blobmsg wrapper is corrupt must
    /// reply `INVALID_ARGUMENT` and surface Malformed, not silently dispatch with
    /// empty args.
    #[test]
    fn dispatch_corrupt_data_wrapper_replies_invalid_argument() {
        let (a, mut b) = MockTransport::pair();
        let obj = UbusObject::new("example").method("ping", |_| Ok(BlobMsgTable::empty()));
        let mut srv = server_with_obj(a, obj);
        let mut env = EnvBuilder::new();
        env.push_u32(UbusAttr::ObjId, 7);
        env.push_str(UbusAttr::Method, "ping");
        env.push_data(&[0, 0, 0, 0]);
        b.send(&Frame::to_peer(MsgType::Invoke, 9, 7, env.finish()))
            .unwrap();
        match srv.poll_one().unwrap() {
            DispatchOutcome::Malformed(_) => {}
            other => panic!("expected Malformed, got {other:?}"),
        }
        let status = b.recv().unwrap().expect("status frame");
        let s = find(&status.payload, UbusAttr::Status)
            .and_then(|a| a.as_u32())
            .map(|u| u as i32);
        assert_eq!(s, Some(UBUS_STATUS_INVALID_ARGUMENT));
    }

    /// Handler error surfaces both as `UNKNOWN_ERROR (9)` to the peer and as
    /// `DispatchOutcome::HandlerErr` to the caller — the caller can
    /// then record the inner error via injected telemetry without the
    /// peer needing to carry the error string.
    #[test]
    fn dispatch_handler_err_surfaces_outcome_and_unknown_error() {
        let (a, mut b) = MockTransport::pair();
        let obj = UbusObject::new("example").method("fail", |_| Err(UbusError::Malformed("boom")));
        let mut srv = server_with_obj(a, obj);
        b.send(&invoke_frame("fail", None)).unwrap();
        match srv.poll_one().unwrap() {
            DispatchOutcome::HandlerErr(UbusError::Malformed("boom")) => {}
            other => panic!("expected HandlerErr(Malformed(\"boom\")), got {other:?}"),
        }
        let status = b.recv().unwrap().expect("status frame");
        let s = find(&status.payload, UbusAttr::Status)
            .and_then(|a| a.as_u32())
            .map(|u| u as i32);
        assert_eq!(s, Some(UBUS_STATUS_UNKNOWN_ERROR));
    }

    /// A non-INVOKE frame (PING) is consumed but reported as Ignored
    /// so callers can count broker traffic without confusing it with
    /// an INVOKE that was dispatched.
    #[test]
    fn poll_one_ignores_non_invoke_frame() {
        let (a, mut b) = MockTransport::pair();
        let mut srv = server_with_obj(a, UbusObject::new("example"));
        let env = EnvBuilder::new().finish();
        b.send(&Frame::to_peer(MsgType::Ping, 1, 1, env)).unwrap();
        match srv.poll_one().unwrap() {
            DispatchOutcome::Ignored(MsgType::Ping) => {}
            other => panic!("expected Ignored(Ping), got {other:?}"),
        }
    }

    /// `unregister` before a successful ADD_OBJECT would emit
    /// `RemoveObject{obj_id: 0}`, which upstream `ubusd_handle_remove_object`
    /// silently drops (the broker uses OBJID to look up the responder).
    /// Surface the misuse rather than ship a frame the broker discards.
    #[test]
    fn unregister_before_registration_is_rejected() {
        let (a, _b) = MockTransport::pair();
        let mut srv = UbusConnection::with_object_id(a, UbusObject::new("example"), 0);
        match srv.unregister() {
            Err(UbusError::NotRegistered) => {}
            other => panic!("expected NotRegistered, got {other:?}"),
        }
    }

    /// Symmetric guard: dispatching INVOKE before registration would
    /// emit a STATUS without OBJID that upstream silently drops.
    #[test]
    fn dispatch_before_registration_surfaces_not_registered() {
        let (a, mut b) = MockTransport::pair();
        let obj = UbusObject::new("example").method("ping", |_| Ok(BlobMsgTable::empty()));
        let mut srv = UbusConnection::with_object_id(a, obj, 0);
        b.send(&invoke_frame("ping", None)).unwrap();
        match srv.poll_one() {
            Err(UbusError::NotRegistered) => {}
            other => panic!("expected NotRegistered, got {other:?}"),
        }
    }

    fn queue_hello(srv_t: &mut MockTransport, seq: u16, peer: u32) {
        srv_t
            .send(&Frame::to_peer(
                MsgType::Hello,
                seq,
                peer,
                EnvBuilder::new().finish(),
            ))
            .unwrap();
    }

    fn queue_add_object_status(srv_t: &mut MockTransport, seq: u16) {
        let mut env = EnvBuilder::new();
        env.push_u32(UbusAttr::ObjId, 7);
        env.push_i32(UbusAttr::Status, 0);
        srv_t
            .send(&Frame::to_peer(MsgType::Status, seq, 0, env.finish()))
            .unwrap();
    }

    #[test]
    fn connect_and_register_accepts_unsolicited_hello_with_seq_zero() {
        let (cli_t, mut srv_t) = MockTransport::pair();
        queue_hello(&mut srv_t, 0, 1025);
        queue_add_object_status(&mut srv_t, 2);
        let conn = UbusConnection::connect_and_register(cli_t, UbusObject::new("example.t"))
            .expect("HELLO + ADD_OBJECT accepted");
        assert_eq!(conn.peer_id(), Some(1025));
    }

    /// Silent broker: the server-side handshake must `Timeout("hello")`
    /// rather than park forever, mirroring the client-side guarantee.
    #[test]
    fn connect_and_register_times_out_when_broker_is_silent() {
        let (cli_t, _srv_t) = MockTransport::pair();
        let err = match UbusConnection::connect_and_register(cli_t, UbusObject::new("example.t")) {
            Ok(_) => panic!("connect_and_register must fail"),
            Err(e) => e,
        };
        assert!(matches!(err, UbusError::Timeout("hello")), "got {err:?}");
    }

    /// Broker accepts HELLO but never replies to ADD_OBJECT — the
    /// `do_add_object` loop must surface `Timeout("add_object")`
    /// instead of parking forever.
    #[test]
    fn connect_and_register_times_out_when_add_object_unanswered() {
        let (cli_t, mut srv_t) = MockTransport::pair();
        queue_hello(&mut srv_t, 0, 1025);
        let err = match UbusConnection::connect_and_register(cli_t, UbusObject::new("example.t")) {
            Ok(_) => panic!("connect_and_register must fail"),
            Err(e) => e,
        };
        assert!(
            matches!(err, UbusError::Timeout("add_object")),
            "got {err:?}"
        );
    }

    #[test]
    fn connect_and_register_rejects_hello_with_peer_zero() {
        let (cli_t, mut srv_t) = MockTransport::pair();
        queue_hello(&mut srv_t, 0, 0);
        let err = match UbusConnection::connect_and_register(cli_t, UbusObject::new("example.t")) {
            Ok(_) => panic!("connect_and_register must fail"),
            Err(e) => e,
        };
        assert!(matches!(err, UbusError::Malformed(_)), "got {err:?}");
    }

    /// INVOKE addressed to a different ObjId than ours must not reach
    /// any handler (would mis-dispatch into someone else's namespace).
    #[test]
    fn dispatch_invoke_with_wrong_obj_id_replies_malformed() {
        let (a, mut b) = MockTransport::pair();
        let obj = UbusObject::new("example").method("ping", |_| Ok(BlobMsgTable::empty()));
        let mut srv = server_with_obj(a, obj);
        let mut env = EnvBuilder::new();
        env.push_u32(UbusAttr::ObjId, 99); // not our object_id (=7)
        env.push_str(UbusAttr::Method, "ping");
        env.push_data(BlobMsgTable::empty().contents());
        b.send(&Frame::to_peer(MsgType::Invoke, 1, 99, env.finish()))
            .unwrap();
        match srv.poll_one().unwrap() {
            DispatchOutcome::Malformed(_) => {}
            other => panic!("expected Malformed, got {other:?}"),
        }
    }
}
