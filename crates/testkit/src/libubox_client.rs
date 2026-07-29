//! `LibuboxClient` — a thin synchronous wrapper around libubus's
//! `ubus_invoke` so transport tests can drive a libubox-shaped INVOKE
//! against a Rust-side server through upstream `ubusd`. The existing
//! `_upstream` matrix only exercises ours → broker → ours; the
//! libubox-shaped UBUS_ATTR_DATA bytes are never on the wire there,
//! so a regression in our server's `strip_outer_table` decoder slips
//! the matrix. This fixture closes that gap; [`LibuboxClient::invoke`]
//! covers the no-args shape, and [`LibuboxClient::invoke_with_args`]
//! covers libubox-built blobmsg args under `UBUS_ATTR_DATA`.
//!
//! When `libubox-sys` was built without `.dep/bin/` resolved (the
//! `have_libubox` cfg is unset), [`LibuboxClient::connect`] returns
//! `Ok(None)` so the call site reuses the existing skip shape. If
//! `.dep/bin/libubus.so` is present but the cfg is still unset, the
//! build cache is poisoned and the stub panics instead of allowing a
//! vacuous pass.

#[cfg(not(have_libubox))]
use std::io;
#[cfg(not(have_libubox))]
use std::path::Path;

#[cfg(have_libubox)]
mod inner {
    use std::cell::RefCell;
    use std::ffi::CString;
    use std::io;
    use std::os::fd::RawFd;
    use std::os::unix::ffi::OsStrExt;
    use std::path::Path;
    use std::ptr;

    use libubox_sys as sys;

    use crate::libubox_blob::{libubus_global_lock, BlobBufGuard};

    // Buffer the synchronous `ubus_invoke` reply via a thread-local —
    // libubus calls our `data_cb` from inside the same `ubus_invoke`
    // call, on the same thread, so cell + thread_local is enough; no
    // heap-allocated priv/key required. Keyed on a `Box::into_raw`
    // priv pointer would let multi-threaded callers coexist, but PR1
    // only ever has one in-flight invoke per process.
    thread_local! {
        static REPLY: RefCell<Option<Vec<u8>>> = const { RefCell::new(None) };
    }

    pub struct LibuboxClient {
        ctx: *mut sys::ubus_context,
    }

    impl LibuboxClient {
        pub fn connect(socket_path: &Path) -> io::Result<Option<Self>> {
            let c_path = CString::new(socket_path.as_os_str().as_bytes())?;
            let _libubus = libubus_global_lock();
            let ctx = unsafe { sys::ubus_connect(c_path.as_ptr()) };
            if ctx.is_null() {
                // ENOENT / EACCES / glibc loader issues all surface as
                // a NULL ctx with errno set. We don't reach for errno
                // (would need a thread_local libc dance); the caller
                // gets a plain io::Error and the test layer's
                // spawn_or_skip path handles it.
                return Err(io::Error::other(format!(
                    "libubox ubus_connect({}) returned NULL",
                    socket_path.display()
                )));
            }
            Ok(Some(Self { ctx }))
        }

        /// Synchronous INVOKE with no args. The returned `Vec<u8>` is the
        /// reply blob's `blob_data` — the bytes *inside* the top-level
        /// wrapper attr (the nested attrs / blobmsg contents), without the
        /// wrapper's own 4-byte header. This is the contents-only shape
        /// `tests/ubusd_handshake.rs` decodes against.
        pub fn invoke(&mut self, object_name: &str, method: &str) -> io::Result<Vec<u8>> {
            self.invoke_raw(object_name, method, ptr::null_mut(), -1)
        }

        /// Synchronous INVOKE carrying `fd` through libubus's SCM_RIGHTS
        /// slot. Upstream ubusd relays it to the invoked object's owner.
        ///
        /// Takes ownership: libubus closes any fd it sends
        /// (libubus-io.c `ubus_send_msg`), so a borrowed fd would be
        /// double-closed by the caller's own drop.
        pub fn invoke_with_fd(
            &mut self,
            object_name: &str,
            method: &str,
            fd: rustix::fd::OwnedFd,
        ) -> io::Result<Vec<u8>> {
            use rustix::fd::IntoRawFd;
            self.invoke_raw(object_name, method, ptr::null_mut(), fd.into_raw_fd())
        }

        /// Synchronous INVOKE with string args built by upstream
        /// libubox's blobmsg builder. This drives the args-bearing
        /// libubox wire shape: `UBUS_ATTR_DATA` carries the contents of
        /// the blobmsg table that libubox built, not a Rust-constructed
        /// wrapper.
        pub fn invoke_with_args(
            &mut self,
            object_name: &str,
            method: &str,
            args: &[(&str, &str)],
        ) -> io::Result<Vec<u8>> {
            let mut msg = BlobBufGuard::new()?;
            for (name, value) in args {
                msg.add_string(name, value)?;
            }
            self.invoke_raw(object_name, method, msg.head(), -1)
        }

