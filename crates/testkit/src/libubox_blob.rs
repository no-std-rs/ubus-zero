//! Small RAII wrapper for libubox's opaque `blob_buf`.

use std::ffi::CString;
use std::io;
use std::sync::{Mutex, MutexGuard, PoisonError};

use libubox_sys as sys;

/// Serializes all libubus calls in this process.
///
/// The pinned `libubus.c` declares its outgoing `struct blob_buf b` at line
/// 43 as process-global storage, so concurrent contexts corrupt each other's
/// requests. Holding this across a synchronous wait is intentional: fixture
/// scenarios pair one libubus fixture with in-tree counterparts, which never
/// acquire this lock.
pub(crate) static LIBUBUS_GLOBAL: Mutex<()> = Mutex::new(());

pub(crate) fn libubus_global_lock() -> MutexGuard<'static, ()> {
    LIBUBUS_GLOBAL
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
}

pub(crate) struct BlobBufGuard {
    buf: sys::blob_buf,
}

impl BlobBufGuard {
    pub(crate) fn new() -> io::Result<Self> {
        // SAFETY: `blob_buf` is opaque Rust storage. Zeroed storage is the
        // initial state expected by `blob_buf_init`, which initializes it
        // before any other operation observes it.
        let mut buf = unsafe { core::mem::MaybeUninit::<sys::blob_buf>::zeroed().assume_init() };
        // SAFETY: `buf` is valid writable storage for libubox's blob buffer.
        let rc = unsafe { sys::blob_buf_init(&mut buf, 0) };
        if rc != 0 {
            return Err(io::Error::other(format!("blob_buf_init returned {rc}")));
        }
        Ok(Self { buf })
    }

    pub(crate) fn add_string(&mut self, name: &str, value: &str) -> io::Result<()> {
        let name_c =
            CString::new(name).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
        let value_c =
            CString::new(value).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
        // SAFETY: both C strings and the initialized `blob_buf` stay valid
        // throughout this immediate libubox call.
        let rc =
            unsafe { sys::blobmsg_add_string(&mut self.buf, name_c.as_ptr(), value_c.as_ptr()) };
        if rc != 0 {
            return Err(io::Error::other(format!(
                "blobmsg_add_string({name}) returned {rc}"
            )));
        }
        Ok(())
    }

    #[allow(dead_code)] // Used by host-role scenarios as they are added.
    pub(crate) fn add_u32(&mut self, name: &str, value: u32) -> io::Result<()> {
        let name_c =
            CString::new(name).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
        // SAFETY: `name_c` and initialized `self.buf` are valid for this
        // immediate call; libubox copies the field into the buffer.
        let rc = unsafe { sys::blobmsg_add_u32(&mut self.buf, name_c.as_ptr(), value) };
        if rc != 0 {
            return Err(io::Error::other(format!(
                "blobmsg_add_u32({name}) returned {rc}"
            )));
        }
        Ok(())
    }

    pub(crate) fn head(&self) -> *mut sys::blob_attr {
        // SAFETY: `self.buf` remains initialized and owned by this guard.
        unsafe { sys::blob_buf_head(&self.buf) }
    }
}

impl Drop for BlobBufGuard {
    fn drop(&mut self) {
        // SAFETY: `blob_buf_init` succeeded before this guard was created,
        // and this is its unique owner.
        unsafe { sys::blob_buf_free(&mut self.buf) };
    }
}
