//! Current declarative gateway schema and exact backend catalog validation.
use anyhow::{Context, Result, ensure};
use sea_orm::{ConnectionTrait, DatabaseBackend, DbErr, QueryResult, Statement};
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};

#[path = "current_schema/catalog.rs"]
mod catalog;
#[path = "current_schema/definition.rs"]
mod definition;
#[path = "current_schema/sqlite.rs"]
mod sqlite;
#[cfg(test)]
#[path = "current_schema/tests.rs"]
mod tests;

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Table {
    name: String,
    #[serde(default)]
    sqlite_only: bool,
    columns: Vec<Column>,
    primary: Vec<String>,
    unique: Vec<Vec<String>>,
    foreign: Vec<Foreign>,
    checks: Vec<String>,
    #[serde(default)]
    sqlite_checks: Vec<String>,
    indexes: Vec<Index>,
}
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Column {
    name: String,
    kind: String,
    mysql_kind: Option<String>,
    sqlite_kind: Option<String>,
    #[serde(default)]
    sqlite_only: bool,
    nullable: bool,
    binary: bool,
    automatic: bool,
    default: Option<String>,
}
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
struct Foreign {
    columns: Vec<String>,
    table: String,
    references: Vec<String>,
    delete: String,
    update: String,
}
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Index {
    name: String,
    columns: Vec<String>,
    unique: bool,
    #[serde(default)]
    sqlite_only: bool,
    #[serde(default)]
    mysql_prefixes: BTreeMap<String, u32>,
}
#[derive(Clone, Default)]
struct ObservedIndex {
    columns: Vec<String>,
    prefixes: Vec<Option<i64>>,
    unique: bool,
    primary: bool,
}
#[derive(Default)]
struct Constraint {
    kind: String,
    columns: Vec<String>,
    references: Vec<String>,
    table: String,
    delete: String,
    update: String,
    expression: String,
}
fn expected(backend: DatabaseBackend) -> Result<Vec<Table>> {
    let mut tables: Vec<Table> = serde_json::from_str(include_str!("current_schema/expected.json"))
        .context("invalid compiled current schema model")?;
    if backend != DatabaseBackend::Sqlite {
        tables.retain(|table| !table.sqlite_only);
        for table in &mut tables {
            table.columns.retain(|column| !column.sqlite_only);
            table.indexes.retain(|index| !index.sqlite_only);
        }
    } else {
        for table in &mut tables {
            table.checks.append(&mut table.sqlite_checks);
        }
    }
    Ok(tables)
}

/// Render fresh DDL from the same definition used by all catalog validators.
pub(super) fn statements(backend: DatabaseBackend) -> Result<Vec<String>> {
    definition::statements(backend, expected(backend)?)
}

pub(super) async fn validate_sqlite_metadata(
    db: &impl ConnectionTrait,
    selected: Option<&[&str]>,
) -> Result<()> {
    for table in expected(DatabaseBackend::Sqlite)? {
        if selected.is_none_or(|names| names.contains(&table.name.as_str())) {
            sqlite::validate(db, &table).await?;
        }
    }
    Ok(())
}

