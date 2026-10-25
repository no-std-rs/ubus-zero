//! Unix socket adapters for Linux, Android and macOS. [`StreamSender`] sends caller-owned encoded frames
//! without an allocator, preserving offsets over partial writes and EAGAIN.
//! The allocating client transport is available with the `unix` feature.

#[cfg(feature = "unix")]
mod allocated;
#[cfg(feature = "unix")]
pub use allocated::UnixUbusTransport;

use crate::wire::{Codec, HEADER_SIZE};
use rustix::{fd::BorrowedFd, io::Errno, net::SendFlags};

/// Sends never raise SIGPIPE. macOS has no MSG_NOSIGNAL; there sockets carry
/// SO_NOSIGPIPE instead (see [`StreamSender::new`]).
#[cfg(not(target_os = "macos"))]
pub(crate) const NO_SIGPIPE: SendFlags = SendFlags::NOSIGNAL;
#[cfg(target_os = "macos")]
pub(crate) const NO_SIGPIPE: SendFlags = SendFlags::empty();

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StartError {
    InvalidFrame,
    AbandonedFrame,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SendProgress {
    Pending,
    Complete,
}

/// A serialized, allocation-free sender over an already connected stream.
///
/// This object must be the connection's only writer. Finish each pending frame
/// before starting the next. Creation/connection/HELLO and replies remain the
/// caller's responsibility. This is intended for the normal queue consumer,
/// not reentrant access from a signal handler.
pub struct StreamSender<'fd> {
    fd: BorrowedFd<'fd>,
    poisoned: bool,
    /// Why SO_NOSIGPIPE could not be set. Sends then fail with this error
    /// instead of risking SIGPIPE.
    #[cfg(target_os = "macos")]
    nosigpipe_error: Option<Errno>,
}

impl<'fd> StreamSender<'fd> {
    /// On macOS this sets SO_NOSIGPIPE on the caller's socket, since sends there
    /// cannot suppress SIGPIPE per call. If that fails (Darwin returns EINVAL
    /// once the peer has closed), every [`PendingSend::advance`] returns the
    /// error without sending.
    pub fn new(fd: BorrowedFd<'fd>) -> Self {
        Self {
            fd,
            poisoned: false,
            #[cfg(target_os = "macos")]
            nosigpipe_error: rustix::net::sockopt::set_socket_nosigpipe(fd, true).err(),
        }
    }

    pub fn start<'s, 'bytes>(
        &'s mut self,
        bytes: &'bytes [u8],
    ) -> Result<PendingSend<'s, 'fd, 'bytes>, StartError> {
        if self.poisoned {
            return Err(StartError::AbandonedFrame);
        }
        if bytes.len() < HEADER_SIZE + 4
            || !matches!(Codec::read_frame_borrowed(bytes), Ok((_, n)) if n == bytes.len())
        {
            return Err(StartError::InvalidFrame);
        }
        Ok(PendingSend {
            sender: self,
            bytes,
            sent: 0,
        })
    }
}

/// Holds the sender exclusively until completion or abandonment. Dropping
/// after a partial write poisons it: a subsequent frame cannot corrupt the
/// unfinished frame. Close/reconnect the socket after such abandonment.
pub struct PendingSend<'s, 'fd, 'bytes> {
    sender: &'s mut StreamSender<'fd>,
    bytes: &'bytes [u8],
    sent: usize,
}

impl PendingSend<'_, '_, '_> {
    /// One nonblocking, SIGPIPE-suppressed syscall. EAGAIN/EINTR leave the
    /// offset intact; poll for writability or retry later. No hidden retries.
    pub fn advance(&mut self) -> Result<SendProgress, Errno> {
        if self.sent == self.bytes.len() {
            return Ok(SendProgress::Complete);
        }
        #[cfg(target_os = "macos")]
        if let Some(error) = self.sender.nosigpipe_error {
            return Err(error);
        }
        match rustix::net::send(
            self.sender.fd,
            &self.bytes[self.sent..],
            SendFlags::DONTWAIT | NO_SIGPIPE,
        )? {
            0 => Err(Errno::PIPE),
            n => {
                self.sent += n;
                Ok(if self.sent == self.bytes.len() {
                    SendProgress::Complete
                } else {
                    SendProgress::Pending
                })
            }
        }
    }
}

impl Drop for PendingSend<'_, '_, '_> {
    fn drop(&mut self) {
        if self.sent != 0 && self.sent != self.bytes.len() {
            self.sender.poisoned = true;
        }
    }
}

#[cfg(all(test, target_os = "macos", feature = "alloc"))]
mod tests {
    use super::*;
    use crate::envelope::EnvBuilder;
    use crate::wire::{Frame, MsgType};
    use rustix::fd::AsFd;

    #[test]
    fn sender_on_a_closed_peer_reports_an_error_without_sigpipe() {
        let (stream, peer) = rustix::net::socketpair(
            rustix::net::AddressFamily::UNIX,
            rustix::net::SocketType::STREAM,
            rustix::net::SocketFlags::empty(),
            None,
        )
        .unwrap();
        drop(peer);
        let bytes = Codec::write_frame(&Frame::to_broker(
            MsgType::Ping,
            10,
            EnvBuilder::new().finish(),
        ));
        // SIGPIPE would kill the test process here; whether SO_NOSIGPIPE
        // was rejected (EINVAL) or applied (EPIPE), the send must fail.
        let mut sender = StreamSender::new(stream.as_fd());
        let mut pending = sender.start(&bytes).unwrap();
        assert!(matches!(pending.advance(), Err(Errno::INVAL | Errno::PIPE)));
    }
}
