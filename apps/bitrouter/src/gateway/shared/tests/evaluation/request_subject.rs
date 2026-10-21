//! Real PostgreSQL request-subject storage; no current configuration authority.

use std::time::Duration;

use crate::gateway::shared::{schema, store::Store, test_database::EmptyDatabase};
use crate::storage::evaluation::{
    canonical_digest, evidence_digest, shared::SharedEvaluation, validate_subject,
};
use anyhow::{Context, Result, ensure};
use bitrouter_ai::eval::{
    EVAL_SCHEMA_VERSION, EvalDecisionRef, EvalScope, EvalSubject, EvidenceItem,
};
use sea_orm::{ConnectionTrait, DatabaseBackend, DatabaseTransaction, IsolationLevel, Statement};

const MAX_SUBJECT_BYTES: usize = 1024 * 1024;

const OWNER: &str = "request-subject-owner";
const FROZEN_AT: &str = "2026-10-21T01:00:00Z";

#[tokio::test]
#[ignore = "requires BITROUTER_TEST_DATABASE_URL with PostgreSQL CREATEDB privilege"]
async fn postgres_shared_request_subject() -> Result<()> {
    let mut database = EmptyDatabase::create().await?;
    let result = tokio::time::timeout(Duration::from_secs(30), exercise(&mut database))
        .await
        .context("request subject regression deadline exceeded")
        .and_then(|result| result);
    // The timed future has been dropped; retained pools are closed before DROP.
    database.finish(result).await
}

async fn exercise(database: &mut EmptyDatabase) -> Result<()> {
    let db = database.connect(3, Duration::from_secs(3)).await?;
    let other_db = database.connect(3, Duration::from_secs(3)).await?;
    async {
        let namespace = format!("request-eval-{}", uuid::Uuid::new_v4());
        let first = Store::new(db.clone(), &namespace, Duration::from_secs(3))?;
        let second = Store::new(other_db.clone(), &namespace, Duration::from_secs(3))?;
        schema::initialize_all(&first).await?;
        let shared = SharedEvaluation::new(&namespace)?;
        let subject = fixture()?;
        rejection_and_rollback(&first, &second, &shared, &subject).await?;
        replay_and_namespaces(&first, &second, &shared, &subject).await?;
        boundary(&first, &second, &shared).await?;
        Ok::<(), anyhow::Error>(())
    }
    .await
}

fn fixture() -> Result<EvalSubject> {
    let request = format!("canonical-request-{}", uuid::Uuid::new_v4());
    let policy_digest = canonical_digest(&"frozen-original-policy")?;
    let attributes = std::collections::BTreeMap::from([("success".into(), "true".into())]);
    let evidence = vec![EvidenceItem {
        evidence_id: "request-outcome".into(),
        kind: "request.outcome".into(),
        digest: canonical_digest(&attributes)?,
        redacted: true,
        attributes,
    }];
    let subject = EvalSubject {
        schema_version: EVAL_SCHEMA_VERSION,
        eval_id: format!("request:{request}"),
        scope: EvalScope::Request,
        subject_id: request.clone(),
        policy_digest: policy_digest.clone(),
        preset: None,
        cohort: None,
        holdout: true,
        decisions: vec![EvalDecisionRef {
            decision_id: "frozen-decision".into(),
            policy: "original-policy".into(),
            route_projection: "original-route".into(),
            request_key: request,
            selected_tier: "standard".into(),
            selected_effort: None,
            baseline_tier: None,
            baseline_effort: None,
            policy_digest,
            experiment: None,
            route_measurement: None,
        }],
        requested_dimensions: ["cost.usd_micros".into(), "latency.ms".into()].into(),
        evidence_digest: evidence_digest(&evidence)?,
        evidence,
        observed_at: FROZEN_AT.into(),
    };
    validate_subject(&subject)?;
    Ok(subject)
}

