use super::super::{
    lookup_candidate, page,
    tests::{candidate, commit},
};
use super::*;
use sea_orm::{DatabaseConnection, TransactionTrait};

const NO_CHARGE: &Charge<'_> = &|_, _| Ok(());

async fn open() -> Result<DatabaseConnection> {
    let db = crate::db::connect("sqlite::memory:").await?;
    crate::db::run_migrations(&db).await?;
    // Local fixture only. The registered migration and exact schema validators
    // own deployment of these columns/table; the archive writer never creates DDL.
    for table in ["tenancy_execution_receipts", "tenancy_usage_evidence"] {
        let columns = db
            .query_all(sqlite(format!("PRAGMA table_info({table})"), []))
            .await?;
        let mut present = false;
        for column in columns {
            present |= column.try_get::<String>("", "name")? == "storage_kind";
        }
        if !present {
            db.execute_unprepared(&format!("ALTER TABLE {table} ADD COLUMN storage_kind INTEGER NOT NULL DEFAULT 0 CHECK(storage_kind IN (0,1))")).await?;
        }
    }
    db.execute_unprepared(
        "CREATE TABLE IF NOT EXISTS tenancy_receipt_archive (
        installation_id TEXT NOT NULL, tenant_id TEXT NOT NULL, execution_id TEXT NOT NULL,
        format_version INTEGER NOT NULL, codec INTEGER NOT NULL,
        receipt_bytes INTEGER NOT NULL, usage_bytes INTEGER NOT NULL, payload BLOB NOT NULL,
        PRIMARY KEY (installation_id,tenant_id,execution_id))",
    )
    .await?;
    Ok(db)
}

fn captured(id: &str) -> Result<Candidate> {
    candidate("installation", "tenant", id, "creator")?
        .with_usage_evidence(crate::tenancy::receipts::usage_evidence::Snapshot::default())
}

async fn relocate(db: &DatabaseConnection, candidate: &Candidate) -> Result<Prepared> {
    let prepared = Prepared::new(candidate, NO_CHARGE)?;
    let tx = db.begin().await?;
    ensure!(archive_one(&tx, &prepared, NO_CHARGE).await?);
    tx.commit().await?;
    Ok(prepared)
}

#[test]
fn codec_roundtrips_raw_and_compressed_exact_components() -> Result<()> {
    let raw = codec::Encoded::encode("x", None, NO_CHARGE)?;
    ensure!(raw.codec == 0 && raw.decode(NO_CHARGE)? == ("x".to_owned(), None));
    let receipt = "r".repeat(32 * 1024);
    let usage = "u".repeat(8 * 1024);
    let compressed = codec::Encoded::encode(&receipt, Some(&usage), NO_CHARGE)?;
    ensure!(compressed.codec == 1);
    ensure!(compressed.decode(NO_CHARGE)? == (receipt, Some(usage)));
    Ok(())
}

#[test]
fn codec_rejects_unknown_versions_codecs_lengths_frames_and_tails() -> Result<()> {
    for corruption in 0..10 {
        let mut encoded = codec::Encoded::encode(&"x".repeat(4096), None, NO_CHARGE)?;
        match corruption {
            0 => encoded.version = 2,
            1 => encoded.codec = 2,
            2 => encoded.receipt_bytes = super::super::MAX_RECEIPT + 1,
            3 => encoded.usage_bytes = Some(0),
            4 => encoded.receipt_bytes -= 1,
            5 => encoded.payload.push(0),
            6 => {
                let extra = encoded.payload.clone();
                encoded.payload.extend(extra);
            }
            7 => {
                encoded.payload.pop();
            }
            8 => encoded.payload = vec![0x50, 0x2a, 0x4d, 0x18, 0, 0, 0, 0],
            9 => encoded.codec = 0,
            _ => anyhow::bail!("invalid test case"),
        }
        ensure!(
            encoded.decode(NO_CHARGE).is_err(),
            "accepted codec corruption {corruption}"
        );
    }
    let mut dictionary = codec::Encoded::encode("x", None, NO_CHARGE)?;
    dictionary.codec = 1;
    dictionary.payload = zstd::bulk::compress(b"x", 1)?;
    ensure!(dictionary.payload.len() > 5 && dictionary.payload[4] & 3 == 0);
    // Add a one-byte nonzero Dictionary_ID to an otherwise ordinary frame.
    dictionary.payload[4] |= 1;
    dictionary.payload.insert(5, 1);
    ensure!(dictionary.decode(NO_CHARGE).is_err());
    Ok(())
}

