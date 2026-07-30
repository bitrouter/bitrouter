//! Request settlement bridge for generic evaluation subjects.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, PoisonError};

use async_trait::async_trait;
use bitrouter_sdk::Result as BitrouterResult;
use bitrouter_sdk::language_model::{SettlementContext, SettlementRecorder, Usage};

use super::store::EvalStore;
use super::types::{
    EVAL_SCHEMA_VERSION, EvalDecisionRef, EvalScope, EvalSubject, EvidenceItem, canonical_digest,
    evidence_digest,
};
use crate::metering::{PricingTable, calculate_charge_micro_usd};

/// A policy decision waiting for the always-run settlement stage to attach
/// request outcome evidence. This is process-local correlation state only; it
/// never participates in routing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingEvalDecision {
    pub request_id: String,
    pub decision_id: String,
    pub policy: String,
    pub policy_digest: String,
    pub request_key: String,
    pub selected_tier: String,
    pub baseline_tier: Option<String>,
    pub preset: Option<String>,
    pub holdout: bool,
}

/// Bounded-lifetime request correlation used between model selection and
/// settlement. Restarting BitRouter may lose in-flight observations, but can
/// never alter the active routing policy.
#[derive(Clone, Default)]
pub struct PendingEvalDecisionStore {
    entries: Arc<Mutex<BTreeMap<String, PendingEvalDecision>>>,
}

impl PendingEvalDecisionStore {
    pub fn insert(&self, decision: PendingEvalDecision) {
        self.entries
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(decision.request_id.clone(), decision);
    }

    fn take(&self, request_id: &str) -> Option<PendingEvalDecision> {
        self.entries
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(request_id)
    }

    #[cfg(test)]
    pub(crate) fn get(&self, request_id: &str) -> Option<PendingEvalDecision> {
        self.entries
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(request_id)
            .cloned()
    }
}

/// Converts a settled routed request into a generic, redacted eval subject.
/// It deliberately emits no score: evaluator-specific work happens outside
/// the request path and returns through the exchange API.
pub struct EvalSettlementRecorder {
    store: EvalStore,
    pending: PendingEvalDecisionStore,
    pricing: Arc<PricingTable>,
}

impl EvalSettlementRecorder {
    pub fn new(
        store: EvalStore,
        pending: PendingEvalDecisionStore,
        pricing: Arc<PricingTable>,
    ) -> Self {
        Self {
            store,
            pending,
            pricing,
        }
    }

    fn subject(
        &self,
        decision: PendingEvalDecision,
        context: &SettlementContext,
    ) -> anyhow::Result<EvalSubject> {
        let mut attributes = BTreeMap::from([
            ("provider".to_string(), context.provider_id.clone()),
            ("model".to_string(), context.model_id.clone()),
            (
                "prompt_tokens".to_string(),
                context.prompt_tokens.to_string(),
            ),
            (
                "completion_tokens".to_string(),
                context.completion_tokens.to_string(),
            ),
            (
                "reasoning_tokens".to_string(),
                context.reasoning_tokens.to_string(),
            ),
            (
                "request_duration_ms".to_string(),
                context.request_duration_ms.to_string(),
            ),
            ("success".to_string(), context.error.is_none().to_string()),
        ]);
        if let Some(error) = &context.error {
            attributes.insert("error_code".into(), error.error_code().into());
        }
        let usage = Usage {
            prompt_tokens: context.prompt_tokens,
            completion_tokens: context.completion_tokens,
            reasoning_tokens: context.reasoning_tokens,
            cache_read_tokens: context.cache_read_tokens,
            cache_write_tokens: context.cache_write_tokens,
            web_search_count: context.web_search_count,
            origin: context.usage_origin,
            raw: None,
        };
        if let Some(pricing) = self
            .pricing
            .resolve(&context.provider_id, &context.model_id)
            && let Some(cost) = calculate_charge_micro_usd(&usage, &pricing)
        {
            attributes.insert("cost_micro_usd".into(), cost.to_string());
        }
        let evidence = vec![EvidenceItem {
            evidence_id: "request-outcome".into(),
            kind: "request.outcome".into(),
            digest: canonical_digest(&attributes)?,
            redacted: true,
            attributes,
        }];
        let evidence_digest = evidence_digest(&evidence)?;
        Ok(EvalSubject {
            schema_version: EVAL_SCHEMA_VERSION,
            eval_id: format!("request:{}", context.request_id),
            scope: EvalScope::Request,
            subject_id: context.request_id.clone(),
            policy_digest: decision.policy_digest.clone(),
            preset: decision.preset,
            cohort: None,
            holdout: decision.holdout,
            decisions: vec![EvalDecisionRef {
                decision_id: decision.decision_id,
                policy: decision.policy,
                request_key: decision.request_key,
                selected_tier: decision.selected_tier,
                baseline_tier: decision.baseline_tier,
                policy_digest: decision.policy_digest,
            }],
            requested_dimensions: BTreeSet::from([
                "cost.usd_micros".into(),
                "latency.ms".into(),
                "quality.pass".into(),
            ]),
            evidence,
            evidence_digest,
            observed_at: chrono::Utc::now().to_rfc3339(),
        })
    }
}

