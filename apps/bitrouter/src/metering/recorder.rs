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
use bitrouter_sdk::event::PipelineEvent;
use bitrouter_sdk::language_model::{
    FinishReason, SettlementContext, SettlementRecorder, Usage, UsageOrigin,
};
use serde::Serialize;

use crate::metering::db::{MeteringSessionIdentity, ReconciliationStatus, RequestMetric};
use crate::metering::pricing::{
    ChargeEvidence, PricingSource, PricingTable, calculate_charge_evidence,
    unavailable_charge_evidence,
};
use crate::metering::store::MeteringStore;
use crate::session_identity::SessionIdentityObserved;

/// Always-run settlement recorder writing through [`MeteringStore`].
pub struct MeteringRecorder {
    store: MeteringStore,
    pricing: Arc<PricingTable>,
    reconciliation_providers: HashSet<String>,
}

/// Content-free, typed proof that metering successfully persisted its
/// authoritative request observation. Later settlement recorders consume this
/// instead of recomputing price or treating absent usage as zero.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MeteringSettlementEvent {
    pub request_id: String,
    pub provider_id: String,
    pub model_id: String,
    pub usage_origin: UsageOrigin,
    pub prompt_tokens: Option<u64>,
    pub completion_tokens: Option<u64>,
    pub reasoning_tokens: Option<u64>,
    pub cache_read_tokens: Option<u64>,
    pub cache_write_tokens: Option<u64>,
    pub total_tokens: Option<u64>,
    pub cost_micro_usd: Option<u64>,
    pub duration_ms: u64,
    pub error_code: Option<String>,
    pub finish_reason: Option<String>,
}

impl PipelineEvent for MeteringSettlementEvent {
    fn event_name(&self) -> &'static str {
        "metering.settled"
    }
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
        // Preserve the pipeline-observed provenance for downstream authority.
        // The legacy store normalization below may encode known zero-usage
        // rejection semantics, but it must not manufacture trajectory evidence.
        let observed_usage_origin = ctx.usage_origin;
        let observed_usage_known = observed_usage_origin != UsageOrigin::Unknown;
        Self::normalize_zero_usage_rejection(ctx);
        let charge_evidence = self.charge_evidence(ctx);
        let cost_micro_usd = charge_evidence
            .charge_micro_usd
            .map(|value| {
                u64::try_from(value).map_err(|_| {
                    bitrouter_sdk::BitrouterError::internal(
                        "metering computed a negative request charge",
                    )
                })
            })
            .transpose()?;
        let total_tokens = if ctx.usage_origin == UsageOrigin::Unknown {
            None
        } else {
            Some(
                ctx.prompt_tokens
                    .checked_add(ctx.completion_tokens)
                    .ok_or_else(|| {
                        bitrouter_sdk::BitrouterError::internal(
                            "metering settlement token total overflow",
                        )
                    })?,
            )
        };
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
        let session_event = ctx.get_event::<SessionIdentityObserved>().cloned();
        let session_identity = session_event
            .as_ref()
            .filter(|event| {
                event.attributed
                    || event.authenticated_controller_instance_id.is_some()
                    || event.harness.is_some()
            })
            .map(|event| {
                let serialized = serde_json::to_string(event).map_err(|error| {
                    bitrouter_sdk::BitrouterError::internal(format!(
                        "serialize normalized session identity: {error}"
                    ))
                })?;
                Ok::<MeteringSessionIdentity, bitrouter_sdk::BitrouterError>(
                    MeteringSessionIdentity {
                        agent_harness: event.harness.clone(),
                        controller_instance_id: event.authenticated_controller_instance_id.clone(),
                        acp_session_id: event.acp_session_id.clone(),
                        native_root_session_id: event.native_root_session_id.clone(),
                        native_agent_thread_id: event.native_agent_thread_id.clone(),
                        native_parent_agent_thread_id: event.native_parent_agent_thread_id.clone(),
                        native_turn_id: event.native_turn_id.clone(),
                        route_lease_id: event.route_lease_id.clone(),
                        serialized,
                    },
                )
            })
            .transpose()?;
        if let Some(event) = &session_event {
            ctx.emit(bitrouter_observe::otel::SpanAttributes(
                session_span_attributes(event),
            ));
        }
        let metric = RequestMetric {
            request_id: ctx.request_id.clone(),
            user_id: ctx.caller.user_id().to_string(),
            api_key_id: ctx.caller.api_key_id().to_string(),
            launch_id: ctx.caller.launch_id().map(str::to_string),
            session_identity,
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
        ctx.emit(MeteringSettlementEvent {
            request_id: ctx.request_id.clone(),
            provider_id: ctx.provider_id.clone(),
            model_id: ctx.model_id.clone(),
            usage_origin: observed_usage_origin,
            prompt_tokens: observed_usage_known.then_some(ctx.prompt_tokens),
            completion_tokens: observed_usage_known.then_some(ctx.completion_tokens),
            reasoning_tokens: observed_usage_known.then_some(ctx.reasoning_tokens),
            cache_read_tokens: observed_usage_known.then_some(ctx.cache_read_tokens),
            cache_write_tokens: observed_usage_known.then_some(ctx.cache_write_tokens),
            total_tokens: observed_usage_known.then_some(total_tokens).flatten(),
            cost_micro_usd: observed_usage_known.then_some(cost_micro_usd).flatten(),
            duration_ms: ctx.request_duration_ms,
            error_code: ctx
                .error
                .as_ref()
                .map(|error| error.error_code().to_owned()),
            finish_reason: ctx.finish_reason.as_ref().map(finish_reason_kind),
        });
        tracing::debug!(
            request_id = %ctx.request_id,
            "metering settlement recorded"
        );
        Ok(())
    }
}

fn session_span_attributes(
    event: &SessionIdentityObserved,
) -> serde_json::Map<String, serde_json::Value> {
    let mut attributes = serde_json::Map::from_iter([
        (
            "bitrouter.agent.session.attributed".to_string(),
            serde_json::Value::Bool(event.attributed),
        ),
        (
            "bitrouter.agent.session.route_scope".to_string(),
            serde_json::Value::String(event.route_scope.clone()),
        ),
        (
            "bitrouter.agent.session.origin".to_string(),
            serde_json::json!(event.origin),
        ),
    ]);
    for (name, value) in [
        ("bitrouter.agent.harness", event.harness.as_ref()),
        (
            "bitrouter.acp.controller_instance_id",
            event.authenticated_controller_instance_id.as_ref(),
        ),
        ("bitrouter.acp.session_id", event.acp_session_id.as_ref()),
        (
            "bitrouter.agent.root_session_id",
            event.native_root_session_id.as_ref(),
        ),
        (
            "bitrouter.agent.thread_id",
            event.native_agent_thread_id.as_ref(),
        ),
        (
            "bitrouter.agent.parent_thread_id",
            event.native_parent_agent_thread_id.as_ref(),
        ),
        ("bitrouter.agent.turn_id", event.native_turn_id.as_ref()),
        (
            "bitrouter.acp.route_lease_id",
            event.route_lease_id.as_ref(),
        ),
    ] {
        if let Some(value) = value {
            attributes.insert(name.to_string(), serde_json::Value::String(value.clone()));
        }
    }
    attributes
}

fn finish_reason_kind(reason: &FinishReason) -> String {
    match reason {
        FinishReason::Stop => "stop",
        FinishReason::Length => "length",
        FinishReason::ToolCalls => "tool_calls",
        FinishReason::ContentFilter => "content_filter",
        FinishReason::Other(_) => "other",
        FinishReason::Error(_) => "error",
    }
    .to_owned()
}
