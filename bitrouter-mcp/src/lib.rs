//! MCP types and traits for BitRouter.
//!
//! This is a lightweight protocol library containing error definitions,
//! server traits, and MCP-specific types. Configuration types live in
//! [`bitrouter_core::routers::upstream`].
//!
//! Enable the `client` feature to get runtime upstream connection and
//! registry components.
#[cfg(feature = "client")]
pub mod client;
pub mod error;
pub mod server;
#[cfg(feature = "client")]
pub mod transports;
pub mod types;