#[tokio::test]
async fn incomplete_usage_roundtrip_page_replay_and_rollback_preserve_exact_evidence() -> Result<()>
{
    let db = open().await?;
    for original in [
        candidate("installation", "tenant", "old", "creator")?,
        captured("captured")?,
    ] {
        ensure!(commit(&db, &original).await?);
        let r = &original.receipt;
        let before = page(
            &db,
            &r.installation_id,
            &r.tenant_id,
            &r.correlation_id,
            None,
            100,
        )
        .await?;
        let prepared = Prepared::new(&original, NO_CHARGE)?;
        ensure!(prepared.relocated_bytes() >= original.json.len() && prepared.stored_bytes() > 0);
        let tx = db.begin().await?;
        ensure!(archive_one(&tx, &prepared, NO_CHARGE).await?);
        tx.rollback().await?;
        let inline = lookup_candidate(&db, &r.installation_id, &r.tenant_id, &r.execution_id)
            .await?
            .context("rollback receipt")?;
        super::super::same_completion(&inline, &original)?;
        relocate(&db, &original).await?;
        let loaded = lookup_candidate(&db, &r.installation_id, &r.tenant_id, &r.execution_id)
            .await?
            .context("archived receipt")?;
        super::super::same_completion(&loaded, &original)?;
        let tx = db.begin().await?;
        ensure!(!archive_one(&tx, &prepared, NO_CHARGE).await?);
        tx.commit().await?;
        ensure!(!commit(&db, &original).await?);
        let after = page(
            &db,
            &r.installation_id,
            &r.tenant_id,
            &r.correlation_id,
            None,
            100,
        )
        .await?;
        ensure!(
            before.execution_ids == after.execution_ids
                && before.multiple_executions == after.multiple_executions
        );
        let row = db
            .query_one(sqlite(
                format!(
                    "SELECT receipt_json,storage_kind FROM tenancy_execution_receipts WHERE {KEY}"
                ),
                values(&r.installation_id, &r.tenant_id, &r.execution_id),
            ))
            .await?
            .context("marker")?;
        ensure!(
            row.try_get::<String>("", "receipt_json")?.is_empty()
                && row.try_get::<i64>("", "storage_kind")? == 1
        );
        let mut conflict = candidate(
            "installation",
            "tenant",
            r.execution_id.as_str(),
            "other-creator",
        )?;
        if original.usage_evidence.is_some() {
            conflict = conflict.with_usage_evidence(
                crate::tenancy::receipts::usage_evidence::Snapshot::default(),
            )?;
        }
        ensure!(commit(&db, &conflict).await.is_err());
    }
    db.close().await?;
    Ok(())
}

#[tokio::test]
async fn archive_rejects_corrupt_markers_sidecars_digests_and_payloads() -> Result<()> {
    for mutation in [
        "DELETE FROM tenancy_receipt_archive",
        "UPDATE tenancy_execution_receipts SET storage_kind=0",
        "UPDATE tenancy_execution_receipts SET receipt_json='{}'",
        "UPDATE tenancy_execution_receipts SET content_digest=printf('%064d',0)",
        "UPDATE tenancy_execution_receipts SET byte_count=1",
        "UPDATE tenancy_receipt_archive SET payload=CAST(payload || X'00' AS BLOB)",
        "UPDATE tenancy_receipt_archive SET payload=zeroblob(1310721)",
        "UPDATE tenancy_receipt_archive SET payload=CAST(payload AS TEXT)",
        "UPDATE tenancy_receipt_archive SET receipt_bytes=1048577",
        "UPDATE tenancy_receipt_archive SET usage_bytes=262145",
        "UPDATE tenancy_receipt_archive SET codec=2",
        "UPDATE tenancy_receipt_archive SET format_version=2",
        "DELETE FROM tenancy_usage_evidence",
        "UPDATE tenancy_usage_evidence SET storage_kind=0",
        "UPDATE tenancy_usage_evidence SET evidence_json='{}'",
        "UPDATE tenancy_usage_evidence SET receipt_digest=printf('%064d',0)",
        "UPDATE tenancy_usage_evidence SET evidence_digest=printf('%064d',0)",
        "UPDATE tenancy_usage_evidence SET byte_count=1",
        "UPDATE tenancy_usage_evidence SET schema_version=2",
    ] {
        let db = open().await?;
        let original = captured("execution")?;
        commit(&db, &original).await?;
        relocate(&db, &original).await?;
        db.execute_unprepared(mutation).await?;
        let r = &original.receipt;
        ensure!(
            lookup_candidate(&db, &r.installation_id, &r.tenant_id, &r.execution_id)
                .await
                .is_err(),
            "accepted {mutation}"
        );
        db.close().await?;
    }
    Ok(())
}

