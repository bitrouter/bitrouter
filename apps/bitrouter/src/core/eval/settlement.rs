//! Request settlement bridge for generic evaluation subjects.

use std::collections::{BTreeMap, BTreeSet};

use crate::gateway::error::Result as BitrouterResult;
use crate::gateway::pipeline::settlement::{SettlementContext, SettlementRecorder};
use async_trait::async_trait;

use super::store::EvalStore;
use crate::gateway::observability::decision::{
    CapturedEvalDecision, EvalInvocation, PendingEvalDecision, PendingEvalDecisionStore,
    routing_evidence,
};
use {
    crate::storage::evaluation::canonical_digest, crate::storage::evaluation::evidence_digest,
    bitrouter_ai::eval::EVAL_SCHEMA_VERSION, bitrouter_ai::eval::EvalDecisionRef,
    bitrouter_ai::eval::EvalScope, bitrouter_ai::eval::EvalSubject,
    bitrouter_ai::eval::EvidenceItem,
};

/// Converts a settled routed request into a generic, redacted eval subject.
/// It deliberately emits no score: evaluator-specific work happens outside
/// the request path and returns through the exchange API.
pub struct EvalSettlementRecorder {
    pending: PendingEvalDecisionStore,
    owner: RecorderOwner,
}

enum RecorderOwner {
    Legacy {
        store: EvalStore,
    },
    Tenant {
        execution: std::sync::Arc<crate::tenancy::execution::TenantExecution>,
        observation: std::sync::Arc<crate::tenancy::eval::observation::RequiredObservation>,
        caller: crate::gateway::caller::CallerContext,
    },
}

impl EvalSettlementRecorder {
    pub fn new(store: EvalStore, pending: PendingEvalDecisionStore) -> Self {
        Self {
            pending,
            owner: RecorderOwner::Legacy { store },
        }
    }

    /// The namespace is captured from admission, independently of settlement
    /// metadata. No public caller or request identifier can select a tenant store.
    pub(crate) fn for_execution(
        execution: &std::sync::Arc<crate::tenancy::execution::TenantExecution>,
        pending: PendingEvalDecisionStore,
        observation: std::sync::Arc<crate::tenancy::eval::observation::RequiredObservation>,
    ) -> Self {
        Self {
            pending,
            owner: RecorderOwner::Tenant {
                execution: execution.clone(),
                observation,
                caller: execution.caller(),
            },
        }
    }

    fn subject(
        &self,
        decision: &PendingEvalDecision,
        context: &SettlementContext,
    ) -> anyhow::Result<EvalSubject> {
        if decision.selected_effort != context.reasoning_effort {
            anyhow::bail!("settled reasoning effort does not match the recorded policy treatment");
        }
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
        if let Some(effort) = context.reasoning_effort {
            attributes.insert("reasoning_effort".into(), effort.to_string());
        }
        attributes.extend(decision.observation_snapshot().attributes());
        let charge = crate::metering::tariff::settlement_charge_evidence(context);
        if let Some(cost) = charge.charge_micro_usd {
            attributes.insert("cost_micro_usd".into(), cost.to_string());
        }
        if let Some(snapshot) = &charge.tariff_snapshot {
            attributes.insert("outbound_protocol".into(), snapshot.protocol.to_string());
            attributes.insert("tariff_profile".into(), snapshot.endpoint_profile.clone());
            attributes.insert("pricing_version".into(), snapshot.pricing_version.clone());
        }
        let mut evidence = vec![EvidenceItem {
            evidence_id: "request-outcome".into(),
            kind: "request.outcome".into(),
            digest: canonical_digest(&attributes)?,
            redacted: true,
            attributes,
        }];
        evidence.extend(routing_evidence(context)?);
        let evidence_digest = evidence_digest(&evidence)?;
        Ok(EvalSubject {
            schema_version: EVAL_SCHEMA_VERSION,
            eval_id: format!("request:{}", context.request_id),
            scope: EvalScope::Request,
            subject_id: context.request_id.clone(),
            policy_digest: decision.policy_digest.clone(),
            preset: decision.preset.clone(),
            cohort: None,
            holdout: decision.holdout,
            decisions: vec![EvalDecisionRef {
                decision_id: decision.decision_id.clone(),
                policy: decision.policy.clone(),
                route_projection: decision.route_projection.clone(),
                request_key: decision.request_key.clone(),
                selected_tier: decision.selected_tier.clone(),
                selected_effort: decision.selected_effort,
                baseline_tier: decision.baseline_tier.clone(),
                baseline_effort: decision.baseline_effort,
                policy_digest: decision.policy_digest.clone(),
                experiment: decision.experiment.clone(),
                route_measurement: decision.route_measurement.clone(),
            }],
            requested_dimensions: BTreeSet::from([
                "cost.usd_micros".into(),
                "latency.ms".into(),
                "quality.pass".into(),
            ]),
            evidence,
            evidence_digest,
            observed_at: decision.observed_at.clone(),
        })
    }
}

