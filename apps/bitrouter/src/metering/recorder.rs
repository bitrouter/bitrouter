//! `MeteringRecorder` — the OSS `SettlementRecorder`.
//!
//! For every settled request (success or failure):
//! 1. Normalize provider usage into four non-overlapping buckets and compute
//!    auditable micro-USD evidence. Missing usage or pricing is persisted as
//!    an unknown charge, never exposed as a computed zero-dollar request.
//! 2. Write a `RequestMetric` row to [`super::MeteringStore`].
//!
//! No charging, no balance check, no funding-source selection. Those are
//! deployment-specific; if the OSS deployment needs a hard spend cap, it
//! goes through `apps/bitrouter/src/policy/` reading `MeteringStore`.

use std::collections::HashSet;
use std::sync::Arc;

use async_trait::async_trait;

use bitrouter_sdk::Result;
use bitrouter_sdk::language_model::{SettlementContext, SettlementRecorder, Usage, UsageOrigin};

use crate::metering::db::{ReconciliationStatus, RequestMetric};
use crate::metering::pricing::{
    ChargeEvidence, PricingSource, PricingTable, calculate_charge_evidence,
    unavailable_charge_evidence,
};
use crate::metering::store::MeteringStore;

/// Always-run settlement recorder writing through [`MeteringStore`].
pub struct MeteringRecorder {
    store: MeteringStore,
    pricing: Arc<PricingTable>,
    reconciliation_providers: HashSet<String>,
}

impl MeteringRecorder {
    /// Build a recorder over the shared `MeteringStore` and a
    /// `(provider, service_id) → ModelPricing` table.
    pub fn new(store: MeteringStore, pricing: Arc<PricingTable>) -> Self {
        Self {
            store,
            pricing,
            reconciliation_providers: HashSet::new(),
        }
    }

    /// Require request-scoped authoritative reconciliation for this provider.
    pub fn with_reconciliation_provider(mut self, provider_id: impl Into<String>) -> Self {
        self.reconciliation_providers.insert(provider_id.into());
        self
    }

    fn charge_evidence(&self, ctx: &SettlementContext) -> ChargeEvidence {
        let usage = Usage {
            prompt_tokens: ctx.prompt_tokens,
            completion_tokens: ctx.completion_tokens,
            reasoning_tokens: ctx.reasoning_tokens,
            cache_read_tokens: ctx.cache_read_tokens,
            cache_write_tokens: ctx.cache_write_tokens,
            web_search_count: 0,
            origin: ctx.usage_origin,
            raw: ctx.raw_usage.clone().map(Box::new),
        };
        if ctx.usage_origin == UsageOrigin::Unknown {
            return unavailable_charge_evidence(&usage, "usage_unavailable");
        }
        match self.pricing.resolve(&ctx.provider_id, &ctx.model_id) {
            Some(pricing) if !pricing.is_unconfigured() => {
                calculate_charge_evidence(&usage, &pricing, PricingSource::Configured)
            }
            _ => unavailable_charge_evidence(&usage, "pricing_not_found"),
        }
    }

    fn normalize_zero_usage_rejection(ctx: &mut SettlementContext) {
        let error_code = match ctx.error {
            Some(bitrouter_sdk::BitrouterError::UpstreamPolicyViolation { .. }) => {
                Some("upstream_policy_violation")
            }
            Some(bitrouter_sdk::BitrouterError::UpstreamRateLimited { .. }) => {
                Some("upstream_rate_limited")
            }
            _ => None,
        };
        let has_no_usage = ctx.prompt_tokens == 0
            && ctx.completion_tokens == 0
            && ctx.reasoning_tokens == 0
            && ctx.cache_read_tokens == 0
            && ctx.cache_write_tokens == 0
            && ctx.usage_origin == UsageOrigin::Unknown
            && ctx.raw_usage.is_none();
        if let (Some(error_code), true) = (error_code, has_no_usage) {
            ctx.usage_origin = UsageOrigin::ProviderReported;
            ctx.raw_usage = Some(serde_json::json!({
                "error": { "code": error_code },
                "usage": null
            }));
        }
    }
}

#[async_trait]
impl SettlementRecorder for MeteringRecorder {
    async fn record(&self, ctx: &mut SettlementContext) -> Result<()> {
        tracing::debug!(
            request_id = %ctx.request_id,
            provider = %ctx.provider_id,
            model = %ctx.model_id,
            "metering settlement started"
        );
        Self::normalize_zero_usage_rejection(ctx);
        let charge_evidence = self.charge_evidence(ctx);
        if charge_evidence.charge_micro_usd.is_none() {
            // Demoted from `warn` to `debug` — the per-request "finished"
            // log already records `cost_usd` (or its absence) for every
            // call, so an info-level operator stream doesn't need a
            // duplicate warning on every uncatalogued model. Pricing
            // gaps are still visible by enabling DEBUG on this module.
            tracing::debug!(
                provider = %ctx.provider_id,
                model = %ctx.model_id,
                request_id = %ctx.request_id,
                reason = charge_evidence.unknown_reason.as_deref().unwrap_or("unknown"),
                "metering: charge evidence incomplete"
            );
        }
        let estimated_charge_micro_usd = charge_evidence.charge_micro_usd.unwrap_or(0);
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
            uncached_input_tokens: charge_evidence.normalized_usage.uncached_input_tokens,
            output_tokens: charge_evidence.normalized_usage.output_tokens,
            usage_origin: ctx.usage_origin,
            raw_usage: ctx.raw_usage.clone(),
            charge_status: charge_evidence.status,
            charge_evidence,
            reconciliation_status: if self.reconciliation_providers.contains(&ctx.provider_id) {
                ReconciliationStatus::Pending
            } else {
                ReconciliationStatus::NotApplicable
            },
            estimated_charge_micro_usd,
            latency_ms: ctx.request_duration_ms,
            generation_time_ms: ctx.upstream_duration_ms.unwrap_or(0),
            streamed: ctx.streamed,
            error: ctx
                .error
                .as_ref()
                .map(|error| error.error_code().to_string()),
        };
        self.store.record_request(metric).await?;
        tracing::debug!(
            request_id = %ctx.request_id,
            "metering settlement recorded"
        );
        Ok(())
    }
}
