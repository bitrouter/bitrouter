//! `MeteringRecorder` — the OSS `SettlementRecorder`.
//!
//! For every settled request (success or failure):
//! 1. Compute the estimated micro-USD from the pricing table + token counts
//!    via [`super::calculate_charge_micro_usd`]. When pricing is missing
//!    for the resolved `(provider, service_id)`, write 0 and log a
//!    warning — never silently bill the zero, never block the request.
//! 2. Write a `RequestMetric` row to [`super::MeteringStore`].
//!
//! No charging, no balance check, no funding-source selection. Those are
//! deployment-specific; if the OSS deployment needs a hard spend cap, it
//! goes through `apps/bitrouter/src/policy/` reading `MeteringStore`.

use std::sync::Arc;

use async_trait::async_trait;

use bitrouter_sdk::Result;
use bitrouter_sdk::language_model::{SettlementContext, SettlementRecorder, Usage};

use crate::metering::db::RequestMetric;
use crate::metering::pricing::{PricingTable, calculate_charge_micro_usd};
use crate::metering::store::MeteringStore;

/// Always-run settlement recorder writing through [`MeteringStore`].
pub struct MeteringRecorder {
    store: MeteringStore,
    pricing: Arc<PricingTable>,
}

impl MeteringRecorder {
    /// Build a recorder over the shared `MeteringStore` and a
    /// `(provider, service_id) → ModelPricing` table.
    pub fn new(store: MeteringStore, pricing: Arc<PricingTable>) -> Self {
        Self { store, pricing }
    }

    fn estimate_charge(&self, ctx: &SettlementContext) -> (i64, bool) {
        // Compose a `Usage` from the SettlementContext tokens. We don't
        // need every bucket the SDK exposes (cache splits are stored in
        // the row but not yet billed differently) — just prompt +
        // completion, which is what `calculate_charge_micro_usd` reads.
        let usage = Usage {
            prompt_tokens: ctx.prompt_tokens,
            completion_tokens: ctx.completion_tokens,
            reasoning_tokens: ctx.reasoning_tokens,
            cache_read_tokens: ctx.cache_read_tokens,
            cache_write_tokens: ctx.cache_write_tokens,
        };
        match self.pricing.resolve(&ctx.provider_id, &ctx.model_id) {
            Some(pricing) if !pricing.is_unconfigured() => {
                match calculate_charge_micro_usd(&usage, &pricing) {
                    Some(c) => (c, false),
                    // Pricing exists but is partial and a nonzero bucket
                    // had no rate — treat the same as missing.
                    None => (0, true),
                }
            }
            _ => (0, true),
        }
    }
}

#[async_trait]
impl SettlementRecorder for MeteringRecorder {
    async fn record(&self, ctx: &mut SettlementContext) -> Result<()> {
        let (estimated_charge_micro_usd, missing_pricing) = self.estimate_charge(ctx);
        if missing_pricing {
            // Demoted from `warn` to `debug` — the per-request "finished"
            // log already records `cost_usd` (or its absence) for every
            // call, so an info-level operator stream doesn't need a
            // duplicate warning on every uncatalogued model. Pricing
            // gaps are still visible by enabling DEBUG on this module.
            tracing::debug!(
                provider = %ctx.provider_id,
                model = %ctx.model_id,
                request_id = %ctx.request_id,
                "metering: no pricing for (provider, model); estimated charge = 0"
            );
        }
        let metric = RequestMetric {
            request_id: ctx.request_id.clone(),
            user_id: ctx.caller.user_id().to_string(),
            api_key_id: ctx.caller.api_key_id().to_string(),
            model_id: ctx.model_id.clone(),
            provider_id: ctx.provider_id.clone(),
            prompt_tokens: ctx.prompt_tokens,
            completion_tokens: ctx.completion_tokens,
            reasoning_tokens: ctx.reasoning_tokens,
            cache_read_tokens: ctx.cache_read_tokens,
            cache_write_tokens: ctx.cache_write_tokens,
            estimated_charge_micro_usd,
            latency_ms: ctx.latency_ms,
            generation_time_ms: ctx.generation_time_ms,
            streamed: ctx.streamed,
            error: ctx.error.as_ref().map(|e| e.to_string()),
        };
        self.store.record_request(metric).await
    }
}
