//! Raw Unix-socket peer helpers for SCM_RIGHTS transport tests.

use std::io;
use std::mem::MaybeUninit;
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

use rustix::net::{
    RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags, SendAncillaryBuffer,
    SendAncillaryMessage, SendFlags,
};
use rustix::{
    fd::{BorrowedFd, OwnedFd},
    io::{IoSlice, IoSliceMut},
    pipe::{pipe_with, PipeFlags},
};
use ubus::error::UbusError;
use ubus::wire::{Codec, Frame};

const RECV_SCRATCH_BYTES: usize = 64 * 1024;

/// A raw Unix-stream peer which preserves SCM_RIGHTS ancillary data.
pub struct FdPeer {
    stream: UnixStream,
}

impl FdPeer {
    pub fn connect(path: &Path) -> io::Result<Self> {
        Ok(Self {
            stream: UnixStream::connect(path)?,
        })
    }

    pub fn from_stream(stream: UnixStream) -> Self {
        Self { stream }
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
        let mut control_storage = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(1))];
        let mut control = SendAncillaryBuffer::new(&mut control_storage);
        let rights = fd.map(|fd| [fd]);
        if let Some(rights) = rights.as_ref() {
            if !control.push(SendAncillaryMessage::ScmRights(rights)) {
                return Err(io::Error::other("sendmsg SCM_RIGHTS buffer is too small"));
            }
        }

        let iov = [IoSlice::new(frame_bytes)];
        // SAFETY: `self.stream` owns this raw fd and remains alive throughout
        // the sendmsg call.
        let socket = unsafe { BorrowedFd::borrow_raw(self.stream.as_raw_fd()) };
        let sent = rustix::net::sendmsg(socket, &iov, &mut control, SendFlags::empty())
            .map_err(|e| io::Error::from_raw_os_error(e.raw_os_error()))?;
        if sent != frame_bytes.len() {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                format!("sendmsg wrote {sent} of {} frame bytes", frame_bytes.len()),
            ));
        }
        Ok(())
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

        let iov = [IoSlice::new(frame_bytes)];
        // SAFETY: `self.stream` owns this raw fd and remains alive throughout
        // the sendmsg call.
        let socket = unsafe { BorrowedFd::borrow_raw(self.stream.as_raw_fd()) };
        let sent = rustix::net::sendmsg(socket, &iov, &mut control, SendFlags::empty())
            .map_err(|e| io::Error::from_raw_os_error(e.raw_os_error()))?;
        if sent != frame_bytes.len() {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                format!("sendmsg wrote {sent} of {} frame bytes", frame_bytes.len()),
            ));
        }
        Ok(())
    }

    /// Receive one socket read, appending its bytes and returning its first fd.
    pub fn recv_frame_with_fd(&self, buf: &mut Vec<u8>) -> io::Result<(usize, Option<OwnedFd>)> {
        let mut scratch = [0_u8; RECV_SCRATCH_BYTES];
        let mut iov = [IoSliceMut::new(&mut scratch)];
        let mut control_storage = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(1))];
        let mut control = RecvAncillaryBuffer::new(&mut control_storage);
        // SAFETY: `self.stream` owns this raw fd and remains alive throughout
        // the recvmsg call.
        let socket = unsafe { BorrowedFd::borrow_raw(self.stream.as_raw_fd()) };
        let received =
            rustix::net::recvmsg(socket, &mut iov, &mut control, RecvFlags::CMSG_CLOEXEC)
                .map_err(|e| io::Error::from_raw_os_error(e.raw_os_error()))?;

        let fd = control.drain().find_map(|message| match message {
            RecvAncillaryMessage::ScmRights(mut fds) => fds.next(),
            _ => None,
        });
        buf.extend_from_slice(&scratch[..received.bytes]);
        Ok((received.bytes, fd))
    }

    /// Read through one complete UBUS frame, retaining the first fd observed.
    pub fn recv_full_frame_with_fd(
        &self,
        timeout: Duration,
    ) -> io::Result<(Frame, Option<OwnedFd>)> {
        self.stream.set_read_timeout(Some(timeout))?;

        let mut bytes = Vec::new();
        let mut received_fd = None;
        loop {
            match Codec::read_frame(&bytes) {
                Ok((frame, _)) => return Ok((frame, received_fd)),
                Err(UbusError::Short { .. }) => {}
                Err(e) => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("invalid UBUS frame: {e}"),
                    ));
                }
            }

            let (read, fd) = self.recv_frame_with_fd(&mut bytes)?;
            if received_fd.is_none() {
                received_fd = fd;
            }
            if read == 0 {
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
}
