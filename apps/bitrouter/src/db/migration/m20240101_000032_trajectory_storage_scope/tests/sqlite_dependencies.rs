//! Regression for SQLite schema reparse failures after partial downgrade DDL.
use super::*;
use sea_orm_migration::MigratorTrait;

const LAST_TABLE: &str = "trajectory_outbox";
const LAST_COLUMN: &str = "logical_identity_json";
type Historical = Vec<Vec<Vec<Option<String>>>>;

#[derive(Debug, PartialEq, Eq)]
struct SchemaObject {
    schema: String,
    kind: String,
    name: String,
    table: String,
    rootpage: Option<i64>,
    sql: Option<String>,
}

#[derive(Debug, PartialEq, Eq)]
struct TableRows {
    schema: String,
    table: String,
    columns: Vec<String>,
    values: Vec<Vec<String>>,
}

#[derive(Debug, PartialEq, Eq)]
struct Snapshot {
    schema: Vec<SchemaObject>,
    rows: Vec<TableRows>,
    ledger: Vec<(String, i64)>,
}

fn quoted(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}

async fn snapshot(db: &DatabaseConnection) -> Result<Snapshot> {
    let schema = db
        .query_all(Statement::from_string(
            DatabaseBackend::Sqlite,
            "SELECT 'main' AS schema_name, type, name, tbl_name, rootpage, sql FROM main.sqlite_schema \
             UNION ALL SELECT 'temp' AS schema_name, type, name, tbl_name, rootpage, sql FROM temp.sqlite_schema \
             ORDER BY schema_name, type, name",
        ))
        .await?
        .into_iter()
        .map(|row| {
            Ok(SchemaObject {
                schema: row.try_get("", "schema_name")?,
                kind: row.try_get("", "type")?,
                name: row.try_get("", "name")?,
                table: row.try_get("", "tbl_name")?,
                rootpage: row.try_get("", "rootpage")?,
                sql: row.try_get("", "sql")?,
            })
        })
        .collect::<std::result::Result<Vec<_>, DbErr>>()?;
    let mut rows = Vec::new();
    for table in schema.iter().filter(|entry| entry.kind == "table") {
        let columns = db
            .query_all(Statement::from_sql_and_values(
                DatabaseBackend::Sqlite,
                "SELECT name FROM pragma_table_info(?, ?) ORDER BY cid",
                [table.name.clone().into(), table.schema.clone().into()],
            ))
            .await?
            .into_iter()
            .map(|row| row.try_get::<String>("", "name"))
            .collect::<std::result::Result<Vec<_>, _>>()?;
        ensure!(!columns.is_empty(), "fixture table has no columns");
        // Preserve type, NULL, embedded NULs, blob bytes and exact text spelling.
        // Result-column order sorts duplicate rows deterministically without
        // relying on rowid or the predecessor's primary-key shape.
        let expressions = columns
            .iter()
            .enumerate()
            .map(|(index, column)| {
                let column = quoted(column);
                format!(
                    "CASE WHEN {column} IS NULL THEN 'null' ELSE typeof({column}) || ':' || hex(CAST({column} AS BLOB)) END AS c{index}"
                )
            })
            .collect::<Vec<_>>()
            .join(", ");
        let order = (1..=columns.len())
            .map(|index| index.to_string())
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!(
            "SELECT {expressions} FROM {}.{} ORDER BY {order}",
            quoted(&table.schema),
            quoted(&table.name),
        );
        let values = db
            .query_all(Statement::from_string(DatabaseBackend::Sqlite, sql))
            .await?
            .into_iter()
            .map(|row| {
                (0..columns.len())
                    .map(|index| row.try_get::<String>("", &format!("c{index}")))
                    .collect::<std::result::Result<Vec<_>, _>>()
            })
            .collect::<std::result::Result<Vec<_>, _>>()?;
        rows.push(TableRows {
            schema: table.schema.clone(),
            table: table.name.clone(),
            columns,
            values,
        });
    }
    let ledger = crate::db::migration::Migrator::get_migration_models(db)
        .await?
        .into_iter()
        .map(|row| (row.version, row.applied_at))
        .collect();
    Ok(Snapshot {
        schema,
        rows,
        ledger,
    })
}

