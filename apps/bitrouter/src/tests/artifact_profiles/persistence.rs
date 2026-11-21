//! Read gateway rows and artifact identity in the process tests.

use super::*;
use sea_orm::{ConnectionTrait, DatabaseConnection, DbBackend, Statement};

async fn strings(db: &DatabaseConnection, sql: String) -> Result<Vec<String>> {
    db.query_all(Statement::from_string(DbBackend::Sqlite, sql))
        .await?
        .into_iter()
        .map(|row| Ok(row.try_get("", "value")?))
        .collect()
}

fn identifier(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

pub(super) async fn table_rows(db: &DatabaseConnection, table: &str) -> Result<Vec<String>> {
    let columns = db
        .query_all(Statement::from_string(
            DbBackend::Sqlite,
            format!("PRAGMA table_info({})", identifier(table)),
        ))
        .await?
        .into_iter()
        .map(|row| row.try_get::<String>("", "name"))
        .collect::<Result<Vec<_>, _>>()?;
    ensure!(!columns.is_empty(), "missing table {table}");
    strings(
        db,
        format!(
            "SELECT json_array({}) AS value FROM {} ORDER BY value",
            columns
                .iter()
                .map(|name| identifier(name))
                .collect::<Vec<_>>()
                .join(","),
            identifier(table),
        ),
    )
    .await
}

pub(super) fn identity(fixture: &mut Fixture, binary: &Path) -> Result<ArtifactIdentity> {
    let mut command = fixture.command(binary);
    command.args(["--json", "build-info"]);
    fixture.run(command)?.json()
}
