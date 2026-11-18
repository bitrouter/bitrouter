//! Fresh current schema. No historical ledger, row conversion or authority upgrade.
use sea_orm::{ConnectionTrait, DatabaseBackend, DbErr, Statement, TransactionTrait};
use sea_orm_migration::{SchemaManagerConnection, prelude::*};

use crate::db::store_format::{self, State};

pub(super) struct Migration;
impl MigrationName for Migration {
    fn name(&self) -> &str {
        store_format::GATEWAY_SERVER
    }
}

pub(super) async fn open(connection: SchemaManagerConnection<'_>) -> Result<(), DbErr> {
    let tx = connection.begin().await?;
    let result = async {
        if tx.get_database_backend() == DatabaseBackend::Postgres {
            tx.execute(Statement::from_sql_and_values(
                DatabaseBackend::Postgres,
                "SELECT pg_advisory_xact_lock($1)",
                [0x4252_4d30_5343_4831_i64.into()],
            ))
            .await?;
        }
        let state = store_format::inspect(&tx)
            .await
            .map_err(|error| DbErr::Custom(error.to_string()))?;
        if tx.get_database_backend() == DatabaseBackend::Sqlite {
            if state == State::Empty {
                crate::db::mixed_store::initialize_gateway_schema(&tx)
                    .await
                    .map_err(|error| DbErr::Custom(error.to_string()))?;
            }
            return validate(&tx).await;
        }
        if state == State::Current {
            return validate(&tx).await;
        }
        let manager = SchemaManager::new(&tx);
        Migration.up(&manager).await?;
        // Install and publish the only ledger row after the complete schema
        // validates. SQLite/PostgreSQL commit the schema and marker together.
        // MySQL's implicit DDL commits cannot publish a half-built baseline.
        super::Migrator::install(&tx).await?;
        tx.execute(Statement::from_sql_and_values(
            tx.get_database_backend(),
            match tx.get_database_backend() {
                DatabaseBackend::Postgres => {
                    "INSERT INTO seaql_migrations(version,applied_at) VALUES ($1,$2)"
                }
                _ => "INSERT INTO seaql_migrations(version,applied_at) VALUES (?,?)",
            },
            [
                store_format::GATEWAY_SERVER.into(),
                chrono::Utc::now().timestamp().into(),
            ],
        ))
        .await?;
        validate(&tx).await?;
        Ok(())
    }
    .await;
    match result {
        Ok(()) => tx.commit().await,
        Err(error) => {
            tx.rollback().await?;
            Err(error)
        }
    }
}

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        if store_format::inspect(manager.get_connection())
            .await
            .map_err(|error| DbErr::Custom(error.to_string()))?
            != State::Empty
        {
            return Err(DbErr::Custom(
                "baseline initialization requires a genuinely empty database".into(),
            ));
        }
        service_tables(manager).await?;
        if manager.get_database_backend() == DatabaseBackend::MySql {
            // This path only constructs a fresh gateway. Historical fixtures
            // retain the column comparisons inherited from their database.
            let mut alter = Table::alter();
            alter.table(Alias::new("requests"));
            for &name in super::MYSQL_METERING_IDENTITIES {
                let nullable = !matches!(name, "request_id" | "user_id" | "api_key_id");
                alter.modify_column(super::m20240101_000028_resource_tenancy::identity(
                    DatabaseBackend::MySql,
                    name,
                    255,
                    nullable,
                ));
            }
            manager.alter_table(alter).await?;
        }
        super::m20240101_000020_create_checkpoint_evolution::scope(manager).await?;
        validate_tables(manager.get_connection()).await
    }

    async fn down(&self, _: &SchemaManager) -> Result<(), DbErr> {
        Err(DbErr::Custom(
            "current store rollback requires its original binary and backup".into(),
        ))
    }
}

pub(super) async fn service_tables(manager: &SchemaManager<'_>) -> Result<(), DbErr> {
    super::m20240101_000001_create_auth_tables::initialize_current(manager).await?;
    super::m20240101_000002_create_metering_tables::initialize_current(manager).await?;
    super::m20240101_000007_add_metering_evidence::initialize_current(manager).await?;
    super::m20240101_000008_add_metering_reconciliation::initialize_current(manager).await?;
    super::m20240101_000010_create_eval_exchange::initialize_current(manager).await?;
    super::m20240101_000011_scope_eval_exchange::initialize_current(manager).await?;
    super::m20240101_000012_create_trajectory_ledger::initialize_current(manager).await?;
    super::m20240101_000013_create_continuation_registry::initialize_current(manager).await?;
    super::m20240101_000015_add_metering_launch_id::initialize_current(manager).await?;
    super::m20240101_000016_add_acp_metering_identity::initialize_current(manager).await?;
    super::m20240101_000017_add_metering_route_scope::initialize_current(manager).await?;
    super::m20240101_000020_create_checkpoint_evolution::initialize_current(manager).await?;
    super::m20240101_000021_add_router_request_identity::initialize_current(manager).await?;
    super::m20240101_000028_resource_tenancy::initialize_current(manager).await?;
    super::m20240101_000029_tenancy_configurations::initialize_current(manager).await?;
    super::m20240101_000030_eval_storage_scope::initialize_current(manager).await?;
    super::m20240101_000031_native_continuation_epoch::initialize_current(manager).await?;
    super::m20240101_000032_trajectory_storage_scope::initialize_current(manager).await?;
    super::m20240101_000034_continuation_storage_scope::initialize_current(manager).await?;
    super::m20240101_000037_owned_oauth::initialize_current(manager).await?;
    super::m20240101_000038_eval_import_provenance::initialize_current(manager).await?;
    super::m20240101_000039_resource_lifecycle::initialize_current(manager).await?;
    receipts(manager).await?;
    Ok(())
}

