//! MCP types and traits for BitRouter.
//!
//! This is a lightweight protocol library containing error definitions,
//! server traits, and MCP-specific types. Configuration types live in
//! [`bitrouter_core::routers::upstream`].
//!
//! Enable the `client-stdio` feature for child-process MCP connections.
pub mod bridge;
pub mod client;
pub mod error;
pub mod server;
pub mod transports;
pub mod types;
