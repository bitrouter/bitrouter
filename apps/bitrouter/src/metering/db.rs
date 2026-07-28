//! The `RequestMetric` row — the typed shape the recorder writes and the
//! store persists into the `requests` table.
//!
//! v1 schema: a single `requests` table that records pipeline-observed
//! usage + the estimated micro-USD computed from the pricing table. No
//! charge state, no funding source — those are billing concepts, and
//! this OSS module measures, not bills.
//!
//! The `requests` table is created by the sea-orm migrations in
//! `crate::db::migration` (including the in-place rename of the
//! pre-OSS-refactor `final_charge_micro_usd` column); this module only
//! reads and writes rows.

use bitrouter_sdk::language_model::UsageOrigin;
use serde::{Deserialize, Serialize};

use super::pricing::{ChargeEvidence, ChargeStatus};

/// Whether a local metering row requires and has received authoritative
/// request-scoped reconciliation.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReconciliationStatus {
    /// The selected provider does not expose request-scoped settlement.
    #[default]
    NotApplicable,
    /// The request must not enter a benchmark artifact yet.
    Pending,
    /// An authoritative computed receipt has been applied.
    Computed,
    /// An authoritative receipt confirms no charge.
    NotCharged,
    /// Reconciliation terminated without usable evidence.
    Unknown,
}

impl ReconciliationStatus {
    /// Stable database and JSON representation.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NotApplicable => "not_applicable",
            Self::Pending => "pending",
            Self::Computed => "computed",
            Self::NotCharged => "not_charged",
            Self::Unknown => "unknown",
        }
    }

    /// Parse a persisted value, failing closed for unrecognized values.
    pub fn from_persisted(value: &str) -> Self {
        match value {
            "not_applicable" => Self::NotApplicable,
            "pending" => Self::Pending,
            "computed" => Self::Computed,
            "not_charged" => Self::NotCharged,
            _ => Self::Unknown,
        }
    }
}

/// One settled request, as recorded by [`super::MeteringRecorder`].
#[derive(Debug, Clone)]
pub struct RequestMetric {
    /// Unique request id.
    pub request_id: String,
    /// Owning user id.
    pub user_id: String,
    /// API key id that made the request.
    pub api_key_id: String,
    /// Resolved model id.
    pub model_id: String,
    /// Resolved provider id.
    pub provider_id: String,
    /// Prompt tokens consumed.
    pub prompt_tokens: u64,
    /// Completion tokens consumed.
    pub completion_tokens: u64,
    /// Reasoning tokens consumed (subset of completion on most providers).
    pub reasoning_tokens: u64,
    /// Cache-read prompt tokens.
    pub cache_read_tokens: u64,
    /// Cache-write prompt tokens.
    pub cache_write_tokens: u64,
    /// Uncached input tokens after cache subsets are removed.
    pub uncached_input_tokens: u64,
    /// Non-reasoning output tokens.
    pub output_tokens: u64,
    /// Usage provenance.
    pub usage_origin: UsageOrigin,
    /// Verbatim provider usage object.
    pub raw_usage: Option<serde_json::Value>,
    /// Whether the charge is computed or unknown.
    pub charge_status: ChargeStatus,
    /// Auditable normalization and pricing evidence.
    pub charge_evidence: ChargeEvidence,
    /// Whether this row requires authoritative request reconciliation.
    pub reconciliation_status: ReconciliationStatus,
    /// Estimated charge in micro-USD computed from pricing × tokens. Consult
    /// `charge_status` before using this legacy non-null numeric column: its
    /// stored value is `0` when the auditable charge is unknown.
    pub estimated_charge_micro_usd: i64,
    /// End-to-end latency in milliseconds.
    pub latency_ms: u64,
    /// Upstream generation time in milliseconds.
    pub generation_time_ms: u64,
    /// Whether the request was streamed.
    pub streamed: bool,
    /// Error string if the request failed, else `None`.
    pub error: Option<String>,
}
