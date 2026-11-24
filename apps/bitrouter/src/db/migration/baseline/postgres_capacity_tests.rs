//! Native PostgreSQL upgrades use the original owner and a transactional catalog.
use super::*;
use crate::db::{
    current_schema,
    migration::{capacity, provider_technical, server_fixture::OwnedServer},
};
use anyhow::{Context, Result, ensure};
use sea_orm::DatabaseConnection;
use std::sync::{Arc, Mutex};

const BACKEND: DatabaseBackend = DatabaseBackend::Postgres;

async fn legacy(db: &DatabaseConnection, identity: bool) -> Result<()> {
    for statement in current_schema::legacy_statements(BACKEND)? {
        db.execute_unprepared(&statement).await?;
    }
    db.execute(Statement::from_sql_and_values(
        BACKEND,
        "INSERT INTO seaql_migrations(version,applied_at) VALUES($1,$2)",
        [store_format::LEGACY_GATEWAY_POSTGRES.into(), 1_i64.into()],
    ))
    .await?;
    if identity {
        db.execute(Statement::from_sql_and_values(BACKEND,
            "INSERT INTO tenancy_installation(singleton,installation_id,schema_version,authority_revision,operation_ids,safety_operation_ids,saturated,created_at) VALUES(1,'legacy-owner',$1,7,0,0,false,'2026-11-18T00:00:00Z')",
            [crate::tenancy::AUTHORITY_SCHEMA_VERSION.into()])).await?;
    }
    current_schema::validate_legacy(db).await?;
    ensure!(store_format::inspect(db).await? == State::Legacy);
    ensure!(!SchemaManager::new(db).has_table(capacity::TABLE).await?);
    Ok(())
}

async fn malformed_receipt(db: &impl ConnectionTrait) -> Result<()> {
    db.execute_unprepared(
        "INSERT INTO tenancy_execution_receipts(installation_id,tenant_id,execution_id,principal_id,correlation_id,receipt_json,content_digest,disposition_json,byte_count,unknown_outcome,recorded_at) VALUES('legacy-owner','tenant','historical','principal','correlation','{',repeat('0',64),'{}',1,false,'2026-11-18T00:00:00Z')"
    ).await?;
    Ok(())
}

async fn original_marker(db: &impl ConnectionTrait) -> Result<()> {
    let row = db
        .query_one(Statement::from_string(
            BACKEND,
            "SELECT version,applied_at FROM seaql_migrations",
        ))
        .await?
        .context("legacy ledger")?;
    ensure!(row.try_get::<String>("", "version")? == store_format::LEGACY_GATEWAY_POSTGRES);
    ensure!(row.try_get::<i64>("", "applied_at")? == 1);
    Ok(())
}

#[tokio::test]
#[ignore = "requires dedicated PostgreSQL CREATEDB; no fallback"]
async fn native_postgres_upgrade_preserves_epoch_and_current_reopen_never_reads_history()
-> Result<()> {
    for optional in [false, true] {
        let fixture = OwnedServer::new(BACKEND, "BITROUTER_TEST_MIGRATION_DATABASE_URL").await?;
        let checked = async {
            legacy(&fixture.database, true).await?;
            if optional {
                provider_technical::install(&fixture.database).await?;
            }
            fixture.database.execute_unprepared(
                "INSERT INTO tenancy_continuation_epoch(singleton,installation_id,key_id,created_at) VALUES(1,'legacy-owner',repeat('1',64),'2026-11-18T00:00:00Z')"
            ).await?;
            let owned = crate::db::connect_owned_tenancy(&fixture.url).await?;
            owned.migrate().await?;
            let db = owned.database();
            ensure!(store_format::inspect(db).await? == State::Current);
            current_schema::validate(db).await?;
            ensure!(capacity::epoch_required(db).await?);
            let audited = crate::tenancy::receipts::store::capacity_backfill(db).await?;
            capacity::verify(db, &audited).await?;
            let saved = capacity::load(db).await?;
            ensure!(saved.live_bytes == 0 && saved.unknown_count == 0);
            // A damaged historical body distinguishes an accidental complete
            // audit from the bounded schema/projection checks on current reopen.
            malformed_receipt(db).await?;
            owned.database().clone().close().await?;
            drop(owned);

            let mut reopened = crate::db::connect_owned_tenancy(&fixture.url).await?;
            let observed = Arc::new(Mutex::new(Vec::<String>::new()));
            let callback = observed.clone();
            reopened.db.set_metric_callback(move |info| {
                if let Ok(mut queries) = callback.lock() {
                    queries.push(info.statement.sql.to_ascii_lowercase().replace('"', ""));
                }
            });
            reopened.database().execute_unprepared("SET default_transaction_read_only=on").await?;
            for _ in 0..2 {
                reopened.migrate().await?;
                let loaded = capacity::load(reopened.database()).await?;
                ensure!(loaded.live_bytes == saved.live_bytes && loaded.unknown_count == saved.unknown_count);
                ensure!(capacity::epoch_required(reopened.database()).await?);
            }
            let queries = observed.lock().map_err(|_| anyhow::anyhow!("SQL observation poisoned"))?.clone();
            for query in &queries {
                for table in [
                    "tenancy_execution_receipts", "tenancy_usage_evidence", "tenancy_receipt_archive",
                    "tenancy_receipt_auxiliary", "tenancy_unknown_obligations", "tenancy_oauth_slots",
                    "tenancy_resource_revisions", "provider_continuations", "tenancy_provider_technical_facts",
                ] {
                    ensure!(!query.contains(&format!("from {table}")) && !query.contains(&format!("join {table}")), "current reopen queried retained source: {query}");
                }
            }
            ensure!(!queries.is_empty(), "driver query observer was not installed");
            ensure!(crate::tenancy::receipts::store::capacity_backfill(reopened.database()).await.is_err());
            reopened.database().clone().close().await?;
            drop(reopened);
            Ok(())
        }.await;
        fixture.finish(checked).await?;
    }
    Ok(())
}

