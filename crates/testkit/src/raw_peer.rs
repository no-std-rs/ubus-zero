use std::io;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};

use crate::path::unique_socket_path;

pub struct RawPeer {
    listener: UnixListener,
    path: PathBuf,
}

impl RawPeer {
    pub fn bind() -> io::Result<Self> {
        Self::bind_labelled("raw_peer")
    }

    pub fn bind_labelled(label: &str) -> io::Result<Self> {
        let path = unique_socket_path(label);
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path)?;
        Ok(Self { listener, path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Tests should call `connect` on the client transport *before*
    /// this — the listener backlog holds the pending connect until
    /// `accept` is called.
    pub fn accept(&self) -> io::Result<UnixStream> {
        let (stream, _) = self.listener.accept()?;
        Ok(stream)
    }
}

impl Drop for RawPeer {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}
