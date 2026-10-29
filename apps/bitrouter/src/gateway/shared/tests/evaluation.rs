//! Shared built-in evaluation admission using real PostgreSQL.

mod request_subject;

use anyhow::{Context, Result, ensure};
use bitrouter_ai::eval::{AdmissionStatus, MetricUnit, MetricValue};
use sea_orm::{ConnectionTrait, DatabaseBackend, IsolationLevel, Statement};
use std::time::Duration;

use super::super::{schema, store::Store, test_database::EmptyDatabase};
use crate::{
    storage::evaluation::{result_digest, shared::SharedEvaluation},
    trajectory::{
        evaluation::{TrajectoryEvaluationEnvelope, build_operational_evaluation},
        shared::tests::guarded_batch,
        types::TrajectoryEventKind,
    },
};

const OWNER: &str = "evaluation-owner";
const FROZEN_AT: &str = "2026-10-21T01:00:00Z";

#[tokio::test]
#[ignore = "requires BITROUTER_TEST_DATABASE_URL with PostgreSQL CREATEDB privilege"]
async fn postgres_shared_evaluation() -> Result<()> {
    let mut database = EmptyDatabase::create().await?;
    let result = tokio::time::timeout(Duration::from_secs(30), exercise(&mut database))
        .await
        .context("shared evaluation regression deadline exceeded")
        .and_then(|result| result);
    // The timed future has been dropped; retained pools are closed before DROP.
    database.finish(result).await
}

async fn exercise(database: &mut EmptyDatabase) -> Result<()> {
    let db = database.connect(3, Duration::from_secs(3)).await?;
    let other_db = database.connect(3, Duration::from_secs(3)).await?;
    let namespace = format!("evaluation-{}", uuid::Uuid::new_v4());
    let first = Store::new(db.clone(), &namespace, Duration::from_secs(3))?;
    let second = Store::new(other_db.clone(), &namespace, Duration::from_secs(3))?;
    async {
        schema::initialize_all(&first).await?;
        let envelope = fixture()?;
        #[cfg(feature = "core")]
        {
            let local = crate::core::eval::store::EvalStore::new(db.clone());
            local.insert_subject_owned(&envelope.subject, OWNER).await?;
            local.insert_result_owned(&envelope.result, OWNER).await?;
        }
        run(&first, &second, &envelope).await?;
        #[cfg(feature = "core")]
        {
            let local = crate::core::eval::store::EvalStore::new(db.clone());
            ensure!(
                local
                    .subject_for_owner(&envelope.subject.eval_id, OWNER)
                    .await?
                    == Some(envelope.subject.clone())
            );
            ensure!(
                local
                    .result(&result_digest(&envelope.result)?)
                    .await?
                    .context("missing legacy result")?
                    .result
                    == envelope.result
            );
        }
        Ok::<(), anyhow::Error>(())
    }
    .await
}

fn fixture() -> Result<TrajectoryEvaluationEnvelope> {
    let episode = format!("eval-{}", uuid::Uuid::new_v4());
    let start = super::trajectory::start(&episode, "eval-request")?;
    let mut history = vec![start.event.clone()];
    history.extend(guarded_batch(&[], &start.event)?);
    history.push(super::trajectory::event(
        &episode,
        "eval-request",
        "settled",
        4,
        TrajectoryEventKind::RequestSettled,
    )?);
    build_operational_evaluation(&history)
}

