//! Read the actual pre-refactor database, including its compressed row envelopes.

use std::time::Duration;

use anyhow::{Context, Result, ensure};
use bitrouter::agent_store::DatabaseExecutionStore;
use bitrouter::db;
use bitrouter::harness::storage::{ExecutionRecord, ExecutionStore};
use sea_orm::{ConnectionTrait, DatabaseConnection, DbBackend, Statement};
use serde_json::Value;
use sha2::{Digest, Sha256};

const THREAD: &str = "1149fbda-eb06-43b7-8d0e-108e857d9c2d";
const OWNER: &str = "46c9adc1-3b84-42b3-8495-24d9b26e9e5f";
const DATABASE: &[u8] = include_bytes!("../fixtures/baseline-runtime-v7/database.sqlite.zst");
const JOURNAL: &[u8] = include_bytes!("../fixtures/baseline-runtime-v7/journal.json.zst");

#[tokio::test]
async fn baseline_v7_database_reopens_without_rewriting_records() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(60), exercise())
        .await
        .context("baseline database read exceeded its deadline")?
}

async fn exercise() -> Result<()> {
    let database = zstd::stream::decode_all(DATABASE)?;
    ensure!(
        hex::encode(Sha256::digest(&database))
            == "28e61cdbf75ad2a918404ea30a185915d59b2cf58e67d5ac5e3b7dbc902064e9",
        "baseline database identity changed"
    );
    let journal = zstd::stream::decode_all(JOURNAL)?;
    ensure!(
        hex::encode(Sha256::digest(&journal))
            == "c460809ed7218760ba88faf4a7fb56a1da03422555a66faecd4315bce99b450f",
        "baseline journal identity changed"
    );
    let expected: Value = serde_json::from_slice(&journal)?;
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("baseline.sqlite");
    std::fs::write(&path, database)?;
    let url = format!("sqlite://{}", path.display());
    let mut original = None;
    // Each pass uses a fresh pool; repeated migration application must preserve
    // ledger timestamps, opaque record bytes and the stopped writer's authority.
    for _ in 0..2 {
        let connection = db::connect(&url).await?;
        let result = check_database(&connection, &expected, &mut original).await;
        let closed = connection.close().await;
        result?;
        closed?;
    }
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
    ensure!(before.ledger.len() == 26 && before.records.len() == 284);
    if let Some(original) = original.as_ref() {
        ensure!(before == *original, "reopening changed the baseline state");
    }
    db::run_migrations(connection).await?;
    let store = DatabaseExecutionStore::new(connection.clone());
    let owner = store.read_owner(OWNER).await.map_err(anyhow::Error::msg)?;
    ensure!(
        owner
            .as_ref()
            .is_some_and(|owner| owner.stopped_at_ms.is_some())
    );
    let index = store
        .read_index(0, None, 16, 1024 * 1024)
        .await
        .map_err(anyhow::Error::msg)?;
    ensure!(index.entries.len() == 1 && index.next_after.is_none());
    let head = &index.entries[0];
    ensure!(head.execution_id == THREAD && head.format_version == 7 && head.version == 284);
    let loaded = store
        .load(THREAD)
        .await
        .map_err(anyhow::Error::msg)?
        .context("baseline execution missing")?;
    ensure!(loaded.format_version == 7 && loaded.version == 284);
    ensure!(
        serde_json::to_value(&loaded.records)? == *expected,
        "candidate decoding changed the baseline journal"
    );
    let mut records = Vec::new();
    let mut after = 0;
    loop {
        let page = store
            .read_records(THREAD, after, Some(284), 16, 4 * 1024 * 1024)
            .await
            .map_err(anyhow::Error::msg)?
            .context("baseline page missing")?;
        ensure!(page.cutoff == 284 && !page.records.is_empty());
        after += u64::try_from(page.records.len())?;
        records.extend(page.records);
        if page.next_after.is_none() {
            break;
        }
        ensure!(page.next_after == Some(after) && after < 284);
    }
    ensure!(after == 284 && serde_json::to_value(&records)? == *expected);
    let mut history = Vec::new();
    let mut after = 0;
    loop {
        let page = store
            .thread_history(THREAD, after, 284, 32, 2 * 1024 * 1024)
            .await
            .map_err(anyhow::Error::msg)?;
        for event in &page.events {
            ensure!(event.seq > after && event.seq <= 284);
            after = event.seq;
        }
        ensure!(!page.events.is_empty());
        history.extend(page.events);
        if !page.more {
            break;
        }
    }
    let expected_events = records
        .iter()
        .filter_map(|record| match record {
            ExecutionRecord::ThreadEvent { event } => Some(event),
            _ => None,
        })
        .collect::<Vec<_>>();
    ensure!(history.len() == 136 && after == 284);
    ensure!(serde_json::to_value(history)? == serde_json::to_value(expected_events)?);
    let mut accepted = 0;
    for record in &records {
        if let ExecutionRecord::AcceptedKey { entry } = record {
            let found = store
                .find_key(&entry.scope, &entry.key)
                .await
                .map_err(anyhow::Error::msg)?;
            ensure!(found.as_ref() == Some(entry));
            accepted += 1;
        }
    }
    ensure!(accepted == 2);
    ensure!(store.read_owner(OWNER).await.map_err(anyhow::Error::msg)? == owner);
    ensure!(
        raw_state(connection).await? == before,
        "migration or reads changed baseline bytes"
    );
    *original = Some(before);
    Ok(())
}
