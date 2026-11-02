use super::super::store::{self, Candidate, Disposition};
use super::*;
use anyhow::{Context, Result, ensure};
use bitrouter_ai::tenancy::{
    execution::{ExecutionReceipt, Outcome, Provenance, ReceiptState, Usage},
    operation::DispatchState,
    validation::Validate,
};
use sea_orm::{ConnectionTrait, DatabaseBackend, DatabaseConnection, Statement, TransactionTrait};
use serde_json::json;

fn receipt() -> Result<ExecutionReceipt> {
    Ok(ExecutionReceipt {
        installation_id: "installation".try_into()?,
        tenant_id: "tenant".try_into()?,
        principal_id: "principal".try_into()?,
        execution_id: "execution".try_into()?,
        correlation_id: "correlation".try_into()?,
        execution_revision: "revision".try_into()?,
        state: ReceiptState::Unknown,
        outcome: Outcome::Unknown,
        attempts: vec![],
        usage: Usage {
            input_tokens: None,
            output_tokens: None,
            cache_read_tokens: None,
            cache_write_tokens: None,
            reasoning_tokens: None,
            provenance: Provenance::Unknown,
            complete: false,
        },
        recorded_at: "2026-10-25T00:00:00Z".try_into()?,
    })
}
fn candidate() -> Result<Candidate> {
    Candidate::new(
        receipt()?,
        Disposition {
            dispatch: DispatchState::NotStarted,
            statuses: vec![],
            auxiliary_digest: None,
        },
    )?
    .with_usage_evidence(Snapshot::default())
}
async fn open() -> Result<DatabaseConnection> {
    let db = crate::db::connect("sqlite::memory:").await?;
    crate::db::run_migrations(&db).await?;
    Ok(db)
}
async fn write(db: &DatabaseConnection, candidate: &Candidate) -> Result<bool> {
    let tx = db.begin().await?;
    match store::insert(&tx, candidate).await {
        Ok(inserted) => {
            tx.commit().await?;
            Ok(inserted)
        }
        Err(error) => {
            tx.rollback().await?;
            Err(error)
        }
    }
}
async fn sql(db: &DatabaseConnection, sql: &str) -> Result<()> {
    db.execute(Statement::from_string(
        DatabaseBackend::Sqlite,
        sql.to_owned(),
    ))
    .await?;
    Ok(())
}
async fn load(db: &DatabaseConnection) -> Result<Candidate> {
    let r = receipt()?;
    store::lookup_candidate(db, &r.installation_id, &r.tenant_id, &r.execution_id)
        .await?
        .context("receipt missing")
}
#[test]
fn retains_original_zero_absence_invalid_and_media_search_without_raw_content() -> Result<()> {
    let mut capture = Snapshot::default();
    let id = "attempt".try_into()?;
    capture.observe(&id,Protocol::Messages,Source::MessageStart,&json!({"input_tokens":0,"server_tool_use":{"web_search_requests":2},"secret":"canary-never-retained"}));
    capture.observe(
        &id,
        Protocol::Messages,
        Source::MessageDelta,
        &json!({"output_tokens":4}),
    );
    capture.observe(
        &id,
        Protocol::Messages,
        Source::MessageDelta,
        &json!({"output_tokens":3}),
    );
    capture.observe(&id,Protocol::ChatCompletions,Source::ChatChunk,&json!({"prompt_tokens_details":{"audio_tokens":0,"image_tokens":8},"completion_tokens":-1}));
    ensure!(capture.frames.len() == 4 && capture.frames[0].unsupported);
    ensure!(
        capture.frames[0]
            .quantities
            .iter()
            .any(|q| q.dimension == Dimension::InputTokens && q.value == Some(0))
    );
    ensure!(
        !capture.frames[0]
            .quantities
            .iter()
            .any(|q| q.dimension == Dimension::OutputTokens)
    );
    ensure!(
        capture.frames[0]
            .quantities
            .iter()
            .any(|q| q.dimension == Dimension::WebSearchRequests && q.value == Some(2))
    );
    ensure!(
        capture.frames[1].quantities[0].value == Some(4)
            && capture.frames[2].quantities[0].value == Some(3)
    );
    ensure!(
        capture.frames[3].invalid
            && capture.frames[3]
                .quantities
                .iter()
                .any(|q| q.dimension == Dimension::OutputTokens && q.value.is_none())
    );
    ensure!(
        capture
            .frames
            .iter()
            .enumerate()
            .all(|(i, f)| f.sequence == i as u64 && f.semantics == Semantics::Snapshot)
    );
    ensure!(!serde_json::to_string(&capture.frames)?.contains("canary"));
    Ok(())
}
#[test]
fn bounds_are_sticky_and_keep_a_prefix_across_attempts() -> Result<()> {
    let mut capture = Snapshot::default();
    let id = "attempt".try_into()?;
    for _ in 0..(MAX_ATTEMPT_FRAMES + 7) {
        capture.observe(
            &id,
            Protocol::Messages,
            Source::MessageDelta,
            &json!({"output_tokens":1}),
        );
    }
    capture.observe(
        &"other".try_into()?,
        Protocol::Messages,
        Source::MessageDelta,
        &json!({"output_tokens":2}),
    );
    ensure!(capture.frames.len() == MAX_ATTEMPT_FRAMES && capture.dropped == 8);
    let mut global = Snapshot::default();
    for i in 0..8 {
        for _ in 0..100 {
            global.observe(
                &format!("a{i}").try_into()?,
                Protocol::Responses,
                Source::ResponseTerminal,
                &json!({"output_tokens":0}),
            );
        }
    }
    ensure!(global.frames.len() == MAX_FRAMES && global.dropped == 288);
    Ok(())
}
#[tokio::test]
async fn atomic_failure_replay_deletion_and_inventory_corruption() -> Result<()> {
    let db = open().await?;
    let candidate = candidate()?;
    sql(&db,"CREATE TRIGGER evidence_failure BEFORE INSERT ON tenancy_usage_evidence BEGIN SELECT RAISE(ABORT,'injected storage failure'); END").await?;
    ensure!(write(&db, &candidate).await.is_err());
    let count = db
        .query_one(Statement::from_string(
            DatabaseBackend::Sqlite,
            "SELECT COUNT(*) AS count FROM tenancy_execution_receipts".to_owned(),
        ))
        .await?
        .context("receipt count")?;
    ensure!(
        count.try_get::<i64>("", "count")? == 0,
        "sidecar failure must roll back the receipt insert"
    );

    ensure!(load(&db).await.is_err());
    sql(&db, "DROP TRIGGER evidence_failure").await?;
    ensure!(write(&db, &candidate).await? && !write(&db, &candidate).await?);
    let saved = load(&db).await?;
    ensure!(saved.digest == candidate.digest);
    let evidence = saved.usage_evidence.context("evidence")?;
    evidence.document.validate()?;
    ensure!(
        storage::inventory(&db, &receipt()?.installation_id).await?
            == evidence.json.len() as u64 + 16 * 1024
    );
    sql(&db, "UPDATE tenancy_usage_evidence SET byte_count=1").await?;
    ensure!(
        load(&db).await.is_err()
            && storage::inventory(&db, &receipt()?.installation_id)
                .await
                .is_err()
    );
    sql(
        &db,
        "UPDATE tenancy_usage_evidence SET byte_count=length(CAST(evidence_json AS BLOB))",
    )
    .await?;
    sql(&db, "DELETE FROM tenancy_usage_evidence").await?;
    ensure!(
        load(&db).await.is_err()
            && storage::inventory(&db, &receipt()?.installation_id)
                .await
                .is_err()
    );
    ensure!(
        write(&db, &candidate).await.is_err(),
        "replay must not repair missing evidence"
    );
    db.close().await?;
    Ok(())
}
#[tokio::test]
async fn historical_absence_differs_from_empty_capture_and_corrupt_orphan_is_refused() -> Result<()>
{
    let db = open().await?;
    let r = receipt()?;
    let old = Candidate::new(
        r.clone(),
        Disposition {
            dispatch: DispatchState::NotStarted,
            statuses: vec![],
            auxiliary_digest: None,
        },
    )?;
    write(&db, &old).await?;
    ensure!(load(&db).await?.usage_evidence.is_none());
    ensure!(historical(&r).capture == Capture::NotCaptured);
    sql(&db,"INSERT INTO tenancy_usage_evidence VALUES('installation','tenant','orphan',1,'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa','aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa','{}',2)").await?;
    ensure!(storage::inventory(&db, &r.installation_id).await.is_err());
    db.close().await?;
    Ok(())
}
#[tokio::test]
async fn durable_evidence_survives_restart_and_digest_corruption_fails_closed() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let url = format!(
        "sqlite://{}?mode=rwc",
        dir.path().join("evidence.db").display()
    );
    let db = crate::db::connect(&url).await?;
    crate::db::run_migrations(&db).await?;
    let original = candidate()?;
    write(&db, &original).await?;
    db.close().await?;
    let db = crate::db::connect(&url).await?;
    ensure!(load(&db).await?.digest == original.digest);
    sql(&db,"UPDATE tenancy_usage_evidence SET evidence_json=replace(evidence_json,'captured','not_captured'),byte_count=byte_count+4").await?;
    ensure!(
        load(&db).await.is_err()
            && storage::inventory(&db, &receipt()?.installation_id)
                .await
                .is_err()
    );
    db.close().await?;
    Ok(())
}