async fn counts(tx: &DatabaseTransaction, namespace: &str) -> Result<(i64, i64, i64)> {
    let row = tx.query_one(Statement::from_sql_and_values(DatabaseBackend::Postgres,
        "SELECT (SELECT count(*) FROM gateway_shared_eval_subjects WHERE namespace=$1) AS subjects,
                (SELECT count(*) FROM gateway_shared_eval_results WHERE namespace=$1) AS results,
                (SELECT count(*) FROM gateway_shared_eval_admission_events WHERE namespace=$1) AS admissions",
        [namespace.into()])).await?.context("missing evaluation counts")?;
    Ok((
        row.try_get("", "subjects")?,
        row.try_get("", "results")?,
        row.try_get("", "admissions")?,
    ))
}

async fn stored(
    tx: &DatabaseTransaction,
    namespace: &str,
    id: &str,
) -> Result<(String, String, String)> {
    let row = tx
        .query_one(Statement::from_sql_and_values(
            DatabaseBackend::Postgres,
            "SELECT subject_json,content_digest,created_at FROM gateway_shared_eval_subjects
         WHERE namespace=$1 AND eval_id=$2",
            [namespace.into(), id.into()],
        ))
        .await?
        .context("missing request subject")?;
    Ok((
        row.try_get("", "subject_json")?,
        row.try_get("", "content_digest")?,
        row.try_get("", "created_at")?,
    ))
}

async fn rejection_and_rollback(
    first: &Store,
    second: &Store,
    shared: &SharedEvaluation,
    subject: &EvalSubject,
) -> Result<()> {
    let tx = first
        .begin_with_isolation(IsolationLevel::ReadCommitted)
        .await?;
    let error = shared
        .insert_request_subject_in(&tx, OWNER, subject)
        .await
        .err()
        .context("ReadCommitted request subject accepted")?;
    ensure!(error.to_string().contains("Serializable"));
    tx.rollback().await?;
    for scope in [EvalScope::Episode, EvalScope::Task] {
        let mut wrong_scope = subject.clone();
        wrong_scope.scope = scope;
        validate_subject(&wrong_scope)?;
        let tx = first.begin().await?;
        let error = shared
            .insert_request_subject_in(&tx, OWNER, &wrong_scope)
            .await
            .err()
            .context("non-request subject accepted")?;
        ensure!(error.to_string().contains("Request scope"));
        ensure!(counts(&tx, first.namespace()).await? == (0, 0, 0));
        tx.rollback().await?;
    }
    let tx = first.begin().await?;
    ensure!(
        shared
            .insert_request_subject_in(&tx, "", subject)
            .await
            .is_err()
    );
    let mut invalid_time = subject.clone();
    invalid_time.observed_at = "invalid-time".into();
    ensure!(
        shared
            .insert_request_subject_in(&tx, OWNER, &invalid_time)
            .await
            .is_err()
    );
    let mut unredacted = subject.clone();
    unredacted.evidence[0].redacted = false;
    unredacted.evidence_digest = evidence_digest(&unredacted.evidence)?;
    ensure!(
        shared
            .insert_request_subject_in(&tx, OWNER, &unredacted)
            .await
            .is_err()
    );
    ensure!(counts(&tx, first.namespace()).await? == (0, 0, 0));
    tx.rollback().await?;

    let tx = first.begin().await?;
    shared
        .insert_request_subject_in(&tx, OWNER, subject)
        .await?;
    ensure!(
        shared
            .subject_in(&tx, OWNER, &subject.eval_id)
            .await?
            .as_ref()
            == Some(subject)
    );
    ensure!(counts(&tx, first.namespace()).await? == (1, 0, 0));
    // The storage method must neither commit nor open an independent writer.
    let observer = second.begin().await?;
    ensure!(
        shared
            .subject_in(&observer, OWNER, &subject.eval_id)
            .await?
            .is_none()
    );
    observer.commit().await?;
    tx.rollback().await?;
    let tx = second.begin().await?;
    ensure!(
        shared
            .subject_in(&tx, OWNER, &subject.eval_id)
            .await?
            .is_none()
    );
    ensure!(counts(&tx, first.namespace()).await? == (0, 0, 0));
    tx.commit().await?;
    Ok(())
}

