use super::*;

async fn rollout(
    service: &ControllerEvidence,
    directory: &str,
    id: &str,
    turn: &str,
) -> Result<PathBuf> {
    let path = service
        .collector
        .root()
        .directory
        .join(directory)
        .join(if id == "native" {
            "rollout-native.jsonl".into()
        } else {
            format!("rollout-native_{id}.jsonl")
        });
    write_rows(&path, vec![
        json!({"ordinal":0,"type":"session_meta","payload":{"id":"native","cli_version":"0.153.4"}}),
        json!({"ordinal":1,"type":"turn_context","payload":{"turn_id":turn}}),
    ]).await?;
    service
        .collector
        .reconcile(
            NodeKey {
                namespace: service.collector.root().namespace.clone(),
                harness: Harness::Codex,
                native_id: "native".into(),
                agent_id: None,
            },
            &path,
            None,
        )
        .await?;
    Ok(path)
}

fn lifecycle(method: &str, id: &str, path: &Path) -> Vec<Value> {
    vec![
        json!({"method":method,"direction":"client","phase":"request","operation_id":id,"payload":{"threadId":"native"}}),
        json!({"method":method,"direction":"server","phase":"response","operation_id":id,"payload":{"thread":{"id":"native","path":path,"ephemeral":false}}}),
    ]
}

fn turn(id: &str) -> Vec<Value> {
    vec![
        json!({"method":"turn/start","direction":"client","phase":"request","operation_id":id,"payload":{"threadId":"native","input":[]}}),
        json!({"method":"turn/start","direction":"server","phase":"response","operation_id":id,"payload":{"turn":{"id":id,"status":"inProgress"}}}),
    ]
}

async fn spool(service: &ControllerEvidence, rows: Vec<Value>) -> Result<PathBuf> {
    let path = service
        .spool
        .join(format!("{}.jsonl", uuid::Uuid::new_v4()));
    let mut complete =
        vec![json!({"method":"runtime/started","phase":"metadata","version":"codex-cli 0.153.4"})];
    complete.extend(rows);
    for (sequence, row) in complete.iter_mut().enumerate() {
        row["sequence"] = json!(sequence);
    }
    write_rows(&path, complete).await?;
    import_spool(
        &service.store,
        &service.spool,
        &path,
        SourceDescriptor {
            namespace: service.collector.root().namespace.clone(),
            harness: Harness::Codex,
            format: SourceFormat::CodexAppServer,
            locator: format!("spool:{}", path.display()),
            node: None,
        },
    )
    .await?;
    Ok(path)
}

async fn producer(service: &ControllerEvidence, turn: &str) -> Result<PromptEvidence> {
    prompt(
        service,
        turn,
        Event::CodexAccepted {
            thread_id: "native".into(),
            turn_id: turn.into(),
            role: "prompt".into(),
        },
    )
    .await
}

