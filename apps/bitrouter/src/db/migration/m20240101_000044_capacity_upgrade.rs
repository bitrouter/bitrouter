//! Durable result for the one stopped-owner legacy count-fence transition.
//! Creating this empty table never changes authority format or clears a stop.
use sea_orm::{ConnectionTrait, DatabaseBackend, Statement};
use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;
pub(crate) const TABLE: &str = "tenancy_capacity_upgrades";

pub(crate) fn definition() -> TableCreateStatement {
    let mut table = Table::create();
    table.table(Alias::new(TABLE));
    for (name, width) in [
        ("installation_id", 128),
        ("operation_id", 128),
        ("request_digest", 64),
    ] {
        table.col(super::m20240101_000028_resource_tenancy::identity(
            DatabaseBackend::Sqlite,
            name,
            width,
            false,
        ));
    }
    table.col(ColumnDef::new(Alias::new("result_json")).text().not_null());
    table.primary_key(
        Index::create()
            .col(Alias::new("installation_id"))
            .col(Alias::new("operation_id")),
    );
    table.to_owned()
}

fn normalized(sql: &str) -> String {
    sql.to_ascii_lowercase()
        .replace("if not exists", "")
        .chars()
        .filter(|c| !c.is_ascii_whitespace())
        .collect()
}

async fn table(db: &impl ConnectionTrait, definition: TableCreateStatement) -> Result<(), DbErr> {
    let expected = definition.to_string(SqliteQueryBuilder);
    // DDL is generated locally. Extract its quoted table identity, never an
    // untrusted SQL identifier. SQLite preserves CREATE text except IF NOT EXISTS.
    let name = expected.split('"').nth(1).ok_or_else(incompatible)?;
    let rows = db.query_all(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
        "SELECT type,CASE WHEN length(CAST(sql AS BLOB))<=65536 THEN sql ELSE NULL END AS sql FROM sqlite_schema WHERE name=? LIMIT 2", [name.into()])).await?;
    if rows.len() != 1
        || rows[0].try_get::<String>("", "type")? != "table"
        || normalized(&rows[0].try_get::<String>("", "sql")?) != normalized(&expected)
    {
        return Err(incompatible());
    }
    if db
        .query_one(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "SELECT 1 FROM sqlite_schema WHERE tbl_name=? AND type='trigger' LIMIT 1",
            [name.into()],
        ))
        .await?
        .is_some()
    {
        return Err(incompatible());
    }
    if db.query_one(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
        "SELECT 1 FROM temp.sqlite_schema WHERE lower(name)=? OR (type='trigger' AND lower(tbl_name)=?) LIMIT 1", [name.into(), name.into()])).await?.is_some() {
        return Err(incompatible());
    }
    let indexes = db.query_all(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
        "SELECT CASE WHEN length(CAST(sql AS BLOB))<=65536 THEN sql ELSE NULL END AS sql FROM sqlite_schema WHERE tbl_name=? AND type='index' AND sql IS NOT NULL LIMIT 2", [name.into()])).await?;
    if name == "tenancy_credentials" {
        let expected = Index::create()
            .name("idx_tenancy_credentials_state")
            .table(Alias::new(name))
            .col(Alias::new("state"))
            .col(Alias::new("tenant_id"))
            .to_string(SqliteQueryBuilder);
        if indexes.len() != 1
            || normalized(&indexes[0].try_get::<String>("", "sql")?) != normalized(&expected)
        {
            return Err(incompatible());
        }
    } else if !indexes.is_empty() {
        return Err(incompatible());
    }
    Ok(())
}
fn incompatible() -> DbErr {
    DbErr::Custom("incompatible capacity upgrade authority schema".into())
}

/// Read-only exact authority table profile shared by startup-independent tools.
pub(crate) async fn validate_authority(db: &impl ConnectionTrait) -> Result<(), DbErr> {
    if db.get_database_backend() != DatabaseBackend::Sqlite {
        return Err(incompatible());
    }
    let mut definitions =
        super::m20240101_000028_resource_tenancy::statements(DatabaseBackend::Sqlite);
    definitions.extend(super::m20240101_000029_tenancy_configurations::statements(
        DatabaseBackend::Sqlite,
    ));
    definitions.push(super::m20240101_000039_resource_lifecycle::definition(
        DatabaseBackend::Sqlite,
    ));
    for definition in definitions {
        table(db, definition).await?;
    }
    Ok(())
}

pub(crate) async fn validate(db: &impl ConnectionTrait) -> Result<(), DbErr> {
    if db.get_database_backend() != DatabaseBackend::Sqlite {
        return Err(incompatible());
    }
    table(db, definition()).await
}

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        if manager.get_database_backend() != DatabaseBackend::Sqlite {
            return Ok(());
        }
        if !manager.has_table(TABLE).await? {
            manager.create_table(definition()).await?;
        }
        validate(manager.get_connection()).await
    }
    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        if manager.get_database_backend() == DatabaseBackend::Sqlite
            && manager.has_table(TABLE).await?
        {
            return Err(DbErr::Custom(
                "capacity upgrade requires verified-backup rollback".into(),
            ));
        }
        Ok(())
    }
}