async fn run(first: &Store, second: &Store, envelope: &TrajectoryEvaluationEnvelope) -> Result<()> {
    let shared = SharedEvaluation::new(first.namespace())?;
    let other_namespace = format!("evaluation-{}", uuid::Uuid::new_v4());
    let other = SharedEvaluation::new(&other_namespace)?;
    let id = result_digest(&envelope.result)?;
    let tx = first
        .begin_with_isolation(IsolationLevel::ReadCommitted)
        .await?;
    ensure!(
        shared
            .apply_trajectory_in(&tx, OWNER, envelope)
            .await
            .is_err()
    );
    tx.rollback().await?;
    let tx = first.begin().await?;
    let initial = shared.apply_trajectory_in(&tx, OWNER, envelope).await?;
    ensure!(!initial.duplicate && initial.status == AdmissionStatus::Admitted);
    tx.rollback().await?;
    let tx = second.begin().await?;
    ensure!(
        shared
            .subject_in(&tx, OWNER, &envelope.subject.eval_id)
            .await?
            .is_none()
    );
    ensure!(shared.result_in(&tx, OWNER, &id).await?.is_none());
    ensure!(shared.latest_admission_in(&tx, OWNER, &id).await?.is_none());
    tx.commit().await?;
    let (a, b) = tokio::join!(
        apply(first, &shared, envelope),
        apply(second, &shared, envelope)
    );
    let (a, b) = (a?, b?);
    ensure!(a.result_id == id && b.result_id == id && a.duplicate != b.duplicate);
    let tx = second.begin().await?;
    let admission = shared
        .latest_admission_in(&tx, OWNER, &id)
        .await?
        .context("missing admission")?;
    ensure!(
        shared
            .apply_trajectory_in(&tx, OWNER, envelope)
            .await?
            .duplicate
    );
    ensure!(shared.latest_admission_in(&tx, OWNER, &id).await? == Some(admission));
    ensure!(
        shared
            .subject_in(&tx, "foreign", &envelope.subject.eval_id)
            .await?
            .is_none()
    );
    ensure!(shared.result_in(&tx, "foreign", &id).await?.is_none());
    ensure!(
        shared
            .latest_admission_in(&tx, "foreign", &id)
            .await?
            .is_none()
    );
    other.apply_trajectory_in(&tx, OWNER, envelope).await?;
    ensure!(
        other
            .subject_in(&tx, OWNER, &envelope.subject.eval_id)
            .await?
            == Some(envelope.subject.clone())
    );
    ensure!(other.result_in(&tx, OWNER, &id).await? == Some(envelope.result.clone()));
    tx.commit().await?;
    // Later state in another namespace cannot change this namespace's latest fold.
    let tx = first.begin().await?;
    dispute(&tx, &other_namespace, &id).await?;
    ensure!(
        shared
            .latest_admission_in(&tx, OWNER, &id)
            .await?
            .context("missing admission")?
            .status
            == AdmissionStatus::Admitted
    );
    ensure!(
        other
            .apply_trajectory_in(&tx, OWNER, envelope)
            .await?
            .status
            == AdmissionStatus::Disputed
    );
    tx.commit().await?;
    conflicts(first, &shared, envelope).await?;
    boundary(first, second, &shared).await?;
    let tx = first.begin().await?;
    dispute(&tx, first.namespace(), &id).await?;
    let before = shared.latest_admission_in(&tx, OWNER, &id).await?;
    ensure!(
        shared
            .apply_trajectory_in(&tx, OWNER, envelope)
            .await?
            .status
            == AdmissionStatus::Disputed
    );
    ensure!(shared.latest_admission_in(&tx, OWNER, &id).await? == before);
    tx.commit().await?;
    Ok(())
}

async fn apply(
    store: &Store,
    shared: &SharedEvaluation,
    envelope: &TrajectoryEvaluationEnvelope,
) -> Result<bitrouter_ai::eval::AdmissionOutcome> {
    store
        .retry(|| async {
            let tx = store.begin().await?;
            let outcome = shared.apply_trajectory_in(&tx, OWNER, envelope).await?;
            tx.commit().await?;
            Ok(outcome)
        })
        .await
}

async fn dispute(tx: &sea_orm::DatabaseTransaction, namespace: &str, id: &str) -> Result<()> {
    // Test-only independent state transition. Production writers must serialize
    // on the subject before changing admissions; no arbitrary admission API exists.
    tx.execute(Statement::from_sql_and_values(DatabaseBackend::Postgres,
        "INSERT INTO gateway_shared_eval_admission_events (namespace,result_id,status,reason,authority_id,created_at)
         VALUES ($1,$2,'disputed','fixture dispute','fixture.authority',$3)",
        [namespace.into(),id.into(),FROZEN_AT.into()])).await?;
    Ok(())
}

