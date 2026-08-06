//! The SEP-2640 skills port: the trait the app implements to serve the skills
//! this server holds.
//!
//! The *wire shapes* — `SkillEntry`, [`ListSkillsResult`], the `skills/*`
//! method names, the `skill://` scheme — live in
//! [`bitrouter_sdk::mcp::skills`], because both halves of the gateway speak
//! them and the SDK's hook traits need them to reason about skills in flight.
//! This module owns only what a *server* must implement, which is why it sits
//! beside the other capability ports rather than in the SDK.
//!
//! Distinct from [`super::skills`], which is the older tool-shaped surface
//! (`skills_search` / `skills_get`). Both are served: the tool form works with
//! every MCP client today, while the method form is what SEP-aware hosts will
//! consume. Neither supersedes the other.

use bitrouter_sdk::mcp::skills::{GetSkillResult, ListSkillsResult};

use crate::error::ToolError;

/// The body of a skill file, as MCP resource contents carry it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkillFileBody {
    /// Valid UTF-8, served as `text`.
    Text(String),
    /// Anything else, served as base64 `blob`.
    Blob(String),
}

/// One skill file's contents, the answer to a `resources/read`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillFile {
    /// The URI that was read.
    pub uri: String,
    /// Best-effort content type.
    pub mime_type: Option<String>,
    /// The file's body.
    pub body: SkillFileBody,
}

/// Serves the skills this server holds.
///
/// Implemented app-side over the installed-skills root; the crate owns the
/// handler wiring and never reads the filesystem itself, matching every other
/// port in [`super`].
#[async_trait::async_trait]
pub trait SkillCatalog: Send + Sync {
    /// Every skill this server serves.
    async fn list(&self) -> Result<ListSkillsResult, ToolError>;
    /// One skill's entry by the URI of its `SKILL.md`.
    ///
    /// Returns a [`ToolError`] when the URI does not identify a skill this
    /// server serves; the handler maps that to JSON-RPC `-32602`, the code the
    /// SEP mandates (and the one `resources/read` already uses for unknown
    /// resources).
    async fn get(&self, uri: &str) -> Result<GetSkillResult, ToolError>;
    /// Read one file of one skill — the skill's `SKILL.md` or any supporting
    /// file enumerated in its entry's `resources`.
    ///
    /// Implementations MUST refuse a URI that resolves outside the skill it
    /// names; a skill's URI space is not a general filesystem gateway.
    async fn read(&self, uri: &str) -> Result<SkillFile, ToolError>;
}
