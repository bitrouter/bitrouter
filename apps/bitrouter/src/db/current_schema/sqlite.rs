//! Check SQLite's effective columns and indexes as well as its stored DDL.
use super::*;

async fn query(db: &impl ConnectionTrait, sql: String, name: &str) -> Result<Vec<QueryResult>> {
    Ok(db
        .query_all(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            sql,
            [name.into()],
        ))
        .await?)
}

pub(super) async fn validate(db: &impl ConnectionTrait, table: &Table) -> Result<()> {
    // Eight catalog rows fit the smallest borrowed offline reader page. Names,
    // defaults and types are bounded before they cross that reader boundary.
    for offset in (0..=table.columns.len()).step_by(8) {
        let rows = query(db, format!("SELECT CASE WHEN length(CAST(name AS BLOB))<=128 THEN name END AS name, CASE WHEN length(CAST(type AS BLOB))<=256 THEN type END AS kind, \"notnull\" AS required, CASE WHEN length(CAST(dflt_value AS BLOB))<=4096 THEN dflt_value END AS default_value, dflt_value IS NULL AS no_default, pk, hidden FROM pragma_table_xinfo(?, 'main') ORDER BY cid LIMIT 8 OFFSET {offset}"), &table.name).await?;
        let columns = &table.columns[offset..table.columns.len().min(offset + 8)];
        ensure!(
            rows.len() == columns.len(),
            "SQLite column inventory differs: {}",
            table.name
        );
        for (row, column) in rows.iter().zip(columns) {
            ensure!(
                row.try_get::<String>("", "name")? == column.name
                    && row
                        .try_get::<String>("", "kind")?
                        .eq_ignore_ascii_case(column.sqlite_kind.as_ref().unwrap_or(&column.kind))
                    && row.try_get::<i64>("", "required")? == i64::from(!column.nullable)
                    && row.try_get::<i64>("", "hidden")? == 0
                    && row.try_get::<Option<String>>("", "default_value")? == column.default
                    && (row.try_get::<i64>("", "no_default")? == 1) == column.default.is_none(),
                "SQLite column behavior differs: {}.{}",
                table.name,
                column.name
            );
            let primary = table
                .primary
                .iter()
                .position(|name| name == &column.name)
                .map_or(0, |index| index + 1);
            ensure!(
                row.try_get::<i64>("", "pk")? == i64::try_from(primary)?,
                "SQLite primary key order differs"
            );
        }
    }
    let integer_primary = table.primary.len() == 1
        && table.columns.iter().any(|column| {
            column.name == table.primary[0]
                && column.sqlite_kind.as_ref().unwrap_or(&column.kind) == "integer"
        });
    let mut primary = !table.primary.is_empty() && !integer_primary;
    let mut unique = table.unique.clone();
    let mut explicit = table
        .indexes
        .iter()
        .map(|index| (index.name.as_str(), index))
        .collect::<BTreeMap<_, _>>();
    let index_count = table.indexes.len() + table.unique.len() + usize::from(primary);
    for offset in (0..=index_count).step_by(8) {
        let rows = query(db, format!("SELECT CASE WHEN length(CAST(name AS BLOB))<=256 THEN name END AS name, origin, \"unique\" AS unique_value, partial FROM pragma_index_list(?, 'main') ORDER BY name LIMIT 8 OFFSET {offset}"), &table.name).await?;
        ensure!(
            rows.len() == (index_count - offset).min(8),
            "SQLite index inventory differs"
        );
        for row in rows {
            let name: String = row.try_get("", "name")?;
            ensure!(
                row.try_get::<i64>("", "partial")? == 0,
                "partial SQLite index"
            );
            let columns = query(db, "SELECT CASE WHEN length(CAST(name AS BLOB))<=128 THEN name END AS name, coll, \"desc\" AS descending, cid FROM pragma_index_xinfo(?, 'main') WHERE key=1 ORDER BY seqno LIMIT 8".into(), &name).await?;
            ensure!(columns.len() < 8, "SQLite index key exceeds current bound");
            let mut names = Vec::new();
            for column in columns {
                ensure!(
                    column.try_get::<String>("", "coll")? == "BINARY"
                        && column.try_get::<i64>("", "descending")? == 0
                        && column.try_get::<i64>("", "cid")? >= 0,
                    "altered SQLite index expression or comparison"
                );
                names.push(column.try_get::<String>("", "name")?);
            }
            let is_unique = row.try_get::<i64>("", "unique_value")? == 1;
            match row.try_get::<String>("", "origin")?.as_str() {
                "pk" if primary => {
                    ensure!(
                        is_unique && names == table.primary,
                        "SQLite primary index differs"
                    );
                    primary = false;
                }
                "u" if is_unique => {
                    let at = unique
                        .iter()
                        .position(|expected| expected == &names)
                        .context("unexpected SQLite unique key")?;
                    unique.remove(at);
                }
                "c" => {
                    let expected = explicit
                        .remove(name.as_str())
                        .context("unexpected SQLite index")?;
                    ensure!(
                        is_unique == expected.unique && names == expected.columns,
                        "SQLite index differs: {name}"
                    );
                }
                _ => anyhow::bail!("unexpected SQLite index origin"),
            }
        }
    }
    ensure!(
        !primary && unique.is_empty() && explicit.is_empty(),
        "missing SQLite index"
    );
    Ok(())
}
