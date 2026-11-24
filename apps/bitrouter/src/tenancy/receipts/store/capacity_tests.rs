//! Durable policy quantities agree with full audit across upgrades and writes.
use super::tests::{candidate, commit};
use super::*;
use crate::db::{migration::capacity, store_format};
use sea_orm::{DatabaseConnection, DbErr, ExecResult, Statement, TransactionTrait};

async fn open() -> Result<DatabaseConnection> {
    let db = crate::db::connect("sqlite::memory:").await?;
    crate::db::run_migrations(&db).await?;
    db.execute(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
        "INSERT INTO tenancy_installation(singleton,installation_id,schema_version,authority_revision,operation_ids,safety_operation_ids,saturated,created_at) VALUES(1,'installation',?,0,0,0,0,'2026-10-25T00:00:00Z')",
        [crate::tenancy::AUTHORITY_SCHEMA_VERSION.into()])).await?;
    Ok(db)
}

fn quantities(v: &Inventory) -> (u64, u64, u64, u64, u64, u64) {
    (
        v.live_bytes,
        v.history_bytes,
        v.history_rows,
        v.oauth_bytes,
        v.technical_bytes,
        v.unknown_count,
    )
}
async fn matches_audit(db: &impl ConnectionTrait) -> Result<()> {
    let audit = inventory(db, &"installation".try_into()?).await?;
    capacity::verify(db, &audit).await?;
    let durable = capacity::load(db).await?;
    ensure!(
        quantities(&audit) == quantities(&durable),
        "durable policy quantities differ from audit"
    );
    Ok(())
}
fn unknown(id: &str) -> Result<Candidate> {
    let mut receipt = candidate("installation", "tenant", id, "creator")?.receipt;
    receipt.state = ReceiptState::Unknown;
    receipt.outcome = bitrouter_ai::tenancy::execution::Outcome::Unknown;
    Candidate::new(
        receipt,
        Disposition {
            dispatch: DispatchState::Unknown,
            statuses: vec![],
            auxiliary_digest: None,
        },
    )
}
fn auxiliary_candidate(id: &str) -> Result<Candidate> {
    use crate::tenancy::receipts::auxiliary::{
        Snapshot,
        count::{Envelope, Fact, Outcome},
    };
    use bitrouter_ai::tenancy::{
        authority::{OwnerKind, ResourceKind},
        configuration::ResourceRef,
    };
    candidate("installation", "tenant", id, "creator")?.with_auxiliary(Snapshot::Counts(Envelope {
        version: 2,
        counts: vec![Fact {
            ordinal: 0,
            work: "c53bbdf9-7771-41db-88af-a5017795a020".into(),
            credential: ResourceRef {
                owner_kind: OwnerKind::Tenant,
                owner_id: "tenant".try_into()?,
                resource_kind: ResourceKind::Credential,
                resource_id: "credential".try_into()?,
                revision_id: "r1".try_into()?,
            },
            candidate: "01".repeat(32),
            prompt: "02".repeat(32),
            generation_request: Some("03".repeat(32)),
            count_request: Some("04".repeat(32)),
            http_status: Some(200),
            outcome: Outcome::Counted,
            input_tokens: Some(11),
        }],
    }))
}
async fn archive_value(db: &DatabaseConnection, value: &Candidate) -> Result<()> {
    let prepared = archive::Prepared::new(value, &|_, _| Ok(()))?;
    let tx = db.begin().await?;
    ensure!(archive::archive_one(&tx, &prepared, &|_, _| Ok(())).await?);
    tx.commit().await?;
    Ok(())
}
async fn legacy_marker(db: &DatabaseConnection) -> Result<()> {
    let tx = db.begin().await?;
    tx.execute_unprepared("DROP TABLE tenancy_receipt_capacity")
        .await?;
    tx.execute(Statement::from_sql_and_values(
        DatabaseBackend::Sqlite,
        "UPDATE seaql_migrations SET version=?",
        [store_format::LEGACY_GATEWAY_SQLITE.into()],
    ))
    .await?;
    tx.commit().await?;
    ensure!(store_format::inspect(db).await? == store_format::State::Legacy);
    Ok(())
}

