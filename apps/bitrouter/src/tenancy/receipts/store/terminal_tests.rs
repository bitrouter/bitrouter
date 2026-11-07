//! Real SQLite final proofs, with the original eager path as an independent oracle.
use super::*;
use crate::tenancy::{
    oauth::storage as oauth,
    receipts::{auxiliary, auxiliary_store, usage_evidence},
};
use bitrouter_ai::tenancy::execution::{Attempt, Outcome};
use sea_orm::{DatabaseConnection, DbErr, ExecResult, Statement, TransactionTrait};
use std::sync::{
    Mutex,
    atomic::{AtomicBool, Ordering},
};

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

// Only these two payload columns are observed. The delegate retains scalar
// counts, and can change one real SQL row after its scalar read has completed.
struct Transfer<'a> {
    tx: &'a DatabaseTransaction,
    bodies: Mutex<[(usize, usize); 2]>,
    between: Option<(&'static str, &'static str)>,
    changed: AtomicBool,
}
impl<'a> Transfer<'a> {
    fn new(tx: &'a DatabaseTransaction, between: Option<(&'static str, &'static str)>) -> Self {
        Self {
            tx,
            bodies: Mutex::new([(0, 0); 2]),
            between,
            changed: AtomicBool::new(false),
        }
    }
    fn counts(&self) -> Result<[(usize, usize); 2]> {
        self.bodies
            .lock()
            .map(|value| *value)
            .map_err(|_| anyhow::anyhow!("transfer observation poisoned"))
    }
    async fn observed(&self, sql: &str, rows: &[QueryResult]) -> std::result::Result<(), DbErr> {
        for (index, column) in ["receipt_json", "evidence_json"].into_iter().enumerate() {
            if sql.contains(&format!(" AS \"{column}\"")) {
                let mut counts = self
                    .bodies
                    .lock()
                    .map_err(|_| DbErr::Custom("transfer observation poisoned".into()))?;
                counts[index].0 += 1;
                for row in rows {
                    // A BLOB must not become a false zero-byte observation just
                    // because the normal String decoder rejects its type.
                    let bytes = match row.try_get::<Option<String>>("", column) {
                        Ok(value) => value.map_or(0, |value| value.len()),
                        Err(_) => row
                            .try_get::<Option<Vec<u8>>>("", column)?
                            .map_or(0, |value| value.len()),
                    };
                    counts[index].1 = counts[index]
                        .1
                        .checked_add(bytes)
                        .ok_or_else(|| DbErr::Custom("transfer count overflow".into()))?;
                }
            }
        }
        if let Some((marker, mutation)) = self.between
            && sql.contains(marker)
            && !self.changed.load(Ordering::Acquire)
        {
            let result = self.tx.execute_unprepared(mutation).await?;
            if result.rows_affected() != 1 {
                return Err(DbErr::Custom("between-read mutation missed row".into()));
            }
            self.changed.store(true, Ordering::Release);
        }
        Ok(())
    }
}
#[async_trait::async_trait]
impl ConnectionTrait for Transfer<'_> {
    fn get_database_backend(&self) -> DatabaseBackend {
        DatabaseBackend::Sqlite
    }
    async fn execute(&self, _: Statement) -> std::result::Result<ExecResult, DbErr> {
        Err(DbErr::Custom("unexpected reader write".into()))
    }
    async fn execute_unprepared(&self, _: &str) -> std::result::Result<ExecResult, DbErr> {
        Err(DbErr::Custom("unexpected reader write".into()))
    }
    async fn query_one(
        &self,
        statement: Statement,
    ) -> std::result::Result<Option<QueryResult>, DbErr> {
        let sql = statement.sql.clone();
        let row = self.tx.query_one(statement).await?;
        self.observed(&sql, row.as_slice()).await?;
        Ok(row)
    }
    async fn query_all(
        &self,
        statement: Statement,
    ) -> std::result::Result<Vec<QueryResult>, DbErr> {
        let sql = statement.sql.clone();
        let rows = self.tx.query_all(statement).await?;
        self.observed(&sql, &rows).await?;
        Ok(rows)
    }
}
async fn charged(
    db: &impl ConnectionTrait,
    value: &Candidate,
    charge: &archive::Charge<'_>,
) -> Result<Option<Candidate>> {
    let r = &value.receipt;
    lookup_candidate_bounded(
        db,
        &r.installation_id,
        &r.tenant_id,
        &r.execution_id,
        charge,
    )
    .await
}

