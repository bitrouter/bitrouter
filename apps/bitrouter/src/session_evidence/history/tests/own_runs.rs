use super::*;
use crate::session_evidence::execution::runs::RunOutcome;

#[tokio::test]
async fn copied_metadata_preserves_context_without_reassigning_parent_execution() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let url = format!(
        "sqlite://{}",
        directory.path().join("evidence.db").display()
    );
    let db = crate::db::connect(&url).await?;
    crate::db::run_migrations(&db).await?;
    let make_resolver = |db| -> Result<HistoryResolver> {
        let store = EvidenceStore::new(db, "local")?;
        Ok(HistoryResolver::new(
            store.clone(),
            NativeCollector::new(
                store,
                NativeRoot {
                    harness: Harness::Codex,
                    namespace: "fixture".into(),
                    directory: directory.path().into(),
                },
            )?,
        ))
    };
    let resolver = make_resolver(db.clone())?;
    let contents = row(
        0,
        "session_meta",
        json!({"id":"child","cli_version":"0.153.4","forked_from_id":"parent","parent_thread_id":"parent","subagent_history_start_ordinal":5}),
    )? + &row(
        1,
        "session_meta",
        json!({"id":"parent","cli_version":"0.153.4"}),
    )? + &row(
        2,
        "event_msg",
        json!({"type":"task_started","turn_id":"parent-turn"}),
    )? + &row(
        3,
        "response_item",
        json!({"type":"message","role":"user","content":"inherited"}),
    )? + &row(
        4,
        "turn_context",
        json!({"turn_id":"parent-turn","root_turn_id":"parent-turn"}),
    )? + &row(
        5,
        "event_msg",
        json!({"type":"task_started","turn_id":"child-turn"}),
    )? + &row(
        6,
        "turn_context",
        json!({"turn_id":"child-turn","root_turn_id":"parent-turn"}),
    )? + &row(
        7,
        "response_item",
        json!({"type":"message","role":"assistant","content":"child result","internal_chat_message_metadata_passthrough":{"turn_id":"child-turn"}}),
    )? + &row(
        8,
        "event_msg",
        json!({"type":"task_complete","turn_id":"child-turn"}),
    )?;
    let path = directory.path().join("rollout-child.jsonl");
    tokio::fs::write(&path, contents).await?;
    let mut remaining = 1;
    let prefix = resolver
        .collector
        .reconcile_limited(node("child"), &path, None, &mut remaining)
        .await?;
    assert_eq!(prefix.source.cursor.next_sequence, 1);
    let history = resolver.resolve(node("child")).await?;
    let projection = history.projection.as_ref().context("child projection")?;
    assert_eq!(projection.node.native_id, "child");
    assert_eq!(projection.effective_context.len(), 2);
    let execution = history
        .codex_executions
        .as_ref()
        .context("own executions")?;
    assert!(execution.gaps.is_empty());
    assert_eq!(execution.runs.len(), 1);
    assert_eq!(execution.runs[0].turn_id, "child-turn");
    assert_eq!(
        execution.runs[0].root_turn_id.as_deref(),
        Some("parent-turn")
    );
    assert_eq!(execution.runs[0].outcome, Some(RunOutcome::Completed));
    let graph = resolver
        .store
        .execution_graph(&BTreeSet::from([node("child")]))
        .await?;
    assert!(!graph.gaps.contains("native_lifecycle_invalid"));
    assert_eq!(graph.inherited_records.len(), 2);
    assert!(graph.facts.iter().all(|fact|!matches!(&fact.event,
        crate::session_evidence::execution::FactKind::RunStarted {run_id} if run_id=="parent-turn")));
    let original_facts = resolver.store.node_facts(&node("child"), None, 128).await?;
    assert!(
        original_facts
            .iter()
            .any(|fact| graph.inherited_records.contains_key(&fact.record_id))
    );
    let expected = serde_json::to_value(execution)?;
    tokio::fs::remove_file(&path).await?;
    drop(resolver);
    db.close().await?;
    let reopened = make_resolver(crate::db::connect(&url).await?)?;
    let restored = reopened.resolve(node("child")).await?;
    assert_eq!(serde_json::to_value(restored.codex_executions)?, expected);
    assert_eq!(restored.projection, history.projection);
    let restored_graph = reopened
        .store
        .execution_graph(&BTreeSet::from([node("child")]))
        .await?;
    assert_eq!(restored_graph.inherited_records, graph.inherited_records);
    assert!(restored.gaps.contains("native_source_file_unavailable"));
    Ok(())
}

