//! Unix-stream transport for Linux, Android and macOS, adapted from Cellctl's ubus adapter.
//!
//! Enable `unix` to use this adapter with `no_std + alloc`. Reads are buffered
//! and nonblocking; writes and optional operation deadlines use monotonic time.

use alloc::{format, vec::Vec};
use core::time::Duration;
use rustix::{
    event::{poll, PollFd, PollFlags, Timespec},
    fd::OwnedFd,
    io::Errno,
    net::{RecvFlags, SocketAddrUnix, SocketType},
    time::{clock_gettime, ClockId},
};

use crate::{
    wire::{Codec, Frame, MAX_MSG_LEN},
    Transport, UbusError,
};

/// A close-on-exec, nonblocking Unix stream socket. macOS cannot set both
/// atomically, so it applies them right after creating the socket.
fn stream_socket() -> Result<OwnedFd, Errno> {
    #[cfg(not(target_os = "macos"))]
    {
        rustix::net::socket_with(
            rustix::net::AddressFamily::UNIX,
            SocketType::STREAM,
            rustix::net::SocketFlags::CLOEXEC | rustix::net::SocketFlags::NONBLOCK,
            None,
        )
    }
    #[cfg(target_os = "macos")]
    {
        use rustix::io::{fcntl_getfd, fcntl_setfd, ioctl_fionbio, FdFlags};
        let fd = rustix::net::socket(rustix::net::AddressFamily::UNIX, SocketType::STREAM, None)?;
        fcntl_setfd(&fd, fcntl_getfd(&fd)? | FdFlags::CLOEXEC)?;
        ioctl_fionbio(&fd, true)?;
        rustix::net::sockopt::set_socket_nosigpipe(&fd, true)?;
        Ok(fd)
    }
}

/// An error after a partial send makes this transport unusable. Drop it and
/// reconnect: another frame cannot safely follow the unfinished wire frame.
pub struct UnixUbusTransport {
    fd: OwnedFd,
    rx_buf: Vec<u8>,
    deadline: Option<u128>,
    abandoned_send: bool,
}

impl UnixUbusTransport {
    pub fn connect(path: &str) -> Result<Self, UbusError> {
        let fd = stream_socket().map_err(|e| io_error("socket", e))?;
        let address = SocketAddrUnix::new(path).map_err(|e| io_error("address", e))?;
        rustix::net::connect(&fd, &address).map_err(|e| io_error("connect", e))?;
        Ok(Self {
            fd,
            rx_buf: Vec::with_capacity(1024),
            deadline: None,
            abandoned_send: false,
        })
    }

    /// Limit subsequent operations to a wall-clock duration; `None` clears it.
    /// Reset this before each request when reusing a connection.
    pub fn set_deadline(&mut self, timeout: Option<Duration>) {
        self.deadline = timeout.map(|timeout| now_ns().saturating_add(timeout.as_nanos()));
    }

    fn check_deadline(&self) -> Result<(), UbusError> {
        if self.deadline.is_some_and(|deadline| now_ns() >= deadline) {
            return Err(UbusError::Timeout("transport"));
        }
        Ok(())
    }

    fn park(&self, flags: PollFlags) {
        let remaining = self
            .deadline
            .map(|deadline| deadline.saturating_sub(now_ns()))
            .unwrap_or(50_000_000)
            .min(50_000_000);
        let timeout = Timespec {
            tv_sec: 0,
            tv_nsec: remaining as _,
        };
        let mut fds = [PollFd::new(&self.fd, flags)];
        let _ = poll(&mut fds, Some(&timeout));
    }
}

impl Transport for UnixUbusTransport {
    fn send(&mut self, frame: &Frame) -> Result<(), UbusError> {
        if self.abandoned_send {
            return Err(UbusError::Closed);
        }
        let bytes = Codec::write_frame(frame);
        if bytes.len() > MAX_MSG_LEN + crate::wire::HEADER_SIZE {
            return Err(UbusError::Malformed(
                "outbound frame exceeds maximum length",
            ));
        }
        let deadline = now_ns() + 5_000_000_000;
        let mut sent = 0;
        let result = (|| {
            while sent < bytes.len() {
                self.check_deadline()?;
                match rustix::net::send(&self.fd, &bytes[sent..], super::NO_SIGPIPE) {
                    Ok(0) => return Err(UbusError::Closed),
                    Ok(n) => sent += n,
                    Err(Errno::INTR) => continue,
                    Err(Errno::AGAIN) => {
                        if now_ns() >= deadline {
                            return Err(UbusError::Timeout("send"));
                        }
                        self.park(PollFlags::OUT);
                    }
                    Err(e) => return Err(io_error("send", e)),
                }
            }
            Ok(())
        })();
        self.abandoned_send = result.is_err() && sent != 0;
        result
    }

    fn recv(&mut self) -> Result<Option<Frame>, UbusError> {
        if self.abandoned_send {
            return Err(UbusError::Closed);
        }
        self.check_deadline()?;
        let mut chunk = [0u8; 2048];
        let mut closed = false;
        // Decode as soon as one frame is buffered. A busy producer cannot starve
        // a complete reply by keeping the socket readable indefinitely.
        loop {
            // Socket frames include the outer blob header. The codec's
            // standalone eight-byte-header form is for in-memory users;
            // treating that wire fragment as a frame loses its pending body.
            if self.rx_buf.len() >= crate::wire::HEADER_SIZE + 4 {
                match Codec::read_frame(&self.rx_buf) {
                    Ok((frame, consumed)) => {
                        self.rx_buf.drain(..consumed);
                        return Ok(Some(frame));
                    }
                    Err(UbusError::Short { .. }) => {}
                    Err(error) => {
                        self.rx_buf.clear();
                        return Err(error);
                    }
                }
            }
            if closed {
                return Err(UbusError::Closed);
            }
            match rustix::net::recv(&self.fd, &mut chunk, RecvFlags::empty()) {
                Ok((0, _)) => closed = true,
                Ok((n, _)) => {
                    if self.rx_buf.len().saturating_add(n) > MAX_MSG_LEN * 2 {
                        self.rx_buf.clear();
                        return Err(UbusError::Malformed("rx buffer exceeded cap"));
                    }
                    self.rx_buf.extend_from_slice(&chunk[..n]);
                }
                Err(Errno::AGAIN) => return Ok(None),
                Err(Errno::INTR) => self.check_deadline()?,
                Err(e) => {
                    self.rx_buf.clear();
                    return Err(io_error("recv", e));
                }
            }
        }
    }

