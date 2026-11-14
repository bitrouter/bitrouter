//! Real SQLite ownership primitives on the minimal ordinary main target. These
//! fixtures do not install or claim a complete Core/authority schema.
use super::*;
use anyhow::Context;
use sea_orm::{ConnectionTrait, DatabaseBackend, Statement};

fn owned_control() -> (Arc<Proof>, Arc<Control>) {
    let proof = Proof::new(Arc::new(AtomicBool::new(true)));
    let control = Arc::new(Control::new(proof.clone()));
    (proof, control)
}

async fn minimal_target(db: &DatabaseConnection) -> Result<()> {
    for sql in [
        "CREATE TABLE tenancy_installation(singleton INTEGER PRIMARY KEY, revision INTEGER NOT NULL)",
        "INSERT INTO tenancy_installation(singleton,revision) VALUES(1,7)",
        "CREATE TABLE intent_trigger_rows(value INTEGER NOT NULL)",
        "CREATE TRIGGER intent_updates AFTER UPDATE ON tenancy_installation BEGIN INSERT INTO intent_trigger_rows(value) VALUES(NEW.revision); END",
    ] {
        db.execute_unprepared(sql).await?;
    }
    Ok(())
}

async fn scalar(db: &impl ConnectionTrait, sql: &str) -> Result<i64> {
    Ok(db
        .query_one(Statement::from_string(DatabaseBackend::Sqlite, sql))
        .await?
        .context("intent fixture scalar missing")?
        .try_get("", "value")?)
}

#[tokio::test]
async fn owned_zero_row_intent_is_same_transaction_no_row_trigger_and_preserves_rollback()
-> Result<()> {
    for mode in ["write", "repeatable", "receipt"] {
        let db = crate::db::connect("sqlite::memory:").await?;
        minimal_target(&db).await?;
        db.execute_unprepared("CREATE TEMP TABLE tenancy_installation(singleton INTEGER PRIMARY KEY, revision INTEGER NOT NULL)").await?;
        db.execute_unprepared("INSERT INTO temp.tenancy_installation VALUES(1,99)")
            .await?;
        let before = scalar(&db, "SELECT total_changes() AS value").await?;
        let (proof, control) = owned_control();
        let tx = match mode {
            "write" => control.begin_write(&db).await?,
            "repeatable" => control.begin_write_repeatable_read(&db).await?,
            _ => control.begin_write_receipt(&db).await?,
        };
        let checked: Result<()> = async {
            ensure!(proof.write_intent_counts_for_test() == (1, 1));
            proof.wait_write_intent_entered_for_test(0).await?;
            proof.wait_write_intent_acquired_for_test(0).await?;
            ensure!(scalar(&tx, "SELECT changes() AS value").await? == 0);
            ensure!(scalar(&tx, "SELECT total_changes() AS value").await? == before);
            ensure!(
                scalar(
                    &tx,
                    "SELECT revision AS value FROM main.tenancy_installation"
                )
                .await?
                    == 7
            );
            ensure!(
                scalar(
                    &tx,
                    "SELECT revision AS value FROM temp.tenancy_installation"
                )
                .await?
                    == 99
            );
            ensure!(scalar(&tx, "SELECT count(*) AS value FROM intent_trigger_rows").await? == 0);
            ensure!(
                tx.execute_unprepared(
                    "UPDATE main.tenancy_installation SET revision=8 WHERE singleton=1"
                )
                .await?
                .rows_affected()
                    == 1
            );
            Ok(())
        }
        .await;
        let settled = if checked.is_ok() {
            control.commit(tx).await
        } else {
            control.rollback(tx).await
        };
        let verified: Result<()> = async {
            checked?;
            settled?;
            ensure!(proof.disposition() == Disposition::Committed);
            ensure!(
                scalar(
                    &db,
                    "SELECT revision AS value FROM main.tenancy_installation"
                )
                .await?
                    == 8
            );
            ensure!(scalar(&db, "SELECT count(*) AS value FROM intent_trigger_rows").await? == 1);
            let tx = control.begin_write(&db).await?;
            let updated = tx
                .execute_unprepared(
                    "UPDATE main.tenancy_installation SET revision=9 WHERE singleton=1",
                )
                .await;
            let rolled_back = control.rollback(tx).await;
            updated?;
            rolled_back?;
            ensure!(proof.disposition() == Disposition::RolledBack);
            ensure!(
                scalar(
                    &db,
                    "SELECT revision AS value FROM main.tenancy_installation"
                )
                .await?
                    == 8
            );
            ensure!(scalar(&db, "SELECT count(*) AS value FROM intent_trigger_rows").await? == 1);
            ensure!(
                scalar(
                    &db,
                    "SELECT revision AS value FROM temp.tenancy_installation"
                )
                .await?
                    == 99
            );
            Ok(())
        }
        .await;
        let closed = db.close().await;
        verified.with_context(|| format!("intent family {mode}"))?;
        closed?;
    }
    Ok(())
}

