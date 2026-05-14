//! `ReceiptRecorder` — a `language_model::SettlementRecorder` that records a
//! receipt for **every** request (success or failure) by feeding the
//! `MetricsStore`.
//!
//! cloud #207 / #198 lessons:
//! - every billing + identity column is written (`user_id`, `api_key_id`,
//!   `final_charge_micro_usd`, `funding_source`, …) — never left NULL;
//! - failed requests are recorded too, with a non-empty `error` — they are not
//!   silently dropped.

use std::sync::Arc;

use async_trait::async_trait;

use bitrouter_sdk::Result;
use bitrouter_sdk::language_model::{SettlementContext, SettlementRecorder};
use bitrouter_sdk::metrics::{MetricsStore, RequestMetric};

/// Always-run settlement recorder. Holds the `MetricsStore` it writes through —
/// the store is the single writer of the `requests` table, so the receipt and
/// the usage metric are one and the same row.
pub struct ReceiptRecorder {
    metrics_store: Arc<dyn MetricsStore>,
}

impl ReceiptRecorder {
    /// Build a `ReceiptRecorder` over a `MetricsStore`.
    pub fn new(metrics_store: Arc<dyn MetricsStore>) -> Self {
        Self { metrics_store }
    }
}

#[async_trait]
impl SettlementRecorder for ReceiptRecorder {
    async fn record(&self, ctx: &SettlementContext) -> Result<()> {
        let metric = RequestMetric {
            request_id: ctx.request_id.clone(),
            // identity columns — always populated (cloud #198)
            user_id: ctx.caller.user_id().to_string(),
            api_key_id: ctx.caller.api_key_id().to_string(),
            model_id: ctx.model_id.clone(),
            provider_id: ctx.provider_id.clone(),
            prompt_tokens: ctx.prompt_tokens,
            completion_tokens: ctx.completion_tokens,
            reasoning_tokens: ctx.reasoning_tokens,
            latency_ms: ctx.latency_ms,
            generation_time_ms: ctx.generation_time_ms,
            // billing columns — always populated (cloud #207)
            final_charge_micro_usd: ctx.final_charge_micro_usd,
            funding_source: ctx.funding_source,
            byok_used: ctx.byok_used,
            stream: ctx.streamed,
            // failed requests are recorded with a non-empty error (cloud #198)
            error: ctx.error.as_ref().map(|e| e.to_string()),
        };
        self.metrics_store.record_request(metric).await
    }
}
