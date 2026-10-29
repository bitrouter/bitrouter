//! Native skill selection belongs to the original Turn checkpoint. A source
//! receipt identifies content; it never grants tools or MCP execution consent.

use crate::skills::library::LocalSkillExt;
use crate::skills::mcp::HeldMcpSkillExt;
use bitrouter_ai::core::skills::{
    SkillConsent, SkillConsentAnswer, SkillConsentRequest, SkillConstraints, SkillSelector,
};

pub mod explain;
pub mod files;
pub mod selection;
pub(crate) mod state;

use serde::{Deserialize, Serialize};

use bitrouter_ai::harness::skills::{HeldMcpSkill, McpSkillCatalog};
use bitrouter_ai::harness::skills::{LocalSkill, LocalSkillCatalog};

use super::checkpoint::sha256;
use crate::storage::checkpoint::snapshot::context::view::SourceRevision;
use bitrouter_ai::core::protocol::{CoreError, ErrorCode};

/// Checkpoint/host contract for frozen skill selection and explicit MCP consent.
/// A core without this feature must not drop these facts while restoring.
pub const FEATURE: &str = "native_skills_v1";
pub const FILES_FEATURE: &str = "native_skill_files_v1";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", content = "binding", rename_all = "snake_case")]
pub enum SkillSource {
    Local(LocalSkill),
    Mcp(HeldMcpSkill),
}

impl SkillSource {
    pub fn selector(&self) -> SkillSelector {
        match self {
            Self::Local(skill) => SkillSelector::Local {
                scope: skill.scope.clone(),
                directory_id: skill.directory_id.clone(),
            },
            Self::Mcp(skill) => SkillSelector::Mcp {
                server: skill.origin.server.clone(),
                uri: skill.entry.uri.clone(),
            },
        }
    }

    pub fn identity(&self) -> Result<String, CoreError> {
        match self {
            Self::Local(skill) => skill
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
            Self::Local(skill) => &skill.name,
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
            Self::Local(skill) => &skill.description,
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
            Self::Local(skill) => Ok(skill.instruction_bytes),
            Self::Mcp(skill) => skill
                .resource(&skill.entry.uri)
                .map(|file| file.size)
                .map_err(invalid),
        }
    }

    pub fn verify(&self, bytes: &[u8]) -> Result<(), CoreError> {
        match self {
            Self::Local(skill) => {
                if bytes.len() as u64 != skill.instruction_bytes
                    || sha256(bytes) != skill.instruction_sha256
                {
                    return Err(invalid("managed skill instruction commitment changed"));
                }
                let text = std::str::from_utf8(bytes)
                    .map_err(|_| invalid("skill instructions require UTF-8"))?;
                let frontmatter = crate::skills::format::parse_frontmatter(text)
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
    pub local: LocalSkillCatalog,
    pub mcp: McpSkillCatalog,
}

impl SkillCatalog {
    pub fn sources(&self) -> Vec<SkillSource> {
        self.local
            .skills
            .iter()
            .cloned()
            .map(SkillSource::Local)
            .chain(self.mcp.skills.iter().cloned().map(SkillSource::Mcp))
            .collect()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillCandidate {
    pub source: SkillSource,
    pub reason: String,
    pub selected: bool,
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

pub trait SkillConsentAnswerExt: Sized {
    fn fingerprint(&self) -> Result<String, CoreError>;
}

impl SkillConsentAnswerExt for SkillConsentAnswer {
    fn fingerprint(&self) -> Result<String, CoreError> {
        serde_json::to_vec(&serde_json::json!({"type":"skill.consent.answer","answer":self}))
            .map(|bytes| sha256(&bytes))
            .map_err(|error| CoreError::rejected(ErrorCode::CheckpointConflict, error.to_string()))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillSelection {
    /// Assessment input may rebase before selection. Once selected, this and
    /// the instruction budget are immutable; the original catalog never moves.
    pub source: SourceRevision,
    pub catalog: SkillCatalog,
    pub constraints: SkillConstraints,
    pub max_selected: usize,
    pub instruction_limit_bytes: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_limit_bytes: Option<u64>,
    pub candidates: Vec<SkillCandidate>,
    /// Selection and loaded bytes are separate durable transitions.
    pub selected: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<CoreError>,
    pub validated_step_count: Option<usize>,
}

fn invalid(message: impl Into<String>) -> CoreError {
    CoreError::rejected(ErrorCode::ArtifactUnavailable, message)
}
