//! Frozen skill source descriptions; discovery and verification stay in the host.

use crate::protocol::mcp_skills::SkillEntry;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LocalScope {
    Project,
    Global,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct LocalSkill {
    pub scope: LocalScope,
    pub directory: PathBuf,
    pub workspace: PathBuf,
    pub api_key_id: String,
    pub user_id: String,
    pub local: bool,
    pub directory_id: String,
    pub name: String,
    pub description: String,
    /// Content fingerprint of the directory manifest, without a version store.
    pub version: String,
    /// Frozen file manifest captured from the existing skill directory.
    pub receipt: Value,
    pub instruction_sha256: String,
    pub instruction_bytes: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct LocalSkillCatalog {
    pub skills: Vec<LocalSkill>,
    pub problems: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SkillScope {
    pub workspace: PathBuf,
    pub api_key_id: String,
    pub user_id: String,
    pub local: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SkillOrigin {
    /// Host configuration key, never the upstream's self-declared serverInfo.
    pub server: String,
    pub configuration_digest: String,
    pub scope: SkillScope,
}

/// Persist this complete entry for as long as its instructions remain in
/// context. A later listing is not a replacement for this commitment.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HeldMcpSkill {
    pub origin: SkillOrigin,
    pub entry: SkillEntry,
    /// Covers the full entry, including every URI/digest/size and frontmatter.
    pub entry_digest: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct McpSkillProblem {
    pub server: String,
    pub uri: Option<String>,
    pub reason: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct McpSkillCatalog {
    pub skills: Vec<HeldMcpSkill>,
    pub excluded: Vec<McpSkillProblem>,
}
