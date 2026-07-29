//! Typed wire-message layer above [`crate::wire::Frame`]. Each
//! variant carries the attrs ubusd expects for its `MsgType`, so
//! constructing an INVOKE without an `ObjId` or a STATUS without a
//! status code becomes a compile error rather than a hand-rolled
//! `find(...).unwrap_or(...)` runtime path. Replies (`Status`,
//! `DataReply`) carry their target peer because the value comes from
//! the originating client's frame, not from the message itself.

use crate::blobmsg::BlobMsgTable;
use crate::envelope::{iter_attrs, unwrap, EnvBuilder, UbusAttr};
use crate::error::UbusError;
use crate::wire::{Frame, MsgType, ObjId, PeerId};

/// Outbound (locally-built) message. Encoded into a [`Frame`] via
/// [`OutboundMsg::into_frame`]. The seq comes from the caller; peer is
/// implied by the message kind for requests, explicit on replies.
#[derive(Clone, Debug)]
pub enum OutboundMsg<'a> {
    Hello,
    Lookup {
        obj_path: &'a str,
    },
    Invoke {
        obj_id: ObjId,
        method: &'a str,
        args: &'a BlobMsgTable,
    },
    AddObject {
        path: &'a str,
        signature: &'a [u8],
    },
    /// Broker-bound (peer=0) — libubox sends REMOVE_OBJECT to the
    /// broker to detach the object id; the broker uses `ObjId` from
    /// the envelope to find which connection owns it. Setting
    /// `peer=local_id` would address the message back at our own
    /// connection and ubusd would silently drop it.
    RemoveObject {
        obj_id: ObjId,
    },
    /// Reply: peer is the originating client's local id, copied from
    /// the inbound INVOKE frame. `obj_id` carries the responder's
    /// registered object id — upstream `ubusd_handle_response` looks
    /// the object up by OBJID before forwarding the reply, so a STATUS
    /// without OBJID is silently dropped by the broker.
    Status {
        peer: PeerId,
        obj_id: Option<ObjId>,
        status: i32,
    },
    /// Reply payload accompanying a successful INVOKE. `obj_id` is set
    /// when the server has registered an object.
    DataReply {
        peer: PeerId,
        obj_id: Option<ObjId>,
        data: &'a BlobMsgTable,
    },
}

impl<'a> OutboundMsg<'a> {
    pub fn into_frame(self, seq: u16) -> Frame {
        match self {
            // libubus wraps even an "empty" HELLO with the 4-byte
            // empty envelope header. Without it, upstream `ubusd`'s
            // recv loop reads our 8-byte frame, then waits for the
            // 4 envelope bytes that never come — the next outbound
            // header (LOOKUP, ADD_OBJECT) gets mis-decoded as that
            // envelope and the connection desyncs forever.
            Self::Hello => Frame::to_broker(MsgType::Hello, seq, EnvBuilder::new().finish()),
            Self::Lookup { obj_path } => {
                let mut env = EnvBuilder::new();
                env.push_str(UbusAttr::ObjPath, obj_path);
                Frame::to_broker(MsgType::Lookup, seq, env.finish())
            }
            Self::Invoke {
                obj_id,
                method,
                args,
            } => {
                let mut env = EnvBuilder::new();
                env.push_u32(UbusAttr::ObjId, obj_id.get());
                env.push_str(UbusAttr::Method, method);
                // libubox convention: UBUS_ATTR_DATA carries the inner
                // contents of the args TABLE, not the wrapped TABLE
                // attr itself. Send contents to stay byte-compatible
                // with `ubus_invoke_async_fd`.
                env.push_data(args.contents());
                Frame::to_peer(MsgType::Invoke, seq, obj_id.get(), env.finish())
            }
            Self::AddObject { path, signature } => {
                let mut env = EnvBuilder::new();
                env.push_str(UbusAttr::ObjPath, path);
                env.push_signature(signature);
                Frame::to_broker(MsgType::AddObject, seq, env.finish())
            }
            Self::RemoveObject { obj_id } => {
                let mut env = EnvBuilder::new();
                env.push_u32(UbusAttr::ObjId, obj_id.get());
                Frame::to_broker(MsgType::RemoveObject, seq, env.finish())
            }
            Self::Status {
                peer,
                obj_id,
                status,
            } => {
                let mut env = EnvBuilder::new();
                if let Some(id) = obj_id {
                    env.push_u32(UbusAttr::ObjId, id.get());
                }
                env.push_i32(UbusAttr::Status, status);
                Frame::to_peer(MsgType::Status, seq, peer.get(), env.finish())
            }
            Self::DataReply { peer, obj_id, data } => {
                let mut env = EnvBuilder::new();
                if let Some(id) = obj_id {
                    env.push_u32(UbusAttr::ObjId, id.get());
                }
                // Reply DATA carries contents-without-wrapper to match
                // the request shape (see Invoke arm above).
                env.push_data(data.contents());
                Frame::to_peer(MsgType::Data, seq, peer.get(), env.finish())
            }
        }
    }
}