    fn wait_recv(&mut self) {
        self.park(PollFlags::IN);
    }
}

fn now_ns() -> u128 {
    let time = clock_gettime(ClockId::Monotonic);
    time.tv_sec as u128 * 1_000_000_000 + time.tv_nsec as u128
}

fn io_error(operation: &str, error: Errno) -> UbusError {
    UbusError::Io(format!("{operation}: errno {}", error.raw_os_error()))
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use crate::{envelope::EnvBuilder, wire::MsgType};
    use rustix::fd::FromRawFd;
    use std::{
        io::{Read, Write},
        os::{fd::IntoRawFd, unix::net::UnixStream},
    };

    fn pair() -> (UnixStream, UnixUbusTransport) {
        let (peer, stream) = UnixStream::pair().unwrap();
        stream.set_nonblocking(true).unwrap();
        // SAFETY: ownership is transferred from the stream exactly once.
        let fd = unsafe { OwnedFd::from_raw_fd(stream.into_raw_fd()) };
        (
            peer,
            UnixUbusTransport {
                fd,
                rx_buf: Vec::new(),
                deadline: None,
                abandoned_send: false,
            },
        )
    }

    #[test]
    fn partial_and_queued_frames_survive_close() {
        let frame = Frame::to_broker(MsgType::Ping, 42, EnvBuilder::new().finish());
        let bytes = Codec::write_frame(&frame);
        for split in 1..bytes.len() {
            let (mut peer, mut transport) = pair();
            peer.write_all(&bytes[..split]).unwrap();
            assert!(transport.recv().unwrap().is_none(), "split at {split}");
            peer.write_all(&bytes[split..]).unwrap();
            peer.write_all(&bytes).unwrap();
            drop(peer);
            assert_eq!(
                transport.recv().unwrap(),
                Some(frame.clone()),
                "split at {split}"
            );
            assert_eq!(
                transport.recv().unwrap(),
                Some(frame.clone()),
                "split at {split}"
            );
            assert!(matches!(transport.recv(), Err(UbusError::Closed)));
        }
    }

    #[test]
    fn truncated_wire_header_never_becomes_a_header_only_reply() {
        let frame = Frame::to_broker(MsgType::Hello, 0, EnvBuilder::new().finish());
        let bytes = Codec::write_frame(&frame);
        for split in 1..bytes.len() {
            let (mut peer, mut transport) = pair();
            peer.write_all(&bytes[..split]).unwrap();
            drop(peer);
            assert!(
                matches!(transport.recv(), Err(UbusError::Closed)),
                "EOF at {split} bytes"
            );
        }
    }

    #[test]
    fn silent_peer_obeys_deadline() {
        let (_peer, mut transport) = pair();
        transport.set_deadline(Some(Duration::ZERO));
        assert!(matches!(
            transport.recv(),
            Err(UbusError::Timeout("transport"))
        ));
    }

    #[test]
    fn partial_send_timeout_blocks_further_io_after_deadline_reset() {
        let (mut peer, mut transport) = pair();
        rustix::net::sockopt::set_socket_send_buffer_size(&transport.fd, 4096).unwrap();
        peer.set_nonblocking(true).unwrap();
        let mut env = EnvBuilder::new();
        env.push_data(&alloc::vec![0x55; 128 * 1024]);
        let frame = Frame::to_broker(MsgType::Ping, 42, env.finish());
        transport.set_deadline(Some(Duration::from_millis(50)));
        assert!(matches!(transport.send(&frame), Err(UbusError::Timeout(_))));

        let mut prefix = Vec::new();
        assert_eq!(
            peer.read_to_end(&mut prefix).unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
        assert!(!prefix.is_empty());
        assert!(prefix.len() < Codec::write_frame(&frame).len());

        transport.set_deadline(None);
        let next = Frame::to_broker(MsgType::Ping, 43, EnvBuilder::new().finish());
        assert!(matches!(transport.send(&next), Err(UbusError::Closed)));
        assert!(matches!(transport.recv(), Err(UbusError::Closed)));
        assert_eq!(
            peer.read(&mut [0; 32]).unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock,
            "a later frame must never be appended to the abandoned prefix"
        );
    }

    #[test]
    fn zero_byte_send_timeout_preserves_connection_for_retry() {
        let (mut peer, mut transport) = pair();
        let frame = Frame::to_broker(MsgType::Ping, 42, EnvBuilder::new().finish());
        transport.set_deadline(Some(Duration::ZERO));
        assert!(matches!(
            transport.send(&frame),
            Err(UbusError::Timeout("transport"))
        ));
        transport.set_deadline(None);
        transport.send(&frame).unwrap();
        let expected = Codec::write_frame(&frame);
        let mut actual = alloc::vec![0; expected.len()];
        peer.read_exact(&mut actual).unwrap();
        assert_eq!(actual, expected);
        peer.write_all(&expected).unwrap();
        assert_eq!(transport.recv().unwrap(), Some(frame));
    }
}