async fn insert(store: &Store, shared: &SharedEvaluation, subject: &EvalSubject) -> Result<()> {
    store
        .retry(|| async {
            let tx = store.begin().await?;
            shared
                .insert_request_subject_in(&tx, OWNER, subject)
                .await?;
            tx.commit().await?;
            Ok(())
        })
        .await
}

async fn replay_and_namespaces(
    first: &Store,
    second: &Store,
    shared: &SharedEvaluation,
    subject: &EvalSubject,
) -> Result<()> {
    // Two independent pools, with Serializable retry owned by the callers.
    let (a, b) = tokio::join!(
        insert(first, shared, subject),
        insert(second, shared, subject)
    );
    a?;
    b?;
    let tx = second.begin().await?;
    let original = stored(&tx, first.namespace(), &subject.eval_id).await?;
    shared
        .insert_request_subject_in(&tx, OWNER, subject)
        .await?;
    shared
        .insert_request_subject_in(&tx, OWNER, subject)
        .await?;
    ensure!(
        stored(&tx, first.namespace(), &subject.eval_id).await? == original,
        "exact replay rewrote immutable subject or creation timestamp"
    );
    ensure!(
        shared
            .subject_in(&tx, OWNER, &subject.eval_id)
            .await?
            .as_ref()
            == Some(subject)
    );
    // Holdout subject persists without pretending it has any result/admission.
    ensure!(subject.holdout && counts(&tx, first.namespace()).await? == (1, 0, 0));
    ensure!(
        shared
            .subject_in(&tx, "foreign-owner", &subject.eval_id)
            .await?
            .is_none()
    );
    tx.commit().await?;

    let tx = first.begin().await?;
    ensure!(
        shared
            .insert_request_subject_in(&tx, "foreign-owner", subject)
            .await
            .is_err()
    );
    tx.rollback().await?;
    let mut changed = subject.clone();
    changed.cohort = Some("different-valid-content".into());
    validate_subject(&changed)?;
    let tx = first.begin().await?;
    ensure!(
        shared
            .insert_request_subject_in(&tx, OWNER, &changed)
            .await
            .is_err()
    );
    tx.rollback().await?;

    // Same eval ID in a different namespace may carry different owned content.
    // Neither read nor exact-compare may accidentally use the original row.
    let namespace = format!("request-eval-{}", uuid::Uuid::new_v4());
    let foreign = SharedEvaluation::new(&namespace)?;
    let tx = first.begin().await?;
    ensure!(
        foreign
            .subject_in(&tx, OWNER, &subject.eval_id)
            .await?
            .is_none()
    );
    foreign
        .insert_request_subject_in(&tx, "foreign-owner", &changed)
        .await?;
    ensure!(
        foreign
            .subject_in(&tx, "foreign-owner", &subject.eval_id)
            .await?
            == Some(changed.clone())
    );
    ensure!(
        foreign
            .subject_in(&tx, OWNER, &subject.eval_id)
            .await?
            .is_none()
    );
    ensure!(
        shared
            .subject_in(&tx, OWNER, &subject.eval_id)
            .await?
            .as_ref()
            == Some(subject)
    );
    ensure!(counts(&tx, &namespace).await? == (1, 0, 0));
    tx.commit().await?;
    let tx = second.begin().await?;
    ensure!(
        foreign
            .insert_request_subject_in(&tx, OWNER, subject)
            .await
            .is_err()
    );
    tx.rollback().await?;

    // Identical input cannot heal corrupt storage. These mutations are test
    // faults, rolled back by the caller, not production repair interfaces.
    for sql in [
        "UPDATE gateway_shared_eval_subjects SET created_at='invalid-time' WHERE namespace=$1 AND eval_id=$2",
        "UPDATE gateway_shared_eval_subjects SET subject_json=' ' || subject_json WHERE namespace=$1 AND eval_id=$2",
        "UPDATE gateway_shared_eval_subjects SET holdout=false WHERE namespace=$1 AND eval_id=$2",
    ] {
        let tx = first.begin().await?;
        tx.execute(Statement::from_sql_and_values(
            DatabaseBackend::Postgres,
            sql,
            [first.namespace().into(), subject.eval_id.clone().into()],
        ))
        .await?;
        ensure!(
            shared
                .insert_request_subject_in(&tx, OWNER, subject)
                .await
                .is_err()
        );
        tx.rollback().await?;
    }
    let tx = second.begin().await?;
    ensure!(stored(&tx, first.namespace(), &subject.eval_id).await? == original);
    ensure!(counts(&tx, first.namespace()).await? == (1, 0, 0));
    tx.commit().await?;
    Ok(())
}