/// Typed parse of an inbound [`Frame`]. Required attrs surface as
/// fields; missing-required-attr is a `Malformed` error rather than
/// silently turning into a default. Optional attrs use `Option`.
#[derive(Clone, Debug)]
pub enum InboundMsg<'a> {
    Hello {
        seq: u16,
        peer: u32,
    },
    /// Status reply — required `status` attr.
    Status {
        seq: u16,
        peer: u32,
        status: i32,
    },
    /// Data frame — `obj_id` and `data` are both optional in the wire
    /// shape (broker may DATA the OBJID before STATUS=0; server-side
    /// DATA replies always include the inner blobmsg payload).
    Data {
        seq: u16,
        peer: u32,
        obj_id: Option<u32>,
        data: Option<&'a [u8]>,
    },
    Ping {
        seq: u16,
        peer: u32,
    },
    /// Inbound INVOKE on a registered object — `obj_id` and `method`
    /// are required by the wire contract, but the typed parse keeps
    /// `data` optional because some peers omit the empty-args wrapper.
    Invoke {
        seq: u16,
        peer: u32,
        obj_id: ObjId,
        method: &'a str,
        data: Option<&'a [u8]>,
    },
    /// Anything we don't decode into a richer shape — Subscribe,
    /// Unsubscribe, Notify, Monitor — passes through here so the
    /// dispatch loop can fall through without losing the frame type.
    Other {
        ty: MsgType,
        seq: u16,
        peer: u32,
    },
}

