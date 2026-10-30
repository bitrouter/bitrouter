use std::collections::{BTreeMap, BTreeSet};

use sea_orm::IntoActiveModel;

use super::*;
use crate::storage::evaluation::*;
use bitrouter_ai::eval::{EvalDecisionRef, EvalVerdict, EvaluatorIdentity, EvaluatorKind};

async fn store() -> anyhow::Result<EvalStore> {
    let db = crate::db::connect("sqlite::memory:").await?;
    crate::db::run_migrations(&db).await?;
    Ok(EvalStore::new(db))
}

pub(super) fn subject() -> anyhow::Result<EvalSubject> {
    let evidence = Vec::new();
    Ok(EvalSubject {
        schema_version: 1,
        eval_id: "eval-1".into(),
        scope: EvalScope::Request,
        subject_id: "request-1".into(),
        policy_digest: "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
            .into(),
        preset: Some("auto".into()),
        cohort: None,
        holdout: false,
        decisions: Vec::new(),
        requested_dimensions: BTreeSet::new(),
        evidence_digest: evidence_digest(&evidence)?,
        evidence,
        observed_at: "2026-07-30T00:00:00Z".into(),
    })
}

pub(super) fn result(verdict: EvalVerdict) -> EvaluationResult {
    EvaluationResult {
        schema_version: 1,
        eval_id: "eval-1".into(),
        evidence_digest: "sha256:4f53cda18c2baa0c0354bb5f9a3ecbe5ed12ab4d8e11ba873c2f11161202b945"
            .into(),
        evaluator: EvaluatorIdentity {
            authority_id: "local".into(),
            evaluator_id: "human".into(),
            kind: EvaluatorKind::Human,
            version: "1".into(),
            config_digest:
                "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into(),
        },
        verdict,
        metrics: BTreeMap::new(),
        hard_violations: Vec::new(),
        confidence_ppm: None,
        evidence_refs: Vec::new(),
        decision_credit: BTreeMap::new(),
        idempotency_key: "submission-1".into(),
        submitted_at: "2026-07-30T00:01:00Z".into(),
    }
}

#[tokio::test]
async fn subject_and_result_round_trip() -> anyhow::Result<()> {
    let store = store().await?;
    let subject = subject()?;
    store.insert_subject(&subject).await?;
    store.insert_result(&result(EvalVerdict::Pass)).await?;

    assert_eq!(store.subject("eval-1").await?, Some(subject));
    assert_eq!(store.results_for_subject("eval-1").await?.len(), 1);
    Ok(())
}

#[tokio::test]
async fn idempotency_replays_identical_content_and_rejects_conflicts() -> anyhow::Result<()> {
    let store = store().await?;
    store.insert_subject(&subject()?).await?;
    let pass = result(EvalVerdict::Pass);
    assert!(matches!(
        store.insert_result(&pass).await?,
        ResultInsertOutcome::Inserted { .. }
    ));
    assert!(matches!(
        store.insert_result(&pass).await?,
        ResultInsertOutcome::Duplicate { .. }
    ));
    assert!(
        store
            .insert_result(&result(EvalVerdict::Fail))
            .await
            .is_err()
    );
    Ok(())
}

#[tokio::test]
async fn snapshot_root_commits_subject_decisions() -> anyhow::Result<()> {
    async fn root_for(selected_tier: &str) -> anyhow::Result<String> {
        let store = store().await?;
        let mut subject = subject()?;
        subject.decisions = vec![EvalDecisionRef {
            decision_id: "decision-1".into(),
            policy: "auto".into(),
            route_projection: "semantic_route/v1|unknown|implement|normal".into(),
            request_key: "semantic_route/v1|unknown|implement|normal".into(),
            selected_tier: selected_tier.into(),
            selected_effort: None,
            baseline_tier: Some("strong".into()),
            baseline_effort: None,
            policy_digest: subject.policy_digest.clone(),
            experiment: None,
            route_measurement: None,
        }];
        store.insert_subject(&subject).await?;
        let inserted = store.insert_result(&result(EvalVerdict::Pass)).await?;
        store
            .append_admission_event(
                inserted.result_id(),
                AdmissionStatus::Admitted,
                "admitted",
                "local",
            )
            .await?;
        Ok(store
            .freeze_snapshot("2026-07-30T00:02:00Z")
            .await?
            .evidence_root)
    }

    assert_ne!(root_for("economy").await?, root_for("strong").await?);
    Ok(())
}

#[tokio::test]
async fn selected_snapshot_excludes_other_admitted_local_results() -> anyhow::Result<()> {
    let store = store().await?;
    let historical_subject = subject()?;
    store.insert_subject(&historical_subject).await?;
    let historical = store.insert_result(&result(EvalVerdict::Pass)).await?;
    store
        .append_admission_event(
            historical.result_id(),
            AdmissionStatus::Admitted,
            "admitted",
            "local",
        )
        .await?;

    let mut current_subject = subject()?;
    current_subject.eval_id = "eval-current".into();
    current_subject.subject_id = "request-current".into();
    store.insert_subject(&current_subject).await?;
    let mut current_result = result(EvalVerdict::Pass);
    current_result.eval_id = current_subject.eval_id.clone();
    current_result.idempotency_key = "submission-current".into();
    let current = store.insert_result(&current_result).await?;
    store
        .append_admission_event(
            current.result_id(),
            AdmissionStatus::Admitted,
            "admitted",
            "local",
        )
        .await?;

    let snapshot = store
        .freeze_snapshot_for_result_ids(
            "2026-07-30T00:02:00Z",
            "local",
            &[current.result_id().to_string()],
        )
        .await?;

    assert_eq!(snapshot.entries.len(), 1);
    assert_eq!(snapshot.entries[0].result_id, current.result_id());
    assert!(
        store
            .freeze_snapshot_for_result_ids(
                "2026-07-30T00:03:00Z",
                "local",
                &["sha256:missing".into()],
            )
            .await
            .is_err()
    );
    Ok(())
}

#[tokio::test]
async fn snapshot_manifest_tampering_breaks_content_address_verification() -> anyhow::Result<()> {
    let store = store().await?;
    store.insert_subject(&subject()?).await?;
    let inserted = store.insert_result(&result(EvalVerdict::Pass)).await?;
    store
        .append_admission_event(
            inserted.result_id(),
            AdmissionStatus::Admitted,
            "admitted",
            "local",
        )
        .await?;
    let snapshot = store.freeze_snapshot("2026-07-30T00:02:00Z").await?;
    let row = snapshot_entity::Entity::find_by_id(&snapshot.evidence_root)
        .one(&store.db)
        .await?
        .ok_or_else(|| anyhow::anyhow!("snapshot row is missing"))?;
    let mut tampered = snapshot.clone();
    tampered.entries[0].eval_id = "different-eval".into();
    let mut active = row.into_active_model();
    active.manifest_json = Set(serde_json::to_string(&tampered)?);
    active.update(&store.db).await?;

    assert!(
        store
            .snapshot_by_root(&snapshot.evidence_root)
            .await
            .is_err()
    );
    Ok(())
}
