use super::super::tests::{candidate, commit};
use super::*;
use crate::tenancy::receipts::{
    auxiliary, auxiliary_store,
    usage_evidence::{Snapshot, storage},
};
use sea_orm::{DatabaseConnection, TransactionTrait};

async fn open() -> Result<DatabaseConnection> {
    let db = crate::db::connect("sqlite::memory:").await?;
    crate::db::run_migrations(&db).await?;
    Ok(db)
}

async fn archive(db: &DatabaseConnection, value: &Candidate) -> Result<()> {
    let prepared = super::super::archive::Prepared::new(value, &|_, _| Ok(()))?;
    let tx = db.begin().await?;
    ensure!(super::super::archive::archive_one(&tx, &prepared, &|_, _| Ok(())).await?);
    tx.commit().await?;
    Ok(())
}

#[tokio::test]
async fn complete_mixed_history_scan_preserves_unknowns_and_detects_late_digest_corruption()
-> Result<()> {
    let db = open().await?;
    let mut expected_bytes = 0_u64;
    // More than two key pages; checkpoints are not product capacity limits.
    for index in 0..137 {
        let value = candidate(
            "installation",
            "tenant",
            &format!("execution-{index:04}"),
            "creator",
        )?
        .with_usage_evidence(Snapshot::default())?;
        commit(&db, &value).await?;
        expected_bytes += value.bytes() + value.auxiliary_bytes();
        if index % 2 == 0 {
            archive(&db, &value).await?;
        }
    }
    let mut receipt = candidate("installation", "tenant", "unknown", "creator")?.receipt;
    receipt.state = ReceiptState::Unknown;
    receipt.outcome = bitrouter_ai::tenancy::execution::Outcome::Unknown;
    let unknown = Candidate::new(
        receipt,
        Disposition {
            dispatch: DispatchState::Unknown,
            statuses: vec![],
            auxiliary_digest: None,
        },
    )?;
    commit(&db, &unknown).await?;
    expected_bytes += unknown.bytes();
    let mut visited = 0usize;
    scan(&db, &unknown.receipt.installation_id, |_| {
        visited += 1;
        Ok(())
    })
    .await?;
    ensure!(visited == 138);
    let inventory = load(&db, &unknown.receipt.installation_id).await?;
    ensure!(inventory.rows == 138 && inventory.bytes == expected_bytes);
    ensure!(inventory.unknown_count == 1);
    db.execute_unprepared("UPDATE tenancy_execution_receipts SET content_digest=printf('%064d',0) WHERE execution_id='execution-0136'").await?;
    ensure!(load(&db, &unknown.receipt.installation_id).await.is_err());
    db.close().await?;
    Ok(())
}

#[tokio::test]
async fn archive_usage_reader_replay_and_offline_body_preserve_bytes_without_restoring_inline()
-> Result<()> {
    let db = open().await?;
    let mut value = candidate("installation", "tenant", "execution", "creator")?
        .with_usage_evidence(Snapshot::default())?;
    commit(&db, &value).await?;
    archive(&db, &value).await?;
    let original = value.usage_evidence.as_ref().context("original usage")?;
    let expected = original.digest.clone();
    let bytes = original.json.as_bytes().to_vec();
    storage::validate(&db, &mut value, Some(&expected)).await?;
    ensure!(
        value
            .usage_evidence
            .as_ref()
            .context("archived usage")?
            .json
            .as_bytes()
            == bytes
    );
    let tx = db.begin().await?;
    storage::insert(&tx, &value, false).await?;
    tx.commit().await?;
    let receipt = &value.receipt;
    let body = crate::tenancy::receipts::read::body(
        &db,
        &receipt.installation_id,
        &receipt.tenant_id,
        &crate::tenancy::receipts::read::Request::UsageEvidence(receipt.execution_id.clone()),
        || {
            crate::tenancy::error::Failure::from(
                bitrouter_ai::tenancy::operation::ErrorCode::StorageUnavailable,
            )
        },
    )
    .await
    .map_err(|_| anyhow::anyhow!("archive usage body failed"))?;
    ensure!(body == bytes);
    let row = db
        .query_one(Statement::from_string(
            DatabaseBackend::Sqlite,
            "SELECT storage_kind,length(evidence_json) AS inline_bytes FROM tenancy_usage_evidence",
        ))
        .await?
        .context("usage marker")?;
    ensure!(
        row.try_get::<i64>("", "storage_kind")? == 1
            && row.try_get::<i64>("", "inline_bytes")? == 0
    );
    ensure!(load(&db, &receipt.installation_id).await?.unknown_count == 0);
    db.close().await?;
    Ok(())
}