#[tokio::test]
async fn inputs_bind_distinct_rollouts_and_keep_original_provenance_after_file_loss_and_reopen()
-> Result<()> {
    let directory = tempfile::tempdir()?;
    let handle = fixture(directory.path(), Harness::Codex).await?;
    let service = &handle.service;
    create(service, directory.path()).await?;
    producer(service, "before").await?;
    let evidence = producer(service, "after").await?;
    let original = rollout(service, "original", "native", "before").await?;
    let reverted = rollout(service, "reverted", "replacement", "after").await?;
    let mut rows = lifecycle("thread/start", "create", &original);
    rows.extend(turn("before"));
    rows.extend(lifecycle("thread/revert", "revert", &reverted));
    rows.push(json!({"method":"thread/reverted","direction":"server","phase":"notification","payload":{"threadId":"native"}}));
    rows.extend(turn("after"));
    let spool = spool(service, rows).await?;
    let selected = BTreeMap::from([("attempt".into(), evidence)]);
    let result = service.native_inputs(&selected, &BTreeSet::new()).await?;
    assert_eq!(result["attempt"].bindings.len(), 2);
    let mut context_to_remove = None;
    for binding in &result["attempt"].bindings {
        let history = binding.codex_history.as_ref().context("history")?;
        assert!(history.gaps.is_empty(), "{:?}", history.gaps);
        let source = history.source.as_ref().context("source binding")?;
        assert_eq!(source.node, binding.node);
        assert_eq!(
            source.rollout_id,
            if binding.native_id == "before" {
                "native"
            } else {
                "replacement"
            }
        );
        assert_eq!(history.turn_contexts.len(), 1);
        let context = &history.turn_contexts[0];
        assert_eq!(context.range.source_id, source.id);
        let original_record = service.store.records(&context.range).await?;
        assert_eq!(RecordRef::from_record(&original_record[0])?, *context);
        assert_eq!(
            original_record[0].input.raw["payload"]["turn_id"],
            binding.native_id
        );
        if binding.native_id == "before" {
            context_to_remove = Some(context.record_id.clone());
        }
    }
    tokio::fs::remove_file(original).await?;
    tokio::fs::remove_file(reverted).await?;
    tokio::fs::remove_file(spool).await?;
    drop(handle);
    let reopened = fixture(directory.path(), Harness::Codex).await?;
    let restored = reopened
        .service
        .native_inputs(&selected, &BTreeSet::new())
        .await?;
    assert_eq!(
        serde_json::to_value(&restored)?,
        serde_json::to_value(&result)?
    );
    use sea_orm::{ConnectionTrait, DbBackend, Statement};
    let db = crate::db::connect(&format!(
        "sqlite:{}",
        directory.path().join("router/evidence.db").display()
    ))
    .await?;
    db.execute(Statement::from_sql_and_values(
        DbBackend::Sqlite,
        "DELETE FROM native_evidence_records WHERE id = ?",
        [context_to_remove.context("old context")?.into()],
    ))
    .await?;
    let damaged = reopened
        .service
        .native_inputs(&selected, &BTreeSet::new())
        .await?;
    assert_eq!(damaged["attempt"].bindings.len(), 2);
    for binding in &damaged["attempt"].bindings {
        let history = binding.codex_history.as_ref().context("history")?;
        if binding.native_id == "before" {
            assert!(history.source.is_none());
            assert!(
                history
                    .gaps
                    .contains("native_input_rollout_records_invalid")
            );
        } else {
            assert!(history.source.is_some());
        }
    }
    Ok(())
}

#[tokio::test]
async fn rollout_names_alone_cannot_bind_a_foreign_path_or_a_duplicate_physical_source()
-> Result<()> {
    for foreign in [true, false] {
        let directory = tempfile::tempdir()?;
        let handle = fixture(directory.path(), Harness::Codex).await?;
        let service = &handle.service;
        create(service, directory.path()).await?;
        let evidence = producer(service, "turn").await?;
        let original = rollout(service, "original", "native", "turn").await?;
        let path = if foreign {
            directory.path().join("outside/rollout-native.jsonl")
        } else {
            rollout(service, "copy", "native", "turn").await?;
            original
        };
        let mut rows = lifecycle("thread/start", "create", &path);
        rows.extend(turn("turn"));
        spool(service, rows).await?;
        let result = service
            .native_inputs(
                &BTreeMap::from([("attempt".into(), evidence)]),
                &BTreeSet::new(),
            )
            .await?;
        let history = result["attempt"].bindings[0]
            .codex_history
            .as_ref()
            .context("history")?;
        assert!(history.source.is_none());
        assert!(history.gaps.contains(if foreign {
            "native_input_rollout_path_outside_root"
        } else {
            "native_input_rollout_source_ambiguous"
        }));
    }
    Ok(())
}

