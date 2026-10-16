//! Durable admission for embedding runtimes that own their model/tool loop.
//! The callbacks never execute tools and never receive provider credentials.

use async_trait::async_trait;
use bitrouter_ai::core::evidence::{
    NativeAttemptReport, NativeEvidenceCommitment, NativeInputCount, NativeRoute,
};
use serde::{Deserialize, Serialize};

use crate::gateway::error::Result;
use crate::gateway::pipeline::types::RoutingTarget;
use bitrouter_ai::routing::RouterRequestIdentity;
use bitrouter_ai::types::{Message, Prompt, ReasoningEffortSource};

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
    /// Complete count metadata was rejected; its token count is evidence only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub report_rejection: Option<NativeInputCountRejection>,
}

/// Versioned evidence for count metadata that cannot enter the durable runtime.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeInputCountRejection {
    /// Version 1 hashes the serde JSON report with report_rejection absent.
    pub version: u32,
    /// Complete report limit admitted before executing the counter.
    pub byte_limit: u64,
    /// Commitment to the complete report before metadata projection.
    pub original: NativeEvidenceCommitment,
    /// Original counted tokens, without a usable request binding or fit claim.
    pub input_tokens: Option<u64>,
}

impl NativeInputCountRejection {
    /// Unavailable routing outcome paired with a rejected count report.
    pub const REASON: &'static str = "input count report exceeds its admitted byte limit";
}

/// Version-1 auxiliary report allowance, including a complete request identity.
/// Preparation/provider error codes use the SDK's controlled vocabulary; count
/// metadata has explicit rejection evidence if the complete report is larger.
pub fn auxiliary_report_allowance(request_id: &str) -> Result<u64> {
    crate::gateway::pipeline::native_report::commitment(&request_id)?
        .bytes
        .checked_add(4096)
        .ok_or_else(|| {
            crate::gateway::error::BitrouterError::internal("auxiliary report allowance exhausted")
        })
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
    /// Validation wall time, excluding the intent and outcome checkpoint waits.
    /// Intervening live-gate callbacks can still wait for concurrent commits.
    pub elapsed_ms: u64,
    /// Validation time excluding all live-gate callbacks as well. Absent when
    /// the embedding control does not measure them; legacy wall time is not a
    /// substitute for this observation.
    #[serde(default)]
    pub work_elapsed_ms: Option<u64>,
}

/// Kept inside the live pipeline; only request digests cross the core boundary.
#[derive(Default)]
pub(crate) struct NativeCountedRequests(std::sync::Mutex<Option<String>>);