async fn conflicts(
    store: &Store,
    shared: &SharedEvaluation,
    envelope: &TrajectoryEvaluationEnvelope,
) -> Result<()> {
    let id = result_digest(&envelope.result)?;
    let tx = store.begin().await?;
    ensure!(
        shared
            .apply_trajectory_in(&tx, "foreign", envelope)
            .await
            .is_err()
    );
    tx.rollback().await?;
    let mut changed = envelope.clone();
    changed.result.metrics.insert(
        "trajectory.fixture".into(),
        MetricValue::new(1, MetricUnit::Count),
    );
    changed
        .subject
        .requested_dimensions
        .insert("trajectory.fixture".into());
    let tx = store.begin().await?;
    ensure!(
        shared
            .apply_trajectory_in(&tx, OWNER, &changed)
            .await
            .is_err()
    );
    tx.rollback().await?;
    // A valid but different subject cannot reuse this owner's idempotency key.
    let mut different = fixture()?;
    different.result.idempotency_key = envelope.result.idempotency_key.clone();
    let tx = store.begin().await?;
    ensure!(
        shared
            .apply_trajectory_in(&tx, OWNER, &different)
            .await
            .is_err()
    );
    tx.rollback().await?;
    let tx = store.begin().await?;
    ensure!(
        shared
            .subject_in(&tx, OWNER, &different.subject.eval_id)
            .await?
            .is_none()
    );
    tx.commit().await?;
    let mut outside_scope = fixture()?;
    outside_scope.result.metrics.insert(
        "quality.custom".into(),
        MetricValue::new(1, MetricUnit::Count),
    );
    outside_scope
        .subject
        .requested_dimensions
        .insert("quality.custom".into());
    let tx = store.begin().await?;
    let error = shared
        .apply_trajectory_in(&tx, OWNER, &outside_scope)
        .await
        .err()
        .context("invalid metric admitted")?;
    ensure!(error.to_string().contains("metric outside its scope"));
    tx.rollback().await?;
    let mut holdout = fixture()?;
    holdout.subject.holdout = true;
    let tx = store.begin().await?;
    ensure!(
        shared
            .apply_trajectory_in(&tx, OWNER, &holdout)
            .await
            .is_err()
    );
    tx.rollback().await?;
    let mut large = fixture()?;
    large.result.idempotency_key = "x".repeat(1024 * 1024 + 1);
    let tx = store.begin().await?;
    let error = shared
        .apply_trajectory_in(&tx, OWNER, &large)
        .await
        .err()
        .context("oversized envelope admitted")?;
    ensure!(error.to_string().contains("envelope byte limit"));
    tx.rollback().await?;
    for sql in [
        "UPDATE gateway_shared_eval_subjects SET holdout=true WHERE namespace=$1 AND eval_id=$2",
        "UPDATE gateway_shared_eval_subjects SET subject_json=' ' || subject_json WHERE namespace=$1 AND eval_id=$2",
    ] {
        let tx = store.begin().await?;
        tx.execute(Statement::from_sql_and_values(
            DatabaseBackend::Postgres,
            sql,
            [
                store.namespace().into(),
                envelope.subject.eval_id.clone().into(),
            ],
        ))
        .await?;
        ensure!(
            shared
                .apply_trajectory_in(&tx, OWNER, envelope)
                .await
                .is_err()
        );
        tx.rollback().await?;
    }
    let tx = store.begin().await?;
    tx.execute(Statement::from_sql_and_values(DatabaseBackend::Postgres,
        "UPDATE gateway_shared_eval_results SET authority_id='wrong' WHERE namespace=$1 AND result_id=$2",
        [store.namespace().into(),id.clone().into()])).await?;
    ensure!(shared.result_in(&tx, OWNER, &id).await.is_err());
    tx.rollback().await?;
    let tx = store.begin().await?;
    tx.execute(Statement::from_sql_and_values(
        DatabaseBackend::Postgres,
        "DELETE FROM gateway_shared_eval_admission_events WHERE namespace=$1 AND result_id=$2",
        [store.namespace().into(), id.into()],
    ))
    .await?;
    ensure!(
        shared
            .apply_trajectory_in(&tx, OWNER, envelope)
            .await
            .is_err()
    );
    tx.rollback().await?;
    Ok(())
}

async fn boundary(first: &Store, second: &Store, shared: &SharedEvaluation) -> Result<()> {
    use crate::storage::evaluation::evidence_digest;
    let mut envelope = fixture()?;
    envelope
        .subject
        .evidence
        .first_mut()
        .context("missing fixture evidence")?
        .attributes
        .insert("padding".into(), String::new());
    envelope.subject.evidence_digest = evidence_digest(&envelope.subject.evidence)?;
    envelope.result.evidence_digest = envelope.subject.evidence_digest.clone();
    let remaining = (1024usize * 1024)
        .checked_sub(serde_json::to_vec(&envelope)?.len())
        .context("fixture already exceeds limit")?;
    envelope
        .subject
        .evidence
        .first_mut()
        .context("missing fixture evidence")?
        .attributes
        .insert("padding".into(), "x".repeat(remaining));
    envelope.subject.evidence_digest = evidence_digest(&envelope.subject.evidence)?;
    envelope.result.evidence_digest = envelope.subject.evidence_digest.clone();
    ensure!(serde_json::to_vec(&envelope)?.len() == 1024 * 1024);
    let tx = first.begin().await?;
    shared.apply_trajectory_in(&tx, OWNER, &envelope).await?;
    tx.commit().await?;
    let tx = second.begin().await?;
    ensure!(
        shared
            .apply_trajectory_in(&tx, OWNER, &envelope)
            .await?
            .duplicate
    );
    ensure!(
        shared
            .subject_in(&tx, OWNER, &envelope.subject.eval_id)
            .await?
            == Some(envelope.subject.clone())
    );
    tx.commit().await?;
    envelope
        .subject
        .evidence
        .first_mut()
        .context("missing fixture evidence")?
        .attributes
        .get_mut("padding")
        .context("missing padding")?
        .push('x');
    envelope.subject.evidence_digest = evidence_digest(&envelope.subject.evidence)?;
    envelope.result.evidence_digest = envelope.subject.evidence_digest.clone();
    ensure!(serde_json::to_vec(&envelope)?.len() == 1024 * 1024 + 1);
    let tx = first.begin().await?;
    let error = shared
        .apply_trajectory_in(&tx, OWNER, &envelope)
        .await
        .err()
        .context("oversized valid envelope accepted")?;
    ensure!(error.to_string().contains("envelope byte limit"));
    tx.rollback().await?;
    Ok(())
}
