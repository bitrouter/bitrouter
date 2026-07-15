//! The human-escalation port: the `notify_human` / `request_attach` /
//! `request_review` tools' backing bridge.
//!
//! Under `bitrouter tui` these ride the fleet socket to the supervising human;
//! headless (no TUI attached) they return a note saying so rather than erroring
//! — a subagent with no human in the loop should hear "nobody's watching", not
//! a failure. The app-side adapter owns the socket; the crate only owns the
//! tool argument shapes.

use crate::error::ToolError;

/// Arguments to `notify_human`.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct NotifyArgs {
    /// The one-line message to show the human.
    pub message: String,
}

/// Arguments to `request_attach` / `request_review`.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct HumanHandleArgs {
    /// The subagent handle (as returned by `spawn_subagent`).
    pub handle: String,
}

/// Reach the supervising human from a fleet bridge. Each method delivers to the
/// TUI (or reports that no human is attached) and returns pre-built JSON — the
/// crate never touches the socket itself.
#[async_trait::async_trait]
pub trait HumanBridge: Send + Sync {
    /// Post a one-line notice to the human.
    async fn notify(&self, message: &str) -> Result<serde_json::Value, ToolError>;
    /// Ask the human to attach to a subagent's pane and drive it directly.
    async fn request_attach(&self, handle: &str) -> Result<serde_json::Value, ToolError>;
    /// Flag a subagent's work for the human's review queue.
    async fn request_review(&self, handle: &str) -> Result<serde_json::Value, ToolError>;
}