impl NativeCountedRequests {
    pub(crate) fn select(&self, count: Option<&NativeInputCount>) -> Result<()> {
        let mut current = self.0.lock().map_err(|_| {
            crate::gateway::error::BitrouterError::internal("input count binding unavailable")
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
            .map_err(|_| {
                crate::gateway::error::BitrouterError::internal("input count binding unavailable")
            })?
            .as_deref()
            == Some(digest))
    }
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
            return Err(crate::gateway::error::BitrouterError::bad_request(
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

/// A managed request's immutable output bound, checked after provider shaping
/// and authentication, immediately before the HTTP request can be sent.
pub(crate) struct NativeOutputReservation(pub u32);

/// Every controlled call carries this marker, even before output admission.
pub(crate) struct NativeManagedRequest;

/// Maximum decoded HTTP entity bytes consumed for one managed provider reply.
pub(crate) struct NativeResponseByteLimit(pub u64);

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

/// Per-request durable controls supplied by a native embedding runtime.
#[async_trait]
pub trait NativeExecutionControl: Send + Sync {
    /// Supply acknowledged semantic evidence and authorized context candidates
    /// after entry authentication and checks, before the shared policy planner.
    async fn prepare_routing(
        &self,
        _prompt: &Prompt,
    ) -> Result<Option<crate::gateway::routing::preparation::Prepared>> {
        Ok(None)
    }

    /// Acknowledge the exact shared model/context plan before dispatch proceeds.
    async fn commit_routing(&self, _plan: &crate::gateway::routing::plan::Plan) -> Result<()> {
        Ok(())
    }

    /// Collect the admitted attempt from provider streaming while retaining the
    /// same durable plan and complete-result barrier. Display observations do
    /// not authorize actions. Once a stream opens it is never retried.
    fn observe_stream(&self) -> bool {
        false
    }

    /// Bounded display-only observation after stream policy. Implementations
    /// must not use partial tool arguments as execution authority.
    async fn on_stream_part(&self, _request_id: &str, _part: &bitrouter_ai::types::StreamPart) {}

    /// Bound the HTTP executor's complete decoded response body, including SSE
    /// framing and terminal events. This also bounds unsuccessful response bodies.
    /// The bound is checked before passing each chunk to JSON/SSE parsing. It is
    /// not an allocation accounting guarantee for custom executors or hooks.
    fn provider_response_byte_limit(&self) -> Option<u64> {
        None
    }

    /// Bound canonical results before copying them into durable reports,
    /// including results from custom executors. The original execution still
    /// settles; rejection must not trigger another provider attempt. This does
    /// not bound allocations inside the executor or trusted extension hooks.
    fn canonical_output_byte_limit(&self) -> Option<u64> {
        None
    }

    /// Bound the complete report, including diagnostics and estimator metadata.
    /// The selected route and request ID remain intact. A rejected report keeps
    /// bounded commitments and accounting evidence; original SDK settlement is
    /// still required. Returning no limit preserves legacy embedding behavior.
    fn attempt_report_byte_limit(
        &self,
        _request_id: &str,
        _route: &NativeRoute,
    ) -> Result<Option<u64>> {
        Ok(None)
    }

    /// Request cancellation of active provider I/O. The SDK still reports the
    /// resulting attempt and runs settlement; cancellation does not prove that
    /// the provider performed no work. Ordinary callers never trigger this.
    async fn provider_cancelled(&self) {
        std::future::pending::<()>().await;
    }

    /// Commit callback intent and recheck live source, cancellation and limits.
    async fn before_preparation_work(
        &self,
        _work: &super::native_preparation::NativePreparationWork,
    ) -> Result<()> {
        Ok(())
    }

    /// Record success/failure even after cancellation; a failed ACK stops the
    /// next preparation callback and all dependent model execution.
    async fn after_preparation_work(
        &self,
        _report: super::native_preparation::NativePreparationWorkReport,
    ) -> Result<()> {
        Ok(())
    }

    /// Optional complete auxiliary-outcome limit, checked before callback or
    /// counter execution. Legacy embeddings retain their existing contract.
    fn auxiliary_report_byte_limit(&self, _request_id: &str) -> Result<Option<u64>> {
        Ok(None)
    }

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

    /// Total time spent in live-gate callbacks since before_context_validation.
    /// Includes checks invoked by App transforms and must be request-local.
    fn context_validation_gate_duration(&self) -> Option<std::time::Duration> {
        None
    }

    /// Validate embedding preparation contracts within an acknowledged validation
    /// intent. The pipeline permits only whole-message removal or an explicitly
    /// authorized deterministic projection of authenticated Responses reasoning.
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
    /// rebuilt context. Complete messages or explicitly authorized Responses
    /// reasoning may be removed; arbitrary part rewrites, input additions,
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

    /// Persist integration work before any actual authentication or HTTP I/O.
    /// Implementations recheck live dispatch gates and reserve internal retries.
    async fn before_provider_work(
        &self,
        _work: &super::native_work::NativeProviderWork,
    ) -> Result<()> {
        Ok(())
    }

    /// Persist integration outcome. ACK failure must close subsequent admission
    /// without suppressing response consumption and SDK usage settlement.
    async fn after_provider_work(&self, _report: super::native_work::NativeProviderWorkReport) {}

    /// Preserve complete outcome/usage before another attempt is considered.
    /// Observation failure must close subsequent admission in the control
    /// implementation. It must not suppress SDK settlement for a billed call.
    async fn after_attempt(&self, report: NativeAttemptReport);
}

pub(crate) fn route_from_target(target: &RoutingTarget) -> NativeRoute {
    NativeRoute {
        provider: target.provider_name.clone(),
        model: target.service_id.clone(),
        protocol: target.api_protocol.clone(),
        constraints: target.model_constraints.clone(),
        output_token_limit_supported: None,
        input_count: None,
        protocol_validation: Default::default(),
        continuation: Default::default(),
    }
}