#[tokio::test]
async fn policy_projection_tracks_first_write_replay_rollback_and_auxiliary_archive() -> Result<()>
{
    let db = open().await?;
    let known = auxiliary_candidate("known")?;
    let pending = unknown("pending")?;
    ensure!(commit(&db, &known).await? && commit(&db, &pending).await?);
    matches_audit(&db).await?;
    let before = capacity::load(&db).await?;
    ensure!(before.live_bytes > 0 && before.unknown_count == 1);
    ensure!(!commit(&db, &known).await? && !commit(&db, &pending).await?);
    ensure!(quantities(&before) == quantities(&capacity::load(&db).await?));
    let rolled = auxiliary_candidate("rolled")?;
    let tx = db.begin().await?;
    insert(&tx, &rolled).await?;
    super::super::auxiliary_store::insert(&tx, &rolled).await?;
    tx.rollback().await?;
    ensure!(quantities(&before) == quantities(&capacity::load(&db).await?));
    archive_value(&db, &known).await?;
    matches_audit(&db).await?;
    let after = capacity::load(&db).await?;
    ensure!(
        after.history_rows == 1
            && after.history_bytes > 0
            && after.live_bytes < before.live_bytes
            && after.unknown_count == 1
    );
    let prepared = archive::Prepared::new(&known, &|_, _| Ok(()))?;
    let tx = db.begin().await?;
    ensure!(!archive::archive_one(&tx, &prepared, &|_, _| Ok(())).await?);
    tx.commit().await?;
    ensure!(quantities(&after) == quantities(&capacity::load(&db).await?));
    db.close().await?;
    Ok(())
}