async fn receipts(manager: &SchemaManager<'_>) -> Result<(), DbErr> {
    use super::{
        m20240101_000035_execution_receipts as receipts, m20240101_000040_usage_evidence as usage,
        m20240101_000042_receipt_archive as archive,
        m20240101_000043_unknown_obligations as obligations,
        m20240101_000045_ordered_receipts as ordered,
    };
    let backend = manager.get_database_backend();
    let mut receipt = receipts::definition(backend);
    let mut evidence = usage::definition(backend);
    if backend == DatabaseBackend::Sqlite {
        receipt.col(archive::marker());
        evidence.col(archive::marker());
    }
    receipt.col(ordered::format_column());
    manager.create_table(receipt).await?;
    manager.create_index(receipts::correlation()).await?;
    manager.create_table(evidence).await?;
    manager.create_table(ordered::intent_table(backend)).await?;
    manager.create_index(ordered::directory_index()).await?;
    if backend == DatabaseBackend::Sqlite {
        manager.create_table(archive::definition()).await?;
        manager.create_index(archive::storage_index()).await?;
        obligations::initialize_current(manager).await?;
    }
    Ok(())
}

/// Shared read-only validators remain usable by startup and stopped recovery.
pub(super) async fn validate(db: &impl ConnectionTrait) -> Result<(), DbErr> {
    if db.get_database_backend() != DatabaseBackend::Sqlite {
        crate::db::current_schema::validate(db).await?;
    }
    validate_tables(db).await
}

async fn validate_tables(db: &impl ConnectionTrait) -> Result<(), DbErr> {
    super::m20240101_000040_usage_evidence::validate(db).await?;
    super::m20240101_000045_ordered_receipts::validate(db).await?;
    if db.get_database_backend() == DatabaseBackend::Sqlite {
        super::m20240101_000044_capacity_upgrade::validate_authority(db).await?;
        super::m20240101_000043_unknown_obligations::validate(db).await?;
        super::m20240101_000037_owned_oauth::validate(db).await?;
    }
    Ok(())
}

