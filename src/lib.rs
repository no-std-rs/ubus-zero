//! Pure-Rust, `no_std` building blocks for the OpenWrt ubus protocol.
//! Borrowed codecs and fixed buffers work without an allocator.
//!
//! With the default `alloc` feature, the crate provides a synchronous `client::UbusClient`, an object provider
//! through `server::UbusConnection`, compatible wire/blob/blobmsg codecs,
//! and a runtime-neutral `Transport` seam. It is a focused protocol
//! implementation, not a source-compatible port of the C `libubus` API.

#![no_std]
#![cfg_attr(test, allow(clippy::unwrap_used))]

#[cfg(feature = "alloc")]
extern crate alloc;

#[doc = include_str!("../docs/bounded-io.md")]
pub mod bounded;

pub mod blob;
pub mod blobmsg;
#[cfg(feature = "alloc")]
pub mod client;
pub mod envelope;
pub mod error;
#[cfg(feature = "alloc")]
pub mod msg;
#[cfg(feature = "alloc")]
pub mod server;
#[cfg(feature = "alloc")]
pub mod transport;
pub mod wire;

#[cfg(feature = "json")]
pub mod json;
#[cfg(all(
    any(feature = "unix", feature = "signal"),
    any(target_os = "linux", target_os = "android")
))]
pub mod unix;

#[cfg(feature = "alloc")]
pub use blobmsg::{BlobBuilder, BlobMsgTable};
pub use blobmsg::{BlobIter, BlobRef, BlobType, BlobValue};
pub use error::{BufferError, UbusError};
#[cfg(feature = "alloc")]
pub use msg::{InboundMsg, OutboundMsg};
#[cfg(feature = "alloc")]
pub use server::{DispatchOutcome, Method, MethodHandler, UbusObject};
#[cfg(feature = "alloc")]
pub use transport::Transport;
pub use wire::{Header, MsgType};

#[cfg(all(feature = "signal", any(target_os = "linux", target_os = "android")))]
pub mod signal;

pub type Result<T> = core::result::Result<T, UbusError>;
