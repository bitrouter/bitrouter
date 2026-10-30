//! SQLite's ASCII identifier lookup differs from a case-sensitive catalog WHERE.
//! Capture every object and table's typed SQL bytes, not only the colliding row.
use super::*;
use sea_orm::ConnectOptions;
use std::time::Duration;

#[derive(Debug, PartialEq, Eq)]
struct Object {
    schema: String,
    kind: String,
    name: String,
    table: String,
    rootpage: i64,
    sql: Option<String>,
}
#[derive(Debug, PartialEq, Eq)]
struct Contents {
    schema: String,
    table: String,
    rows: Vec<Vec<String>>,
}
#[derive(Debug, PartialEq, Eq)]
struct Image {
    objects: Vec<Object>,
    contents: Vec<Contents>,
}
fn identifier(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\"\""))
}
async fn image(db: &DatabaseConnection) -> Result<Image> {
    let backend = DatabaseBackend::Sqlite;
    let rows = db.query_all(Statement::from_string(backend,
        "SELECT 'main' AS schema_name,type,name,tbl_name,rootpage,sql FROM main.sqlite_schema UNION ALL SELECT 'temp' AS schema_name,type,name,tbl_name,rootpage,sql FROM temp.sqlite_schema ORDER BY schema_name COLLATE BINARY,type COLLATE BINARY,name COLLATE BINARY".to_owned())).await?;
    let mut objects = Vec::new();
    let mut contents = Vec::new();
    for row in rows {
        let object = Object {
            schema: row.try_get("", "schema_name")?,
            kind: row.try_get("", "type")?,
            name: row.try_get("", "name")?,
            table: row.try_get("", "tbl_name")?,
            rootpage: row.try_get("", "rootpage")?,
            sql: row.try_get("", "sql")?,
        };
        ensure!(matches!(object.schema.as_str(), "main" | "temp"));
        if object.kind == "table" {
            let columns = db
                .query_all(Statement::from_string(
                    backend,
                    format!(
                        "PRAGMA {}.table_info({})",
                        object.schema,
                        identifier(&object.name)
                    ),
                ))
                .await?;
            let names = columns
                .iter()
                .map(|row| row.try_get::<String>("", "name"))
                .collect::<std::result::Result<Vec<_>, _>>()?;
            ensure!(!names.is_empty());
            let selected = names
                .iter()
                .enumerate()
                .map(|(i, name)| format!("quote({}) AS c{i}", identifier(name)))
                .collect::<Vec<_>>()
                .join(",");
            let rows = db
                .query_all(Statement::from_string(
                    backend,
                    format!(
                        "SELECT {selected} FROM {}.{} LIMIT 101",
                        object.schema,
                        identifier(&object.name)
                    ),
                ))
                .await?;
            ensure!(rows.len() <= 100, "fixture row bound");
            let mut values = rows
                .iter()
                .map(|row| {
                    (0..names.len())
                        .map(|i| row.try_get::<String>("", &format!("c{i}")))
                        .collect::<std::result::Result<Vec<_>, _>>()
                })
                .collect::<std::result::Result<Vec<_>, _>>()?;
            values.sort();
            contents.push(Contents {
                schema: object.schema.clone(),
                table: object.name.clone(),
                rows: values,
            });
        }
        objects.push(object);
    }
    Ok(Image { objects, contents })
}

