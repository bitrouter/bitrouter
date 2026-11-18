//! Read independent BRO history without making an old dataset serveable.

use std::time::Duration;

use anyhow::{Context, Result, ensure};
use bitrouter::db;
use sea_orm::{ConnectionTrait, DatabaseConnection, DbBackend, Statement};
use serde_json::Value;
use sha2::{Digest, Sha256};

// Reuse the transfer reader's bounded Base64 reconstruction and digest checks
// against the independent journal produced by the original fixture exporter.
#[path = "../../src/db/mixed_store/activation/transfer/codec.rs"]
mod codec;

const THREAD: &str = "1149fbda-eb06-43b7-8d0e-108e857d9c2d";
const OWNER: &str = "46c9adc1-3b84-42b3-8495-24d9b26e9e5f";
const DATABASE: &[u8] = include_bytes!("../fixtures/baseline-runtime-v7/database.sqlite.zst");
const JOURNAL: &[u8] = include_bytes!("../fixtures/baseline-runtime-v7/journal.json.zst");

#[tokio::test]
async fn baseline_v7_history_is_read_only_and_never_adopts_an_old_store() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(60), exercise())
        .await
        .context("baseline database read exceeded its deadline")?
}

async fn exercise() -> Result<()> {
    let database = zstd::bulk::decompress(DATABASE, 64 * 1024 * 1024)?;
    ensure!(
        hex::encode(Sha256::digest(&database))
            == "28e61cdbf75ad2a918404ea30a185915d59b2cf58e67d5ac5e3b7dbc902064e9",
        "baseline database identity changed"
    );
    let journal = zstd::bulk::decompress(JOURNAL, 16 * 1024 * 1024)?;
    ensure!(
        hex::encode(Sha256::digest(&journal))
            == "c460809ed7218760ba88faf4a7fb56a1da03422555a66faecd4315bce99b450f",
        "baseline journal identity changed"
    );
    let expected: Value = serde_json::from_slice(&journal)?;
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("baseline.sqlite");
    std::fs::write(&path, &database)?;
    let url = format!("sqlite://{}", path.display());
    ensure!(
        db::connect(&url).await.is_err(),
        "old dataset became writable"
    );
    ensure!(
        std::fs::read(&path)? == database,
        "refusal rewrote old bytes"
    );
    let mut original = None;
    // Each independent history read uses a fresh read-only pool. No migration,
    // serving authority, live restoration or data conversion is implied.
    for _ in 0..2 {
        let connection = db::connect(&format!("{url}?mode=ro")).await?;
        let result = check_database(&connection, &expected, &mut original).await;
        let closed = connection.close().await;
        result?;
        closed?;
    }
    ensure!(
        std::fs::read(&path)? == database,
        "history reads rewrote old bytes"
    );
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
struct RawState {
    ledger: Vec<(String, i64)>,
    records: Vec<(i64, String)>,
}

async fn raw_state(connection: &DatabaseConnection) -> Result<RawState> {
    let ledger = connection
        .query_all(Statement::from_string(
            DbBackend::Sqlite,
            "SELECT version, applied_at FROM seaql_migrations ORDER BY version",
        ))
        .await?
        .into_iter()
        .map(|row| Ok((row.try_get("", "version")?, row.try_get("", "applied_at")?)))
        .collect::<Result<Vec<_>, sea_orm::DbErr>>()?;
    let records = connection
        .query_all(Statement::from_string(
            DbBackend::Sqlite,
            "SELECT sequence, payload FROM bro_execution_records ORDER BY execution_id, sequence",
        ))
        .await?
        .into_iter()
        .map(|row| Ok((row.try_get("", "sequence")?, row.try_get("", "payload")?)))
        .collect::<Result<Vec<_>, sea_orm::DbErr>>()?;
    Ok(RawState { ledger, records })
}

async fn check_database(
    connection: &DatabaseConnection,
    expected: &Value,
    original: &mut Option<RawState>,
) -> Result<()> {
    let integrity = connection
        .query_one(Statement::from_string(
            DbBackend::Sqlite,
            "PRAGMA integrity_check",
        ))
        .await?
        .context("missing integrity result")?
        .try_get::<String>("", "integrity_check")?;
    ensure!(integrity == "ok", "{integrity}");
    let before = raw_state(connection).await?;
    ensure!(before.records.len() == 284);
    if let Some(original) = original.as_ref() {
        ensure!(before == *original, "reopening changed the baseline state");
    } else {
        ensure!(
            before.ledger.len() == 26,
            "baseline migration ledger changed"
        );
    }
    let owner = connection
        .query_one(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "SELECT stopped_at_ms FROM bro_runtime_owners WHERE server_instance_id = ?",
            [OWNER.into()],
        ))
        .await?
        .context("baseline runtime owner missing")?;
    ensure!(owner.try_get::<Option<i64>>("", "stopped_at_ms")?.is_some());
    let heads = connection
        .query_all(Statement::from_string(
            DbBackend::Sqlite,
            "SELECT id, version, format_version FROM bro_executions",
        ))
        .await?;
    ensure!(
        heads.len() == 1
            && heads[0].try_get::<String>("", "id")? == THREAD
            && heads[0].try_get::<i64>("", "version")? == 284
            && heads[0].try_get::<i32>("", "format_version")? == 7
    );
    let expected = expected
        .as_array()
        .context("baseline journal is not an array")?;
    ensure!(expected.len() == before.records.len());
    for (index, ((seq, payload), expected)) in before.records.iter().zip(expected).enumerate() {
        ensure!(*seq == i64::try_from(index + 1)?);
        ensure!(
            serde_json::from_str::<Value>(&codec::decode(payload).map_err(anyhow::Error::msg)?)?
                == *expected,
            "journal differs at sequence {seq}"
        );
    }
    ensure!(
        raw_state(connection).await? == before,
        "reads changed historical baseline bytes"
    );
    *original = Some(before);
    Ok(())
}
