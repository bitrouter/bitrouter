mod sqlite_dependencies;

use super::*;
use anyhow::{Context, Result, ensure};
#[cfg(any(feature = "db-postgres", feature = "db-mysql"))]
use sea_orm::ConnectOptions;
use sea_orm::{Database, DatabaseConnection};
use sea_orm_migration::MigratorTrait;

const OLD_COLUMNS: [&[&str]; 5] = [
    &[
        "episode_id",
        "owner_user_id",
        "correlation_digest",
        "correlation_key_id",
        "correlation_source",
        "history_completeness",
        "next_sequence",
        "first_captured_at",
        "last_captured_at",
        "closed_at",
        "latest_request_id",
    ],
    &[
        "event_id",
        "owner_user_id",
        "episode_id",
        "request_id",
        "sequence",
        "kind",
        "event_json",
        "content_digest",
        "captured_at",
    ],
    &[
        "request_id",
        "owner_user_id",
        "episode_id",
        "start_event_id",
        "settlement_event_id",
        "settlement_outbox_id",
        "full_input_digest",
        "native_parent_id",
        "protocol",
        "status",
    ],
    &[
        "owner_user_id",
        "full_input_digest",
        "episode_id",
        "ambiguous",
    ],
    &[
        "outbox_id",
        "owner_user_id",
        "topic",
        "payload_json",
        "payload_digest",
        "attempts",
        "created_at",
        "delivered_at",
    ],
];
async fn seed(db: &DatabaseConnection) -> Result<()> {
    insert(
        db,
        TABLES[0],
        OLD_COLUMNS[0],
        vec![
            "legacy episode_id é ".into(),
            "legacy owner_user_id é ".into(),
            "legacy correlation_digest é ".into(),
            "legacy correlation_key_id é ".into(),
            "legacy correlation_source é ".into(),
            "legacy history_completeness é ".into(),
            1_i64.into(),
            "legacy first_captured_at é ".into(),
            "legacy last_captured_at é ".into(),
            Option::<String>::None.into(),
            Option::<String>::None.into(),
        ],
    )
    .await?;
    insert(
        db,
        TABLES[1],
        OLD_COLUMNS[1],
        vec![
            "legacy event_id é ".into(),
            "legacy owner_user_id é ".into(),
            "legacy episode_id é ".into(),
            Option::<String>::None.into(),
            1_i64.into(),
            "legacy kind é ".into(),
            "{\n  \"z\": \"e\\u0301 CASE\", \"a\": 1\n}  ".into(),
            "legacy content_digest é ".into(),
            "legacy captured_at é ".into(),
        ],
    )
    .await?;
    insert(
        db,
        TABLES[2],
        OLD_COLUMNS[2],
        vec![
            "legacy request_id é ".into(),
            "legacy owner_user_id é ".into(),
            "legacy episode_id é ".into(),
            "legacy start_event_id é ".into(),
            Option::<String>::None.into(),
            Option::<String>::None.into(),
            "legacy full_input_digest é ".into(),
            Option::<String>::None.into(),
            "legacy protocol é ".into(),
            "legacy status é ".into(),
        ],
    )
    .await?;
    insert(
        db,
        TABLES[3],
        OLD_COLUMNS[3],
        vec![
            "legacy owner_user_id é ".into(),
            "legacy full_input_digest é ".into(),
            "legacy episode_id é ".into(),
            false.into(),
        ],
    )
    .await?;
    insert(
        db,
        TABLES[4],
        OLD_COLUMNS[4],
        vec![
            "legacy outbox_id é ".into(),
            "legacy owner_user_id é ".into(),
            "legacy topic é ".into(),
            "{\n  \"z\": \"e\\u0301 CASE\", \"a\": 1\n}  ".into(),
            "legacy payload_digest é ".into(),
            1_i64.into(),
            "legacy created_at é ".into(),
            Option::<String>::None.into(),
        ],
    )
    .await?;
    Ok(())
}

