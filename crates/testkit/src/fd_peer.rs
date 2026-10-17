//! Raw Unix-socket peer helpers for SCM_RIGHTS transport tests.

use std::cell::RefCell;
use std::io;
use std::mem::MaybeUninit;
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::{Duration, Instant};

use rustix::net::{
    RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags, SendAncillaryBuffer,
    SendAncillaryMessage, SendFlags,
};
use rustix::{
    fd::{AsRawFd as _, BorrowedFd, OwnedFd},
    io::{IoSlice, IoSliceMut},
    pipe::{pipe_with, PipeFlags},
};
use ubus::error::UbusError;
use ubus::wire::{Codec, Frame};

const RECV_SCRATCH_BYTES: usize = 64 * 1024;

/// A raw Unix-stream peer which preserves SCM_RIGHTS ancillary data.
pub struct FdPeer {
    stream: UnixStream,
    pending: RefCell<PendingFrame>,
}

#[derive(Default)]
struct PendingFrame {
    bytes: Vec<u8>,
    fd: Option<OwnedFd>,
}

impl FdPeer {
    pub fn connect(path: &Path) -> io::Result<Self> {
        Ok(Self {
            stream: UnixStream::connect(path)?,
            pending: RefCell::default(),
        })
    }

    pub fn from_stream(stream: UnixStream) -> Self {
        Self {
            stream,
            pending: RefCell::default(),
        }
    }

    pub fn raw_fd(&self) -> RawFd {
        self.stream.as_raw_fd()
    }

    /// Send one frame and, optionally, one file descriptor in SCM_RIGHTS.
    pub fn send_frame_with_fd(
        &self,
        frame_bytes: &[u8],
        fd: Option<BorrowedFd<'_>>,
    ) -> io::Result<()> {
        let fds: Vec<_> = fd.into_iter().map(|fd| fd.as_raw_fd()).collect();
        self.send_frame_with_fds(frame_bytes, &fds)
    }

