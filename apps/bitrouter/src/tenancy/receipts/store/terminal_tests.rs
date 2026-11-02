//! Real SQLite final proofs, with the original eager path as an independent oracle.
use super::*;
use crate::tenancy::{
    oauth::storage as oauth,
    receipts::{auxiliary, auxiliary_store, usage_evidence},
};
use bitrouter_ai::tenancy::execution::{Attempt, Outcome};
use sea_orm::{DatabaseConnection, Statement, TransactionTrait};
use std::sync::atomic::{AtomicBool, Ordering};

const FREE: &archive::Charge<'_> = &|_, _| Ok(());

async fn open() -> Result<DatabaseConnection> {
    let db = crate::db::connect("sqlite::memory:").await?;
    crate::db::run_migrations(&db).await?;
    Ok(db)
}

fn value() -> Result<Candidate> {
    tests::candidate("installation", "tenant", "execution", "creator")?
        .with_usage_evidence(usage_evidence::Snapshot::default())
}

fn key(value: &Candidate) -> obligations::Key {
    obligations::Key {
        tenant: value.receipt.tenant_id.clone(),
        execution: value.receipt.execution_id.clone(),
    }
}

async fn saved(db: &impl ConnectionTrait, value: &Candidate) -> Result<Candidate> {
    let receipt = &value.receipt;
    lookup_candidate(
        db,
        &receipt.installation_id,
        &receipt.tenant_id,
        &receipt.execution_id,
    )
    .await?
    .context("actual canonical receipt")
}

async fn count(db: &impl ConnectionTrait, table: &str) -> Result<i64> {
    db.query_one(Statement::from_string(
        DatabaseBackend::Sqlite,
        format!("SELECT COUNT(*) AS total FROM {table}"),
    ))
    .await?
    .context("actual row count")?
    .try_get("", "total")
    .map_err(Into::into)
}

async fn empty(db: &DatabaseConnection) -> Result<()> {
    for table in [
        "tenancy_execution_receipts",
        "tenancy_usage_evidence",
        "tenancy_receipt_auxiliary",
        "tenancy_unknown_obligations",
        "tenancy_receipt_archive",
    ] {
        ensure!(
            count(db, table).await? == 0,
            "rollback retained rows in {table}"
        );
    }
    Ok(())
}

fn pending(value: &Candidate, resource: &str) -> Result<oauth::Row> {
    let receipt = &value.receipt;
    Ok(oauth::pending::tests::fixture(
        &receipt.installation_id,
        &receipt.tenant_id,
        &receipt.execution_id,
        resource,
        "2026-10-01T00:00:00Z",
    )?
    .1)
}

fn fact(row: &oauth::Row, outcome: auxiliary::Outcome) -> Result<auxiliary::Fact> {
    let oauth::State::RefreshStarted(work) = &row.metadata.state else {
        anyhow::bail!("fixture must own pending refresh work");
    };
    Ok(auxiliary::Fact {
        credential: row.metadata.reference.clone(),
        slot: row.metadata.slot.clone(),
        generation: row.metadata.generation,
        work: work.id.clone(),
        request_digest: Some("a".repeat(64)),
        http_status: (outcome == auxiliary::Outcome::Rejected).then_some(403),
        outcome,
    })
}