        fn invoke_raw(
            &mut self,
            object_name: &str,
            method: &str,
            msg: *mut sys::blob_attr,
            fd: RawFd,
        ) -> io::Result<Vec<u8>> {
            let object_c = CString::new(object_name)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
            let method_c =
                CString::new(method).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;

            // libubus builds both lookup and invoke requests in one
            // process-global blob buffer; retain the lock through the
            // synchronous wait so no other fixture can clobber either frame.
            let _libubus = libubus_global_lock();
            let mut id: u32 = 0;
            let rc = unsafe { sys::ubus_lookup_id(self.ctx, object_c.as_ptr(), &mut id) };
            if rc != 0 {
                return Err(io::Error::other(format!(
                    "ubus_lookup_id({object_name}) returned {rc}"
                )));
            }

            REPLY.with(|cell| *cell.borrow_mut() = None);
            let rc = unsafe {
                // `ubus_invoke_fd(..., fd = -1)` is what the inline
                // `ubus_invoke` wrapper expands to in libubus.h.
                sys::ubus_invoke_fd(
                    self.ctx,
                    id,
                    method_c.as_ptr(),
                    msg,
                    Some(data_cb),
                    ptr::null_mut(),
                    sys::UBUS_INVOKE_TIMEOUT_MS,
                    fd,
                )
            };
            if rc != 0 {
                return Err(io::Error::other(format!(
                    "ubus_invoke({object_name}.{method}) returned {rc}"
                )));
            }
            Ok(REPLY.with(|cell| cell.borrow_mut().take().unwrap_or_default()))
        }
    }

    impl Drop for LibuboxClient {
        fn drop(&mut self) {
            if !self.ctx.is_null() {
                let _libubus = libubus_global_lock();
                unsafe { sys::ubus_free(self.ctx) };
                self.ctx = ptr::null_mut();
            }
        }
    }

    /// libubus invokes this on each DATA frame; for `ubus_invoke` we
    /// only ever expect one per call. Copy the blob's `blob_data` (the
    /// nested attrs inside the wrapper, past its header) into the
    /// thread-local so the caller can return it.
    extern "C" fn data_cb(
        _req: *mut sys::ubus_request,
        _ty: core::ffi::c_int,
        msg: *mut sys::blob_attr,
    ) {
        if msg.is_null() {
            return;
        }
        let len = unsafe { sys::blob_len(msg) } as usize;
        let data = unsafe { sys::blob_data(msg) } as *const u8;
        if data.is_null() || len == 0 {
            REPLY.with(|cell| *cell.borrow_mut() = Some(Vec::new()));
            return;
        }
        let slice = unsafe { core::slice::from_raw_parts(data, len) };
        // Force a heap copy: libubus owns `msg` for the duration of
        // the callback only.
        let mut buf = Vec::with_capacity(len);
        buf.extend_from_slice(slice);
        REPLY.with(|cell| *cell.borrow_mut() = Some(buf));
    }
}

#[cfg(have_libubox)]
pub use inner::LibuboxClient;

#[cfg(not(have_libubox))]
pub struct LibuboxClient {
    _never: core::convert::Infallible,
}

#[cfg(not(have_libubox))]
impl LibuboxClient {
    /// Stub for builds without `.dep/bin/` resolved — always `Ok(None)`
    /// so the test layer's skip short-circuit kicks in, unless the
    /// built library is present and Cargo replayed a stale failed
    /// `libubox-sys` probe.
    pub fn connect(_socket_path: &Path) -> io::Result<Option<Self>> {
        if libubus_artifact_exists() {
            panic!(
                "libubox-sys was built without cfg(have_libubox), but .dep/bin/libubus.so exists; \
                 this usually means Cargo replayed a poisoned failed libubox-sys build-script \
                 probe cache. Remedy: touch ubus/libubox-sys/build.rs and rebuild."
            );
        }
        Ok(None)
    }

    pub fn invoke(&mut self, _object_name: &str, _method: &str) -> io::Result<Vec<u8>> {
        match self._never {}
    }

    pub fn invoke_with_fd(
        &mut self,
        _object_name: &str,
        _method: &str,
        _fd: rustix::fd::OwnedFd,
    ) -> io::Result<Vec<u8>> {
        match self._never {}
    }

    pub fn invoke_with_args(
        &mut self,
        _object_name: &str,
        _method: &str,
        _args: &[(&str, &str)],
    ) -> io::Result<Vec<u8>> {
        match self._never {}
    }
}

#[cfg(not(have_libubox))]
fn libubus_artifact_exists() -> bool {
    [
        concat!(env!("CARGO_MANIFEST_DIR"), "/../../.dep/bin/libubus.so"),
        concat!(env!("CARGO_MANIFEST_DIR"), "/../.dep/bin/libubus.so"),
        concat!(env!("CARGO_MANIFEST_DIR"), "/.dep/bin/libubus.so"),
    ]
    .iter()
    .any(|path| Path::new(path).is_file())
}
