//! Linux socket adapters. [`StreamSender`] sends caller-owned encoded frames
//! without an allocator, preserving offsets over partial writes and EAGAIN.
//! The allocating client transport is available with the `unix` feature.

#[cfg(feature = "unix")]
mod allocated;
#[cfg(feature = "unix")]
pub use allocated::UnixUbusTransport;

use crate::wire::{Codec, HEADER_SIZE};
use rustix::{fd::BorrowedFd, io::Errno, net::SendFlags};

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
}

impl<'fd> StreamSender<'fd> {
    pub fn new(fd: BorrowedFd<'fd>) -> Self {
        Self {
            fd,
            poisoned: false,
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
        match rustix::net::send(
            self.sender.fd,
            &self.bytes[self.sent..],
            SendFlags::DONTWAIT | SendFlags::NOSIGNAL,
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
