use super::*;
use crate::session_evidence::types::AttemptPhase;
use bitrouter_sdk::acp::controller::tasks::{TaskSelectRequest, TaskSelectionMode};

fn execution(turn: &str, root: &str) -> Vec<Value> {
    vec![
        json!({"type":"event_msg","payload":{"type":"task_started","turn_id":turn}}),
        json!({"type":"turn_context","payload":{"turn_id":turn,"root_turn_id":root}}),
        json!({"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"done"}],"internal_chat_message_metadata_passthrough":{"turn_id":turn}}}),
        json!({"type":"event_msg","payload":{"type":"task_complete","turn_id":turn}}),
    ]
}

async fn native_history(
    service: &ControllerEvidence,
    node: &str,
    child: bool,
    turns: &[(&str, &str)],
) -> Result<PathBuf> {
    let path = service
        .collector
        .root()
        .directory
        .join("sessions")
        .join(format!("rollout-{node}.jsonl"));
    let mut rows =
        vec![json!({"type":"session_meta","payload":{"id":node,"cli_version":"0.153.4"}})];
    if child {
        rows[0]["payload"]["parent_thread_id"] = json!("native");
        rows[0]["payload"]["source"] =
            json!({"subagent":{"thread_spawn":{"parent_thread_id":"native"}}});
        rows[0]["payload"]["subagent_history_start_ordinal"] = json!(2);
        rows.push(json!({"type":"session_meta","payload":{"id":"native","cli_version":"0.153.4"}}));
    }
    for (turn, root) in turns {
        rows.extend(execution(turn, root));
    }
    for (ordinal, row) in rows.iter_mut().enumerate() {
        row["ordinal"] = json!(ordinal);
    }
    write_rows(&path, rows).await?;
    service
        .collector
        .reconcile(
            NodeKey {
                namespace: service.collector.root().namespace.clone(),
                harness: Harness::Codex,
                native_id: node.into(),
                agent_id: None,
            },
            &path,
            None,
        )
        .await?;
    Ok(path)
}

#[tokio::test]
async fn reused_child_membership_is_turn_scoped_and_old_observation_is_immutable() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let handle = fixture(directory.path(), Harness::Codex).await?;
    let service = &handle.service;
    create(service, directory.path()).await?;
    producer(service, "first").await?;
    let root = native_history(
        service,
        "native",
        false,
        &[("first", "first"), ("second", "second")],
    )
    .await?;
    native_history(
        service,
        "worker",
        true,
        &[("child-first", "first"), ("child-second", "second")],
    )
    .await?;
    let mut rows = lifecycle("thread/start", "create", &root);
    rows.extend(turn("first"));
    rows.extend(turn("second"));
    spool(service, rows).await?;
    let first = service.reconcile().await?;
    let attempt = first.attempts.first().context("first attempt")?;
    assert_eq!(attempt.members.len(), 2, "{:?}", first.gaps);
    let id = attempt
        .execution_snapshot
        .as_ref()
        .context("membership pointer")?
        .clone();
    let observed = service.store.attempt_executions(&id).await?;
    assert_eq!(observed.descendants.len(), 1);
    assert_eq!(observed.descendants[0].execution.turn_id, "child-first");
    assert_eq!(observed.inputs.bindings.len(), 1);
    assert_eq!(attempt.phase, AttemptPhase::Settling);
    assert!(attempt.effective_manifest.is_none());
    let original = serde_json::to_value(&observed)?;
    let status = service.store.task_status(&attempt.session).await?;
    service
        .control_task_select(TaskSelectRequest {
            session_id: "acp".into(),
            request_id: "next-task".into(),
            expected: status.current.context("current task")?,
            mode: TaskSelectionMode::NewTask,
        })
        .await?;
    producer(service, "second").await?;
    let second = service.reconcile().await?;
    let current = second
        .attempts
        .iter()
        .find(|candidate| candidate.id != attempt.id)
        .context("second attempt")?;
    let evidence = second
        .attempt_executions
        .get(&current.id)
        .context("second membership")?;
    assert_eq!(evidence.descendants.len(), 1, "{:?}", second.gaps);
    assert_eq!(evidence.descendants[0].execution.turn_id, "child-second");
    assert_eq!(
        serde_json::to_value(service.store.attempt_executions(&id).await?)?,
        original
    );
    drop(handle);
    let reopened = fixture(directory.path(), Harness::Codex).await?;
    assert_eq!(
        serde_json::to_value(reopened.service.store.attempt_executions(&id).await?)?,
        original
    );
    let recovered = reopened.service.reconcile().await?;
    assert!(recovered.attempt_executions.values().any(|membership| {
        membership
            .descendants
            .iter()
            .any(|child| child.execution.turn_id == "child-second")
    }));
    Ok(())
}

#[tokio::test]
async fn archived_attempt_receives_late_children_without_reassigning_them_to_current_task()
-> Result<()> {
    let directory = tempfile::tempdir()?;
    let handle = fixture(directory.path(), Harness::Codex).await?;
    let service = &handle.service;
    create(service, directory.path()).await?;
    producer(service, "first").await?;
    let root = native_history(
        service,
        "native",
        false,
        &[("first", "first"), ("second", "second")],
    )
    .await?;
    let mut rows = lifecycle("thread/start", "create", &root);
    rows.extend(turn("first"));
    rows.extend(turn("second"));
    spool(service, rows).await?;
    let initial = service.reconcile().await?;
    let attempt = &initial.attempts[0];
    let first_id = attempt
        .execution_snapshot
        .as_ref()
        .context("initial snapshot")?;
    assert!(
        initial.attempt_executions[&attempt.id]
            .descendants
            .is_empty()
    );
    let status = service.store.task_status(&attempt.session).await?;
    service
        .control_task_select(TaskSelectRequest {
            session_id: "acp".into(),
            request_id: "new-task".into(),
            expected: status.current.context("current task")?,
            mode: TaskSelectionMode::NewTask,
        })
        .await?;
    producer(service, "second").await?;
    native_history(
        service,
        "worker",
        true,
        &[("late-first", "first"), ("second-child", "second")],
    )
    .await?;
    let reconciled = service.reconcile().await?;
    assert_eq!(
        reconciled.attempt_executions.len(),
        2,
        "{:?}",
        reconciled.gaps
    );
    let archived = &reconciled.attempt_executions[&attempt.id];
    assert_eq!(archived.descendants.len(), 1);
    assert_eq!(archived.descendants[0].execution.turn_id, "late-first");
    let current = reconciled
        .attempt_executions
        .iter()
        .find(|(id, _)| **id != attempt.id)
        .context("current membership")?
        .1;
    assert_eq!(current.descendants.len(), 1);
    assert_eq!(current.descendants[0].execution.turn_id, "second-child");
    assert!(
        service
            .store
            .attempt_executions(first_id)
            .await?
            .descendants
            .is_empty()
    );
    Ok(())
}

#[tokio::test]
async fn an_older_scan_cannot_overwrite_late_child_membership_at_the_same_task_revision()
-> Result<()> {
    let directory = tempfile::tempdir()?;
    let handle = fixture(directory.path(), Harness::Codex).await?;
    let service = &handle.service;
    create(service, directory.path()).await?;
    producer(service, "first").await?;
    let root = native_history(service, "native", false, &[("first", "first")]).await?;
    let mut rows = lifecycle("thread/start", "create", &root);
    rows.extend(turn("first"));
    spool(service, rows).await?;
    let before = service.reconcile().await?;
    let attempt = &before.attempts[0];
    let stamps = service.membership_stamps(&mut BTreeSet::new()).await?;
    let mut older = before.attempt_executions[&attempt.id].clone();
    older.prefixes.clear();
    native_history(service, "worker", true, &[("late", "first")]).await?;
    let latest = service.reconcile().await?;
    assert_eq!(latest.attempts[0].revision, attempt.revision);
    assert_eq!(latest.attempt_executions[&attempt.id].descendants.len(), 1);
    let error = service
        .store
        .record_attempt_executions(
            attempt,
            stamps.get(&attempt.id).and_then(Option::as_ref),
            older,
        )
        .await
        .err()
        .context("stale scan must fail")?;
    assert!(
        error.to_string().contains("execution pointer changed"),
        "{error:#}"
    );
    assert_eq!(
        service
            .store
            .active_attempt(&attempt.session)
            .await?
            .context("original task")?
            .revision,
        attempt.revision
    );
    Ok(())
}

#[tokio::test]
async fn malformed_sibling_spawn_metadata_blocks_descendant_membership() -> Result<()> {
    for invalid in [json!(false), json!("other-parent"), Value::Null] {
        let directory = tempfile::tempdir()?;
        let handle = fixture(directory.path(), Harness::Codex).await?;
        let service = &handle.service;
        create(service, directory.path()).await?;
        producer(service, "first").await?;
        let root = native_history(service, "native", false, &[("first", "first")]).await?;
        let child = native_history(service, "worker", true, &[("child", "first")]).await?;
        let other = child
            .parent()
            .context("child directory")?
            .join("other")
            .join("rollout-worker.jsonl");
        let mut records: Vec<Value> = tokio::fs::read_to_string(&child)
            .await?
            .lines()
            .map(serde_json::from_str)
            .collect::<std::result::Result<_, _>>()?;
        let bad_ordinal = invalid.is_null();
        if bad_ordinal {
            records[2]["ordinal"] = json!(999);
        } else {
            records[0]["payload"]["source"]["subagent"]["thread_spawn"]["parent_thread_id"] =
                invalid;
        }
        write_rows(&other, records).await?;
        service
            .collector
            .reconcile(
                NodeKey {
                    native_id: "worker".into(),
                    namespace: service.collector.root().namespace.clone(),
                    harness: Harness::Codex,
                    agent_id: None,
                },
                &other,
                None,
            )
            .await?;
        let mut rows = lifecycle("thread/start", "create", &root);
        rows.extend(turn("first"));
        spool(service, rows).await?;
        let result = service.reconcile().await?;
        let attempt = &result.attempts[0];
        assert_eq!(attempt.members.len(), 1, "{:?}", result.gaps);
        assert!(
            result.attempt_executions[&attempt.id]
                .descendants
                .is_empty()
        );
        if !bad_ordinal {
            assert!(result.gaps.contains("native_attempt_spawn_ambiguous"));
        }
    }
    Ok(())
}

#[tokio::test]
async fn immutable_membership_rejects_cross_controller_receipts_and_fabricated_own_runs()
-> Result<()> {
    for foreign_thread in [false, true] {
        let directory = tempfile::tempdir()?;
        let handle = fixture(directory.path(), Harness::Codex).await?;
        let service = &handle.service;
        create(service, directory.path()).await?;
        producer(service, "turn").await?;
        let root = native_history(service, "native", false, &[("turn", "turn")]).await?;
        let mut rows = lifecycle("thread/start", "create", &root);
        rows.extend(turn("turn"));
        spool(service, rows).await?;
        let before = service.reconcile().await?;
        let attempt = &before.attempts[0];
        let stamp = service.store.execution_pointer_stamp(&attempt.id).await?;
        let mut forged = before.attempt_executions[&attempt.id].clone();
        forged.prefixes.clear();
        let selected = forged.inputs.bindings[0]
            .codex_history
            .as_mut()
            .context("selected history")?;
        selected.execution.as_mut().context("own run")?.root_turn_id = Some("fabricated".into());
        let error = service
            .store
            .record_attempt_executions(attempt, stamp.as_ref(), forged)
            .await
            .err()
            .context("fabricated run must fail")?;
        assert!(
            error
                .to_string()
                .contains("input own rollout execution changed"),
            "{error:#}"
        );

        let foreign_handle = fixture(directory.path(), Harness::Codex).await?;
        let foreign = codex_source(
            &foreign_handle.service,
            &foreign_handle.service.spool,
            if foreign_thread {
                "foreign-thread"
            } else {
                "native"
            },
        )
        .await?;
        let source = service
            .store
            .source(&foreign)
            .await?
            .context("foreign source")?;
        let mut references = Vec::new();
        for sequence in 0..3 {
            let rows = service
                .store
                .records(&SourceRange {
                    source_id: foreign.clone(),
                    generation: "spool/1".into(),
                    start: sequence,
                    end: sequence + 1,
                })
                .await?;
            references.push(RecordRef::from_record(&rows[0])?);
        }
        let mut forged = before.attempt_executions[&attempt.id].clone();
        forged.prefixes.clear();
        forged.inputs.inspected.push(SourceRange {
            source_id: foreign,
            generation: "spool/1".into(),
            start: 0,
            end: 3,
        });
        let input = &mut forged.inputs.bindings[0];
        if foreign_thread {
            input.node.native_id = "foreign-thread".into();
        }
        input.process_header = references[0].clone();
        input.input = references[1].clone();
        input.acknowledgements[0].record = references[2].clone();
        input.execution = Default::default();
        input.codex_history = None;
        input.process_id = Path::new(
            source
                .descriptor
                .locator
                .strip_prefix("spool:")
                .context("path")?,
        )
        .file_stem()
        .and_then(|name| name.to_str())
        .context("process id")?
        .into();
        let error = service
            .store
            .record_attempt_executions(attempt, stamp.as_ref(), forged)
            .await
            .err()
            .context("foreign receipt must fail")?;
        assert!(
            error.to_string().contains(if foreign_thread {
                "producer/native identity mismatch"
            } else {
                "another controller or profile"
            }),
            "{error:#}"
        );
        assert!(
            service
                .store
                .task_status(&attempt.session)
                .await?
                .current
                .is_some()
        );
    }
    Ok(())
}

#[tokio::test]
async fn unrelated_pointer_inventory_and_saturated_counters_cannot_block_raw_tasks() -> Result<()> {
    use sea_orm::{ConnectionTrait, DbBackend, Statement};
    let directory = tempfile::tempdir()?;
    let handle = fixture(directory.path(), Harness::Codex).await?;
    let service = &handle.service;
    create(service, directory.path()).await?;
    producer(service, "first").await?;
    let root = native_history(service, "native", false, &[("first", "first")]).await?;
    let mut rows = lifecycle("thread/start", "create", &root);
    rows.extend(turn("first"));
    spool(service, rows).await?;
    let before = service.reconcile().await?;
    let attempt = &before.attempts[0];
    let db = crate::db::connect(&format!(
        "sqlite:{}",
        directory.path().join("router/evidence.db").display()
    ))
    .await?;
    db.execute(Statement::from_sql_and_values(DbBackend::Sqlite,
        "WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM n WHERE x < 1025) INSERT INTO native_evidence_objects (id, owner, kind, object_key, revision, digest, object_json) SELECT 'unrelated-pointer-' || x, (SELECT owner FROM native_evidence_objects WHERE kind = 'attempt_execution_pointer' AND object_key = ?), 'attempt_execution_pointer', 'unrelated-' || x, 0, 'invalid', '{}' FROM n", [attempt.id.clone().into()])).await?;
    let healthy = service.reconcile().await?;
    assert_eq!(healthy.attempt_executions[&attempt.id].members().len(), 1);
    db.execute(Statement::from_sql_and_values(DbBackend::Sqlite, "UPDATE native_evidence_objects SET revision = ? WHERE kind = 'attempt_execution_pointer' AND object_key = ?", [i64::MAX.into(), attempt.id.clone().into()])).await?;
    let uncertain = service.reconcile().await?;
    assert!(uncertain.gaps.contains("native_attempt_membership_invalid"));
    assert!(!uncertain.attempt_executions.contains_key(&attempt.id));
    assert!(
        service
            .store
            .task_status(&attempt.session)
            .await?
            .current
            .is_some()
    );
    producer(service, "second").await?;
    assert!(
        service
            .store
            .task_status(&attempt.session)
            .await?
            .current
            .is_some()
    );
    Ok(())
}

#[tokio::test]
async fn real_attempt_backlog_preserves_current_targets_and_rotates_archives() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let handle = fixture(directory.path(), Harness::Codex).await?;
    let service = &handle.service;
    let mut all = BTreeSet::new();
    let mut current = BTreeSet::new();
    for name in ["first", "second"] {
        let session = crate::session_evidence::types::AcpSessionKey {
            namespace: service.collector.root().namespace.clone(),
            harness: Harness::Codex,
            session_id: name.into(),
        };
        service
            .store
            .seed_attempt_chain(session.clone(), name, 513)
            .await?;
        current.insert(
            service
                .store
                .active_attempt(&session)
                .await?
                .context("current attempt")?
                .id,
        );
        all.extend(
            service
                .store
                .unsettled_attempts(&session)
                .await?
                .into_iter()
                .map(|attempt| attempt.id),
        );
    }
    assert_eq!(all.len(), MAX_GRAPH_ITEMS + 2);
    let mut seen = BTreeSet::new();
    for _ in 0..2 {
        let mut gaps = BTreeSet::new();
        let page = service.membership_stamps(&mut gaps).await?;
        assert_eq!(page.len(), MAX_GRAPH_ITEMS);
        assert!(current.iter().all(|id| page.contains_key(id)));
        assert!(gaps.contains("native_attempt_membership_inventory_pending"));
        seen.extend(page.into_keys());
    }
    assert_eq!(seen, all);
    Ok(())
}

