//! Read-only current authority schema validation, shared with stopped recovery.
use sea_orm::{ConnectionTrait, DatabaseBackend, Statement};
use sea_orm_migration::prelude::*;

fn normalized(sql: &str) -> String {
    let mut quote = None;
    let mut normalized = String::new();
    for character in sql.chars() {
        if let Some(delimiter) = quote {
            normalized.push(character);
            if character == delimiter {
                quote = None;
            }
        } else if matches!(character, '\'' | '"') {
            quote = Some(character);
            normalized.push(character);
        } else if !character.is_ascii_whitespace() {
            normalized.push(character.to_ascii_lowercase());
        }
    }
    normalized.replace("ifnotexists", "")
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
        "SELECT CASE WHEN length(CAST(sql AS BLOB))<=65536 THEN sql ELSE NULL END AS sql FROM sqlite_schema WHERE tbl_name=? AND type='index' AND sql IS NOT NULL LIMIT 3", [name.into()])).await?;
    if name == "tenancy_credentials" {
        let expected = Index::create()
            .name("idx_tenancy_credentials_state")
            .table(Alias::new(name))
            .col(Alias::new("state"))
            .col(Alias::new("tenant_id"))
            .to_string(SqliteQueryBuilder);
        let directory = super::m20240101_000045_ordered_receipts::directory_index()
            .to_string(SqliteQueryBuilder);
        let mut expected = vec![normalized(&expected), normalized(&directory)];
        let mut actual = indexes
            .iter()
            .map(|row| row.try_get::<String>("", "sql").map(|sql| normalized(&sql)))
            .collect::<Result<Vec<_>, _>>()?;
        actual.sort_unstable();
        expected.sort_unstable();
        if actual != expected {
            return Err(incompatible());
        }
    } else if name == "tenancy_resource_lifecycle" && db.query_one(Statement::from_string(DatabaseBackend::Sqlite,
        "SELECT 1 FROM seaql_migrations WHERE version='m20240101_000049_admission_index' LIMIT 1")).await?.is_some() {
        let expected = super::m20240101_000049_admission_index::head_index().to_string(SqliteQueryBuilder);
        if indexes.len() != 1 || normalized(&indexes[0].try_get::<String>("", "sql")?) != normalized(&expected) {
            return Err(incompatible());
        }
    } else if !indexes.is_empty() {
        return Err(incompatible());
    }
    Ok(())
}
fn incompatible() -> DbErr {
    DbErr::Custom("incompatible current authority schema".into())
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
