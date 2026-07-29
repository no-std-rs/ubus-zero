//! Hand-rolled FFI for the subset of `libubox` / `libubus` that
//! `ubus-testkit::LibuboxClient` needs to drive a libubox-shaped
//! INVOKE through upstream `ubusd`. No bindgen — matches workspace
//! precedent (`mediatek/`, `nolibc-rt/`); 10 symbols don't justify
//! the dep tree.
//!
//! When the build script can't find `.dep/bin/{libubox,libubus}.so`
//! and `.dep/bin/include/`, `cfg(have_libubox)` is unset and the FFI
//! is omitted. `cargo check --workspace` on a fresh clone (before
//! `tools/ubusd-builder/build.sh`) then still passes.

#![allow(non_camel_case_types)]

include!(concat!(env!("OUT_DIR"), "/sizes.rs"));

/// `true` when `libubox-sys` was built with `.dep/bin/{libubox,libubus}.so`
/// and headers reachable. Tests can short-circuit on `!AVAILABLE` to
/// skip libubox-shaped arms gracefully.
pub const AVAILABLE: bool = cfg!(have_libubox);

/// `UBUS_ATTR_DATA` from `libubus.h` — the envelope attribute that
/// carries the blobmsg payload on INVOKE / DATA frames.
pub const UBUS_ATTR_DATA: u32 = 4;

/// Default per-call timeout for synchronous invokes from the testkit.
/// Five seconds leaves headroom for parallel-suite contention.
pub const UBUS_INVOKE_TIMEOUT_MS: i32 = 5_000;

pub mod json;

#[cfg(have_libubox)]
mod ffi {
    use super::{BLOB_ATTR_SIZE, BLOB_BUF_SIZE, UBUS_CONTEXT_SIZE};
    use core::ffi::{c_char, c_int, c_uint, c_void};

    // Opaque storage with explicit alignment. We never inspect the
    // bytes from Rust — every field access goes through C accessors.
    // `_align` forces a worst-case 8-byte alignment so libubox's
    // pointer-sized members land on naturally aligned offsets.
    #[repr(C)]
    pub struct ubus_context {
        _align: [u64; 0],
        _data: [u8; UBUS_CONTEXT_SIZE],
    }

    #[repr(C)]
    pub struct blob_buf {
        _align: [u64; 0],
        _data: [u8; BLOB_BUF_SIZE],
    }

    #[repr(C)]
    pub struct blob_attr {
        _align: [u64; 0],
        _data: [u8; BLOB_ATTR_SIZE],
    }

    pub type ubus_data_handler_t =
        Option<unsafe extern "C" fn(req: *mut ubus_request, ty: c_int, msg: *mut blob_attr)>;

    /// `struct ubus_request` — only fields we touch are `priv` (private
    /// callback context, written before invoke) and `data_cb` (response
    /// callback). The struct is much larger than what we model; we
    /// allocate it on the C side via `calloc` of the size libubus
    /// declares, but for a synchronous `ubus_invoke` we let libubus
    /// stack-allocate it internally and pass us the data via callback —
    /// so we never need this struct laid out in Rust storage. The opaque
    /// pointer below is sufficient.
    #[repr(C)]
    pub struct ubus_request {
        _opaque: [u8; 0],
    }

    /// Server-role libubus types contain intrusive AVL/list nodes. C owns
    /// their layout, allocation, and destruction; Rust retains only pointers.
    #[repr(C)]
    pub struct ubus_object {
        _opaque: [u8; 0],
    }

    #[repr(C)]
    pub struct ubus_subscriber {
        _opaque: [u8; 0],
    }

    #[repr(C)]
    pub struct ubus_event_handler {
        _opaque: [u8; 0],
    }

    /// Transient request data supplied to a server method callback. Deferred
    /// replies use the C helper to clone and later free this opaque value.
    #[repr(C)]
    pub struct ubus_request_data {
        _opaque: [u8; 0],
    }

    pub type ubus_zero_testkit_method_cb = Option<
        unsafe extern "C" fn(
            rust_ctx: *mut c_void,
            method: *const c_char,
            msg: *mut blob_attr,
            req: *mut ubus_request_data,
        ) -> c_int,
    >;
    pub type ubus_zero_testkit_notify_cb = Option<
        unsafe extern "C" fn(
            rust_ctx: *mut c_void,
            method: *const c_char,
            msg: *mut blob_attr,
        ) -> c_int,
    >;
    pub type ubus_zero_testkit_remove_cb =
        Option<unsafe extern "C" fn(rust_ctx: *mut c_void, id: u32)>;
    pub type ubus_zero_testkit_event_cb =
        Option<unsafe extern "C" fn(rust_ctx: *mut c_void, ty: *const c_char, msg: *mut blob_attr)>;

