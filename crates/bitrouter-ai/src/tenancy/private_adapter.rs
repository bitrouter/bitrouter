//! Private, operator-paired generation adapter wire values.
//!
//! These values are claims, including after successful JSON decoding. Receivers
//! must authenticate the envelope and bind the attempt to live authority.

use serde::{Deserialize, Serialize};

use super::{
    configuration::ResourceRef,
    identity::{
        AttemptId, CorrelationId, ExecutionId, InstallationId, InstanceId, PrincipalId, RevisionId,
        TenantId,
    },
};

/// Base64url JSON and base64url HMAC, without padding, separated by a dot.
pub const ATTEMPT_HEADER: &str = "x-bitrouter-adapter-attempt";
/// The same envelope format, bound to the raw 32-byte attempt HMAC.
pub const OUTCOME_HEADER: &str = "x-bitrouter-adapter-outcome";
/// Maximum encoded size of either header value.
pub const MAX_HEADER_BYTES: usize = 4096;
/// Prefix for HMAC-SHA256 over the exact decoded attempt JSON bytes.
pub const ATTEMPT_DOMAIN: &[u8] = b"bitrouter.private-adapter.attempt.v1\0";
/// Prefix for HMAC-SHA256 over the attempt HMAC and exact outcome JSON bytes.
pub const OUTCOME_DOMAIN: &[u8] = b"bitrouter.private-adapter.outcome.v1\0";
/// Upper bound on the lifetime of a newly minted send claim.
pub const MAX_ATTEMPT_LIFETIME_MS: u64 = 30_000;

/// One physical generation send from an ordered, profiled execution.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Attempt {
    pub audience: String,
    pub installation_id: InstallationId,
    pub instance_id: InstanceId,
    pub tenant_id: TenantId,
    pub principal_id: PrincipalId,
    pub execution_revision: RevisionId,
    pub execution_id: ExecutionId,
    /// Lookup label only. It neither authorizes nor deduplicates requests.
    pub correlation_id: CorrelationId,
    pub attempt_id: AttemptId,
    pub route: ResourceRef,
    pub profile: ResourceRef,
    pub credential: ResourceRef,
    /// Position in the immutable retained root.
    pub ordinal: u16,
    /// Zero-based position in the submitted ordered subset.
    pub rank: u16,
    pub method: String,
    pub endpoint: String,
    /// Base64url SHA-256 of the exact outbound HTTP body.
    pub body_sha256: String,
    pub stream: bool,
    /// Epoch milliseconds. The peer must additionally enforce its live plan's lifetime.
    pub expires_at_ms: u64,
}

/// An authenticated assertion about the same physical send.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Outcome {
    pub disposition: Disposition,
    pub status: u16,
    /// Safe Retry-After delta in whole seconds, when present on the wire.
    pub retry_after: Option<u64>,
}

/// Status classifications permitted by the private adapter contract.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Disposition {
    /// A terminal non-streaming provider response was observed.
    Completed,
    /// The provider accepted an SSE response. The normal stream decoder still
    /// requires a real terminal and handles disconnects as incomplete.
    Streaming,
    /// The provider gave a definitive HTTP rejection.
    ProviderRejected,
    /// The peer refused before sending to the provider.
    Refused,
    /// A definitive provider disposition could not be established.
    Indeterminate,
}