#[tokio::test]
async fn startup_reverse_coverage_rejects_orphan_archive_and_sidecars() -> Result<()> {
    for mutation in [
        "DELETE FROM tenancy_execution_receipts",
        "UPDATE tenancy_receipt_archive SET execution_id='orphan'",
        "UPDATE tenancy_usage_evidence SET execution_id='orphan'",
        "UPDATE tenancy_usage_evidence SET storage_kind=0",
        "UPDATE tenancy_execution_receipts SET receipt_json='{}'",
    ] {
        let db = open().await?;
        let value = candidate("installation", "tenant", "execution", "creator")?
            .with_usage_evidence(Snapshot::default())?;
        commit(&db, &value).await?;
        archive(&db, &value).await?;
        db.execute_unprepared(mutation).await?;
        ensure!(
            load(&db, &value.receipt.installation_id).await.is_err(),
            "accepted {mutation}"
        );
        db.close().await?;
    }
    Ok(())
}

#[tokio::test]
async fn archived_known_rejection_discharge_requires_exact_oauth_work_and_generation() -> Result<()>
{
    use crate::tenancy::oauth::{
        profile::Profile,
        storage::{Metadata, State, Work, slot},
    };
    use bitrouter_ai::tenancy::{
        authority::{OwnerKind, ResourceKind},
        configuration::ResourceRef,
    };
    let db = open().await?;
    let value = candidate("installation", "tenant", "execution", "creator")?;
    let reference = ResourceRef {
        owner_kind: OwnerKind::Tenant,
        owner_id: "tenant".try_into()?,
        resource_kind: ResourceKind::Credential,
        resource_id: "slot".try_into()?,
        revision_id: "r1".try_into()?,
    };
    let work = Work {
        id: uuid::Uuid::new_v4().to_string(),
        execution: value.receipt.execution_id.clone(),
        tenant: value.receipt.tenant_id.clone(),
        principal: value.receipt.principal_id.clone(),
        expected_generation: 1,
        started_at: "2026-10-25T00:00:00Z".try_into()?,
    };
    let registration =
        crate::gateway::providers::login::find("anthropic").context("registration")?;
    let metadata = Metadata {
        format: 1,
        installation: value.receipt.installation_id.clone(),
        slot: slot(&value.receipt.installation_id, &reference)?,
        reference: reference.clone(),
        profile: Profile {
            format: 1,
            mechanism: "claude_code_refresh_v1".into(),
            provider: "claude-code".into(),
            account: "default".into(),
            registration: "anthropic".into(),
            token_endpoint: registration.auth.token_endpoint,
            client_id: registration.auth.client_id,
        },
        seed: "a".repeat(64),
        generation: 1,
        state: State::Quarantined(work.clone()),
    };
    metadata.validate()?;
    let value = value.with_auxiliary(vec![auxiliary::Fact {
        credential: reference,
        slot: metadata.slot.clone(),
        generation: 1,
        work: work.id.clone(),
        request_digest: Some("b".repeat(64)),
        http_status: Some(403),
        outcome: auxiliary::Outcome::Rejected,
    }])?;
    let tx = db.begin().await?;
    super::super::insert(&tx, &value).await?;
    auxiliary_store::insert(&tx, &value).await?;
    tx.commit().await?;
    archive(&db, &value).await?;
    ensure!(auxiliary_store::pending_receipted(&db, &metadata, &work).await?);
    for changed in [
        Work {
            id: uuid::Uuid::new_v4().to_string(),
            ..work.clone()
        },
        Work {
            principal: "other".try_into()?,
            ..work.clone()
        },
        Work {
            expected_generation: 2,
            ..work.clone()
        },
    ] {
        ensure!(
            auxiliary_store::pending_receipted(&db, &metadata, &changed)
                .await
                .is_err()
        );
    }
    let missing = Work {
        execution: "missing".try_into()?,
        ..work
    };
    ensure!(!auxiliary_store::pending_receipted(&db, &metadata, &missing).await?);
    ensure!(
        load(&db, &value.receipt.installation_id)
            .await?
            .unknown_count
            == 0
    );
    db.close().await?;
    Ok(())
}