    extern "C" {
        // libubox/blob.h
        pub fn blob_buf_init(buf: *mut blob_buf, id: c_int) -> c_int;
        pub fn blob_buf_free(buf: *mut blob_buf);

        // libubox/blobmsg.h — `blobmsg_add_string` is `static inline`;
        // `accessors.c` re-exports it under the `ubus_zero_` prefix.
        #[link_name = "ubus_zero_blobmsg_add_string"]
        pub fn blobmsg_add_string(
            buf: *mut blob_buf,
            name: *const c_char,
            string: *const c_char,
        ) -> c_int;
        pub fn blobmsg_add_u32(buf: *mut blob_buf, name: *const c_char, val: u32) -> c_int;

        // libubox/blob.h — `blob_data` / `blob_len` are `static inline`,
        // and `blob_buf.head` is hidden behind opaque Rust storage.
        // `accessors.c` re-exports them under the `ubus_zero_blob_*`
        // prefix, which is what we declare here.
        #[link_name = "ubus_zero_blob_data"]
        pub fn blob_data(attr: *const blob_attr) -> *mut c_void;
        #[link_name = "ubus_zero_blob_len"]
        pub fn blob_len(attr: *const blob_attr) -> c_uint;
        #[link_name = "ubus_zero_blob_buf_head"]
        pub fn blob_buf_head(buf: *const blob_buf) -> *mut blob_attr;

        // libubus.h
        pub fn ubus_connect(path: *const c_char) -> *mut ubus_context;
        pub fn ubus_free(ctx: *mut ubus_context);
        pub fn ubus_lookup_id(ctx: *mut ubus_context, path: *const c_char, id: *mut u32) -> c_int;
        pub fn ubus_add_object(ctx: *mut ubus_context, obj: *mut ubus_object) -> c_int;
        pub fn ubus_remove_object(ctx: *mut ubus_context, obj: *mut ubus_object) -> c_int;
        pub fn ubus_register_subscriber(ctx: *mut ubus_context, sub: *mut ubus_subscriber)
            -> c_int;
        // Both subscription operations are exported by the pinned libubus.
        pub fn ubus_subscribe(ctx: *mut ubus_context, sub: *mut ubus_subscriber, id: u32) -> c_int;
        pub fn ubus_unsubscribe(
            ctx: *mut ubus_context,
            sub: *mut ubus_subscriber,
            id: u32,
        ) -> c_int;
        // `timeout < 0` means no subscriber replies are expected.
        pub fn ubus_notify(
            ctx: *mut ubus_context,
            obj: *mut ubus_object,
            ty: *const c_char,
            msg: *mut blob_attr,
            timeout: c_int,
        ) -> c_int;
        pub fn ubus_send_event(
            ctx: *mut ubus_context,
            id: *const c_char,
            data: *mut blob_attr,
        ) -> c_int;
        pub fn ubus_register_event_handler(
            ctx: *mut ubus_context,
            ev: *mut ubus_event_handler,
            pattern: *const c_char,
        ) -> c_int;
        pub fn ubus_send_reply(
            ctx: *mut ubus_context,
            req: *mut ubus_request_data,
            msg: *mut blob_attr,
        ) -> c_int;

        // C-owned constructors for the intrusive server-role libubus types.
        pub fn ubus_zero_testkit_object_create(
            name: *const c_char,
            methods: *const *const c_char,
            n_methods: c_int,
            cb: ubus_zero_testkit_method_cb,
            rust_ctx: *mut c_void,
        ) -> *mut ubus_object;
        pub fn ubus_zero_testkit_object_free(obj: *mut ubus_object);
        pub fn ubus_zero_testkit_subscriber_create(
            cb: ubus_zero_testkit_notify_cb,
            remove_cb: ubus_zero_testkit_remove_cb,
            rust_ctx: *mut c_void,
        ) -> *mut ubus_subscriber;
        pub fn ubus_zero_testkit_subscriber_free(sub: *mut ubus_subscriber);
        pub fn ubus_zero_testkit_event_handler_create(
            cb: ubus_zero_testkit_event_cb,
            rust_ctx: *mut c_void,
        ) -> *mut ubus_event_handler;
        pub fn ubus_zero_testkit_event_handler_free(ev: *mut ubus_event_handler);

        // Header inlines plus the otherwise-hidden `ctx->sock.fd`.
        pub fn ubus_zero_ubus_handle_event(ctx: *mut ubus_context);
        pub fn ubus_zero_ubus_ctx_fd(ctx: *const ubus_context) -> c_int;
        pub fn ubus_zero_ubus_unregister_event_handler(
            ctx: *mut ubus_context,
            ev: *mut ubus_event_handler,
        ) -> c_int;
        pub fn ubus_zero_ubus_unregister_subscriber(
            ctx: *mut ubus_context,
            sub: *mut ubus_subscriber,
        ) -> c_int;
        pub fn ubus_zero_ubus_request_set_fd(
            ctx: *mut ubus_context,
            req: *mut ubus_request_data,
            fd: c_int,
        );
        pub fn ubus_zero_ubus_request_get_caller_fd(req: *mut ubus_request_data) -> c_int;
        pub fn ubus_zero_testkit_defer_request(
            ctx: *mut ubus_context,
            req: *mut ubus_request_data,
        ) -> *mut ubus_request_data;
        pub fn ubus_zero_testkit_complete_deferred(
            ctx: *mut ubus_context,
            req: *mut ubus_request_data,
            ret: c_int,
        );

        // `ubus_invoke` is a `static inline` wrapper over `ubus_invoke_fd`
        // with `fd = -1`. Bind directly to the underlying real symbol so
        // we don't need a second C shim.
        pub fn ubus_invoke_fd(
            ctx: *mut ubus_context,
            obj: u32,
            method: *const c_char,
            msg: *mut blob_attr,
            cb: ubus_data_handler_t,
            priv_: *mut c_void,
            timeout: c_int,
            fd: c_int,
        ) -> c_int;
    }

    #[cfg(have_blobmsg_json)]
    extern "C" {
        // libubox/blobmsg_json.h — real export.
        pub fn blobmsg_add_json_from_string(buf: *mut blob_buf, s: *const c_char) -> bool;
        // `blobmsg_format_json` is `static inline`; accessors.c re-exports it.
        #[link_name = "ubus_zero_blobmsg_format_json"]
        pub fn blobmsg_format_json(attr: *mut blob_attr, list: bool) -> *mut c_char;
        // Frees the malloc'd string returned by `blobmsg_format_json`.
        pub fn ubus_zero_free(ptr: *mut c_void);
    }
}

#[cfg(have_libubox)]
pub use ffi::*;