#[tokio::test]
async fn legacy_upgrade_backfills_once_and_reopen_does_not_decode_history() -> Result<()> {
    let db = open().await?;
    let known = auxiliary_candidate("archived")?;
    ensure!(commit(&db, &known).await?);
    archive_value(&db, &known).await?;
    ensure!(commit(&db, &unknown("pending")?).await?);
    let before = capacity::load(&db).await?;
    legacy_marker(&db).await?;
    crate::db::run_migrations(&db).await?;
    ensure!(store_format::inspect(&db).await? == store_format::State::Current);
    ensure!(quantities(&before) == quantities(&capacity::load(&db).await?));
    matches_audit(&db).await?;
    // Reopening the upgraded schema must not decode even a malformed historical
    // body. Explicit audit still finds the damage.
    db.execute_unprepared(
        "UPDATE tenancy_execution_receipts SET receipt_json='{' WHERE execution_id='pending'",
    )
    .await?;
    crate::db::run_migrations(&db).await?;
    ensure!(quantities(&before) == quantities(&capacity::load(&db).await?));
    ensure!(inventory(&db, &"installation".try_into()?).await.is_err());
    db.close().await?;
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn committed_terminal_with_lost_ack_survives_reopen_without_replay_charge() -> Result<()> {
    use crate::tenancy::{
        ownership,
        transaction::{Control, Disposition as TransactionDisposition, Proof},
    };
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };

    let directory = tempfile::tempdir()?;
    let path = directory.path().join("lost-commit-ack.sqlite");
    let known = auxiliary_candidate("committed-known")?;
    let pending = unknown("committed-unknown")?;
    let expected;
    {
        let owner = ownership::acquire_path(&path, true)?;
        let db =
            crate::db::connect_tenancy(&format!("sqlite://{}?mode=rwc", path.display()), &owner)
                .await?;
        crate::db::run_migrations(&db).await?;
        db.execute(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
            "INSERT INTO tenancy_installation(singleton,installation_id,schema_version,authority_revision,operation_ids,safety_operation_ids,saturated,created_at) VALUES(1,'installation',?,0,0,0,0,'2026-10-25T00:00:00Z')",
            [crate::tenancy::AUTHORITY_SCHEMA_VERSION.into()])).await?;
        let live = Arc::new(AtomicBool::new(true));
        let proof = Proof::new(live.clone());
        let control = Control::new(proof.clone());
        let tx = control.begin_write(&db).await?;
        let mut changes = Vec::new();
        for value in [&known, &pending] {
            let staged = stage_terminal(&tx, value).await?;
            ensure!(staged.inserted());
            super::super::auxiliary_store::insert(&tx, value).await?;
            changes.push(staged.reconcile(&|_, _| Ok(())).await?);
        }
        matches_audit(&tx).await?;
        expected = quantities(&capacity::load(&tx).await?);
        ensure!(expected.0 > 0 && expected.5 == 1);
        proof.lose_next_commit_ack_for_test();
        let error = control
            .commit(tx)
            .await
            .err()
            .context("successful SQL COMMIT must lose its acknowledgement")?;
        ensure!(
            error.downcast_ref::<DbErr>().is_some_and(|error| {
                matches!(error, DbErr::Custom(message)
                    if message == "commit acknowledgement lost by test transport")
            }),
            "failure must come from the post-COMMIT transport hook: {error:#}"
        );
        control.conclude();
        ensure!(proof.disposition() == TransactionDisposition::Ambiguous);
        ensure!(!live.load(Ordering::Acquire));
        ensure!(proof.ensure_clean().is_err());
        ensure!(ownership::acquire_path(&path, false).is_err());
        db.close().await?;
        drop(changes);
        drop(control);
        drop(proof);
        drop(owner);
    }

    // Dropping the original fixture models process death; this is a new owner
    // and pool, not an actor process-kill or acknowledgement-retention test.
    let owner = ownership::acquire_path(&path, false)?;
    let db =
        crate::db::connect_tenancy(&format!("sqlite://{}?mode=rw", path.display()), &owner).await?;
    crate::db::run_migrations(&db).await?;
    for value in [&known, &pending] {
        let saved = lookup_candidate(
            &db,
            &value.receipt.installation_id,
            &value.receipt.tenant_id,
            &value.receipt.execution_id,
        )
        .await?
        .context("real COMMIT must persist receipt and bound evidence despite lost ACK")?;
        same_completion(&saved, value)?;
    }
    for (table, count) in [
        ("tenancy_execution_receipts", 2),
        ("tenancy_usage_evidence", 2),
        ("tenancy_receipt_auxiliary", 1),
        ("tenancy_unknown_obligations", 1),
    ] {
        let row = db
            .query_one(Statement::from_string(
                DatabaseBackend::Sqlite,
                format!("SELECT COUNT(*) AS n FROM {table}"),
            ))
            .await?
            .context("durable source count")?;
        ensure!(row.try_get::<i64>("", "n")? == count);
    }
    matches_audit(&db).await?;
    ensure!(quantities(&capacity::load(&db).await?) == expected);

    let live = Arc::new(AtomicBool::new(true));
    let proof = Proof::new(live.clone());
    let control = Control::new(proof.clone());
    let tx = control.begin_write(&db).await?;
    let mut changes = Vec::new();
    for value in [&known, &pending] {
        let staged = stage_terminal(&tx, value).await?;
        ensure!(
            !staged.inserted(),
            "committed execution must replay exactly"
        );
        super::super::auxiliary_store::insert(&tx, value).await?;
        let change = staged.reconcile(&|_, _| Ok(())).await?;
        ensure!(change.delta() == 0);
        changes.push(change);
    }
    control.commit(tx).await?;
    ensure!(proof.disposition() == TransactionDisposition::Committed);
    ensure!(live.load(Ordering::Acquire));
    proof.ensure_clean()?;
    drop(changes);
    matches_audit(&db).await?;
    ensure!(quantities(&capacity::load(&db).await?) == expected);
    db.close().await?;
    Ok(())
}