#[async_trait]
impl SettlementRecorder for EvalSettlementRecorder {
    async fn record(&self, context: &mut SettlementContext) -> BitrouterResult<()> {
        let Some(decision) = self.pending.take(&context.request_id) else {
            return Ok(());
        };
        let subject = self.subject(decision, context).map_err(|error| {
            bitrouter_sdk::BitrouterError::internal(format!(
                "building request eval subject: {error}"
            ))
        })?;
        self.store.insert_subject(&subject).await.map_err(|error| {
            bitrouter_sdk::BitrouterError::internal(format!(
                "persisting request eval subject: {error}"
            ))
        })?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::sync::Arc;

    use bitrouter_sdk::caller::CallerContext;
    use bitrouter_sdk::event::EventBus;
    use bitrouter_sdk::language_model::{SettlementContext, SettlementRecorder, UsageOrigin};

    use super::{EvalSettlementRecorder, PendingEvalDecision, PendingEvalDecisionStore};
    use crate::eval::store::EvalStore;
    use crate::metering::PricingTable;

    const DIGEST: &str = "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    #[tokio::test]
    async fn settlement_creates_a_redacted_request_subject() -> anyhow::Result<()> {
        let db = crate::db::connect("sqlite::memory:").await?;
        crate::db::run_migrations(&db).await?;
        let store = EvalStore::new(db);
        let pending = PendingEvalDecisionStore::default();
        pending.insert(PendingEvalDecision {
            request_id: "request-1".into(),
            decision_id: "decision-1".into(),
            policy: "auto:cost".into(),
            policy_digest: DIGEST.into(),
            request_key: "opening".into(),
            selected_tier: "economy".into(),
            baseline_tier: Some("strong".into()),
            preset: Some("auto:cost".into()),
            holdout: false,
        });
        let recorder =
            EvalSettlementRecorder::new(store.clone(), pending, Arc::new(PricingTable::new()));
        let mut context = settlement_context();

        recorder.record(&mut context).await?;

        let subject = store
            .subject("request:request-1")
            .await?
            .ok_or_else(|| anyhow::anyhow!("request subject missing"))?;
        assert_eq!(subject.policy_digest, DIGEST);
        assert_eq!(subject.decisions.len(), 1);
        assert!(subject.evidence.iter().all(|item| item.redacted));
        assert_eq!(
            subject.requested_dimensions,
            BTreeSet::from([
                "cost.usd_micros".to_string(),
                "latency.ms".to_string(),
                "quality.pass".to_string(),
            ])
        );
        Ok(())
    }

    fn settlement_context() -> SettlementContext {
        SettlementContext {
            request_id: "request-1".into(),
            caller: CallerContext::local(),
            target: None,
            model_id: "model".into(),
            provider_id: "provider".into(),
            account_label: None,
            prompt_tokens: 10,
            completion_tokens: 5,
            reasoning_tokens: 0,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            usage_origin: UsageOrigin::ProviderReported,
            raw_usage: None,
            web_search_count: 0,
            media_input_count: 0,
            media_output_count: 0,
            server_tool_calls: Vec::new(),
            streamed: false,
            request_duration_ms: 123,
            upstream_duration_ms: Some(100),
            ttft_ms: Some(10),
            generation_duration_ms: Some(90),
            first_token_kind: None,
            finish_reason: None,
            error: None,
            events: EventBus::default(),
        }
    }
}
