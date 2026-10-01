//! Durable admission for embedding runtimes that own their model/tool loop.
//! The callbacks never execute tools and never receive provider credentials.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::language_model::routing::RouterRequestIdentity;
use crate::language_model::types::{
    ApiProtocol, Capability, GenerateResult, Message, Prompt, ReasoningEffortSource, RoutingTarget,
};

/// Independently declared token limits. An input limit is not a combined
/// context window, and an absent limit is unknown rather than unbounded.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct ModelTokenLimits {
    /// Maximum input tokens, independently of the output allowance.
    pub max_input_tokens: Option<u64>,
    /// Maximum generated tokens for one request.
    pub max_output_tokens: Option<u64>,
    /// Combined input and output capacity, only when explicitly known.
    pub context_window: Option<u64>,
}

/// Explicit provider support, never inferred from a compatible generation API.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum InputTokenCounting {
    /// POST /responses/input_tokens on the same provider endpoint.
    Responses,
}

/// Provider-reported input size bound to the finalized generation request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum NativeInputCount {
    /// A count is evidence for one request, not an estimate for other routes.
    Counted {
        /// Provider-reported full input, including protocol framing and tools.
        input_tokens: u64,
        /// SHA-256 commitment to serving identity, endpoint and final JSON.
        request_sha256: String,
        /// Counter provenance, independent of generated usage or cache claims.
        source: String,
    },
    /// A configured count failed; this cannot silently become a verified fit.
    Unavailable {
        /// Credential-free failure detail supplied by the executor.
        reason: String,
    },
}

/// Complete observation of a count operation, separate from generated usage.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NativeInputCountReport {
    /// Request identity shared with the prepared and admitted plans.
    pub request_id: String,
    /// Original position in the frozen provider candidate chain.
    pub route_index: u32,
    /// Complete count or an explicit unavailability reason.
    pub outcome: NativeInputCount,
    /// Count operation time; does not include durable acknowledgement waits.
    pub elapsed_ms: u64,
}

/// Read-only hook and bound-checker validation of a committed rebuilt context.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NativeContextValidationReport {
    /// Canonical request whose prepared context was reconstructed.
    pub request_id: String,
    /// Whether every read-only guard and frozen request checker allowed it.
    pub allowed: bool,
    /// Controlled SDK error category only; never upstream diagnostic text.
    pub error_code: Option<String>,
    /// Validation time, excluding the intent and outcome checkpoint waits.
    pub elapsed_ms: u64,
}

/// Kept inside the live pipeline; only request digests cross the core boundary.
#[derive(Default)]
pub(crate) struct NativeCountedRequests(std::sync::Mutex<Option<String>>);

impl NativeCountedRequests {
    pub(crate) fn select(&self, count: Option<&NativeInputCount>) -> Result<()> {
        let mut current = self.0.lock().map_err(|_| {
            crate::error::BitrouterError::internal("input count binding unavailable")
        })?;
        *current = match count {
            Some(NativeInputCount::Counted { request_sha256, .. }) => Some(request_sha256.clone()),
            _ => None,
        };
        Ok(())
    }

    pub(crate) fn matches(&self, digest: &str) -> Result<bool> {
        Ok(self
            .0
            .lock()
            .map_err(|_| crate::error::BitrouterError::internal("input count binding unavailable"))?
            .as_deref()
            == Some(digest))
    }
}

/// Credential-free facts about one concrete provider/model candidate.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NativeRouteConstraints {
    /// Empty means unknown; a nonempty list is a positive capability inventory.
    pub capabilities: Vec<Capability>,
    /// Known limits for this exact route.
    pub token_limits: ModelTokenLimits,
    /// Opt-in counting contract for this exact provider/model.
    #[serde(default)]
    pub input_token_counting: Option<InputTokenCounting>,
    /// No source means that the routing table supplied no authoritative facts.
    pub source: Option<String>,
}

/// Ordered subset of a frozen plan's candidates admitted by the embedding
/// runtime. Indices keep their original identity in attempt reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativePlanAdmission {
    /// Strictly increasing indices into the plan's original route chain.
    pub route_indices: Vec<u32>,
}

impl NativePlanAdmission {
    pub(crate) fn validate(&self, route_count: usize) -> Result<()> {
        if self.route_indices.is_empty()
            || self.route_indices.windows(2).any(|pair| pair[0] >= pair[1])
            || self
                .route_indices
                .iter()
                .any(|index| *index as usize >= route_count)
        {
            return Err(crate::error::BitrouterError::bad_request(
                "managed admission must preserve an ordered nonempty route subset",
            ));
        }
        Ok(())
    }
}

/// Whether a managed request permits policy to choose its effective model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NativeModelSelection {
    /// Resolve the requested model without an effective-model policy.
    Fixed,
    /// Permit the named router's model policy to select an effective model.
    Policy,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
/// Credential-free serving identity for a selected provider candidate.
pub struct NativeRoute {
    /// Configured provider identity.
    pub provider: String,
    /// Provider service/model identity.
    pub model: String,
    /// Provider wire protocol.
    pub protocol: ApiProtocol,
    /// Facts captured before the runtime commits its admission decision.
    pub constraints: NativeRouteConstraints,
    /// Executor/provider declaration; final wire validation is still required.
    pub output_token_limit_supported: Option<bool>,
    /// Counts belong to the prepared request and this concrete route.
    #[serde(default)]
    pub input_count: Option<NativeInputCount>,
}

