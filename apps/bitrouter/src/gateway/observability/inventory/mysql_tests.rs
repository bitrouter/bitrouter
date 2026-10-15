//! Ignored regressions for a dedicated MySQL BITROUTER_TEST_DATABASE_URL.
//! Applies real migrations; fixtures and cleanup use only fresh UUIDs.
//! After wiring this module, run with --lib --no-default-features --features
//! db-mysql and filter gateway::observability::inventory::mysql_tests -- --ignored.

use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use sea_orm::{
    ActiveModelTrait, DatabaseConnection, EntityTrait, IsolationLevel, Set, TransactionTrait,
};
use tokio::sync::Mutex;
use tokio::time::timeout;

use super::{
    COVERAGE_KIND, CoverageRegistration, GatewayInventory, coverage_matches_locked, lock_connection,
};
use crate::storage::capture::connections;
use crate::storage::evolution::{EvolutionStore, records};

// The two ignored tests may start together against an unmigrated database.
static MIGRATIONS: Mutex<()> = Mutex::const_new(());
const RUN_DEADLINE: Duration = Duration::from_secs(60);
const IO_DEADLINE: Duration = Duration::from_secs(10);

async fn test_database() -> Result<DatabaseConnection> {
    let url = std::env::var("BITROUTER_TEST_DATABASE_URL").context(
        "set BITROUTER_TEST_DATABASE_URL to a dedicated MySQL test database; no fallback",
    )?;
    let parsed = url::Url::parse(&url)?;
    ensure!(
        parsed.scheme() == "mysql" && !parsed.path().trim_matches('/').is_empty(),
        "inventory regressions require an explicitly named MySQL test database"
    );
    Ok(timeout(IO_DEADLINE, crate::db::connect(&url))
        .await
        .context("connecting to the MySQL test database timed out")??)
}

async fn create_recording_fixture(db: &DatabaseConnection, id: &str) -> Result<connections::Model> {
    {
        let _migration_guard = MIGRATIONS.lock().await;
        crate::db::run_migrations(db).await?;
    }
    // Direct persistence keeps these library tests independent of the harness.
    Ok(connections::ActiveModel {
        connection_id: Set(id.into()),
        owner: Set(id.into()),
        source: Set("mysql-inventory-regression".into()),
        controller_instance_id: Set(Some(id.into())),
        route_scope_id: Set(Some(id.into())),
        state: Set("recording".into()),
        head: Set(0),
        started_at: Set(chrono::Utc::now().to_rfc3339()),
        metadata_json: Set("{}".into()),
    }
    .insert(db)
    .await?)
}

async fn finish(db: DatabaseConnection, id: &str, result: Result<()>) -> Result<()> {
    // Run cleanup and close even after a regression failure or deadline. Dropped
    // transactions roll back; no spawned tasks can retain a connection.
    let cleaned = timeout(IO_DEADLINE, async {
        let store = EvolutionStore::new(db.clone(), id)?;
        records::Entity::delete_by_id(store.id(COVERAGE_KIND, id)?)
            .exec(&db)
            .await?;
        connections::Entity::delete_by_id(id).exec(&db).await?;
        Ok::<(), anyhow::Error>(())
    })
    .await
    .context("cleaning up the UUID-scoped MySQL fixtures timed out");
    let closed = timeout(IO_DEADLINE, db.close())
        .await
        .context("closing the MySQL test pool timed out");
    result?;
    cleaned??;
    closed??;
    Ok(())
}