async fn database() -> Result<(DatabaseConnection, Historical)> {
    // A single in-memory connection also makes TEMP objects visible to every
    // preflight/DDL query; no pool routing can hide this regression fixture.
    let db = crate::db::connect("sqlite::memory:").await?;
    let migrations = crate::db::migration::Migrator::migrations();
    let count = migrations
        .iter()
        .position(|entry| entry.name() == Migration.name())
        .context("parent must register migration032")?
        + 1;
    crate::db::migration::Migrator::up(&db, Some(count.try_into()?)).await?;
    seed(&db).await?;
    let historical = historical(&db).await?;
    ensure!(
        crate::db::migration::Migrator::get_migration_models(&db)
            .await?
            .last()
            .is_some_and(|entry| entry.version == Migration.name())
    );
    db.execute_unprepared(
        "CREATE TABLE ddl_fixture_driver (id INTEGER PRIMARY KEY, payload BLOB NOT NULL)",
    )
    .await?;
    db.execute_unprepared("CREATE INDEX ddl_fixture_driver_payload ON ddl_fixture_driver(payload)")
        .await?;
    db.execute_unprepared("INSERT INTO ddl_fixture_driver VALUES (1, X'00FF1020')")
        .await?;
    Ok((db, historical))
}

async fn refused_unchanged(db: &DatabaseConnection) -> Result<()> {
    let before = snapshot(db).await?;
    let error = crate::db::migration::Migrator::down(db, Some(1))
        .await
        .err()
        .context("downgrade unexpectedly accepted a view/trigger")?;
    ensure!(
        error
            .to_string()
            .contains("cannot resolve view/trigger dependencies"),
        "expected preflight refusal, got {error}"
    );
    ensure!(
        snapshot(db).await? == before,
        "downgrade changed schema, rows or ledger before refusal"
    );
    Ok(())
}

async fn finish_down(db: &DatabaseConnection, before: &Historical) -> Result<()> {
    let ledger = crate::db::migration::Migrator::get_migration_models(db).await?;
    crate::db::migration::Migrator::down(db, Some(1)).await?;
    let after = crate::db::migration::Migrator::get_migration_models(db).await?;
    ensure!(after.len() + 1 == ledger.len() && ledger.starts_with(&after));
    ensure!(historical(db).await? == *before);
    let manager = SchemaManager::new(db);
    for table in TABLES {
        for &column in columns(table) {
            ensure!(!has_column(&manager, table, column).await?);
        }
    }
    for index in &INDEXES {
        ensure!(!manager.has_index(index.table, index.name).await?);
    }
    ensure!(manager.has_table("ddl_fixture_driver").await?);
    ensure!(
        manager
            .has_index("ddl_fixture_driver", "ddl_fixture_driver_payload")
            .await?
    );
    Ok(())
}

#[tokio::test]
async fn sqlite_last_table_views_refuse_before_any_schema_row_or_ledger_change() -> Result<()> {
    for temporary in [false, true] {
        let (db, historical) = database().await?;
        let (temporary, schema) = if temporary {
            ("TEMP ", "temp")
        } else {
            ("", "main")
        };
        db.execute_unprepared(&format!(
            "CREATE {temporary}VIEW ddl_scope_view AS SELECT {LAST_COLUMN} FROM main.{LAST_TABLE}"
        ))
        .await?;
        // Resolve the fixture before testing down, so an unrelated invalid view
        // cannot stand in for an actual dependency on the last table.
        db.query_all(Statement::from_string(
            DatabaseBackend::Sqlite,
            format!("SELECT * FROM {schema}.ddl_scope_view"),
        ))
        .await?;
        refused_unchanged(&db).await?;
        db.execute_unprepared(&format!("DROP VIEW {schema}.ddl_scope_view"))
            .await?;
        finish_down(&db, &historical).await?;
        db.close().await?;
    }
    Ok(())
}

