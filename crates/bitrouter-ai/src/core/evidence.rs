//! Provider-attempt evidence carried by the managed core protocol.
//! These are observations, not authority to dispatch, restore ownership, execute
//! tools or settle a bill. Verification and provider adaptation remain in the service.

use crate::types::{ApiProtocol, Capability, GenerateResult, NormalizedUsage, UsageOrigin};
use serde::{Deserialize, Serialize};

/// Independently declared token limits. An input limit is not a combined
/// context window, and an absent limit is unknown rather than unbounded.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
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
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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

/// Credential-free facts about one concrete provider/model candidate.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NativeRouteConstraints {
    /// Positive capability observations, not an exhaustive denylist. Omitted
    /// capabilities remain unknown even when this list is nonempty.
    pub capabilities: Vec<Capability>,
    /// Known limits for this exact route.
    pub token_limits: ModelTokenLimits,
    /// Opt-in counting contract for this exact provider/model.
    #[serde(default)]
    pub input_token_counting: Option<InputTokenCounting>,
    /// No source means that the routing table supplied no authoritative facts.
    pub source: Option<String>,
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
    /// Shared executor/adapter assessment of the actual prepared prompt.
    #[serde(default)]
    pub protocol_validation: NativeProtocolValidation,
    /// Candidate-local continuation choice; no private handle is serialized.
    #[serde(default)]
    pub continuation: NativeContinuationInput,
}

/// Protocol feasibility is distinct from catalog capabilities and token counts.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum NativeProtocolValidation {
    /// An embedding executor has not established a wire-format guarantee.
    #[default]
    Unverified,
    /// The serving adapter can preserve this request's required semantics.
    Compatible,
    /// A local check rejected the candidate before upstream work.
    Rejected {
        /// Stable controlled reason; never an upstream body or private identity.
        reason: String,
    },
}

/// Complete outcome of one actual provider attempt, before gateway settlement.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NativeAttemptReport {
    /// Gateway request correlation identity.
    pub request_id: String,
    /// Zero-based position in the frozen route chain.
    pub attempt_index: u32,
    /// Selected provider candidate for this attempt.
    pub route: NativeRoute,
    /// Actual execution provider, using the same identity as gateway settlement.
    #[serde(default)]
    pub actual_provider: Option<String>,
    /// Actual execution model, when an upstream result is available.
    pub actual_model: Option<String>,
    /// Complete output and optional reported usage. Missing usage stays unknown.
    pub result: Option<GenerateResult>,
    /// Complete output was received but could not enter the embedding runtime.
    /// Its bounded usage summary does not authorize content or tool execution.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_rejection: Option<NativeOutputRejection>,
    /// Complete report metadata was rejected before durable delivery. Its
    /// commitments do not retain readable raw diagnostics or actual identities.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub report_rejection: Option<NativeReportRejection>,
    /// Failure detail when no successful upstream result was obtained.
    pub error: Option<String>,
    /// Provider execution wall time, excluding durable admission waits.
    pub elapsed_ms: u64,
    /// Host token estimate, independent of provider billing and settlement writes.
    #[serde(default)]
    pub token_cost: NativeTokenCost,
    /// Explicit raw provider cache counters with their serving-protocol provenance.
    #[serde(default)]
    pub cache: NativeCacheObservation,
    /// Authenticated private-history evidence from the actual provider attempt.
    #[serde(default)]
    pub private_context: NativePrivateContextObservation,
    /// Actual provider-state use and output artifact evidence.
    #[serde(default)]
    pub continuation: NativeContinuationObservation,
}

impl NativeAttemptReport {
    /// A rejected outcome is terminal and cannot authorize tools or a retry.
    pub fn rejection_reason(&self) -> Option<&'static str> {
        match self
            .report_rejection
            .as_ref()
            .map(|rejected| rejected.reason)
        {
            Some(NativeReportRejectionReason::ByteLimit) => {
                Some("attempt report exceeds its admitted byte limit")
            }
            Some(NativeReportRejectionReason::NonFinitePricing) => {
                Some("attempt report has non-finite pricing evidence")
            }
            None if self.output_rejection.is_some() => {
                Some("canonical model output exceeds its admitted byte limit")
            }
            None => None,
        }
    }
}

/// SHA-256 commitment to the exact serde JSON encoding of a named value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeEvidenceCommitment {
    /// Serialized JSON bytes, including escaping and string quotes.
    pub bytes: u64,
    /// Lowercase hexadecimal SHA-256 of those JSON bytes.
    pub sha256: String,
}