#[tokio::test]
async fn immutable_membership_rechecks_competing_producers_and_connections() -> Result<()> {
    for case in ["producer", "connection", "ambiguous_connection"] {
        let competing_producer = case == "producer";
        let directory = tempfile::tempdir()?;
        let handle = fixture(directory.path(), Harness::Codex).await?;
        let service = &handle.service;
        create(service, directory.path()).await?;
        producer(service, "turn").await?;
        let root = native_history(service, "native", false, &[("turn", "turn")]).await?;
        let mut rows = lifecycle("thread/start", "create", &root);
        rows.extend(turn("turn"));
        spool(service, rows).await?;
        let before = service.reconcile().await?;
        let original = &before.attempts[0];
        let mut forged = before.attempt_executions[&original.id].clone();
        if competing_producer {
            prompt(
                service,
                "competing",
                Event::CodexAccepted {
                    thread_id: "native".into(),
                    turn_id: "turn".into(),
                    role: "prompt".into(),
                },
            )
            .await?;
        } else if case == "connection" {
            codex_source(service, &service.spool, "native").await?;
        } else {
            let mut duplicate = Vec::new();
            for request in ["duplicate-first", "duplicate-second"] {
                let mut rows = turn("turn");
                for row in &mut rows {
                    row["operation_id"] = json!(request);
                }
                duplicate.extend(rows);
            }
            spool(service, duplicate).await?;
        }
        let attempt = service
            .store
            .active_attempt(&original.session)
            .await?
            .context("current attempt")?;
        let prompts = service
            .store
            .prompt_bridge_evidence(&attempt.session, &attempt.id)
            .await?;
        let fresh = service
            .native_inputs(
                &BTreeMap::from([(attempt.id.clone(), prompts)]),
                &BTreeSet::new(),
            )
            .await?;
        assert!(fresh[&attempt.id].bindings.is_empty(), "{case}");
        forged.attempt_revision = attempt.revision;
        forged.prefixes.clear();
        forged.inputs.inspected = fresh[&attempt.id].inspected.clone();
        forged.inputs.gaps = fresh[&attempt.id].gaps.clone();
        let stamp = service.store.execution_pointer_stamp(&attempt.id).await?;
        let error = service
            .store
            .record_attempt_executions(&attempt, stamp.as_ref(), forged)
            .await
            .err()
            .context("ambiguous selection must fail")?;
        assert!(
            error.to_string().contains(if competing_producer {
                "input producer unavailable or ambiguous"
            } else {
                "input accepted by competing requests"
            }),
            "{error:#}"
        );
    }
    Ok(())
}

