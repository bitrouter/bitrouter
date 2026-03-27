//! MCP client runtime — connects to upstream MCP servers and aggregates capabilities.
//!
//! This module contains the client, transport, and bridge implementations
//! for communicating with upstream MCP servers. Protocol types, server traits,
//! and error definitions live in [`bitrouter_mcp`].

pub mod client;
pub mod transports;
