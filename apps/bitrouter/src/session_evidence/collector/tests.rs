use super::*;
use serde_json::json;
use tokio::io::AsyncWriteExt;

async fn collector(harness: Harness, root: &Path) -> Result<NativeCollector> {
    let db = crate::db::connect("sqlite::memory:").await?;
    crate::db::run_migrations(&db).await?;
    NativeCollector::new(
        EvidenceStore::new(db, "local")?,
        NativeRoot {
            harness,
            namespace: "profile".into(),
            directory: root.to_owned(),
        },
    )
}

fn node(harness: Harness) -> NodeKey {
    NodeKey {
        namespace: "profile".into(),
        harness,
        native_id: "session-1".into(),
        agent_id: None,
    }
}

fn jsonl(value: Value) -> Result<String> {
    Ok(format!("{}\n", serde_json::to_string(&value)?))
}

#[tokio::test]
async fn partial_line_is_retried_and_move_preserves_source_identity() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let collector = collector(Harness::ClaudeCode, directory.path()).await?;
    let path = directory.path().join("session-1.jsonl");
    let first =
        jsonl(json!({"type":"user","uuid":"u1","sessionId":"session-1","version":"2.1.220"}))?;
    tokio::fs::write(&path, format!("{first}{{\"type\":\"assistant\"")).await?;
    let initial = collector
        .reconcile(node(Harness::ClaudeCode), &path, None)
        .await?;
    assert_eq!(initial.source.cursor.offset, first.len() as u64);
    assert_eq!(initial.source.cursor.next_sequence, 1);
    assert!(initial.gaps.contains("partial_native_line"));
    let mut writer = tokio::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .await?;
    writer
        .write_all(b",\"sessionId\":\"session-1\",\"uuid\":\"a1\"}\n")
        .await?;
    writer.flush().await?;
    let complete = collector
        .reconcile(node(Harness::ClaudeCode), &path, None)
        .await?;
    assert_eq!(complete.source.cursor.next_sequence, 2);
    assert!(!complete.gaps.contains("partial_native_line"));
    let renamed = directory.path().join("moved.jsonl");
    tokio::fs::rename(&path, &renamed).await?;
    let moved = collector
        .reconcile(node(Harness::ClaudeCode), &renamed, None)
        .await?;
    assert_eq!(moved.source.id, complete.source.id);
    assert_eq!(moved.source.cursor, complete.source.cursor);
    Ok(())
}

#[tokio::test]
async fn overwritten_prefix_starts_a_new_generation_without_erasing_old_records() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let collector = collector(Harness::ClaudeCode, directory.path()).await?;
    let path = directory.path().join("session-1.jsonl");
    tokio::fs::write(
        &path,
        jsonl(json!({"type":"user","sessionId":"session-1","uuid":"u1"}))?,
    )
    .await?;
    let first = collector
        .reconcile(node(Harness::ClaudeCode), &path, None)
        .await?;
    tokio::fs::write(
        &path,
        jsonl(json!({"type":"user","sessionId":"session-1","uuid":"u2"}))?,
    )
    .await?;
    let replaced = collector
        .reconcile(node(Harness::ClaudeCode), &path, None)
        .await?;
    assert_ne!(
        first.source.cursor.generation,
        replaced.source.cursor.generation
    );
    assert!(replaced.gaps.contains("source_replaced_or_truncated"));
    assert_eq!(
        collector
            .store
            .records(first.range.as_ref().context("first range")?)
            .await?[0]
            .input
            .raw["uuid"],
        "u1"
    );
    Ok(())
}

#[tokio::test]
async fn claude_discovery_includes_all_children_but_no_unrelated_sessions() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let collector = collector(Harness::ClaudeCode, directory.path()).await?;
    let project = directory.path().join("project");
    let children = project.join("session-1/subagents");
    tokio::fs::create_dir_all(&children).await?;
    tokio::fs::write(project.join("session-1.jsonl"), "").await?;
    tokio::fs::write(project.join("other.jsonl"), "").await?;
    for id in ["a1", "a2", "a3"] {
        tokio::fs::write(children.join(format!("agent-{id}.jsonl")), "").await?;
    }
    let found = collector.discover("session-1").await?;
    assert_eq!(found.len(), 4);
    assert_eq!(
        found
            .iter()
            .filter(|(node, _)| node.agent_id.is_some())
            .count(),
        3
    );
    assert!(collector.discover("../other").await.is_err());
    Ok(())
}