#[tokio::test]
async fn copied_context_inside_compaction_does_not_prove_an_input_executed_in_a_rollout()
-> Result<()> {
    let directory = tempfile::tempdir()?;
    let handle = fixture(directory.path(), Harness::Codex).await?;
    let service = &handle.service;
    create(service, directory.path()).await?;
    let evidence = producer(service, "turn").await?;
    let original = rollout(service, "original", "native", "other").await?;
    let mut file = tokio::fs::OpenOptions::new()
        .append(true)
        .open(&original)
        .await?;
    use tokio::io::AsyncWriteExt;
    file.write_all(b"{\"ordinal\":2,\"type\":\"compacted\",\"payload\":{\"replacement_history\":[{\"type\":\"turn_context\",\"payload\":{\"turn_id\":\"turn\"}}]}}\n").await?;
    drop(file);
    service
        .collector
        .reconcile(
            NodeKey {
                namespace: service.collector.root().namespace.clone(),
                harness: Harness::Codex,
                native_id: "native".into(),
                agent_id: None,
            },
            &original,
            None,
        )
        .await?;
    let mut rows = lifecycle("thread/start", "create", &original);
    rows.extend(turn("turn"));
    spool(service, rows).await?;
    let result = service
        .native_inputs(
            &BTreeMap::from([("attempt".into(), evidence)]),
            &BTreeSet::new(),
        )
        .await?;
    let history = result["attempt"].bindings[0]
        .codex_history
        .as_ref()
        .context("history")?;
    assert!(history.source.is_none());
    assert!(
        history
            .gaps
            .contains("native_input_rollout_turn_unobserved")
    );
    Ok(())
}

#[tokio::test]
async fn duplicate_rollout_inventory_retains_only_bounded_ambiguity_and_budget_gaps() -> Result<()>
{
    let directory = tempfile::tempdir()?;
    let handle = fixture(directory.path(), Harness::Codex).await?;
    let service = &handle.service;
    create(service, directory.path()).await?;
    let evidence = producer(service, "turn").await?;
    let original = rollout(service, "original", "native", "turn").await?;
    let mut rows = lifecycle("thread/start", "create", &original);
    rows.extend(turn("turn"));
    spool(service, rows).await?;
    let result = service
        .native_inputs(
            &BTreeMap::from([("attempt".into(), evidence)]),
            &BTreeSet::new(),
        )
        .await?;
    let binding = result["attempt"].bindings[0].clone();
    assert!(
        binding
            .codex_history
            .as_ref()
            .context("history")?
            .source
            .is_some()
    );
    for n in 0..32 {
        rollout(service, &format!("copy-{n}"), "native", "turn").await?;
    }
    for limited in [false, true] {
        let mut binding = binding.clone();
        let history = binding.codex_history.as_mut().context("history")?;
        history.source = None;
        history.turn_contexts.clear();
        history.inspected.clear();
        let mut group = Group {
            source: service
                .store
                .source(&binding.origin.request.range.source_id)
                .await?
                .context("controller")?,
            spool: service.spool.clone(),
            native_root: service.collector.root().directory.clone(),
            registration: binding.controller_registration.clone(),
            prompts: vec![],
            inspected: vec![],
            gaps: BTreeSet::new(),
            bindings: vec![binding],
        };
        let mut records = if limited { 1 } else { MAX_RECORDS as u64 };
        let mut bytes = 16 * 1024;
        service
            .corroborate_rollouts(&mut group, &mut records, &mut bytes)
            .await?;
        let history = group.bindings[0]
            .codex_history
            .as_ref()
            .context("history")?;
        assert!(history.source.is_none());
        assert!(history.gaps.contains(if limited {
            "native_input_rollout_inventory_limit"
        } else {
            "native_input_rollout_source_ambiguous"
        }));
        assert!(
            !history
                .gaps
                .contains("native_input_rollout_materialization_limit")
        );
        assert!(bytes > 0);
    }
    Ok(())
}

