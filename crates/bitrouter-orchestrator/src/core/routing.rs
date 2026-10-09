//! Immutable context evidence and model-step routing records. Estimates are
//! distinct from observed usage; unknown token capacity is never a verified fit.

use bitrouter_ai::types::{Message, Prompt, ReasoningEffort, UsageOrigin};
use bitrouter_sdk::language_model::native::{NativeAttemptReport, NativeInputCount, NativePlan};
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
        if self.output_allowance.is_some() && self.output_allowance != prompt.params.max_tokens {
            return Err(invalid("model preparation changed the output reservation"));
        }
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
    /// The committed task allocation is shared by every step in this turn.
    pub allocation_id: Option<String>,
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
    /// Per-provider assessment after shared model selection. Unknown facts
    /// permit an attempt but never establish a verified capacity fit.
    pub routes: Vec<RouteFeasibility>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouteFeasibility {
    pub route_index: u32,
    pub rejection_reasons: Vec<String>,
    pub unverified_constraints: Vec<String>,
}

pub(super) fn assess_routes(plan: &NativePlan) -> Result<Vec<RouteFeasibility>, CoreError> {
    let required = plan.prompt.required_capabilities();
    plan.routes
        .iter()
        .enumerate()
        .map(|(index, route)| {
            let mut assessment = RouteFeasibility {
                route_index: u32::try_from(index).map_err(|_| invalid("route index exhausted"))?,
                rejection_reasons: Vec::new(),
                unverified_constraints: Vec::new(),
            };
            match &route.protocol_validation {
                bitrouter_sdk::language_model::native::NativeProtocolValidation::Compatible => {}
                bitrouter_sdk::language_model::native::NativeProtocolValidation::Unverified => {
                    assessment
                        .unverified_constraints
                        .push("protocol_compatibility_unknown".into())
                }
                bitrouter_sdk::language_model::native::NativeProtocolValidation::Rejected {
                    reason,
                } => assessment
                    .rejection_reasons
                    .push(format!("protocol_incompatible:{reason}")),
            }
            let input_tokens = match &route.input_count {
                Some(NativeInputCount::Counted { input_tokens, .. }) => Some(*input_tokens),
                Some(NativeInputCount::Unavailable { .. }) => {
                    assessment
                        .rejection_reasons
                        .push("input_token_count_unavailable".into());
                    None
                }
                None => {
                    assessment
                        .unverified_constraints
                        .push("input_token_count_unknown".into());
                    None
                }
            };
            if route.constraints.capabilities.is_empty() {
                assessment
                    .unverified_constraints
                    .push("capabilities_unknown".into());
            } else if required
                .iter()
                .any(|capability| !route.constraints.capabilities.contains(capability))
            {
                // The shared catalog records positive observations, not an
                // exhaustive denylist. Known serving-protocol incompatibilities
                // are rejected separately by the shared adapter assessment.
                assessment
                    .unverified_constraints
                    .push("required_capability_unknown".into());
            }
            let limits = &route.constraints.token_limits;
            if let (Some(input), Some(limit)) = (input_tokens, limits.max_input_tokens)
                && input > limit
            {
                assessment
                    .rejection_reasons
                    .push("input_limit_exceeded".into());
            }
            match route.output_token_limit_supported {
                Some(false)
                    if plan.prompt.params.max_tokens.is_some_and(|reservation| {
                        limits.output_reservation_covers_model(reservation)
                    }) => {}
                Some(false) => assessment
                    .rejection_reasons
                    .push("output_reservation_unsupported".into()),
                None => assessment
                    .unverified_constraints
                    .push("output_reservation_support_unknown".into()),
                Some(true) => {}
            }
            if limits.max_input_tokens == Some(0) {
                assessment
                    .rejection_reasons
                    .push("input_capacity_exhausted".into());
            } else if limits.max_input_tokens.is_none() {
                assessment
                    .unverified_constraints
                    .push("input_limit_unknown".into());
            }
            match plan.prompt.params.max_tokens {
                None | Some(0) => assessment
                    .rejection_reasons
                    .push("output_reservation_missing".into()),
                Some(output) => {
                    if let (Some(input), Some(window)) = (input_tokens, limits.context_window)
                        && input
                            .checked_add(u64::from(output))
                            .is_none_or(|total| total > window)
                    {
                        assessment
                            .rejection_reasons
                            .push("context_window_exceeded".into());
                    }
                    if limits
                        .max_output_tokens
                        .is_some_and(|limit| u64::from(output) > limit)
                    {
                        assessment
                            .rejection_reasons
                            .push("output_limit_exceeded".into());
                    }
                    // A nonempty managed request consumes input capacity too.
                    // Even without a tokenizer, output alone cannot fill or
                    // exceed the entire combined window.
                    if limits
                        .context_window
                        .is_some_and(|limit| u64::from(output) >= limit)
                    {
                        assessment
                            .rejection_reasons
                            .push("context_window_exhausted_by_output".into());
                    }
                }
            }
            if limits.max_output_tokens.is_none() {
                assessment
                    .unverified_constraints
                    .push("output_limit_unknown".into());
            }
            if limits.context_window.is_none() {
                assessment
                    .unverified_constraints
                    .push("context_window_unknown".into());
            }
            Ok(assessment)
        })
        .collect()
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
    /// Configured model-token estimate only, not an authoritative charge.
    /// A missing price or usage stays unknown; auxiliary fees are not included.
    pub cost_micro_usd: Option<u64>,
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
        let usage_origin = report
            .result
            .as_ref()
            .and_then(|result| result.usage.as_ref())
            .map(|usage| usage.origin)
            .or_else(|| {
                report
                    .output_rejection
                    .as_ref()
                    .and_then(|rejected| rejected.usage.as_ref())
                    .map(|usage| usage.origin)
            })
            .or_else(|| {
                report
                    .report_rejection
                    .as_ref()
                    .and_then(|rejected| rejected.usage.as_ref())
                    .map(|usage| usage.origin)
            });
        Self {
            decision_id: decision_id.into(),
            attempt_id: attempt_id.into(),
            requested_effort: plan.prompt.params.reasoning_effort,
            usage_origin,
            cost_micro_usd: report.token_cost.estimated_micro_usd(),
            cost_source: if report.token_cost.estimated_micro_usd().is_some() {
                "configured_token_estimate"
            } else {
                "unknown_price_or_receipt"
            }
            .into(),
            cache_observation_source: report.cache.source.clone(),
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
