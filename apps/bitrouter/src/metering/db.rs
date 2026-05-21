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
    /// Estimated charge in micro-USD computed from pricing × tokens. `0`
    /// when pricing was unavailable (the recorder skips the math in that
    /// case and writes 0, plus emits a `PricingUnavailable` event).
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