/// Why a complete managed attempt report cannot enter durable state verbatim.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NativeReportRejectionReason {
    /// The serialized report exceeds its frozen allowance.
    ByteLimit,
    /// Pricing floats cannot round-trip through JSON; their bits are retained.
    NonFinitePricing,
}

/// Bounded metadata outcome, after canonical output admission and estimation,
/// before metadata projection. Version 1 hashes the serde JSON representation
/// of NativeAttemptReport with report_rejection absent, not provider wire bytes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeReportRejection {
    /// Commitment and projection schema version.
    pub version: u32,
    /// Frozen complete-report allowance for this attempt.
    pub byte_limit: u64,
    /// Explicit rejection cause, independent of provider execution success.
    pub reason: NativeReportRejectionReason,
    /// Commitment to the report before metadata projection. This differs from
    /// the durable outcome digest, which commits to the projected report.
    pub original: NativeEvidenceCommitment,
    /// Actual execution identities are commitments, never selected-route aliases.
    pub actual_provider: Option<NativeEvidenceCommitment>,
    /// Commitment to the actual served model, when execution supplied one.
    pub actual_model: Option<NativeEvidenceCommitment>,
    /// A complete canonical output was received, possibly already rejected.
    pub had_result: bool,
    /// Original counters without opaque raw usage; the gateway settles the original.
    pub usage: Option<NativeOutputUsage>,
}

/// Bounded evidence for a complete canonical output rejected before delivery.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NativeOutputRejection {
    /// Maximum admitted serialized canonical result size.
    pub byte_limit: u64,
    /// Original canonical counters and provenance, without the raw provider
    /// object. Full usage remains available to the gateway settlement recorders.
    pub usage: Option<NativeOutputUsage>,
}

/// Canonical counters retained without unbounded raw provider metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeOutputUsage {
    /// Original prompt-token total, including cache subsets.
    pub prompt_tokens: u64,
    /// Original completion-token total, including reasoning.
    pub completion_tokens: u64,
    /// Reasoning subset of completion tokens.
    pub reasoning_tokens: u64,
    /// Cache-read subset of prompt tokens.
    pub cache_read_tokens: u64,
    /// Cache-write subset of prompt tokens.
    pub cache_write_tokens: u64,
    /// Provider-reported search calls, independent of token cost.
    pub web_search_count: u64,
    /// Original canonical usage provenance.
    pub origin: crate::types::UsageOrigin,
}

/// Frozen rates used to reproduce a configured token estimate.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NativeTokenRates {
    /// Micro-USD per uncached input token.
    pub uncached_input: Option<f64>,
    /// Micro-USD per cache-read input token.
    pub cache_read: Option<f64>,
    /// Micro-USD per cache-write input token.
    pub cache_write: Option<f64>,
    /// Micro-USD per output token, including reasoning.
    pub output: Option<f64>,
}

/// Lossless pricing floats in a bounded report-rejection summary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeTokenRateBits {
    /// Uncached-input rate as f64::to_bits.
    pub uncached_input: Option<u64>,
    /// Cache-read rate as f64::to_bits.
    pub cache_read: Option<u64>,
    /// Cache-write rate as f64::to_bits.
    pub cache_write: Option<u64>,
    /// Output rate as f64::to_bits.
    pub output: Option<u64>,
}

/// Model-token estimate only: excludes provider tools, storage, counter fees,
/// taxes and account-specific adjustments. It is not a final provider invoice.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum NativeTokenCost {
    /// The host has insufficient evidence to estimate this actual attempt.
    Unknown {
        /// Stable local reason, never upstream diagnostic text.
        reason: String,
    },
    /// The estimate remains unknown; its original diagnostic is committed.
    UnknownCommitment {
        /// Commitment to the original reason string or prior commitment.
        reason: NativeEvidenceCommitment,
    },
    /// The amount and numeric evidence remain available after metadata rejection.
    /// This is still an estimate, never a provider-reported or reconciled bill.
    ConfiguredEstimateCommitment {
        /// Host-estimated micro-USD, including an explicitly estimated zero.
        micro_usd: u64,
        /// Provenance of the counters used by the estimator.
        usage_origin: UsageOrigin,
        /// Non-overlapping input and output counters used for the estimate.
        normalized_usage: NormalizedUsage,
        /// Exact IEEE-754 bits, preserving non-finite evidence without JSON nulls.
        rates: NativeTokenRateBits,
        /// Commitment to original pricing identity fields or a prior commitment.
        pricing_metadata: NativeEvidenceCommitment,
    },
    /// The host applied a frozen configured price to canonical usage.
    ConfiguredEstimate {
        /// Rounded integer micro-USD from the host's shared metering calculation.
        micro_usd: u64,
        /// Origin of the canonical counters used by that calculation.
        usage_origin: UsageOrigin,
        /// Non-overlapping counters used for the estimate.
        normalized_usage: NormalizedUsage,
        /// Effective rates after context-tier inheritance.
        rates: NativeTokenRates,
        /// Digest identifying the complete pricing entry.
        pricing_version: String,
        /// Provider key used to resolve the price.
        pricing_provider: String,
        /// Served model key used to resolve the price.
        pricing_model: String,
    },
}

