//! The skills-introspection port: the `skills_search` / `skills_get` tools'
//! backing query.
//!
//! Read-only. The app-side adapter reads the installed-skills root via
//! `bitrouter-skills`; the crate stays skills-free and only owns the tool
//! argument shapes.

use crate::error::ToolError;

/// Arguments to `skills_search`.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct SkillsSearchArgs {
    /// Substring matched (case-insensitively) against installed skills' name
    /// and description.
    pub query: String,
}

/// Arguments to `skills_get`.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct SkillsGetArgs {
    /// The skill's canonical name (as returned by `skills_search`).
    pub name: String,
}

/// Search and fetch installed BitRouter skills so the orchestrator can hand one
/// to a subagent. Returns pre-built JSON — the crate never reads the filesystem
/// itself.
#[async_trait::async_trait]
pub trait SkillsQuery: Send + Sync {
    /// Installed skills whose name/description match `query`.
    async fn search(&self, query: &str) -> Result<serde_json::Value, ToolError>;
    /// One skill's frontmatter + SKILL.md body, or a `ToolError` when no
    /// installed skill has that name.
    async fn get(&self, name: &str) -> Result<serde_json::Value, ToolError>;
}
