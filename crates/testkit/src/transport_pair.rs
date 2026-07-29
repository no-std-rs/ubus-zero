use std::io;

use rustix::fd::OwnedFd;
use rustix::net::{AddressFamily, SocketFlags, SocketType};

pub struct TransportPair {
    a: OwnedFd,
    b: OwnedFd,
}

impl TransportPair {
    pub fn new() -> io::Result<Self> {
        let (a, b) = open_socketpair()?;
        Ok(Self { a, b })
    }

    pub fn a(&self) -> &OwnedFd {
        &self.a
    }

    pub fn b(&self) -> &OwnedFd {
        &self.b
    }

    pub fn split(self) -> (OwnedFd, OwnedFd) {
        (self.a, self.b)
    }
}

fn io_err(e: rustix::io::Errno) -> io::Error {
    io::Error::from_raw_os_error(e.raw_os_error())
}

#[cfg(not(target_os = "macos"))]
fn open_socketpair() -> io::Result<(OwnedFd, OwnedFd)> {
    rustix::net::socketpair(
        AddressFamily::UNIX,
        SocketType::STREAM,
        SocketFlags::CLOEXEC | SocketFlags::NONBLOCK,
        None,
    )
    .map_err(io_err)
}

#[cfg(target_os = "macos")]
fn open_socketpair() -> io::Result<(OwnedFd, OwnedFd)> {
    use rustix::fd::AsFd;
    use rustix::io::{fcntl_getfd, fcntl_setfd, ioctl_fionbio, FdFlags};
    let (a, b) = rustix::net::socketpair(
        AddressFamily::UNIX,
        SocketType::STREAM,
        SocketFlags::empty(),
        None,
    )
    .map_err(io_err)?;
    for fd in [a.as_fd(), b.as_fd()] {
        let flags = fcntl_getfd(fd).map_err(io_err)?;
        fcntl_setfd(fd, flags | FdFlags::CLOEXEC).map_err(io_err)?;
        ioctl_fionbio(fd, true).map_err(io_err)?;
    }
    Ok((a, b))
}
