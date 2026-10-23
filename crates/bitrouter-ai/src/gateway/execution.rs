//! Credential-free projections of durable gateway work.
//!
//! Execution identity is distinct from client correlation and provider attempts.
//! An unknown outcome does not establish cancellation or permission to replay.

use serde::{Deserialize, Serialize};

use crate::types::{ModelOperation, UsageOrigin};

/// A tenant-authorized snapshot of one durable execution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct GatewayExecution {
    /// Service-generated durable identity, stable across duplicate submissions.
    pub execution_id: String,
    /// Caller-visible request correlation; it is not an idempotency key.
    pub correlation_id: String,
    /// Revision used for conditional operator acknowledgement.
    pub revision: u64,
    /// Operation originally bound to the submission.
    pub operation: ModelOperation,
    /// Immutable routing generation used to admit this execution.
    pub config_generation: u64,
    /// Database-clock admission time, as Unix milliseconds.
    pub accepted_at_ms: i64,
    /// Database-clock time of the last durable state change.
    pub updated_at_ms: i64,
    /// Provider completion and delivery are separate observations.
    pub state: ExecutionState,
    /// Bounded list of attempts; this contains no credentials or response bodies.
    pub attempts: Vec<GatewayAttempt>,
}

/// Durable state; unknown work deliberately carries no terminal result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum ExecutionState {
    /// Admission committed, with no observed provider dispatch yet.
    Accepted,
    /// At least one attempt is owned by a live worker.
    Running,
    /// The provider may still be running; retained reservations remain binding.
    Unknown {
        /// Bounded service code, never a raw provider diagnostic.
        reason: String,
    },
    /// A terminal observation committed exactly once in shared storage.
    Terminal {
        /// Durable provider/accounting evidence, distinct from client delivery.
        result: TerminalObservation,
    },
    /// An authorized operator reconciled work whose provider outcome was unknown.
    Resolved {
        /// The audited operator assertion; it is not a provider observation.
        resolution: OperatorResolution,
    },
}

/// Stable identity and conservative state of a single provider call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct GatewayAttempt {
    /// Service-generated identity, distinct from execution and continuation IDs.
    pub attempt_id: String,
    /// Non-secret shared resource identity selected for the actual account.
    pub resource_pool_id: String,
    /// Revision of the credential authority bound before dispatch.
    pub credential_revision: u64,
    /// Whether provider termination has actually been established.
    pub state: AttemptState,
}

/// Dispatch intent is not proof that a provider accepted or completed work.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum AttemptState {
    /// Resource reserved; no upstream dispatch has been authorized.
    Prepared,
    /// Durable intent exists; the upstream may have accepted the request.
    Dispatched,
    /// A bounded provider response established a terminal outcome.
    Terminal {
        /// Usage/cost evidence remains explicitly optional.
        result: TerminalObservation,
    },
    /// No provider dispatch was authorized before the reservation was released.
    NotDispatched,
    /// Dispatch may have occurred and no terminal evidence is available.
    Unknown {
        /// Bounded service code, excluding raw errors and secret values.
        reason: String,
    },
    /// Operator evidence released the original reservation without replay.
    Resolved {
        /// Auditable acknowledgement for the original attempt.
        resolution: OperatorResolution,
    },
}

/// One committed terminal fact; it does not assert that a client received it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct TerminalObservation {
    /// Observed terminal outcome, without raw provider response/error content.
    pub outcome: TerminalOutcome,
    /// Observed usage and price; absent values are unknown, not zero.
    pub usage: ExecutionUsage,
    /// Database-clock persistence time as Unix milliseconds.
    pub recorded_at_ms: i64,
}

/// Provider termination is separate from successful response delivery.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum TerminalOutcome {
    /// A successful provider terminal was observed.
    Succeeded,
    /// A definite error or local pre-dispatch failure was observed.
    Failed {
        /// Stable, bounded service error classification.
        code: String,
    },
}

/// Content-free accounting facts, never an operator balance or invoice.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ExecutionUsage {
    /// Provenance of the token evidence.
    pub origin: UsageOrigin,
    /// Observed prompt/input tokens, if available.
    pub prompt_tokens: Option<u64>,
    /// Observed completion/output tokens, if available.
    pub completion_tokens: Option<u64>,
    /// Observed reasoning tokens, if separately reported.
    pub reasoning_tokens: Option<u64>,
    /// Observed prompt tokens read from provider cache.
    pub cache_read_tokens: Option<u64>,
    /// Observed prompt tokens written into provider cache.
    pub cache_write_tokens: Option<u64>,
    /// Observed total; absence cannot be interpreted as zero.
    pub total_tokens: Option<u64>,
    /// Auditable observed cost; unknown price/usage remains absent.
    pub cost_micro_usd: Option<u64>,
}

/// Explicit operator acknowledgement of an ambiguous provider attempt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ResolveAttemptRequest {
    /// Reject stale management decisions instead of overwriting newer evidence.
    pub expected_revision: u64,
    /// Provider work must have been checked before releasing its reservation.
    pub outcome: ResolutionOutcome,
    /// Bounded external receipt/case reference; never a credential or raw prompt.
    pub evidence_reference: String,
}

/// Audited operator evidence, distinct from gateway-observed provider facts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct OperatorResolution {
    /// Identity established by the separate operator authentication surface.
    pub operator_id: String,
    /// Explicit operator assertion, not a synthesized provider response.
    pub outcome: ResolutionOutcome,
    /// Bounded external receipt or case identifier.
    pub evidence_reference: String,
    /// Database-clock persistence time as Unix milliseconds.
    pub recorded_at_ms: i64,
}

/// Assertions accepted only from an authorized operator reconciliation path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum ResolutionOutcome {
    /// External evidence establishes that no provider work was accepted.
    NotAccepted,
    /// External evidence establishes provider termination; usage remains unknown.
    Terminated,
}

/// At-least-once terminal event; receivers deduplicate using `event_id`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct GatewayTerminalEvent {
    /// Stable identity preserved across redelivery and lost acknowledgements.
    pub event_id: String,
    /// Authenticated owning tenant; no credential material is included.
    pub tenant_id: String,
    /// Immutable snapshot committed with the terminal accounting effects.
    pub execution: GatewayExecution,
}

#[cfg(test)]
mod tests {
    use super::{ExecutionState, ExecutionUsage};

    #[test]
    fn unknown_state_rejects_claimed_terminal_evidence() {
        let contradictory = serde_json::json!({
            "status": "unknown", "reason": "owner_lost", "result": {}
        });
        assert!(serde_json::from_value::<ExecutionState>(contradictory).is_err());
    }

    #[test]
    fn missing_usage_is_not_a_zero_charge() -> Result<(), serde_json::Error> {
        let value = serde_json::json!({
            "origin": "unknown", "prompt_tokens": null, "completion_tokens": null,
            "reasoning_tokens": null, "cache_read_tokens": null, "cache_write_tokens": null,
            "total_tokens": null, "cost_micro_usd": null
        });
        let usage: ExecutionUsage = serde_json::from_value(value)?;
        assert_eq!(usage.total_tokens, None);
        assert_eq!(usage.cost_micro_usd, None);
        Ok(())
    }
}
