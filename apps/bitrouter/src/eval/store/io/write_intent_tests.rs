//! Real SQLite ownership primitives on the minimal ordinary Eval target.
//! This is not a fully migrated tenancy/actor or financial fixture.
use super::*;
use crate::storage::write::Write;
use sea_orm::sqlx::{Connection as _, SqliteConnection, sqlite::SqliteConnectOptions};

async fn database(
    path: &std::path::Path,
) -> Result<(sea_orm::DatabaseConnection, SqliteConnection)> {
    let db = crate::db::connect(&format!("sqlite://{}?mode=rwc", path.display())).await?;
    let peer = async {
    // eval_subjects.eval_id is an ordinary required table/column from original
    // migration000010 onward. Deliberately no tenancy table or marker exists.
    for statement in [
        "CREATE TABLE eval_subjects(eval_id TEXT NOT NULL PRIMARY KEY)",
        "INSERT INTO eval_subjects VALUES ('original')",
        "CREATE TABLE eval_update_observations(value INTEGER NOT NULL)",
        "CREATE TRIGGER eval_updated AFTER UPDATE ON eval_subjects BEGIN INSERT INTO eval_update_observations VALUES (1); END",
        "CREATE TEMP TABLE eval_subjects(eval_id TEXT NOT NULL)",
        "INSERT INTO temp.eval_subjects VALUES ('temporary')",
    ] {
        db.execute_unprepared(statement).await?;
    }
    let mode = text(&db, "PRAGMA journal_mode").await?;
    ensure!(mode == "wal", "fixture requires actual WAL");
    // The original pool owns this private file for the whole peer lifetime.
    // ZERO busy wait makes the competing lock assertion immediate, not a retry.
    let peer = SqliteConnection::connect_with(
        &SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(false)
            .busy_timeout(std::time::Duration::ZERO),
    )
    .await?;
        Ok::<_, anyhow::Error>(peer)
    }
    .await;
    match peer {
        Ok(peer) => Ok((db, peer)),
        Err(error) => {
            db.close().await?;
            Err(error)
        }
    }
}

async fn text(db: &impl ConnectionTrait, sql: &'static str) -> Result<String> {
    let row = db
        .query_one(sea_orm::Statement::from_string(
            DatabaseBackend::Sqlite,
            sql,
        ))
        .await?
        .context("text observation missing")?;
    Ok(row.try_get_by_index(0)?)
}

async fn number(db: &impl ConnectionTrait, sql: &'static str) -> Result<i64> {
    let row = db
        .query_one(sea_orm::Statement::from_string(
            DatabaseBackend::Sqlite,
            sql,
        ))
        .await?
        .context("numeric observation missing")?;
    Ok(row.try_get_by_index(0)?)
}

fn sqlite_code(error: &anyhow::Error) -> Option<i32> {
    error.chain().find_map(|cause| {
        cause
            .downcast_ref::<sea_orm::sqlx::Error>()
            .and_then(sea_orm::sqlx::Error::as_database_error)
            .and_then(|error| error.code())
            .and_then(|code| code.parse().ok())
    })
}

async fn close(
    db: sea_orm::DatabaseConnection,
    peer: SqliteConnection,
    checked: Result<()>,
) -> Result<()> {
    // Attempt both original owners even when an assertion or one close fails.
    let peer_closed = peer.close().await;
    let db_closed = db.close().await;
    peer_closed?;
    db_closed?;
    checked
}

