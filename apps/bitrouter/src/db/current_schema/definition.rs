//! Backend DDL rendering for the compiled current schema declaration.
use super::*;

fn identifier(backend: DatabaseBackend, name: &str) -> String {
    if backend == DatabaseBackend::MySql {
        format!("`{}`", name.replace('`', "``"))
    } else {
        format!("\"{}\"", name.replace('"', "\"\""))
    }
}

fn names(backend: DatabaseBackend, columns: &[String]) -> String {
    columns
        .iter()
        .map(|name| identifier(backend, name))
        .collect::<Vec<_>>()
        .join(", ")
}

fn column(backend: DatabaseBackend, value: &Column) -> Result<String> {
    let kind = match backend {
        DatabaseBackend::Sqlite => value.sqlite_kind.as_ref().unwrap_or(&value.kind).clone(),
        DatabaseBackend::Postgres if value.automatic => {
            ensure!(matches!(value.kind.as_str(), "integer" | "bigint"));
            if value.kind == "bigint" {
                "bigserial"
            } else {
                "serial"
            }
            .into()
        }
        _ => super::kind(backend, value),
    };
    let mut sql = format!("{} {kind}", identifier(backend, &value.name));
    if value.binary {
        sql.push_str(match backend {
            DatabaseBackend::Sqlite => " COLLATE BINARY",
            DatabaseBackend::Postgres => " COLLATE \"C\"",
            DatabaseBackend::MySql => " CHARACTER SET utf8mb4 COLLATE utf8mb4_0900_bin",
        });
    }
    if !value.nullable {
        sql.push_str(" NOT NULL");
    }
    if value.automatic {
        match backend {
            DatabaseBackend::Sqlite => {
                ensure!(kind == "integer", "SQLite automatic key requires integer");
                sql.push_str(" PRIMARY KEY AUTOINCREMENT");
            }
            DatabaseBackend::MySql => sql.push_str(" AUTO_INCREMENT"),
            DatabaseBackend::Postgres => {}
        }
    }
    if let Some(default) = &value.default {
        sql.push_str(" DEFAULT ");
        sql.push_str(default);
    }
    Ok(sql)
}

fn table(backend: DatabaseBackend, value: &Table) -> Result<String> {
    let mut parts = value
        .columns
        .iter()
        .map(|c| column(backend, c))
        .collect::<Result<Vec<_>>>()?;
    let automatic = value.columns.iter().find(|column| column.automatic);
    if backend == DatabaseBackend::Sqlite
        && let Some(column) = automatic
    {
        ensure!(
            value.primary == [column.name.clone()],
            "automatic key must be the sole primary key"
        );
    } else if !value.primary.is_empty() {
        parts.push(format!("PRIMARY KEY ({})", names(backend, &value.primary)));
    }
    for columns in &value.unique {
        parts.push(format!("UNIQUE ({})", names(backend, columns)));
    }
    for foreign in &value.foreign {
        parts.push(format!(
            "FOREIGN KEY ({}) REFERENCES {} ({}) ON DELETE {} ON UPDATE {}",
            names(backend, &foreign.columns),
            identifier(backend, &foreign.table),
            names(backend, &foreign.references),
            foreign.delete,
            foreign.update,
        ));
    }
    for check in &value.checks {
        // Compiled checks contain double-quoted column names and single-quoted
        // literals. Only identifiers change between these backend dialects.
        let expression = if backend == DatabaseBackend::MySql {
            check.replace('"', "`")
        } else {
            check.clone()
        };
        parts.push(format!("CHECK {expression}"));
    }
    let mut sql = format!(
        "CREATE TABLE {} ({})",
        identifier(backend, &value.name),
        parts.join(", ")
    );
    if backend == DatabaseBackend::MySql {
        sql.push_str(" ENGINE=InnoDB");
    }
    Ok(sql)
}

fn index(backend: DatabaseBackend, table: &Table, value: &Index) -> String {
    let columns = value
        .columns
        .iter()
        .map(|name| {
            let mut sql = identifier(backend, name);
            if backend == DatabaseBackend::MySql
                && let Some(prefix) = value.mysql_prefixes.get(name)
            {
                sql.push_str(&format!(" ({prefix})"));
            }
            sql
        })
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "CREATE {}INDEX {} ON {} ({columns})",
        if value.unique { "UNIQUE " } else { "" },
        identifier(backend, &value.name),
        identifier(backend, &table.name)
    )
}

pub(super) fn statements(backend: DatabaseBackend, mut tables: Vec<Table>) -> Result<Vec<String>> {
    let mut installed = BTreeSet::new();
    let mut result = Vec::new();
    while !tables.is_empty() {
        let at = tables
            .iter()
            .position(|table| {
                table
                    .foreign
                    .iter()
                    .all(|foreign| installed.contains(&foreign.table))
            })
            .context("cyclic or missing current schema dependency")?;
        let value = tables.remove(at);
        ensure!(
            installed.insert(value.name.clone()),
            "duplicate current table"
        );
        result.push(table(backend, &value)?);
        result.extend(
            value
                .indexes
                .iter()
                .map(|value_index| index(backend, &value, value_index)),
        );
    }
    Ok(result)
}
