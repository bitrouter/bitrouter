//! MCP types, config, and traits for BitRouter.
//!
//! This is a lightweight protocol library containing configuration types,
//! error definitions, access groups, parameter filters, and the admin trait.
//!
//! Enable the `client` feature to get runtime upstream connection and
//! registry components.
#[cfg(feature = "client")]
pub mod client;
pub mod config;
pub mod error;
pub mod groups;
pub mod param_filter;
pub mod server;
#[cfg(feature = "client")]
pub mod transports;
pub mod types;
