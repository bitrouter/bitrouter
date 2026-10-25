//! Opt-in isolated DDL tests. Never reuse or clean an existing continuation table.
use super::*;
use anyhow::Context;
use sea_orm::{
    ConnectOptions,
    sea_query::{Expr, Query},
};
use std::time::Duration;

const TABLE: &str = "provider_continuations";
const VIEW: &str = "continuation034_external_view";

#[tokio::test]
#[ignore = "requires BITROUTER_TEST_MIGRATION_DATABASE_URL for a dedicated database without provider_continuations"]
async fn server_migration034_preserves_legacy_bytes_exact_scopes_and_dependencies() -> Result<()> {
    let address = std::env::var("BITROUTER_TEST_MIGRATION_DATABASE_URL")
        .context("dedicated migration database URL required; no fallback")?;
    let parsed = url::Url::parse(&address).context("invalid migration database URL")?;
    match parsed.scheme() {
        "postgres" | "postgresql" => ensure!(cfg!(feature = "db-postgres"), "enable db-postgres"),
        "mysql" => ensure!(cfg!(feature = "db-mysql"), "enable db-mysql"),
        _ => anyhow::bail!("migration fixture requires PostgreSQL or MySQL"),
    }
    ensure!(
        !parsed.path().trim_matches('/').is_empty(),
        "dedicated database name required"
    );
    tokio::time::timeout(Duration::from_secs(120), exercise(&address))
        .await
        .context("migration034 scenarios exceeded two minutes; inspect owned fixture table")?
}