#[tokio::test]
#[ignore = "requires an isolated real Codex proxy capture"]
async fn captured_codex_proxy_records_bind_input_rollouts() -> Result<()> {
    // ACP producers are fixtures; the proxy records and rollouts are original
    // native output. This is not full adapter/runtime conformance.
    let capture = PathBuf::from(std::env::var("BITROUTER_TEST_CODEX_LIFECYCLE_CAPTURE")?);
    let profile = capture.join("profile");
    let directory = tempfile::tempdir()?;
    let handle = fixture_at(directory.path(), Harness::Codex, Some(&profile)).await?;
    let service = &handle.service;
    create(service, directory.path()).await?;
    let mut files = tokio::fs::read_dir(capture.join("proxy")).await?;
    let mut paths = Vec::new();
    while let Some(file) = files.next_entry().await? {
        if file
            .path()
            .extension()
            .is_some_and(|extension| extension == "jsonl")
        {
            paths.push(file.path());
        }
    }
    ensure!(
        paths.len() == 1,
        "capture needs one complete proxy connection"
    );
    let path = &paths[0];
    ensure!(
        tokio::fs::metadata(path).await?.len() <= MAX_OBJECT_BYTES as u64,
        "capture exceeds fixture limit"
    );
    let raw = tokio::fs::read_to_string(path).await?;
    let mut requests = BTreeMap::new();
    let mut turns = Vec::new();
    let mut reverted = false;
    for line in raw.lines() {
        let row: Value = serde_json::from_str(line)?;
        if row["method"] == "thread/revert" && row["phase"] == "response" {
            reverted = true;
        }
        if row["method"] != "turn/start" {
            continue;
        }
        let id = row["operation_id"].as_str().context("RPC id")?.to_owned();
        if row["phase"] == "request" {
            requests.insert(
                id,
                row["payload"]["threadId"]
                    .as_str()
                    .context("thread")?
                    .to_owned(),
            );
        } else if row["phase"] == "response" && row["payload"].get("error_code").is_none() {
            turns.push((
                requests.remove(&id).context("original request")?,
                row["payload"]["turn"]["id"]
                    .as_str()
                    .context("turn")?
                    .to_owned(),
            ));
        }
    }
    ensure!(
        reverted && turns.len() >= 8,
        "capture lacks fork/resume/compact/revert inputs"
    );
    let copied = service.spool.join(path.file_name().context("spool name")?);
    tokio::fs::copy(path, &copied).await?;
    import_spool(
        &service.store,
        &service.spool,
        &copied,
        SourceDescriptor {
            namespace: service.collector.root().namespace.clone(),
            harness: Harness::Codex,
            format: SourceFormat::CodexAppServer,
            locator: format!("spool:{}", copied.display()),
            node: None,
        },
    )
    .await?;
    let mut selected = BTreeMap::new();
    for (n, (thread, turn)) in turns.iter().enumerate() {
        let evidence = prompt(
            service,
            &format!("input-{n}"),
            Event::CodexAccepted {
                thread_id: thread.clone(),
                turn_id: turn.clone(),
                role: "prompt".into(),
            },
        )
        .await?;
        selected.insert("attempt".into(), evidence);
    }
    let result = service.native_inputs(&selected, &BTreeSet::new()).await?;
    assert_eq!(result["attempt"].bindings.len(), turns.len());
    let mut by_thread = BTreeMap::<String, BTreeSet<String>>::new();
    for binding in &result["attempt"].bindings {
        let history = binding.codex_history.as_ref().context("history")?;
        assert!(
            history.gaps.is_empty(),
            "{}: {:?}",
            binding.native_id,
            history.gaps
        );
        let source = history.source.as_ref().context("source")?;
        assert!(!history.turn_contexts.is_empty());
        by_thread
            .entry(binding.node.native_id.clone())
            .or_default()
            .insert(source.rollout_id.clone());
    }
    assert!(by_thread.values().any(|rollouts| rollouts.len() == 2));
    assert!(by_thread.len() >= 3);
    drop(handle);
    let reopened = fixture_at(directory.path(), Harness::Codex, Some(&profile)).await?;
    let restored = reopened
        .service
        .native_inputs(&selected, &BTreeSet::new())
        .await?;
    assert_eq!(
        serde_json::to_value(result)?,
        serde_json::to_value(restored)?
    );
    Ok(())
}