#[tokio::test]
async fn failed_upgrade_rolls_back_schema_and_marker_and_can_retry() -> Result<()> {
    let db = open().await?;
    let value = candidate("installation", "tenant", "execution", "creator")?;
    ensure!(commit(&db, &value).await?);
    legacy_marker(&db).await?;
    db.execute_unprepared("UPDATE tenancy_execution_receipts SET content_digest='bad'")
        .await?;
    ensure!(crate::db::run_migrations(&db).await.is_err());
    ensure!(
        !sea_orm_migration::SchemaManager::new(&db)
            .has_table(capacity::TABLE)
            .await?
    );
    ensure!(store_format::inspect(&db).await? == store_format::State::Legacy);
    db.execute(Statement::from_sql_and_values(
        DatabaseBackend::Sqlite,
        "UPDATE tenancy_execution_receipts SET content_digest=?",
        [value.digest.as_str().into()],
    ))
    .await?;
    crate::db::run_migrations(&db).await?;
    matches_audit(&db).await?;
    db.close().await?;
    Ok(())
}

#[tokio::test]
async fn missing_current_projection_never_reinitializes_and_overflow_rolls_back_source()
-> Result<()> {
    let db = open().await?;
    db.execute_unprepared("UPDATE tenancy_receipt_capacity SET live_bytes=9223372036854775807")
        .await?;
    let value = candidate("installation", "tenant", "execution", "creator")?;
    ensure!(commit(&db, &value).await.is_err());
    ensure!(
        lookup_candidate(
            &db,
            &value.receipt.installation_id,
            &value.receipt.tenant_id,
            &value.receipt.execution_id
        )
        .await?
        .is_none()
    );
    db.execute_unprepared("DELETE FROM tenancy_receipt_capacity")
        .await?;
    crate::db::run_migrations(&db).await?;
    ensure!(capacity::load(&db).await.is_err());
    ensure!(commit(&db, &value).await.is_err());
    ensure!(
        lookup_candidate(
            &db,
            &value.receipt.installation_id,
            &value.receipt.tenant_id,
            &value.receipt.execution_id
        )
        .await?
        .is_none()
    );
    db.close().await?;
    Ok(())
}

#[tokio::test]
async fn legacy_sources_without_identity_and_foreign_installations_cannot_be_zeroed() -> Result<()>
{
    for foreign in [false, true] {
        let db = open().await?;
        ensure!(
            commit(
                &db,
                &candidate("installation", "tenant", "execution", "creator")?
            )
            .await?
        );
        if foreign {
            db.execute_unprepared("UPDATE tenancy_installation SET installation_id='foreign'")
                .await?;
        } else {
            db.execute_unprepared("DELETE FROM tenancy_installation")
                .await?;
        }
        legacy_marker(&db).await?;
        ensure!(crate::db::run_migrations(&db).await.is_err());
        ensure!(
            !sea_orm_migration::SchemaManager::new(&db)
                .has_table(capacity::TABLE)
                .await?
        );
        db.close().await?;
    }
    Ok(())
}

