//! Two exact legacy definitions plus the exact archive extension, never a union
//! of individually optional columns or permissive SQLite affinity checks.
use super::*;
use sea_orm::{Statement, Value};

fn sql(statement: impl Into<String>, values: impl IntoIterator<Item = Value>) -> Statement {
    Statement::from_sql_and_values(DatabaseBackend::Sqlite, statement, values)
}

/// Presence selects the complete upgraded profile; any malformed or partial
/// profile is an error, not a reason to retry the legacy validator.
pub(in crate::db::migration) async fn present(db: &impl ConnectionTrait) -> Result<bool, DbErr> {
    Ok(db.query_one(sql(
        "SELECT 1 FROM main.sqlite_schema WHERE lower(name)='tenancy_receipt_archive' OR lower(tbl_name)='tenancy_receipt_archive' UNION ALL SELECT 1 FROM temp.sqlite_schema WHERE lower(name)='tenancy_receipt_archive' OR lower(tbl_name)='tenancy_receipt_archive' LIMIT 1", []
    )).await?.is_some())
}

pub(super) async fn no_shadows(db: &impl ConnectionTrait) -> Result<(), DbErr> {
    if db.query_one(sql(
        "SELECT 1 FROM temp.sqlite_schema WHERE lower(name) IN ('tenancy_execution_receipts','tenancy_usage_evidence','tenancy_receipt_archive','idx_tenancy_receipts_correlation','idx_tenancy_receipts_storage') UNION ALL SELECT 1 FROM main.sqlite_schema WHERE type='trigger' AND lower(tbl_name) IN ('tenancy_execution_receipts','tenancy_usage_evidence','tenancy_receipt_archive') UNION ALL SELECT 1 FROM temp.sqlite_schema WHERE type='trigger' AND lower(tbl_name) IN ('tenancy_execution_receipts','tenancy_usage_evidence','tenancy_receipt_archive') LIMIT 1", []
    )).await?.is_some() {
        return Err(incompatible());
    }
    Ok(())
}

async fn table(
    db: &impl ConnectionTrait,
    name: &str,
    expected: TableCreateStatement,
) -> Result<(), DbErr> {
    let rows = db.query_all(sql(
        "SELECT type,name,CASE WHEN length(CAST(sql AS BLOB))<=65536 THEN sql ELSE NULL END AS sql FROM main.sqlite_schema WHERE lower(name)=? LIMIT 2", [name.into()]
    )).await?;
    let row = rows.first().ok_or_else(incompatible)?;
    if rows.len() != 1
        || row.try_get::<String>("", "type")? != "table"
        || row.try_get::<String>("", "name")? != name
        || row.try_get::<String>("", "sql")? != expected.to_string(SqliteQueryBuilder)
    {
        return Err(incompatible());
    }
    Ok(())
}

async fn receipt_indexes(db: &impl ConnectionTrait) -> Result<(), DbErr> {
    let indexes = db.query_all(sql(
        "SELECT CASE WHEN length(CAST(name AS BLOB))<=256 THEN name ELSE NULL END AS name,origin,\"unique\" AS unique_value,partial FROM pragma_index_list('tenancy_execution_receipts') LIMIT 4", []
    )).await?;
    if indexes.len() != 3 {
        return Err(incompatible());
    }
    let mut primary = false;
    let mut correlation = false;
    let mut storage = false;
    for row in indexes {
        let name = row.try_get::<String>("", "name")?;
        let origin = row.try_get::<String>("", "origin")?;
        let (unique, expected): (i64, &[&str]) = match (origin.as_str(), name.as_str()) {
            ("pk", _) if !primary => {
                primary = true;
                (1, &["installation_id", "tenant_id", "execution_id"])
            }
            ("c", "idx_tenancy_receipts_correlation") if !correlation => {
                correlation = true;
                (
                    0,
                    &[
                        "installation_id",
                        "tenant_id",
                        "correlation_id",
                        "execution_id",
                    ],
                )
            }
            ("c", STORAGE_INDEX) if !storage => {
                storage = true;
                (
                    0,
                    &[
                        "installation_id",
                        "storage_kind",
                        "tenant_id",
                        "execution_id",
                    ],
                )
            }
            _ => return Err(incompatible()),
        };
        if row.try_get::<i64>("", "unique_value")? != unique
            || row.try_get::<i64>("", "partial")? != 0
        {
            return Err(incompatible());
        }
        let columns = db.query_all(sql(
            "SELECT CASE WHEN length(CAST(name AS BLOB))<=128 THEN name ELSE NULL END AS column_name,CASE WHEN key=1 AND cid>=0 AND coll='BINARY' AND \"desc\"=0 THEN 1 ELSE 0 END AS compatible FROM pragma_index_xinfo(?) WHERE key=1 ORDER BY seqno LIMIT 5", [name.into()]
        )).await?;
        if columns.len() != expected.len() {
            return Err(incompatible());
        }
        for (column, expected) in columns.iter().zip(expected) {
            if column.try_get::<String>("", "column_name")? != *expected
                || column.try_get::<i64>("", "compatible")? != 1
            {
                return Err(incompatible());
            }
        }
    }
    if !primary || !correlation || !storage {
        return Err(incompatible());
    }
    Ok(())
}

pub(super) async fn validate(db: &impl ConnectionTrait) -> Result<(), DbErr> {
    no_shadows(db).await?;
    let mut receipts = receipt_definition();
    if db
        .query_one(sql(
            "SELECT 1 FROM sqlite_schema WHERE name='tenancy_dispatch_intents' LIMIT 1",
            [],
        ))
        .await?
        .is_some()
    {
        receipts.col(super::super::m20240101_000045_ordered_receipts::format_column());
    }
    table(db, RECEIPTS, receipts).await?;
    table(db, USAGE, usage_definition()).await?;
    table(db, TABLE, definition()).await?;
    receipt_indexes(db).await?;
    usage::validate_indexes(db).await?;
    let rows = db.query_all(sql(
        "SELECT CASE WHEN length(CAST(name AS BLOB))<=256 THEN name ELSE NULL END AS name,origin,\"unique\" AS unique_value,partial FROM pragma_index_list('tenancy_receipt_archive') LIMIT 2", []
    )).await?;
    let row = rows.first().ok_or_else(incompatible)?;
    if rows.len() != 1
        || row.try_get::<String>("", "origin")? != "pk"
        || row.try_get::<i64>("", "unique_value")? != 1
        || row.try_get::<i64>("", "partial")? != 0
    {
        return Err(incompatible());
    }
    let name = row.try_get::<String>("", "name")?;
    let columns = db.query_all(sql(
        "SELECT name AS column_name,CASE WHEN key=1 AND cid>=0 AND coll='BINARY' AND \"desc\"=0 THEN 1 ELSE 0 END AS compatible FROM pragma_index_xinfo(?) WHERE key=1 ORDER BY seqno LIMIT 4", [name.into()]
    )).await?;
    if columns.len() != 3 {
        return Err(incompatible());
    }
    for (row, name) in columns
        .iter()
        .zip(["installation_id", "tenant_id", "execution_id"])
    {
        if row.try_get::<String>("", "column_name")? != name
            || row.try_get::<i64>("", "compatible")? != 1
        {
            return Err(incompatible());
        }
    }
    Ok(())
}
