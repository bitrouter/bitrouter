use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use bitrouter_sdk::Result;
use bitrouter_sdk::language_model::SettlementRecorder;
use bitrouter_sdk::language_model::settlement::SettlementContext;

use crate::adequacy::observer::classify_failure;
use crate::adequacy::reliability::{ReliabilityKey, ReliabilityObservation};
use crate::adequacy::{AdequacyLedger, InadequacyCause, Outcome};
use crate::policy_table_router::PolicyTable;

#[derive(Clone)]
pub(crate) struct PendingAdequacyDecision {
    pub request_id: String,
    pub request_key: String,
    pub ledger_key: String,
    pub static_tier: Option<String>,
    pub selected_tier: Option<String>,
    pub half_open_probe: bool,
    pub exploration_allowed: bool,
    pub table: Arc<PolicyTable>,
    pub ledger: Arc<AdequacyLedger>,
}

#[derive(Default)]
pub(crate) struct PendingAdequacyStore {
    pending: Mutex<HashMap<String, PendingAdequacyDecision>>,
}

impl PendingAdequacyStore {
    pub(crate) fn insert(&self, decision: PendingAdequacyDecision) {
        let mut guard = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        guard.insert(decision.request_id.clone(), decision);
    }

    fn take(&self, request_id: &str) -> Option<PendingAdequacyDecision> {
        let mut guard = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        guard.remove(request_id)
    }
}

pub(crate) struct AdequacySettlementRecorder {
    pending: Arc<PendingAdequacyStore>,
}

impl AdequacySettlementRecorder {
    pub(crate) fn new(pending: Arc<PendingAdequacyStore>) -> Self {
        Self { pending }
    }

    fn served_tier(pending: &PendingAdequacyDecision, ctx: &SettlementContext) -> Option<String> {
        let explicit = format!("{}:{}", ctx.provider_id, ctx.model_id);
        pending
            .table
            .tier_of_model(&explicit)
            .or_else(|| pending.table.tier_of_model(&ctx.model_id))
            .map(ToString::to_string)
    }

    fn cause(ctx: &SettlementContext) -> InadequacyCause {
        ctx.error
            .as_ref()
            .map(classify_failure)
            .unwrap_or(InadequacyCause::None)
    }

    fn outcome_for(
        pending: &PendingAdequacyDecision,
        served_tier: &str,
        cause: InadequacyCause,
    ) -> Option<Outcome> {
        let static_tier = pending.static_tier.as_deref();
        let escalation_tier = pending.table.escalation_tier();

        if static_tier == Some(served_tier) && Some(served_tier) != escalation_tier {
            return Some(Outcome::StaticDowngrade { cause });
        }

        if !pending.table.exploration_enabled() {
            return None;
        }
        let escalation_tier = escalation_tier?;
        if static_tier != Some(escalation_tier) || !pending.exploration_allowed {
            return None;
        }

        let trialed = pending.table.explore_tier() == Some(served_tier);
        let served_escalation = served_tier == escalation_tier;
        if !trialed && !served_escalation {
            return None;
        }

        Some(Outcome::Exploration { trialed, cause })
    }
}