#[tokio::test]
async fn fresh_terminal_matches_eager_reconciliation_for_every_source_kind() -> Result<()> {
    for case in 0..5 {
        let db = open().await?;
        let mut value = value()?;
        if case == 1 || case == 2 {
            let mut receipt = value.receipt;
            if case == 1 {
                receipt.state = ReceiptState::Unknown;
                receipt.outcome = Outcome::Unknown;
            } else {
                for (id, outcome) in [("first", Outcome::Failed), ("last", Outcome::Unknown)] {
                    receipt.attempts.push(Attempt {
                        attempt_id: id.try_into()?,
                        target: None,
                        outcome,
                        usage: receipt.usage.clone(),
                        native_usage: vec![],
                    });
                }
            }
            let statuses = vec![None; receipt.attempts.len()];
            value = Candidate::new(
                receipt,
                Disposition {
                    dispatch: DispatchState::Unknown,
                    statuses,
                    auxiliary_digest: None,
                },
            )?
            .with_usage_evidence(usage_evidence::Snapshot::default())?;
        }
        if case >= 3 {
            let row = pending(&value, "refresh")?;
            let outcome = if case == 3 {
                auxiliary::Outcome::Unknown
            } else {
                auxiliary::Outcome::Rejected
            };
            value = tests::candidate("installation", "tenant", "execution", "creator")?
                .with_auxiliary(vec![fact(&row, outcome)?])?
                .with_usage_evidence(usage_evidence::Snapshot::default())?;
            let tx = db.begin().await?;
            oauth::insert(&tx, &row).await?;
            let prior =
                obligations::reconcile(&tx, &value.receipt.installation_id, &key(&value), FREE)
                    .await?;
            ensure!(prior.delta() == 1);
            tx.commit().await?;
        }
        let mut expected = None;
        // Each transaction starts from the same committed projection, not from
        // the other path's already reconciled output.
        for staged_path in [false, true] {
            let tx = db.begin().await?;
            let result: Result<_> = async {
                let change = if staged_path {
                    let staged = stage_terminal(&tx, &value).await?;
                    ensure!(staged.inserted());
                    auxiliary_store::insert(&tx, &value).await?;
                    staged.reconcile(FREE).await?
                } else {
                    ensure!(insert(&tx, &value).await?);
                    auxiliary_store::insert(&tx, &value).await?;
                    obligations::reconcile(&tx, &value.receipt.installation_id, &key(&value), FREE)
                        .await?
                };
                same_completion(&saved(&tx, &value).await?, &value)?;
                let inventory =
                    obligations::validate(&tx, &value.receipt.installation_id, FREE).await?;
                ensure!(inventory.unknown_count == u64::from((1..=3).contains(&case)));
                Ok((change.before, change.after, inventory.unknown_count))
            }
            .await;
            tx.rollback().await?;
            let observed = result?;
            if let Some(expected) = &expected {
                ensure!(
                    &observed == expected,
                    "fresh and eager source classification differ"
                );
            } else {
                expected = Some(observed);
            }
        }
        db.close().await?;
    }
    Ok(())
}

#[tokio::test]
async fn final_proof_rejects_internally_canonical_but_different_completion() -> Result<()> {
    for component in ["receipt", "auxiliary", "usage"] {
        let db = open().await?;
        let row = pending(&value()?, "refresh")?;
        let original_fact = fact(&row, auxiliary::Outcome::Unknown)?;
        let expected = tests::candidate("installation", "tenant", "execution", "creator")?
            .with_auxiliary(vec![original_fact.clone()])?
            .with_usage_evidence(usage_evidence::Snapshot::default())?;
        let mut receipt = expected.receipt.clone();
        if component == "receipt" {
            receipt.recorded_at = "2026-10-25T00:00:01Z".try_into()?;
        }
        let mut changed_fact = original_fact;
        if component == "auxiliary" {
            changed_fact.request_digest = Some("b".repeat(64));
        }
        let mut usage = usage_evidence::Snapshot::default();
        if component == "usage" {
            usage.estimator_seen = true;
        }
        let replacement = Candidate::new(
            receipt,
            Disposition {
                dispatch: DispatchState::NotStarted,
                statuses: vec![],
                auxiliary_digest: None,
            },
        )?
        .with_auxiliary(vec![changed_fact])?
        .with_usage_evidence(usage)?;
        let tx = db.begin().await?;
        let result: Result<()> = async {
            let staged = stage_terminal(&tx, &expected).await?;
            ensure!(staged.inserted());
            auxiliary_store::insert(&tx, &expected).await?;
            for table in [
                "tenancy_receipt_auxiliary",
                "tenancy_usage_evidence",
                "tenancy_execution_receipts",
            ] {
                tx.execute_unprepared(&format!("DELETE FROM {table}"))
                    .await?;
            }
            ensure!(insert(&tx, &replacement).await?);
            auxiliary_store::insert(&tx, &replacement).await?;
            // A decoder-only regression would pass this independently valid
            // replacement. Final proof must still compare intended completion.
            let actual = saved(&tx, &replacement).await?;
            same_completion(&actual, &replacement)?;
            ensure!(same_completion(&actual, &expected).is_err());
            let error = staged
                .reconcile(FREE)
                .await
                .err()
                .context("accepted a different canonical completion")?;
            ensure!(error.to_string().contains("immutable receipt conflict"));
            Ok(())
        }
        .await;
        tx.rollback().await?;
        result?;
        empty(&db).await?;
        db.close().await?;
    }
    Ok(())
}