#[tokio::test]
#[ignore = "requires db-mysql and a dedicated MySQL BITROUTER_TEST_DATABASE_URL"]
async fn lock_connection_reads_current_state_and_head_after_an_older_snapshot() -> Result<()> {
    let db = test_database().await?;
    let id = uuid::Uuid::new_v4().to_string();
    let result = timeout(RUN_DEADLINE, async {
        let initial = create_recording_fixture(&db, &id).await?;
        // RR consistent reads retain the first read's snapshot. Taking a lock
        // must also read current values, not reuse that snapshot.
        // https://dev.mysql.com/doc/refman/8.4/en/innodb-consistent-read.html
        // https://dev.mysql.com/doc/refman/8.4/en/innodb-locking-reads.html
        let reader = db
            .begin_with_config(Some(IsolationLevel::RepeatableRead), None)
            .await?;
        ensure!(
            connections::Entity::find_by_id(&id).one(&reader).await? == Some(initial.clone()),
            "failed to establish the old connection snapshot"
        );

        // Reader stays open, so this transaction uses another pooled connection.
        // Await its commit before locking: no sleep or scheduling race is needed.
        let writer = db.begin().await?;
        let mut changed: connections::ActiveModel = initial.clone().into();
        changed.state = Set("complete".into());
        changed.head = Set(7);
        changed.update(&writer).await?;
        writer.commit().await?;
        ensure!(
            connections::Entity::find_by_id(&id).one(&reader).await? == Some(initial),
            "reader did not retain its old RR snapshot after the writer committed"
        );

        let current = lock_connection(&reader, &id).await?;
        reader.rollback().await?;
        ensure!(
            current.state == "complete" && current.head == 7,
            "lock_connection returned stale state/head: {:?}/{}",
            current.state,
            current.head
        );
        Ok(())
    })
    .await
    .context("MySQL connection-lock regression timed out")
    .and_then(|result| result);
    finish(db, &id, result).await
}

#[tokio::test]
#[ignore = "requires db-mysql and a dedicated MySQL BITROUTER_TEST_DATABASE_URL"]
async fn locked_coverage_rejects_stale_absence_and_revision() -> Result<()> {
    let db = test_database().await?;
    let id = uuid::Uuid::new_v4().to_string();
    let result = timeout(RUN_DEADLINE, async {
        create_recording_fixture(&db, &id).await?;
        let inventory = GatewayInventory::new(db.clone());
        let store = EvolutionStore::new(db.clone(), &id)?;
        let mut observations = Vec::new();

        // Exercise both absence -> registration and revision 0 -> revision 1.
        // Each ordinary read pins an RR snapshot before the other transaction
        // commits. Keep connection state/head unchanged to isolate coverage.
        // https://dev.mysql.com/doc/refman/8.4/en/innodb-consistent-read.html
        for expected in [None, Some(0_i64)] {
            let reader = db
                .begin_with_config(Some(IsolationLevel::RepeatableRead), None)
                .await?;
            let old = store
                .get_in::<CoverageRegistration>(&reader, COVERAGE_KIND, &id)
                .await?
                .map(|(revision, _)| revision);
            ensure!(old == expected, "unexpected initial coverage revision");

            if expected.is_none() {
                // register_capture owns and commits a separate transaction.
                inventory.register_capture(&id, &id, &id).await?;
            } else {
                let writer = db.begin().await?;
                lock_connection(&writer, &id).await?;
                ensure!(
                    inventory
                        .touch_coverage(&writer, &BTreeMap::from([(id.clone(), 0)]))
                        .await?,
                    "registered coverage was unexpectedly invalid"
                );
                writer.commit().await?;
            }
            let (current, _) = store
                .get::<CoverageRegistration>(COVERAGE_KIND, &id)
                .await?
                .context("writer did not commit coverage")?;
            let wanted = match expected {
                None => 0,
                Some(revision) => revision + 1,
            };
            ensure!(current == wanted, "writer did not advance coverage");
            ensure!(
                store
                    .get_in::<CoverageRegistration>(&reader, COVERAGE_KIND, &id)
                    .await?
                    .map(|(revision, _)| revision)
                    == expected,
                "reader did not retain its old coverage snapshot"
            );

            lock_connection(&reader, &id).await?;
            let stale = BTreeMap::from([(id.clone(), expected)]);
            let fresh = BTreeMap::from([(id.clone(), Some(current))]);
            // The mutation-phase check must preserve both absent and present fences.
            let accepts_stale = coverage_matches_locked(&store, &reader, &stale).await?;
            let accepts_current = coverage_matches_locked(&store, &reader, &fresh).await?;
            reader.rollback().await?;
            observations.push((expected, accepts_stale, accepts_current));
        }
        // Collect both cases before asserting, so the absent-row regression
        // cannot prevent exercising the obsolete existing revision as well.
        ensure!(
            observations
                .iter()
                .all(|(_, accepts_stale, accepts_current)| !*accepts_stale && *accepts_current),
            "locked coverage must reject stale revisions and accept current ones; \
             (expected, accepts_stale, accepts_current): {observations:?}"
        );
        Ok(())
    })
    .await
    .context("MySQL coverage-lock regression timed out")
    .and_then(|result| result);
    finish(db, &id, result).await
}
