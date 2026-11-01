use super::tests::{result, subject};
use super::*;
use bitrouter_ai::eval::EvalVerdict;
use std::sync::{Arc, OnceLock};

async fn fixture() -> Result<(
    EvalStore,
    crate::tenancy::Service,
    Arc<OnceLock<crate::tenancy::storage::Budget>>,
)> {
    let db = crate::db::connect("sqlite::memory:").await?;
    crate::db::run_migrations(&db).await?;
    let tenancy = crate::tenancy::Service::open(db.clone(), None).await?;
    let cell = Arc::new(OnceLock::new());
    Ok((
        EvalStore::new(db).with_storage_budget(Some(cell.clone())),
        tenancy,
        cell,
    ))
}

#[tokio::test]
async fn legacy_eval_writers_wait_for_budget_and_preserve_exchange_replay() -> Result<()> {
    let (store, tenancy, cell) = fixture().await?;
    let subject = subject()?;
    assert!(store.insert_subject(&subject).await.is_err());
    assert!(
        store
            .insert_settlement_subject_owned(&subject, "local")
            .await
            .is_err()
    );
    assert!(store.subject(&subject.eval_id).await?.is_none());
    cell.set(tenancy.storage_budget())
        .map_err(|_| anyhow::anyhow!("cell already set"))?;
    store
        .insert_settlement_subject_owned(&subject, "local")
        .await?;
    store.insert_subject_owned(&subject, "local").await?;
    let result = result(EvalVerdict::Pass);
    let saved = store.insert_result_owned(&result, "local").await?;
    assert_eq!(
        store.insert_result(&result).await?.result_id(),
        saved.result_id()
    );
    store
        .append_admission_event(
            saved.result_id(),
            AdmissionStatus::Admitted,
            "accepted",
            "local",
        )
        .await?;
    let snapshot = store.freeze_snapshot("2026-07-30T00:02:00Z").await?;
    assert!(!snapshot.entries.is_empty());
    assert!(
        store
            .snapshot_by_root(&snapshot.evidence_root)
            .await?
            .is_some()
    );
    let mut conflict = subject.clone();
    conflict.subject_id = "different".into();
    assert!(store.insert_subject(&conflict).await.is_err());
    assert!(tenancy.storage_budget().reserve(0, false).is_ok());
    tenancy.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn delegated_legacy_eval_sql_failure_fences_without_publishing_subject() -> Result<()> {
    let (store, tenancy, cell) = fixture().await?;
    cell.set(tenancy.storage_budget())
        .map_err(|_| anyhow::anyhow!("cell already set"))?;
    store.db.execute_unprepared("CREATE TRIGGER reject_eval_subject BEFORE INSERT ON eval_subjects BEGIN SELECT RAISE(ABORT, 'fixture storage failure'); END").await?;
    let subject = subject()?;
    let error = store
        .insert_settlement_subject_owned(&subject, "local")
        .await
        .err()
        .context("SQL failure accepted")?;
    assert!(error.to_string().contains("local storage SQL failed"));
    assert!(store.subject(&subject.eval_id).await?.is_none());
    assert!(matches!(
        tenancy.storage_budget().reserve(0, false),
        Err(ErrorCode::StorageUnavailable)
    ));
    tenancy.shutdown().await?;
    Ok(())
}