impl NativeRoute {
    pub(crate) fn from_target(target: &RoutingTarget) -> Self {
        Self {
            provider: target.provider_name.clone(),
            model: target.service_id.clone(),
            protocol: target.api_protocol.clone(),
            constraints: target.model_constraints.clone(),
            output_token_limit_supported: None,
            input_count: None,
        }
    }
}

/// A managed request's immutable output bound, checked after provider shaping
/// and authentication, immediately before the HTTP request can be sent.
pub(crate) struct NativeOutputReservation(pub u32);

/// A redacted snapshot after shared auth, prompt preparation and model policy.
/// The pipeline executes this exact prompt and route chain without rerunning
/// selection after the embedding runtime acknowledges the plan.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NativePlan {
    /// SDK request correlation identity.
    pub request_id: String,
    /// Selector before shared ingress preparation.
    pub original_model: String,
    /// Resolved effective model after the single policy selection.
    pub effective_model: String,
    /// Preserved separately because canonical params skip transient ownership
    /// during serialization; a checkpoint must not turn policy into caller input.
    pub effort_source: ReasoningEffortSource,
    /// Exact prepared model input.
    pub prompt: Prompt,
    /// Ordered fallback candidates; credentials are deliberately excluded.
    pub routes: Vec<NativeRoute>,
    /// Named-router binding frozen before model selection.
    pub router: Option<RouterRequestIdentity>,
}

/// Complete outcome of one actual provider attempt, before SDK settlement.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NativeAttemptReport {
    /// SDK request correlation identity.
    pub request_id: String,
    /// Zero-based position in the frozen route chain.
    pub attempt_index: u32,
    /// Selected provider candidate for this attempt.
    pub route: NativeRoute,
    /// Actual execution model, when an upstream result is available.
    pub actual_model: Option<String>,
    /// Complete output and optional reported usage. Missing usage stays unknown.
    pub result: Option<GenerateResult>,
    /// Failure detail when no successful upstream result was obtained.
    pub error: Option<String>,
    /// Provider execution wall time, excluding durable admission waits.
    pub elapsed_ms: u64,
}

/// Per-request durable controls supplied by a native embedding runtime.
#[async_trait]
pub trait NativeExecutionControl: Send + Sync {
    /// Fixed selection still resolves aliases and provider fallback, but skips
    /// effective-model policy. Explicit effort remains caller-owned in either mode.
    fn model_selection(&self) -> NativeModelSelection {
        NativeModelSelection::Policy
    }

    /// Persist the exact prepared input and count intent before contacting a
    /// configured provider counter. Recheck source, permission and cancellation.
    async fn before_input_count(&self, _plan: &NativePlan, _route_index: u32) -> Result<()> {
        Ok(())
    }

    /// Preserve count evidence before another count or final plan admission.
    async fn after_input_count(&self, _report: NativeInputCountReport) -> Result<()> {
        Ok(())
    }

    /// Persist intent and check dispatch gates before rebuilt-context validation.
    async fn before_context_validation(&self, _request_id: &str) -> Result<()> {
        Ok(())
    }

    /// Recheck the live gate before each hook/checker within that durable intent.
    async fn check_context_validation(&self, _request_id: &str) -> Result<()> {
        Ok(())
    }

    /// Validate embedding preparation contracts within an acknowledged validation
    /// intent. The pipeline freezes every field except whole-message removal.
    async fn validate_context_rebuild(
        &self,
        _original: &Prompt,
        _rebuilt: &Prompt,
        _request_id: &str,
    ) -> Result<()> {
        Ok(())
    }

    /// Commit validation outcome before a new count or provider attempt.
    async fn after_context_validation(&self, _report: NativeContextValidationReport) -> Result<()> {
        Ok(())
    }

    /// Commit candidate assessments and return an ordered nonempty subset of
    /// the frozen routes, or reject the plan before any provider attempt.
    async fn plan(&self, plan: NativePlan) -> Result<NativePlanAdmission>;

    /// After rejecting a plan with no provider attempt, optionally commit one
    /// rebuilt context. Only complete messages may be removed; input additions,
    /// model/effort changes and generation-parameter changes are not permitted.
    /// The pipeline re-counts and re-admits the frozen provider chain without
    /// rerunning authentication, preparation, model selection or route hooks.
    /// Frozen request checks run again before any new count or generation.
    /// Provider continuation disables this callback. Every preparation/route
    /// hook must implement its read-only revalidation contract to proceed.
    async fn rebuild_context(&self, _rejected: &NativePlan) -> Result<Option<Vec<Message>>> {
        Ok(None)
    }

    /// Authorize every actual attempt, including fallbacks, after persisting
    /// its identity and budget reservation. A previous uncommitted outcome
    /// must prevent authorization of the next attempt.
    async fn before_attempt(&self, request_id: &str, attempt_index: u32) -> Result<()>;

    /// Preserve complete outcome/usage before another attempt is considered.
    /// Observation failure must close subsequent admission in the control
    /// implementation. It must not suppress SDK settlement for a billed call.
    async fn after_attempt(&self, report: NativeAttemptReport);
}
