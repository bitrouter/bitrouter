//! Provider re-exports for `bitrouter-cli`.
//!
//! Re-surfaces the ACP and MCP provider modules so that the CLI does not
//! need a direct dependency on `bitrouter-providers`. The re-exports are
//! feature-gated to match the underlying provider features on this crate;
//! callers reach them via `bitrouter::providers::acp::*` and
//! `bitrouter::providers::mcp::*`.

#[cfg(feature = "acp")]
pub use bitrouter_providers::acp;

#[cfg(feature = "mcp")]
pub use bitrouter_providers::mcp;