/// Validate all ordinary server tables before selecting current data.
///
/// # Errors
/// Rejects incomplete catalogs, altered constraints, identity collations,
/// mutation hooks, and nontransactional storage engines. Performs no writes.
pub(crate) async fn validate(db: &impl ConnectionTrait) -> Result<(), DbErr> {
    validate_inner(db)
        .await
        .map_err(|error| DbErr::Custom(format!("incompatible current server schema: {error:#}")))
}
async fn rows(db: &impl ConnectionTrait, sql: &str) -> Result<Vec<QueryResult>> {
    let result = db
        .query_all(Statement::from_string(db.get_database_backend(), sql))
        .await?;
    ensure!(result.len() < 8193, "catalog exceeds current schema bound");
    Ok(result)
}
async fn validate_inner(db: &impl ConnectionTrait) -> Result<()> {
    let backend = db.get_database_backend();
    ensure!(
        backend != DatabaseBackend::Sqlite,
        "server catalog required"
    );
    let expected = expected(backend)?;
    let tables = rows(db, catalog::tables(backend)).await?;
    let mut names = BTreeSet::new();
    for row in tables {
        let name: String = row.try_get("", "table_name")?;
        ensure!(names.insert(name.clone()), "duplicate catalog table");
        ensure!(
            row.try_get::<String>("", "compatible")? == "yes",
            "unsupported table storage or mutation hooks: {name}"
        );
    }
    ensure!(
        expected.iter().all(|table| names.contains(&table.name)),
        "missing current table"
    );
    let ordinary = expected
        .iter()
        .map(|table| table.name.as_str())
        .collect::<BTreeSet<_>>();
    let extra = names.iter().any(|name| !ordinary.contains(name.as_str()));
    let (shared_tables, shared_sequences) = shared_composition(db, extra).await?;
    ensure!(
        names.len() == expected.len() + shared_tables.len()
            && shared_tables.iter().all(|name| names.contains(name)),
        "foreign table outside validated composition"
    );
    let mut columns: BTreeMap<String, Vec<QueryResult>> = BTreeMap::new();
    for row in rows(db, catalog::columns(backend)).await? {
        let table: String = row.try_get("", "table_name")?;
        if !shared_tables.contains(&table) {
            columns.entry(table).or_default().push(row);
        }
    }
    let mut indexes: BTreeMap<String, BTreeMap<String, ObservedIndex>> = BTreeMap::new();
    for row in rows(db, catalog::indexes(backend)).await? {
        let table: String = row.try_get("", "table_name")?;
        if shared_tables.contains(&table) {
            continue;
        }
        let name: String = row.try_get("", "index_name")?;
        ensure!(
            row.try_get::<String>("", "compatible")? == "yes",
            "unsupported index definition: {table}/{name}"
        );
        let index = indexes.entry(table).or_default().entry(name).or_default();
        index.unique = row.try_get::<String>("", "unique_value")? == "yes";
        index.primary = row.try_get::<String>("", "primary_value")? == "yes";
        index.columns.push(row.try_get("", "column_name")?);
        index.prefixes.push(row.try_get("", "prefix_length")?);
    }
    let mut constraints: BTreeMap<String, BTreeMap<String, Constraint>> = BTreeMap::new();
    for row in rows(db, catalog::constraints(backend)).await? {
        let table: String = row.try_get("", "table_name")?;
        if shared_tables.contains(&table) {
            continue;
        }
        let name: String = row.try_get("", "constraint_name")?;
        ensure!(
            row.try_get::<String>("", "compatible")? == "yes",
            "unenforced or altered constraint: {table}/{name}"
        );
        let item = constraints
            .entry(table)
            .or_default()
            .entry(name)
            .or_default();
        item.kind = row.try_get("", "kind")?;
        item.table = row.try_get("", "foreign_table")?;
        item.delete = row.try_get("", "delete_rule")?;
        item.update = row.try_get("", "update_rule")?;
        item.expression = row.try_get("", "expression")?;
        if backend == DatabaseBackend::Postgres {
            item.columns = serde_json::from_str(&row.try_get::<String>("", "columns_json")?)?;
            item.references = serde_json::from_str(&row.try_get::<String>("", "references_json")?)?;
        } else {
            if let Some(column) = row.try_get::<Option<String>>("", "column_name")? {
                item.columns.push(column);
            }
            if let Some(column) = row.try_get::<Option<String>>("", "foreign_column")? {
                item.references.push(column);
            }
        }
    }
    for table in &expected {
        let observed = columns
            .remove(&table.name)
            .context("missing table columns")?;
        ensure!(
            observed.len() == table.columns.len(),
            "column inventory differs: {}",
            table.name
        );
        for (column, row) in table.columns.iter().zip(&observed) {
            if backend == DatabaseBackend::MySql && table.name == "requests" && column.binary {
                ensure!(
                    row.try_get::<String>("", "binary_value")? == "yes",
                    "requests.{} requires utf8mb4_0900_bin; stop writers and correct the metering identity collations on a verified backup before restart",
                    column.name
                );
            }
            validate_column(backend, column, row)
                .with_context(|| format!("column {}.{}", table.name, column.name))?;
        }
        validate_constraints(
            backend,
            table,
            constraints.remove(&table.name).unwrap_or_default(),
        )?;
        validate_indexes(
            backend,
            table,
            indexes.remove(&table.name).unwrap_or_default(),
        )?;
    }
    ensure!(
        columns.is_empty() && indexes.is_empty() && constraints.is_empty(),
        "foreign catalog objects"
    );
    if backend == DatabaseBackend::Postgres {
        let mut automatic = expected
            .iter()
            .flat_map(|table| {
                table
                    .columns
                    .iter()
                    .filter(|column| column.automatic)
                    .map(|column| (table.name.clone(), column.name.clone()))
            })
            .collect::<BTreeSet<_>>();
        for row in rows(db, catalog::SEQUENCES).await? {
            let identity = (
                row.try_get::<String>("", "table_name")?,
                row.try_get::<String>("", "column_name")?,
            );
            if shared_sequences.contains(&identity) {
                continue;
            }
            ensure!(
                row.try_get::<String>("", "compatible")? == "yes",
                "invalid sequence definition"
            );
            ensure!(automatic.remove(&identity), "unowned or duplicate sequence");
        }
        ensure!(automatic.is_empty(), "missing automatic column sequence");
    }
    Ok(())
}

