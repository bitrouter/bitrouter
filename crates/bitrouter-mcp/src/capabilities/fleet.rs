//! The fleet capability port: tool input schemas plus the `Fleet` trait the
//! orchestrator profile injects. The app-side adapter (`SubstrateFleet` in
//! `apps/bitrouter`) implements this against the substrate; the crate stays
//! substrate-free and only owns the argument shapes and tool descriptions.

use crate::error::ToolError;

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct SpawnArgs {
    /// ACP agent id: a bundled-catalog id (`claude-acp`, `codex-acp`,
    /// `gemini-cli`) or a configured `agents:` entry.
    pub agent: String,
    /// The task prompt. Phrase it with clear boundaries and an output
    /// contract; the subagent works in an isolated worktree.
    pub task: String,
    /// Isolate in a fresh git worktree + branch (default true — set false
    /// only for read-only investigation tasks).
    pub worktree: Option<bool>,
    /// Optional JSON Schema the subagent's final reply must satisfy; the
    /// summary then carries `result`/`schema_ok` (one repair re-prompt).
    pub result_schema: Option<serde_json::Value>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct HandleArgs {
    /// Subagent handle, as returned by `spawn_subagent`.
    pub handle: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct PromptArgs {
    /// Subagent handle, as returned by `spawn_subagent`.
    pub handle: String,
    /// The follow-up prompt (e.g. review feedback to address).
    pub text: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct StatusArgs {
    /// Subagent handle; omit for a whole-fleet snapshot.
    pub handle: Option<String>,
}

/// Spawn and manage worktree-isolated ACP subagents. The methods speak plain
/// JSON — the summary/diff/status payloads are assembled adapter-side — so
/// the crate never touches a substrate type.
#[async_trait::async_trait]
pub trait Fleet: Send + Sync {
    /// Spawn a subagent, send it `args.task`, and block until the turn ends.
    async fn spawn(&self, args: SpawnArgs) -> Result<serde_json::Value, ToolError>;
    /// Send a follow-up prompt to a running subagent and block on the turn.
    async fn prompt(&self, args: PromptArgs) -> Result<serde_json::Value, ToolError>;
    /// Snapshot one subagent (`Some(handle)`) or the whole fleet (`None`).
    async fn status(&self, handle: Option<&str>) -> Result<serde_json::Value, ToolError>;
    /// The subagent's full diff against its spawn base.
    async fn diff(&self, handle: &str) -> Result<String, ToolError>;
    /// Apply the subagent's diff onto the base working tree, uncommitted.
    async fn apply(&self, handle: &str) -> Result<serde_json::Value, ToolError>;
    /// Merge the subagent's branch into the base repository, keeping history.
    async fn merge(&self, handle: &str) -> Result<serde_json::Value, ToolError>;
    /// Shut the subagent down (its worktree is retained).
    async fn close(&self, handle: &str) -> Result<serde_json::Value, ToolError>;
}