#[tokio::test]
async fn unknown_and_changed_receipts_never_relocate() -> Result<()> {
    let mut unknown = candidate("installation", "tenant", "unknown", "creator")?;
    unknown.receipt.state = bitrouter_ai::tenancy::execution::ReceiptState::Unknown;
    unknown.receipt.outcome = Outcome::Unknown;
    ensure!(Prepared::new(&unknown, NO_CHARGE).is_err());
    let db = open().await?;
    let original = captured("execution")?;
    commit(&db, &original).await?;
    let prepared = Prepared::new(&original, NO_CHARGE)?;
    db.execute_unprepared("UPDATE tenancy_execution_receipts SET content_digest=printf('%064d',0)")
        .await?;
    let tx = db.begin().await?;
    ensure!(archive_one(&tx, &prepared, NO_CHARGE).await.is_err());
    tx.rollback().await?;
    ensure!(
        db.query_one(sqlite("SELECT 1 FROM tenancy_receipt_archive", []))
            .await?
            .is_none()
    );
    db.close().await?;
    Ok(())
}

#[cfg(all(feature = "core", feature = "harness"))]
#[tokio::test]
async fn native_and_recovery_use_archived_sidecar_and_the_callers_budget() -> Result<()> {
    use crate::storage::operation_budget::{Error, OperationBudget};
    let db = open().await?;
    let original = captured("execution")?;
    commit(&db, &original).await?;
    relocate(&db, &original).await?;
    let r = &original.receipt;
    let check = || Ok(());
    let tx = db.begin().await?;
    let budget = OperationBudget::new(&check);
    let native = super::super::native_namespace_candidate_in(
        &tx,
        &r.installation_id,
        &r.tenant_id,
        &r.execution_id,
        &budget,
    )
    .await?
    .context("native archive")?;
    super::super::same_completion(&native, &original)?;
    let budget = OperationBudget::with_limits(&check, 4096, 1024);
    let error = super::super::native_namespace_candidate_in(
        &tx,
        &r.installation_id,
        &r.tenant_id,
        &r.execution_id,
        &budget,
    )
    .await
    .err()
    .context("capacity rejection")?;
    ensure!(matches!(
        error.downcast_ref::<Error>(),
        Some(Error::Capacity)
    ));
    let recovery = super::super::recovery_lookup(
        &tx,
        &r.installation_id,
        &r.tenant_id,
        &r.execution_id,
        &crate::tenancy::session::recovery::Budget::default(),
    )
    .await?
    .context("recovery archive")?;
    super::super::same_completion(&recovery, &original)?;
    tx.rollback().await?;
    db.close().await?;
    Ok(())
}

#[cfg(unix)]
mod reuse;
mod transfer;

#[tokio::test]
async fn known_auxiliary_stays_inline_and_unknown_auxiliary_never_archives() -> Result<()> {
    use crate::tenancy::receipts::{auxiliary, auxiliary_store};
    use bitrouter_ai::tenancy::{
        authority::{OwnerKind, ResourceKind},
        configuration::ResourceRef,
    };
    let db = open().await?;
    for unknown in [false, true] {
        let original = captured(if unknown {
            "unknown-auxiliary"
        } else {
            "known-auxiliary"
        })?;
        let credential = ResourceRef {
            owner_kind: OwnerKind::Tenant,
            owner_id: "tenant".try_into()?,
            resource_kind: ResourceKind::Credential,
            resource_id: "credential".try_into()?,
            revision_id: "revision".try_into()?,
        };
        let fact = auxiliary::Fact {
            slot: crate::tenancy::oauth::storage::slot(
                &original.receipt.installation_id,
                &credential,
            )?,
            credential,
            generation: 0,
            work: uuid::Uuid::new_v4().to_string(),
            request_digest: unknown.then(|| "a".repeat(64)),
            http_status: None,
            outcome: if unknown {
                auxiliary::Outcome::Unknown
            } else {
                auxiliary::Outcome::NotSent
            },
        };
        let original = original.with_auxiliary(vec![fact])?;
        let tx = db.begin().await?;
        ensure!(super::super::insert(&tx, &original).await?);
        auxiliary_store::insert(&tx, &original).await?;
        tx.commit().await?;
        if unknown {
            ensure!(Prepared::new(&original, NO_CHARGE).is_err());
            continue;
        }
        relocate(&db, &original).await?;
        let r = &original.receipt;
        let saved = lookup_candidate(&db, &r.installation_id, &r.tenant_id, &r.execution_id)
            .await?
            .context("archived auxiliary")?;
        super::super::same_completion(&saved, &original)?;
        ensure!(saved.auxiliary_unclean() && !saved.auxiliary_unknown());
        ensure!(!commit(&db, &original).await?);
        #[cfg(all(feature = "core", feature = "harness"))]
        ensure!(
            super::super::recovery_lookup(
                &db,
                &r.installation_id,
                &r.tenant_id,
                &r.execution_id,
                &crate::tenancy::session::recovery::Budget::default()
            )
            .await
            .is_err()
        );
    }
    db.close().await?;
    Ok(())
}
