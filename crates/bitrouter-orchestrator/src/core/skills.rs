//! Native skill selection belongs to the original Turn checkpoint. A source
//! receipt identifies content; it never grants tools or MCP execution consent.

pub mod explain;
pub mod files;
pub mod selection;

use bitrouter_sdk::decision_model::policy::DecisionPolicy;
use serde::{Deserialize, Serialize};

use crate::harness::skills::library::{LibraryScope, ManagedSkill, ManagedSkillCatalog};
use crate::harness::skills::mcp::{HeldMcpSkill, McpSkillCatalog};

use super::checkpoint::sha256;
use super::context_router::planner::SourceRevision;
use super::protocol::{CoreError, ErrorCode};

/// Checkpoint/host contract for frozen skill selection and explicit MCP consent.
/// A core without this feature must not drop these facts while restoring.
pub const FEATURE: &str = "native_skills_v1";
pub const FILES_FEATURE: &str = "native_skill_files_v1";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum SkillSelector {
    Managed {
        scope: LibraryScope,
        installation_id: String,
    },
    Mcp {
        server: String,
        uri: String,
    },
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SkillConstraints {
    #[serde(default)]
    pub include: Vec<SkillSelector>,
    #[serde(default)]
    pub exclude: Vec<SkillSelector>,
}

impl SkillConstraints {
    pub fn validate(&self) -> Result<(), CoreError> {
        if self.include.len().saturating_add(self.exclude.len()) > 128 {
            return Err(invalid("skill constraint count exceeds host budget"));
        }
        for (index, selector) in self.include.iter().enumerate() {
            if self.include[..index].contains(selector) || self.exclude.contains(selector) {
                return Err(invalid(
                    "skill constraints contain duplicate or contradictory includes",
                ));
            }
        }
        for selector in self.include.iter().chain(&self.exclude) {
            match selector {
                SkillSelector::Managed {
                    installation_id, ..
                } if installation_id.trim().is_empty() => {
                    return Err(invalid("skill installation identity is empty"));
                }
                SkillSelector::Mcp { server, uri }
                    if server.trim().is_empty() || uri.trim().is_empty() =>
                {
                    return Err(invalid("MCP skill source identity is empty"));
                }
                _ => {}
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", content = "binding", rename_all = "snake_case")]
pub enum SkillSource {
    Managed(ManagedSkill),
    Mcp(HeldMcpSkill),
}

impl SkillSource {
    pub fn selector(&self) -> SkillSelector {
        match self {
            Self::Managed(skill) => SkillSelector::Managed {
                scope: skill.scope.clone(),
                installation_id: skill.installation_id.clone(),
            },
            Self::Mcp(skill) => SkillSelector::Mcp {
                server: skill.origin.server.clone(),
                uri: skill.entry.uri.clone(),
            },
        }
    }

    pub fn identity(&self) -> Result<String, CoreError> {
        match self {
            Self::Managed(skill) => skill
                .material()
                .map(|material| material.material_id)
                .map_err(invalid),
            Self::Mcp(skill) => Ok(format!(
                "mcp_skill_{}",
                sha256(
                    &serde_json::to_vec(&(&skill.origin, &skill.entry_digest),)
                        .map_err(|error| invalid(error.to_string()))?
                )
            )),
        }
    }

    pub fn name(&self) -> &str {
        match self {
            Self::Managed(skill) => &skill.name,
            Self::Mcp(skill) => skill
                .entry
                .frontmatter
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or(""),
        }
    }

    pub fn description(&self) -> &str {
        match self {
            Self::Managed(skill) => &skill.description,
            Self::Mcp(skill) => skill
                .entry
                .frontmatter
                .get("description")
                .and_then(|v| v.as_str())
                .unwrap_or(""),
        }
    }

    pub fn instruction_bytes(&self) -> Result<u64, CoreError> {
        match self {
            Self::Managed(skill) => Ok(skill.instruction_bytes),
            Self::Mcp(skill) => skill
                .resource(&skill.entry.uri)
                .map(|file| file.size)
                .map_err(invalid),
        }
    }

    pub fn verify(&self, bytes: &[u8]) -> Result<(), CoreError> {
        match self {
            Self::Managed(skill) => {
                if bytes.len() as u64 != skill.instruction_bytes
                    || sha256(bytes) != skill.instruction_sha256
                {
                    return Err(invalid("managed skill instruction commitment changed"));
                }
                let text = std::str::from_utf8(bytes)
                    .map_err(|_| invalid("skill instructions require UTF-8"))?;
                let frontmatter = crate::harness::skills::format::parse_frontmatter(text)
                    .map_err(|error| invalid(error.to_string()))?;
                frontmatter
                    .validate()
                    .map_err(|error| invalid(error.to_string()))?;
                if frontmatter.name != skill.name || frontmatter.description != skill.description {
                    return Err(invalid("managed skill frontmatter changed"));
                }
            }
            Self::Mcp(skill) => skill
                .verify(skill.resource(&skill.entry.uri).map_err(invalid)?, bytes)
                .map_err(invalid)?,
        }
        if bytes.contains(&0) {
            return Err(invalid("binary skill instructions are unsupported"));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct SkillCatalog {
    pub managed: ManagedSkillCatalog,
    pub mcp: McpSkillCatalog,
}

impl SkillCatalog {
    pub fn sources(&self) -> Vec<SkillSource> {
        self.managed
            .skills
            .iter()
            .cloned()
            .map(SkillSource::Managed)
            .chain(self.mcp.skills.iter().cloned().map(SkillSource::Mcp))
            .collect()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillCandidate {
    pub source: SkillSource,
    pub reason: String,
    pub selected: bool,
    pub probability: Option<f64>,
    pub content: Option<String>,
    /// Verified supporting bytes in this original Turn, outside filesystem
    /// discovery. Keys are exact manifest URIs or local bundle-relative paths.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub files: std::collections::BTreeMap<String, String>,
    /// An optional root that failed before activation remains an excluded
    /// selection intent, never a partially loaded instruction or a new version.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<CoreError>,
    /// Consent is specific to this complete MCP binding. Earlier requests stay
    /// available when a pending request must be fenced under a new owner.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub consents: Vec<SkillConsent>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SkillConsent {
    pub request_id: String,
    pub execution_epoch: u64,
    pub permission_revision: u64,
    pub tool_manifest_digest: String,
    pub skill_id: String,
    pub entry_digest: String,
    pub approved: Option<bool>,
    pub answer_operation_id: Option<String>,
    pub revoked: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SkillConsentRequest {
    pub session_id: String,
    pub run_id: String,
    pub agent_id: String,
    pub agent_turn_id: String,
    pub consent: SkillConsent,
    pub source: HeldMcpSkill,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SkillConsentAnswer {
    pub session_id: String,
    pub run_id: String,
    pub agent_id: String,
    pub agent_turn_id: String,
    pub request_id: String,
    pub execution_epoch: u64,
    pub skill_id: String,
    pub entry_digest: String,
    pub approved: bool,
}

impl SkillConsentAnswer {
    pub fn fingerprint(&self) -> Result<String, CoreError> {
        serde_json::to_vec(&serde_json::json!({"type":"skill.consent.answer","answer":self}))
            .map(|bytes| sha256(&bytes))
            .map_err(|error| CoreError::rejected(ErrorCode::CheckpointConflict, error.to_string()))
    }
}

impl SkillConsentRequest {
    pub fn answer(&self, approved: bool) -> SkillConsentAnswer {
        SkillConsentAnswer {
            session_id: self.session_id.clone(),
            run_id: self.run_id.clone(),
            agent_id: self.agent_id.clone(),
            agent_turn_id: self.agent_turn_id.clone(),
            request_id: self.consent.request_id.clone(),
            execution_epoch: self.consent.execution_epoch,
            skill_id: self.consent.skill_id.clone(),
            entry_digest: self.consent.entry_digest.clone(),
            approved,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillSelection {
    /// Assessment input may rebase before selection. Once selected, this and
    /// the instruction budget are immutable; the original catalog never moves.
    pub source: SourceRevision,
    pub catalog: SkillCatalog,
    pub constraints: SkillConstraints,
    pub policy: DecisionPolicy,
    pub max_selected: usize,
    pub instruction_limit_bytes: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_limit_bytes: Option<u64>,
    pub candidates: Vec<SkillCandidate>,
    pub decision_id: Option<String>,
    /// Selection and loaded bytes are separate durable transitions.
    pub selected: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<CoreError>,
    pub validated_step_count: Option<usize>,
}

fn invalid(message: impl Into<String>) -> CoreError {
    CoreError::rejected(ErrorCode::ArtifactUnavailable, message)
}
