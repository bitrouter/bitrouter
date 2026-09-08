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
async fn file_links_share_evidence_but_identical_replacements_do_not() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let collector = collector(Harness::ClaudeCode, directory.path()).await?;
    let path = directory.path().join("session-1.jsonl");
    let alias = directory.path().join("alias.jsonl");
    let bytes = jsonl(json!({"type":"user","sessionId":"session-1","uuid":"u1"}))?;
    tokio::fs::write(&path, &bytes).await?;
    let opened = File::open(&path).await?;
    let original_identity = file_identity(&opened, &opened.metadata().await?).await?;
    let original = collector
        .reconcile(node(Harness::ClaudeCode), &path, None)
        .await?;
    tokio::fs::hard_link(&path, &alias).await?;
    let linked = collector
        .reconcile(node(Harness::ClaudeCode), &alias, None)
        .await?;
    assert_eq!(original.source.id, linked.source.id);
    assert_eq!(original.source.cursor, linked.source.cursor);

    tokio::fs::remove_file(&path).await?;
    tokio::fs::write(&path, &bytes).await?;
    let replacement = collector
        .reconcile(node(Harness::ClaudeCode), &path, None)
        .await?;
    assert_ne!(original.source.id, replacement.source.id);
    // An in-flight reader still owns the old file, even after its pathname
    // names a different file containing exactly the same transcript bytes.
    assert_eq!(
        file_identity(&opened, &opened.metadata().await?).await?,
        original_identity
    );
    let retained = collector
        .reconcile(node(Harness::ClaudeCode), &alias, None)
        .await?;
    assert_eq!(original.source.id, retained.source.id);
    assert_eq!(original.source.cursor, retained.source.cursor);
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
    let nested = children.join("a1/deeper");
    tokio::fs::create_dir_all(&nested).await?;
    tokio::fs::write(nested.join("agent-nested.jsonl"), "").await?;
    tokio::fs::write(nested.join("session-1.jsonl"), "").await?;
    let unrelated = project.join("other/subagents");
    tokio::fs::create_dir_all(&unrelated).await?;
    tokio::fs::write(unrelated.join("agent-other.jsonl"), "").await?;
    let found = collector.discover("session-1").await?;
    assert_eq!(found.len(), 5);
    assert_eq!(
        found
            .iter()
            .filter(|(node, _)| node.agent_id.is_some())
            .count(),
        4
    );
    assert!(collector.discover("../other").await.is_err());
    Ok(())
}

#[tokio::test]
async fn metadata_reconciliation_resolves_unknown_parent_and_retains_conflicts() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let collector = collector(Harness::ClaudeCode, directory.path()).await?;
    let path = directory
        .path()
        .join("project/session-1/subagents/nested/agent-child.jsonl");
    tokio::fs::create_dir_all(path.parent().context("parent")?).await?;
    let mut child = node(Harness::ClaudeCode);
    child.agent_id = Some("child".into());
    tokio::fs::write(
        &path,
        jsonl(json!({
            "type":"user", "sessionId":"session-1", "agentId":"child", "uuid":"child-u"
        }))?,
    )
    .await?;
    let transcript = collector.reconcile(child.clone(), &path, None).await?;
    assert!(
        collector
            .reconcile_agent_metadata(&transcript)
            .await?
            .is_none()
    );
    let metadata_path = path.with_extension("meta.json");
    tokio::fs::write(&metadata_path, b"{}").await?;
    let unknown = collector
        .reconcile_agent_metadata(&transcript)
        .await?
        .context("metadata")?;
    let roots = BTreeSet::from([child.clone()]);
    assert!(
        collector
            .store
            .execution_graph(&roots)
            .await?
            .gaps
            .contains("native_parent_agent_unknown")
    );

    tokio::fs::write(
        &metadata_path,
        serde_json::to_vec(&json!({"parentAgentId":"parent","toolUseId":"spawn"}))?,
    )
    .await?;
    let known = collector
        .reconcile_agent_metadata(&transcript)
        .await?
        .context("metadata")?;
    let replay = collector
        .reconcile_agent_metadata(&transcript)
        .await?
        .context("replayed metadata")?;
    assert_eq!(known.source, replay.source);
    assert_ne!(
        unknown.source.cursor.generation,
        known.source.cursor.generation
    );
    assert!(
        collector
            .store
            .execution_graph(&roots)
            .await?
            .gaps
            .is_empty()
    );
    assert_eq!(
        collector
            .store
            .records(unknown.range.as_ref().context("unknown range")?)
            .await?[0]
            .input
            .raw,
        json!({})
    );

    tokio::fs::remove_file(&metadata_path).await?;
    assert!(
        collector
            .reconcile_agent_metadata(&transcript)
            .await?
            .is_none()
    );
    assert!(
        collector
            .store
            .execution_graph(&roots)
            .await?
            .gaps
            .is_empty()
    );
    tokio::fs::write(
        &metadata_path,
        serde_json::to_vec(&json!({"parentAgentId":null}))?,
    )
    .await?;
    collector.reconcile_agent_metadata(&transcript).await?;
    assert!(
        collector
            .store
            .execution_graph(&roots)
            .await?
            .gaps
            .contains("conflicting_native_parent")
    );
    Ok(())
}