#[tokio::test]
#[ignore = "requires an isolated original Codex subagent capture"]
async fn captured_codex_subagent_followups_keep_distinct_own_turns() -> Result<()> {
    // The capture is produced by an isolated native CLI through BitRouter's
    // proxy with a deterministic local Responses provider, never model mocks
    // standing in for native thread creation or subagent execution.
    let capture = std::path::PathBuf::from(std::env::var("BITROUTER_TEST_CODEX_SUBAGENT_CAPTURE")?);
    let directory = tempfile::tempdir()?;
    let mut pending = vec![capture.join("profile/sessions")];
    let mut child = None;
    let mut parent = None;
    let mut count = 0;
    while let Some(path) = pending.pop() {
        ensure!(pending.len() < MAX_GRAPH_ITEMS, "capture directory limit");
        let mut entries = tokio::fs::read_dir(path).await?;
        while let Some(entry) = entries.next_entry().await? {
            if entry.file_type().await?.is_dir() {
                pending.push(entry.path());
                continue;
            }
            if entry
                .path()
                .extension()
                .is_none_or(|extension| extension != "jsonl")
            {
                continue;
            }
            count += 1;
            ensure!(count <= 32, "capture source limit");
            ensure!(
                entry.metadata().await?.len() <= MAX_OBJECT_BYTES as u64,
                "capture file limit"
            );
            let text = tokio::fs::read_to_string(entry.path()).await?;
            let first: Value = serde_json::from_str(text.lines().next().context("metadata")?)?;
            let metadata = &first["payload"];
            if let Some(parent_id) = metadata["parent_thread_id"].as_str() {
                ensure!(child.is_none(), "capture needs exactly one subagent");
                ensure!(
                    metadata["subagent_history_start_ordinal"]
                        .as_u64()
                        .is_some_and(|cut| cut > 0),
                    "capture lacks copied history cut"
                );
                parent = Some(parent_id.to_owned());
                child = Some(metadata["id"].as_str().context("child id")?.to_owned());
            }
            tokio::fs::write(directory.path().join(entry.file_name()), text).await?;
        }
    }
    let resolver = resolver(directory.path()).await?;
    let child = child.context("native child")?;
    let child_history = resolver.resolve(node(&child)).await?;
    assert!(child_history.projection.is_some());
    let executions = child_history
        .codex_executions
        .as_ref()
        .context("child execution")?;
    assert!(executions.gaps.is_empty(), "{:?}", executions.gaps);
    assert_eq!(executions.runs.len(), 2);
    let parent_history = resolver.resolve(node(&parent.context("parent")?)).await?;
    let parent_turns: BTreeSet<_> = parent_history
        .codex_executions
        .as_ref()
        .context("parent execution")?
        .runs
        .iter()
        .map(|run| run.turn_id.clone())
        .collect();
    let roots: BTreeSet<_> = executions
        .runs
        .iter()
        .filter_map(|run| run.root_turn_id.clone())
        .collect();
    assert_eq!(roots.len(), 2);
    assert_eq!(roots, parent_turns);
    let mut previous_end = 0;
    for run in &executions.runs {
        assert_eq!(run.outcome, Some(RunOutcome::Completed), "{:?}", run.gaps);
        let span = run.observed_span.as_ref().context("own bookends")?;
        assert!(span.start >= previous_end);
        previous_end = span.end;
        assert!(!run.contexts.is_empty());
        for reference in &run.records {
            let records = resolver.store.records(&reference.range).await?;
            assert_eq!(
                crate::session_evidence::types::RecordRef::from_record(&records[0])?,
                *reference
            );
        }
        // Replay the original source cut immediately before this turn's end.
        // This proves the retained open prefix, not live-process liveness.
        let terminal = run.terminations.first().context("native terminal")?;
        let inspected = executions.inspected.as_ref().context("native prefix")?;
        let mut scanner =
            crate::session_evidence::execution::rollout_runs::Scanner::new(node(&child));
        let mut budget = MAX_OBJECT_BYTES;
        let mut start = 0;
        while start < terminal.record.range.start {
            let end = (start + RECORD_PAGE_SIZE).min(terminal.record.range.start);
            for record in resolver
                .store
                .records(&SourceRange {
                    start,
                    end,
                    ..inspected.clone()
                })
                .await?
            {
                scanner.push(&record, &mut budget);
            }
            start = end;
        }
        let prefix = scanner.finish();
        let open = prefix
            .runs
            .iter()
            .find(|candidate| candidate.turn_id == run.turn_id)
            .context("native unfinished turn")?;
        assert_eq!(
            open.execution_state(),
            Some(
                crate::session_evidence::execution::rollout_runs::ExecutionState::AwaitingTerminal
            )
        );
        assert_eq!(open.root_turn_id, run.root_turn_id);
        assert!(open.outcome.is_none() && open.observed_span.is_none());
    }
    Ok(())
}
