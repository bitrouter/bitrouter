//! Compatibility import for pre-eval workflow reward artifacts.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::eval::EvalService;
use crate::eval::admission::SubmissionPrincipal;
use crate::eval::types::{
    EVAL_SCHEMA_VERSION, EvalDecisionRef, EvalScope, EvalSubject, EvalVerdict, EvaluationResult,
    EvaluatorIdentity, EvaluatorKind, EvidenceItem, MetricUnit, MetricValue, canonical_digest,
    evidence_digest,
};
use crate::workflow_state::archive::{
    RequestTransportOutcome, SemanticPolicyTransitionCandidate, SemanticSettlementOutcome,
};
use crate::workflow_state::predictive::CanonicalPolicyProjection;

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RewardEvalImportSummary {
    pub candidate_count: usize,
    pub admitted_count: usize,
    pub skipped_count: usize,
    pub skipped_reasons: BTreeMap<String, usize>,
    pub eval_ids: Vec<String>,
}

/// Translate legacy benchmark reward artifacts into the generic eval exchange.
/// This is intentionally one-way: the adapter cannot write policy locks or the
/// sealed legacy adequacy tables.
pub async fn import_semantic_reward_feedback(
    service: &EvalService,
    candidates: &[SemanticPolicyTransitionCandidate],
) -> anyhow::Result<RewardEvalImportSummary> {
    let mut summary = RewardEvalImportSummary {
        candidate_count: candidates.len(),
        ..RewardEvalImportSummary::default()
    };
    for candidate in candidates {
        let Some((policy, request_key)) = candidate_policy_key(candidate) else {
            import_skip(&mut summary, "missing_named_policy");
            continue;
        };
        if candidate.request_transport_outcome != RequestTransportOutcome::Completed {
            import_skip(&mut summary, "request_not_completed");
            continue;
        }
        if !matches!(
            candidate.settlement_outcome,
            SemanticSettlementOutcome::AuthoritativeComputed
                | SemanticSettlementOutcome::ProviderReportedComputed
        ) {
            import_skip(&mut summary, "settlement_not_authoritative_computed");
            continue;
        }
        let (Some(selected_tier), Some(baseline_tier)) = (
            candidate.selected_tier.as_deref(),
            candidate.static_tier.as_deref(),
        ) else {
            import_skip(&mut summary, "missing_tier_transition");
            continue;
        };
        let identity = canonical_digest(&(
            "workflow-reward-adapter-v1",
            candidate.request_id.as_str(),
            candidate.task_id.as_str(),
            policy.as_str(),
            request_key.as_str(),
        ))?;
        let eval_id = format!("reward:{}", identity.trim_start_matches("sha256:"));
        let policy_digest = match &candidate.policy_digest {
            Some(digest) => digest.clone(),
            None => canonical_digest(&("legacy-policy-attribution", policy.as_str()))?,
        };
        let decision_id = format!("{}:{policy}", candidate.request_id);
        let attributes = BTreeMap::from([
            ("reward".into(), candidate.reward.to_string()),
            ("task_id".into(), candidate.task_id.clone()),
            (
                "transport".into(),
                format!("{:?}", candidate.request_transport_outcome).to_ascii_lowercase(),
            ),
            (
                "settlement".into(),
                format!("{:?}", candidate.settlement_outcome).to_ascii_lowercase(),
            ),
        ]);
        let evidence = vec![EvidenceItem {
            evidence_id: "benchmark-outcome".into(),
            kind: "task.reward".into(),
            digest: canonical_digest(&attributes)?,
            redacted: true,
            attributes,
        }];
        let subject = EvalSubject {
            schema_version: EVAL_SCHEMA_VERSION,
            eval_id: eval_id.clone(),
            scope: EvalScope::Task,
            subject_id: candidate.task_id.clone(),
            policy_digest: policy_digest.clone(),
            preset: Some(policy.clone()),
            cohort: Some(candidate.session_key.clone()),
            holdout: false,
            decisions: vec![EvalDecisionRef {
                decision_id,
                policy,
                route_projection: None,
                request_key,
                selected_tier: selected_tier.to_string(),
                selected_effort: candidate.selected_effort,
                baseline_tier: Some(baseline_tier.to_string()),
                baseline_effort: candidate.static_effort,
                predictive_v1_fallback_tier: None,
                policy_digest,
            }],
            requested_dimensions: BTreeSet::from(["quality.pass".into()]),
            evidence_digest: evidence_digest(&evidence)?,
            evidence,
            observed_at: "1970-01-01T00:00:00Z".into(),
        };
        service.store().insert_subject(&subject).await?;
        let passed = candidate.reward >= 1.0;
        let result = EvaluationResult {
            schema_version: EVAL_SCHEMA_VERSION,
            eval_id: eval_id.clone(),
            evidence_digest: subject.evidence_digest,
            evaluator: EvaluatorIdentity {
                authority_id: "local-workflow-reward".into(),
                evaluator_id: "workflow-reward-adapter".into(),
                kind: EvaluatorKind::TaskNative,
                version: "1".into(),
                config_digest: canonical_digest(&"workflow-reward-adapter-v1")?,
            },
            verdict: if passed {
                EvalVerdict::Pass
            } else {
                EvalVerdict::Fail
            },
            metrics: BTreeMap::from([(
                "quality.pass".into(),
                MetricValue::new(i64::from(passed), MetricUnit::Boolean),
            )]),
            hard_violations: Vec::new(),
            confidence_ppm: Some(1_000_000),
            evidence_refs: vec!["benchmark-outcome".into()],
            decision_credit: BTreeMap::new(),
            idempotency_key: format!("workflow-reward:{identity}"),
            submitted_at: "1970-01-01T00:00:00Z".into(),
        };
        let outcome = service
            .submit(result, SubmissionPrincipal::LocalOperator)
            .await?;
        if outcome.status == crate::eval::types::AdmissionStatus::Admitted {
            summary.admitted_count += 1;
            summary.eval_ids.push(eval_id);
        } else {
            import_skip(
                &mut summary,
                &format!("admission_{:?}", outcome.status).to_ascii_lowercase(),
            );
        }
    }
    Ok(summary)
}

