//! Real database aborts distinguish transaction retry from inference replay.

use std::sync::atomic::{AtomicUsize, Ordering};

use super::*;
use crate::gateway::shared::test_database::EmptyDatabase;

#[tokio::test]
#[ignore = "requires PostgreSQL CREATEDB privilege"]
async fn postgres_retry_uses_deadline_and_never_retries_uncertain_errors() -> Result<()> {
    let mut database = EmptyDatabase::create().await?;
    let result = tokio::time::timeout(Duration::from_secs(20), async {
        let db = database.connect(2, Duration::from_secs(2)).await?;
        db.execute_unprepared(
            "CREATE SEQUENCE retry_attempts; CREATE TABLE retry_effect (id INTEGER PRIMARY KEY)",
        )
        .await?;
        let store = Store::new(db.clone(), "retry-test", Duration::from_secs(2))?;
        let calls = AtomicUsize::new(0);
        store
            .retry(|| async {
                calls.fetch_add(1, Ordering::SeqCst);
                let tx = store.begin().await?;
                tx.execute_unprepared("INSERT INTO retry_effect VALUES (1)")
                    .await?;
                // Sequence increments survive rollback; the effect above must
                // not. This makes the first four complete transactions abort.
                // https://www.postgresql.org/docs/current/functions-sequence.html
                tx.execute_unprepared(
                    "DO $$ BEGIN IF nextval('retry_attempts') <= 4 THEN \
                     RAISE EXCEPTION 'fixture abort' USING ERRCODE = '40001'; \
                     END IF; END $$",
                )
                .await?;
                tx.commit().await?;
                Ok(())
            })
            .await?;
        ensure!(calls.load(Ordering::SeqCst) == 5);
        let row = db
            .query_one(Statement::from_string(
                DatabaseBackend::Postgres,
                "SELECT COUNT(*) AS count FROM retry_effect",
            ))
            .await?
            .context("missing committed effect")?;
        ensure!(row.try_get::<i64>("", "count")? == 1);

        // Simulated server error codes only, not a claim of a lost COMMIT ACK.
        // Neither a connection-class error nor a uniqueness error authorizes a
        // complete transaction retry under the production allowlist.
        for code in ["08006", "23505"] {
            let calls = AtomicUsize::new(0);
            let error = store
                .retry(|| async {
                    calls.fetch_add(1, Ordering::SeqCst);
                    let tx = store.begin().await?;
                    tx.execute_unprepared(&format!(
                        "DO $$ BEGIN RAISE EXCEPTION 'fixture error' USING ERRCODE = '{code}'; END $$"
                    ))
                    .await?;
                    tx.commit().await?;
                    Ok(())
                })
                .await
                .err()
                .context("non-retryable database failure unexpectedly succeeded")?;
            ensure!(!retryable_abort(&error));
            ensure!(error.chain().any(|cause| {
                matches!(cause.downcast_ref::<sea_orm::DbErr>(),
                    Some(sea_orm::DbErr::Exec(sea_orm::RuntimeErr::SqlxError(
                        sea_orm::SqlxError::Database(database))))
                    if database.code().as_deref() == Some(code))
            }));
            ensure!(calls.load(Ordering::SeqCst) == 1);
        }
        let short = Store::new(db, "retry-test", Duration::from_millis(100))?;
        let calls = AtomicUsize::new(0);
        let started = tokio::time::Instant::now();
        let error = short
            .retry(|| async {
                calls.fetch_add(1, Ordering::SeqCst);
                let tx = short.begin().await?;
                tx.execute_unprepared(
                    "DO $$ BEGIN RAISE EXCEPTION 'fixture deadlock' USING ERRCODE = '40P01'; END $$",
                )
                .await?;
                tx.commit().await?;
                Ok(())
            })
            .await
            .err()
            .context("persistent abort escaped the total deadline")?;
        ensure!(error.chain().any(|cause| cause.is::<tokio::time::error::Elapsed>()));
        ensure!(calls.load(Ordering::SeqCst) >= 2);
        ensure!(started.elapsed() < Duration::from_secs(2));
        Ok(())
    })
    .await
    .context("retry regression campaign deadline")
    .and_then(|result| result);
    database.finish(result).await
}

#[derive(Debug, thiserror::Error)]
#[error("synthetic serialization abort")]
struct Abort;

impl sea_orm::sqlx::error::DatabaseError for Abort {
    fn message(&self) -> &str {
        "synthetic serialization abort"
    }
    fn code(&self) -> Option<std::borrow::Cow<'_, str>> {
        Some("40001".into())
    }
    fn as_error(&self) -> &(dyn std::error::Error + Send + Sync + 'static) {
        self
    }
    fn as_error_mut(&mut self) -> &mut (dyn std::error::Error + Send + Sync + 'static) {
        self
    }
    fn into_error(self: Box<Self>) -> Box<dyn std::error::Error + Send + Sync + 'static> {
        self
    }
    fn kind(&self) -> sea_orm::sqlx::error::ErrorKind {
        sea_orm::sqlx::error::ErrorKind::Other
    }
}

