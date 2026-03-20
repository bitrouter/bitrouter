//! MCP types, config, and traits for BitRouter.
//!
//! This is a lightweight protocol library containing configuration types,
//! error definitions, access groups, parameter filters, and the admin trait.
//! The runtime gateway (upstream connections, tool aggregation) lives in the
//! `bitrouter` binary crate behind the `mcp` feature gate.
pub mod admin;
pub mod config;
pub mod error;
pub mod groups;
pub mod param_filter;
