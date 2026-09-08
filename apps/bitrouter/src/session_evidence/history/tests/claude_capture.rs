use super::*;
use crate::session_evidence::execution::input_runs::InputOutcome;
use crate::session_evidence::types::{SourceDescriptor, SourceFormat};
use std::collections::BTreeMap;
use std::path::{Component, PathBuf};
use tokio::io::AsyncWriteExt;

async fn capture() -> Result<(PathBuf, Value)> {
    let root = PathBuf::from(std::env::var("BITROUTER_TEST_CLAUDE_LIFECYCLE_CAPTURE")?);
    let metadata: Value =
        serde_json::from_slice(&tokio::fs::read(root.join("capture.json")).await?)?;
    ensure!(
        metadata["schema"] == "claude-native-capture/1",
        "capture schema"
    );
    ensure!(
        metadata["version"] == "2.1.220 (Claude Code)",
        "capture producer version"
    );
    Ok((root, metadata))
}

fn text<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    value[key].as_str().context("capture string missing")
}

fn capture_path(root: &Path, relative: &str) -> Result<PathBuf> {
    ensure!(
        Path::new(relative)
            .components()
            .all(|part| matches!(part, Component::Normal(_))),
        "capture path is not relative"
    );
    Ok(root.join(relative))
}

async fn records(store: &EvidenceStore, range: &SourceRange) -> Result<Vec<StoredRecord>> {
    ensure!(
        range.end - range.start <= MAX_RECORDS as u64,
        "capture record limit"
    );
    let mut records = vec![];
    let mut start = range.start;
    while start < range.end {
        let end = (start + RECORD_PAGE_SIZE).min(range.end);
        let page = store
            .records(&SourceRange {
                start,
                end,
                ..range.clone()
            })
            .await?;
        ensure!(page.len() as u64 == end - start, "capture record missing");
        records.extend(page);
        start = end;
    }
    Ok(records)
}

fn context_uuids<'a>(projection: &Projection, records: &'a [StoredRecord]) -> Result<Vec<&'a str>> {
    let by_id: BTreeMap<_, _> = records.iter().map(|record| (&record.id, record)).collect();
    projection
        .effective_context
        .iter()
        .map(|reference| {
            let record = by_id
                .get(&reference.record_id)
                .context("context record missing")?;
            text(&record.input.raw, "uuid")
        })
        .collect()
}