#[tokio::test]
async fn final_proof_rejects_sidecar_and_index_corruption_before_projection() -> Result<()> {
    for mutation in [
        "DELETE FROM tenancy_usage_evidence",
        "UPDATE tenancy_usage_evidence SET evidence_json='{'",
        "UPDATE tenancy_usage_evidence SET receipt_digest='foreign'",
        "UPDATE tenancy_usage_evidence SET evidence_digest='wrong'",
        "UPDATE tenancy_usage_evidence SET schema_version=2",
        "UPDATE tenancy_usage_evidence SET byte_count=byte_count+1",
        "UPDATE tenancy_usage_evidence SET storage_kind=1",
        "UPDATE tenancy_execution_receipts SET principal_id='foreign'",
        "UPDATE tenancy_execution_receipts SET storage_kind=1",
        "DELETE FROM tenancy_receipt_auxiliary",
        "UPDATE tenancy_receipt_auxiliary SET evidence_json='{'",
    ] {
        let db = open().await?;
        let row = pending(&value()?, "refresh")?;
        let value = tests::candidate("installation", "tenant", "execution", "creator")?
            .with_auxiliary(vec![fact(&row, auxiliary::Outcome::Unknown)?])?
            .with_usage_evidence(usage_evidence::Snapshot::default())?;
        let tx = db.begin().await?;
        let result: Result<()> = async {
            let staged = stage_terminal(&tx, &value).await?;
            ensure!(staged.inserted());
            auxiliary_store::insert(&tx, &value).await?;
            tx.execute_unprepared(mutation).await?;
            ensure!(staged.reconcile(FREE).await.is_err(), "accepted {mutation}");
            ensure!(count(&tx, "tenancy_unknown_obligations").await? == 0);
            Ok(())
        }
        .await;
        tx.rollback().await?;
        result?;
        empty(&db).await?;
        db.close().await?;
    }
    Ok(())
}

#[tokio::test]
async fn final_proof_checks_absence_when_no_sidecar_was_intended() -> Result<()> {
    for auxiliary in [false, true] {
        let db = open().await?;
        let expected = tests::candidate("installation", "tenant", "execution", "creator")?;
        let tx = db.begin().await?;
        let result: Result<()> = async {
            let staged = stage_terminal(&tx, &expected).await?;
            ensure!(staged.inserted());
            if auxiliary {
                let row = pending(&expected, "refresh")?;
                let extra = tests::candidate("installation", "tenant", "execution", "creator")?
                    .with_auxiliary(vec![fact(&row, auxiliary::Outcome::Unknown)?])?;
                auxiliary_store::insert(&tx, &extra).await?;
            } else {
                usage_evidence::storage::insert_fresh(&tx, &value()?).await?;
            }
            ensure!(staged.reconcile(FREE).await.is_err());
            ensure!(count(&tx, "tenancy_unknown_obligations").await? == 0);
            Ok(())
        }
        .await;
        tx.rollback().await?;
        result?;
        empty(&db).await?;
        db.close().await?;
    }
    Ok(())
}

