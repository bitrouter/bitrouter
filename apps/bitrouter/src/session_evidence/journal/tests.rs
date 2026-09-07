use super::super::types::{Harness, SourceFormat};
use super::*;
use anyhow::Context;
use serde_json::json;
use tokio::io::AsyncWriteExt;

async fn store() -> Result<EvidenceStore> {
    let db = crate::db::connect("sqlite::memory:").await?;
    crate::db::run_migrations(&db).await?;
    EvidenceStore::new(db, "local")
}

fn descriptor(format: SourceFormat) -> SourceDescriptor {
    SourceDescriptor {
        namespace: "fixture".into(),
        harness: Harness::Codex,
        format,
        locator: "controller:one".into(),
        node: None,
    }
}

#[tokio::test]
async fn concurrent_controller_observations_are_durable_and_ordered() -> Result<()> {
    let store = store().await?;
    let journal = Journal::new(
        store.clone(),
        descriptor(SourceFormat::Acp),
        "adapter/1".into(),
    )
    .await?;
    let (left, right) = tokio::join!(
        journal.append_record(json!({"method":"session/update","block":1})),
        journal.append_record(json!({"method":"session/update","block":2}))
    );
    let returned = [left?, right?];
    let source = journal.source.lock().await.clone();
    let records = store
        .records(&SourceRange {
            source_id: source.id,
            generation: source.cursor.generation,
            start: 0,
            end: 2,
        })
        .await?;
    assert_eq!(records.len(), 2);
    for record in returned {
        assert!(
            records.contains(&record),
            "append must return the committed body and position"
        );
        super::super::types::RecordRef::from_record(&record)?.validate()?;
    }
    assert_ne!(records[0].id, records[1].id);
    assert_eq!(records[0].input.sequence, 0);
    assert_eq!(records[1].input.sequence, 1);
    let reopened = Journal::new(store, descriptor(SourceFormat::Acp), "adapter/1".into()).await?;
    reopened
        .append(json!({"method":"controller/disconnect"}))
        .await?;
    assert_eq!(reopened.source.lock().await.cursor.next_sequence, 3);
    Ok(())
}

#[tokio::test]
async fn spool_import_recovers_partial_writes_without_duplicate_events() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("native.jsonl");
    let store = store().await?;
    let descriptor = descriptor(SourceFormat::CodexAppServer);
    tokio::fs::write(
        &path,
        b"{\"method\":\"runtime/started\",\"version\":\"fixture/1\"}\n{\"method\":",
    )
    .await?;
    let first = import_spool(&store, directory.path(), &path, descriptor.clone()).await?;
    assert_eq!(first.range.context("first range")?.end, 1);
    assert!(first.gaps.contains("native_spool_partial_line"));
    let mut file = tokio::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .await?;
    file.write_all(b"\"thread/started\"}\n").await?;
    file.flush().await?;
    let next = import_spool(&store, directory.path(), &path, descriptor.clone()).await?;
    let range = next.range.context("new range")?;
    assert_eq!((range.start, range.end), (1, 2));
    assert!(next.gaps.is_empty());
    let replay = import_spool(&store, directory.path(), &path, descriptor.clone()).await?;
    assert!(replay.range.is_none());
    tokio::fs::write(&path, b"{\"method\":\"different\"}\n").await?;
    let replaced = import_spool(&store, directory.path(), &path, descriptor).await?;
    assert!(replaced.gaps.contains("native_spool_replaced"));
    assert_eq!(replaced.source.cursor, replay.source.cursor);
    assert_eq!(
        store.records(&range).await?[0]
            .input
            .producer_version
            .as_deref(),
        Some("fixture/1")
    );
    Ok(())
}

#[tokio::test]
async fn spool_backlog_advances_in_bounded_passes_and_eventually_drains() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("native.jsonl");
    let store = store().await?;
    let descriptor = descriptor(SourceFormat::CodexAppServer);
    tokio::fs::write(&path, "{\"method\":\"item/completed\"}\n".repeat(260)).await?;
    for expected in [128, 256, 260] {
        let imported = import_spool(&store, directory.path(), &path, descriptor.clone()).await?;
        assert_eq!(imported.source.cursor.next_sequence, expected);
        assert_eq!(
            imported.gaps.contains("native_spool_backlog"),
            expected != 260
        );
    }
    Ok(())
}
