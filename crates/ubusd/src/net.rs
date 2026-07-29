//! Atomic close-on-exec, non-blocking Unix socket setup for the broker.

use rustix::fd::{AsFd, OwnedFd};
use rustix::io::Errno;
use rustix::net::{AddressFamily, SocketType};

pub(crate) fn unix_stream_socket() -> Result<OwnedFd, Errno> {
    #[cfg(not(target_os = "macos"))]
    {
        use rustix::net::SocketFlags;
        rustix::net::socket_with(
            AddressFamily::UNIX,
            SocketType::STREAM,
            SocketFlags::CLOEXEC | SocketFlags::NONBLOCK,
            None,
        )
    }
    #[cfg(target_os = "macos")]
    {
        let fd = rustix::net::socket(AddressFamily::UNIX, SocketType::STREAM, None)?;
        set_cloexec_nonblock(&fd)?;
        Ok(fd)
    }
}

pub(crate) fn accept_nonblock_cloexec<Fd: AsFd>(listener: Fd) -> Result<OwnedFd, Errno> {
    #[cfg(not(target_os = "macos"))]
    {
        use rustix::net::SocketFlags;
        rustix::net::accept_with(listener, SocketFlags::NONBLOCK | SocketFlags::CLOEXEC)
    }
    #[cfg(target_os = "macos")]
    {
        let fd = rustix::net::accept(listener)?;
        set_cloexec_nonblock(&fd)?;
        Ok(fd)
    }
}

#[cfg(target_os = "macos")]
fn set_cloexec_nonblock<Fd: AsFd>(fd: Fd) -> Result<(), Errno> {
    use rustix::io::{fcntl_getfd, fcntl_setfd, ioctl_fionbio, FdFlags};
    let flags = fcntl_getfd(&fd)?;
    fcntl_setfd(&fd, flags | FdFlags::CLOEXEC)?;
    ioctl_fionbio(&fd, true)?;
    Ok(())
}
