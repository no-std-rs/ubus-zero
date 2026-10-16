//! Portable, no_std dashboard logic. The firmware supplies async HTTP and SPI.
#![no_std]

extern crate alloc;

pub mod panel;
pub mod render;
pub mod rpc;
pub mod stats;
