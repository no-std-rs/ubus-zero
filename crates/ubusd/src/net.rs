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
        // macOS has no MSG_NOSIGNAL, so the socket itself must suppress
        // SIGPIPE: an embedding host may keep the default disposition.
        // These failures concern this connection only (Darwin rejects socket
        // options with EINVAL once the peer has closed), so report them as an
        // aborted connection, which the broker drops before continuing.
        set_cloexec_nonblock(&fd)
            .and_then(|()| rustix::net::sockopt::set_socket_nosigpipe(&fd, true))
            .map_err(|_| Errno::CONNABORTED)?;
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

/// Control buffer for receiving one SCM_RIGHTS descriptor. Linux truncates
/// longer messages and closes the extra descriptors. Darwin reports the
/// untruncated `cmsg_len`, which rustix 1.1 trusts while draining; it also
/// rejects control data above MCLBYTES (2048) at send time, so a buffer of
/// that size can never be truncated there.
#[cfg(not(target_os = "macos"))]
pub(crate) const RIGHTS_CONTROL: usize = rustix::cmsg_space!(ScmRights(1));
#[cfg(target_os = "macos")]
pub(crate) const RIGHTS_CONTROL: usize = 2048;

/// Flags that receive SCM_RIGHTS descriptors close-on-exec. macOS has no
/// MSG_CMSG_CLOEXEC; there [`mark_received`] sets the flag right after
/// recvmsg, which only matters if another thread forks in between.
#[cfg(not(target_os = "macos"))]
pub(crate) const RECV_CLOEXEC: rustix::net::RecvFlags = rustix::net::RecvFlags::CMSG_CLOEXEC;
#[cfg(target_os = "macos")]
pub(crate) const RECV_CLOEXEC: rustix::net::RecvFlags = rustix::net::RecvFlags::empty();

/// Make a descriptor received with [`RECV_CLOEXEC`] close-on-exec.
#[cfg_attr(not(target_os = "macos"), allow(clippy::unnecessary_wraps))]
pub(crate) fn mark_received<Fd: AsFd>(fd: Fd) -> Result<(), Errno> {
    #[cfg(target_os = "macos")]
    {
        use rustix::io::{fcntl_getfd, fcntl_setfd, FdFlags};
        let flags = fcntl_getfd(&fd)?;
        fcntl_setfd(&fd, flags | FdFlags::CLOEXEC)?;
    }
    #[cfg(not(target_os = "macos"))]
    let _ = fd;
    Ok(())
}

/// Close-on-exec Unix stream pair for tests, optionally nonblocking.
#[cfg(test)]
pub(crate) fn test_socketpair(nonblock: bool) -> (OwnedFd, OwnedFd) {
    #[cfg(not(target_os = "macos"))]
    {
        use rustix::net::SocketFlags;
        let flags = if nonblock {
            SocketFlags::CLOEXEC | SocketFlags::NONBLOCK
        } else {
            SocketFlags::CLOEXEC
        };
        rustix::net::socketpair(AddressFamily::UNIX, SocketType::STREAM, flags, None)
            .expect("socketpair")
    }
    #[cfg(target_os = "macos")]
    {
        use rustix::io::{fcntl_getfd, fcntl_setfd, ioctl_fionbio, FdFlags};
        let (a, b) = rustix::net::socketpair(
            AddressFamily::UNIX,
            SocketType::STREAM,
            rustix::net::SocketFlags::empty(),
            None,
        )
        .expect("socketpair");
        for fd in [&a, &b] {
            fcntl_setfd(fd, fcntl_getfd(fd).unwrap() | FdFlags::CLOEXEC).unwrap();
            ioctl_fionbio(fd, nonblock).unwrap();
            // Darwin's Unix stream buffers default to 8 KiB; tests size their
            // bursts for Linux's larger defaults.
            rustix::net::sockopt::set_socket_send_buffer_size(fd, 256 * 1024).unwrap();
            rustix::net::sockopt::set_socket_recv_buffer_size(fd, 256 * 1024).unwrap();
        }
        (a, b)
    }
}

/// Nonblocking pipe for tests.
#[cfg(test)]
pub(crate) fn test_pipe_nonblock() -> (OwnedFd, OwnedFd) {
    #[cfg(not(target_os = "macos"))]
    return rustix::pipe::pipe_with(rustix::pipe::PipeFlags::NONBLOCK).unwrap();
    #[cfg(target_os = "macos")]
    {
        let (read, write) = rustix::pipe::pipe().unwrap();
        rustix::io::ioctl_fionbio(&read, true).unwrap();
        rustix::io::ioctl_fionbio(&write, true).unwrap();
        (read, write)
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;

    #[test]
    fn accepted_sockets_suppress_sigpipe() {
        let path = std::env::temp_dir().join(format!("ubusd-net-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let address = rustix::net::SocketAddrUnix::new(path.to_str().unwrap()).unwrap();
        let listener = unix_stream_socket().unwrap();
        rustix::net::bind(&listener, &address).unwrap();
        rustix::net::listen(&listener, 1).unwrap();
        let client = rustix::net::socket(AddressFamily::UNIX, SocketType::STREAM, None).unwrap();
        rustix::net::connect(&client, &address).unwrap();
        let accepted = accept_nonblock_cloexec(&listener).unwrap();
        let _ = std::fs::remove_file(&path);
        assert!(rustix::net::sockopt::socket_nosigpipe(&accepted).unwrap());
        let flags = rustix::io::fcntl_getfd(&accepted).unwrap();
        assert!(flags.contains(rustix::io::FdFlags::CLOEXEC));
    }
}
