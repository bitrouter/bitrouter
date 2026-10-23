#![doc = include_str!("../README.md")]
#![forbid(unsafe_code)]

pub mod control;
#[cfg(feature = "core")]
pub mod core;
pub mod error;
pub mod gateway;
pub mod harness;
pub mod stream;

mod transport;
