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
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SkillMetadata {
    pub name: String,
    pub description: String,
    pub material: MaterialRef,
}

redacted_contract_debug!(MaterialRef, McpTool, HarnessInventory, SkillMetadata);