#[tokio::test]
#[ignore = "requires dedicated PostgreSQL CREATEDB; no fallback"]
async fn native_postgres_failed_backfill_rolls_back_ddl_and_marker_and_can_retry() -> Result<()> {
    for identity in [false, true] {
        let fixture = OwnedServer::new(BACKEND, "BITROUTER_TEST_MIGRATION_DATABASE_URL").await?;
        let checked = async {
            legacy(&fixture.database, identity).await?;
            malformed_receipt(&fixture.database).await?;
            let owned = crate::db::connect_owned_tenancy(&fixture.url).await?;
            let db = owned.database();
            ensure!(owned.migrate().await.is_err());
            original_marker(db).await?;
            ensure!(!SchemaManager::new(db).has_table(capacity::TABLE).await?);
            current_schema::validate_legacy(db).await?;
            ensure!(db.query_one(Statement::from_string(BACKEND,
                "SELECT receipt_json FROM tenancy_execution_receipts WHERE execution_id='historical'"
            )).await?.context("source disappeared after failed upgrade")?.try_get::<String>("", "receipt_json")? == "{");
            db.execute_unprepared("DELETE FROM tenancy_execution_receipts WHERE execution_id='historical'").await?;
            owned.migrate().await?;
            ensure!(store_format::inspect(db).await? == State::Current);
            let audited = crate::tenancy::receipts::store::capacity_backfill(db).await?;
            capacity::verify(db, &audited).await?;
            db.clone().close().await?;
            drop(owned);
            Ok(())
        }.await;
        fixture.finish(checked).await?;
    }
    Ok(())
}

#[tokio::test]
#[ignore = "requires dedicated PostgreSQL CREATEDB; no fallback"]
async fn native_postgres_legacy_catalog_is_exact_before_any_capacity_ddl() -> Result<()> {
    for corruption in [
        "DROP INDEX idx_tenancy_receipts_storage",
        "ALTER TABLE tenancy_oauth_slots DROP COLUMN pending_tenant_id",
    ] {
        let fixture = OwnedServer::new(BACKEND, "BITROUTER_TEST_MIGRATION_DATABASE_URL").await?;
        let checked = async {
            legacy(&fixture.database, false).await?;
            fixture.database.execute_unprepared(corruption).await?;
            ensure!(
                crate::db::connect_owned_tenancy(&fixture.url)
                    .await
                    .is_err()
            );
            ensure!(
                super::super::Migrator::up(&fixture.database, None)
                    .await
                    .is_err()
            );
            original_marker(&fixture.database).await?;
            ensure!(
                !SchemaManager::new(&fixture.database)
                    .has_table(capacity::TABLE)
                    .await?
            );
            Ok(())
        }
        .await;
        fixture.finish(checked).await?;
    }
    // Unlike MySQL's staged DDL retry, PostgreSQL DDL is transactional: an
    // unexpected capacity table under the old marker is never silently adopted.
    let fixture = OwnedServer::new(BACKEND, "BITROUTER_TEST_MIGRATION_DATABASE_URL").await?;
    let checked = async {
        legacy(&fixture.database, false).await?;
        capacity::ensure_table(&SchemaManager::new(&fixture.database)).await?;
        ensure!(
            current_schema::validate_legacy(&fixture.database)
                .await
                .is_err()
        );
        ensure!(
            super::super::Migrator::up(&fixture.database, None)
                .await
                .is_err()
        );
        original_marker(&fixture.database).await?;
        ensure!(
            fixture
                .database
                .query_all(Statement::from_string(
                    BACKEND,
                    "SELECT singleton FROM tenancy_receipt_capacity"
                ))
                .await?
                .is_empty()
        );
        Ok(())
    }
    .await;
    fixture.finish(checked).await
}
