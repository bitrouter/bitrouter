//! Server-side tool loop — BitRouter as the tool executor.
//!
//! BitRouter injects tools into an outbound `Prompt`, intercepts the model's
//! calls to them, executes them itself, feeds the results back, and re-calls
//! the upstream — looping until the model stops calling router-owned tools,
//! all behind a single caller-visible response. A router tool looks like an
//! ordinary function tool to the upstream provider and like a server-resolved
//! tool to the caller.
//!
//! [`toolset::RouterToolset`] is the provider-agnostic executor seam; the MCP
//! implementation bridges the [`crate::mcp`] routing module (an MCP *client*
//! consuming upstream MCP servers) into the LLM request loop — the inverse of
//! the standalone `bitrouter-mcp` server crate.
//!
//! Per crate guideline 2, this module does not `pub use` from its submodules;
//! downstream reaches types directly (e.g. `server_tools::toolset::RouterToolset`).

pub mod approval;
pub mod classify;
pub mod config;
pub mod loop_controller;
pub mod mcp_toolset;
pub mod stream;
pub mod toolset;
