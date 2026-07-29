//! Standalone broker binary: `ubusd-rs --socket /var/run/ubus.sock`.
//!
//! Signal handlers write tags to a self-pipe: TERM and INT make `run()` return
//! cleanly so `Drop` unlinks the socket and the process exits 0, while HUP
//! reloads ACLs. Handlers are installed only over SIG_DFL, preserving custom
//! dispositions and inherited SIG_IGN just as uloop does.

use std::io;
use std::process::ExitCode;
use std::sync::atomic::{AtomicI32, Ordering};

use rustix::fd::{FromRawFd, IntoRawFd, OwnedFd};
use ubusd_rs::{Broker, Config};

static WAKE_WRITE_FD: AtomicI32 = AtomicI32::new(-1);

/// Suppress SIGPIPE process-wide: a peer disconnect mid-send must
/// surface as `EPIPE` from `send(2)`, not kill the daemon. Linux gets
/// the same effect via `MSG_NOSIGNAL`; macOS has no per-write flag for
/// AF_UNIX so the process-wide ignore is the portable belt-and-braces.
fn ignore_sigpipe() {
    #[allow(non_camel_case_types)]
    type sighandler_t = usize;
    extern "C" {
        fn signal(signum: i32, handler: sighandler_t) -> sighandler_t;
    }
    const SIGPIPE: i32 = 13;
    const SIG_IGN: sighandler_t = 1;
    // SAFETY: `signal(2)` with SIG_IGN is async-signal-safe and has no
    // memory or thread-safety requirements; only effect is the kernel
    // disposition for the signal.
    unsafe {
        signal(SIGPIPE, SIG_IGN);
    }
}

extern "C" fn on_signal(sig: libc::c_int) {
    let tag = match sig {
        libc::SIGTERM => b'T',
        libc::SIGINT => b'I',
        libc::SIGHUP => b'H',
        _ => return,
    };
    // SAFETY: `__errno_location` and `write(2)` are async-signal-safe. The
    // pipe fd is set before handlers are installed and remains open to exit.
    unsafe {
        let errno = libc::__errno_location();
        let saved_errno = *errno;
        let fd = WAKE_WRITE_FD.load(Ordering::Relaxed);
        if fd >= 0 {
            let _ = libc::write(fd, &tag as *const u8 as *const libc::c_void, 1);
        }
        *errno = saved_errno;
    }
}

fn install_signal_handler(sig: libc::c_int) -> io::Result<()> {
    // SAFETY: each sigaction structure is fully initialized before use; the
    // handler has the one-argument C signal-handler ABI represented by
    // `sa_sigaction` on supported Unix targets.
    unsafe {
        let mut old: libc::sigaction = std::mem::zeroed();
        if libc::sigaction(sig, std::ptr::null(), &mut old) != 0 {
            return Err(io::Error::last_os_error());
        }
        if old.sa_sigaction != libc::SIG_DFL {
            return Ok(());
        }
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = on_signal as *const () as usize;
        action.sa_flags = 0;
        libc::sigemptyset(&mut action.sa_mask);
        if libc::sigaction(sig, &action, std::ptr::null_mut()) != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

fn setup_wake_pipe() -> Option<OwnedFd> {
    let mut fds = [-1; 2];
    // SAFETY: `fds` is a valid two-element output array. On success both
    // descriptors are owned here and immediately wrapped in `OwnedFd`.
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) } != 0 {
        eprintln!(
            "ubusd-rs: create signal wake pipe: {}",
            io::Error::last_os_error()
        );
        return None;
    }
    // SAFETY: successful pipe2 returned live, uniquely owned descriptors.
    let read_end = unsafe { OwnedFd::from_raw_fd(fds[0]) };
    // SAFETY: successful pipe2 returned live, uniquely owned descriptors.
    let write_end = unsafe { OwnedFd::from_raw_fd(fds[1]) };
    WAKE_WRITE_FD.store(write_end.into_raw_fd(), Ordering::Relaxed);

    for sig in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP] {
        if let Err(e) = install_signal_handler(sig) {
            eprintln!("ubusd-rs: install signal handler for {sig}: {e}");
        }
    }
    Some(read_end)
}

fn main() -> ExitCode {
    ignore_sigpipe();
    let wake_pipe = setup_wake_pipe();
    let mut cfg = Config::default();
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        if let Some(v) = a.strip_prefix("--socket=") {
            cfg.socket_path = v.into();
        } else if let Some(v) = a.strip_prefix("--acl-dir=") {
            cfg.acl_dir = Some(v.into());
        } else if a == "--acl-dir" || a == "-A" {
            match args.next() {
                Some(v) => cfg.acl_dir = Some(v),
                None => {
                    eprintln!("ubusd-rs: {a} requires a value");
                    return ExitCode::from(64);
                }
            }
        } else if a == "--socket" || a == "-s" {
            match args.next() {
                Some(v) => cfg.socket_path = v,
                None => {
                    eprintln!("ubusd-rs: --socket requires a value");
                    return ExitCode::from(64);
                }
            }
        } else if a == "--help" || a == "-h" {
            println!(
                "usage: ubusd-rs [--socket=/var/run/ubus.sock] [-A <acl-dir>|--acl-dir=<acl-dir>]\n\
                 \n\
                 Pure-Rust ubusd reimplementation. Wire surface:\n\
                 HELLO / ADD_OBJECT / REMOVE_OBJECT / LOOKUP / INVOKE /\n\
                 SUBSCRIBE / UNSUBSCRIBE / NOTIFY / PING."
            );
            return ExitCode::SUCCESS;
        } else {
            eprintln!("ubusd-rs: unknown arg: {a}");
            return ExitCode::from(64);
        }
    }

    let mut broker = match Broker::bind(cfg.clone()) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("ubusd-rs: bind {}: {}", cfg.socket_path, e);
            return ExitCode::from(2);
        }
    };
    if let Some(wake_pipe) = wake_pipe {
        broker.set_wake_pipe(wake_pipe);
    }
    eprintln!("ubusd-rs: listening on {}", cfg.socket_path);
    if let Err(e) = broker.run() {
        eprintln!("ubusd-rs: fatal: {e}");
        return ExitCode::from(1);
    }
    ExitCode::SUCCESS
}
