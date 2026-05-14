//! `MetricsStore` — usage metrics infrastructure (crate root).
//!
//! See design doc 003 §4.7. `MetricsStore` is **infrastructure, not a hook**:
//! it is injected via `App::builder()` alongside `RoutingTable`. The authoritative
//! writer is the Settlement stage's `ReceiptRecorder`; the readers are
//! PreRequest hooks (`PolicyHook` / `RateLimitHook`) doing spend/rate gating.

use crate::Result;
use crate::caller::FundingSource;
use async_trait::async_trait;
use chrono::{DateTime, Utc};

/// A rolling time window for usage queries.
#[derive(Debug, Clone, Copy)]
pub enum TimeWindow {
    /// Trailing 60 seconds.
    LastMinute,
    /// Trailing 60 minutes.
    LastHour,
    /// Since UTC 00:00 today.
    Today,
    /// Since UTC 00:00 Monday this week.
    ThisWeek,
    /// Since UTC 00:00 on the 1st this month.
    ThisMonth,
    /// An explicit `[start, end)` range.
    Custom {
        /// Inclusive start.
        start: DateTime<Utc>,
        /// Exclusive end.
        end: DateTime<Utc>,
    },
}

/// Token usage rollup over a window.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TokenUsage {
    /// Prompt (input) tokens.
    pub prompt_tokens: u64,
    /// Completion (output) tokens.
    pub completion_tokens: u64,
    /// Sum of prompt + completion.
    pub total_tokens: u64,
}

/// Request / token rate over the trailing minute.
#[derive(Debug, Clone, Copy, Default)]
pub struct RateMetrics {
    /// Requests observed in the trailing minute.
    pub requests_per_minute: f64,
    /// Tokens observed in the trailing minute.
    pub tokens_per_minute: f64,
}

/// One settled request, as recorded by `ReceiptRecorder` into the store.
#[derive(Debug, Clone)]
pub struct RequestMetric {
    /// Unique request id.
    pub request_id: String,
    /// API key id that made the request.
    pub api_key_id: String,
    /// Owning user id.
    pub user_id: String,
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
    /// End-to-end latency in milliseconds.
    pub latency_ms: u64,
    /// Upstream generation time in milliseconds.
    pub generation_time_ms: u64,
    /// Final charge in micro-USD (0 for BYOK / unsettled).
    pub final_charge_micro_usd: i64,
    /// Which funding source settled the request.
    pub funding_source: FundingSource,
    /// Whether a BYOK key was applied (derived from the `ByokKeyApplied` event).
    pub byok_used: bool,
    /// Whether the request was streamed.
    pub stream: bool,
    /// Error string if the request failed, else `None`.
    pub error: Option<String>,
}

/// Usage metrics store. The core crate defines the trait; plugins implement it
/// (e.g. `bitrouter-settlement::SqliteMetricsStore`). Injected via `App::builder`.
#[async_trait]
pub trait MetricsStore: Send + Sync {
    /// Total spend (micro-USD) for `key` within `window`.
    async fn get_spend(&self, key: &str, window: TimeWindow) -> Result<u64>;

    /// Request count for `key` within `window`.
    async fn get_request_count(&self, key: &str, window: TimeWindow) -> Result<u64>;

    /// Token usage for `key` on `model` within `window`.
    async fn get_token_usage(
        &self,
        key: &str,
        model: &str,
        window: TimeWindow,
    ) -> Result<TokenUsage>;

    /// Current request/token rate for `key`.
    async fn get_rate(&self, key: &str) -> Result<RateMetrics>;

    /// Record one settled request. Called by `ReceiptRecorder` in the
    /// Settlement stage — ideally in the same transaction as the receipt write.
    async fn record_request(&self, record: RequestMetric) -> Result<()>;
}