#[tokio::test]
async fn damaged_sibling_identity_withholds_descendant_membership() -> Result<()> {
    use sea_orm::{ConnectionTrait, DbBackend, Statement};
    let directory = tempfile::tempdir()?;
    let handle = fixture(directory.path(), Harness::Codex).await?;
    let service = &handle.service;
    create(service, directory.path()).await?;
    producer(service, "turn").await?;
    let root = native_history(service, "native", false, &[("turn", "turn")]).await?;
    let child = native_history(service, "worker", true, &[("child", "turn")]).await?;
    let mut rows = lifecycle("thread/start", "create", &root);
    rows.extend(turn("turn"));
    spool(service, rows).await?;
    let before = service.reconcile().await?;
    let attempt = &before.attempts[0];
    assert_eq!(before.attempt_executions[&attempt.id].descendants.len(), 1);
    let sibling = child.with_file_name("rollout-worker_revision-1.jsonl");
    tokio::fs::copy(&child, &sibling).await?;
    let node = before.attempt_executions[&attempt.id].descendants[0]
        .node
        .clone();
    let collected = service.collector.reconcile(node, &sibling, None).await?;
    let db = crate::db::connect(&format!(
        "sqlite:{}",
        directory.path().join("router/evidence.db").display()
    ))
    .await?;
    db.execute(Statement::from_sql_and_values(DbBackend::Sqlite,
        "UPDATE native_evidence_objects SET object_json = '{}' WHERE kind = 'rollout_identity' AND object_key = ?",
        [collected.source.id.into()])).await?;
    let after = service.reconcile().await?;
    assert!(
        after.attempt_executions[&attempt.id].descendants.is_empty(),
        "{:?}",
        after.gaps
    );
    assert!(
        after.attempt_executions[&attempt.id]
            .gaps
            .contains("native_attempt_spawn_ambiguous")
    );
    Ok(())
}