async fn connect(address: &str) -> Result<DatabaseConnection> {
    let mut options = ConnectOptions::new(address);
    options
        .min_connections(1)
        .max_connections(1)
        .sqlx_logging(false);
    Database::connect(options)
        .await
        .map_err(|_| anyhow::anyhow!("migration fixture connection unavailable"))
}
async fn cleanup(db: &DatabaseConnection) -> Result<()> {
    SchemaManager::new(db)
        .drop_table(Table::drop().table(Alias::new(TABLE)).to_owned())
        .await?;
    Ok(())
}
async fn matches(db: &DatabaseConnection, column: &str, value: &str) -> Result<bool> {
    let query = Query::select()
        .expr(Expr::val(1_i32))
        .from(Alias::new(TABLE))
        .and_where(Expr::col(Alias::new(column)).eq(value))
        .limit(1)
        .to_owned();
    Ok(db
        .query_one(db.get_database_backend().build(&query))
        .await?
        .is_some())
}
async fn set_scope(db: &DatabaseConnection, value: Option<&str>) -> Result<()> {
    let mut query = Query::update();
    query.table(Alias::new(TABLE));
    for column in COLUMNS {
        query.value(Alias::new(column), value);
    }
    db.execute(db.get_database_backend().build(&query)).await?;
    Ok(())
}
async fn check_indexes(manager: &SchemaManager<'_>) -> Result<()> {
    for index in INDEXES {
        validate_index(manager, &index).await?;
    }
    Ok(())
}
async fn prepared(db: &DatabaseConnection) -> Result<Vec<Option<String>>> {
    let manager = SchemaManager::new(db);
    let mut insert = Query::insert();
    insert
        .into_table(Alias::new(TABLE))
        .columns(
            [
                "continuation_identity",
                "owner_identity",
                "ciphertext",
                "nonce",
                "target_fingerprint",
                "key_id",
                "cipher_version",
                "created_at",
                "expires_at",
                "purge_after",
                "publication_state",
                "publication_generation",
                "publication_instance_id",
                "publication_lease_until",
            ]
            .map(Alias::new),
        )
        .values([
            "LegacyCase  ".into(),
            "LegacyOwner  ".into(),
            "opaque é\nCipher bytes  ".into(),
            Option::<String>::None.into(),
            "TargetCase  ".into(),
            "KeyCase ".into(),
            1_i32.into(),
            "2020-01-01T00:00:00+00:00".into(),
            "ExpiresOriginal  ".into(),
            "PurgeOriginal  ".into(),
            "active".into(),
            "LegacyGeneration  ".into(),
            "LegacyProducer  ".into(),
            "LeaseOriginal  ".into(),
        ])?;
    db.execute(db.get_database_backend().build(&insert)).await?;
    let before = legacy(db).await?;
    // Simulate an implicit-DDL-commit interruption: only one additive column.
    manager
        .alter_table(
            Table::alter()
                .table(Alias::new(TABLE))
                .add_column(identity(db.get_database_backend(), COLUMNS[0], 128, true))
                .to_owned(),
        )
        .await?;
    Migration.up(&manager).await?;
    Migration.up(&manager).await?;
    check_indexes(&manager).await?;
    ensure!(legacy(db).await? == before);
    for column in COLUMNS {
        let query = Query::select()
            .column(Alias::new(column))
            .from(Alias::new(TABLE))
            .to_owned();
        let row = db
            .query_one(db.get_database_backend().build(&query))
            .await?
            .context("legacy row")?;
        ensure!(row.try_get::<Option<String>>("", column)?.is_none());
    }
    set_scope(db, Some("OwnerCase")).await?;
    for column in COLUMNS {
        ensure!(matches(db, column, "OwnerCase").await?);
        ensure!(!matches(db, column, "ownercase").await?);
        ensure!(!matches(db, column, "OwnerCase ").await?);
    }
    ensure!(Migration.down(&manager).await.is_err());
    check_indexes(&manager).await?;
    ensure!(legacy(db).await? == before);
    set_scope(db, None).await?;
    // A dependent view must be refused before dropping even the first index.
    db.execute_unprepared(&format!(
        "CREATE VIEW {VIEW} AS SELECT installation_id FROM {TABLE}"
    ))
    .await?;
    let refused: Result<()> = async {
        ensure!(Migration.down(&manager).await.is_err());
        check_indexes(&manager).await?;
        ensure!(legacy(db).await? == before);
        Ok(())
    }
    .await;
    db.execute_unprepared(&format!("DROP VIEW {VIEW}")).await?;
    refused?;
    if db.get_database_backend() == DatabaseBackend::Postgres {
        db.execute_unprepared("ALTER TABLE provider_continuations SET UNLOGGED")
            .await?;
        ensure!(
            Migration.up(&manager).await.is_err(),
            "UNLOGGED continuation table adopted"
        );
        ensure!(legacy(db).await? == before);
        check_indexes(&manager).await?;
        db.execute_unprepared("ALTER TABLE provider_continuations SET LOGGED")
            .await?;
    }
    Ok(before)
}
async fn exercise(address: &str) -> Result<()> {
    let db = connect(address).await?;
    let manager = SchemaManager::new(&db);
    ensure!(
        !manager.has_table(TABLE).await?,
        "dedicated database already contains provider_continuations"
    );
    // Create only this test's predecessor table. Do not initialize or remove
    // the historical key-epoch table that migration013 also manages.
    manager.create_table(crate::db::migration::m20240101_000013_create_continuation_registry::provider_continuations_table()).await?;
    let before = match prepared(&db).await {
        Ok(before) => before,
        Err(error) => {
            let cleaned = cleanup(&db).await;
            let closed = db.close().await;
            cleaned?;
            closed?;
            return Err(error);
        }
    };
    db.close().await?;
    let db = connect(address).await?;
    let result: Result<()> = async {
        let manager = SchemaManager::new(&db);
        Migration.up(&manager).await?;
        ensure!(legacy(&db).await? == before);
        Migration.down(&manager).await?;
        ensure!(legacy(&db).await? == before);
        for column in COLUMNS {
            ensure!(!manager.has_column(TABLE, column).await?);
        }
        Ok(())
    }
    .await;
    let cleaned = cleanup(&db).await;
    let closed = db.close().await;
    result?;
    cleaned?;
    closed?;
    Ok(())
}