#[tokio::test]
async fn charged_receipt_scalar_distinguishes_absence_invalid_storage_and_boundary() -> Result<()> {
    use crate::storage::operation_budget::Error;
    let db = open().await?;
    let value = tests::candidate("installation", "tenant", "execution", &"p".repeat(128))?;
    ensure!(tests::commit(&db, &value).await?);
    for (mutation, missing, body_queries) in [
        (None, false, 1),
        (Some("DELETE FROM tenancy_execution_receipts"), true, 0),
        (
            Some("UPDATE tenancy_execution_receipts SET receipt_json=printf('%1048577s','x')"),
            false,
            0,
        ),
        (
            Some("UPDATE tenancy_execution_receipts SET principal_id=principal_id||'p'"),
            false,
            0,
        ),
        (
            Some("UPDATE tenancy_execution_receipts SET receipt_json=CAST(receipt_json AS BLOB)"),
            false,
            1,
        ),
    ] {
        let tx = db.begin().await?;
        let result: Result<()> = async {
            if let Some(sql) = mutation {
                ensure!(tx.execute_unprepared(sql).await?.rows_affected() == 1);
            }
            let reader = Transfer::new(&tx, None);
            let loaded = charged(&reader, &value, FREE).await;
            let seen = reader.counts()?;
            ensure!(seen[0].0 == body_queries && seen[1] == (0, 0));
            if mutation.is_none() {
                same_completion(&loaded?.context("boundary candidate missing")?, &value)?;
                ensure!(seen[0].1 == value.json.len());
            } else if missing {
                ensure!(loaded?.is_none() && seen[0].1 == 0);
            } else {
                ensure!(loaded.is_err() && seen[0].1 == 0);
                // Accepted fail-closed ordering: even an already-invalid row
                // may first encounter the caller's reservation refusal.
                let reader = Transfer::new(&tx, None);
                let error = charged(&reader, &value, &|_, _| Err(Error::Capacity.into()))
                    .await
                    .err()
                    .context("callback refusal accepted")?;
                ensure!(matches!(
                    error.downcast_ref::<Error>(),
                    Some(Error::Capacity)
                ));
                ensure!(reader.counts()? == [(0, 0); 2]);
            }
            Ok(())
        }
        .await;
        tx.rollback().await?;
        result?;
    }
    db.close().await?;
    Ok(())
}

#[tokio::test]
async fn charged_usage_metadata_covers_presence_and_corruption_before_body() -> Result<()> {
    let db = open().await?;
    let mut cases = vec![
        (false, false, None),
        (false, true, None),
        (true, false, None),
        (true, true, None),
    ];
    cases.extend(
        [
            "UPDATE tenancy_usage_evidence SET evidence_json=CAST(evidence_json AS BLOB)",
            "UPDATE tenancy_usage_evidence SET receipt_digest=printf('%064d',0)",
            "UPDATE tenancy_usage_evidence SET evidence_digest=printf('%064d',0)",
            "UPDATE tenancy_usage_evidence SET schema_version=1.5",
            "UPDATE tenancy_usage_evidence SET byte_count=1.5",
            "UPDATE tenancy_usage_evidence SET storage_kind=0.5",
            "UPDATE tenancy_usage_evidence SET schema_version=X'31'",
            "UPDATE tenancy_usage_evidence SET byte_count=CAST(byte_count AS BLOB)",
            "duplicate",
        ]
        .map(|sql| (true, true, Some(sql))),
    );
    for (expected, actual, mutation) in cases {
        let value = if expected {
            value()?
        } else {
            tests::candidate("installation", "tenant", "execution", "creator")?
        };
        let tx = db.begin().await?;
        let result: Result<()> = async {
            ensure!(insert(&tx, &value).await?);
            if expected && !actual { tx.execute_unprepared("DELETE FROM tenancy_usage_evidence").await?; }
            if !expected && actual { usage_evidence::storage::insert_fresh(&tx, &self::value()?).await?; }
            if matches!(mutation, Some("duplicate" | "UPDATE tenancy_usage_evidence SET storage_kind=0.5")) {
                // One real row without the marker CHECK, so the type case reaches
                // the reader. Only duplication adds a second row; rollback removes the relation.
                tx.execute_unprepared("CREATE TEMP TABLE tenancy_usage_evidence AS SELECT * FROM main.tenancy_usage_evidence").await?;
            }
            match mutation {
                Some("duplicate") => {
                    tx.execute_unprepared("INSERT INTO temp.tenancy_usage_evidence SELECT * FROM main.tenancy_usage_evidence").await?;
                }
                Some(sql) => { ensure!(tx.execute_unprepared(sql).await?.rows_affected() == 1); }
                None => {}
            }
            let reader = Transfer::new(&tx, None);
            let loaded = charged(&reader, &value, FREE).await;
            let seen = reader.counts()?;
            ensure!(seen[0] == (1, value.json.len()));
            if expected == actual && mutation.is_none() {
                same_completion(&loaded?.context("valid usage candidate missing")?, &value)?;
                let bytes = value.usage_evidence.as_ref().map_or(0, |e| e.json.len());
                ensure!(seen[1] == (usize::from(actual), bytes));
            } else {
                let error = loaded.err().context("invalid usage metadata accepted")?;
                ensure!(seen[1] == (0, 0), "metadata refusal transferred a usage body");
                if mutation == Some("duplicate") { ensure!(error.to_string().contains("multiplicity")); }
            }
            Ok(())
        }.await;
        tx.rollback().await?;
        result?;
        empty(&db).await?;
    }
    db.close().await?;
    Ok(())
}

