//! Model execution and shared input preparation.

pub(crate) mod capabilities;
pub mod client;
pub mod conversion;
pub mod decision;
pub mod diagnostics;
pub mod error;
#[cfg(any(feature = "core", feature = "harness"))]
pub(crate) mod history;
pub mod protocol;
pub mod stream;
pub mod target;
