//! Skill metadata validation and wire values for the gateway MCP proxy.

pub mod protocol;
#[cfg(any(feature = "mcp", test))]
pub mod wire;