    /// Send one frame with every supplied descriptor in one SCM_RIGHTS cmsg.
    pub fn send_frame_with_fds(&self, frame_bytes: &[u8], fds: &[RawFd]) -> io::Result<()> {
        // Linux accepts at most 253 SCM_RIGHTS descriptors in one message.
        const MAX_RIGHTS: usize = 253;
        if fds.len() > MAX_RIGHTS {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("SCM_RIGHTS supports at most {MAX_RIGHTS} fds per message"),
            ));
        }
        let mut control_storage =
            [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(MAX_RIGHTS))];
        let mut control = SendAncillaryBuffer::new(&mut control_storage);
        let rights: Vec<_> = fds
            .iter()
            .map(|fd| {
                // SAFETY: callers supply borrowed descriptors and this method
                // uses them only for the duration of sendmsg.
                unsafe { BorrowedFd::borrow_raw(*fd) }
            })
            .collect();
        if !rights.is_empty() && !control.push(SendAncillaryMessage::ScmRights(&rights)) {
            return Err(io::Error::other("sendmsg SCM_RIGHTS buffer is too small"));
        }

        // Once any byte is sent, SCM_RIGHTS belongs to that first byte.
        // A short write must send the remaining bytes without duplicating it.
        // SAFETY: the stream owns the socket for every sendmsg call below.
        let socket = unsafe { BorrowedFd::borrow_raw(self.stream.as_raw_fd()) };
        let mut offset = 0;
        while offset < frame_bytes.len() {
            let iov = [IoSlice::new(&frame_bytes[offset..])];
            let sent = match rustix::net::sendmsg(socket, &iov, &mut control, SendFlags::empty()) {
                Ok(sent) => sent,
                Err(rustix::io::Errno::INTR) => continue,
                Err(e) => return Err(io::Error::from_raw_os_error(e.raw_os_error())),
            };
            if sent == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "sendmsg wrote no bytes",
                ));
            }
            offset += sent;
            control.clear();
        }
        Ok(())
    }

    /// Receive one socket read, appending its bytes and returning its first fd.
    pub fn recv_frame_with_fd(&self, buf: &mut Vec<u8>) -> io::Result<(usize, Option<OwnedFd>)> {
        self.recv_bytes_with_fd(buf, RECV_SCRATCH_BYTES)
    }

    fn recv_bytes_with_fd(
        &self,
        buf: &mut Vec<u8>,
        limit: usize,
    ) -> io::Result<(usize, Option<OwnedFd>)> {
        let mut scratch = [0_u8; RECV_SCRATCH_BYTES];
        let mut iov = [IoSliceMut::new(
            &mut scratch[..limit.min(RECV_SCRATCH_BYTES)],
        )];
        let mut control_storage = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(1))];
        let mut control = RecvAncillaryBuffer::new(&mut control_storage);
        // SAFETY: `self.stream` owns this raw fd and remains alive throughout
        // the recvmsg call.
        let socket = unsafe { BorrowedFd::borrow_raw(self.stream.as_raw_fd()) };
        let received =
            rustix::net::recvmsg(socket, &mut iov, &mut control, RecvFlags::CMSG_CLOEXEC)
                .map_err(|e| io::Error::from_raw_os_error(e.raw_os_error()))?;

        let mut fd = None;
        for message in control.drain() {
            if let RecvAncillaryMessage::ScmRights(fds) = message {
                for received in fds {
                    if fd.is_none() {
                        fd = Some(received);
                    }
                    // Every additional received descriptor closes here.
                }
            }
        }
        buf.extend_from_slice(&scratch[..received.bytes]);
        Ok((received.bytes, fd))
    }

    /// Receive exactly one UBUS frame and its first SCM_RIGHTS descriptor.
    ///
    /// Reads stop at the frame boundary, even when several frames are already
    /// queued in the stream. This prevents assigning the next frame's fd to
    /// this one or discarding its bytes. Incomplete frames survive a timeout.
    pub fn recv_full_frame_with_fd(
        &self,
        timeout: Duration,
    ) -> io::Result<(Frame, Option<OwnedFd>)> {
        let deadline = Instant::now() + timeout;
        let mut pending = self.pending.borrow_mut();
        loop {
            // All broker frames include an outer blob_attr, including HELLO.
            // Avoid the codec's header-only fallback at exactly eight bytes.
            let need = if pending.bytes.len() < 12 {
                12
            } else {
                match Codec::read_frame(&pending.bytes) {
                    Ok((frame, consumed)) => {
                        assert_eq!(consumed, pending.bytes.len());
                        pending.bytes.clear();
                        return Ok((frame, pending.fd.take()));
                    }
                    Err(UbusError::Short { need, .. }) => need,
                    Err(e) => {
                        *pending = PendingFrame::default();
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            format!("invalid UBUS frame: {e}"),
                        ));
                    }
                }
            };
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "UBUS frame deadline",
                ));
            }
            self.stream.set_read_timeout(Some(remaining))?;
            let limit = need - pending.bytes.len();
            let (read, fd) = match self.recv_bytes_with_fd(&mut pending.bytes, limit) {
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                    ) =>
                {
                    return Err(e);
                }
                Err(e) => {
                    *pending = PendingFrame::default();
                    return Err(e);
                }
                Ok(received) => received,
            };
            if pending.fd.is_none() {
                pending.fd = fd;
            }
            if read == 0 {
                *pending = PendingFrame::default();
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "socket closed before a complete UBUS frame",
                ));
            }
        }
    }
}