#[cfg(all(test, any(feature = "db-postgres", feature = "db-mysql")))]
mod server_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::{Result, ensure};

    #[derive(serde::Deserialize)]
    struct SchemaObject {
        r#type: String,
        name: String,
        tbl_name: String,
        sql: String,
    }

    fn compact_sql(sql: &str) -> String {
        let mut quoted = None;
        let mut out = String::new();
        for character in sql.chars() {
            if let Some(quote) = quoted {
                out.push(character);
                if character == quote {
                    quoted = None;
                }
            } else if character == '\'' || character == '"' {
                quoted = Some(character);
                out.push(character);
            } else if !character.is_ascii_whitespace() {
                out.push(character.to_ascii_lowercase());
            }
        }
        out
    }

    #[tokio::test]
    async fn sqlite_current_baseline_matches_frozen_catalog() -> Result<()> {
        // Pin the service's table inventory independently of the production
        // classifier. Every retained table and index must match the M0 DDL.
        const SERVICE_TABLES: &[&str] = &[
            "api_keys",
            "checkpoint_evolution",
            "eval_admission_events",
            "eval_import_provenance",
            "eval_results",
            "eval_snapshots",
            "eval_subjects",
            "provider_continuation_key_epoch",
            "provider_continuations",
            "requests",
            "seaql_migrations",
            "tenancy_active_configurations",
            "tenancy_bindings",
            "tenancy_configurations",
            "tenancy_continuation_epoch",
            "tenancy_credentials",
            "tenancy_dispatch_intents",
            "tenancy_execution_receipts",
            "tenancy_installation",
            "tenancy_oauth_slots",
            "tenancy_operations",
            "tenancy_principals",
            "tenancy_receipt_archive",
            "tenancy_receipt_auxiliary",
            "tenancy_resource_lifecycle",
            "tenancy_resource_revisions",
            "tenancy_tenants",
            "tenancy_unknown_obligations",
            "tenancy_usage_evidence",
            "trajectory_episodes",
            "trajectory_events",
            "trajectory_outbox",
            "trajectory_prefix_index",
            "trajectory_requests",
            "users",
        ];
        let mixed: Vec<SchemaObject> =
            serde_json::from_str(include_str!("baseline/fixtures/sqlite-current-schema.json"))?;
        ensure!(mixed.len() == 113);
        let frozen: Vec<_> = mixed
            .into_iter()
            .filter(|object| SERVICE_TABLES.contains(&object.tbl_name.as_str()))
            .collect();
        ensure!(frozen.len() == 78);
        let db = crate::db::connect("sqlite::memory:").await?;
        super::super::Migrator::up(&db, None).await?;
        let rows = db.query_all(Statement::from_string(DatabaseBackend::Sqlite,
            "SELECT type,name,tbl_name,sql FROM sqlite_schema WHERE name NOT LIKE 'sqlite_%' ORDER BY type,name")).await?;
        ensure!(
            rows.len() == frozen.len(),
            "current schema object count differs from reviewed catalog"
        );
        let expected: std::collections::BTreeMap<_, _> = frozen
            .iter()
            .map(|object| {
                (
                    (object.r#type.as_str(), object.name.as_str()),
                    (object.tbl_name.as_str(), compact_sql(&object.sql)),
                )
            })
            .collect();
        for row in rows {
            let kind: String = row.try_get("", "type")?;
            let name: String = row.try_get("", "name")?;
            let table: String = row.try_get("", "tbl_name")?;
            let sql: String = row.try_get("", "sql")?;
            let frozen = expected
                .get(&(kind.as_str(), name.as_str()))
                .ok_or_else(|| anyhow::anyhow!("unreviewed schema object {kind}/{name}"))?;
            ensure!(
                table == frozen.0 && compact_sql(&sql) == frozen.1,
                "current schema differs from frozen catalog: {kind}/{name}"
            );
        }
        validate(&db).await?;
        db.close().await?;
        Ok(())
    }

    #[tokio::test]
    async fn fresh_baseline_has_current_receipts_and_one_atomic_marker() -> Result<()> {
        let db = crate::db::connect("sqlite::memory:").await?;
        super::super::Migrator::up(&db, None).await?;
        ensure!(store_format::inspect(&db).await? == State::Current);
        validate(&db).await?;
        let before = db
            .query_all(Statement::from_string(
                DatabaseBackend::Sqlite,
                "SELECT name,sql FROM sqlite_schema ORDER BY name",
            ))
            .await?;
        let ledger = super::super::Migrator::get_migration_models(&db).await?;
        ensure!(
            ledger.len() == 1
                && ledger[0].version == crate::db::mixed_store::Owner::BitRouter.marker()
        );
        super::super::Migrator::up(&db, None).await?;
        let after = db
            .query_all(Statement::from_string(
                DatabaseBackend::Sqlite,
                "SELECT name,sql FROM sqlite_schema ORDER BY name",
            ))
            .await?;
        ensure!(before.len() == after.len());
        for (before, after) in before.iter().zip(&after) {
            ensure!(before.try_get::<String>("", "name")? == after.try_get::<String>("", "name")?);
            ensure!(
                before.try_get::<Option<String>>("", "sql")?
                    == after.try_get::<Option<String>>("", "sql")?
            );
        }
        ensure!(super::super::Migrator::get_migration_models(&db).await? == ledger);
        db.close().await?;
        Ok(())
    }

    #[tokio::test]
    async fn old_or_unmarked_stores_refuse_without_ledger_mutation() -> Result<()> {
        for marked in [false, true] {
            let db = crate::db::connect("sqlite::memory:").await?;
            db.execute_unprepared(
                "CREATE TABLE preserved (value TEXT); INSERT INTO preserved VALUES ('original')",
            )
            .await?;
            if marked {
                db.execute_unprepared("CREATE TABLE seaql_migrations (version TEXT PRIMARY KEY,applied_at BIGINT NOT NULL); INSERT INTO seaql_migrations VALUES ('m20240101_000048_native_generate_content',1)").await?;
            }
            ensure!(super::super::Migrator::up(&db, None).await.is_err());
            ensure!(
                SchemaManager::new(&db)
                    .has_table("seaql_migrations")
                    .await?
                    == marked
            );
            ensure!(
                !SchemaManager::new(&db)
                    .has_table("tenancy_installation")
                    .await?
            );
            ensure!(
                db.query_one(Statement::from_string(
                    DatabaseBackend::Sqlite,
                    "SELECT value FROM preserved WHERE value='original'"
                ))
                .await?
                .is_some()
            );
            db.close().await?;
        }
        Ok(())
    }
}