#[test]
fn chat_null_chunks_do_not_exhaust_original_frame_capacity() -> Result<()> {
    let mut snapshot = Snapshot::default();
    let id = "attempt".try_into()?;
    for _ in 0..1024 {
        snapshot.observe(
            &id,
            Protocol::ChatCompletions,
            Source::ChatChunk,
            &serde_json::Value::Null,
        );
    }
    snapshot.observe(
        &id,
        Protocol::ChatCompletions,
        Source::ChatChunk,
        &json!({"prompt_tokens":0,"completion_tokens":8}),
    );
    ensure!(snapshot.frames.len() == 1 && snapshot.dropped == 0 && !snapshot.frames[0].invalid);
    Ok(())
}

#[tokio::test]
async fn missing_table_extra_index_and_oversized_cells_are_refused() -> Result<()> {
    let db = open().await?;
    let candidate = candidate()?;
    write(&db, &candidate).await?;
    sql(
        &db,
        "CREATE INDEX extra_usage_index ON tenancy_usage_evidence(tenant_id)",
    )
    .await?;
    ensure!(
        storage::inventory(&db, &receipt()?.installation_id)
            .await
            .is_err()
    );
    sql(&db, "DROP INDEX extra_usage_index").await?;
    sql(
        &db,
        "UPDATE tenancy_usage_evidence SET evidence_json=printf('%300000s','x'), byte_count=300000",
    )
    .await?;
    ensure!(
        load(&db).await.is_err()
            && storage::inventory(&db, &receipt()?.installation_id)
                .await
                .is_err()
    );
    sql(&db, "DROP TABLE tenancy_usage_evidence").await?;
    ensure!(
        storage::inventory(&db, &receipt()?.installation_id)
            .await
            .is_err()
    );
    db.close().await?;
    Ok(())
}