#[async_trait]
impl SettlementRecorder for AdequacySettlementRecorder {
    async fn record(&self, ctx: &mut SettlementContext) -> Result<()> {
        let Some(pending) = self.pending.take(&ctx.request_id) else {
            return Ok(());
        };
        let Some(served_tier) = Self::served_tier(&pending, ctx) else {
            tracing::debug!(
                request_id = %ctx.request_id,
                provider = %ctx.provider_id,
                model = %ctx.model_id,
                request_key = %pending.request_key,
                "adequacy settlement skipped: served model not in policy tiers"
            );
            return Ok(());
        };
        let cause = Self::cause(ctx);
        let route_key = pending
            .table
            .model_of_tier(&served_tier)
            .unwrap_or(&ctx.model_id);
        let reliability_observation = match cause {
            InadequacyCause::None => Some(ReliabilityObservation::Success),
            InadequacyCause::ProviderTransient => Some(ReliabilityObservation::TransientFailure),
            _ => None,
        };
        if let Some(observation) = reliability_observation {
            let endpoint_key = reliability_key(ctx);
            pending
                .ledger
                .observe_provider_reliability(
                    &ctx.request_id,
                    route_key,
                    endpoint_key,
                    observation,
                    pending.half_open_probe,
                )
                .await?;
        }
        if cause == InadequacyCause::ProviderTransient {
            tracing::debug!(
                request_id = %ctx.request_id,
                request_key = %pending.request_key,
                route_key,
                "provider transient recorded only in reliability ledger"
            );
            return Ok(());
        }
        let Some(outcome) = Self::outcome_for(&pending, &served_tier, cause) else {
            tracing::debug!(
                request_id = %ctx.request_id,
                request_key = %pending.request_key,
                served_tier = %served_tier,
                static_tier = ?pending.static_tier,
                selected_tier = ?pending.selected_tier,
                exploration_allowed = pending.exploration_allowed,
                "adequacy settlement skipped"
            );
            return Ok(());
        };

        tracing::debug!(
            request_id = %ctx.request_id,
            request_key = %pending.request_key,
            served_tier = %served_tier,
            static_tier = ?pending.static_tier,
            selected_tier = ?pending.selected_tier,
            observation = ?outcome,
            "adequacy settlement recorded"
        );
        pending.ledger.observe(&pending.ledger_key, outcome).await;
        tracing::debug!(
            request_id = %ctx.request_id,
            request_key = %pending.request_key,
            "adequacy settlement completed"
        );
        Ok(())
    }
}

