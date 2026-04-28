//! ACP (Agent Client Protocol) provider — feature-gated.
//!
//! Provides a `Send`-safe facade over ACP's `!Send` runtime by confining
//! the protocol to a dedicated OS thread with a single-threaded tokio
//! runtime and `LocalSet`.

pub mod discovery;
pub mod eager;
pub mod install;
pub mod platform;
pub mod provider;
pub mod proxy;
pub mod registry;
pub mod session_import;
pub mod shim;
pub mod state;
pub mod types;

mod client;
mod connection;