async fn shared_composition(
    db: &impl ConnectionTrait,
    extra: bool,
) -> Result<(BTreeSet<String>, BTreeSet<(String, String)>)> {
    if !extra {
        return Ok(Default::default());
    }
    ensure!(
        db.get_database_backend() == DatabaseBackend::Postgres,
        "foreign tables in ordinary server schema"
    );
    #[cfg(feature = "multi-instance")]
    {
        let (tables, sequences) = crate::gateway::shared::schema::validated_tables(db).await?;
        Ok((
            tables.into_iter().map(str::to_owned).collect(),
            sequences
                .into_iter()
                .map(|(table, column)| (table.to_owned(), column.to_owned()))
                .collect(),
        ))
    }
    #[cfg(not(feature = "multi-instance"))]
    anyhow::bail!("shared schema composition requires the shared gateway profile")
}
fn kind(backend: DatabaseBackend, column: &Column) -> String {
    if backend == DatabaseBackend::Postgres {
        return column.kind.replacen("varchar", "character varying", 1);
    }
    if let Some(kind) = &column.mysql_kind {
        return kind.clone();
    }
    match column.kind.as_str() {
        "integer" => "int".into(),
        "boolean" => "tinyint(1)".into(),
        "varchar" => "varchar(255)".into(),
        other => other.into(),
    }
}
fn validate_column(backend: DatabaseBackend, column: &Column, row: &QueryResult) -> Result<()> {
    ensure!(
        row.try_get::<String>("", "column_name")? == column.name,
        "column order/name differs"
    );
    ensure!(
        row.try_get::<String>("", "kind")? == kind(backend, column),
        "column type differs"
    );
    ensure!(
        (row.try_get::<String>("", "nullable")? == "yes") == column.nullable,
        "nullability differs"
    );
    ensure!(
        row.try_get::<String>("", "compatible")? == "yes",
        "generated, identity or foreign type"
    );
    if column.kind.starts_with("varchar") || column.kind == "text" {
        ensure!(
            row.try_get::<String>("", "collation_valid")? == "yes",
            "unsupported text comparison"
        );
        if column.binary {
            ensure!(
                row.try_get::<String>("", "binary_value")? == "yes",
                "identity collation is not byte-exact"
            );
        }
    }
    ensure!(
        (row.try_get::<String>("", "automatic")? == "yes") == column.automatic,
        "automatic column behavior differs"
    );
    let default: Option<String> = row.try_get("", "default_value")?;
    ensure!(
        default_matches(backend, column, default.as_deref()),
        "column default differs"
    );
    Ok(())
}
fn default_matches(backend: DatabaseBackend, column: &Column, actual: Option<&str>) -> bool {
    if column.automatic {
        return if backend == DatabaseBackend::Postgres {
            actual.is_some_and(|value| {
                value.starts_with("nextval('") && value.ends_with("'::regclass)")
            })
        } else {
            actual.is_none()
        };
    }
    match (&column.default, actual) {
        (None, None) => true,
        (Some(expected), Some(actual)) => {
            if backend == DatabaseBackend::MySql
                && let Some(literal) = expected
                    .strip_prefix('\'')
                    .and_then(|value| value.strip_suffix('\''))
            {
                return actual == literal.replace("''", "'");
            }
            let actual = normalized(actual);
            let expected = normalized(expected);
            actual == expected
        }
        _ => false,
    }
}
fn validate_indexes(
    backend: DatabaseBackend,
    table: &Table,
    mut actual: BTreeMap<String, ObservedIndex>,
) -> Result<()> {
    for expected in &table.indexes {
        let observed = actual
            .remove(&expected.name)
            .with_context(|| format!("missing index {}", expected.name))?;
        let prefixes = expected
            .columns
            .iter()
            .map(|column| {
                if backend == DatabaseBackend::MySql {
                    expected.mysql_prefixes.get(column).copied().map(i64::from)
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();
        ensure!(
            observed.columns == expected.columns
                && observed.prefixes == prefixes
                && observed.unique == expected.unique
                && !observed.primary,
            "index changed: {}",
            expected.name
        );
    }
    let mut primary = !table.primary.is_empty();
    let mut unique = table.unique.clone();
    for (name, observed) in actual {
        ensure!(
            observed.prefixes.iter().all(Option::is_none),
            "unexpected prefix on constraint or foreign-key index: {}/{}",
            table.name,
            name
        );
        if observed.primary && primary {
            ensure!(
                observed.unique && observed.columns == table.primary,
                "primary index differs: {}",
                table.name
            );
            primary = false;
        } else if observed.unique && !observed.primary {
            let position = unique
                .iter()
                .position(|columns| columns == &observed.columns)
                .context("unexpected unique index")?;
            unique.remove(position);
        } else {
            ensure!(
                backend == DatabaseBackend::MySql
                    && !observed.primary
                    && table
                        .foreign
                        .iter()
                        .any(|foreign| foreign.columns == observed.columns),
                "unexpected index: {}/{}",
                table.name,
                name
            );
        }
    }
    ensure!(
        !primary && unique.is_empty(),
        "missing primary/unique index: {}",
        table.name
    );
    Ok(())
}
fn validate_constraints(
    backend: DatabaseBackend,
    table: &Table,
    actual: BTreeMap<String, Constraint>,
) -> Result<()> {
    let mut primary = !table.primary.is_empty();
    let mut unique = table.unique.clone();
    if backend == DatabaseBackend::MySql {
        unique.extend(
            table
                .indexes
                .iter()
                .filter(|index| index.unique)
                .map(|index| index.columns.clone()),
        );
    }
    let mut foreign = table.foreign.clone();
    let mut checks = table
        .checks
        .iter()
        .map(|expression| normalized(expression))
        .collect::<Vec<_>>();
    for (_, constraint) in actual {
        match constraint.kind.as_str() {
            "p" if primary => {
                ensure!(
                    constraint.columns == table.primary,
                    "primary key differs: {}",
                    table.name
                );
                primary = false;
            }
            "u" => {
                let position = unique
                    .iter()
                    .position(|columns| columns == &constraint.columns)
                    .context("unexpected unique constraint")?;
                unique.remove(position);
            }
            "f" => {
                let observed = Foreign {
                    columns: constraint.columns,
                    table: constraint.table,
                    references: constraint.references,
                    delete: rule(backend, &constraint.delete),
                    update: rule(backend, &constraint.update),
                };
                let position = foreign
                    .iter()
                    .position(|expected| {
                        expected.columns == observed.columns
                            && expected.table == observed.table
                            && expected.references == observed.references
                            && rule(backend, &expected.delete) == observed.delete
                            && rule(backend, &expected.update) == observed.update
                    })
                    .context("foreign key differs")?;
                foreign.remove(position);
            }
            "c" => {
                let expression = normalized(&constraint.expression);
                let position = checks
                    .iter()
                    .position(|expected| check_matches(backend, expected, &expression))
                    .with_context(|| format!("check constraint differs: {}", table.name))?;
                checks.remove(position);
            }
            _ => anyhow::bail!("unexpected constraint: {}", table.name),
        }
    }
    ensure!(
        !primary && unique.is_empty() && foreign.is_empty() && checks.is_empty(),
        "missing current constraints: {}",
        table.name
    );
    Ok(())
}
fn check_matches(backend: DatabaseBackend, expected: &str, observed: &str) -> bool {
    // MySQL 8.4 can serialize quotes in CHECK_CLAUSE as backslash-quote.
    // Compare the escaped spelling of the trusted pattern without decoding
    // escapes in an arbitrary catalog expression.
    expected == observed
        || (backend == DatabaseBackend::MySql && expected.replace('\'', "\\'") == observed)
}
fn rule(backend: DatabaseBackend, value: &str) -> String {
    if backend == DatabaseBackend::MySql && value == "NO ACTION" {
        "RESTRICT".into()
    } else {
        value.into()
    }
}
fn normalized(value: &str) -> String {
    let casts = [
        "::character varying[]",
        "::character varying",
        "::text[]",
        "::text",
        "::integer",
        "::bigint",
    ];
    let mut literal = false;
    let mut out = String::new();
    let mut offset = 0;
    while offset < value.len() {
        let remaining = &value[offset..];
        if !literal {
            if let Some(cast) = casts.iter().find(|cast| remaining.starts_with(**cast)) {
                offset += cast.len();
                continue;
            }
            if remaining.starts_with("_utf8mb4'") || remaining.starts_with("_utf8mb4\\'") {
                offset += "_utf8mb4".len();
                continue;
            }
        }
        let Some(character) = remaining.chars().next() else {
            break;
        };
        offset += character.len_utf8();
        if character == '\'' {
            literal = !literal;
            out.push(character);
        } else if literal {
            out.push(character);
        } else if !character.is_ascii_whitespace()
            && !matches!(character, '(' | ')' | '"')
            && character != char::from(96)
        {
            out.push(character.to_ascii_lowercase());
        }
    }
    if out.starts_with("publication_state=anyarray[") && out.ends_with(']') {
        out = out.replacen("=anyarray[", "in", 1);
        out.pop();
    }
    out
}
