//! Separate exact legacy and indexed profiles. Partial upgrades never fall back.
use super::*;
use sea_orm::{Statement, Value};
fn sql(query: impl Into<String>, values: impl IntoIterator<Item = Value>) -> Statement {
    Statement::from_sql_and_values(DatabaseBackend::Sqlite, query, values)
}
pub(in crate::db::migration) async fn present(db: &impl ConnectionTrait) -> Result<bool, DbErr> {
    Ok(db.query_one(sql("SELECT 1 FROM main.sqlite_schema WHERE lower(name) IN ('tenancy_unknown_obligations','idx_tenancy_unknown_due','idx_tenancy_oauth_execution') OR lower(tbl_name)='tenancy_unknown_obligations' UNION ALL SELECT 1 FROM temp.sqlite_schema WHERE lower(name) IN ('tenancy_unknown_obligations','idx_tenancy_unknown_due','idx_tenancy_oauth_execution') OR lower(tbl_name)='tenancy_unknown_obligations' UNION ALL SELECT 1 FROM pragma_table_info('tenancy_oauth_slots') WHERE lower(name) IN ('pending_tenant_id','pending_execution_id') LIMIT 1", [])).await?.is_some())
}
async fn no_shadows(db: &impl ConnectionTrait) -> Result<(), DbErr> {
    if db.query_one(sql("SELECT 1 FROM temp.sqlite_schema WHERE lower(name) IN ('tenancy_oauth_slots','tenancy_receipt_auxiliary','tenancy_unknown_obligations','idx_tenancy_oauth_pending','idx_tenancy_oauth_execution','idx_tenancy_unknown_due') UNION ALL SELECT 1 FROM main.sqlite_schema WHERE type='trigger' AND lower(tbl_name) IN ('tenancy_oauth_slots','tenancy_receipt_auxiliary','tenancy_unknown_obligations') UNION ALL SELECT 1 FROM temp.sqlite_schema WHERE type='trigger' AND lower(tbl_name) IN ('tenancy_oauth_slots','tenancy_receipt_auxiliary','tenancy_unknown_obligations') LIMIT 1", [])).await?.is_some() { return Err(incompatible()); }
    Ok(())
}
async fn object(
    db: &impl ConnectionTrait,
    name: &str,
    kind: &str,
    expected: String,
) -> Result<(), DbErr> {
    let rows = db.query_all(sql("SELECT type,name,CASE WHEN length(CAST(sql AS BLOB))<=65536 THEN sql ELSE NULL END AS sql FROM main.sqlite_schema WHERE lower(name)=? LIMIT 2", [name.into()])).await?;
    let row = rows.first().ok_or_else(incompatible)?;
    if rows.len() != 1
        || row.try_get::<String>("", "type")? != kind
        || row.try_get::<String>("", "name")? != name
        || row.try_get::<String>("", "sql")? != expected
    {
        return Err(incompatible());
    }
    Ok(())
}
async fn indexes(
    db: &impl ConnectionTrait,
    table: &str,
    primary: &[&str],
    secondary: &[(&str, &[&str])],
) -> Result<(), DbErr> {
    let limit =
        <i64 as TryFrom<usize>>::try_from(secondary.len() + 2).map_err(|_| incompatible())?;
    let rows = db.query_all(sql("SELECT CASE WHEN length(CAST(name AS BLOB))<=256 THEN name ELSE NULL END AS name,origin,\"unique\" AS unique_value,partial FROM pragma_index_list(?) LIMIT ?", [table.into(), limit.into()])).await?;
    if rows.len() != secondary.len() + 1 {
        return Err(incompatible());
    }
    let mut primary_seen = false;
    for row in rows {
        let name: String = row.try_get("", "name")?;
        let is_primary = row.try_get::<String>("", "origin")? == "pk";
        let columns = if is_primary && !primary_seen {
            primary_seen = true;
            primary
        } else if !is_primary && row.try_get::<String>("", "origin")? == "c" {
            secondary
                .iter()
                .find(|(expected, _)| *expected == name)
                .map(|(_, columns)| *columns)
                .ok_or_else(incompatible)?
        } else {
            return Err(incompatible());
        };
        if row.try_get::<i64>("", "unique_value")? != i64::from(is_primary)
            || row.try_get::<i64>("", "partial")? != 0
        {
            return Err(incompatible());
        }
        let actual = db.query_all(sql("SELECT name AS column_name,CASE WHEN key=1 AND cid>=0 AND coll='BINARY' AND \"desc\"=0 THEN 1 ELSE 0 END AS compatible FROM pragma_index_xinfo(?) WHERE key=1 ORDER BY seqno LIMIT 8", [name.into()])).await?;
        if actual.len() != columns.len() {
            return Err(incompatible());
        }
        for (row, name) in actual.iter().zip(columns) {
            if row.try_get::<String>("", "column_name")? != *name
                || row.try_get::<i64>("", "compatible")? != 1
            {
                return Err(incompatible());
            }
        }
    }
    if !primary_seen {
        return Err(incompatible());
    }
    for (name, columns) in secondary {
        object(
            db,
            name,
            "index",
            index(name, table, columns).to_string(SqliteQueryBuilder),
        )
        .await?;
    }
    Ok(())
}
pub(in crate::db::migration) async fn validate_legacy(
    db: &impl ConnectionTrait,
) -> Result<(), DbErr> {
    if db.get_database_backend() != DatabaseBackend::Sqlite || present(db).await? {
        return Err(incompatible());
    }
    validate(db, false).await
}
pub(super) async fn validate(db: &impl ConnectionTrait, upgraded: bool) -> Result<(), DbErr> {
    no_shadows(db).await?;
    let mut definitions =
        super::super::m20240101_000037_owned_oauth::sqlite_definitions().into_iter();
    let mut slots = definitions.next().ok_or_else(incompatible)?;
    if upgraded {
        for name in PENDING_COLUMNS {
            slots.col(pending_column(name));
        }
    }
    object(db, SLOTS, "table", slots.to_string(SqliteQueryBuilder)).await?;
    object(
        db,
        AUXILIARY,
        "table",
        definitions
            .next()
            .ok_or_else(incompatible)?
            .to_string(SqliteQueryBuilder),
    )
    .await?;
    let mut slot_indexes: Vec<(&str, &[&str])> = vec![(
        "idx_tenancy_oauth_pending",
        &["installation_id", "state", "slot_key"],
    )];
    if upgraded {
        slot_indexes.push((
            PENDING,
            &[
                "installation_id",
                "pending_tenant_id",
                "pending_execution_id",
                "slot_key",
            ],
        ));
    }
    indexes(db, SLOTS, &["slot_key"], &slot_indexes).await?;
    indexes(
        db,
        AUXILIARY,
        &["installation_id", "tenant_id", "execution_id"],
        &[],
    )
    .await?;
    if upgraded {
        object(
            db,
            TABLE,
            "table",
            definition().to_string(SqliteQueryBuilder),
        )
        .await?;
        indexes(
            db,
            TABLE,
            &["installation_id", "tenant_id", "execution_id"],
            &[(
                DUE,
                &[
                    "installation_id",
                    "escalated",
                    "due_seconds",
                    "due_nanos",
                    "tenant_id",
                    "execution_id",
                ],
            )],
        )
        .await?;
    }
    Ok(())
}