#[tokio::test]
async fn charged_body_rechecks_between_scalar_and_payload_reads() -> Result<()> {
    let db = open().await?;
    let value = value()?;
    ensure!(tests::commit(&db, &value).await?);
    for (marker, mutation, usage, body_survives) in [
        (
            " AS \"receipt_bytes\"",
            "UPDATE tenancy_execution_receipts SET receipt_json=receipt_json||' '",
            false,
            false,
        ),
        (
            " AS \"receipt_bytes\"",
            "UPDATE tenancy_execution_receipts SET receipt_json=CAST(receipt_json AS BLOB)",
            false,
            false,
        ),
        (
            " AS inline_bytes",
            "UPDATE tenancy_usage_evidence SET evidence_json=evidence_json||' '",
            true,
            false,
        ),
        (
            " AS inline_bytes",
            "UPDATE tenancy_usage_evidence SET receipt_digest=printf('%064d',0)",
            true,
            true,
        ),
        (
            " AS inline_bytes",
            "UPDATE tenancy_usage_evidence SET byte_count=1.5",
            true,
            true,
        ),
    ] {
        let tx = db.begin().await?;
        let result: Result<()> = async {
            let reader = Transfer::new(&tx, Some((marker, mutation)));
            let loaded = charged(&reader, &value, FREE).await;
            ensure!(
                reader.changed.load(Ordering::Acquire),
                "mutation boundary was not reached"
            );
            ensure!(loaded.is_err());
            let seen = reader.counts()?;
            let index = usize::from(usage);
            ensure!(seen[index].0 == 1, "body guard was not exercised");
            if body_survives {
                ensure!(
                    seen[index].1
                        == value
                            .usage_evidence
                            .as_ref()
                            .context("captured usage")?
                            .json
                            .len()
                );
            } else {
                ensure!(
                    seen[index].1 == 0,
                    "changed body crossed the driver boundary"
                );
            }
            if !usage {
                ensure!(seen[1] == (0, 0));
            }
            Ok(())
        }
        .await;
        tx.rollback().await?;
        result?;
    }
    same_completion(&saved(&db, &value).await?, &value)?;
    db.close().await?;
    Ok(())
}

#[tokio::test]
async fn charged_lookup_preserves_logical_callbacks_and_refusal() -> Result<()> {
    use crate::storage::operation_budget::Error;
    use bitrouter_ai::tenancy::operation::ErrorCode;
    let db = open().await?;
    for captured in [false, true] {
        let value = if captured {
            value()?
        } else {
            tests::candidate("installation", "tenant", "execution", "creator")?
        };
        let r = &value.receipt;
        let bytes = value.json.len()
            + value.private_json.len()
            + 256
            + [
                r.installation_id.as_str(),
                r.tenant_id.as_str(),
                r.execution_id.as_str(),
                r.principal_id.as_str(),
                r.correlation_id.as_str(),
                value.digest.as_str(),
                r.recorded_at.as_str(),
            ]
            .iter()
            .map(|s| s.len())
            .sum::<usize>();
        let mut expected = vec![
            (1, 256),
            (0, 0),
            (1, 256),
            (1, bytes * 3),
            (0, 0),
            (1, 1024),
            (2, 2048),
            (0, 0),
            (2, 1024),
            (0, 0),
        ];
        if let Some(e) = &value.usage_evidence {
            expected.extend([(2, (e.json.len() + 128) * 3 + 4096), (0, 0)]);
        }
        expected.push((0, 0));
        for refuse in std::iter::once(None).chain((0..expected.len()).map(Some)) {
            let tx = db.begin().await?;
            let result: Result<()> = async {
                ensure!(insert(&tx, &value).await?);
                let calls = Mutex::new(Vec::new());
                let reader = Transfer::new(&tx, None);
                let loaded = charged(&reader, &value, &|rows, bytes| {
                    let mut seen = calls
                        .lock()
                        .map_err(|_| anyhow::anyhow!("callback trace poisoned"))?;
                    let index = seen.len();
                    seen.push((rows, bytes));
                    if refuse == Some(index) {
                        return Err(if rows == 0 {
                            Error::Rejected(ErrorCode::AuthorityUnavailable)
                        } else {
                            Error::Capacity
                        }
                        .into());
                    }
                    Ok(())
                })
                .await;
                let seen = calls
                    .into_inner()
                    .map_err(|_| anyhow::anyhow!("callback trace poisoned"))?;
                ensure!(
                    seen == expected[..refuse.map_or(expected.len(), |index| index + 1)],
                    "callback schedule: {seen:?}, expected {expected:?}"
                );
                match refuse {
                    None => {
                        same_completion(&loaded?.context("valid charged lookup missing")?, &value)?
                    }
                    Some(index) => {
                        let error = loaded.err().context("failed logical callback accepted")?;
                        match (expected[index].0, error.downcast_ref::<Error>()) {
                            (0, Some(Error::Rejected(ErrorCode::AuthorityUnavailable))) => {}
                            (rows, Some(Error::Capacity)) if rows > 0 => {}
                            _ => anyhow::bail!("callback failure changed cause"),
                        }
                    }
                }
                let body = reader.counts()?;
                // Debit indices 3 and 10 precede the respective body SELECTs;
                // later callback failures must not pretend those reads vanished.
                let receipt_read = refuse.is_none_or(|index| index > 3);
                let usage_read = captured && refuse.is_none_or(|index| index > 10);
                ensure!(
                    body[0]
                        == (
                            usize::from(receipt_read),
                            if receipt_read { value.json.len() } else { 0 }
                        )
                );
                ensure!(
                    body[1]
                        == (
                            usize::from(usage_read),
                            if usage_read {
                                value
                                    .usage_evidence
                                    .as_ref()
                                    .context("usage trace")?
                                    .json
                                    .len()
                            } else {
                                0
                            }
                        )
                );
                Ok(())
            }
            .await;
            tx.rollback().await?;
            result?;
            empty(&db).await?;
        }
    }
    db.close().await?;
    Ok(())
}

