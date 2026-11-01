use super::super::tests::candidate;
use super::*;
use crate::tenancy::{
    oauth::storage as oauth,
    receipts::{auxiliary, auxiliary_store},
};
use bitrouter_ai::tenancy::execution::{Attempt, Outcome};
use sea_orm::{DatabaseConnection, TransactionTrait};
const FREE: &Charge<'static> = &|_, _| Ok(());

async fn open() -> Result<DatabaseConnection> {
    let db = crate::db::connect("sqlite::memory:").await?;
    crate::db::run_migrations(&db).await?;
    Ok(db)
}
fn key(id: &str) -> Result<Key> {
    Ok(Key {
        tenant: "tenant".try_into()?,
        execution: id.try_into()?,
    })
}
fn unknown(id: &str) -> Result<Candidate> {
    let mut receipt = candidate("installation", "tenant", id, "creator")?.receipt;
    receipt.state = ReceiptState::Unknown;
    receipt.outcome = Outcome::Unknown;
    Candidate::new(
        receipt,
        Disposition {
            dispatch: DispatchState::Unknown,
            statuses: vec![],
            auxiliary_digest: None,
        },
    )
}
async fn put(tx: &DatabaseTransaction, value: &Candidate) -> Result<()> {
    super::super::insert(tx, value).await?;
    auxiliary_store::insert(tx, value).await
}
async fn ready(
    tx: &DatabaseTransaction,
    crypto: &crate::tenancy::oauth::crypto::Crypto,
    row: &oauth::Row,
) -> Result<()> {
    let mut metadata = row.metadata.clone();
    metadata.state = oauth::State::Ready;
    let next = oauth::Row::new(crypto, metadata, &row.token(crypto)?)?;
    oauth::replace(tx, row, &next).await
}
#[tokio::test]
async fn canonical_outcome_and_attempt_unknown_count_but_incomplete_usage_does_not() -> Result<()> {
    let db = open().await?;
    let installation = "installation".try_into()?;
    let tx = db.begin().await?;
    for (id, outcome, attempt, expected) in [
        ("usage", Outcome::Failed, false, 0),
        ("overall", Outcome::Unknown, false, 1),
        ("attempt", Outcome::Failed, true, 1),
    ] {
        let mut receipt = candidate("installation", "tenant", id, "creator")?.receipt;
        receipt.outcome = outcome;
        if attempt {
            receipt.attempts.push(Attempt {
                attempt_id: "attempt".try_into()?,
                target: None,
                outcome: Outcome::Unknown,
                usage: receipt.usage.clone(),
                native_usage: vec![],
            });
        }
        let value = Candidate::new(
            receipt,
            Disposition {
                dispatch: DispatchState::Unknown,
                statuses: if attempt { vec![None] } else { vec![] },
                auxiliary_digest: None,
            },
        )?;
        put(&tx, &value).await?;
        ensure!(
            reconcile(&tx, &installation, &key(id)?, FREE)
                .await?
                .delta()
                == expected
        );
        ensure!(
            reconcile(&tx, &installation, &key(id)?, FREE)
                .await?
                .delta()
                == 0
        );
    }
    ensure!(tenant_count(&tx, &installation, &"tenant".try_into()?, FREE).await? == 2);
    let inventory = validate(&tx, &installation, FREE).await?;
    ensure!(inventory.unknown_count == 2);
    tx.commit().await?;
    let tx = db.begin().await?;
    put(&tx, &unknown("rollback")?).await?;
    ensure!(
        reconcile(&tx, &installation, &key("rollback")?, FREE)
            .await?
            .delta()
            == 1
    );
    tx.rollback().await?;
    ensure!(
        lookup(&db, &installation, &key("rollback")?, FREE)
            .await?
            .is_none()
    );
    db.close().await?;
    Ok(())
}
#[tokio::test]
async fn multiple_pending_sources_count_once_and_resolution_replay_cannot_clear_receipt_unknown()
-> Result<()> {
    let db = open().await?;
    let installation = "installation".try_into()?;
    let key = key("execution")?;
    let (crypto_a, a) = oauth::pending::tests::fixture(
        &installation,
        &key.tenant,
        &key.execution,
        "a",
        "2026-10-01T00:00:00Z",
    )?;
    let (crypto_b, b) = oauth::pending::tests::fixture(
        &installation,
        &key.tenant,
        &key.execution,
        "b",
        "2026-10-02T00:00:00Z",
    )?;
    let mut tx = db.begin().await?;
    oauth::insert(&tx, &a).await?;
    ensure!(reconcile(&tx, &installation, &key, FREE).await?.delta() == 1);
    tx.commit().await?;
    tx = db.begin().await?;
    oauth::insert(&tx, &b).await?;
    ensure!(reconcile(&tx, &installation, &key, FREE).await?.delta() == 0);
    tx.commit().await?;
    tx = db.begin().await?;
    ready(&tx, &crypto_a, &a).await?;
    ensure!(reconcile(&tx, &installation, &key, FREE).await?.delta() == 0);
    tx.commit().await?;
    tx = db.begin().await?;
    ready(&tx, &crypto_b, &b).await?;
    ensure!(reconcile(&tx, &installation, &key, FREE).await?.delta() == -1);
    ensure!(reconcile(&tx, &installation, &key, FREE).await?.delta() == 0);
    tx.commit().await?;
    tx = db.begin().await?;
    let (_, c) = oauth::pending::tests::fixture(
        &installation,
        &key.tenant,
        &key.execution,
        "c",
        "2026-10-03T00:00:00Z",
    )?;
    oauth::insert(&tx, &c).await?;
    ensure!(reconcile(&tx, &installation, &key, FREE).await?.delta() == 1);
    put(&tx, &unknown("execution")?).await?;
    let change = reconcile(&tx, &installation, &key, FREE).await?;
    ensure!(
        change.delta() == 0 && change.after.context("merged")?.source_mask == (RECEIPT | PENDING)
    );
    ensure!(validate(&tx, &installation, FREE).await?.unknown_count == 1);
    tx.commit().await?;
    // Removing immutable source data cannot be laundered by reconciliation.
    let tx = db.begin().await?;
    tx.execute_unprepared("DELETE FROM tenancy_execution_receipts")
        .await?;
    ensure!(reconcile(&tx, &installation, &key, FREE).await.is_err());
    tx.rollback().await?;
    db.close().await?;
    Ok(())
}
#[tokio::test]
async fn exact_rejection_settles_pending_but_unknown_witness_preserves_auxiliary_source()
-> Result<()> {
    for (outcome, model_unknown) in [
        (auxiliary::Outcome::Rejected, false),
        (auxiliary::Outcome::Unknown, false),
        (auxiliary::Outcome::Unknown, true),
    ] {
        let db = open().await?;
        let installation = "installation".try_into()?;
        let key = key("execution")?;
        let (crypto, row) = oauth::pending::tests::fixture(
            &installation,
            &key.tenant,
            &key.execution,
            "slot",
            "2026-10-01T00:00:00Z",
        )?;
        let work = match &row.metadata.state {
            oauth::State::RefreshStarted(work) => work,
            _ => anyhow::bail!("fixture state"),
        };
        let base = if model_unknown {
            unknown("execution")?
        } else {
            candidate("installation", "tenant", "execution", "creator")?
        };
        let value = base.with_auxiliary(vec![auxiliary::Fact {
            credential: row.metadata.reference.clone(),
            slot: row.metadata.slot.clone(),
            generation: 0,
            work: work.id.clone(),
            request_digest: Some("a".repeat(64)),
            http_status: if outcome == auxiliary::Outcome::Rejected {
                Some(403)
            } else {
                None
            },
            outcome,
        }])?;
        let tx = db.begin().await?;
        oauth::insert(&tx, &row).await?;
        ensure!(reconcile(&tx, &installation, &key, FREE).await?.delta() == 1);
        put(&tx, &value).await?;
        ensure!(auxiliary_store::pending_receipted(&tx, &row.metadata, work).await?);
        let result = reconcile(&tx, &installation, &key, FREE).await?;
        if outcome == auxiliary::Outcome::Rejected {
            ensure!(result.delta() == -1 && result.after.is_none());
            let prepared = archive::Prepared::new(&value, FREE)?;
            ensure!(archive::archive_one(&tx, &prepared, FREE).await?);
            ensure!(reconcile(&tx, &installation, &key, FREE).await?.delta() == 0);
        } else {
            let mask = AUXILIARY | if model_unknown { RECEIPT } else { 0 };
            ensure!(result.delta() == 0 && result.after.context("auxiliary")?.source_mask == mask);
        }
        let expected = u64::from(outcome == auxiliary::Outcome::Unknown);
        ensure!(validate(&tx, &installation, FREE).await?.unknown_count == expected);
        tx.commit().await?;
        let tx = db.begin().await?;
        let mut changed = row.metadata.clone();
        if let oauth::State::RefreshStarted(work) = &mut changed.state {
            work.principal = "other".try_into()?;
        }
        let changed = oauth::Row::new(&crypto, changed, &row.token(&crypto)?)?;
        oauth::replace(&tx, &row, &changed).await?;
        ensure!(reconcile(&tx, &installation, &key, FREE).await.is_err());
        tx.rollback().await?;
        db.close().await?;
    }
    Ok(())
}
#[tokio::test]
async fn scheduling_merges_earlier_sources_and_keeps_escalation_sticky_without_digest_change()
-> Result<()> {
    let db = open().await?;
    let installation = "installation".try_into()?;
    let key = key("execution")?;
    let (_, late) = oauth::pending::tests::fixture(
        &installation,
        &key.tenant,
        &key.execution,
        "late",
        "2026-10-02T00:00:00Z",
    )?;
    let (_, early) = oauth::pending::tests::fixture(
        &installation,
        &key.tenant,
        &key.execution,
        "early",
        "2026-10-01T00:00:00Z",
    )?;
    let tx = db.begin().await?;
    oauth::insert(&tx, &late).await?;
    let _change = reconcile(&tx, &installation, &key, FREE).await?;
    let before = lookup(&tx, &installation, &key, FREE)
        .await?
        .context("before")?;
    let hash = validate(&tx, &installation, FREE).await?;
    mark_escalated(&tx, &installation, &before, FREE).await?;
    mark_escalated(&tx, &installation, &before, FREE).await?;
    ensure!(validate(&tx, &installation, FREE).await? == hash);
    oauth::insert(&tx, &early).await?;
    let result = reconcile(&tx, &installation, &key, FREE).await?;
    ensure!(result.delta() == 0);
    let after = result.after.context("after")?;
    ensure!(after.first < before.first && after.escalated && after.due == due(after.first)?);
    ensure!(
        due_page(&tx, &installation, before.due, FREE)
            .await?
            .is_empty()
    );
    ensure!(validate(&tx, &installation, FREE).await?.digest != hash.digest);
    tx.commit().await?;
    ensure!(due(DateTime::<Utc>::MAX_UTC).is_err());
    db.close().await?;
    Ok(())
}
#[tokio::test]
async fn bidirectional_validation_rejects_missing_extra_wrong_masks_and_hidden_pending_rows()
-> Result<()> {
    for mutation in [
        "DELETE FROM tenancy_unknown_obligations",
        "UPDATE tenancy_unknown_obligations SET execution_id='orphan'",
        "UPDATE tenancy_unknown_obligations SET source_mask=2",
        "UPDATE tenancy_unknown_obligations SET due_seconds=due_seconds+1",
        "UPDATE tenancy_oauth_slots SET pending_tenant_id=NULL,pending_execution_id=NULL",
        "UPDATE tenancy_oauth_slots SET state='ready',pending_tenant_id=NULL,pending_execution_id=NULL",
    ] {
        let db = open().await?;
        let installation = "installation".try_into()?;
        let key = key("execution")?;
        let (_, row) = oauth::pending::tests::fixture(
            &installation,
            &key.tenant,
            &key.execution,
            "slot",
            "2026-10-01T00:00:00Z",
        )?;
        let tx = db.begin().await?;
        oauth::insert(&tx, &row).await?;
        let _change = reconcile(&tx, &installation, &key, FREE).await?;
        validate(&tx, &installation, FREE).await?;
        tx.execute_unprepared(mutation).await?;
        ensure!(
            validate(&tx, &installation, FREE).await.is_err(),
            "accepted {mutation}"
        );
        tx.rollback().await?;
        db.close().await?;
    }
    Ok(())
}
#[tokio::test]
async fn complete_pages_and_indexed_counts_have_no_lifetime_limit_or_partial_success() -> Result<()>
{
    let db = open().await?;
    let installation = "installation".try_into()?;
    let tx = db.begin().await?;
    for i in 0..71 {
        let id = format!("execution-{i:03}");
        put(&tx, &unknown(&id)?).await?;
        let _change = reconcile(&tx, &installation, &key(&id)?, FREE).await?;
    }
    let inventory = validate(&tx, &installation, FREE).await?;
    ensure!(inventory.unknown_count == 71);
    ensure!(tenant_count(&tx, &installation, &"tenant".try_into()?, FREE).await? == 71);
    let mut fold = Fold::new();
    let mut after = None;
    loop {
        let rows = page(&tx, &installation, after.as_ref(), FREE).await?;
        if rows.is_empty() {
            break;
        }
        ensure!(rows.len() <= PAGE);
        for row in rows {
            fold.push(&installation, &row)?;
            after = Some(row.key);
        }
    }
    ensure!(fold.finish() == inventory);
    let now = DateTime::parse_from_rfc3339("2026-11-01T00:00:00Z")?.with_timezone(&Utc);
    let mut marked = 0;
    ensure!(next_due(&tx, &installation, FREE).await?.is_some());
    loop {
        let rows = due_page(&tx, &installation, now, FREE).await?;
        if rows.is_empty() {
            break;
        }
        for row in rows {
            mark_escalated(&tx, &installation, &row, FREE).await?;
            marked += 1;
        }
    }
    ensure!(marked == 71 && validate(&tx, &installation, FREE).await? == inventory);
    ensure!(next_due(&tx, &installation, FREE).await?.is_none());
    ensure!(
        tenant_count(
            &tx,
            &installation,
            &"tenant".try_into()?,
            &|_, _| anyhow::bail!("budget interrupted")
        )
        .await
        .is_err()
    );
    let plan=tx.query_all(sql("EXPLAIN QUERY PLAN SELECT COUNT(*) FROM tenancy_unknown_obligations WHERE installation_id='installation' AND tenant_id='tenant'", [])).await?;
    let details = plan
        .into_iter()
        .map(|row| row.try_get::<String>("", "detail"))
        .collect::<std::result::Result<Vec<_>, _>>()?
        .join("\n");
    ensure!(
        details.contains("COVERING INDEX") && details.contains("installation_id=? AND tenant_id=?")
    );
    tx.rollback().await?;
    db.close().await?;
    Ok(())
}

