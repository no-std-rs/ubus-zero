//! Synchronous framed-byte transport seam. Implementors return
//! `Err(Closed)` on peer hang-up; `recv()` returns `Ok(None)` when no
//! full frame is buffered.

use alloc::sync::Arc;
use alloc::vec::Vec;
use spin::Mutex;

use crate::error::UbusError;
use crate::wire::{Codec, Frame};

pub trait Transport {
    /// Implementations must loop on partial writes before returning.
    fn send(&mut self, frame: &Frame) -> Result<(), UbusError>;

    /// `Ok(None)` when no full frame is buffered without blocking.
    fn recv(&mut self) -> Result<Option<Frame>, UbusError>;

    /// Called by the client/server handshake loops between non-blocking
    /// `recv()` polls. Default is a no-op; real I/O implementations
    /// should park (`poll`/`epoll`/yield) so the loops don't pin a CPU
    /// while waiting for the broker.
    fn wait_recv(&mut self) {}
}

pub struct SharedBuf {
    state: Mutex<SharedBufState>,
}

struct SharedBufState {
    buf: Vec<u8>,
    closed: bool,
}

impl SharedBuf {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(SharedBufState {
                buf: Vec::new(),
                closed: false,
            }),
        }
    }
    /// No-op once the buffer is closed — appending after close is a
    /// programming error on the writer side, not a runtime error to
    /// recover from. Public mostly so test fixtures can drive each end.
    pub fn append(&self, data: &[u8]) {
        let mut g = self.state.lock();
        if g.closed {
            return;
        }
        g.buf.extend_from_slice(data);
    }
    pub fn close(&self) {
        self.state.lock().closed = true;
    }
    pub fn is_closed(&self) -> bool {
        self.state.lock().closed
    }
    /// Parse one frame from the buffered bytes, draining only on success.
    /// Returns `Err(Closed)` if the peer hung up and the residual bytes
    /// can never grow into a full frame.
    pub fn try_read_frame(&self) -> Result<Option<(Frame, usize)>, UbusError> {
        let mut g = self.state.lock();
        match Codec::read_frame(&g.buf) {
            Ok((frame, n)) => {
                g.buf.drain(..n);
                Ok(Some((frame, n)))
            }
            Err(UbusError::Short { .. }) if g.closed => Err(UbusError::Closed),
            Err(UbusError::Short { .. }) => Ok(None),
            Err(e) => Err(e),
        }
    }
}

impl Default for SharedBuf {
    fn default() -> Self {
        Self::new()
    }
}

pub struct MockTransport {
    pub tx: Arc<SharedBuf>,
    pub rx: Arc<SharedBuf>,
}

impl MockTransport {
    /// `(a, b)` — `a`'s writes are readable on `b` and vice versa.
    pub fn pair() -> (Self, Self) {
        let ab = Arc::new(SharedBuf::new());
        let ba = Arc::new(SharedBuf::new());
        (
            Self {
                tx: ab.clone(),
                rx: ba.clone(),
            },
            Self { tx: ba, rx: ab },
        )
    }
}

impl Transport for MockTransport {
    fn send(&mut self, frame: &Frame) -> Result<(), UbusError> {
        if self.tx.is_closed() {
            return Err(UbusError::Closed);
        }
        let bytes = Codec::write_frame(frame);
        self.tx.append(&bytes);
        Ok(())
    }

    fn recv(&mut self) -> Result<Option<Frame>, UbusError> {
        Ok(self.rx.try_read_frame()?.map(|(f, _)| f))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::{Frame, MsgType};

    #[test]
    fn closed_peer_clean_buffer_returns_closed() {
        let (mut a, _b) = MockTransport::pair();
        // _b drops without writing anything. Close both halves directly
        // through `a`'s endpoints to mark hangup with no buffered bytes.
        a.rx.close();
        a.tx.close();
        assert!(matches!(a.recv(), Err(UbusError::Closed)));
    }

    #[test]
    fn closed_peer_with_partial_frame_returns_closed() {
        let (mut a, _b) = MockTransport::pair();
        // Partial header (4 of 8 bytes), then peer closes — the parser
        // can't grow these into a frame so the second `recv()` after
        // close must surface `Closed` rather than hang on `Ok(None)`.
        a.rx.append(&[0u8; 4]);
        a.rx.close();
        assert!(matches!(a.recv(), Err(UbusError::Closed)));
    }

    #[test]
    fn append_after_close_is_silently_ignored() {
        let buf = SharedBuf::new();
        buf.close();
        buf.append(b"late");
        // Reader sees no bytes (and the close), so it surfaces Closed.
        assert!(matches!(buf.try_read_frame(), Err(UbusError::Closed)));
    }

    #[test]
    fn open_buffer_partial_frame_returns_none() {
        let (mut a, _b) = MockTransport::pair();
        a.rx.append(&[0u8; 4]);
        // Not closed: parser should wait for more bytes, not error.
        assert!(matches!(a.recv(), Ok(None)));
    }

    #[test]
    fn full_frame_round_trips_through_pair() {
        let (mut a, mut b) = MockTransport::pair();
        let f = Frame::to_broker(MsgType::Hello, 7, alloc::vec::Vec::new());
        b.send(&f).unwrap();
        let got = a.recv().unwrap().expect("frame buffered");
        assert_eq!(got.hdr.ty, MsgType::Hello);
        assert_eq!(got.hdr.seq, 7);
    }
}