#[async_trait]
impl SettlementRecorder for EvalSettlementRecorder {
    async fn record(&self, context: &mut SettlementContext) -> BitrouterResult<()> {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        let guard = match &self.owner {
            RecorderOwner::Tenant { observation, .. } => Some(observation.begin()?),
            RecorderOwner::Legacy { .. } => None,
        };
        let owner = match &self.owner {
            RecorderOwner::Legacy { .. } => context.caller.user_id(),
            RecorderOwner::Tenant {
                execution, caller, ..
            } => {
                if context.request_id != execution.id().as_str()
                    || context.caller.user_id() != caller.user_id()
                    || context.caller.api_key_id() != caller.api_key_id()
                    || context.caller.is_local()
                    || context.caller.launch_id().is_some()
                {
                    return Err(crate::gateway::error::BitrouterError::internal(
                        "tenant evaluation settlement does not match its admitted execution",
                    ));
                }
                caller.user_id()
            }
        };
        if context.has_event::<crate::gateway::metering::replay::CanonicalRequestReplayRejected>() {
            if let Some(guard) = guard {
                guard.skip()?;
            }
            return Ok(());
        }
        let invocation = context.get_event::<EvalInvocation>().cloned();
        let pending = invocation.as_ref().and_then(|invocation| {
            match context.get_events::<CapturedEvalDecision>().last() {
                Some(snapshot) => snapshot.decision_for(invocation, owner).cloned(),
                None => self.pending.peek(invocation, owner),
            }
        });
        let Some(decision) = pending else {
            if let Some(guard) = guard {
                if invocation.is_some() || context.has_event::<CapturedEvalDecision>() {
                    return Err(crate::gateway::error::BitrouterError::internal(
                        "required tenant evaluation decision is unavailable",
                    ));
                }
                guard.skip()?;
            }
            return Ok(());
        };
        let subject = self.subject(&decision, context).map_err(|error| {
            let message = match self.owner {
                RecorderOwner::Legacy { .. } => format!("building request eval subject: {error}"),
                RecorderOwner::Tenant { .. } => {
                    "building required tenant evaluation subject failed".into()
                }
            };
            crate::gateway::error::BitrouterError::internal(message)
        })?;
        match (&self.owner, guard) {
            (RecorderOwner::Legacy { store }, None) => {
                store
                    .insert_subject_owned(&subject, owner)
                    .await
                    .map_err(|error| {
                        crate::gateway::error::BitrouterError::internal(format!(
                            "persisting request eval subject: {error}"
                        ))
                    })?;
            }
            (
                RecorderOwner::Tenant {
                    execution,
                    observation,
                    ..
                },
                Some(guard),
            ) => {
                let subject = guard.prepare(subject)?;
                execution
                    .persist_observation(observation.clone(), subject, deadline)
                    .await?;
                guard.finish()?;
            }
            _ => {
                return Err(crate::gateway::error::BitrouterError::internal(
                    "invalid evaluation recorder ownership",
                ));
            }
        }
        if let Some(invocation) = &invocation {
            self.pending.take(invocation, owner);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::sync::Arc;
    use std::time::Duration;

    use crate::gateway::caller::CallerContext;
    use crate::gateway::observability::event::EventBus;
    use crate::gateway::pipeline::settlement::{SettlementContext, SettlementRecorder};
    use bitrouter_ai::types::{ReasoningEffort, UsageOrigin};
    use sea_orm::{ConnectionTrait, DatabaseBackend, Statement, TransactionTrait};

    use super::{
        EvalInvocation, EvalSettlementRecorder, PendingEvalDecision, PendingEvalDecisionStore,
    };
    use crate::core::eval::store::EvalStore;
    use crate::gateway::observability::decision::tests::test_decision;
    use crate::gateway::observability::settlement::PolicySettlementRecorder;
    use crate::trajectory::canonical::CorrelationKey;
    use crate::trajectory::settlement::TrajectorySettlementRecorder;
    use crate::trajectory::store::TrajectoryStore;
    use {
        bitrouter_ai::eval::EvalExperimentRef, bitrouter_ai::eval::ExperimentArm,
        bitrouter_ai::eval::ExperimentAssignmentUnit,
    };

    const DIGEST: &str = "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    #[tokio::test]
    async fn untracked_settlement_preserves_snapshot_across_pending_eviction() -> anyhow::Result<()>
    {
        settlement_during_pending_change(true).await
    }

    #[tokio::test]
    async fn untracked_settlement_does_not_adopt_a_late_pending_decision() -> anyhow::Result<()> {
        settlement_during_pending_change(false).await
    }

    async fn settlement_during_pending_change(initially_present: bool) -> anyhow::Result<()> {
        let db = crate::db::connect("sqlite::memory:").await?;
        crate::db::run_migrations(&db).await?;
        let store = EvalStore::new(db.clone());
        let pending = PendingEvalDecisionStore::with_limits_for_test(1, Duration::from_secs(60));
        let invocation = EvalInvocation::new("local");
        if initially_present {
            pending.insert(&invocation, test_decision("request-1"));
        }
        let recorder = PolicySettlementRecorder::new(
            pending.clone(),
            Some(TrajectorySettlementRecorder::new(
                TrajectoryStore::new(db.clone()),
                None,
                CorrelationKey::from_bytes(rand::random())?,
            )),
            Some(Arc::new(EvalSettlementRecorder::new(
                store.clone(),
                pending.clone(),
            ))),
        );
        let mut context = settlement_context_with(&invocation);

        // In-memory SQLite has one connection. Hold it to suspend the real
        // trajectory lookup after its caller has captured the pending decision.
        let blocker = db.begin().await?;
        let mut recording = Box::pin(recorder.record(&mut context));
        assert!(futures::poll!(recording.as_mut()).is_pending());
        if initially_present {
            pending.insert(&EvalInvocation::new("local"), test_decision("replacement"));
            assert!(pending.peek(&invocation, "local").is_none());
        } else {
            pending.insert(&invocation, test_decision("request-1"));
        }
        blocker.rollback().await?;
        tokio::time::timeout(Duration::from_secs(5), recording).await??;

        let subject = store.subject("request:request-1").await?;
        assert_eq!(subject.is_some(), initially_present);
        if let Some(subject) = subject {
            assert_eq!(subject.decisions[0].decision_id, "decision-request-1");
            assert!(pending.peek(&invocation, "local").is_none());
        } else {
            // No original observation means no subject or successful-persist cleanup.
            assert!(pending.peek(&invocation, "local").is_some());
        }
        let events = context.events.dump_json();
        assert_eq!(
            events[1],
            serde_json::json!({"event": "eval.settlement_snapshot", "data": {}})
        );
        Ok(())
    }

    #[tokio::test]
    async fn settlement_creates_a_redacted_request_subject() -> anyhow::Result<()> {
        let db = crate::db::connect("sqlite::memory:").await?;
        crate::db::run_migrations(&db).await?;
        let store = EvalStore::new(db);
        let pending = PendingEvalDecisionStore::default();
        let invocation = EvalInvocation::new("local");
        pending.insert(
            &invocation,
            PendingEvalDecision {
                request_id: "request-1".into(),
                decision_id: "decision-1".into(),
                policy: "auto:cost".into(),
                policy_digest: DIGEST.into(),
                route_projection: "opening".into(),
                request_key: "opening".into(),
                selected_tier: "economy".into(),
                selected_effort: Some(ReasoningEffort::Low),
                baseline_tier: Some("strong".into()),
                baseline_effort: Some(ReasoningEffort::High),
                experiment: Some(EvalExperimentRef {
                    experiment_id: DIGEST.into(),
                    arm: ExperimentArm::Challenger,
                    assignment_unit: ExperimentAssignmentUnit::Task,
                    assignment_id_digest:
                        "sha256:abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789"
                            .into(),
                    challenger_propensity_ppm: 100_000,
                }),
                route_measurement: None,
                preset: Some("auto:cost".into()),
                holdout: false,
                continuation_proposed_tier: Some("balanced".into()),
                continuation_proposed_model: Some("balanced:balanced-model".into()),
                continuation_proposed_effort: Some(ReasoningEffort::Medium),
                continuation_adjustment: Some("pin".into()),
                predicted_role: None,
                predicted_task_family: Some("code:review".into()),
                predicted_action: None,
                prediction_confidence_ppm: None,
                task_family_confidence_ppm: Some(1_500_000),
                task_family_reason_codes: vec![
                    "task_code_review".into(),
                    "customer_secret".into(),
                    "task_code_review".into(),
                ],
                predictor_contract_digest: None,
                prediction_confidence_kind: None,
                observation: None,
                observed_at: "2026-08-08T00:00:00Z".into(),
            },
        );
        let recorder = EvalSettlementRecorder::new(store.clone(), pending.clone());
        let mut context = settlement_context_with(&invocation);
        context.reasoning_effort = Some(ReasoningEffort::Low);

        recorder.record(&mut context).await?;

        let subject = store
            .subject("request:request-1")
            .await?
            .ok_or_else(|| anyhow::anyhow!("request subject missing"))?;
        assert_eq!(subject.policy_digest, DIGEST);
        assert_eq!(subject.decisions.len(), 1);
        assert_eq!(
            subject.decisions[0].selected_effort,
            Some(ReasoningEffort::Low)
        );
        assert_eq!(
            subject.decisions[0].baseline_effort,
            Some(ReasoningEffort::High)
        );
        assert_eq!(
            subject.decisions[0]
                .experiment
                .as_ref()
                .map(|experiment| experiment.arm),
            Some(ExperimentArm::Challenger)
        );
        let experiment = subject.decisions[0]
            .experiment
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("settlement must preserve experiment metadata"))?;
        assert_eq!(experiment.experiment_id, DIGEST);
        assert_eq!(experiment.assignment_unit, ExperimentAssignmentUnit::Task);
        assert_eq!(
            experiment.assignment_id_digest,
            "sha256:abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789"
        );
        assert_eq!(experiment.challenger_propensity_ppm, 100_000);
        assert!(subject.evidence.iter().all(|item| item.redacted));
        let evidence = subject
            .evidence
            .first()
            .ok_or_else(|| anyhow::anyhow!("request outcome evidence missing"))?;
        assert_eq!(
            evidence.attributes.get("continuation_proposed_tier"),
            Some(&"balanced".to_owned())
        );
        assert_eq!(
            evidence.attributes.get("continuation_proposed_model"),
            Some(&"balanced:balanced-model".to_owned())
        );
        assert_eq!(
            evidence.attributes.get("continuation_proposed_effort"),
            Some(&"medium".to_owned())
        );
        assert_eq!(
            evidence.attributes.get("continuation_adjustment"),
            Some(&"pin".to_owned())
        );
        assert_eq!(
            evidence.attributes.get("predicted_task_family"),
            Some(&"code:review".to_owned())
        );
        assert_eq!(
            evidence.attributes.get("task_family_confidence_ppm"),
            Some(&"1000000".to_owned())
        );
        assert_eq!(
            evidence.attributes.get("task_family_reason_codes"),
            Some(&"task_code_review".to_owned())
        );
        assert_eq!(
            subject.requested_dimensions,
            BTreeSet::from([
                "cost.usd_micros".to_string(),
                "latency.ms".to_string(),
                "quality.pass".to_string(),
            ])
        );
        assert!(pending.peek(&invocation, "local").is_none());
        Ok(())
    }

    #[tokio::test]
    async fn settlement_storage_failure_preserves_pending_decision_for_retry() -> anyhow::Result<()>
    {
        let db = crate::db::connect("sqlite::memory:").await?;
        crate::db::run_migrations(&db).await?;
        let store = EvalStore::new(db.clone());
        let pending = PendingEvalDecisionStore::default();
        let invocation = EvalInvocation::new("local");
        pending.insert(
            &invocation,
            PendingEvalDecision {
                request_id: "request-1".into(),
                decision_id: "decision-1".into(),
                policy: "auto:cost".into(),
                policy_digest: DIGEST.into(),
                route_projection: "opening".into(),
                request_key: "opening".into(),
                selected_tier: "economy".into(),
                selected_effort: None,
                baseline_tier: Some("strong".into()),
                baseline_effort: None,
                experiment: None,
                route_measurement: None,
                preset: Some("auto:cost".into()),
                holdout: false,
                continuation_proposed_tier: None,
                continuation_proposed_model: None,
                continuation_proposed_effort: None,
                continuation_adjustment: None,
                predicted_role: Some("implement".into()),
                predicted_task_family: Some("code:review".into()),
                predicted_action: Some("mutate".into()),
                prediction_confidence_ppm: Some(900_000),
                task_family_confidence_ppm: Some(800_000),
                task_family_reason_codes: Vec::new(),
                predictor_contract_digest: Some(
                    "sha256:7483fb5fa02c0141f568b82287234895c666fef426789e32783bdd3a00cea3ec"
                        .into(),
                ),
                prediction_confidence_kind: Some("heuristic_margin".into()),
                observation: Some(
                    crate::gateway::observability::prediction::PredictionObservation::new(
                        crate::gateway::observability::prediction::ObservedActionClass::Mutate,
                    ),
                ),
                observed_at: "2026-08-08T00:00:00Z".into(),
            },
        );
        db.execute(Statement::from_string(
            DatabaseBackend::Sqlite,
            "CREATE TRIGGER fail_eval_subject BEFORE INSERT ON eval_subjects \
             BEGIN SELECT RAISE(FAIL, 'injected eval failure'); END"
                .to_owned(),
        ))
        .await?;
        let recorder = EvalSettlementRecorder::new(store.clone(), pending.clone());
        let mut context = settlement_context_with(&invocation);

        assert!(recorder.record(&mut context).await.is_err());
        assert!(pending.peek(&invocation, "local").is_some());
        db.execute(Statement::from_string(
            DatabaseBackend::Sqlite,
            "DROP TRIGGER fail_eval_subject".to_owned(),
        ))
        .await?;

        recorder.record(&mut context).await?;

        assert!(pending.peek(&invocation, "local").is_none());
        assert!(store.subject("request:request-1").await?.is_some());
        Ok(())
    }

    #[tokio::test]
    async fn request_subject_digest_is_stable_across_storage_retries() -> anyhow::Result<()> {
        let db = crate::db::connect("sqlite::memory:").await?;
        crate::db::run_migrations(&db).await?;
        let recorder =
            EvalSettlementRecorder::new(EvalStore::new(db), PendingEvalDecisionStore::default());
        let decision = PendingEvalDecision {
            request_id: "request-1".into(),
            decision_id: "decision-1".into(),
            policy: "auto:cost".into(),
            policy_digest: DIGEST.into(),
            route_projection: "opening".into(),
            request_key: "opening".into(),
            selected_tier: "economy".into(),
            selected_effort: None,
            baseline_tier: Some("strong".into()),
            baseline_effort: None,
            experiment: None,
            route_measurement: None,
            preset: Some("auto:cost".into()),
            holdout: false,
            continuation_proposed_tier: None,
            continuation_proposed_model: None,
            continuation_proposed_effort: None,
            continuation_adjustment: None,
            predicted_role: Some("implement".into()),
            predicted_task_family: Some("code:review".into()),
            predicted_action: Some("mutate".into()),
            prediction_confidence_ppm: Some(900_000),
            task_family_confidence_ppm: Some(800_000),
            task_family_reason_codes: Vec::new(),
            predictor_contract_digest: Some(
                "sha256:7483fb5fa02c0141f568b82287234895c666fef426789e32783bdd3a00cea3ec".into(),
            ),
            prediction_confidence_kind: Some("heuristic_margin".into()),
            observation: None,
            observed_at: "2026-08-08T00:00:00Z".into(),
        };
        let first = recorder.subject(&decision, &settlement_context())?;
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        let second = recorder.subject(&decision, &settlement_context())?;

        assert_eq!(
            crate::storage::evaluation::subject_digest(&first)?,
            crate::storage::evaluation::subject_digest(&second)?
        );
        Ok(())
    }

    fn settlement_context() -> SettlementContext {
        SettlementContext {
            operation: bitrouter_ai::types::ModelOperation::Generation,
            request_id: "request-1".into(),
            caller: CallerContext::local(),
            target: None,
            model_id: "model".into(),
            reasoning_effort: None,
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

    #[tokio::test]
    async fn native_subject_uses_the_same_admitted_tariff_as_metering() -> anyhow::Result<()> {
        use crate::gateway::pipeline::types::RoutingTarget;
        use crate::metering::pricing::{ModelPricing, PricingTable};
        use bitrouter_ai::types::{ApiProtocol, ModelOperation};
        let db = crate::db::connect("sqlite::memory:").await?;
        crate::db::run_migrations(&db).await?;
        let recorder =
            EvalSettlementRecorder::new(EvalStore::new(db), PendingEvalDecisionStore::default());
        let mut context = settlement_context();
        context.operation = ModelOperation::Decisions;
        context.completion_tokens = 0;
        context.target = Some(RoutingTarget {
            registry: None,
            model_constraints: Default::default(),
            provider_name: context.provider_id.clone(),
            service_id: context.model_id.clone(),
            api_protocol: ApiProtocol::Decisions,
            api_base: "https://fixture.invalid/v1".into(),
            api_key: String::new(),
            api_key_override: None,
            api_base_override: None,
            account_label: None,
            auth_scheme: Default::default(),
            headers: Vec::new(),
            chat_token_limit_field: None,
            chat_supports_store: None,
            chat_supports_stream_options: None,
            chat_google_extensions: false,
            reasoning_effort: None,
        });
        let target = context
            .target
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("missing target"))?;
        let mut table = PricingTable::new();
        table.configure_endpoint(&target.provider_name, None, &target.api_base);
        table.insert_for_protocol(
            &target.provider_name,
            &target.service_id,
            ApiProtocol::Decisions,
            ModelPricing::cache_aware(Some(0.1), Some(0.0), Some(0.0), Some(0.0)),
        );
        let snapshot = table.snapshot(target);
        context.emit(snapshot.clone());
        table.insert_for_protocol(
            "provider",
            "model",
            ApiProtocol::Decisions,
            ModelPricing::new(100.0, 100.0),
        );
        let metering = crate::metering::tariff::settlement_charge_evidence(&context);
        let subject = recorder.subject(&test_decision("request-1"), &context)?;
        let attributes = &subject
            .evidence
            .first()
            .ok_or_else(|| anyhow::anyhow!("missing outcome"))?
            .attributes;
        assert_eq!(metering.charge_micro_usd, Some(1));
        assert_eq!(
            attributes.get("cost_micro_usd").map(String::as_str),
            Some("1")
        );
        assert_eq!(
            attributes.get("outbound_protocol").map(String::as_str),
            Some("decisions")
        );
        assert_eq!(
            attributes.get("pricing_version"),
            Some(&snapshot.tariff.pricing_version)
        );
        context.cache_read_tokens = 4;
        let gated = recorder.subject(&test_decision("request-1"), &context)?;
        assert!(
            !gated
                .evidence
                .first()
                .ok_or_else(|| anyhow::anyhow!("missing outcome"))?
                .attributes
                .contains_key("cost_micro_usd")
        );
        assert!(
            crate::metering::tariff::settlement_charge_evidence(&context)
                .charge_micro_usd
                .is_none()
        );
        Ok(())
    }

    fn settlement_context_with(invocation: &EvalInvocation) -> SettlementContext {
        let mut context = settlement_context();
        context.emit(invocation.clone());
        context
    }
}