fn padding(subject: &mut EvalSubject, padding: String) -> Result<()> {
    let evidence = subject
        .evidence
        .first_mut()
        .context("missing request evidence")?;
    evidence.attributes.insert("padding".into(), padding);
    evidence.digest = canonical_digest(&evidence.attributes)?;
    subject.evidence_digest = evidence_digest(&subject.evidence)?;
    validate_subject(subject)
}

async fn boundary(first: &Store, second: &Store, shared: &SharedEvaluation) -> Result<()> {
    let mut subject = fixture()?;
    padding(&mut subject, String::new())?;
    let remaining = MAX_SUBJECT_BYTES
        .checked_sub(serde_json::to_vec(&subject)?.len())
        .context("request subject fixture already exceeds limit")?;
    padding(&mut subject, "x".repeat(remaining))?;
    ensure!(serde_json::to_vec(&subject)?.len() == MAX_SUBJECT_BYTES);
    insert(first, shared, &subject).await?;
    let tx = second.begin().await?;
    shared
        .insert_request_subject_in(&tx, OWNER, &subject)
        .await?;
    ensure!(shared.subject_in(&tx, OWNER, &subject.eval_id).await? == Some(subject.clone()));
    ensure!(counts(&tx, first.namespace()).await? == (2, 0, 0));
    let original = stored(&tx, first.namespace(), &subject.eval_id).await?;
    tx.commit().await?;
    padding(&mut subject, "x".repeat(remaining + 1))?;
    ensure!(serde_json::to_vec(&subject)?.len() == MAX_SUBJECT_BYTES + 1);
    let tx = first.begin().await?;
    let error = shared
        .insert_request_subject_in(&tx, OWNER, &subject)
        .await
        .err()
        .context("subject above serialized byte limit accepted")?;
    ensure!(error.to_string().contains("subject byte limit"));
    tx.rollback().await?;
    // Raw attribute bytes are small; JSON escaping alone crosses the limit.
    let mut escaped = fixture()?;
    padding(&mut escaped, "\0".repeat(MAX_SUBJECT_BYTES / 6 + 1))?;
    ensure!(serde_json::to_vec(&escaped)?.len() > MAX_SUBJECT_BYTES);
    let tx = first.begin().await?;
    let error = shared
        .insert_request_subject_in(&tx, OWNER, &escaped)
        .await
        .err()
        .context("escaped oversized subject accepted")?;
    ensure!(error.to_string().contains("subject byte limit"));
    tx.rollback().await?;
    let tx = second.begin().await?;
    ensure!(stored(&tx, first.namespace(), &subject.eval_id).await? == original);
    ensure!(
        shared
            .subject_in(&tx, OWNER, &escaped.eval_id)
            .await?
            .is_none()
    );
    ensure!(counts(&tx, first.namespace()).await? == (2, 0, 0));
    tx.commit().await?;
    Ok(())
}
