//! Copy historical Thread records without decoding them or invoking tools.
use anyhow::{Context, Result};
use sea_orm::{ConnectionTrait, DatabaseBackend, DatabaseConnection, Statement};
use std::{fs, path::Path};

pub(super) async fn install_approved(dir: &Path, db: &DatabaseConnection) -> Result<String> {
    const CAPTURE: &[u8] = include_bytes!("../fixtures/m0-approved-write/database.sqlite.zst");
    install_capture(dir, db, CAPTURE, true).await?;
    Ok(db
        .query_one(Statement::from_string(
            DatabaseBackend::Sqlite,
            "SELECT id FROM bro_executions",
        ))
        .await?
        .context("captured thread")?
        .try_get_by_index(0)?)
}

pub(super) async fn install_capture(
    dir: &Path,
    db: &DatabaseConnection,
    bytes: &[u8],
    with_metering: bool,
) -> Result<()> {
    let capture = dir.join("captured.sqlite");
    fs::write(&capture, zstd::stream::decode_all(bytes)?)?;
    db.execute(Statement::from_sql_and_values(
        DatabaseBackend::Sqlite,
        "ATTACH ? AS captured",
        [capture.to_str().context("fixture path")?.into()],
    ))
    .await?;
    for table in [
        "bro_executions",
        "bro_execution_index",
        "bro_execution_records",
        "bro_acceptance_keys",
        "bro_runtime_owners",
    ] {
        db.execute_unprepared(&format!(
            "INSERT INTO main.{table} SELECT * FROM captured.{table}"
        ))
        .await?;
    }
    db.execute_unprepared("UPDATE main.bro_runtime_ownership SET (generation,server_instance_id,stopped_at_ms)=(SELECT generation,server_instance_id,stopped_at_ms FROM captured.bro_runtime_ownership WHERE id=1) WHERE id=1").await?;
    if with_metering {
        db.execute_unprepared("INSERT INTO main.requests SELECT * FROM captured.requests")
            .await?;
    }
    db.execute_unprepared("DETACH DATABASE captured").await?;
    Ok(())
}