#[tokio::test]
async fn generic_minimal_begin_stays_deferred_and_post_begin_receipt_error_must_rollback()
-> Result<()> {
    let db = crate::db::connect("sqlite::memory:").await?;
    let (proof, control) = owned_control();
    let tx = control.begin(&db).await?;
    ensure!(scalar(&tx, "SELECT 1 AS value").await? == 1);
    control.rollback(tx).await?;
    ensure!(proof.write_intent_counts_for_test() == (0, 0));
    let result = control.begin_write_receipt(&db).await;
    let checked: Result<()> = async {
        let error = result
            .err()
            .context("missing actual intent target was accepted")?;
        let sql = error
            .downcast_ref::<DbErr>()
            .context("original typed SQL error lost")?;
        ensure!(!matches!(sql, DbErr::ConnectionAcquire(_)));
        ensure!(proof.disposition() == Disposition::RolledBack);
        ensure!(proof.sql_counts_for_test() == (2, 2));
        ensure!(proof.write_intent_counts_for_test() == (1, 0));
        proof.ensure_clean()?;
        Ok(())
    }
    .await;
    let closed = db.clone().close().await;
    checked?;
    closed?;
    let failure = control.begin_write_receipt(&db).await;
    let error = failure.err().context("closed pool was accepted")?;
    ensure!(matches!(
        error.downcast_ref::<DbErr>(),
        Some(DbErr::ConnectionAcquire(_))
    ));
    ensure!(proof.disposition() == Disposition::NoTransaction);
    ensure!(proof.sql_counts_for_test() == (3, 2));
    ensure!(proof.write_intent_counts_for_test() == (1, 0));
    Ok(())
}

#[tokio::test]
async fn readonly_connection_keeps_generic_read_and_refuses_owned_write_with_rollback() -> Result<()>
{
    let db = crate::db::connect("sqlite::memory:").await?;
    minimal_target(&db).await?;
    db.execute_unprepared("PRAGMA query_only=ON").await?;
    let (proof, control) = owned_control();
    let checked: Result<()> = async {
        let tx = control.begin_repeatable_read(&db).await?;
        let read = scalar(
            &tx,
            "SELECT revision AS value FROM main.tenancy_installation",
        )
        .await;
        let rolled_back = control.rollback(tx).await;
        ensure!(read? == 7);
        rolled_back?;
        ensure!(proof.write_intent_counts_for_test() == (0, 0));
        let result = control.begin_write(&db).await;
        let error = result
            .err()
            .context("SQLite query_only writer was accepted")?;
        ensure!(error.downcast_ref::<DbErr>().is_some());
        ensure!(proof.disposition() == Disposition::RolledBack);
        ensure!(proof.sql_counts_for_test() == (2, 2));
        ensure!(proof.write_intent_counts_for_test() == (1, 0));
        ensure!(
            scalar(
                &db,
                "SELECT revision AS value FROM main.tenancy_installation"
            )
            .await?
                == 7
        );
        ensure!(scalar(&db, "SELECT count(*) AS value FROM intent_trigger_rows").await? == 0);
        Ok(())
    }
    .await;
    let closed = db.close().await;
    checked?;
    closed?;
    Ok(())
}