#[tokio::test]
async fn fresh_stage_checks_full_archive_scope_and_existing_canonical_rows() -> Result<()> {
    let db = open().await?;
    let expected = value()?;
    for (installation, tenant) in [
        ("installation", "tenant"),
        ("installation", "other"),
        ("other", "tenant"),
    ] {
        let tx = db.begin().await?;
        let result: Result<()> = async {
            let archived = tests::candidate(installation, tenant, "execution", "creator")?
                .with_usage_evidence(usage_evidence::Snapshot::default())?;
            ensure!(insert(&tx, &archived).await?);
            let prepared = archive::Prepared::new(&archived, FREE)?;
            ensure!(archive::archive_one(&tx, &prepared, FREE).await?);
            ensure!(
                tx.execute_unprepared("DELETE FROM tenancy_execution_receipts")
                    .await?
                    .rows_affected()
                    == 1
            );
            ensure!(
                tx.execute_unprepared("DELETE FROM tenancy_usage_evidence")
                    .await?
                    .rows_affected()
                    == 1
            );
            ensure!(count(&tx, "tenancy_receipt_archive").await? == 1);
            let stage = stage_terminal(&tx, &expected).await;
            if installation == "installation" && tenant == "tenant" {
                let error = stage.err().context("same-key orphan archive accepted")?;
                ensure!(
                    error
                        .to_string()
                        .contains("orphan or duplicate receipt archive")
                );
                ensure!(
                    count(&tx, "tenancy_execution_receipts").await? == 0
                        && count(&tx, "tenancy_usage_evidence").await? == 0,
                    "orphan refusal followed a write"
                );
            } else {
                let staged = stage?;
                ensure!(staged.inserted());
                ensure!(staged.reconcile(FREE).await?.delta() == 0);
                same_completion(&saved(&tx, &expected).await?, &expected)?;
            }
            ensure!(count(&tx, "tenancy_receipt_archive").await? == 1);
            Ok(())
        }
        .await;
        tx.rollback().await?;
        result?;
        empty(&db).await?;
    }
    let tx = db.begin().await?;
    let result: Result<()> = async {
        ensure!(insert(&tx, &expected).await?);
        ensure!(
            tx.execute_unprepared("UPDATE tenancy_execution_receipts SET receipt_json='{'")
                .await?
                .rows_affected()
                == 1
        );
        let error = stage_terminal(&tx, &expected)
            .await
            .err()
            .context("existing corrupt receipt bypassed canonical replay")?;
        ensure!(
            error.downcast_ref::<serde_json::Error>().is_some(),
            "existing receipt did not reach canonical JSON decoding"
        );
        ensure!(
            count(&tx, "tenancy_execution_receipts").await? == 1
                && count(&tx, "tenancy_usage_evidence").await? == 1
        );
        Ok(())
    }
    .await;
    tx.rollback().await?;
    result?;
    empty(&db).await?;
    db.close().await?;
    Ok(())
}
