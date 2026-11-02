use super::*;
use bitrouter_ai::tenancy::execution::{Outcome, Provenance, Usage};
use sea_orm::{DatabaseConnection, TransactionTrait};

fn candidate(installation: &str, tenant: &str, id: &str, principal: &str) -> Result<Candidate> {
    Candidate::new(
        ExecutionReceipt {
            installation_id: installation.try_into()?,
            tenant_id: tenant.try_into()?,
            principal_id: principal.try_into()?,
            execution_id: id.try_into()?,
            correlation_id: "same-correlation".try_into()?,
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
    )
}
async fn open() -> Result<DatabaseConnection> {
    let db = crate::db::connect("sqlite::memory:").await?;
    crate::db::run_migrations(&db).await?;
    Ok(db)
}
async fn commit(db: &DatabaseConnection, candidate: &Candidate) -> Result<bool> {
    let tx = db.begin().await?;
    let result = insert(&tx, candidate).await;
    match result {
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
#[tokio::test]
async fn committed_exact_replay_is_immutable_and_every_identity_is_scoped() -> Result<()> {
    let db = open().await?;
    let initial = candidate("installation", "tenant", "A", "creator")?;
    ensure!(commit(&db, &initial).await?);
    ensure!(!commit(&db, &initial).await?);
    let tx = db.begin().await?;
    let loaded = lookup(
        &tx,
        &initial.receipt.installation_id,
        &initial.receipt.tenant_id,
        &initial.receipt.execution_id,
    )
    .await?
    .context("committed receipt")?;
    ensure!(serde_json::to_string(&loaded)? == initial.json);
    tx.rollback().await?;
    ensure!(
        commit(
            &db,
            &candidate("installation", "tenant", "A", "other-creator")?
        )
        .await
        .is_err()
    );
    for (installation, tenant) in [
        ("other-installation", "tenant"),
        ("installation", "other-tenant"),
    ] {
        ensure!(
            lookup(
                &db,
                &installation.try_into()?,
                &tenant.try_into()?,
                &"A".try_into()?
            )
            .await?
            .is_none()
        );
        ensure!(commit(&db, &candidate(installation, tenant, "A", "creator")?).await?);
    }
    let loaded = lookup(
        &db,
        &initial.receipt.installation_id,
        &initial.receipt.tenant_id,
        &initial.receipt.execution_id,
    )
    .await?
    .context("unchanged creator")?;
    ensure!(loaded.principal_id.as_str() == "creator");
    db.close().await?;
    Ok(())
}
#[tokio::test]
async fn correlation_multiplicity_is_query_wide_case_sensitive_and_cursor_bounded() -> Result<()> {
    let db = open().await?;
    for id in ["A", "B", "a"] {
        commit(&db, &candidate("installation", "tenant", id, "creator")?).await?;
    }
    commit(
        &db,
        &candidate("installation", "other-tenant", "AA", "other")?,
    )
    .await?;
    let tx = db.begin().await?;
    let installation = "installation".try_into()?;
    let tenant = "tenant".try_into()?;
    let correlation = "same-correlation".try_into()?;
    let first = page(&tx, &installation, &tenant, &correlation, None, 1).await?;
    ensure!(
        first.execution_ids == vec!["A".try_into()?]
            && first.multiple_executions
            && first.next_after == Some("A".try_into()?)
    );
    let second = page(
        &tx,
        &installation,
        &tenant,
        &correlation,
        first.next_after.as_ref(),
        1,
    )
    .await?;
    ensure!(second.execution_ids == vec!["B".try_into()?] && second.multiple_executions);
    let last = page(
        &tx,
        &installation,
        &tenant,
        &correlation,
        second.next_after.as_ref(),
        1,
    )
    .await?;
    ensure!(
        last.execution_ids == vec!["a".try_into()?]
            && last.multiple_executions
            && last.next_after.is_none()
    );
    let empty = page(
        &tx,
        &installation,
        &tenant,
        &correlation,
        Some(&"z".try_into()?),
        1,
    )
    .await?;
    ensure!(
        empty.execution_ids.is_empty() && empty.multiple_executions && empty.next_after.is_none()
    );
    ensure!(
        page(&tx, &installation, &tenant, &correlation, None, 101)
            .await
            .is_err()
    );
    tx.rollback().await?;
    db.close().await?;
    Ok(())
}
#[tokio::test]
async fn corrupt_projection_digest_and_oversized_payload_fail_before_returning_evidence()
-> Result<()> {
    for corruption in [
        "UPDATE tenancy_execution_receipts SET principal_id='changed'",
        "UPDATE tenancy_execution_receipts SET content_digest='bad'",
        "UPDATE tenancy_execution_receipts SET byte_count=0",
        "UPDATE tenancy_execution_receipts SET unknown_outcome=1",
        "UPDATE tenancy_execution_receipts SET receipt_json=CAST(zeroblob(1048577) AS TEXT)",
    ] {
        let db = open().await?;
        let value = candidate("installation", "tenant", "execution", "creator")?;
        commit(&db, &value).await?;
        db.execute_unprepared(corruption).await?;
        ensure!(
            lookup(
                &db,
                &value.receipt.installation_id,
                &value.receipt.tenant_id,
                &value.receipt.execution_id
            )
            .await
            .is_err(),
            "corruption was exposed: {corruption}"
        );
        db.close().await?;
    }
    Ok(())
}
#[tokio::test]
async fn rollback_is_not_a_receipt_and_restart_preserves_exact_committed_bytes() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let url = format!("sqlite://{}", dir.path().join("receipts.sqlite").display());
    let db = crate::db::connect(&url).await?;
    crate::db::run_migrations(&db).await?;
    let value = candidate("installation", "tenant", "execution", "creator")?;
    let tx = db.begin().await?;
    ensure!(insert(&tx, &value).await?);
    tx.rollback().await?;
    ensure!(
        lookup(
            &db,
            &value.receipt.installation_id,
            &value.receipt.tenant_id,
            &value.receipt.execution_id
        )
        .await?
        .is_none()
    );
    commit(&db, &value).await?;
    db.close().await?;
    let db = crate::db::connect(&url).await?;
    let saved = lookup(
        &db,
        &value.receipt.installation_id,
        &value.receipt.tenant_id,
        &value.receipt.execution_id,
    )
    .await?
    .context("reopened receipt")?;
    ensure!(serde_json::to_string(&saved)? == value.json);
    let inventory = inventory(&db, &value.receipt.installation_id).await?;
    ensure!(inventory.bytes == value.bytes() && inventory.unknown.is_empty());
    db.close().await?;
    Ok(())
}

#[tokio::test]
async fn startup_inventory_rejects_understated_bytes_and_hidden_unknown_before_admission()
-> Result<()> {
    for corruption in [
        "UPDATE tenancy_execution_receipts SET byte_count=-1",
        "UPDATE tenancy_execution_receipts SET byte_count=byte_count-1",
        "UPDATE tenancy_execution_receipts SET unknown_outcome=1",
        "UPDATE tenancy_execution_receipts SET receipt_json=json_set(receipt_json,'$.state','unknown'),byte_count=length(CAST(json_set(receipt_json,'$.state','unknown') AS BLOB))+length(CAST(disposition_json AS BLOB))",
        "UPDATE tenancy_execution_receipts SET execution_id=CAST(zeroblob(129) AS TEXT)",
        "UPDATE tenancy_execution_receipts SET disposition_json=CAST(zeroblob(1025) AS TEXT),byte_count=length(CAST(receipt_json AS BLOB))+1025",
    ] {
        let db = open().await?;
        let value = candidate("installation", "tenant", "execution", "creator")?;
        commit(&db, &value).await?;
        ensure!(inventory(&db, &value.receipt.installation_id).await?.rows == 1);
        db.execute_unprepared(corruption).await?;
        ensure!(
            inventory(&db, &value.receipt.installation_id)
                .await
                .is_err(),
            "startup admitted corrupted inventory: {corruption}"
        );
        db.close().await?;
    }
    Ok(())
}

#[cfg(any(feature = "db-postgres", feature = "db-mysql"))]
mod server;

mod counts;

#[cfg(all(feature = "core", feature = "harness"))]
mod usage_evidence;