#[tokio::test]
async fn codex_parent_checkpoint_excludes_later_parent_execution() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let collector = collector(Harness::Codex, directory.path()).await?;
    let path = directory.path().join("rollout-session-1.jsonl");
    let first = jsonl(
        json!({"ordinal":0,"type":"session_meta","payload":{"id":"session-1","cli_version":"0.148.0"}}),
    )?;
    let inherited = jsonl(
        json!({"ordinal":1,"type":"response_item","payload":{"type":"message","role":"user"}}),
    )?;
    let later = jsonl(
        json!({"ordinal":2,"type":"response_item","payload":{"type":"message","role":"assistant"}}),
    )?;
    tokio::fs::write(&path, format!("{first}{inherited}{later}")).await?;
    let bound = FileBound {
        end_ordinal_exclusive: Some(2),
        end_byte_offset: Some((first.len() + inherited.len()) as u64),
    };
    let prefix = collector
        .reconcile(node(Harness::Codex), &path, Some(bound.clone()))
        .await?;
    assert_eq!(prefix.range.context("prefix range")?.end, 2);
    assert!(prefix.gaps.is_empty());
    let full = collector
        .reconcile(node(Harness::Codex), &path, None)
        .await?;
    assert_eq!(full.range.context("full range")?.end, 3);
    let prefix = collector
        .reconcile(node(Harness::Codex), &path, Some(bound))
        .await?;
    assert_eq!(prefix.range.context("immutable range")?.end, 2);
    Ok(())
}

#[tokio::test]
async fn root_escape_and_mismatched_native_identity_are_rejected() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let outside = tempfile::tempdir()?;
    let collector = collector(Harness::Codex, directory.path()).await?;
    let foreign = outside.path().join("rollout-session-1.jsonl");
    tokio::fs::write(
        &foreign,
        jsonl(json!({"type":"session_meta","payload":{"id":"session-1"}}))?,
    )
    .await?;
    assert!(
        collector
            .reconcile(node(Harness::Codex), &foreign, None)
            .await
            .is_err()
    );
    let mismatched = directory.path().join("rollout-session-1.jsonl");
    tokio::fs::write(
        &mismatched,
        jsonl(json!({"type":"session_meta","payload":{"id":"another-thread"}}))?,
    )
    .await?;
    let result = collector
        .reconcile(node(Harness::Codex), &mismatched, None)
        .await?;
    assert!(result.range.is_none());
    assert!(result.gaps.contains("native_identity_mismatch"));
    Ok(())
}

#[tokio::test]
async fn already_ingested_parent_honors_byte_only_cut_and_detects_missing_ordinals() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let collector = collector(Harness::Codex, directory.path()).await?;
    let path = directory.path().join("rollout-session-1.jsonl");
    let first = jsonl(json!({"ordinal":0,"type":"session_meta","payload":{"id":"session-1"}}))?;
    let later = jsonl(json!({"ordinal":2,"type":"response_item","payload":{}}))?;
    tokio::fs::write(&path, format!("{first}{later}")).await?;
    collector
        .reconcile(node(Harness::Codex), &path, None)
        .await?;
    let prefix = collector
        .reconcile(
            node(Harness::Codex),
            &path,
            Some(FileBound {
                end_ordinal_exclusive: None,
                end_byte_offset: Some(first.len() as u64),
            }),
        )
        .await?;
    assert_eq!(prefix.range.context("prefix")?.end, 1);
    let hole = collector
        .reconcile(
            node(Harness::Codex),
            &path,
            Some(FileBound {
                end_ordinal_exclusive: Some(3),
                end_byte_offset: None,
            }),
        )
        .await?;
    assert!(hole.gaps.contains("parent_ordinal_gap"));
    let missing = collector
        .reconcile(
            node(Harness::Codex),
            &path,
            Some(FileBound {
                end_ordinal_exclusive: None,
                end_byte_offset: Some((first.len() + later.len() + 1) as u64),
            }),
        )
        .await?;
    assert!(missing.gaps.contains("parent_byte_cut_missing"));
    Ok(())
}

#[tokio::test]
async fn filename_without_native_identity_is_not_complete_attribution() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let collector = collector(Harness::ClaudeCode, directory.path()).await?;
    let path = directory.path().join("session-1.jsonl");
    tokio::fs::write(
        &path,
        jsonl(json!({"type":"user","uuid":"u1","version":"2.1.220"}))?,
    )
    .await?;
    let source = collector
        .reconcile(node(Harness::ClaudeCode), &path, None)
        .await?;
    assert!(source.range.is_some());
    assert!(source.gaps.contains("native_identity_unverified"));
    Ok(())
}
