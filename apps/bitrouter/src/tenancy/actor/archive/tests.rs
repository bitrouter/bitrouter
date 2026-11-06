use super::*;
use crate::tenancy::{
    Service,
    receipts::store::{self, Candidate, Disposition},
};
use anyhow::{Context, Result, ensure};
use bitrouter_ai::tenancy::{
    configuration::Limits,
    execution::{ExecutionReceipt, Outcome, Provenance, ReceiptState, Usage},
    operation::DispatchState,
};
use sea_orm::{DatabaseConnection, TransactionTrait};

mod faults;

fn failure(error: Failure) -> anyhow::Error {
    anyhow::anyhow!("tenant failure: {:?}", error.0.code)
}

async fn seeded() -> Result<(DatabaseConnection, Candidate)> {
    let db = crate::db::connect("sqlite::memory:").await?;
    crate::db::run_migrations(&db).await?;
    let header = crate::tenancy::store::initial_header(&db, chrono::Utc::now()).await?;
    let candidate = Candidate::new(
        ExecutionReceipt {
            installation_id: header.installation,
            tenant_id: "tenant".try_into()?,
            principal_id: "principal".try_into()?,
            execution_id: "execution".try_into()?,
            correlation_id: "correlation".try_into()?,
            execution_revision: "revision".try_into()?,
            state: ReceiptState::Incomplete,
            outcome: Outcome::Failed,
            attempts: Vec::new(),
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
        },
        Disposition {
            dispatch: DispatchState::NotStarted,
            statuses: Vec::new(),
            auxiliary_digest: None,
        },
    )?;
    let tx = db.begin().await?;
    ensure!(store::insert(&tx, &candidate).await?);
    tx.commit().await?;
    Ok((db, candidate))
}

#[tokio::test]
async fn archive_commits_once_preserves_canonical_read_and_restart_inventory() -> Result<()> {
    let (db, candidate) = seeded().await?;
    let installation = &candidate.receipt.installation_id;
    let before = store::inventory(&db, installation).await?;
    ensure!(before.live_bytes > 0 && before.history_rows == 0);
    let service = Service::open(db.clone(), None).await?;
    ensure!(service.archive_history_for_test().await.map_err(failure)?);
    ensure!(!service.archive_history_for_test().await.map_err(failure)?);
    let after = store::inventory(&db, installation).await?;
    ensure!(after.live_bytes == 0 && after.history_rows == 1 && after.history_bytes > 0);
    ensure!(after.rows == before.rows && after.bytes == before.bytes);
    let receipt = &candidate.receipt;
    let loaded =
        store::lookup_candidate(&db, installation, &receipt.tenant_id, &receipt.execution_id)
            .await?
            .context("archived receipt missing")?;
    ensure!(
        serde_json::to_string(&loaded.receipt)? == serde_json::to_string(&candidate.receipt)?
            && loaded.digest == candidate.digest
    );
    service.shutdown().await?;
    let restarted = Service::open(db.clone(), None).await?;
    ensure!(
        !restarted
            .archive_history_for_test()
            .await
            .map_err(failure)?
    );
    restarted.shutdown().await?;
    db.close().await?;
    Ok(())
}

#[tokio::test]
async fn live_owner_blocks_relocation_until_actual_release() -> Result<()> {
    let (db, candidate) = seeded().await?;
    let service = Service::open(db.clone(), None).await?;
    let receipt = &candidate.receipt;
    let lease = service
        .admission
        .acquire(
            &receipt.tenant_id,
            &Limits {
                active_executions: 1,
                queued_work: 0,
                learning_jobs: 0,
            },
        )
        .await
        .map_err(|code| anyhow::anyhow!("{code:?}"))?;
    lease
        .bind_execution(&receipt.execution_id)
        .map_err(|code| anyhow::anyhow!("{code:?}"))?;
    ensure!(!service.archive_history_for_test().await.map_err(failure)?);
    ensure!(
        store::inventory(&db, &receipt.installation_id)
            .await?
            .history_rows
            == 0
    );
    drop(lease);
    ensure!(service.archive_history_for_test().await.map_err(failure)?);
    service.shutdown().await?;
    db.close().await?;
    Ok(())
}

#[tokio::test]
async fn insufficient_physical_space_defers_without_fencing_or_mutating_history() -> Result<()> {
    let (db, candidate) = seeded().await?;
    let service = Service::open(db.clone(), None).await?;
    service.admission.receipt_probe(0);
    let error = service
        .archive_history_for_test()
        .await
        .err()
        .context("expected refusal")?;
    ensure!(error.0.code == ErrorCode::CapacityExceeded);
    ensure!(service.admission.receipts_ready());
    ensure!(
        store::inventory(&db, &candidate.receipt.installation_id)
            .await?
            .history_rows
            == 0
    );
    service.admission.receipt_probe(u64::MAX);
    ensure!(service.archive_history_for_test().await.map_err(failure)?);
    service.shutdown().await?;
    db.close().await?;
    Ok(())
}
