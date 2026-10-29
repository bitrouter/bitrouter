//! Public harness resources contracts. Execution and storage remain application-owned.

use crate::harness::runtime::EffectStatus;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MaterialRef {
    pub material_id: String,
    pub version: String,
    pub sha256: String,
    pub media_type: String,
    pub provenance: String,
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct McpTool {
    pub name: String,
    pub description: String,
    pub parameters: Value,
    pub effect: EffectStatus,
    pub approval_required: bool,
}

/// Frozen at execution preparation and committed before the first model call.
/// No credentials or skill bodies are part of this public inventory.
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HarnessInventory {
    pub binding_digest: String,
    pub tools: Vec<McpTool>,
    pub instructions: Vec<MaterialRef>,
    pub skills: Vec<SkillMetadata>,
    pub skill_problems: Vec<String>,
    #[serde(default, skip_serializing_if = "mcp_catalog_empty")]
    pub mcp_skills: super::skills::McpSkillCatalog,
    #[serde(default, skip_serializing_if = "managed_catalog_empty")]
    pub local_skills: super::skills::LocalSkillCatalog,
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SkillMetadata {
    pub name: String,
    pub description: String,
    pub material: MaterialRef,
}

redacted_contract_debug!(MaterialRef, McpTool, HarnessInventory, SkillMetadata);

fn mcp_catalog_empty(catalog: &super::skills::McpSkillCatalog) -> bool {
    catalog.skills.is_empty() && catalog.excluded.is_empty()
}

fn managed_catalog_empty(catalog: &super::skills::LocalSkillCatalog) -> bool {
    catalog.skills.is_empty() && catalog.problems.is_empty()
}
