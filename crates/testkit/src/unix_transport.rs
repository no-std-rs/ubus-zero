//! Host-side Unix socket transport used by the compatibility suites.

use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

use ubus::error::UbusError;
use ubus::transport::Transport;
use ubus::wire::{Codec, Frame, MAX_MSG_LEN};

/// A non-blocking Unix-stream transport for integration tests and examples.
pub struct UnixUbusTransport {
    stream: UnixStream,
    rx_buf: Vec<u8>,
}

impl UnixUbusTransport {
    pub fn connect(path: impl AsRef<Path>) -> Result<Self, UbusError> {
        let stream = UnixStream::connect(path).map_err(io_error)?;
        stream.set_nonblocking(true).map_err(io_error)?;
        Ok(Self {
            stream,
            rx_buf: Vec::with_capacity(1024),
        })
    }

    fn fill_rx(&mut self) -> Result<bool, UbusError> {
        const RX_BUF_CAP: usize = MAX_MSG_LEN * 2;
        let mut chunk = [0_u8; 2048];
        loop {
            match self.stream.read(&mut chunk) {
                Ok(0) => return Ok(true),
                Ok(n) => {
                    if self.rx_buf.len().saturating_add(n) > RX_BUF_CAP {
                        return Err(UbusError::Malformed("rx buffer exceeded cap"));
                    }
                    self.rx_buf.extend_from_slice(&chunk[..n]);
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(false),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(io_error(error)),
            }
        }
    }
}

impl Transport for UnixUbusTransport {
    fn send(&mut self, frame: &Frame) -> Result<(), UbusError> {
        let bytes = Codec::write_frame(frame);
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut sent = 0;
        while sent < bytes.len() {
            match self.stream.write(&bytes[sent..]) {
                Ok(0) => return Err(UbusError::Closed),
                Ok(n) => sent += n,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    if Instant::now() >= deadline {
                        return Err(UbusError::Io("send timed out".into()));
                    }
                    thread::sleep(Duration::from_millis(1));
                }
                Err(error) => return Err(io_error(error)),
            }
        }
        Ok(())
    }

    fn recv(&mut self) -> Result<Option<Frame>, UbusError> {
        let closed = match self.fill_rx() {
            Ok(closed) => closed,
            Err(error) => {
                self.rx_buf.clear();
                return Err(error);
            }
        };

        match Codec::read_frame(&self.rx_buf) {
            Ok((frame, consumed)) => {
                self.rx_buf.drain(..consumed);
                Ok(Some(frame))
            }
            Err(UbusError::Short { .. }) if closed => Err(UbusError::Closed),
            Err(UbusError::Short { .. }) => Ok(None),
            Err(error) => {
                self.rx_buf.clear();
                Err(error)
            }
        }
    }

    fn wait_recv(&mut self) {
        thread::sleep(Duration::from_millis(1));
    }
}

fn io_error(error: io::Error) -> UbusError {
    UbusError::Io(error.to_string())
}
