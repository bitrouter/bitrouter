use super::*;
use anyhow::{Result, ensure};
use sea_orm::{Database, DatabaseConnection};
use sea_orm_migration::MigratorTrait;

mod server;

async fn fixture() -> Result<DatabaseConnection> {
    let db = Database::connect("sqlite::memory:").await?;
    // Keep every predecessor, including033 when integrated, as the baseline.
    // Never interpret None/latest or a numeric migration suffix as a count.
    let before = super::super::Migrator::migrations()
        .iter()
        .position(|migration| migration.name() == Migration.name())
        .ok_or_else(|| anyhow::anyhow!("migration034 is not registered"))?;
    super::super::Migrator::up(&db, Some(before.try_into()?)).await?;
    db.execute(Statement::from_string(DatabaseBackend::Sqlite,
        "INSERT INTO provider_continuations VALUES ('legacy id é ','legacy owner ','cipher Exact  \\n',NULL,'target Exact ','key ','1','2020-01-01T00:00:00+00:00','expires ','purge ','active','generation ','producer ','lease ')".to_owned())).await?;
    Ok(db)
}

async fn schema(db: &DatabaseConnection) -> Result<Vec<(String, String, String, Option<String>)>> {
    db.query_all(Statement::from_string(
        DatabaseBackend::Sqlite,
        "SELECT type,name,tbl_name,sql FROM sqlite_schema ORDER BY type,name".to_owned(),
    ))
    .await?
    .into_iter()
    .map(|row| {
        Ok((
            row.try_get("", "type")?,
            row.try_get("", "name")?,
            row.try_get("", "tbl_name")?,
            row.try_get("", "sql")?,
        ))
    })
    .collect()
}
async fn legacy(db: &DatabaseConnection) -> Result<Vec<Option<String>>> {
    let row = db.query_one(Statement::from_string(db.get_database_backend(),
        "SELECT continuation_identity,owner_identity,ciphertext,nonce,target_fingerprint,key_id,created_at,expires_at,purge_after,publication_state,publication_generation,publication_instance_id,publication_lease_until FROM provider_continuations".to_owned())).await?
        .ok_or_else(|| anyhow::anyhow!("missing historical row"))?;
    [
        "continuation_identity",
        "owner_identity",
        "ciphertext",
        "nonce",
        "target_fingerprint",
        "key_id",
        "created_at",
        "expires_at",
        "purge_after",
        "publication_state",
        "publication_generation",
        "publication_instance_id",
        "publication_lease_until",
    ]
    .iter()
    .map(|name| Ok(row.try_get("", name)?))
    .collect()
}
#[tokio::test]
async fn additive_resume_and_down_preserve_historical_bytes() -> Result<()> {
    let db = fixture().await?;
    let before_schema = schema(&db).await?;
    let before_row = legacy(&db).await?;
    let manager = SchemaManager::new(&db);
    manager
        .alter_table(
            Table::alter()
                .table(Alias::new(TABLES[0]))
                .add_column(identity(DatabaseBackend::Sqlite, COLUMNS[0], 128, true))
                .to_owned(),
        )
        .await?;
    Migration.up(&manager).await?;
    Migration.up(&manager).await?;
    ensure!(legacy(&db).await? == before_row);
    let row=db.query_one(Statement::from_string(DatabaseBackend::Sqlite,
        "SELECT installation_id,tenant_id,creator_principal_id,cipher_version FROM provider_continuations".to_owned())).await?
        .ok_or_else(|| anyhow::anyhow!("missing historical row"))?;
    for column in COLUMNS {
        ensure!(row.try_get::<Option<String>>("", column)?.is_none());
    }
    ensure!(row.try_get::<i32>("", "cipher_version")? == 1);
    Migration.down(&manager).await?;
    ensure!(schema(&db).await? == before_schema);
    ensure!(legacy(&db).await? == before_row);
    Ok(())
}
#[tokio::test]
async fn dependent_view_or_external_trigger_refuses_before_any_ddl() -> Result<()> {
    // Test both dependencies separately: an early view refusal must not conceal
    // the trigger-on-another-table defect. Single-quoted table spelling too.
    for dependent in [
        "CREATE VIEW outside_view AS SELECT installation_id FROM provider_continuations",
        "CREATE TRIGGER outside_trigger AFTER INSERT ON other_table BEGIN SELECT tenant_id FROM provider_continuations; END",
        "CREATE VIEW quoted_view AS SELECT creator_principal_id FROM 'provider_continuations'",
    ] {
        let db = fixture().await?;
        let manager = SchemaManager::new(&db);
        Migration.up(&manager).await?;
        db.execute(Statement::from_string(
            DatabaseBackend::Sqlite,
            "CREATE TABLE other_table (id INTEGER)".to_owned(),
        ))
        .await?;
        db.execute(Statement::from_string(
            DatabaseBackend::Sqlite,
            dependent.to_owned(),
        ))
        .await?;
        let before_schema = schema(&db).await?;
        let before_row = legacy(&db).await?;
        ensure!(Migration.down(&manager).await.is_err());
        ensure!(
            schema(&db).await? == before_schema,
            "DDL changed on refusal"
        );
        ensure!(
            legacy(&db).await? == before_row,
            "legacy bytes changed on refusal"
        );
        for index in INDEXES {
            ensure!(manager.has_index(index.table, index.name).await?);
        }
    }
    Ok(())
}
#[tokio::test]
async fn partial_provenance_cannot_be_downgraded_to_legacy() -> Result<()> {
    let db = fixture().await?;
    let manager = SchemaManager::new(&db);
    Migration.up(&manager).await?;
    db.execute(Statement::from_string(
        DatabaseBackend::Sqlite,
        "UPDATE provider_continuations SET tenant_id='tenant'".to_owned(),
    ))
    .await?;
    let before = schema(&db).await?;
    ensure!(Migration.down(&manager).await.is_err());
    ensure!(schema(&db).await? == before);
    Ok(())
}

#[tokio::test]
async fn erased_provenance_on_v2_row_cannot_be_downgraded() -> Result<()> {
    let db = fixture().await?;
    let manager = SchemaManager::new(&db);
    Migration.up(&manager).await?;
    db.execute_unprepared("UPDATE provider_continuations SET cipher_version=2")
        .await?;
    let before = schema(&db).await?;
    let bytes = legacy(&db).await?;
    ensure!(Migration.down(&manager).await.is_err());
    ensure!(schema(&db).await? == before);
    ensure!(legacy(&db).await? == bytes);
    Ok(())
}