#[tokio::test]
async fn damaged_derived_objects_do_not_block_task_controls_and_rebuild_from_raw_records()
-> Result<()> {
    use sea_orm::{ConnectionTrait, DbBackend, Statement};
    for damage in ["snapshot_missing", "snapshot_corrupt", "pointer_corrupt"] {
        let directory = tempfile::tempdir()?;
        let handle = fixture(directory.path(), Harness::Codex).await?;
        let service = &handle.service;
        create(service, directory.path()).await?;
        producer(service, "first").await?;
        let root = native_history(service, "native", false, &[("first", "first")]).await?;
        let mut rows = lifecycle("thread/start", "create", &root);
        rows.extend(turn("first"));
        spool(service, rows).await?;
        let before = service.reconcile().await?;
        let attempt = &before.attempts[0];
        let snapshot = attempt.execution_snapshot.as_ref().context("snapshot")?;
        let db = crate::db::connect(&format!(
            "sqlite:{}",
            directory.path().join("router/evidence.db").display()
        ))
        .await?;
        let (sql, kind, key) = match damage {
            "snapshot_missing" => (
                "DELETE FROM native_evidence_objects WHERE kind = ? AND object_key = ?",
                "attempt_executions",
                snapshot,
            ),
            "snapshot_corrupt" => (
                "UPDATE native_evidence_objects SET object_json = '{}' WHERE kind = ? AND object_key = ?",
                "attempt_executions",
                snapshot,
            ),
            _ => (
                "UPDATE native_evidence_objects SET object_json = '{}' WHERE kind = ? AND object_key = ?",
                "attempt_execution_pointer",
                &attempt.id,
            ),
        };
        db.execute(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            sql,
            [kind.into(), key.clone().into()],
        ))
        .await?;
        assert!(
            service
                .store
                .task_status(&attempt.session)
                .await?
                .current
                .is_some()
        );
        let recovered = service.reconcile().await?;
        let result = &recovered.attempts[0];
        assert_eq!(result.members.len(), 1, "{damage}: {:?}", recovered.gaps);
        service
            .store
            .attempt_executions(
                result
                    .execution_snapshot
                    .as_ref()
                    .context("rebuilt snapshot")?,
            )
            .await?;
        producer(service, "second").await?;
        assert!(
            service
                .store
                .task_status(&attempt.session)
                .await?
                .current
                .is_some()
        );
    }
    Ok(())
}