/// Create the read and write ends of a pipe for SCM_RIGHTS round-trip tests.
pub fn pipe() -> io::Result<(OwnedFd, OwnedFd)> {
    pipe_with(PipeFlags::CLOEXEC).map_err(|e| io::Error::from_raw_os_error(e.raw_os_error()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustix::fd::AsFd;
    use rustix::io::{fcntl_getfd, FdFlags};

    #[test]
    fn pipe_ends_are_closed_on_exec() -> io::Result<()> {
        let (read_end, write_end) = pipe()?;

        for fd in [&read_end, &write_end] {
            let flags =
                fcntl_getfd(fd).map_err(|e| io::Error::from_raw_os_error(e.raw_os_error()))?;
            assert!(flags.contains(FdFlags::CLOEXEC));
        }
        Ok(())
    }

    #[test]
    fn passes_pipe_fd_over_socketpair() -> io::Result<()> {
        let (sender, receiver) = UnixStream::pair()?;
        let sender = FdPeer::from_stream(sender);
        let receiver = FdPeer::from_stream(receiver);
        let (read_end, write_end) = pipe()?;

        rustix::io::write(&write_end, b"x")
            .map_err(|e| io::Error::from_raw_os_error(e.raw_os_error()))?;
        sender.send_frame_with_fd(b"frame", Some(read_end.as_fd()))?;

        let mut bytes = Vec::new();
        let (read, received_fd) = receiver.recv_frame_with_fd(&mut bytes)?;
        assert_eq!(read, b"frame".len());
        assert_eq!(bytes, b"frame");
        let received_fd = received_fd.expect("SCM_RIGHTS fd");

        let mut marker = [0_u8; 1];
        let read = rustix::io::read(&received_fd, &mut marker)
            .map_err(|e| io::Error::from_raw_os_error(e.raw_os_error()))?;
        assert_eq!(read, 1);
        assert_eq!(marker, [b'x']);
        Ok(())
    }

    fn frame(seq: u16) -> Frame {
        Frame::to_broker(
            ubus::wire::MsgType::Status,
            seq,
            ubus::envelope::EnvBuilder::new().finish(),
        )
    }

    #[test]
    fn coalesced_frames_are_not_discarded_or_given_the_next_frames_fd() -> io::Result<()> {
        let (sender, receiver) = UnixStream::pair()?;
        let sender = FdPeer::from_stream(sender);
        let receiver = FdPeer::from_stream(receiver);
        let (read_end, write_end) = pipe()?;
        sender.send_frame_with_fd(&Codec::write_frame(&frame(1)), None)?;
        sender.send_frame_with_fd(&Codec::write_frame(&frame(2)), Some(write_end.as_fd()))?;
        sender.send_frame_with_fd(&Codec::write_frame(&frame(3)), None)?;

        for seq in 1..=3 {
            let (received, fd) = receiver.recv_full_frame_with_fd(Duration::from_secs(1))?;
            assert_eq!(received.hdr.seq, seq);
            assert_eq!(fd.is_some(), seq == 2, "descriptor belongs to frame {seq}");
            if let Some(fd) = fd {
                rustix::io::write(&fd, b"b")
                    .map_err(|e| io::Error::from_raw_os_error(e.raw_os_error()))?;
                let mut marker = [0];
                rustix::io::read(&read_end, &mut marker)
                    .map_err(|e| io::Error::from_raw_os_error(e.raw_os_error()))?;
                assert_eq!(marker, *b"b");
                assert!(fcntl_getfd(&fd)
                    .map_err(|e| io::Error::from_raw_os_error(e.raw_os_error()))?
                    .contains(FdFlags::CLOEXEC));
            }
        }
        Ok(())
    }

    #[test]
    fn incomplete_frame_and_fd_survive_a_read_timeout() -> io::Result<()> {
        let (sender, receiver) = UnixStream::pair()?;
        let sender = FdPeer::from_stream(sender);
        let receiver = FdPeer::from_stream(receiver);
        let (read_end, write_end) = pipe()?;
        let bytes = Codec::write_frame(&frame(4));
        sender.send_frame_with_fd(&bytes[..7], Some(write_end.as_fd()))?;
        let error = receiver
            .recv_full_frame_with_fd(Duration::from_millis(10))
            .unwrap_err();
        assert!(matches!(
            error.kind(),
            io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
        ));
        sender.send_frame_with_fd(&bytes[7..], None)?;
        let (received, fd) = receiver.recv_full_frame_with_fd(Duration::from_secs(1))?;
        assert_eq!(received.hdr.seq, 4);
        rustix::io::write(fd.expect("retained fd"), b"t")
            .map_err(|e| io::Error::from_raw_os_error(e.raw_os_error()))?;
        let mut marker = [0];
        rustix::io::read(&read_end, &mut marker)
            .map_err(|e| io::Error::from_raw_os_error(e.raw_os_error()))?;
        assert_eq!(marker, *b"t");
        Ok(())
    }

    #[test]
    fn incomplete_frame_eof_closes_the_retained_fd() -> io::Result<()> {
        let (sender, receiver) = UnixStream::pair()?;
        let sender = FdPeer::from_stream(sender);
        let receiver = FdPeer::from_stream(receiver);
        let (read_end, write_end) = pipe()?;
        sender.send_frame_with_fd(&Codec::write_frame(&frame(5))[..1], Some(write_end.as_fd()))?;
        drop(write_end);
        drop(sender);
        let error = receiver
            .recv_full_frame_with_fd(Duration::from_secs(1))
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof);
        rustix::fs::fcntl_setfl(&read_end, rustix::fs::OFlags::NONBLOCK)
            .map_err(|e| io::Error::from_raw_os_error(e.raw_os_error()))?;
        let mut marker = [0];
        assert_eq!(
            rustix::io::read(&read_end, &mut marker)
                .map_err(|e| io::Error::from_raw_os_error(e.raw_os_error()))?,
            0
        );
        Ok(())
    }

    #[test]
    fn invalid_frame_closes_its_received_fd() -> io::Result<()> {
        let (sender, receiver) = UnixStream::pair()?;
        let sender = FdPeer::from_stream(sender);
        let receiver = FdPeer::from_stream(receiver);
        let (read_end, write_end) = pipe()?;
        let mut bytes = Codec::write_frame(&frame(6));
        bytes[8..12].copy_from_slice(&3_u32.to_be_bytes());
        sender.send_frame_with_fd(&bytes, Some(write_end.as_fd()))?;
        drop(write_end);
        let error = receiver
            .recv_full_frame_with_fd(Duration::from_secs(1))
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        rustix::fs::fcntl_setfl(&read_end, rustix::fs::OFlags::NONBLOCK)
            .map_err(|e| io::Error::from_raw_os_error(e.raw_os_error()))?;
        let mut marker = [0];
        assert_eq!(
            rustix::io::read(&read_end, &mut marker)
                .map_err(|e| io::Error::from_raw_os_error(e.raw_os_error()))?,
            0
        );
        Ok(())
    }

    #[test]
    fn extra_received_descriptors_are_closed() -> io::Result<()> {
        let (sender, receiver) = UnixStream::pair()?;
        let sender = FdPeer::from_stream(sender);
        let receiver = FdPeer::from_stream(receiver);
        let mut readers = Vec::new();
        let mut writers = Vec::new();
        for _ in 0..3 {
            let (read_end, write_end) = pipe()?;
            readers.push(read_end);
            writers.push(write_end);
        }
        let rights: Vec<_> = writers.iter().map(|fd| fd.as_raw_fd()).collect();
        sender.send_frame_with_fds(&Codec::write_frame(&frame(7)), &rights)?;
        drop(writers);
        let (_, received) = receiver.recv_full_frame_with_fd(Duration::from_secs(1))?;
        assert!(received.is_some());
        drop(received);
        for read_end in readers {
            rustix::fs::fcntl_setfl(&read_end, rustix::fs::OFlags::NONBLOCK)
                .map_err(|e| io::Error::from_raw_os_error(e.raw_os_error()))?;
            let mut marker = [0];
            assert_eq!(
                rustix::io::read(&read_end, &mut marker)
                    .map_err(|e| io::Error::from_raw_os_error(e.raw_os_error()))?,
                0
            );
        }
        Ok(())
    }

    #[test]
    fn reset_after_partial_frame_closes_the_retained_fd() -> io::Result<()> {
        let (sender, receiver) = UnixStream::pair()?;
        let sender = FdPeer::from_stream(sender);
        let receiver = FdPeer::from_stream(receiver);
        let (read_end, write_end) = pipe()?;
        sender.send_frame_with_fd(&Codec::write_frame(&frame(8))[..1], Some(write_end.as_fd()))?;
        drop(write_end);
        let error = receiver
            .recv_full_frame_with_fd(Duration::from_millis(10))
            .unwrap_err();
        assert!(matches!(
            error.kind(),
            io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
        ));
        // Closing a Unix socket with unread inbound data produces a reset.
        receiver.send_frame_with_fd(b"unread", None)?;
        drop(sender);
        let error = receiver
            .recv_full_frame_with_fd(Duration::from_secs(1))
            .unwrap_err();
        assert!(matches!(
            error.kind(),
            io::ErrorKind::ConnectionReset | io::ErrorKind::UnexpectedEof
        ));
        // Reset-related disposal can become visible after recvmsg returns.
        // Wait for pipe readiness, then require actual EOF below. A leaked
        // writer still times out rather than being accepted as closed.
        let mut ready = [rustix::event::PollFd::new(
            &read_end,
            rustix::event::PollFlags::IN,
        )];
        let timeout = rustix::event::Timespec {
            tv_sec: 1,
            tv_nsec: 0,
        };
        assert_eq!(
            rustix::event::poll(&mut ready, Some(&timeout))
                .map_err(|e| io::Error::from_raw_os_error(e.raw_os_error()))?,
            1,
            "retained descriptor did not close after socket reset"
        );
        rustix::fs::fcntl_setfl(&read_end, rustix::fs::OFlags::NONBLOCK)
            .map_err(|e| io::Error::from_raw_os_error(e.raw_os_error()))?;
        let mut marker = [0];
        assert_eq!(
            rustix::io::read(&read_end, &mut marker)
                .map_err(|e| io::Error::from_raw_os_error(e.raw_os_error()))?,
            0
        );
        Ok(())
    }
}
