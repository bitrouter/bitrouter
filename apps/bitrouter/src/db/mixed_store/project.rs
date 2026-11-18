use super::*;
use catalog::Object;
use sea_orm::TransactionTrait;

pub(super) async fn import(
    snapshot: &Path,
    path: &Path,
    owner: Owner,
    objects: &[Object],
) -> Result<BTreeMap<String, i64>> {
    private_file(path)?;
    let db = open(path, false).await?;
    let result = build(&db, snapshot, owner, objects).await;
    db.close().await?;
    result
}

async fn build(
    db: &DatabaseConnection,
    snapshot: &Path,
    owner: Owner,
    objects: &[Object],
) -> Result<BTreeMap<String, i64>> {
    let mut uri =
        url::Url::from_file_path(snapshot).map_err(|_| anyhow::anyhow!("invalid snapshot path"))?;
    uri.set_query(Some("mode=ro&immutable=1"));
    let tx = db.begin().await?;
    tx.execute(Statement::from_sql_and_values(
        DatabaseBackend::Sqlite,
        "ATTACH DATABASE ? AS m0",
        [uri.to_string().into()],
    ))
    .await?;
    tx.execute_unprepared("PRAGMA defer_foreign_keys=ON")
        .await?;
    let tables = create_tables(&tx, owner, objects).await?;
    let mut counts = BTreeMap::new();
    for table in tables {
        if table == "seaql_migrations" {
            tx.execute(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
                "INSERT INTO main.seaql_migrations(version,applied_at) SELECT ?,applied_at FROM m0.seaql_migrations", [owner.pending_marker().into()])).await?;
            continue;
        }
        let predicate = if table == "checkpoint_evolution" {
            catalog::evolution(owner)
        } else {
            "1"
        };
        let expected = count(
            &tx,
            format!("SELECT count(*) AS n FROM m0.{table} WHERE {predicate}"),
        )
        .await?;
        if expected > 0 {
            tx.execute_unprepared(&format!("INSERT INTO main.{table}(rowid,{columns}) SELECT rowid,{columns} FROM m0.{table} WHERE {predicate}",
                columns=columns(&tx, table).await?.join(","))).await?;
        }
        let actual = count(&tx, format!("SELECT count(*) AS n FROM main.{table}")).await?;
        ensure!(actual == expected, "row count mismatch for {table}");
        // Include rowid (also unique when an ordinary SQLite primary key is
        // nullable), storage class, NULLs, binary text and all opaque payloads.
        if expected > 0 {
            let comparisons = columns(&tx, table)
                .await?
                .iter()
                .map(|c| format!("s.{c} IS t.{c} COLLATE BINARY AND typeof(s.{c})=typeof(t.{c})"))
                .collect::<Vec<_>>()
                .join(" AND ");
            ensure!(tx.query_one(sql(format!("SELECT 1 FROM m0.{table} s WHERE ({predicate}) AND NOT EXISTS (SELECT 1 FROM main.{table} t WHERE s.rowid=t.rowid AND {comparisons}) LIMIT 1"))).await?.is_none(),
                "row identity or payload mismatch for {table}");
        }
        counts.insert(table.clone(), expected);
    }
    sequences(&tx, owner).await?;
    catalog::integrity(&tx).await?;
    tx.commit().await?;
    Ok(counts)
}