#[tokio::test]
async fn stopped_after_begin_rolls_back_before_polling_intent_or_returning_body() -> Result<()> {
    let db = crate::db::connect("sqlite::memory:").await?;
    minimal_target(&db).await?;
    let (proof, control) = owned_control();
    let gate = proof.hold(Phase::Active);
    let c = control.clone();
    let database = db.clone();
    let work = tokio::spawn(async move { c.begin_write(&database).await });
    let entered =
        tokio::time::timeout(super::super::MANAGEMENT_TIMEOUT, gate.entered.notified()).await;
    control.stop_read();
    let cleanup = control.progress().borrow().cleanup_deadline;
    proof.release_gate();
    let joined = work.await;
    let checked: Result<()> = async {
        entered?;
        let error = joined?
            .err()
            .context("stopped intent returned a body transaction")?;
        ensure!(error.downcast_ref::<ReadExpired>().is_some());
        ensure!(proof.disposition() == Disposition::RolledBack);
        ensure!(proof.sql_counts_for_test() == (1, 1));
        ensure!(proof.write_intent_counts_for_test() == (0, 0));
        ensure!(cleanup.is_some());
        ensure!(scalar(&db, "SELECT count(*) AS value FROM intent_trigger_rows").await? == 0);
        Ok(())
    }
    .await;
    let closed = db.close().await;
    checked?;
    closed?;
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn stop_during_real_writer_contention_retains_transaction_until_acknowledged_rollback()
-> Result<()> {
    use sea_orm::sqlx::{
        Connection as _, Executor as _, SqliteConnection, sqlite::SqliteConnectOptions,
    };
    let home = tempfile::tempdir()?;
    let path = home.path().join("intent.sqlite");
    let owner = crate::tenancy::ownership::acquire_path(&path, true)?;
    let db = crate::db::connect_tenancy(&format!("sqlite://{}?mode=rwc", path.display()), &owner)
        .await?;
    minimal_target(&db).await?;
    let mut writer = SqliteConnection::connect_with(
        &SqliteConnectOptions::new()
            .filename(&owner.path)
            .create_if_missing(false),
    )
    .await?;
    writer.execute("BEGIN IMMEDIATE").await?;
    let (proof, control) = owned_control();
    let c = control.clone();
    let database = db.clone();
    let work = tokio::spawn(async move { c.begin_write_receipt(&database).await });
    let entered = tokio::time::timeout(
        super::super::MANAGEMENT_TIMEOUT,
        proof.wait_write_intent_entered_for_test(0),
    )
    .await;
    control.stop_read();
    let released = writer.execute("ROLLBACK").await;
    let joined = work.await;
    let checked: Result<()> = async {
        entered??;
        released?;
        let error = joined?
            .err()
            .context("cancelled contender escaped into body")?;
        ensure!(error.downcast_ref::<ReadExpired>().is_some());
        ensure!(proof.disposition() == Disposition::RolledBack);
        ensure!(proof.sql_counts_for_test() == (1, 1));
        ensure!(proof.write_intent_counts_for_test() == (1, 1));
        proof.wait_write_intent_acquired_for_test(0).await?;
        ensure!(
            scalar(
                &db,
                "SELECT revision AS value FROM main.tenancy_installation"
            )
            .await?
                == 7
        );
        Ok(())
    }
    .await;
    let writer_closed = writer.close().await;
    let db_closed = db.close().await;
    checked?;
    writer_closed?;
    db_closed?;
    drop(owner);
    Ok(())
}

#[tokio::test]
async fn intent_error_rollback_unwind_never_becomes_drop_ack_or_no_transaction() -> Result<()> {
    use futures::FutureExt;
    let db = crate::db::connect("sqlite::memory:").await?;
    minimal_target(&db).await?;
    db.execute_unprepared("PRAGMA query_only=ON").await?;
    let (proof, control) = owned_control();
    let gate = proof.hold(Phase::RollingBack);
    let c = control.clone();
    let database = db.clone();
    let work = tokio::spawn(async move {
        std::panic::AssertUnwindSafe(c.begin_write_receipt(&database))
            .catch_unwind()
            .await
    });
    let entered =
        tokio::time::timeout(super::super::MANAGEMENT_TIMEOUT, gate.entered.notified()).await;
    let pending = proof.disposition() == Disposition::Pending;
    let injected = proof.unwind_gate();
    let joined = work.await;
    control.conclude();
    let closed = db.close().await;
    entered?;
    injected?;
    ensure!(pending && joined?.is_err());
    ensure!(proof.disposition() == Disposition::Ambiguous);
    ensure!(proof.sql_counts_for_test() == (1, 0));
    ensure!(!proof.live.load(Ordering::Acquire));
    closed?;
    ensure!(
        proof.ensure_clean().is_err(),
        "pool closure was mistaken for rollback ACK"
    );
    Ok(())
}