/// Canonical old-layout receipt/usage/auxiliary/OAuth data for migration043.
pub(crate) async fn seed_legacy(db: &DatabaseConnection) -> Result<()> {
    let installation = "installation".try_into()?;
    let tx = db.begin().await?;
    tx.execute_unprepared("INSERT INTO tenancy_installation (singleton,installation_id,schema_version,authority_revision,operation_ids,safety_operation_ids,saturated,created_at) VALUES (1,'installation',1,1,0,0,0,'2026-10-01T00:00:00Z')").await?;
    for (resource, tenant, state) in [
        ("ready", "tenant", 0),
        ("started", "Tenant", 1),
        ("quarantined", "tenant", 2),
    ] {
        let (crypto, row) = oauth::pending::tests::fixture(
            &installation,
            &tenant.try_into()?,
            &"Execution".try_into()?,
            resource,
            "2026-10-01T00:00:00Z",
        )?;
        let mut metadata = row.metadata.clone();
        let work = match &metadata.state {
            oauth::State::RefreshStarted(work) => work.clone(),
            _ => anyhow::bail!("fixture work"),
        };
        metadata.state = match state {
            0 => oauth::State::Ready,
            1 => oauth::State::RefreshStarted(work.clone()),
            _ => oauth::State::Quarantined(work.clone()),
        };
        let row = oauth::Row::new(&crypto, metadata, &row.token(&crypto)?)?;
        oauth::pending::tests::insert_legacy(&tx, &row).await?;
        if state == 2 {
            let receipt = unknown("Execution")?
                .with_auxiliary(vec![auxiliary::Fact {
                    credential: row.metadata.reference.clone(),
                    slot: row.metadata.slot.clone(),
                    generation: 0,
                    work: work.id,
                    request_digest: Some("a".repeat(64)),
                    http_status: None,
                    outcome: auxiliary::Outcome::Unknown,
                }])?
                .with_usage_evidence(
                    crate::tenancy::receipts::usage_evidence::Snapshot::default(),
                )?;
            put(&tx, &receipt).await?;
        }
    }
    tx.commit().await?;
    Ok(())
}

