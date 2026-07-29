//! UBUS client. [`UbusClient::connect`] performs the HELLO handshake
//! at construction; the resulting handle is ready for `lookup` /
//! `invoke`. The `Data` reply attr's outer TABLE wrapper is stripped
//! before returning so callers can `lookup(reply, "key")` directly.

use alloc::vec::Vec;

use crate::blobmsg::{BlobIter, BlobMsgTable};
use crate::error::UbusError;
use crate::msg::{InboundMsg, OutboundMsg};
use crate::transport::Transport;
use crate::wire::{ObjId, HELLO_NON_HELLO_BUDGET, IDLE_WAIT_BUDGET};

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
        };
        let seq = s.next_seq();
        s.peer_id = Some(do_hello_handshake(&mut s.transport, seq)?);
        Ok(s)
    }

    pub fn peer_id(&self) -> Option<u32> {
        self.peer_id
    }

    pub fn lookup(&mut self, object: &str) -> Result<u32, UbusError> {
        let seq = self.next_seq();
        self.transport
            .send(&OutboundMsg::Lookup { obj_path: object }.into_frame(seq))?;
        let mut found_id: Option<u32> = None;
        let mut data_frame_was_corrupt = false;
        let mut idle_waits = 0u32;
        loop {
            let frame = match self.transport.recv()? {
                Some(f) => f,
                None => {
                    idle_waits += 1;
                    if idle_waits > IDLE_WAIT_BUDGET {
                        return Err(UbusError::Timeout("lookup"));
                    }
                    self.transport.wait_recv();
                    continue;
                }
            };
            if frame.hdr.seq != seq {
                continue;
            }
            match InboundMsg::parse(&frame)? {
                InboundMsg::Data {
                    obj_id: Some(id), ..
                } => {
                    found_id = Some(id);
                }
                InboundMsg::Data { obj_id: None, .. } => data_frame_was_corrupt = true,
                InboundMsg::Status { status, .. } => {
                    if status != 0 {
                        return Err(UbusError::Status(status));
                    }
                    if let Some(id) = found_id {
                        return Ok(id);
                    }
                    return Err(UbusError::Malformed(if data_frame_was_corrupt {
                        "LOOKUP DATA frame missing ObjId attr"
                    } else {
                        "LOOKUP succeeded without ObjId in DATA"
                    }));
                }
                _ => continue,
            }
        }
    }

    pub fn send_invoke(
        &mut self,
        object_id: u32,
        method: &str,
        args: &BlobMsgTable,
    ) -> Result<u16, UbusError> {
        let obj_id =
            ObjId::new(object_id).ok_or(UbusError::Malformed("invoke target obj_id is zero"))?;
        let seq = self.next_seq();
        self.transport.send(
            &OutboundMsg::Invoke {
                obj_id,
                method,
                args,
            }
            .into_frame(seq),
        )?;
        Ok(seq)
    }

    /// On STATUS=0 returns the *contents* of the handler's reply
    /// TABLE (outer wrapper stripped). Returns `Err(Status)` for
    /// non-zero STATUS and `Err(Malformed)` if the broker claimed
    /// success but DATA was missing or unparseable.
    pub fn recv_invoke_reply(&mut self, seq: u16) -> Result<Vec<u8>, UbusError> {
        let mut data: Option<Vec<u8>> = None;
        let mut data_frame_was_corrupt = false;
        let mut idle_waits = 0u32;
        loop {
            let frame = match self.transport.recv()? {
                Some(f) if f.hdr.seq == seq => f,
                Some(_) => continue,
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
        let s = self.seq;
        self.seq = self.seq.wrapping_add(1);
        s
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
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::envelope::{EnvBuilder, UbusAttr};
    use crate::transport::MockTransport;
    use crate::wire::{Frame, MsgType};

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
            .send(&Frame::to_peer(MsgType::Data, seq, 0, bad.finish()))
            .unwrap();
        // Then STATUS=0 — broker claims success.
        let mut ok = EnvBuilder::new();
        ok.push_i32(UbusAttr::Status, 0);
        srv_t
            .send(&Frame::to_peer(MsgType::Status, seq, 0, ok.finish()))
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