#[cfg(all(feature = "core", feature = "harness"))]
#[tokio::test]
async fn recovery_verifies_evidence_and_charges_the_shared_budget() -> Result<()> {
    let db = open().await?;
    let candidate = candidate()?;
    write(&db, &candidate).await?;
    let r = receipt()?;
    let budget = crate::tenancy::session::recovery::Budget::default();
    let recovered = store::recovery_lookup(
        &db,
        &r.installation_id,
        &r.tenant_id,
        &r.execution_id,
        &budget,
    )
    .await?
    .context("recovery receipt")?;
    ensure!(recovered.usage_evidence.is_some());
    let exhausted = crate::tenancy::session::recovery::Budget::default();
    exhausted.charge(0, crate::tenancy::session::recovery::MAX_BYTES)?;
    ensure!(
        store::recovery_lookup(
            &db,
            &r.installation_id,
            &r.tenant_id,
            &r.execution_id,
            &exhausted
        )
        .await
        .is_err()
    );
    sql(&db,"UPDATE tenancy_usage_evidence SET receipt_digest='aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa'").await?;
    ensure!(
        store::recovery_lookup(
            &db,
            &r.installation_id,
            &r.tenant_id,
            &r.execution_id,
            &crate::tenancy::session::recovery::Budget::default()
        )
        .await
        .is_err()
    );
    db.close().await?;
    Ok(())
}