fn reliability_key(ctx: &SettlementContext) -> ReliabilityKey {
    let account_class = if ctx.account_label.is_some() {
        "named_account"
    } else {
        "default"
    };
    let auth_class = ctx
        .target
        .as_ref()
        .map_or("unknown", |target| match target.auth_scheme {
            bitrouter_sdk::language_model::types::AuthScheme::XApiKey => "x_api_key",
            bitrouter_sdk::language_model::types::AuthScheme::Bearer => "bearer",
        });
    ReliabilityKey {
        provider: ctx.provider_id.clone(),
        model: ctx.model_id.clone(),
        credential_class: format!("{account_class}:{auth_class}"),
        endpoint_scope: ctx
            .target
            .as_ref()
            .and_then(|target| target.effective_api_base().parse::<http::Uri>().ok())
            .and_then(|uri| {
                uri.authority()
                    .map(|authority| authority.as_str().to_string())
            })
            .unwrap_or_else(|| "unknown".to_string()),
        protocol: ctx
            .target
            .as_ref()
            .map(|target| target.api_protocol.as_str().to_string())
            .unwrap_or_else(|| "unknown".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use bitrouter_sdk::caller::CallerContext;
    use bitrouter_sdk::config::{PolicyKeyStrategy, PolicyTableConfig};
    use bitrouter_sdk::event::EventBus;
    use bitrouter_sdk::language_model::SettlementRecorder;
    use bitrouter_sdk::language_model::settlement::SettlementContext;

    use crate::adequacy::AdequacyLedger;
    use crate::adequacy::reliability::RoutePermit;
    use crate::adequacy::settlement::{
        AdequacySettlementRecorder, PendingAdequacyDecision, PendingAdequacyStore,
    };
    use crate::policy_table_router::PolicyTable;

    fn policy_table() -> Arc<PolicyTable> {
        let cfg = PolicyTableConfig {
            key_strategy: PolicyKeyStrategy::WorkflowState,
            tiers: [
                ("capable".to_string(), "openai-codex:gpt-5.5".to_string()),
                (
                    "cheap".to_string(),
                    "bitrouter:moonshotai/kimi-k2.7-code".to_string(),
                ),
            ]
            .into_iter()
            .collect(),
            fingerprints: Default::default(),
            default_tier: Some("capable".to_string()),
            tool_use_tier: Some("capable".to_string()),
            tool_safe_tiers: vec!["capable".to_string(), "cheap".to_string()],
            adequacy: bitrouter_sdk::config::AdequacyConfig {
                enabled: true,
                escalation_tier: Some("capable".to_string()),
                explore_enabled: true,
                explore_tier: Some("cheap".to_string()),
                explore_interval: 2,
                explore_threshold: 3,
                explore_opening: false,
                ..Default::default()
            },
        };
        PolicyTable::from_config(&cfg).expect("policy table")
    }

    fn settlement(request_id: &str, provider_id: &str, model_id: &str) -> SettlementContext {
        SettlementContext {
            request_id: request_id.to_string(),
            caller: CallerContext::local(),
            target: None,
            model_id: model_id.to_string(),
            provider_id: provider_id.to_string(),
            account_label: None,
            prompt_tokens: 10,
            completion_tokens: 1,
            reasoning_tokens: 0,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            usage_origin: bitrouter_sdk::language_model::UsageOrigin::ProviderReported,
            raw_usage: None,
            web_search_count: 0,
            media_input_count: 0,
            media_output_count: 0,
            server_tool_calls: Vec::new(),
            streamed: true,
            latency_ms: 1,
            generation_time_ms: 1,
            first_token_latency_ms: None,
            first_token_kind: None,
            error: None,
            events: EventBus::default(),
        }
    }

    #[tokio::test]
    async fn settlement_advances_exploration_from_pending_policy_decision() {
        let table = policy_table();
        let ledger = Arc::new(AdequacyLedger::in_memory_explore(2, 900, 2, 3));
        let pending = Arc::new(PendingAdequacyStore::default());
        let recorder = AdequacySettlementRecorder::new(pending.clone());
        let request_key = "codex|responses|tool_followup|-|-|exec_command|high|medium|none|high|low|medium|low|medium|medium|requires_structured_tools";

        pending.insert(PendingAdequacyDecision {
            request_id: "req-1".to_string(),
            request_key: request_key.to_string(),
            ledger_key: request_key.to_string(),
            static_tier: Some("capable".to_string()),
            selected_tier: Some("capable".to_string()),
            half_open_probe: false,
            exploration_allowed: true,
            table: table.clone(),
            ledger: ledger.clone(),
        });
        recorder
            .record(&mut settlement("req-1", "openai-codex", "gpt-5.5"))
            .await
            .expect("settlement observe succeeds");

        pending.insert(PendingAdequacyDecision {
            request_id: "req-2".to_string(),
            request_key: request_key.to_string(),
            ledger_key: request_key.to_string(),
            static_tier: Some("capable".to_string()),
            selected_tier: Some("capable".to_string()),
            half_open_probe: false,
            exploration_allowed: true,
            table,
            ledger: ledger.clone(),
        });
        recorder
            .record(&mut settlement("req-2", "openai-codex", "gpt-5.5"))
            .await
            .expect("settlement observe succeeds");

        assert!(
            ledger.should_trial(request_key),
            "settlement recorder should advance the same trial cadence as the observe hook"
        );
    }

    #[tokio::test]
    async fn transient_provider_failures_open_route_circuit_without_semantic_pin() {
        let table = policy_table();
        let ledger = Arc::new(AdequacyLedger::in_memory_explore(1, 900, 1, 3));
        let pending = Arc::new(PendingAdequacyStore::default());
        let recorder = AdequacySettlementRecorder::new(pending.clone());
        let request_key = "codex|responses|tool_followup|-|-|exec_command";
        let route_key = "bitrouter:moonshotai/kimi-k2.7-code";

        for index in 1..=2 {
            pending.insert(PendingAdequacyDecision {
                request_id: format!("req-{index}"),
                request_key: request_key.to_string(),
                ledger_key: request_key.to_string(),
                static_tier: Some("capable".to_string()),
                selected_tier: Some("cheap".to_string()),
                half_open_probe: false,
                exploration_allowed: true,
                table: table.clone(),
                ledger: ledger.clone(),
            });
            let mut failed = settlement(
                &format!("req-{index}"),
                "bitrouter",
                "moonshotai/kimi-k2.7-code",
            );
            failed.error = Some(bitrouter_sdk::BitrouterError::UpstreamTimeout);
            recorder
                .record(&mut failed)
                .await
                .expect("transient settlement observation");
        }

        assert!(!ledger.is_pinned(request_key));
        assert_eq!(ledger.reliability_permit(route_key), RoutePermit::Open);
    }
}