#[tokio::test]
async fn fresh_archive_is_refused_but_committed_archive_replay_stays_exact() -> Result<()> {
    let db = open().await?;
    let value = value()?;
    let prepared = archive::Prepared::new(&value, FREE)?;
    let tx = db.begin().await?;
    let result: Result<()> = async {
        let staged = stage_terminal(&tx, &value).await?;
        ensure!(staged.inserted());
        ensure!(archive::archive_one(&tx, &prepared, FREE).await?);
        same_completion(&saved(&tx, &value).await?, &value)?;
        ensure!(
            staged.reconcile(FREE).await.is_err(),
            "fresh receipt acquired archive eligibility"
        );
        Ok(())
    }
    .await;
    tx.rollback().await?;
    result?;
    empty(&db).await?;
    ensure!(tests::commit(&db, &value).await?);
    let tx = db.begin().await?;
    ensure!(archive::archive_one(&tx, &prepared, FREE).await?);
    tx.commit().await?;
    let tx = db.begin().await?;
    let staged = stage_terminal(&tx, &value).await?;
    ensure!(!staged.inserted());
    auxiliary_store::insert(&tx, &value).await?;
    ensure!(staged.reconcile(FREE).await?.delta() == 0);
    same_completion(&saved(&tx, &value).await?, &value)?;
    let row = tx
        .query_one(Statement::from_string(
            DatabaseBackend::Sqlite,
            "SELECT storage_kind,receipt_json FROM tenancy_execution_receipts".to_owned(),
        ))
        .await?
        .context("archived representation")?;
    ensure!(
        row.try_get::<i64>("", "storage_kind")? == 1
            && row.try_get::<String>("", "receipt_json")?.is_empty()
    );
    tx.commit().await?;
    let changed = tests::candidate("installation", "tenant", "execution", "other")?;
    let tx = db.begin().await?;
    let refused = stage_terminal(&tx, &changed).await.is_err();
    tx.rollback().await?;
    ensure!(refused, "changed archived replay accepted");
    let tx = db.begin().await?;
    tx.execute_unprepared("DELETE FROM tenancy_receipt_archive")
        .await?;
    let refused = stage_terminal(&tx, &value).await.is_err();
    tx.rollback().await?;
    ensure!(refused, "missing archive accepted");
    same_completion(&saved(&db, &value).await?, &value)?;
    db.close().await?;
    Ok(())
}

#[tokio::test]
async fn fresh_terminal_preserves_old_schedule_and_refuses_immutable_source_loss() -> Result<()> {
    let db = open().await?;
    let mut receipt = value()?.receipt;
    receipt.state = ReceiptState::Unknown;
    receipt.outcome = Outcome::Unknown;
    let unknown = Candidate::new(
        receipt,
        Disposition {
            dispatch: DispatchState::Unknown,
            statuses: vec![],
            auxiliary_digest: None,
        },
    )?;
    let row = pending(&unknown, "earlier")?;
    let tx = db.begin().await?;
    oauth::insert(&tx, &row).await?;
    let prior = obligations::reconcile(&tx, &unknown.receipt.installation_id, &key(&unknown), FREE)
        .await?
        .after
        .context("prior pending obligation")?;
    obligations::mark_escalated(&tx, &unknown.receipt.installation_id, &prior, FREE).await?;
    tx.commit().await?;
    let tx = db.begin().await?;
    let staged = stage_terminal(&tx, &unknown).await?;
    ensure!(staged.inserted());
    let change = staged.reconcile(FREE).await?;
    let before = change.before.context("retained prior projection")?;
    let after = change.after.context("merged sources")?;
    ensure!(
        before.first == after.first
            && before.due == after.due
            && after.escalated
            && after.source_mask != before.source_mask
    );
    tx.commit().await?;
    let known = value()?;
    let tx = db.begin().await?;
    tx.execute_unprepared("DELETE FROM tenancy_execution_receipts")
        .await?;
    let staged = stage_terminal(&tx, &known).await?;
    ensure!(staged.inserted());
    let refused = staged.reconcile(FREE).await.is_err();
    tx.rollback().await?;
    ensure!(refused, "fresh INSERT cleared an immutable old source");
    ensure!(
        obligations::lookup(&db, &unknown.receipt.installation_id, &key(&unknown), FREE).await?
            == Some(after)
    );
    same_completion(&saved(&db, &unknown).await?, &unknown)?;
    db.close().await?;
    Ok(())
}