async fn insert(
    db: &DatabaseConnection,
    table: &str,
    columns: &[&str],
    values: Vec<Value>,
) -> Result<()> {
    let mut statement = Query::insert();
    statement
        .into_table(Alias::new(table))
        .columns(columns.iter().map(|c| Alias::new(*c)));
    statement.values(values.into_iter().map(Into::into))?;
    db.execute(db.get_database_backend().build(&statement))
        .await?;
    Ok(())
}
async fn historical(db: &DatabaseConnection) -> Result<Vec<Vec<Vec<Option<String>>>>> {
    let mut result = Vec::new();
    let cast = if db.get_database_backend() == DatabaseBackend::MySql {
        "CHAR"
    } else {
        "TEXT"
    };
    for (table, columns) in TABLES.iter().zip(OLD_COLUMNS) {
        let mut statement = Query::select();
        statement.from(Alias::new(*table));
        for column in columns {
            statement.expr_as(
                Expr::col(Alias::new(*column)).cast_as(Alias::new(cast)),
                Alias::new(*column),
            );
        }
        statement.order_by(Alias::new(columns[0]), Order::Asc);
        let mut rows = Vec::new();
        for row in db
            .query_all(db.get_database_backend().build(&statement))
            .await?
        {
            rows.push(
                columns
                    .iter()
                    .map(|c| row.try_get::<Option<String>>("", c))
                    .collect::<Result<Vec<_>, _>>()?,
            );
        }
        result.push(rows);
    }
    Ok(result)
}
async fn predecessor(db: &DatabaseConnection) -> Result<()> {
    let manager = SchemaManager::new(db);
    for table in TABLES {
        ensure!(
            !manager.has_table(table).await?,
            "refusing existing trajectory fixture table"
        );
    }
    crate::db::migration::m20240101_000012_create_trajectory_ledger::Migration
        .up(&manager)
        .await?;
    Ok(())
}
async fn matrix(db: &DatabaseConnection) -> Result<()> {
    predecessor(db).await?;
    seed(db).await?;
    let bytes = historical(db).await?;
    let manager = SchemaManager::new(db);
    // Simulate interrupted implicit-commit DDL, then resume twice.
    manager
        .alter_table(
            Table::alter()
                .table(Alias::new(TABLES[0]))
                .add_column(identity(
                    db.get_database_backend(),
                    "installation_id",
                    128,
                    true,
                ))
                .to_owned(),
        )
        .await?;
    Migration.up(&manager).await?;
    Migration.up(&manager).await?;
    ensure!(
        historical(db).await? == bytes,
        "migration rewrote historical payloads"
    );
    // Last-table occupancy must refuse down before removing any first-table column/index.
    let mut update = Query::update();
    update
        .table(Alias::new(TABLES[4]))
        .value(Alias::new("creator_principal_id"), "creator");
    db.execute(db.get_database_backend().build(&update)).await?;
    ensure!(Migration.down(&manager).await.is_err());
    for table in TABLES {
        for &column in columns(table) {
            ensure!(has_column(&manager, table, column).await?);
        }
    }
    let mut update = Query::update();
    update
        .table(Alias::new(TABLES[4]))
        .value(Alias::new("creator_principal_id"), Option::<String>::None);
    db.execute(db.get_database_backend().build(&update)).await?;
    // An unrelated index survives; a dependent nonunique index refuses pre-DDL.
    manager
        .create_index(
            Index::create()
                .name("trajectory32_fixture_unrelated")
                .table(Alias::new(TABLES[4]))
                .col(Alias::new("topic"))
                .to_owned(),
        )
        .await?;
    manager
        .create_index(
            Index::create()
                .name("trajectory32_fixture_dependency")
                .table(Alias::new(TABLES[4]))
                .col(Alias::new("tenant_id"))
                .to_owned(),
        )
        .await?;
    ensure!(Migration.down(&manager).await.is_err());
    ensure!(manager.has_index(INDEXES[0].table, INDEXES[0].name).await?);
    manager
        .drop_index(
            Index::drop()
                .name("trajectory32_fixture_dependency")
                .table(Alias::new(TABLES[4]))
                .to_owned(),
        )
        .await?;
    Migration.down(&manager).await?;
    ensure!(
        manager
            .has_index(TABLES[4], "trajectory32_fixture_unrelated")
            .await?
    );
    ensure!(historical(db).await? == bytes);
    Migration.up(&manager).await?;
    ensure!(historical(db).await? == bytes);
    Ok(())
}
#[tokio::test]
async fn sqlite_additive_resume_and_all_table_down_preflight() -> Result<()> {
    let db = Database::connect("sqlite::memory:").await?;
    matrix(&db).await
}
#[tokio::test]
async fn sqlite_historical_migration_ledger_is_unchanged() -> Result<()> {
    let db = Database::connect("sqlite::memory:").await?;
    let all = crate::db::migration::Migrator::migrations();
    let count = all
        .iter()
        .position(|m| m.name() == Migration.name())
        .context("register migration032")?;
    crate::db::migration::Migrator::up(&db, Some(count.try_into()?)).await?;
    seed(&db).await?;
    let bytes = historical(&db).await?;
    let before = crate::db::migration::Migrator::get_migration_models(&db).await?;
    crate::db::migration::Migrator::up(&db, None).await?;
    let after = crate::db::migration::Migrator::get_migration_models(&db).await?;
    ensure!(after.starts_with(&before));
    ensure!(
        after
            .iter()
            .map(|m| m.version.as_str())
            .eq(all.iter().map(|m| m.name()))
    );
    ensure!(historical(&db).await? == bytes);
    crate::db::migration::Migrator::up(&db, None).await?;
    ensure!(after == crate::db::migration::Migrator::get_migration_models(&db).await?);
    Ok(())
}
#[tokio::test]
async fn sqlite_incompatible_last_column_refuses_before_any_addition() -> Result<()> {
    let db = Database::connect("sqlite::memory:").await?;
    predecessor(&db).await?;
    seed(&db).await?;
    let bytes = historical(&db).await?;
    let manager = SchemaManager::new(&db);
    db.execute(Statement::from_string(
        DatabaseBackend::Sqlite,
        "ALTER TABLE trajectory_outbox ADD COLUMN tenant_id varchar(128) COLLATE NOCASE".to_owned(),
    ))
    .await?;
    ensure!(Migration.up(&manager).await.is_err());
    ensure!(!has_column(&manager, TABLES[0], "installation_id").await?);
    ensure!(historical(&db).await? == bytes);
    Ok(())
}
#[test]
fn catalog_quoted_literals_are_not_identity_dependencies() -> Result<()> {
    let unrelated = mysql_catalog_tokens("(`topic` = _utf8mb4\'tenant_id\')")?;
    ensure!(!unrelated.contains(&Token::Word("tenant_id".into())));
    ensure!(
        sqlite_tokens("topic = 'tenant_id'")?
            .iter()
            .all(|t| *t != Token::Word("tenant_id".into()))
    );
    Ok(())
}
#[cfg(any(feature = "db-postgres", feature = "db-mysql"))]
#[tokio::test]
#[ignore = "requires BITROUTER_TEST_MIGRATION_DATABASE_URL for a dedicated database without trajectory tables"]
async fn server_migration032_preserves_legacy_and_refuses_partial_down() -> Result<()> {
    let address = std::env::var("BITROUTER_TEST_MIGRATION_DATABASE_URL")?;
    let parsed = url::Url::parse(&address)?;
    ensure!(
        !parsed.path().trim_matches('/').is_empty(),
        "dedicated database name required"
    );
    match parsed.scheme() {
        "postgres" | "postgresql" => ensure!(cfg!(feature = "db-postgres")),
        "mysql" => ensure!(cfg!(feature = "db-mysql")),
        _ => anyhow::bail!("server URL required"),
    }
    let mut options = ConnectOptions::new(address);
    options
        .max_connections(1)
        .min_connections(1)
        .sqlx_logging(false);
    let db = Database::connect(options).await?;
    // No cleanup is attempted on failure; evidence and owned fixture tables remain.
    // Refuse all existing table names before predecessor creates anything.
    tokio::time::timeout(std::time::Duration::from_secs(120), matrix(&db)).await??;
    db.close().await?;
    Ok(())
}