fn candidate_policy_key(candidate: &SemanticPolicyTransitionCandidate) -> Option<(String, String)> {
    if let Some(policy) = candidate.policy.as_deref() {
        return CanonicalPolicyProjection::parse_key(&candidate.request_key)
            .map(|_| (policy.to_string(), candidate.request_key.clone()));
    }
    let (policy, request_key) = candidate.ledger_key.as_deref()?.split_once('\0')?;
    (!policy.is_empty() && CanonicalPolicyProjection::parse_key(request_key).is_some())
        .then(|| (policy.to_string(), request_key.to_string()))
}

fn import_skip(summary: &mut RewardEvalImportSummary, reason: &str) {
    summary.skipped_count += 1;
    *summary
        .skipped_reasons
        .entry(reason.to_string())
        .or_default() += 1;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adequacy::store::AdequacyStore;
    use crate::db;
    use crate::eval::EvalService;
    use crate::eval::store::EvalStore;

    const TOOL_FOLLOWUP_KEY: &str = "agent_trace/v1|tool_followup|normal";

    fn candidate(reward: f64) -> SemanticPolicyTransitionCandidate {
        SemanticPolicyTransitionCandidate {
            trace_id: "trace-1".into(),
            request_id: "req-1".into(),
            session_key: "trial-1".into(),
            task_id: "task-a".into(),
            reward,
            failed_reason: (reward < 1.0).then(|| "verifier_failed".into()),
            request_transport_outcome: RequestTransportOutcome::Completed,
            settlement_outcome: SemanticSettlementOutcome::AuthoritativeComputed,
            request_key: TOOL_FOLLOWUP_KEY.into(),
            ledger_key: None,
            policy: Some("auto:cost".into()),
            policy_digest: Some(
                "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into(),
            ),
            trace_state: "tool_followup".into(),
            static_tier: Some("capable".into()),
            selected_tier: Some("cheap".into()),
            tier_transition: Some("capable -> cheap".into()),
            static_model: Some("openai-codex:gpt-5.5".into()),
            selected_model: Some("bitrouter:moonshotai/kimi-k2.7-code".into()),
            model_transition: Some(
                "openai-codex:gpt-5.5 -> bitrouter:moonshotai/kimi-k2.7-code".into(),
            ),
            static_effort: None,
            selected_effort: None,
            effort_transition: None,
            target_transition: Some(
                "openai-codex:gpt-5.5@default -> bitrouter:moonshotai/kimi-k2.7-code@default"
                    .into(),
            ),
            reason: "exploration_locked".into(),
        }
    }

    #[tokio::test]
    async fn compatibility_reward_import_writes_only_the_eval_exchange() -> anyhow::Result<()> {
        let db = db::connect("sqlite::memory:").await?;
        db::run_migrations(&db).await?;
        let service = EvalService::new(EvalStore::new(db.clone()), Default::default());

        let summary = import_semantic_reward_feedback(&service, &[candidate(1.0)]).await?;

        assert_eq!(summary.admitted_count, 1);
        assert_eq!(service.store().list_subjects().await?.len(), 1);
        assert!(AdequacyStore::new(db).load_all().await?.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn compatibility_reward_import_preserves_the_exact_effort_treatment() -> anyhow::Result<()>
    {
        let db = db::connect("sqlite::memory:").await?;
        db::run_migrations(&db).await?;
        let service = EvalService::new(EvalStore::new(db), Default::default());
        let mut effort_candidate = candidate(1.0);
        effort_candidate.static_effort =
            Some(bitrouter_sdk::language_model::types::ReasoningEffort::High);
        effort_candidate.selected_effort =
            Some(bitrouter_sdk::language_model::types::ReasoningEffort::Low);

        import_semantic_reward_feedback(&service, &[effort_candidate]).await?;

        let subjects = service.store().list_subjects().await?;
        let decision = subjects
            .first()
            .and_then(|subject| subject.decisions.first())
            .ok_or_else(|| anyhow::anyhow!("imported effort decision is missing"))?;
        assert_eq!(
            decision.baseline_effort,
            Some(bitrouter_sdk::language_model::types::ReasoningEffort::High)
        );
        assert_eq!(
            decision.selected_effort,
            Some(bitrouter_sdk::language_model::types::ReasoningEffort::Low)
        );
        Ok(())
    }

    #[tokio::test]
    async fn incomplete_reward_is_not_imported() -> anyhow::Result<()> {
        let db = db::connect("sqlite::memory:").await?;
        db::run_migrations(&db).await?;
        let service = EvalService::new(EvalStore::new(db), Default::default());
        let mut incomplete = candidate(1.0);
        incomplete.request_transport_outcome = RequestTransportOutcome::Failed;

        let summary = import_semantic_reward_feedback(&service, &[incomplete]).await?;

        assert_eq!(summary.admitted_count, 0);
        assert_eq!(summary.skipped_reasons["request_not_completed"], 1);
        assert!(service.store().list_subjects().await?.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn predictive_legacy_key_is_imported_into_the_eval_exchange() -> anyhow::Result<()> {
        let db = db::connect("sqlite::memory:").await?;
        db::run_migrations(&db).await?;
        let service = EvalService::new(EvalStore::new(db), Default::default());
        let mut predictive = candidate(1.0);
        predictive.policy = None;
        predictive.request_key = "agent_route/v1|implement|normal".into();
        predictive.ledger_key = Some("auto\0agent_route/v1|implement|normal".into());

        let summary = import_semantic_reward_feedback(&service, &[predictive]).await?;

        assert_eq!(summary.admitted_count, 1);
        assert_eq!(service.store().list_subjects().await?.len(), 1);
        Ok(())
    }

    #[tokio::test]
    async fn malformed_namespaced_reward_is_not_imported() -> anyhow::Result<()> {
        let db = db::connect("sqlite::memory:").await?;
        db::run_migrations(&db).await?;
        let service = EvalService::new(EvalStore::new(db), Default::default());
        let mut malformed = candidate(1.0);
        malformed.request_key = "agent_route/v2|developer|normal".into();

        let summary = import_semantic_reward_feedback(&service, &[malformed]).await?;

        assert_eq!(summary.admitted_count, 0);
        assert_eq!(summary.skipped_reasons["missing_named_policy"], 1);
        assert!(service.store().list_subjects().await?.is_empty());
        Ok(())
    }
}
