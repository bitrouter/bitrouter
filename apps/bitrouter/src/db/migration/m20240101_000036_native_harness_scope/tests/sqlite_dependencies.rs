//! Regression for SQLite schema reparse failures after partial downgrade DDL.
use super::*;
use sea_orm_migration::MigratorTrait;

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

async fn database() -> Result<DatabaseConnection> {
    // One connection is essential: TEMP schema objects belong to that connection.
    let db = crate::db::connect("sqlite::memory:").await?;
    let migrations = crate::db::migration::Migrator::migrations();
    let count = migrations
        .iter()
        .position(|entry| entry.name() == Migration.name())
        .context("parent must register migration036")?
        + 1;
    crate::db::migration::Migrator::up(&db, Some(count.try_into()?)).await?;
    db.execute_unprepared("INSERT INTO bro_managed_sessions (id, revision, payload_bytes, payload) VALUES ('legacy36', 9, 4, CAST(X'007B7D0A' AS TEXT))").await?;
    db.execute_unprepared(
        "CREATE TABLE native36_driver (id INTEGER PRIMARY KEY, payload BLOB NOT NULL)",
    )
    .await?;
    db.execute_unprepared("CREATE INDEX native36_driver_payload ON native36_driver(payload)")
        .await?;
    db.execute_unprepared("INSERT INTO native36_driver VALUES (1, X'00FF1020')")
        .await?;
    Ok(db)
}

async fn refused(db: &DatabaseConnection) -> Result<()> {
    let before = snapshot(db).await?;
    let error = crate::db::migration::Migrator::down(db, Some(1))
        .await
        .err()
        .context("036 dependency passed downgrade")?;
    ensure!(
        error
            .to_string()
            .contains("cannot resolve view/trigger dependencies"),
        "unexpected refusal: {error}"
    );
    ensure!(
        snapshot(db).await? == before,
        "refusal changed schema/indexes, row bytes or ledger"
    );
    Ok(())
}

async fn finish_down(db: &DatabaseConnection) -> Result<()> {
    let before = crate::db::migration::Migrator::get_migration_models(db).await?;
    crate::db::migration::Migrator::down(db, Some(1)).await?;
    let after = crate::db::migration::Migrator::get_migration_models(db).await?;
    ensure!(after.len() + 1 == before.len() && before.starts_with(&after));
    let manager = SchemaManager::new(db);
    for column in COLUMNS {
        ensure!(!has_column(&manager, "bro_managed_sessions", column).await?);
    }
    ensure!(
        !manager
            .has_index("bro_managed_sessions", "idx_native36_scope")
            .await?
    );
    ensure!(
        manager
            .has_index("native36_driver", "native36_driver_payload")
            .await?
    );
    let row = db.query_one(Statement::from_string(DatabaseBackend::Sqlite,
        "SELECT revision, payload_bytes, typeof(payload) AS kind, hex(CAST(payload AS BLOB)) AS bytes FROM bro_managed_sessions WHERE id = 'legacy36'"
    )).await?.context("historical row")?;
    ensure!(
        row.try_get::<i64>("", "revision")? == 9 && row.try_get::<i64>("", "payload_bytes")? == 4
    );
    ensure!(
        row.try_get::<String>("", "kind")? == "text"
            && row.try_get::<String>("", "bytes")? == "007B7D0A"
    );
    Ok(())
}

#[tokio::test]
async fn sqlite_native36_temp_and_single_quoted_dependencies_refuse_before_any_change() -> Result<()>
{
    for (create, exercise, drop) in [
        (
            "CREATE TEMP VIEW native36_dep AS SELECT execution_revision FROM main.bro_managed_sessions",
            "SELECT * FROM temp.native36_dep",
            "DROP VIEW temp.native36_dep",
        ),
        (
            "CREATE VIEW native36_dep AS SELECT execution_revision FROM 'bro_managed_sessions'",
            "SELECT * FROM main.native36_dep",
            "DROP VIEW main.native36_dep",
        ),
        (
            "CREATE TEMP TRIGGER native36_dep AFTER INSERT ON main.native36_driver BEGIN SELECT execution_revision FROM main.bro_managed_sessions; END",
            "INSERT INTO native36_driver VALUES (2, X'AA00')",
            "DROP TRIGGER temp.native36_dep",
        ),
        (
            "CREATE TRIGGER native36_dep AFTER INSERT ON native36_driver BEGIN SELECT execution_revision FROM 'bro_managed_sessions'; END",
            "INSERT INTO native36_driver VALUES (2, X'AA00')",
            "DROP TRIGGER main.native36_dep",
        ),
    ] {
        let db = database().await?;
        db.execute_unprepared(create).await?;
        // Resolve the real dependency before down; invalid fixture SQL cannot
        // substitute for the column dependency under test.
        db.query_all(Statement::from_string(DatabaseBackend::Sqlite, exercise))
            .await?;
        refused(&db).await?;
        db.execute_unprepared(drop).await?;
        finish_down(&db).await?;
        db.close().await?;
    }
    Ok(())
}

#[tokio::test]
async fn sqlite_native36_conservative_refusal_stops_when_no_scope_column_remains() -> Result<()> {
    let db = database().await?;
    db.execute_unprepared(
        "CREATE TEMP VIEW unrelated36 AS SELECT payload FROM main.native36_driver",
    )
    .await?;
    refused(&db).await?;
    db.execute_unprepared("DROP VIEW temp.unrelated36").await?;
    finish_down(&db).await?;
    db.execute_unprepared(
        "CREATE TEMP VIEW unrelated36 AS SELECT payload FROM main.native36_driver",
    )
    .await?;
    let before = snapshot(&db).await?;
    Migration.down(&SchemaManager::new(&db)).await?;
    ensure!(
        snapshot(&db).await? == before,
        "completed downgrade ceased to be a no-op"
    );
    db.close().await?;
    Ok(())
}