// Deny source SQL at the actual SeaORM connection boundary: a table COUNT/SUM
// cannot masquerade as a cheap projection read even when all JSON is valid.
struct ProjectionOnly<'a>(&'a DatabaseConnection);
impl ProjectionOnly<'_> {
    fn check(stmt: &Statement) -> std::result::Result<(), DbErr> {
        if !stmt.sql.contains("tenancy_receipt_capacity")
            || stmt.sql.to_ascii_lowercase().contains("sum(")
            || stmt.sql.to_ascii_lowercase().contains("count(")
        {
            return Err(DbErr::Custom("historical SQL forbidden".into()));
        }
        Ok(())
    }
}
#[async_trait::async_trait]
impl ConnectionTrait for ProjectionOnly<'_> {
    fn get_database_backend(&self) -> DatabaseBackend {
        self.0.get_database_backend()
    }
    async fn execute(&self, _: Statement) -> std::result::Result<ExecResult, DbErr> {
        Err(DbErr::Custom("read only".into()))
    }
    async fn execute_unprepared(&self, _: &str) -> std::result::Result<ExecResult, DbErr> {
        Err(DbErr::Custom("read only".into()))
    }
    async fn query_one(&self, stmt: Statement) -> std::result::Result<Option<QueryResult>, DbErr> {
        Self::check(&stmt)?;
        self.0.query_one(stmt).await
    }
    async fn query_all(&self, stmt: Statement) -> std::result::Result<Vec<QueryResult>, DbErr> {
        Self::check(&stmt)?;
        self.0.query_all(stmt).await
    }
}

#[tokio::test]
async fn small_and_large_histories_load_only_the_projection() -> Result<()> {
    for size in [1, 129] {
        let db = open().await?;
        for i in 0..size {
            ensure!(
                commit(
                    &db,
                    &candidate(
                        "installation",
                        "tenant",
                        &format!("execution-{i:04}"),
                        "creator"
                    )?
                )
                .await?
            );
        }
        let audit = inventory(&db, &"installation".try_into()?).await?;
        let guarded = ProjectionOnly(&db);
        ensure!(quantities(&capacity::load(&guarded).await?) == quantities(&audit));
        ensure!(
            inventory(&guarded, &"installation".try_into()?)
                .await
                .is_err()
        );
        db.close().await?;
    }
    Ok(())
}

#[tokio::test]
async fn epoch_requirement_is_transactional_and_survives_legacy_upgrade() -> Result<()> {
    let db = open().await?;
    ensure!(!capacity::epoch_required(&db).await?);
    let tx = db.begin().await?;
    capacity::mark_epoch_required(&tx).await?;
    ensure!(capacity::epoch_required(&tx).await?);
    tx.rollback().await?;
    ensure!(!capacity::epoch_required(&db).await?);
    db.execute_unprepared("INSERT INTO tenancy_continuation_epoch(singleton,installation_id,key_id,created_at) VALUES(1,'installation','key','2026-10-25T00:00:00Z')").await?;
    legacy_marker(&db).await?;
    crate::db::run_migrations(&db).await?;
    ensure!(capacity::epoch_required(&db).await?);
    db.execute_unprepared("DELETE FROM tenancy_continuation_epoch")
        .await?;
    crate::db::run_migrations(&db).await?;
    ensure!(capacity::epoch_required(&db).await?);
    db.close().await?;
    Ok(())
}

#[tokio::test]
async fn explicit_audit_detects_positive_underestimate_and_each_policy_projection_mismatch()
-> Result<()> {
    let db = open().await?;
    ensure!(commit(&db, &unknown("pending")?).await?);
    let known = auxiliary_candidate("known")?;
    ensure!(commit(&db, &known).await?);
    archive_value(&db, &known).await?;
    let audit = inventory(&db, &"installation".try_into()?).await?;
    capacity::verify(&db, &audit).await?;
    for (field, value) in [
        ("live_bytes", 1),
        ("history_bytes", 1),
        ("history_rows", 0),
        ("oauth_bytes", 1),
        ("technical_bytes", 1),
        ("unknown_count", 0),
    ] {
        let tx = db.begin().await?;
        let update = Query::update()
            .table(Alias::new(capacity::TABLE))
            .value(Alias::new(field), value)
            .to_owned();
        tx.execute(DatabaseBackend::Sqlite.build(&update)).await?;
        let recomputed = inventory(&tx, &"installation".try_into()?).await?;
        ensure!(
            capacity::verify(&tx, &recomputed).await.is_err(),
            "accepted incorrect {field}"
        );
        tx.rollback().await?;
    }
    matches_audit(&db).await?;
    db.close().await?;
    Ok(())
}
