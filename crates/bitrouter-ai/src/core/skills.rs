//! Core skill constraints and explicit consent wire values.

use super::protocol::{CoreError, ErrorCode};
use crate::harness::skills::{HeldMcpSkill, LocalScope};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum SkillSelector {
    Local {
        scope: LocalScope,
        directory_id: String,
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
                SkillSelector::Local { directory_id, .. } if directory_id.trim().is_empty() => {
                    return Err(invalid("skill directory identity is empty"));
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

fn invalid(message: impl Into<String>) -> CoreError {
    CoreError::rejected(ErrorCode::ArtifactUnavailable, message)
}
