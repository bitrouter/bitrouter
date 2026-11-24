//! Initialize and validate the current SQLite gateway catalog from its DDL.

use super::migration::provider_technical;
use anyhow::{Context, Result, ensure};
use sea_orm::{ConnectionTrait, DatabaseBackend, Statement};
use std::collections::BTreeMap;

fn sql(s: impl Into<String>) -> Statement {
    Statement::from_string(DatabaseBackend::Sqlite, s)
}

pub(super) async fn validate(db: &impl ConnectionTrait, serving: bool) -> Result<()> {
    validate_selected(db, serving, None, false).await
}

pub(super) async fn validate_legacy(db: &impl ConnectionTrait, serving: bool) -> Result<()> {
    validate_selected(db, serving, None, true).await
}

/// Validate the exact current objects used by an independent receipt/authority reader.
pub(super) async fn validate_tables(db: &impl ConnectionTrait, tables: &[&str]) -> Result<()> {
    validate_selected(db, false, Some(tables), false).await
}

async fn validate_selected(
    db: &impl ConnectionTrait,
    serving: bool,
    selected: Option<&[&str]>,
    legacy: bool,
) -> Result<()> {
    ensure!(
        db.get_database_backend() == DatabaseBackend::Sqlite,
        "SQLite required"
    );
    ensure!(db.query_one(sql("SELECT 1 FROM temp.sqlite_schema t JOIN main.sqlite_schema m ON t.name=m.name COLLATE NOCASE LIMIT 1")).await?.is_none(), "shadowed migration catalog");
    ensure!(db.query_one(sql("SELECT 1 FROM temp.sqlite_schema t JOIN main.sqlite_schema m ON t.tbl_name=m.name COLLATE NOCASE WHERE t.type='trigger' LIMIT 1")).await?.is_none(), "temporary mutation hook on current storage");
    let technical = selected.is_none() && !serving && provider_technical::available(db).await?;
    // Preserve SQL token boundaries and quoted literals.
    let tokens = regex::Regex::new(r#""(?:[^"]|"")*"|'(?:[^']|'')*'|[a-zA-Z0-9_]+|[^\x09-\x0d ]"#)?;
    let mut expected = BTreeMap::new();
    let statements = if legacy {
        super::current_schema::legacy_statements(DatabaseBackend::Sqlite)?
    } else {
        super::current_schema::statements(DatabaseBackend::Sqlite)?
    };
    for statement in &statements {
        let parts = tokens
            .find_iter(statement)
            .map(|part| part.as_str())
            .collect::<Vec<_>>();
        let (kind, name, table) = match parts.as_slice() {
            ["CREATE", "TABLE", name, ..] => ("table", *name, *name),
            ["CREATE", "UNIQUE", "INDEX", name, "ON", table, ..]
            | ["CREATE", "INDEX", name, "ON", table, ..] => ("index", *name, *table),
            _ => anyhow::bail!("invalid compiled SQLite schema declaration"),
        };
        if selected.is_some_and(|tables| !tables.contains(&table.trim_matches('"'))) {
            continue;
        }
        ensure!(
            expected
                .insert(
                    name.trim_matches('"'),
                    (kind, table.trim_matches('"'), parts)
                )
                .is_none(),
            "duplicate compiled SQLite schema object"
        );
    }
    if let Some(tables) = selected {
        ensure!(
            tables.iter().all(|table| expected.contains_key(table)),
            "unknown current table selection"
        );
    }
    // Each borrowed read fits the smallest audit page. Validate and release the
    // page before reading another, retaining only recognized catalog objects.
    const PAGE: usize = 8;
    for offset in (0..128).step_by(PAGE) {
        let rows = db.query_all(sql(format!("SELECT type,name,tbl_name,CASE WHEN length(CAST(sql AS BLOB))<=65536 THEN sql END AS sql FROM main.sqlite_schema WHERE NOT (type='index' AND name GLOB 'sqlite_autoindex_*' AND sql IS NULL) ORDER BY name,type LIMIT {PAGE} OFFSET {offset}"))).await?;
        ensure!(offset + rows.len() < 128, "unrecognized SQLite catalog");
        let last = rows.len() < PAGE;
        for row in rows {
            let name: String = row.try_get("", "name")?;
            let kind: String = row.try_get("", "type")?;
            let table: String = row.try_get("", "tbl_name")?;
            if selected
                .is_some_and(|tables| !tables.iter().any(|name| name.eq_ignore_ascii_case(&table)))
            {
                continue;
            }
            let actual: String = row.try_get("", "sql")?;
            if name == "sqlite_sequence" {
                ensure!(
                    kind == "table"
                        && table == name
                        && actual == "CREATE TABLE sqlite_sequence(name,seq)",
                    "invalid sequence catalog"
                );
                continue;
            }
            if matches!(name.as_str(), "sqlite_stat1" | "sqlite_stat4") {
                let columns = if name == "sqlite_stat1" {
                    "tbl,idx,stat"
                } else {
                    "tbl,idx,neq,nlt,ndlt,sample"
                };
                ensure!(
                    kind == "table"
                        && name == table
                        && actual == format!("CREATE TABLE {name}({columns})"),
                    "invalid SQLite statistics catalog"
                );
                continue;
            }
            // A passive table occupying an optional name cannot confer capability.
            // The technical reader/installer checks its exact DDL and stamp before
            // use. Import and archive validation still require the exact catalog.
            if serving
                && matches!(
                    name.as_str(),
                    provider_technical::TABLE | "tenancy_provider_technical_schema"
                )
            {
                ensure!(
                    kind == "table" && name == table && actual.starts_with("CREATE TABLE "),
                    "conflicting optional schema object"
                );
                if db
                    .query_one(Statement::from_sql_and_values(
                        DatabaseBackend::Sqlite,
                        "SELECT 1 FROM pragma_foreign_key_list(?, 'main') LIMIT 1",
                        [name.as_str().into()],
                    ))
                    .await?
                    .is_some()
                {
                    ensure!(
                        name == provider_technical::TABLE
                            && actual
                                == provider_technical::definition(DatabaseBackend::Sqlite)
                                    .to_string(sea_orm::sea_query::SqliteQueryBuilder),
                        "optional table changes gateway foreign keys"
                    );
                }
                continue;
            }
            if technical
                && matches!(
                    name.as_str(),
                    provider_technical::TABLE | "tenancy_provider_technical_schema"
                )
            {
                continue;
            }
            let (expected_kind, expected_table, expected_sql) = expected
                .remove(name.as_str())
                .context("unexpected object in SQLite catalog")?;
            ensure!(
                kind == expected_kind
                    && table == expected_table
                    && tokens
                        .find_iter(&actual)
                        .map(|part| part.as_str())
                        .eq(expected_sql),
                "incompatible schema for {name}"
            );
        }
        if last {
            break;
        }
    }
    ensure!(expected.is_empty(), "incomplete migration catalog");
    super::current_schema::validate_sqlite_metadata(db, selected, legacy).await?;
    require_active_format(db).await
}

async fn require_active_format(db: &impl ConnectionTrait) -> Result<()> {
    let id = db
        .query_one(sql("PRAGMA main.application_id"))
        .await?
        .context("missing gateway application id")?;
    ensure!(
        id.try_get_by_index::<i64>(0)? == 0,
        "unsupported SQLite application ID"
    );
    Ok(())
}

/// Create the empty service projection in the caller's SQLite startup transaction.
/// The caller commits on success and rolls the complete transaction back on error.
///
/// # Errors
/// Refuses a nonempty database, schema conflict or failed final validation.
pub(crate) async fn initialize_gateway_schema(db: &impl ConnectionTrait) -> Result<()> {
    ensure!(
        db.get_database_backend() == DatabaseBackend::Sqlite,
        "SQLite required"
    );
    ensure!(
        db.query_one(sql("SELECT 1 FROM main.sqlite_schema LIMIT 1"))
            .await?
            .is_none(),
        "gateway initialization requires an empty database"
    );
    for statement in super::current_schema::statements(DatabaseBackend::Sqlite)? {
        db.execute_unprepared(&statement).await?;
    }
    super::migration::capacity::initialize(
        db,
        &crate::tenancy::receipts::capacity::Inventory::default(),
        false,
    )
    .await?;
    db.execute(Statement::from_sql_and_values(
        DatabaseBackend::Sqlite,
        "INSERT INTO main.seaql_migrations(version,applied_at) VALUES (?,?)",
        [
            super::store_format::GATEWAY_SQLITE.into(),
            chrono::Utc::now().timestamp().into(),
        ],
    ))
    .await?;
    validate(db, true).await
}
