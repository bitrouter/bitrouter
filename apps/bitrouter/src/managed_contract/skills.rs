//! Core skill constraints and explicit consent wire values.

use bitrouter_ai::harness::skills::HeldMcpSkill;
use serde::{Deserialize, Serialize};

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
