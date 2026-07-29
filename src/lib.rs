//! Pure-Rust, `no_std + alloc` building blocks for the OpenWrt ubus protocol.
//!
//! The crate provides a synchronous [`client::UbusClient`], an object provider
//! through [`server::UbusConnection`], compatible wire/blob/blobmsg codecs,
//! and a runtime-neutral [`Transport`] seam. It is a focused protocol
//! implementation, not a source-compatible port of the C `libubus` API.

#![no_std]
#![cfg_attr(test, allow(clippy::unwrap_used))]

extern crate alloc;

pub mod blob;
pub mod blobmsg;
pub mod client;
pub mod envelope;
pub mod error;
pub mod msg;
pub mod server;
pub mod transport;
pub mod wire;

pub use blobmsg::{BlobBuilder, BlobIter, BlobMsgTable, BlobRef, BlobType, BlobValue};
pub use error::UbusError;
pub use msg::{InboundMsg, OutboundMsg};
pub use server::{DispatchOutcome, Method, MethodHandler, UbusObject};
pub use transport::Transport;
pub use wire::{Header, MsgType};

pub type Result<T> = core::result::Result<T, UbusError>;
