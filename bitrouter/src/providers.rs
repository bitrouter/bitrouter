//! Provider re-exports for `bitrouter-cli`.
//!
//! Re-surfaces the MCP provider module so the CLI can reach it via
//! `bitrouter::providers::mcp` without a direct dependency on
//! `bitrouter-providers`. ACP lives in its own crate (`bitrouter-acp`) and
//! is re-exported at the top level as `bitrouter::acp`.

#[cfg(feature = "mcp")]
pub use bitrouter_providers::mcp;