#[tokio::test]
async fn metadata_requires_owned_transcript_and_matching_file_prefix() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let db = crate::db::connect("sqlite::memory:").await?;
    crate::db::run_migrations(&db).await?;
    let collector = NativeCollector::new(
        EvidenceStore::new(db.clone(), "local")?,
        NativeRoot {
            harness: Harness::ClaudeCode,
            namespace: "profile".into(),
            directory: directory.path().to_owned(),
        },
    )?;
    let path = directory
        .path()
        .join("project/session-1/subagents/agent-child.jsonl");
    tokio::fs::create_dir_all(path.parent().context("parent")?).await?;
    let mut child = node(Harness::ClaudeCode);
    child.agent_id = Some("child".into());
    let raw = jsonl(json!({"type":"user","sessionId":"session-1","agentId":"child","uuid":"u"}))?;
    tokio::fs::write(&path, &raw).await?;
    tokio::fs::write(
        path.with_extension("meta.json"),
        b"{\"parentAgentId\":null}",
    )
    .await?;
    let transcript = collector.reconcile(child.clone(), &path, None).await?;
    collector
        .reconcile_agent_metadata(&transcript)
        .await?
        .context("owned metadata")?;

    let other = NativeCollector::new(EvidenceStore::new(db, "other")?, collector.root.clone())?;
    assert!(other.reconcile_agent_metadata(&transcript).await.is_err());
    let mut forged = transcript.clone();
    forged
        .source
        .descriptor
        .node
        .as_mut()
        .context("node")?
        .agent_id = Some("other".into());
    assert!(collector.reconcile_agent_metadata(&forged).await.is_err());
    let copied = path
        .parent()
        .context("parent")?
        .join("copied/agent-child.jsonl");
    tokio::fs::create_dir_all(copied.parent().context("copy parent")?).await?;
    tokio::fs::write(&copied, &raw).await?;
    tokio::fs::write(
        copied.with_extension("meta.json"),
        b"{\"parentAgentId\":null}",
    )
    .await?;
    forged = transcript.clone();
    forged.path = Some(copied);
    assert!(collector.reconcile_agent_metadata(&forged).await.is_err());
    tokio::fs::write(&path, raw.replace("\"u\"", "\"v\"")).await?;
    assert!(
        collector
            .reconcile_agent_metadata(&transcript)
            .await
            .is_err()
    );
    tokio::fs::write(&path, jsonl(json!({"type":"progress","uuid":"p"}))?).await?;
    let mut unverified = collector.reconcile(child, &path, None).await?;
    assert!(unverified.gaps.contains("native_identity_unverified"));
    unverified.gaps.clear();
    assert!(
        collector
            .reconcile_agent_metadata(&unverified)
            .await
            .is_err()
    );
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
