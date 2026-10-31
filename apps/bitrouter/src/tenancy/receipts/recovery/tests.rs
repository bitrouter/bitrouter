//! Seed serialization compatibility and immutable loss evidence at recovery.
use super::*;
use crate::tenancy::receipts::{read, store, usage_evidence::storage};
use anyhow::Context;
use bitrouter_ai::tenancy::{
    operation::ErrorCode,
    usage_evidence::{Capture, ExecutionUsageEvidenceV1},
};
use sea_orm::{ConnectionTrait, DatabaseBackend, Statement, TransactionTrait};
use serde_json::json;

fn seed() -> Result<Seed> {
    // Data-only fixture: this creates no admission or recovery authority.
    let identity = super::super::owner::Identity {
        installation: "installation".try_into()?,
        tenant: "tenant".try_into()?,
        principal: "principal".try_into()?,
        execution: "execution".try_into()?,
        correlation: "correlation".try_into()?,
        revision: "revision".try_into()?,
    };
    let origin = serde_json::from_value(json!({
        "acquisition":"b7a8854e-80da-46ef-b805-0dc03b061c57", "instance":"instance"
    }))?;
    Ok(Seed::from_owner(&identity, origin))
}

#[test]
fn new_seed_retains_capture_contract_and_legacy_seed_bytes_do_not_change() -> Result<()> {
    let seed = seed()?;
    let at = "2026-10-25T00:00:00Z".try_into()?;
    let current = serde_json::to_value(&seed)?;
    ensure!(current["usage_evidence_version"] == 1);
    let restored: Seed = serde_json::from_value(current.clone())?;
    let candidate = restored.unknown(&at)?;
    let evidence = candidate
        .usage_evidence
        .as_ref()
        .context("unavailable sidecar")?;
    ensure!(evidence.document.capture == Capture::Unavailable);
    ensure!(evidence.document.frames.is_empty() && evidence.document.attempts.is_empty());
    ensure!(evidence.document.estimate.is_none() && candidate.receipt.usage.input_tokens.is_none());

    let mut legacy = current.clone();
    legacy
        .as_object_mut()
        .context("seed")?
        .remove("usage_evidence_version");
    let old: Seed = serde_json::from_value(legacy.clone())?;
    ensure!(serde_json::to_value(&old)? == legacy);
    let old_bytes = serde_json::to_vec(&old)?;
    ensure!(serde_json::to_vec(&serde_json::from_slice::<Seed>(&old_bytes)?)? == old_bytes);
    let old_candidate = old.unknown(&at)?;
    ensure!(old_candidate.usage_evidence.is_none());
    ensure!(candidate.digest != old_candidate.digest);
    ensure!(
        serde_json::to_vec(&candidate.receipt)? == serde_json::to_vec(&old_candidate.receipt)?,
        "capture loss must not widen or alter the frozen receipt"
    );
    let mut future = current;
    future["usage_evidence_version"] = json!(2);
    ensure!(
        serde_json::from_value::<Seed>(future)?
            .unknown(&at)
            .is_err()
    );
    Ok(())
}

#[tokio::test]
async fn recovered_loss_sidecar_is_atomic_survives_restart_and_cannot_disappear() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let url = format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("recovery.db").display()
    );
    let db = crate::db::connect(&url).await?;
    crate::db::run_migrations(&db).await?;
    let seed = seed()?;
    let candidate = seed.unknown(&"2026-10-25T00:00:00Z".try_into()?)?;
    let mut legacy_seed = seed.clone();
    legacy_seed.execution = "legacy-execution".try_into()?;
    legacy_seed.usage_evidence_version = None;
    let legacy = legacy_seed.unknown(&candidate.receipt.recorded_at)?;
    db.execute(Statement::from_string(DatabaseBackend::Sqlite,
        "CREATE TRIGGER evidence_failure BEFORE INSERT ON tenancy_usage_evidence BEGIN SELECT RAISE(ABORT,'injected storage failure'); END".to_owned())).await?;
    let tx = db.begin().await?;
    ensure!(store::insert(&tx, &candidate).await.is_err());
    tx.rollback().await?;
    ensure!(
        store::lookup_candidate(&db, &seed.installation, &seed.tenant, &seed.execution)
            .await?
            .is_none()
    );
    db.execute(Statement::from_string(
        DatabaseBackend::Sqlite,
        "DROP TRIGGER evidence_failure".to_owned(),
    ))
    .await?;
    let tx = db.begin().await?;
    ensure!(store::insert(&tx, &candidate).await?);
    ensure!(store::insert(&tx, &legacy).await?);
    tx.commit().await?;
    db.close().await?;

    let db = crate::db::connect(&url).await?;
    let saved = store::lookup_candidate(&db, &seed.installation, &seed.tenant, &seed.execution)
        .await?
        .context("recovered receipt")?;
    ensure!(saved.digest == candidate.digest && saved.auxiliary_bytes() > 0);
    ensure!(storage::inventory(&db, &seed.installation).await? == saved.auxiliary_bytes());
    let request = read::Request::UsageEvidence(seed.execution.clone());
    let body = read::body(&db, &seed.installation, &seed.tenant, &request, || {
        crate::tenancy::error::Failure::from(ErrorCode::AuthorityUnavailable)
    })
    .await
    .map_err(|_| anyhow::anyhow!("recovered evidence read failed"))?;
    let evidence: ExecutionUsageEvidenceV1 = serde_json::from_slice(&body)?;
    ensure!(evidence.capture == Capture::Unavailable && evidence.frames.is_empty());
    let legacy_body = read::body(
        &db,
        &seed.installation,
        &seed.tenant,
        &read::Request::UsageEvidence(legacy_seed.execution),
        || crate::tenancy::error::Failure::from(ErrorCode::AuthorityUnavailable),
    )
    .await
    .map_err(|_| anyhow::anyhow!("historical evidence read failed"))?;
    ensure!(
        serde_json::from_slice::<ExecutionUsageEvidenceV1>(&legacy_body)?.capture
            == Capture::NotCaptured
    );
    db.execute(Statement::from_string(
        DatabaseBackend::Sqlite,
        "DELETE FROM tenancy_usage_evidence".to_owned(),
    ))
    .await?;
    ensure!(storage::inventory(&db, &seed.installation).await.is_err());
    ensure!(
        read::body(&db, &seed.installation, &seed.tenant, &request, || {
            crate::tenancy::error::Failure::from(ErrorCode::AuthorityUnavailable)
        })
        .await
        .is_err(),
        "lost sidecar must not become historical not_captured"
    );
    db.close().await?;
    Ok(())
}