impl Default for NativeTokenCost {
    fn default() -> Self {
        Self::unknown("accounting_unavailable")
    }
}

impl NativeTokenCost {
    /// Make an explicitly unavailable token estimate.
    pub fn unknown(reason: impl Into<String>) -> Self {
        Self::Unknown {
            reason: reason.into(),
        }
    }

    /// Known configured token estimate, never a claim of an authoritative bill.
    pub fn estimated_micro_usd(&self) -> Option<u64> {
        match self {
            Self::ConfiguredEstimate { micro_usd, .. }
            | Self::ConfiguredEstimateCommitment { micro_usd, .. } => Some(*micro_usd),
            Self::Unknown { .. } | Self::UnknownCommitment { .. } => None,
        }
    }
}

/// Cache counters explicitly present in the serving protocol's raw usage.
/// Missing counters remain `None`, even when the canonical compatibility field
/// defaults to zero. These counters do not establish KV transfer or cost savings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NativeCacheObservation {
    /// Raw observed cache-read tokens, including an explicit observed zero.
    pub read_tokens: Option<u64>,
    /// Raw observed cache-write tokens, including an explicit observed zero.
    pub write_tokens: Option<u64>,
    /// Controlled protocol/provenance label, or `unknown`.
    pub source: String,
    /// Stable reason when no reliable observation can be retained.
    pub unknown_reason: Option<String>,
}

impl Default for NativeCacheObservation {
    fn default() -> Self {
        Self::unknown("cache_usage_unavailable")
    }
}

impl NativeCacheObservation {
    /// Retain an explicit reason without inventing observed cache counters.
    pub fn unknown(reason: &str) -> Self {
        Self {
            read_tokens: None,
            write_tokens: None,
            source: "unknown".into(),
            unknown_reason: Some(reason.into()),
        }
    }
}

/// Controlled rejection/uncertainty reasons, without private identifiers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrivateContextFailure {
    /// No host policy is installed for private history.
    PolicyUnavailable,
    /// The installation key cannot be loaded.
    KeyUnavailable,
    /// A private part has no origin proof.
    ProofMissing,
    /// The proof, message content or part position is invalid.
    ProofInvalid,
    /// The authenticated owner differs from the origin owner.
    OwnerMismatch,
    /// The selected provider/model/endpoint/account differs from the origin.
    TargetMismatch,
    /// Final authentication cannot prove its credential principal.
    AuthorityUnavailable,
    /// Final authentication differs from the origin credential principal.
    AuthorityMismatch,
    /// Private parts must belong to an assistant message.
    InvalidRole,
    /// No successful HTTP result supplies provenance for this output.
    AttemptUnverified,
    /// Content could not be committed or sealed.
    ContentUnavailable,
}

impl PrivateContextFailure {
    /// Stable protocol feasibility diagnostic.
    pub fn reason(self) -> &'static str {
        match self {
            Self::PolicyUnavailable => "private_context_policy_unavailable",
            Self::KeyUnavailable => "private_context_key_unavailable",
            Self::ProofMissing => "private_context_proof_missing",
            Self::ProofInvalid => "private_context_proof_invalid",
            Self::OwnerMismatch => "private_context_owner_mismatch",
            Self::TargetMismatch => "private_context_target_mismatch",
            Self::AuthorityUnavailable => "private_context_authority_unavailable",
            Self::AuthorityMismatch => "private_context_authority_mismatch",
            Self::InvalidRole => "private_context_invalid_role",
            Self::AttemptUnverified => "private_context_attempt_unverified",
            Self::ContentUnavailable => "private_context_content_unavailable",
        }
    }
}

/// Explicit evidence; old receipts without this field remain unknown.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum PrivateContextEvidence {
    /// No observation is available, including an attempt stopped before send.
    #[default]
    Unknown,
    /// The observed input/output contains no private parts.
    NotPresent,
    /// The observed private parts have verified source bindings.
    Verified {
        /// Number of authenticated private parts.
        parts: u64,
    },
    /// The result is retained, but its private context cannot be safely reused.
    Unverified {
        /// Controlled reason for missing provenance.
        reason: PrivateContextFailure,
    },
}