impl<'a> InboundMsg<'a> {
    pub fn parse(frame: &'a Frame) -> Result<Self, UbusError> {
        let seq = frame.hdr.seq;
        let peer = frame.hdr.peer;
        // HELLO has no envelope body in the broker -> client direction.
        if frame.hdr.ty == MsgType::Hello {
            return Ok(Self::Hello { seq, peer });
        }
        let inner = unwrap(&frame.payload)?;
        let mut method = None;
        let mut data = None;
        let mut obj_id = None;
        let mut status = None;
        let mut iter = iter_attrs(inner);
        while let Some(a) = iter.try_next()? {
            match UbusAttr::from_u8(a.id) {
                Some(UbusAttr::Method) => method = a.as_str(),
                Some(UbusAttr::Data) => data = Some(a.as_blobmsg()),
                Some(UbusAttr::ObjId) => obj_id = a.as_u32(),
                Some(UbusAttr::Status) => status = a.as_u32().map(|u| u as i32),
                _ => {}
            }
        }
        match frame.hdr.ty {
            MsgType::Status => {
                let status =
                    status.ok_or(UbusError::Malformed("STATUS frame without Status attr"))?;
                Ok(Self::Status { seq, peer, status })
            }
            MsgType::Data => Ok(Self::Data {
                seq,
                peer,
                obj_id,
                data,
            }),
            MsgType::Ping => Ok(Self::Ping { seq, peer }),
            MsgType::Invoke => {
                let obj_id =
                    obj_id.ok_or(UbusError::Malformed("INVOKE frame without ObjId attr"))?;
                let obj_id =
                    ObjId::new(obj_id).ok_or(UbusError::Malformed("INVOKE frame with ObjId=0"))?;
                let method =
                    method.ok_or(UbusError::Malformed("INVOKE frame without Method attr"))?;
                Ok(Self::Invoke {
                    seq,
                    peer,
                    obj_id,
                    method,
                    data,
                })
            }
            MsgType::Hello => Ok(Self::Hello { seq, peer }),
            ty => Ok(Self::Other { ty, seq, peer }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blobmsg::BlobMsgTable;
    use crate::wire::Codec;

    fn round_trip<'a>(msg: OutboundMsg<'a>, seq: u16) -> Frame {
        let frame = msg.into_frame(seq);
        // Run through the wire codec so we exercise framing + decode.
        let bytes = Codec::write_frame(&frame);
        let (decoded, _) = Codec::read_frame(&bytes).unwrap();
        decoded
    }

    #[test]
    fn invoke_round_trips_through_inbound_parse() {
        let args = BlobMsgTable::build(|b| {
            b.put_str(Some("k"), "v");
        });
        let out = OutboundMsg::Invoke {
            obj_id: ObjId::new(7).unwrap(),
            method: "ping",
            args: &args,
        };
        let frame = round_trip(out, 3);
        match InboundMsg::parse(&frame).unwrap() {
            InboundMsg::Invoke {
                seq,
                peer,
                obj_id,
                method,
                data,
            } => {
                assert_eq!(seq, 3);
                assert_eq!(peer, 7);
                assert_eq!(obj_id.get(), 7);
                assert_eq!(method, "ping");
                assert!(data.is_some());
            }
            other => panic!("expected Invoke, got {other:?}"),
        }
    }

    #[test]
    fn parse_invoke_missing_method_is_malformed() {
        // Hand-craft an INVOKE envelope with ObjId but no Method.
        let mut env = EnvBuilder::new();
        env.push_u32(UbusAttr::ObjId, 1);
        let frame = Frame::to_peer(MsgType::Invoke, 1, 1, env.finish());
        assert!(matches!(
            InboundMsg::parse(&frame),
            Err(UbusError::Malformed("INVOKE frame without Method attr"))
        ));
    }

    #[test]
    fn parse_status_missing_attr_is_malformed() {
        // STATUS envelope without Status attr.
        let env = EnvBuilder::new().finish();
        let frame = Frame::to_peer(MsgType::Status, 1, 1, env);
        assert!(matches!(
            InboundMsg::parse(&frame),
            Err(UbusError::Malformed("STATUS frame without Status attr"))
        ));
    }

    #[test]
    fn parse_status_round_trips_negative_codes() {
        let frame = round_trip(
            OutboundMsg::Status {
                peer: PeerId::new(9).unwrap(),
                obj_id: None,
                status: -3,
            },
            5,
        );
        match InboundMsg::parse(&frame).unwrap() {
            InboundMsg::Status { status, peer, seq } => {
                assert_eq!(status, -3);
                assert_eq!(peer, 9);
                assert_eq!(seq, 5);
            }
            other => panic!("expected Status, got {other:?}"),
        }
    }

    #[test]
    fn parse_hello_does_not_require_envelope() {
        let frame = round_trip(OutboundMsg::Hello, 1);
        assert!(matches!(
            InboundMsg::parse(&frame),
            Ok(InboundMsg::Hello { .. })
        ));
    }

    /// libubus wraps an "empty" HELLO with a 4-byte outer envelope
    /// header (id=0, len=4). Without those 4 bytes upstream `ubusd`'s
    /// recv loop blocks waiting for them and the next outbound header
    /// gets mis-decoded as the missing envelope, desyncing the stream.
    /// Pin the wire shape so a refactor can't quietly revert to a
    /// header-only HELLO.
    #[test]
    fn hello_outbound_carries_empty_envelope() {
        let frame = OutboundMsg::Hello.into_frame(1);
        assert_eq!(frame.hdr.ty, MsgType::Hello);
        assert_eq!(frame.hdr.peer, 0);
        assert_eq!(frame.payload, alloc::vec![0u8, 0, 0, 4]);
    }

    /// Pin the *serialized* HELLO at the 12 bytes upstream `ubusd`
    /// expects (8-byte header + 4-byte empty envelope). A regression
    /// in `Codec::write_frame` that dropped payload on Hello
    /// specifically would slip past the `frame.payload` assertion
    /// above; this pins the wire output.
    #[test]
    fn hello_serialized_wire_is_twelve_bytes() {
        let frame = OutboundMsg::Hello.into_frame(1);
        let bytes = Codec::write_frame(&frame);
        assert_eq!(bytes.len(), 12);
    }

    /// REMOVE_OBJECT must be broker-bound (`peer=0`) per libubox.
    /// Setting `peer=local_id` would address the message back at our
    /// own connection and ubusd would silently drop it.
    #[test]
    fn remove_object_is_broker_bound() {
        let frame = OutboundMsg::RemoveObject {
            obj_id: ObjId::new(7).unwrap(),
        }
        .into_frame(2);
        assert_eq!(frame.hdr.peer, 0);
        assert_eq!(frame.hdr.ty, MsgType::RemoveObject);
    }

    /// Pin the upstream-compat shape: when a server sends STATUS with an
    /// `obj_id`, the encoded envelope must carry both the STATUS and the
    /// OBJID attrs. Upstream `ubusd_handle_response` looks the responder
    /// up by ATTR_OBJID before re-stamping & forwarding the reply to the
    /// caller; missing OBJID makes the broker silently drop the frame
    /// and the caller's `recv_invoke_reply` hangs forever. Order
    /// (OBJID before STATUS) matches upstream libubox emission so a
    /// sequential consumer (monitor printers, shimmed brokers) sees the
    /// canonical layout.
    #[test]
    fn status_with_obj_id_includes_objid_attr_alongside_status() {
        use crate::envelope::{iter_attrs, unwrap, UbusAttr};
        let frame = OutboundMsg::Status {
            peer: PeerId::new(9).unwrap(),
            obj_id: Some(ObjId::new(0x1234_5678).unwrap()),
            status: 0,
        }
        .into_frame(5);
        assert_eq!(frame.hdr.ty, MsgType::Status);
        assert_eq!(frame.hdr.peer, 9);
        let inner = unwrap(&frame.payload).expect("envelope unwrap");
        let ids: alloc::vec::Vec<u8> = iter_attrs(inner).map(|a| a.id).collect();
        assert_eq!(
            ids,
            alloc::vec![UbusAttr::ObjId as u8, UbusAttr::Status as u8],
            "OBJID must precede STATUS to match upstream libubox emission order"
        );
    }

    /// Status with `obj_id: None` must omit the OBJID attr entirely (the
    /// frame is otherwise valid for paths that pre-date object
    /// registration, e.g. handshake-time errors).
    #[test]
    fn status_without_obj_id_omits_objid_attr() {
        use crate::envelope::{find, UbusAttr};
        // Reserved peer-id 1 used for the test — Status replies must
        // address a peer (PeerId is non-zero by construction).
        let frame = OutboundMsg::Status {
            peer: PeerId::new(1).unwrap(),
            obj_id: None,
            status: -1,
        }
        .into_frame(1);
        assert!(
            find(&frame.payload, UbusAttr::ObjId).is_none(),
            "OBJID must not appear when obj_id is None"
        );
        let status = find(&frame.payload, UbusAttr::Status)
            .and_then(|a| a.as_u32())
            .map(|u| u as i32)
            .expect("STATUS attr present");
        assert_eq!(status, -1);
    }
}