#[tokio::test]
async fn full_validation_reserves_one_working_envelope_across_all_pending_pages() -> Result<()> {
    use crate::storage::operation_budget::OperationBudget;
    use std::sync::atomic::{AtomicUsize, Ordering};
    let db = open().await?;
    let installation: InstallationId = "installation".try_into()?;
    let key = key("many-pending")?;
    let tx = db.begin().await?;
    put(&tx, &unknown(key.execution.as_str())?).await?;
    for index in 0..65 {
        let (_, row) = oauth::pending::tests::fixture(
            &installation,
            &key.tenant,
            &key.execution,
            &format!("slot-{index:03}"),
            "2026-10-01T00:00:00Z",
        )?;
        oauth::insert(&tx, &row).await?;
    }
    let _change = reconcile(&tx, &installation, &key, FREE).await?;
    let check = || Ok(());
    let cumulative = OperationBudget::new(&check);
    ensure!(
        validate(&tx, &installation, &|rows, bytes| {
            Ok(cumulative.charge(rows, bytes)?)
        })
        .await
        .is_err(),
        "fixture must exceed a cumulative whole-history allowance"
    );

    let budget =
        OperationBudget::with_limits(&check, validation::WORKING_ROWS, validation::WORKING_BYTES);
    let reservations = AtomicUsize::new(0);
    let checkpoints = AtomicUsize::new(0);
    let actual = validate_with_budget(&tx, &installation, &|rows, bytes| {
        if rows != 0 || bytes != 0 {
            ensure!(rows == validation::WORKING_ROWS && bytes == validation::WORKING_BYTES);
            reservations.fetch_add(1, Ordering::Relaxed);
        } else {
            checkpoints.fetch_add(1, Ordering::Relaxed);
        }
        Ok(budget.charge(rows, bytes)?)
    })
    .await?;
    ensure!(actual.unknown_count == 1 && actual == validate(&tx, &installation, FREE).await?);
    ensure!(reservations.load(Ordering::Relaxed) == 1 && checkpoints.load(Ordering::Relaxed) > 65);

    let too_small = OperationBudget::with_limits(
        &check,
        validation::WORKING_ROWS,
        validation::WORKING_BYTES - 1,
    );
    ensure!(
        validate_with_budget(&tx, &installation, &|rows, bytes| {
            Ok(too_small.charge(rows, bytes)?)
        })
        .await
        .is_err()
    );
    let interrupted = AtomicUsize::new(0);
    ensure!(
        validate_with_budget(&tx, &installation, &|_, _| {
            ensure!(
                interrupted.fetch_add(1, Ordering::Relaxed) < 20,
                "validation interrupted"
            );
            Ok(())
        })
        .await
        .is_err()
    );
    // Fixed-envelope accounting still executes canonical-to-projection coverage.
    tx.execute_unprepared("DELETE FROM tenancy_unknown_obligations")
        .await?;
    ensure!(
        validate_with_budget(&tx, &installation, FREE)
            .await
            .is_err()
    );
    tx.rollback().await?;
    db.close().await?;
    Ok(())
}
