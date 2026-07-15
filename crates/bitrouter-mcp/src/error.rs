//! The error a capability port returns to the handler. The crate owns the
//! tool schemas and surfaces failures as MCP tool errors; the app-side
//! adapter maps its own error type (`anyhow`, substrate, metering) into this
//! plain, substrate-free string carrier.

/// A capability operation failed. Carries the human-readable message the MCP
/// handler wraps into a `CallToolResult::error`.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct ToolError(pub String);

impl ToolError {
    /// Build a `ToolError` from anything string-like.
    pub fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}
