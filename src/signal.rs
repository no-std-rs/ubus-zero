//! Bounded Linux submission from asynchronous signal handlers.
//!
//! Create and split the queue before installing a handler. A sender validates
//! an already encoded frame, then attempts exactly one nonblocking pipe write
//! of at most Linux's `PIPE_BUF` (4096) bytes. Success enqueues the whole frame;
//! `Full` and `Interrupted` enqueue nothing. There are no locks, allocation,
//! retries, logging, clocks or shared sequence counters in this path.
//!
//! The receiver is a byte stream, not a ubus socket. Drain it in normal code,
//! preserve partial frames, and forward complete frames through one connection
//! owner. Do not share a ubus stream between a handler and normal writes.
//! Registration, sequence assignment and reply handling belong to normal code.
//! A handler must preserve `errno` if its syscall backend uses libc, and any
//! globally published sender must outlive the handler (including in-flight
//! calls). Initialize it before installation and disable/quiesce handlers before
//! dropping it. Callers choose whether to drop or coalesce a `Full` submission.

use rustix::{
    fd::{AsFd, BorrowedFd, OwnedFd},
    io::Errno,
    pipe::{pipe_with, PipeFlags},
};

use crate::wire::{Codec, HEADER_SIZE};

/// Linux's atomic pipe-write limit; includes the full encoded ubus frame.
pub const MAX_SUBMISSION_BYTES: usize = 4096;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SubmitError {
    Full,
    Interrupted,
    FrameTooLarge,
    InvalidFrame,
    Io(Errno),
}

/// Both pipe ends remain owned here so a live sender always has a reader.
/// Construct and destroy this value outside signal handlers.
pub struct SignalQueue {
    read: OwnedFd,
    write: OwnedFd,
}

impl SignalQueue {
    pub fn new() -> Result<Self, Errno> {
        let (read, write) = pipe_with(PipeFlags::NONBLOCK | PipeFlags::CLOEXEC)?;
        Ok(Self { read, write })
    }

    /// One receiver, with cloneable senders. The borrow keeps both ends alive.
    pub fn split(&mut self) -> (SignalSender<'_>, SignalReceiver<'_>) {
        (
            SignalSender {
                write: self.write.as_fd(),
                _reader: self.read.as_fd(),
            },
            SignalReceiver {
                read: self.read.as_fd(),
            },
        )
    }
}

/// A non-owning sender; cloning or dropping it makes no syscalls.
#[derive(Clone, Copy)]
pub struct SignalSender<'a> {
    write: BorrowedFd<'a>,
    _reader: BorrowedFd<'a>,
}

impl SignalSender<'_> {
    /// Submit one complete wire frame, including its outer blob and padding.
    /// No bytes are accepted on `Full`, `Interrupted` or validation failure.
    /// Each concurrent/reentrant caller must supply its own immutable bytes.
    pub fn try_submit(&self, frame: &[u8]) -> Result<(), SubmitError> {
        if frame.len() > MAX_SUBMISSION_BYTES {
            return Err(SubmitError::FrameTooLarge);
        }
        if frame.len() < HEADER_SIZE + 4
            || !matches!(Codec::read_frame_borrowed(frame), Ok((_, n)) if n == frame.len())
        {
            return Err(SubmitError::InvalidFrame);
        }
        match rustix::io::write(self.write, frame) {
            Ok(n) if n == frame.len() => Ok(()),
            // NONBLOCK pipe writes <= PIPE_BUF are atomic. A short write
            // would violate that platform contract; never retry a suffix.
            Ok(_) => Err(SubmitError::Io(Errno::IO)),
            Err(Errno::AGAIN) => Err(SubmitError::Full),
            Err(Errno::INTR) => Err(SubmitError::Interrupted),
            Err(error) => Err(SubmitError::Io(error)),
        }
    }
}

/// The single normal-context consumer. Reads may split or combine frames.
pub struct SignalReceiver<'a> {
    read: BorrowedFd<'a>,
}

impl SignalReceiver<'_> {
    /// Read currently available bytes. Zero means the queue is empty.
    /// Keep incomplete frames in caller storage and retry on interruption.
    pub fn read(&mut self, destination: &mut [u8]) -> Result<usize, Errno> {
        if destination.is_empty() {
            return Err(Errno::INVAL);
        }
        match rustix::io::read(self.read, destination) {
            Err(Errno::AGAIN) => Ok(0),
            result => result,
        }
    }
}

impl AsFd for SignalReceiver<'_> {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.read
    }
}