#[tokio::test]
async fn sqlite_external_triggers_refuse_before_any_schema_row_or_ledger_change() -> Result<()> {
    for temporary in [false, true] {
        let (db, historical) = database().await?;
        let (temporary, schema) = if temporary {
            ("TEMP ", "temp")
        } else {
            ("", "main")
        };
        db.execute_unprepared(&format!(
            "CREATE {temporary}TRIGGER ddl_scope_trigger AFTER INSERT ON main.ddl_fixture_driver \
             BEGIN SELECT {LAST_COLUMN} FROM main.{LAST_TABLE}; END"
        ))
        .await?;
        db.execute_unprepared("INSERT INTO ddl_fixture_driver VALUES (2, X'00FE')")
            .await?;
        refused_unchanged(&db).await?;
        db.execute_unprepared(&format!("DROP TRIGGER {schema}.ddl_scope_trigger"))
            .await?;
        finish_down(&db, &historical).await?;
        db.close().await?;
    }
    Ok(())
}

#[tokio::test]
async fn sqlite_unrelated_objects_are_explicitly_refused_only_while_scope_remains() -> Result<()> {
    let (db, historical) = database().await?;
    // SQLite supplies no dependable dependency graph. The policy is deliberately
    // conservative even for these resolvable, visibly unrelated schema objects.
    for (create, drop) in [
        (
            "CREATE VIEW ddl_unrelated_view AS SELECT payload FROM ddl_fixture_driver",
            "DROP VIEW ddl_unrelated_view",
        ),
        (
            "CREATE TRIGGER ddl_unrelated_trigger AFTER UPDATE ON ddl_fixture_driver BEGIN SELECT id FROM ddl_fixture_driver; END",
            "DROP TRIGGER ddl_unrelated_trigger",
        ),
    ] {
        db.execute_unprepared(create).await?;
        refused_unchanged(&db).await?;
        db.execute_unprepared(drop).await?;
    }
    finish_down(&db, &historical).await?;
    db.execute_unprepared(
        "CREATE VIEW ddl_unrelated_view AS SELECT payload FROM ddl_fixture_driver",
    )
    .await?;
    db.execute_unprepared("CREATE TRIGGER ddl_unrelated_trigger AFTER UPDATE ON ddl_fixture_driver BEGIN SELECT id FROM ddl_fixture_driver; END").await?;
    let before = snapshot(&db).await?;
    // A fully resumed downgrade has no remaining scope columns to remove.
    Migration.down(&SchemaManager::new(&db)).await?;
    ensure!(snapshot(&db).await? == before);
    db.close().await?;
    Ok(())
}

#[tokio::test]
async fn sqlite_partial_down_last_remaining_column_is_preflighted() -> Result<()> {
    let (db, historical) = database().await?;
    let manager = SchemaManager::new(&db);
    for index in &INDEXES {
        manager.drop_index(index.drop()).await?;
    }
    for table in TABLES {
        for &column in columns(table) {
            if table == LAST_TABLE && column == LAST_COLUMN {
                continue;
            }
            manager
                .alter_table(
                    Table::alter()
                        .table(Alias::new(table))
                        .drop_column(Alias::new(column))
                        .to_owned(),
                )
                .await?;
        }
    }
    db.execute_unprepared(&format!(
        "CREATE VIEW ddl_scope_view AS SELECT {LAST_COLUMN} FROM {LAST_TABLE}"
    ))
    .await?;
    refused_unchanged(&db).await?;
    db.execute_unprepared("DROP VIEW ddl_scope_view").await?;
    finish_down(&db, &historical).await?;
    db.close().await?;
    Ok(())
}