/// Both independent row projections are committed and reconciled before any
/// product marker can become visible. Both files are fresh DELETE-journal
/// databases on one volume. SQLite commits their markers via its super-journal.
pub(super) async fn finish_pair(service: &Path, bro: &Path, activate: bool) -> Result<()> {
    let db = open(service, false).await?;
    let result: Result<()> = async {
        let tx = db.begin().await?;
        tx.execute(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "ATTACH DATABASE ? AS bro",
            [bro.to_str()
                .context("BRO staging path must be UTF-8")?
                .into()],
        ))
        .await?;
        if !activate {
            // BRST identifies an inspection-only service projection even if its
            // SQLite file is taken out of the published directory.
            tx.execute_unprepared(
                "PRAGMA main.application_id=0x42525354; PRAGMA bro.application_id=0x42525354",
            )
            .await?;
        }
        for (schema, owner) in [("main", Owner::BitRouter), ("bro", Owner::Bro)] {
            let mode = tx
                .query_one(sql(format!("PRAGMA {schema}.journal_mode")))
                .await?
                .context("missing journal mode")?;
            ensure!(
                mode.try_get_by_index::<String>(0)? == "delete",
                "rollback journal required for paired publication"
            );
            let n = tx
                .execute(Statement::from_sql_and_values(
                    DatabaseBackend::Sqlite,
                    format!("UPDATE {schema}.seaql_migrations SET version=? WHERE version=?"),
                    [owner.marker().into(), owner.pending_marker().into()],
                ))
                .await?
                .rows_affected();
            ensure!(n == 1, "missing staged owner marker");
        }
        tx.commit().await?;
        Ok(())
    }
    .await;
    db.close().await?;
    result
}

pub(super) async fn create_tables<'a>(
    db: &impl ConnectionTrait,
    owner: Owner,
    objects: &'a [Object],
) -> Result<Vec<&'a String>> {
    let mut tables = Vec::new();
    for kind in ["table", "index"] {
        for object in objects.iter().filter(|o| o.kind == kind) {
            if catalog::belongs(&object.tbl_name, owner)? {
                db.execute_unprepared(&object.sql).await?;
                if kind == "table" {
                    tables.push(&object.name);
                }
            }
        }
    }
    Ok(tables)
}

async fn columns(db: &impl ConnectionTrait, table: &str) -> Result<Vec<String>> {
    let rows = db
        .query_all(sql(format!(
            "SELECT name FROM pragma_table_info('{table}','main') ORDER BY cid"
        )))
        .await?;
    rows.iter()
        .map(|row| {
            let name: String = row.try_get("", "name")?;
            Ok(format!("\"{name}\""))
        })
        .collect()
}

// AUTOINCREMENT can be ahead of the greatest surviving key, including in an
// empty table. Retain that high-water mark so recovery never reissues an ID.
async fn sequences(db: &impl ConnectionTrait, owner: Owner) -> Result<()> {
    let rows = db
        .query_all(sql("SELECT name,seq FROM m0.sqlite_sequence LIMIT 4"))
        .await?;
    ensure!(rows.len() <= 3, "invalid M0 sequence directory");
    let mut seen = std::collections::BTreeSet::new();
    for row in rows {
        let table: String = row.try_get("", "name")?;
        let seq: i64 = row.try_get("", "seq")?;
        ensure!(
            matches!(
                table.as_str(),
                "bro_execution_index" | "adequacy_reliability_events" | "eval_admission_events"
            ) && seen.insert(table.clone())
                && seq >= 0,
            "invalid M0 sequence"
        );
        if !catalog::belongs(&table, owner)? {
            continue;
        }
        let max = count(
            db,
            format!("SELECT coalesce(max(rowid),0) AS n FROM main.{table}"),
        )
        .await?;
        ensure!(
            seq >= max,
            "invalid M0 sequence high-water mark for {table}"
        );
        let affected = db
            .execute(Statement::from_sql_and_values(
                DatabaseBackend::Sqlite,
                "UPDATE main.sqlite_sequence SET seq=? WHERE name=?",
                [seq.into(), table.clone().into()],
            ))
            .await?
            .rows_affected();
        if affected == 0 {
            db.execute(Statement::from_sql_and_values(
                DatabaseBackend::Sqlite,
                "INSERT INTO main.sqlite_sequence(name,seq) VALUES (?,?)",
                [table.into(), seq.into()],
            ))
            .await?;
        }
    }
    ensure!(db.query_one(sql("SELECT 1 FROM main.sqlite_sequence t WHERE NOT EXISTS (SELECT 1 FROM m0.sqlite_sequence s WHERE s.name=t.name AND s.seq=t.seq) LIMIT 1")).await?.is_none(), "missing M0 sequence");
    Ok(())
}