#[tokio::test(start_paused = true)]
async fn delayed_poll_cannot_start_an_attempt_after_the_total_deadline() -> Result<()> {
    let mut options = sea_orm::ConnectOptions::new("postgres://unused:unused@127.0.0.1:1/unused");
    options
        .connect_lazy(true)
        .min_connections(0)
        .max_connections(1);
    let db = sea_orm::Database::connect(options).await?;
    db.clone().close().await?;
    let store = Store::new(db, "late-poll", Duration::from_millis(100))?;
    let calls = AtomicUsize::new(0);
    let retry = store.retry(|| {
        let first = calls.fetch_add(1, Ordering::SeqCst) == 0;
        std::future::ready(if first {
            Err(sea_orm::DbErr::Exec(sea_orm::RuntimeErr::SqlxError(
                sea_orm::SqlxError::Database(Box::new(Abort)),
            ))
            .into())
        } else {
            // Without the deadline admission check this immediately ready
            // success wins over Tokio's outer timer after the late wake.
            Ok(())
        })
    });
    tokio::pin!(retry);
    ensure!(futures::poll!(retry.as_mut()).is_pending());
    ensure!(calls.load(Ordering::SeqCst) == 1);
    tokio::time::advance(Duration::from_secs(1)).await;
    let error = retry
        .await
        .err()
        .context("expired retry returned success")?;
    ensure!(
        error
            .chain()
            .any(|cause| cause.is::<tokio::time::error::Elapsed>())
    );
    ensure!(calls.load(Ordering::SeqCst) == 1);
    Ok(())
}

#[tokio::test]
#[ignore = "requires PostgreSQL CREATEDB privilege"]
async fn postgres_get_retries_serialization_abort_with_a_fresh_transaction() -> Result<()> {
    let mut database = EmptyDatabase::create().await?;
    let result = tokio::time::timeout(Duration::from_secs(20), async {
        // A single registered connection also requires the aborted transaction
        // to be rolled back before the retry can obtain another transaction.
        let db = database.connect(1, Duration::from_secs(2)).await?;
        let store = Store::new(db.clone(), "get-retry-test", Duration::from_secs(2))?;
        crate::gateway::shared::schema::initialize(&store).await?;
        let value = serde_json::json!({"execution_id": "fixture", "settled": true});
        let tx = store.begin().await?;
        ensure!(
            store
                .create(
                    &tx,
                    NewRecord {
                        kind: Kind::ProjectionReceipt,
                        key: "fixture-terminal",
                        status: "delivered",
                        due_at_ms: None,
                        now_ms: 1,
                        value: &value,
                    },
                )
                .await?
        );
        let expected = store
            .read::<serde_json::Value>(&tx, Kind::ProjectionReceipt, "fixture-terminal")
            .await?
            .context("seeded get fixture record missing")?;
        tx.commit().await?;

        // Change only this owned database's relation name. The view preserves
        // the real migrated columns and forces the production SELECT through a
        // volatile function, without a production hook or RLS/superuser setup.
        // https://www.postgresql.org/docs/current/sql-createview.html
        // https://www.postgresql.org/docs/current/sql-createfunction.html
        // Sequence increments survive the raised transaction abort.
        // https://www.postgresql.org/docs/current/functions-sequence.html
        // This injects a real SQLSTATE, not a naturally occurring SSI cycle.
        // https://www.postgresql.org/docs/current/plpgsql-errors-and-messages.html
        db.execute_unprepared(
            "ALTER TABLE gateway_shared_records RENAME TO get_fixture_records;
             CREATE SEQUENCE get_read_attempts;
             CREATE FUNCTION get_fixture_read() RETURNS SETOF get_fixture_records
             LANGUAGE plpgsql VOLATILE AS $$
             BEGIN
                 IF current_setting('transaction_isolation') <> 'serializable' THEN
                     RAISE EXCEPTION 'get changed the expected transaction isolation';
                 END IF;
                 IF nextval('get_read_attempts') = 1 THEN
                     RAISE EXCEPTION 'fixture first SELECT serialization abort'
                         USING ERRCODE = '40001';
                 END IF;
                 RETURN QUERY SELECT * FROM get_fixture_records;
             END;
             $$;
             CREATE VIEW gateway_shared_records AS SELECT * FROM get_fixture_read();",
        )
        .await?;

        // Call get itself: wrapping this call in a fixture retry would hide the
        // missing production transaction boundary that this regression covers.
        let actual = store
            .get::<serde_json::Value>(Kind::ProjectionReceipt, "fixture-terminal")
            .await?
            .context("retried get lost the committed fixture record")?;
        ensure!(actual.row == expected.row && actual.value == expected.value);
        let attempts = db
            .query_one(Statement::from_string(
                DatabaseBackend::Postgres,
                "SELECT last_value FROM get_read_attempts",
            ))
            .await?
            .context("get SELECT attempt count missing")?;
        ensure!(
            attempts.try_get::<i64>("", "last_value")? == 2,
            "get did not make exactly one fresh transaction retry"
        );
        Ok(())
    })
    .await
    .context("get serialization retry campaign deadline")
    .and_then(|result| result);
    database.finish(result).await
}
