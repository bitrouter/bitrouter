//! ACP (Agent Client Protocol) integration for BitRouter.
//!
//! Provides a `Send`-safe facade over ACP's `!Send` runtime by confining the
//! protocol to a dedicated OS thread with a single-threaded tokio runtime and
//! `LocalSet`. Also bundles agent discovery, registry fetch, install/extract,
//! the routing shim, on-disk install state, and the stdio proxy used by
//! editor integrations.

pub mod discovery;
pub mod eager;
pub mod install;
pub mod ops;
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
