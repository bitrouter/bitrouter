//! Immutable context evidence and model-step routing records. Estimates are
//! distinct from observed usage; unknown token capacity is never a verified fit.

use bitrouter_sdk::language_model::native::{NativeAttemptReport, NativePlan};
use bitrouter_sdk::language_model::types::{Message, Prompt, ReasoningEffort, UsageOrigin};
use serde::{Deserialize, Serialize};

use super::checkpoint::sha256;
use super::protocol::{CoreError, ErrorCode, MaterialRef, RoutingSettings};
use super::session::SessionSnapshot;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextManifest {
    pub context_id: String,
    pub revision: u64,
    pub agent_id: String,
    pub agent_turn_id: String,
    pub signal_revision: u64,
    pub permission_revision: u64,
    pub workspace_revision: Option<String>,
    pub tool_manifest_digest: String,
    pub required_instructions: Vec<String>,
    pub acceptance_criteria: Vec<String>,
    pub system_sha256: String,
    /// Ordered message commitments preserve both role and call/result identity.
    pub message_sha256: Vec<String>,
    /// References retain provenance; bodies live in the step's material snapshot.
    pub materials: Vec<MaterialRef>,
    pub prompt_bytes: u64,
    pub estimated_input_tokens: Option<u64>,
    pub token_estimate_source: String,
    pub output_allowance: Option<u32>,
}

impl ContextManifest {
    pub fn capture(
        state: &SessionSnapshot,
        agent_id: &str,
        prompt: &Prompt,
    ) -> Result<Self, CoreError> {
        let agent = state
            .agents
            .get(agent_id)
            .ok_or_else(|| invalid("unknown context agent"))?;
        let turn = agent
            .turn
            .as_ref()
            .ok_or_else(|| invalid("context has no turn"))?;
        let mut materials = super::session::selected_materials(state, &turn.input)?;
        for material in &mut materials {
            material.content = None;
        }
        Ok(Self {
            context_id: format!("context_{}", agent.agent_id),
            revision: agent.context_revision,
            agent_id: agent.agent_id.clone(),
            agent_turn_id: turn.agent_turn_id.clone(),
            signal_revision: state.signals.revision,
            permission_revision: state.manifest.permission_revision,
            workspace_revision: state.manifest.workspace_revision.clone(),
            tool_manifest_digest: state.manifest.tool_manifest_digest.clone(),
            required_instructions: agent.required_instructions.clone(),
            acceptance_criteria: turn.input.acceptance_criteria.clone(),
            system_sha256: commitment(&prompt.system)?,
            message_sha256: history_commitments(&prompt.messages)?,
            materials,
            prompt_bytes: serde_json::to_vec(prompt).map_err(encoding_error)?.len() as u64,
            estimated_input_tokens: None,
            token_estimate_source: "unknown_no_tokenizer".into(),
            output_allowance: prompt.params.max_tokens,
        })
    }

    /// Shared preparation can add material but cannot remove, rewrite or
    /// reorder committed instructions and history. Validate the whole result
    /// too, so an added unmatched call/result cannot cross the model boundary.
    pub fn validate_prepared(&self, prompt: &Prompt) -> Result<(), CoreError> {
        if commitment(&prompt.system)? != self.system_sha256 {
            return Err(invalid("model preparation changed required instructions"));
        }
        let prepared = history_commitments(&prompt.messages)?;
        let mut expected = self.message_sha256.iter();
        let mut next = expected.next();
        for actual in &prepared {
            if next == Some(actual) {
                next = expected.next();
            }
        }
        if next.is_some() {
            return Err(invalid(
                "model preparation removed or reordered required context",
            ));
        }
        crate::context::validate_history(&prompt.messages).map_err(invalid)
    }

    pub fn prepared_manifest(&self, prompt: &Prompt) -> Result<Self, CoreError> {
        let mut prepared = self.clone();
        prepared.system_sha256 = commitment(&prompt.system)?;
        prepared.message_sha256 = history_commitments(&prompt.messages)?;
        prepared.prompt_bytes = serde_json::to_vec(prompt).map_err(encoding_error)?.len() as u64;
        prepared.output_allowance = prompt.params.max_tokens;
        Ok(prepared)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoutingDecision {
    pub decision_id: String,
    pub policy_id: String,
    pub source: String,
    pub input_state_revision: u64,
    pub modes: RoutingSettings,
    pub context: ContextManifest,
    pub candidate_ids: Vec<String>,
    pub selected_candidate_id: String,
    pub selected_model: String,
    pub selected_effort: Option<ReasoningEffort>,
    pub reason_codes: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApplicationDisposition {
    Applied,
    Rejected,
    Stale,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DecisionApplied {
    pub decision_id: String,
    pub agent_turn_id: String,
    pub step_id: String,
    pub state_revision: u64,
    pub disposition: ApplicationDisposition,
    pub reason: Option<CoreError>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionReceipt {
    pub decision_id: String,
    pub attempt_id: String,
    pub report: NativeAttemptReport,
    /// Canonical serving request effort, not a provider-observed effort value.
    pub requested_effort: Option<ReasoningEffort>,
    pub usage_origin: Option<UsageOrigin>,
    /// A missing price or usage is unknown. It must not become a zero charge.
    pub cost_micro_usd: Option<f64>,
    pub cost_source: String,
    /// Usage without raw cache counters does not prove cache savings.
    pub cache_observation_source: String,
}

impl ExecutionReceipt {
    pub fn capture(
        decision_id: &str,
        attempt_id: &str,
        plan: &NativePlan,
        report: NativeAttemptReport,
    ) -> Self {
        let usage = report
            .result
            .as_ref()
            .and_then(|result| result.usage.as_ref());
        Self {
            decision_id: decision_id.into(),
            attempt_id: attempt_id.into(),
            requested_effort: plan.prompt.params.reasoning_effort,
            usage_origin: usage.map(|usage| usage.origin),
            cost_micro_usd: None,
            cost_source: "unknown_price_or_receipt".into(),
            // Raw totals do not prove cache activity. Preserve the raw report
            // for auditing; cache counters require protocol-specific evidence.
            cache_observation_source: "unknown".into(),
            report,
        }
    }
}

fn history_commitments(messages: &[Message]) -> Result<Vec<String>, CoreError> {
    messages.iter().map(commitment).collect()
}

fn commitment(value: &impl Serialize) -> Result<String, CoreError> {
    serde_json::to_vec(value)
        .map(|bytes| sha256(&bytes))
        .map_err(encoding_error)
}

fn encoding_error(error: serde_json::Error) -> CoreError {
    CoreError::rejected(ErrorCode::CheckpointConflict, error.to_string())
}

fn invalid(message: impl Into<String>) -> CoreError {
    CoreError::rejected(ErrorCode::NoFeasibleRoute, message)
}
