//! Route facts and continuation diagnostics shared by application components.
//! Verification and provider adaptation remain in the service.

use bitrouter_ai::types::Capability;
use serde::{Deserialize, Serialize};

use crate::config::model::{InputTokenCounting, ModelTokenLimits};

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
    /// The artifact does not match the exact ordered history prefix.
    PrefixChanged,
}