#[tokio::test]
async fn standalone_eval_intent_locks_main_before_read_without_rows_or_triggers() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let (db, mut peer) = database(&directory.path().join("eval.sqlite")).await?;
    let checked = async {
        let store = EvalStore::new(db.clone());
        let changes = number(&db, "SELECT total_changes()").await?;
        let write = Write::start(&None, false)?;
        let (write, tx) = store.write_transaction(write).await?;
        let rejected = sea_orm::sqlx::query(
            "UPDATE main.eval_subjects SET eval_id='peer' WHERE eval_id='original'",
        )
        .execute(&mut peer)
        .await
        .err()
        .context("competing writer was admitted before first Eval read")?;
        let code = rejected
            .as_database_error()
            .and_then(|error| error.code())
            .and_then(|code| code.parse::<i32>().ok());
        ensure!(code.is_some_and(|code| code & 0xff == 5));
        // The first application SELECT in this transaction is AFTER intent.
        ensure!(text(&tx, "SELECT eval_id FROM main.eval_subjects").await? == "original");
        ensure!(text(&tx, "SELECT eval_id FROM temp.eval_subjects").await? == "temporary");
        ensure!(number(&tx, "SELECT total_changes()").await? == changes);
        ensure!(number(&tx, "SELECT count(*) FROM main.eval_update_observations").await? == 0);
        ensure!(
            number(
                &tx,
                "SELECT count(*) FROM main.sqlite_master WHERE name='tenancy_installation'"
            )
            .await?
                == 0
        );
        write.finish(tx, Ok::<(), anyhow::Error>(())).await?;
        // Positive release/control: only this genuine row mutation fires it.
        sea_orm::sqlx::query("UPDATE main.eval_subjects SET eval_id='peer'")
            .execute(&mut peer)
            .await?;
        ensure!(text(&db, "SELECT eval_id FROM main.eval_subjects").await? == "peer");
        ensure!(text(&db, "SELECT eval_id FROM temp.eval_subjects").await? == "temporary");
        ensure!(number(&db, "SELECT count(*) FROM main.eval_update_observations").await? == 1);
        Ok(())
    }
    .await;
    close(db, peer, checked).await
}

#[tokio::test]
async fn eval_read_transaction_remains_deferred_and_keeps_original_snapshot() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let (db, mut peer) = database(&directory.path().join("eval.sqlite")).await?;
    let checked = async {
        let store = EvalStore::new(db.clone());
        let tx = store.transaction(false).await?;
        ensure!(text(&tx, "SELECT eval_id FROM main.eval_subjects").await? == "original");
        sea_orm::sqlx::query("UPDATE main.eval_subjects SET eval_id='peer'")
            .execute(&mut peer)
            .await?;
        ensure!(text(&tx, "SELECT eval_id FROM main.eval_subjects").await? == "original");
        tx.commit().await?;
        ensure!(text(&db, "SELECT eval_id FROM main.eval_subjects").await? == "peer");
        Ok(())
    }
    .await;
    close(db, peer, checked).await
}

#[tokio::test]
async fn eval_intent_missing_main_target_refuses_temp_shadow_and_rolls_back() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let (db, peer) = database(&directory.path().join("eval.sqlite")).await?;
    let checked = async {
        db.execute_unprepared("DROP TABLE main.eval_subjects")
            .await?;
        let store = EvalStore::new(db.clone());
        let refused = store
            .write_transaction(Write::start(&None, false)?)
            .await
            .err()
            .context("missing ordinary main target was adopted")?;
        ensure!(sqlite_code(&refused).is_some_and(|code| code & 0xff == 1));
        // Reacquire and actually commit after the helper's acknowledged rollback.
        let read = store.transaction(false).await?;
        ensure!(
            number(
                &read,
                "SELECT count(*) FROM main.sqlite_master WHERE name='eval_subjects'"
            )
            .await?
                == 0
        );
        ensure!(text(&read, "SELECT eval_id FROM temp.eval_subjects").await? == "temporary");
        read.commit().await?;
        Ok(())
    }
    .await;
    close(db, peer, checked).await
}

#[tokio::test]
async fn readonly_eval_reads_work_and_write_intent_refuses_without_losing_pool() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("eval.sqlite");
    let (db, peer) = database(&path).await?;
    close(db, peer, Ok(())).await?;
    let db = crate::db::connect(&format!("sqlite://{}?mode=ro", path.display())).await?;
    let checked = async {
        let store = EvalStore::new(db.clone());
        let read = store.transaction(false).await?;
        ensure!(text(&read, "SELECT eval_id FROM main.eval_subjects").await? == "original");
        read.commit().await?;
        let refused = store
            .write_transaction(Write::start(&None, false)?)
            .await
            .err()
            .context("readonly Eval write intent was accepted")?;
        ensure!(sqlite_code(&refused).is_some_and(|code| code & 0xff == 8));
        let read = store.transaction(false).await?;
        ensure!(text(&read, "SELECT eval_id FROM main.eval_subjects").await? == "original");
        ensure!(number(&read, "SELECT count(*) FROM main.eval_update_observations").await? == 0);
        read.commit().await?;
        Ok(())
    }
    .await;
    let closed = db.close().await;
    closed?;
    checked
}
