//! Authority-bound admission for externally supplied evaluation outcomes.

use anyhow::Result;
use bitrouter_sdk::config::{EvalAuthorityConfig, EvalAuthorityKind, EvalConfig};
use serde::Serialize;

use super::store::{EvalStore, ResultInsertOutcome};
use super::types::{
    AdmissionStatus, EvalVerdict, EvaluationResult, EvaluatorKind, validate_result_for_subject,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubmissionPrincipal {
    LocalOperator,
    BuiltinTrajectory { owner_user_id: String },
    ApiKey { key_id: String, user_id: String },
}

impl SubmissionPrincipal {
    pub fn owner_user_id(&self) -> &str {
        match self {
            Self::LocalOperator => "local",
            Self::BuiltinTrajectory { owner_user_id } => owner_user_id,
            Self::ApiKey { user_id, .. } => user_id,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AdmissionOutcome {
    pub result_id: String,
    pub status: AdmissionStatus,
    pub reason: String,
    pub duplicate: bool,
}

pub async fn submit(
    store: &EvalStore,
    config: &EvalConfig,
    result: EvaluationResult,
    principal: SubmissionPrincipal,
) -> Result<AdmissionOutcome> {
    let subject = match &principal {
        SubmissionPrincipal::LocalOperator => {
            store.subject_for_owner(&result.eval_id, "local").await?
        }
        SubmissionPrincipal::BuiltinTrajectory { owner_user_id } => {
            store
                .subject_for_owner(&result.eval_id, owner_user_id)
                .await?
        }
        SubmissionPrincipal::ApiKey { user_id, .. } => {
            store.subject_for_owner(&result.eval_id, user_id).await?
        }
    }
    .ok_or_else(|| anyhow::anyhow!("unknown eval subject '{}'", result.eval_id))?;
    validate_result_for_subject(&result, &subject)?;
    if matches!(&principal, SubmissionPrincipal::BuiltinTrajectory { .. }) {
        validate_builtin_trajectory_result(&result)?;
    }
    let insertion = store
        .insert_result_owned(&result, principal.owner_user_id())
        .await?;
    let duplicate = matches!(insertion, ResultInsertOutcome::Duplicate { .. });
    let result_id = insertion.result_id().to_string();

    let authority = match &principal {
        SubmissionPrincipal::LocalOperator | SubmissionPrincipal::BuiltinTrajectory { .. } => None,
        SubmissionPrincipal::ApiKey { key_id, user_id } => {
            let Some(authority) = config.authorities.get(&result.evaluator.authority_id) else {
                return record(
                    store,
                    result_id,
                    AdmissionStatus::Rejected,
                    "unknown evaluator authority",
                    &result.evaluator.authority_id,
                    duplicate,
                )
                .await;
            };
            if !principal_matches(authority, key_id, user_id) {
                return record(
                    store,
                    result_id,
                    AdmissionStatus::Rejected,
                    "authenticated principal is not bound to evaluator authority",
                    &result.evaluator.authority_id,
                    duplicate,
                )
                .await;
            }
            Some(authority)
        }
    };

    if let Some(authority) = authority {
        if !kind_matches(authority.kind, result.evaluator.kind) {
            return record(
                store,
                result_id,
                AdmissionStatus::Rejected,
                "evaluator kind does not match authority configuration",
                &result.evaluator.authority_id,
                duplicate,
            )
            .await;
        }
        if result
            .metrics
            .keys()
            .any(|metric| !metric_allowed(authority, metric))
        {
            return record(
                store,
                result_id,
                AdmissionStatus::Rejected,
                "authority submitted a metric outside its scope",
                &result.evaluator.authority_id,
                duplicate,
            )
            .await;
        }
        if !result.hard_violations.is_empty() && !authority.allow_hard_fail {
            return record(
                store,
                result_id,
                AdmissionStatus::Rejected,
                "authority cannot submit hard violations",
                &result.evaluator.authority_id,
                duplicate,
            )
            .await;
        }
    }

    if duplicate && let Some(existing) = store.latest_admissions().await?.remove(&result_id) {
        return Ok(AdmissionOutcome {
            result_id,
            status: existing.status,
            reason: existing.reason,
            duplicate: true,
        });
    }

    if subject.holdout {
        return record(
            store,
            result_id,
            AdmissionStatus::HeldOut,
            "subject belongs to a holdout cohort",
            &result.evaluator.authority_id,
            duplicate,
        )
        .await;
    }

    let latest = store.latest_admissions().await?;
    let prior_results = store.results_for_subject(&result.eval_id).await?;
    let conflicting = prior_results
        .iter()
        .filter(|prior| {
            prior.result_id != result_id
                && latest.get(&prior.result_id).map(|event| event.status)
                    == Some(AdmissionStatus::Admitted)
                && verdicts_conflict(prior.result.verdict, result.verdict)
        })
        .collect::<Vec<_>>();
    if let Some(highest_prior_rank) = conflicting
        .iter()
        .map(|prior| evaluator_rank(prior.result.evaluator.kind))
        .max()
    {
        let new_rank = evaluator_rank(result.evaluator.kind);
        if highest_prior_rank > new_rank {
            return record(
                store,
                result_id,
                AdmissionStatus::Disputed,
                "result conflicts with a higher-authority admitted outcome",
                &result.evaluator.authority_id,
                duplicate,
            )
            .await;
        }
        if highest_prior_rank == new_rank {
            for prior in &conflicting {
                store
                    .append_admission_event(
                        &prior.result_id,
                        AdmissionStatus::Disputed,
                        "conflicting outcome at the same authority level",
                        &result.evaluator.authority_id,
                    )
                    .await?;
            }
            return record(
                store,
                result_id,
                AdmissionStatus::Disputed,
                "conflicting outcome at the same authority level",
                &result.evaluator.authority_id,
                duplicate,
            )
            .await;
        }
        for prior in conflicting {
            store
                .append_admission_event(
                    &prior.result_id,
                    AdmissionStatus::Disputed,
                    "superseded by a conflicting higher-authority outcome",
                    &result.evaluator.authority_id,
                )
                .await?;
        }
    }

    record(
        store,
        result_id,
        AdmissionStatus::Admitted,
        "admitted",
        &result.evaluator.authority_id,
        duplicate,
    )
    .await
}

pub(crate) fn validate_builtin_trajectory_result(result: &EvaluationResult) -> Result<()> {
    use crate::trajectory::evaluation::{
        TRAJECTORY_EVALUATOR_AUTHORITY_ID, TRAJECTORY_EVALUATOR_ID, TRAJECTORY_EVALUATOR_VERSION,
        trajectory_evaluator_config_digest,
    };

    if result.evaluator.authority_id != TRAJECTORY_EVALUATOR_AUTHORITY_ID
        || result.evaluator.evaluator_id != TRAJECTORY_EVALUATOR_ID
        || result.evaluator.kind != EvaluatorKind::Generic
        || result.evaluator.version != TRAJECTORY_EVALUATOR_VERSION
        || result.evaluator.config_digest != trajectory_evaluator_config_digest()?
        || result.verdict != EvalVerdict::Inconclusive
        || !result.hard_violations.is_empty()
        || result.confidence_ppm.is_some()
    {
        anyhow::bail!("trusted trajectory principal requires the built-in operational evaluator")
    }
    if result.metrics.keys().any(|metric| {
        !metric.starts_with("trajectory.")
            && !matches!(metric.as_str(), "cost.usd_micros" | "latency.ms")
    }) {
        anyhow::bail!("trusted trajectory principal submitted a metric outside its scope")
    }
    Ok(())
}

async fn record(
    store: &EvalStore,
    result_id: String,
    status: AdmissionStatus,
    reason: &str,
    authority_id: &str,
    duplicate: bool,
) -> Result<AdmissionOutcome> {
    store
        .append_admission_event(&result_id, status, reason, authority_id)
        .await?;
    Ok(AdmissionOutcome {
        result_id,
        status,
        reason: reason.to_string(),
        duplicate,
    })
}

fn principal_matches(authority: &EvalAuthorityConfig, key_id: &str, user_id: &str) -> bool {
    authority
        .api_key_ids
        .iter()
        .any(|candidate| candidate == key_id)
        || authority
            .user_ids
            .iter()
            .any(|candidate| candidate == user_id)
}

fn metric_allowed(authority: &EvalAuthorityConfig, metric: &str) -> bool {
    authority.allowed_metrics.iter().any(|allowed| {
        allowed == "*"
            || allowed == metric
            || allowed
                .strip_suffix(".*")
                .is_some_and(|prefix| metric.starts_with(&format!("{prefix}.")))
    })
}

fn kind_matches(configured: EvalAuthorityKind, submitted: EvaluatorKind) -> bool {
    matches!(
        (configured, submitted),
        (EvalAuthorityKind::TaskNative, EvaluatorKind::TaskNative)
            | (EvalAuthorityKind::Human, EvaluatorKind::Human)
            | (EvalAuthorityKind::Enterprise, EvaluatorKind::Enterprise)
            | (EvalAuthorityKind::Agentic, EvaluatorKind::Agentic)
            | (EvalAuthorityKind::Generic, EvaluatorKind::Generic)
    )
}

fn evaluator_rank(kind: EvaluatorKind) -> u8 {
    match kind {
        EvaluatorKind::TaskNative => 4,
        EvaluatorKind::Human | EvaluatorKind::Enterprise => 3,
        EvaluatorKind::Generic => 2,
        EvaluatorKind::Agentic => 1,
    }
}

fn verdicts_conflict(left: EvalVerdict, right: EvalVerdict) -> bool {
    matches!(
        (left, right),
        (EvalVerdict::Pass, EvalVerdict::Fail) | (EvalVerdict::Fail, EvalVerdict::Pass)
    )
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet, HashMap};

    use bitrouter_sdk::config::{EvalAuthorityConfig, EvalAuthorityKind};

    use super::*;
    use crate::eval::types::{
        EVAL_SCHEMA_VERSION, EvalScope, EvalSubject, EvaluatorIdentity, evidence_digest,
    };

    async fn service() -> anyhow::Result<crate::eval::EvalService> {
        let db = crate::db::connect("sqlite::memory:").await?;
        crate::db::run_migrations(&db).await?;
        let store = EvalStore::new(db);
        let evidence = Vec::new();
        store
            .insert_subject_owned(
                &EvalSubject {
                    schema_version: EVAL_SCHEMA_VERSION,
                    eval_id: "eval-1".into(),
                    scope: EvalScope::Task,
                    subject_id: "task-1".into(),
                    policy_digest: digest(),
                    preset: Some("auto".into()),
                    cohort: None,
                    holdout: false,
                    decisions: Vec::new(),
                    requested_dimensions: BTreeSet::new(),
                    evidence_digest: evidence_digest(&evidence)?,
                    evidence,
                    observed_at: "2026-07-30T00:00:00Z".into(),
                },
                "user",
            )
            .await?;
        let authorities = HashMap::from([
            (
                "native".into(),
                EvalAuthorityConfig {
                    kind: EvalAuthorityKind::TaskNative,
                    api_key_ids: vec!["key-native".into()],
                    allowed_metrics: vec!["*".into()],
                    allow_hard_fail: true,
                    ..Default::default()
                },
            ),
            (
                "agent".into(),
                EvalAuthorityConfig {
                    kind: EvalAuthorityKind::Agentic,
                    api_key_ids: vec!["key-agent".into()],
                    allowed_metrics: vec!["*".into()],
                    ..Default::default()
                },
            ),
        ]);
        Ok(crate::eval::EvalService::new(
            store,
            EvalConfig { authorities },
        ))
    }

    fn result(
        authority: &str,
        kind: EvaluatorKind,
        verdict: EvalVerdict,
        idempotency_key: &str,
    ) -> EvaluationResult {
        EvaluationResult {
            schema_version: EVAL_SCHEMA_VERSION,
            eval_id: "eval-1".into(),
            evidence_digest:
                "sha256:4f53cda18c2baa0c0354bb5f9a3ecbe5ed12ab4d8e11ba873c2f11161202b945".into(),
            evaluator: EvaluatorIdentity {
                authority_id: authority.into(),
                evaluator_id: authority.into(),
                kind,
                version: "1".into(),
                config_digest: digest(),
            },
            verdict,
            metrics: BTreeMap::new(),
            hard_violations: if verdict == EvalVerdict::Fail {
                vec!["task.failed".into()]
            } else {
                Vec::new()
            },
            confidence_ppm: None,
            evidence_refs: Vec::new(),
            decision_credit: BTreeMap::new(),
            idempotency_key: idempotency_key.into(),
            submitted_at: "2026-07-30T00:01:00Z".into(),
        }
    }

    fn digest() -> String {
        "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into()
    }

    #[tokio::test]
    async fn builtin_trajectory_principal_is_owner_scoped_and_metric_limited() -> anyhow::Result<()>
    {
        let db = crate::db::connect("sqlite::memory:").await?;
        crate::db::run_migrations(&db).await?;
        let store = EvalStore::new(db);
        let evidence = Vec::new();
        store
            .insert_subject_owned(
                &EvalSubject {
                    schema_version: EVAL_SCHEMA_VERSION,
                    eval_id: "trajectory:episode-1:3".into(),
                    scope: EvalScope::Episode,
                    subject_id: "episode-1".into(),
                    policy_digest: digest(),
                    preset: Some("auto:cost".into()),
                    cohort: None,
                    holdout: false,
                    decisions: Vec::new(),
                    requested_dimensions: BTreeSet::from([
                        "trajectory.request_count".into(),
                        "latency.ms".into(),
                    ]),
                    evidence_digest: evidence_digest(&evidence)?,
                    evidence,
                    observed_at: "2026-08-01T00:00:02Z".into(),
                },
                "owner-a",
            )
            .await?;
        let service = crate::eval::EvalService::new(store, EvalConfig::default());
        let mut operational = result(
            "bitrouter.builtin",
            EvaluatorKind::Generic,
            EvalVerdict::Inconclusive,
            "trajectory-operational:episode-1:3",
        );
        operational.eval_id = "trajectory:episode-1:3".into();
        operational.evaluator.authority_id =
            crate::trajectory::evaluation::TRAJECTORY_EVALUATOR_AUTHORITY_ID.into();
        operational.evaluator.evaluator_id =
            crate::trajectory::evaluation::TRAJECTORY_EVALUATOR_ID.into();
        operational.evaluator.version =
            crate::trajectory::evaluation::TRAJECTORY_EVALUATOR_VERSION.into();
        operational.evaluator.config_digest =
            crate::trajectory::evaluation::trajectory_evaluator_config_digest()?;
        operational.evidence_digest =
            "sha256:4f53cda18c2baa0c0354bb5f9a3ecbe5ed12ab4d8e11ba873c2f11161202b945".into();
        operational.metrics = BTreeMap::from([
            (
                "trajectory.request_count".into(),
                crate::eval::types::MetricValue::new(1, crate::eval::types::MetricUnit::Count),
            ),
            (
                "latency.ms".into(),
                crate::eval::types::MetricValue::new(
                    100,
                    crate::eval::types::MetricUnit::Milliseconds,
                ),
            ),
        ]);

        let admitted = service
            .submit(
                operational.clone(),
                SubmissionPrincipal::BuiltinTrajectory {
                    owner_user_id: "owner-a".into(),
                },
            )
            .await?;
        assert_eq!(admitted.status, AdmissionStatus::Admitted);

        operational.idempotency_key = "trajectory-operational:wrong-owner".into();
        let wrong_owner = service
            .submit(
                operational,
                SubmissionPrincipal::BuiltinTrajectory {
                    owner_user_id: "owner-b".into(),
                },
            )
            .await;
        assert!(wrong_owner.is_err());
        Ok(())
    }

    #[tokio::test]
    async fn builtin_trajectory_principal_rejects_non_operational_identity_and_metrics()
    -> anyhow::Result<()> {
        let mut untrusted = result(
            "bitrouter.builtin",
            EvaluatorKind::Generic,
            EvalVerdict::Inconclusive,
            "builtin-invalid",
        );
        untrusted.metrics.insert(
            "quality.pass".into(),
            crate::eval::types::MetricValue::new(1, crate::eval::types::MetricUnit::Boolean),
        );
        assert!(super::validate_builtin_trajectory_result(&untrusted).is_err());
        untrusted.metrics.clear();
        untrusted.evaluator.evaluator_id = "another-evaluator".into();
        assert!(super::validate_builtin_trajectory_result(&untrusted).is_err());
        untrusted.evaluator.evaluator_id =
            crate::trajectory::evaluation::TRAJECTORY_EVALUATOR_ID.into();
        untrusted.evaluator.version = "2".into();
        untrusted.evaluator.config_digest =
            crate::trajectory::evaluation::trajectory_evaluator_config_digest()?;
        assert!(super::validate_builtin_trajectory_result(&untrusted).is_err());
        untrusted.evaluator.version =
            crate::trajectory::evaluation::TRAJECTORY_EVALUATOR_VERSION.into();
        untrusted.evaluator.config_digest = digest();
        assert!(super::validate_builtin_trajectory_result(&untrusted).is_err());
        untrusted.evaluator.config_digest =
            crate::trajectory::evaluation::trajectory_evaluator_config_digest()?;
        untrusted.confidence_ppm = Some(1);
        assert!(super::validate_builtin_trajectory_result(&untrusted).is_err());
        Ok(())
    }

    #[tokio::test]
    async fn remote_identity_must_match_authenticated_authority() -> anyhow::Result<()> {
        let service = service().await?;
        let outcome = service
            .submit(
                result(
                    "native",
                    EvaluatorKind::TaskNative,
                    EvalVerdict::Pass,
                    "one",
                ),
                SubmissionPrincipal::ApiKey {
                    key_id: "key-agent".into(),
                    user_id: "user".into(),
                },
            )
            .await?;
        assert_eq!(outcome.status, AdmissionStatus::Rejected);
        Ok(())
    }

    #[tokio::test]
    async fn agentic_pass_cannot_override_task_native_hard_failure() -> anyhow::Result<()> {
        let service = service().await?;
        let native = service
            .submit(
                result(
                    "native",
                    EvaluatorKind::TaskNative,
                    EvalVerdict::Fail,
                    "native",
                ),
                SubmissionPrincipal::ApiKey {
                    key_id: "key-native".into(),
                    user_id: "user".into(),
                },
            )
            .await?;
        assert_eq!(native.status, AdmissionStatus::Admitted);
        let agentic = service
            .submit(
                result("agent", EvaluatorKind::Agentic, EvalVerdict::Pass, "agent"),
                SubmissionPrincipal::ApiKey {
                    key_id: "key-agent".into(),
                    user_id: "user".into(),
                },
            )
            .await?;
        assert_eq!(agentic.status, AdmissionStatus::Disputed);
        Ok(())
    }

    #[tokio::test]
    async fn lower_authority_conflict_never_enters_training_evidence() -> anyhow::Result<()> {
        let service = service().await?;
        let native = service
            .submit(
                result(
                    "native",
                    EvaluatorKind::TaskNative,
                    EvalVerdict::Pass,
                    "native-pass",
                ),
                SubmissionPrincipal::ApiKey {
                    key_id: "key-native".into(),
                    user_id: "user".into(),
                },
            )
            .await?;
        assert_eq!(native.status, AdmissionStatus::Admitted);

        let mut agentic_result = result(
            "agent",
            EvaluatorKind::Agentic,
            EvalVerdict::Fail,
            "agent-fail",
        );
        agentic_result.hard_violations.clear();
        let agentic = service
            .submit(
                agentic_result,
                SubmissionPrincipal::ApiKey {
                    key_id: "key-agent".into(),
                    user_id: "user".into(),
                },
            )
            .await?;

        assert_eq!(agentic.status, AdmissionStatus::Disputed);
        let snapshot = service
            .store()
            .freeze_snapshot("2026-07-30T00:02:00Z")
            .await?;
        assert_eq!(snapshot.entries.len(), 1);
        assert_eq!(snapshot.entries[0].result_id, native.result_id);
        Ok(())
    }

    #[tokio::test]
    async fn held_out_results_never_enter_training_evidence() -> anyhow::Result<()> {
        let db = crate::db::connect("sqlite::memory:").await?;
        crate::db::run_migrations(&db).await?;
        let store = EvalStore::new(db);
        let evidence = Vec::new();
        store
            .insert_subject(&EvalSubject {
                schema_version: EVAL_SCHEMA_VERSION,
                eval_id: "eval-1".into(),
                scope: EvalScope::Task,
                subject_id: "held-out-task".into(),
                policy_digest: digest(),
                preset: Some("auto".into()),
                cohort: Some("holdout".into()),
                holdout: true,
                decisions: Vec::new(),
                requested_dimensions: BTreeSet::new(),
                evidence_digest: evidence_digest(&evidence)?,
                evidence,
                observed_at: "2026-07-30T00:00:00Z".into(),
            })
            .await?;
        let service = crate::eval::EvalService::new(store.clone(), EvalConfig::default());

        let outcome = service
            .submit(
                result(
                    "native",
                    EvaluatorKind::TaskNative,
                    EvalVerdict::Pass,
                    "held-out",
                ),
                SubmissionPrincipal::LocalOperator,
            )
            .await?;

        assert_eq!(outcome.status, AdmissionStatus::HeldOut);
        assert!(
            store
                .freeze_snapshot("2026-07-30T00:02:00Z")
                .await?
                .entries
                .is_empty()
        );
        Ok(())
    }
}