#[derive(Clone, Copy, Debug)]
enum Collision {
    MainView,
    MainTable,
    TempView,
    TempTable,
    MainIndex,
    TempIndex,
    IndexNamedTable,
    ViewNamedIndex,
}
impl Collision {
    fn index_name(self) -> bool {
        matches!(
            self,
            Self::MainIndex | Self::TempIndex | Self::ViewNamedIndex
        )
    }
}
async fn retain_auxiliary(db: &DatabaseConnection) -> Result<()> {
    db.execute_unprepared("INSERT INTO tenancy_receipt_auxiliary VALUES ('installation','tenant','execution','receipt digest','opaque retained auxiliary bytes','evidence digest',0)").await?;
    Ok(())
}
async fn case(db: &DatabaseConnection, partial: bool, collision: Collision) -> Result<()> {
    let manager = SchemaManager::new(db);
    db.execute_unprepared("CREATE TABLE legacy_oauth_data (id INTEGER PRIMARY KEY,payload BLOB)")
        .await?;
    db.execute_unprepared("INSERT INTO legacy_oauth_data VALUES (1,x'006162ff')")
        .await?;
    db.execute_unprepared(
        "CREATE TEMP TABLE legacy_oauth_temp (id INTEGER PRIMARY KEY,payload TEXT)",
    )
    .await?;
    db.execute_unprepared("INSERT INTO legacy_oauth_temp VALUES (1,'temporary retained bytes')")
        .await?;
    if partial {
        // Start with the OTHER canonical table: a missed last-table collision
        // creates the first index; a missed index collision creates the first
        // table. Both mutations must be caught by the complete schema image.
        let spec = if collision.index_name() {
            &SPECS[1]
        } else {
            &SPECS[0]
        };
        manager
            .create_table(definition(spec, DatabaseBackend::Sqlite))
            .await?;
        if collision.index_name() {
            retain_auxiliary(db).await?;
        } else {
            insert_slot(db).await?;
        }
    }
    match collision {
        Collision::MainView => {
            db.execute_unprepared(
                "CREATE VIEW TENANCY_RECEIPT_AUXILIARY AS SELECT payload FROM legacy_oauth_data",
            )
            .await?;
        }
        Collision::TempView => {
            db.execute_unprepared("CREATE TEMP VIEW TeNaNcY_ReCeIpT_AuXiLiArY AS SELECT payload FROM legacy_oauth_temp").await?;
        }
        Collision::MainTable | Collision::TempTable => {
            // Even the exact expected columns/PK under a case alias cannot be
            // adopted as owned storage or shadow the canonical main table.
            let mut table = definition(&SPECS[1], DatabaseBackend::Sqlite);
            table.table(Alias::new("TENANCY_RECEIPT_AUXILIARY"));
            let sql = table.to_string(SqliteQueryBuilder);
            let sql = if matches!(collision, Collision::TempTable) {
                ensure!(sql.starts_with("CREATE TABLE"));
                sql.replacen("CREATE TABLE", "CREATE TEMP TABLE", 1)
            } else {
                sql
            };
            db.execute_unprepared(&sql).await?;
            db.execute_unprepared("INSERT INTO TENANCY_RECEIPT_AUXILIARY VALUES ('i','t','e','r','case alias historical bytes','d',0)").await?;
        }
        Collision::MainIndex => {
            db.execute_unprepared(
                "CREATE INDEX IDX_TENANCY_OAUTH_PENDING ON legacy_oauth_data(payload)",
            )
            .await?;
        }
        Collision::TempIndex => {
            db.execute_unprepared(
                "CREATE INDEX temp.IdX_TeNaNcY_OaUtH_PeNdInG ON legacy_oauth_temp(payload)",
            )
            .await?;
        }
        Collision::IndexNamedTable => {
            db.execute_unprepared(
                "CREATE INDEX TENANCY_RECEIPT_AUXILIARY ON legacy_oauth_data(payload)",
            )
            .await?;
        }
        Collision::ViewNamedIndex => {
            db.execute_unprepared(
                "CREATE VIEW IDX_TENANCY_OAUTH_PENDING AS SELECT payload FROM legacy_oauth_data",
            )
            .await?;
        }
    }
    let before = image(db).await?;
    for _ in 0..2 {
        ensure!(
            Migration.up(&manager).await.is_err(),
            "accepted {collision:?}, partial={partial}"
        );
        ensure!(
            image(db).await? == before,
            "refused upgrade changed {collision:?}, partial={partial}"
        );
    }
    Ok(())
}

#[tokio::test]
async fn sqlite_identifier_aliases_refuse_before_any_ddl_and_preserve_all_rows() -> Result<()> {
    for partial in [false, true] {
        for collision in [
            Collision::MainView,
            Collision::MainTable,
            Collision::TempView,
            Collision::TempTable,
            Collision::MainIndex,
            Collision::TempIndex,
            Collision::IndexNamedTable,
            Collision::ViewNamedIndex,
        ] {
            let mut options = ConnectOptions::new("sqlite::memory:");
            options.min_connections(1).max_connections(1);
            let db = Database::connect(options).await?;
            let result =
                tokio::time::timeout(Duration::from_secs(5), case(&db, partial, collision)).await;
            db.close().await?;
            result??;
        }
    }
    Ok(())
}

#[tokio::test]
async fn sqlite_exact_owned_partial_tables_and_canonical_index_still_resume() -> Result<()> {
    let mut options = ConnectOptions::new("sqlite::memory:");
    options.min_connections(1).max_connections(1);
    let db = Database::connect(options).await?;
    let result = tokio::time::timeout(Duration::from_secs(5), async {
        let manager = SchemaManager::new(&db);
        for spec in SPECS {
            manager
                .create_table(definition(spec, DatabaseBackend::Sqlite))
                .await?;
        }
        insert_slot(&db).await?;
        retain_auxiliary(&db).await?;
        let before = image(&db).await?;
        Migration.up(&manager).await?;
        let after = image(&db).await?;
        ensure!(after.contents == before.contents);
        ensure!(after.objects.len() == before.objects.len() + 1);
        ensure!(
            before
                .objects
                .iter()
                .all(|object| after.objects.contains(object))
        );
        Migration.up(&manager).await?;
        ensure!(
            image(&db).await? == after,
            "canonical rerun changed owned schema/rows"
        );
        Ok::<_, anyhow::Error>(())
    })
    .await;
    db.close().await?;
    result?
}