/// Private context actually sent and returned by one managed attempt.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NativePrivateContextObservation {
    /// Set only immediately before a generation request is dispatched.
    pub input: PrivateContextEvidence,
    /// Set before the attempt report and core history receive the result.
    pub output: PrivateContextEvidence,
}

/// Stable diagnostics without provider IDs, keys or content.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContinuationFailure {
    /// No installation policy is configured.
    PolicyUnavailable,
    /// The installation key could not be loaded.
    KeyUnavailable,
    /// The artifact, owner or message binding is invalid.
    ArtifactInvalid,
    /// The authenticated owner differs from the artifact owner.
    OwnerMismatch,
    /// Provider state is required but the visible prefix has changed.
    PrefixMismatch,
    /// Provider state is required but the candidate target differs.
    TargetMismatch,
    /// Provider state is required but the requested effort differs.
    EffortMismatch,
    /// Actual authentication has no stable principal proof.
    AuthorityUnavailable,
    /// Actual authentication differs from the successful source principal.
    AuthorityMismatch,
    /// No authenticated successful HTTP attempt supplied this result.
    AttemptUnverified,
    /// The output cannot carry an authenticated artifact.
    ContentUnavailable,
    /// A request attempted to inject or replace a native provider handle.
    BindingChanged,
    /// Locally retained output needs stored state that no selected handle covers.
    Required,
}

impl ContinuationFailure {
    /// Stable feasibility/error reason, suitable for a redacted receipt.
    pub fn reason(self) -> &'static str {
        match self {
            Self::PolicyUnavailable => "native_continuation_policy_unavailable",
            Self::KeyUnavailable => "native_continuation_key_unavailable",
            Self::ArtifactInvalid => "native_continuation_artifact_invalid",
            Self::OwnerMismatch => "native_continuation_owner_mismatch",
            Self::PrefixMismatch => "native_continuation_prefix_mismatch",
            Self::TargetMismatch => "native_continuation_target_mismatch",
            Self::EffortMismatch => "native_continuation_effort_mismatch",
            Self::AuthorityUnavailable => "native_continuation_authority_unavailable",
            Self::AuthorityMismatch => "native_continuation_authority_mismatch",
            Self::AttemptUnverified => "native_continuation_attempt_unverified",
            Self::ContentUnavailable => "native_continuation_content_unavailable",
            Self::BindingChanged => "native_continuation_binding_changed",
            Self::Required => "native_continuation_required",
        }
    }
}

/// Why complete canonical history is sent instead of a stored provider handle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FullHistoryReason {
    /// No authenticated handle is present in the input.
    NoHandle,
    /// The candidate has a different provider/model/endpoint/account/protocol.
    TargetChanged,
    /// The requested reasoning effort changed.
    EffortChanged,
    /// The artifact no longer describes the exact ordered history prefix.
    PrefixChanged,
}

/// Credential-free planned or observed provider context use.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum NativeContinuationInput {
    /// No observation, including an attempt stopped before dispatch.
    #[default]
    Unknown,
    /// Complete history is sent through the serving adapter.
    FullHistory {
        /// Local reason for sending complete history.
        reason: FullHistoryReason,
    },
    /// The provider receives only the suffix after this many canonical messages.
    Resumed {
        /// Number of messages represented by the provider's stored state.
        prefix_messages: u64,
    },
    /// Local candidate preparation failed before dispatch.
    Rejected {
        /// Stable local rejection, without private values.
        reason: ContinuationFailure,
    },
}

/// Evidence for the handle attached to this complete output.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum NativeContinuationOutput {
    /// No output observation exists.
    #[default]
    Unknown,
    /// The protocol has no Responses handle.
    NotSupported,
    /// The actual response did not confirm stored state.
    NotStored,
    /// An encrypted owner/source/prefix-bound artifact was issued.
    Issued,
    /// Output and usage survive, but no reusable artifact could be issued.
    Unverified {
        /// Why provenance or stored state could not be established.
        reason: ContinuationFailure,
    },
}

/// Actual generation evidence; counting alone never populates this observation.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NativeContinuationObservation {
    /// Actual generation dispatch, separately from planned/count work.
    pub input: NativeContinuationInput,
    /// Artifact attached to the complete output before its receipt is emitted.
    pub output: NativeContinuationOutput,
}