#[tokio::test]
#[ignore = "requires an isolated original Claude Code lifecycle capture"]
async fn captured_claude_compactions_preserve_context_and_cli_fork() -> Result<()> {
    let (capture, metadata) = capture().await?;
    let parent = text(&metadata, "parent")?;
    let fork = text(&metadata, "fork")?;
    let directory = tempfile::tempdir()?;
    let native = directory.path().join("projects");
    tokio::fs::create_dir(&native).await?;
    let parent_path = native.join(format!("{parent}.jsonl"));
    let fork_path = native.join(format!("{fork}.jsonl"));
    let before = tokio::fs::read(capture.join("snapshots/parent-before-fork.jsonl")).await?;
    let frozen_fork =
        tokio::fs::read(capture.join("snapshots/fork-before-parent-continuation.jsonl")).await?;
    tokio::fs::write(&parent_path, &before).await?;
    tokio::fs::write(&fork_path, &frozen_fork).await?;
    let url = format!(
        "sqlite://{}",
        directory.path().join("evidence.db").display()
    );
    let db = crate::db::connect(&url).await?;
    crate::db::run_migrations(&db).await?;
    let make = |db| -> Result<HistoryResolver> {
        let store = EvidenceStore::new(db, "local")?;
        Ok(HistoryResolver::new(
            store.clone(),
            NativeCollector::new(
                store,
                NativeRoot {
                    harness: Harness::ClaudeCode,
                    namespace: text(&metadata, "namespace")?.into(),
                    directory: native.clone(),
                },
            )?,
        ))
    };
    let node = |id: &str| -> Result<NodeKey> {
        Ok(NodeKey {
            namespace: text(&metadata, "namespace")?.into(),
            harness: Harness::ClaudeCode,
            native_id: id.into(),
            agent_id: None,
        })
    };
    let resolver = make(db.clone())?;
    let initial = resolver.resolve(node(parent)?).await?;
    let original_range = initial
        .source
        .as_ref()
        .and_then(|source| source.range.clone())
        .context("original range")?;
    let original_records = records(&resolver.store, &original_range).await?;
    let original_projection = initial.projection.context("initial projection")?;
    assert!(
        original_projection.gaps.is_empty(),
        "{:?}",
        original_projection.gaps
    );
    assert_eq!(original_projection.effective_context.len(), 4);
    let fork_before = resolver
        .resolve(node(fork)?)
        .await?
        .projection
        .context("fork projection")?;
    assert!(fork_before.gaps.is_empty(), "{:?}", fork_before.gaps);
    assert_eq!(fork_before.effective_context.len(), 6);
    // CLI --fork-session copies UUIDs in this producer. This is not the
    // independent SDK forkSession API's UUID-remapping conformance case.
    let parent_final = tokio::fs::read(capture_path(
        &capture,
        text(&metadata["transcripts"], parent)?,
    )?)
    .await?;
    ensure!(
        parent_final.starts_with(&before),
        "native history is not an append"
    );
    let mut file = tokio::fs::OpenOptions::new()
        .append(true)
        .open(&parent_path)
        .await?;
    file.write_all(&parent_final[before.len()..]).await?;
    file.flush().await?;
    drop(file);
    assert_eq!(
        tokio::fs::read(capture_path(
            &capture,
            text(&metadata["transcripts"], fork)?
        )?)
        .await?,
        frozen_fork
    );
    let final_history = resolver.resolve(node(parent)?).await?;
    assert!(final_history.gaps.is_empty(), "{:?}", final_history.gaps);
    let projection = final_history
        .projection
        .as_ref()
        .context("final projection")?;
    assert!(projection.gaps.is_empty(), "{:?}", projection.gaps);
    assert_eq!(
        projection
            .transitions
            .iter()
            .filter(|transition| transition.kind == EdgeKind::Compact)
            .count(),
        2
    );
    assert_eq!(
        projection.producer_versions,
        BTreeSet::from(["2.1.220".into()])
    );
    let final_range = final_history
        .source
        .as_ref()
        .and_then(|source| source.range.as_ref())
        .context("final range")?;
    let all = records(&resolver.store, final_range).await?;
    assert_eq!(
        projection.raw_record_ids.len(),
        std::str::from_utf8(&parent_final)?.lines().count()
    );
    let effective = context_uuids(projection, &all)?;
    assert_eq!(
        effective.len(),
        effective.iter().collect::<BTreeSet<_>>().len()
    );
    let first = context_uuids(&original_projection, &original_records)?;
    assert!(first.iter().all(|uuid| !effective.contains(uuid)));
    let compact = all
        .iter()
        .rev()
        .find(|record| record.input.raw["subtype"] == "compact_boundary")
        .context("last compact")?;
    let preserved = &compact.input.raw["compactMetadata"]["preservedMessages"];
    assert!(effective.contains(&text(preserved, "anchorUuid")?));
    for id in preserved["uuids"]
        .as_array()
        .context("preserved messages")?
    {
        assert!(effective.contains(&id.as_str().context("preserved UUID")?));
    }
    let last_command = metadata["operations"]
        .as_array()
        .context("operations")?
        .last()
        .context("last operation")?;
    assert!(effective.contains(&text(last_command, "command")?));
    assert_eq!(
        resolver.resolve(node(fork)?).await?.projection.as_ref(),
        Some(&fork_before)
    );
    assert_eq!(
        records(&resolver.store, &original_range).await?,
        original_records
    );
    tokio::fs::remove_file(&parent_path).await?;
    tokio::fs::remove_file(&fork_path).await?;
    drop(resolver);
    db.close().await?;
    let reopened = make(crate::db::connect(&url).await?)?;
    let restored = reopened.resolve(node(parent)?).await?;
    assert!(restored.variants.is_empty());
    assert_eq!(
        restored.gaps,
        BTreeSet::from(["native_source_file_unavailable".into()])
    );
    assert_eq!(restored.projection.as_ref(), Some(projection));
    let fork_restored = reopened.resolve(node(fork)?).await?;
    assert!(fork_restored.variants.is_empty());
    assert_eq!(
        fork_restored.gaps,
        BTreeSet::from(["native_source_file_unavailable".into()])
    );
    assert_eq!(fork_restored.projection.as_ref(), Some(&fork_before));
    assert_eq!(
        records(&reopened.store, &original_range).await?,
        original_records
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires an isolated original Claude Code lifecycle capture"]
async fn captured_claude_inputs_keep_native_process_command_and_result_identity() -> Result<()> {
    let (capture, metadata) = capture().await?;
    let directory = tempfile::tempdir()?;
    let db = crate::db::connect("sqlite::memory:").await?;
    crate::db::run_migrations(&db).await?;
    let store = EvidenceStore::new(db, "local")?;
    let operations = metadata["operations"]
        .as_array()
        .context("capture operations")?;
    assert_eq!(operations.len(), 9);
    let targets: BTreeSet<_> = operations
        .iter()
        .map(|operation| text(operation, "command").map(str::to_owned))
        .collect::<Result<_>>()?;
    let mut observed = BTreeSet::new();
    let processes = metadata["processes"]
        .as_array()
        .context("capture processes")?;
    assert_eq!(processes.len(), 4);
    for process in processes {
        let id = text(process, "process_id")?;
        let includes_compact = operations
            .iter()
            .any(|operation| operation["process_id"] == id && operation["kind"] == "compact");
        let path = directory.path().join(format!("cli-{id}.jsonl"));
        tokio::fs::copy(capture_path(&capture, text(process, "spool")?)?, &path).await?;
        let imported = crate::session_evidence::journal::import_spool(
            &store,
            directory.path(),
            &path,
            SourceDescriptor {
                namespace: text(&metadata, "namespace")?.into(),
                harness: Harness::ClaudeCode,
                format: SourceFormat::ClaudeCli,
                locator: format!("spool:{}", path.display()),
                node: None,
            },
        )
        .await?;
        assert!(imported.gaps.is_empty(), "{:?}", imported.gaps);
        let range = imported.range.context("imported spool range")?;
        let all = records(&store, &range).await?;
        let mut scanner = crate::session_evidence::native_inputs::Scanner::new(
            &imported.source.descriptor,
            &targets,
        );
        for record in &all {
            scanner.push(record)?;
        }
        let scanned = scanner.finish();
        assert!(scanned.gaps.is_empty(), "{:?}", scanned.gaps);
        assert!(scanned.ambiguous.is_empty());
        for receipt in scanned.receipts {
            let expected = operations
                .iter()
                .find(|operation| operation["command"] == receipt.native_id)
                .context("original command")?;
            assert_eq!(text(expected, "process_id")?, id);
            assert_eq!(receipt.node.native_id, text(expected, "session")?);
            assert!(observed.insert(receipt.native_id.clone()));
            assert_eq!(
                receipt
                    .acknowledgements
                    .iter()
                    .map(|ack| ack.state.as_str())
                    .collect::<Vec<_>>(),
                ["queued", "started", "completed"]
            );
            assert_eq!(receipt.execution.starts.len(), 1);
            assert_eq!(receipt.execution.terminations.len(), 1);
            if expected["kind"] == "compact" {
                // This native local-command result omits user_message_uuid.
                // Retain the gap instead of borrowing an adjacent result.
                assert!(receipt.execution.results.is_empty());
                assert!(receipt.execution.outcome.is_none());
                assert_eq!(
                    receipt.execution.gaps,
                    BTreeSet::from([
                        "native_execution_identity_missing".into(),
                        "native_execution_result_unobserved".into(),
                    ])
                );
            } else {
                assert_eq!(receipt.execution.results.len(), 1);
                let result = &receipt.execution.results[0].record;
                let original = all
                    .iter()
                    .find(|record| record.id == result.record_id)
                    .context("original result")?;
                assert_eq!(
                    original.input.raw["payload"]["user_message_uuid"],
                    receipt.native_id
                );
                if includes_compact {
                    assert_eq!(
                        receipt.execution.gaps,
                        BTreeSet::from(["native_execution_identity_missing".into()])
                    );
                    assert!(receipt.execution.outcome.is_none());
                } else {
                    assert!(
                        receipt.execution.gaps.is_empty(),
                        "{:?}",
                        receipt.execution.gaps
                    );
                    assert_eq!(receipt.execution.outcome, Some(InputOutcome::Completed));
                }
            }
            assert!(
                receipt
                    .execution
                    .records
                    .iter()
                    .all(|record| record.range.source_id == range.source_id)
            );
        }
    }
    assert_eq!(observed, targets);
    Ok(())
}