#[tokio::test]
async fn final_proof_reads_late_oauth_pages_and_checks_later_witness_bindings() -> Result<()> {
    let db = open().await?;
    let expected = value()?;
    let tx = db.begin().await?;
    let mut last = String::new();
    for index in 0..33 {
        let row = pending(&expected, &format!("refresh-{index}"))?;
        last = last.max(row.metadata.slot.clone());
        oauth::insert(&tx, &row).await?;
    }
    tx.commit().await?;
    let tx = db.begin().await?;
    let staged = stage_terminal(&tx, &expected).await?;
    ensure!(staged.reconcile(FREE).await?.after.is_some());
    tx.rollback().await?;
    let tx = db.begin().await?;
    let staged = stage_terminal(&tx, &expected).await?;
    // Keep indexed execution membership intact: this is the 33rd selected
    // row, whose stored generation disagrees with its valid metadata.
    ensure!(
        tx.execute(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "UPDATE tenancy_oauth_slots SET generation=generation+1 WHERE slot_key=?",
            [last.into()]
        ))
        .await?
        .rows_affected()
            == 1
    );
    let error = staged.reconcile(FREE).await.err();
    tx.rollback().await?;
    ensure!(
        error
            .context("late pending corruption was not inspected")?
            .to_string()
            .contains("OAuth slot projection mismatch")
    );
    db.close().await?;

    let db = open().await?;
    let mut rows = [pending(&expected, "a")?, pending(&expected, "b")?];
    rows.sort_by(|a, b| a.metadata.slot.cmp(&b.metadata.slot));
    let witnessed = tests::candidate("installation", "tenant", "execution", "creator")?
        .with_auxiliary(vec![fact(&rows[0], auxiliary::Outcome::Rejected)?])?
        .with_usage_evidence(usage_evidence::Snapshot::default())?;
    let tx = db.begin().await?;
    oauth::insert(&tx, &rows[0]).await?;
    let staged = stage_terminal(&tx, &witnessed).await?;
    auxiliary_store::insert(&tx, &witnessed).await?;
    ensure!(staged.reconcile(FREE).await?.after.is_none());
    tx.rollback().await?;
    let tx = db.begin().await?;
    for row in &rows {
        oauth::insert(&tx, row).await?;
    }
    let staged = stage_terminal(&tx, &witnessed).await?;
    auxiliary_store::insert(&tx, &witnessed).await?;
    let refused = staged.reconcile(FREE).await.is_err();
    tx.rollback().await?;
    ensure!(refused, "first witnessed slot hid a later binding mismatch");
    empty(&db).await?;
    db.close().await?;
    Ok(())
}

#[tokio::test]
async fn final_proof_charge_refusal_keeps_projection_and_transaction_uncommitted() -> Result<()> {
    use crate::storage::operation_budget::{Error, OperationBudget};
    use bitrouter_ai::tenancy::operation::ErrorCode;
    for cancel in [false, true] {
        let db = open().await?;
        let expected = value()?;
        let cancelled = AtomicBool::new(false);
        let charged = AtomicBool::new(false);
        let check = || {
            if cancelled.load(Ordering::Acquire) {
                Err(ErrorCode::AuthorityUnavailable)
            } else {
                Ok(())
            }
        };
        let budget = OperationBudget::with_row_limit(&check, if cancel { 4096 } else { 0 });
        let tx = db.begin().await?;
        let staged = stage_terminal(&tx, &expected).await?;
        ensure!(staged.inserted());
        cancelled.store(cancel, Ordering::Release);
        let result: Result<()> = async {
            let error = staged
                .reconcile(&|rows, bytes| {
                    charged.store(true, Ordering::Release);
                    budget.charge(rows, bytes).map_err(Into::into)
                })
                .await
                .err()
                .context("charge refusal accepted")?;
            ensure!(charged.load(Ordering::Acquire));
            ensure!(count(&tx, "tenancy_unknown_obligations").await? == 0);
            match (cancel, error.downcast_ref::<Error>()) {
                (false, Some(Error::Capacity))
                | (true, Some(Error::Rejected(ErrorCode::AuthorityUnavailable))) => Ok(()),
                _ => anyhow::bail!("final proof lost the original budget/checkpoint error"),
            }
        }
        .await;
        tx.rollback().await?;
        result?;
        empty(&db).await?;
        db.close().await?;
    }
    Ok(())
}
