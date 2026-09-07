use super::*;
use crate::session_evidence::claude_proxy::{NAMESPACE_ENV, ORIGIN_ENV, SPOOL_ENV};
use crate::session_evidence::service::tests::{claude_service, observation, write_rows};
use sea_orm::{ConnectionTrait, DbBackend, Statement, TransactionTrait};
use tokio::io::AsyncWriteExt;

async fn service(directory: &Path) -> Result<EvidenceHandle> {
    let mut handle = claude_service(directory).await?;
    handle.worker.abort();
    if let Err(error) = (&mut handle.worker).await {
        ensure!(error.is_cancelled(), "fixture worker failed");
    }
    Ok(handle)
}

async fn prepare(service: &ControllerEvidence, directory: &Path, profile: &str) -> Result<Value> {
    service
        .observe(observation(
            profile,
            "session/new",
            "request",
            json!({"cwd":directory}),
        ))
        .await?;
    Ok(service.prepare_session_request(profile, "session/new", json!({"cwd":directory,"_meta":{"claudeCode":{"options":{"env":{"CLAUDE_CONFIG_DIR":directory.join(profile)}}}}})).await?)
}

async fn process(params: &Value, messages: &[Value]) -> Result<PathBuf> {
    write_process(params, messages, &uuid::Uuid::new_v4().to_string()).await
}

async fn write_process(params: &Value, messages: &[Value], process: &str) -> Result<PathBuf> {
    let env = &params["_meta"]["claudeCode"]["options"]["env"];
    let path = PathBuf::from(env[SPOOL_ENV].as_str().context("fixture spool")?)
        .join(format!("cli-{process}.jsonl"));
    let configuration: RecordRef =
        serde_json::from_str(env[ORIGIN_ENV].as_str().context("fixture configuration")?)?;
    let mut rows = vec![
        json!({"method":"runtime/started","phase":"metadata","configured_by":configuration,"configuration_status":"present"}),
    ];
    rows.extend(messages.iter().map(
        |message| json!({"method":"runtime/message","phase":"notification","payload":message}),
    ));
    for (sequence, row) in rows.iter_mut().enumerate() {
        row["process_id"] = json!(process);
        row["namespace"] = env[NAMESPACE_ENV].clone();
        row["scope_valid"] = json!(true);
        row["sequence"] = json!(sequence);
    }
    write_rows(&path, rows).await?;
    Ok(path)
}

fn init(id: &str, uuid: &str) -> Value {
    json!({"type":"system","subtype":"init","session_id":id,"uuid":uuid,"claude_code_version":"2.1.257","capabilities":["msg_lifecycle_v1"]})
}

async fn sdk(service: &ControllerEvidence, acp: &str, message: &Value) -> Result<()> {
    let fields = service
        .notification_fields(
            super::super::super::claude_sdk::METHOD,
            &json!({"sessionId":acp,"message":message}),
        )
        .context("fixture SDK fields")?;
    service
        .observe(observation(
            &uuid::Uuid::new_v4().to_string(),
            super::super::super::claude_sdk::METHOD,
            "notification",
            fields,
        ))
        .await?;
    Ok(())
}

#[tokio::test]
async fn a_new_inventory_restarts_directory_pagination_after_a_failed_scan() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let handle = service(directory.path()).await?;
    let prepared = prepare(&handle.service, directory.path(), "profile").await?;
    let message = init("root", "same-event");
    sdk(&handle.service, "root", &message).await?;
    let first_path = write_process(
        &prepared,
        std::slice::from_ref(&message),
        "10000000-0000-4000-8000-000000000000",
    )
    .await?;
    for index in 0..129 {
        write_process(
            &prepared,
            &[],
            &format!("f0000000-0000-4000-8000-{index:012x}"),
        )
        .await?;
    }
    let initial = handle.service.reconcile().await?;
    assert!(initial.gaps.contains("native_spool_backlog"));
    let spool = first_path.parent().context("spool")?.to_owned();
    assert!(
        handle
            .service
            .state
            .lock()
            .await
            .spool_after
            .contains_key(&spool)
    );
    let held = spool.with_extension("held");
    tokio::fs::rename(&spool, &held).await?;
    let failed = handle.service.reconcile().await?;
    assert!(failed.gaps.contains("native_spool_failed"));
    assert!(!handle.service.state.lock().await.inventory_cycle_active);
    assert!(
        handle
            .service
            .state
            .lock()
            .await
            .spool_after
            .contains_key(&spool)
    );
    tokio::fs::rename(&held, &spool).await?;
    // This process sorts before the old directory cursor. Its SDK observation
    // belongs to the next epoch, which must enumerate that prefix again.
    write_process(
        &prepared,
        std::slice::from_ref(&message),
        "00000000-0000-4000-8000-000000000000",
    )
    .await?;
    sdk(&handle.service, "root", &message).await?;
    let recovered = recover(&handle.service).await?;
    assert_eq!(recovered.sdk_bindings.observations.len(), 2);
    assert!(recovered.sdk_bindings.observations.iter().all(|binding| {
        binding.process_id.is_none() && binding.gaps.contains("native_sdk_process_ambiguous")
    }));
    Ok(())
}

#[tokio::test]
async fn an_observer_cancelled_after_commit_is_recovered_without_its_sdk_source_cache() -> Result<()>
{
    let directory = tempfile::tempdir()?;
    let handle = service(directory.path()).await?;
    let prepared = prepare(&handle.service, directory.path(), "profile").await?;
    let message = init("root", "committed-before-cancellation");
    process(&prepared, std::slice::from_ref(&message)).await?;
    let source = handle
        .service
        .store
        .sources(None, 128)
        .await?
        .into_iter()
        .find(|source| {
            source.descriptor.format == SourceFormat::Acp
                && source.descriptor.namespace == handle.service.collector.root().namespace
        })
        .context("current default journal")?;
    let range = SourceRange {
        source_id: source.id.clone(),
        generation: source.cursor.generation.clone(),
        start: source.cursor.next_sequence,
        end: source.cursor.next_sequence + 1,
    };
    let mut pending = Box::pin(sdk(&handle.service, "root", &message));
    // Run the real observer through its initial state reads to its first
    // database await, then prevent its post-commit cache update from finishing.
    std::future::poll_fn(|cx| {
        use std::future::Future;
        match pending.as_mut().poll(cx) {
            std::task::Poll::Pending => std::task::Poll::Ready(Ok(())),
            std::task::Poll::Ready(_) => std::task::Poll::Ready(Err(anyhow::anyhow!(
                "observer must reach a pending database operation"
            ))),
        }
    })
    .await?;
    let state = handle.service.state.lock().await;
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            tokio::select! {
                result = &mut pending => {
                    result?;
                    anyhow::bail!("observer finished through a locked state");
                }
                current = handle.service.store.source(&source.id) => {
                    if current?.context("committed source")?.cursor.next_sequence == range.end {
                        let records = handle.service.store.records(&range).await?;
                        assert_eq!(records[0].input.raw["method"], super::super::super::claude_sdk::METHOD);
                        return anyhow::Ok(());
                    }
                }
            }
            tokio::task::yield_now().await;
        }
    }).await??;
    assert!(state.sdk_sources.is_empty());
    drop(pending);
    drop(state);
    let recovered = recover(&handle.service).await?;
    let binding = recovered
        .sdk_bindings
        .observations
        .first()
        .context("recovered SDK observation")?;
    assert_eq!(binding.observation.range, range);
    assert!(binding.process_id.is_some());
    assert!(binding.gaps.is_empty(), "{:?}", binding.gaps);
    assert!(handle.service.state.lock().await.sdk_sources.is_empty());
    Ok(())
}

async fn recover(service: &ControllerEvidence) -> Result<CollectionSnapshot> {
    let required_epoch = service.state.lock().await.inventory_epoch + 1;
    for _ in 0..256 {
        let snapshot = service.reconcile().await?;
        let state = service.state.lock().await;
        let captured: BTreeSet<_> = state
            .sdk_inventory
            .sources
            .iter()
            .map(|source| source.source_id.clone())
            .collect();
        if state.inventory_epoch >= required_epoch
            && !snapshot.gaps.contains("native_recovery_backlog")
            && !snapshot.gaps.contains("native_spool_backlog")
            && state.sdk_sources.is_subset(&captured)
        {
            return Ok(snapshot);
        }
    }
    anyhow::bail!("fixture recovery did not converge")
}

#[tokio::test]
async fn early_sdk_events_and_reset_keep_exact_profile_and_native_identity_after_restart()
-> Result<()> {
    let directory = tempfile::tempdir()?;
    let handle = service(directory.path()).await?;
    let prepared = prepare(&handle.service, directory.path(), "profile").await?;
    let messages = vec![
        init("acp-root", "init-event"),
        json!({"type":"conversation_reset","uuid":"reset-event","session_id":"acp-root","new_conversation_id":"fresh-native"}),
        json!({"type":"system","subtype":"session_state_changed","uuid":"idle-event","session_id":"fresh-native","state":"idle"}),
    ];
    sdk(&handle.service, "acp-root", &messages[0]).await?;
    let before = handle.service.reconcile().await?;
    assert!(before.sdk_bindings.observations[0].node.is_none());
    let path = process(&prepared, &messages).await?;
    let early = recover(&handle.service).await?;
    assert_eq!(early.sdk_bindings.observations.len(), 1);
    let binding = &early.sdk_bindings.observations[0];
    assert!(binding.gaps.is_empty(), "{:?}", binding.gaps);
    assert_eq!(
        binding
            .node
            .as_ref()
            .context("early native identity")?
            .namespace,
        prepared["_meta"]["claudeCode"]["options"]["env"][NAMESPACE_ENV]
    );
    let raw = handle
        .service
        .store
        .records(&binding.observation.range)
        .await?;
    assert_eq!(raw[0].input.raw["native_scope"], "controller");
    assert_ne!(
        binding.observation.range.source_id,
        binding.native_records[0].range.source_id
    );
    handle
        .service
        .observe(observation(
            "profile",
            "session/new",
            "response",
            json!({"sessionId":"acp-root"}),
        ))
        .await?;
    for message in &messages[1..] {
        sdk(&handle.service, "acp-root", message).await?;
    }
    for id in ["acp-root", "fresh-native"] {
        write_rows(&directory.path().join(format!("profile/projects/work/{id}.jsonl")), vec![json!({"type":"user","sessionId":id,"uuid":format!("user-{id}"),"parentUuid":null,"message":{"content":"work"}})]).await?;
    }
    let snapshot = recover(&handle.service).await?;
    assert_eq!(snapshot.sdk_bindings.observations.len(), 3);
    assert!(
        snapshot
            .sdk_bindings
            .observations
            .iter()
            .all(|binding| binding.gaps.is_empty())
    );
    assert!(!snapshot.gaps.contains("native_sdk_scope_unresolved"));
    let fresh = snapshot
        .sdk_bindings
        .observations
        .iter()
        .find(|binding| {
            binding
                .node
                .as_ref()
                .is_some_and(|node| node.native_id == "fresh-native")
        })
        .context("reset native attachment")?;
    assert_eq!(fresh.acp_session_id, "acp-root");
    assert!(snapshot.graph.facts.iter().any(|fact| {
        fact.acp_session_id.as_deref() == Some("acp-root")
            && fact
                .node
                .as_ref()
                .is_some_and(|node| node.native_id == "fresh-native")
    }));
    let saved = serde_json::to_value(&snapshot.sdk_bindings.observations)?;
    std::fs::remove_file(path)?;
    drop(handle);
    let resumed = service(directory.path()).await?;
    let restored = recover(&resumed.service).await?;
    assert_eq!(
        serde_json::to_value(&restored.sdk_bindings.observations)?,
        saved
    );
    assert!(resumed.service.state.lock().await.loaded.is_empty());
    assert!(resumed.service.state.lock().await.sessions.is_empty());
    Ok(())
}

#[tokio::test]
async fn duplicate_event_in_distinct_processes_is_ambiguous_without_choosing_a_profile()
-> Result<()> {
    let directory = tempfile::tempdir()?;
    let handle = service(directory.path()).await?;
    let a = prepare(&handle.service, directory.path(), "a").await?;
    let b = prepare(&handle.service, directory.path(), "b").await?;
    let message = init("root", "repeated-event");
    sdk(&handle.service, "root", &message).await?;
    process(&a, std::slice::from_ref(&message)).await?;
    let first = recover(&handle.service).await?;
    assert!(first.sdk_bindings.observations[0].node.is_some());
    process(&b, &[message]).await?;
    let conflicting = recover(&handle.service).await?;
    let binding = &conflicting.sdk_bindings.observations[0];
    assert!(binding.node.is_none());
    assert!(binding.process_id.is_none());
    assert!(binding.native_records.is_empty());
    assert!(binding.gaps.contains("native_sdk_process_ambiguous"));
    Ok(())
}

#[tokio::test]
async fn equal_uuid_needs_equal_metadata_and_same_verified_controller() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let left = service(directory.path()).await?;
    let right = service(directory.path()).await?;
    let a = prepare(&left.service, directory.path(), "a").await?;
    let b = prepare(&right.service, directory.path(), "b").await?;
    let message = init("root", "shared-event");
    sdk(&left.service, "root", &message).await?;
    let mut different = message.clone();
    different["claude_code_version"] = json!("2.1.258");
    process(&a, &[different]).await?;
    process(&b, std::slice::from_ref(&message)).await?;
    let snapshot = recover(&left.service).await?;
    assert!(snapshot.sdk_bindings.observations[0].node.is_none());
    process(&a, &[message]).await?;
    let matching = recover(&left.service).await?;
    let binding = &matching.sdk_bindings.observations[0];
    assert!(binding.gaps.is_empty(), "{:?}", binding.gaps);
    assert_eq!(
        binding.node.as_ref().context("correct profile")?.namespace,
        a["_meta"]["claudeCode"]["options"]["env"][NAMESPACE_ENV]
    );
    Ok(())
}

#[tokio::test]
async fn damaged_raw_copy_cannot_bind_and_does_not_hide_later_history() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let handle = service(directory.path()).await?;
    let prepared = prepare(&handle.service, directory.path(), "profile").await?;
    let message = init("root", "native-event");
    sdk(&handle.service, "root", &message).await?;
    process(&prepared, &[message]).await?;
    let captured = recover(&handle.service).await?;
    let reference = &captured.sdk_bindings.observations[0].native_records[0];
    let db = crate::db::connect(&crate::db::anchor_url(
        "sqlite:evidence.db?mode=rwc",
        &directory.path().join("router"),
    ))
    .await?;
    db.execute(Statement::from_sql_and_values(
        DbBackend::Sqlite,
        "UPDATE native_evidence_records SET digest = 'damaged' WHERE id = ?",
        [reference.record_id.clone().into()],
    ))
    .await?;
    handle
        .service
        .observe(observation(
            "profile",
            "session/new",
            "response",
            json!({"sessionId":"root"}),
        ))
        .await?;
    write_rows(&directory.path().join("profile/projects/work/root.jsonl"), vec![json!({"type":"user","sessionId":"root","uuid":"user","parentUuid":null,"message":{"content":"work"}})]).await?;
    drop(handle);
    let resumed = service(directory.path()).await?;
    let snapshot = recover(&resumed.service).await?;
    assert!(snapshot.gaps.contains("native_sdk_inventory_invalid"));
    assert!(snapshot.gaps.contains("native_sdk_inventory_incomplete"));
    assert!(
        snapshot
            .histories
            .iter()
            .any(|history| history.node.native_id == "root")
    );
    assert!(
        snapshot
            .sdk_bindings
            .observations
            .iter()
            .all(|binding| binding.process_id.is_none())
    );
    Ok(())
}

#[tokio::test]
async fn malformed_filtered_fields_and_missing_uuid_remain_unresolved_live_and_after_restart()
-> Result<()> {
    let directory = tempfile::tempdir()?;
    let handle = service(directory.path()).await?;
    let prepared = prepare(&handle.service, directory.path(), "profile").await?;
    let mut malformed = init("root", "same-event");
    malformed["tool_use_id"] = json!({"private":"content"});
    sdk(&handle.service, "root", &malformed).await?;
    let mut legitimate = init("root", "same-event");
    legitimate["tool_use_id"] = Value::Null;
    process(&prepared, &[legitimate]).await?;
    let mut missing = init("root", "unused");
    missing
        .as_object_mut()
        .context("fixture message")?
        .remove("uuid");
    sdk(&handle.service, "root", &missing).await?;
    let live = handle.service.reconcile().await?;
    assert!(live.sdk_bindings.observations.is_empty());
    assert!(live.gaps.contains("native_sdk_scope_unresolved"));
    let source_id = handle
        .service
        .state
        .lock()
        .await
        .sdk_sources
        .iter()
        .next()
        .cloned()
        .context("SDK source")?;
    let source = handle
        .service
        .store
        .source(&source_id)
        .await?
        .context("SDK source")?;
    let mut retained_marker = false;
    for sequence in 0..source.cursor.next_sequence {
        let records = handle
            .service
            .store
            .records(&SourceRange {
                source_id: source_id.clone(),
                generation: "controller/1".into(),
                start: sequence,
                end: sequence + 1,
            })
            .await?;
        retained_marker |= records.iter().any(|record| {
            record
                .input
                .raw
                .pointer("/payload/message/bitrouter_capture_invalid")
                == Some(&Value::Bool(true))
        });
    }
    assert!(retained_marker);
    drop(handle);
    let resumed = service(directory.path()).await?;
    let restored = recover(&resumed.service).await?;
    assert!(restored.sdk_bindings.observations.is_empty());
    assert!(restored.gaps.contains("native_sdk_scope_unresolved"));
    Ok(())
}

#[tokio::test]
async fn raw_sdk_windows_advance_past_corrupt_records_and_more_than_1024_observations() -> Result<()>
{
    let directory = tempfile::tempdir()?;
    let handle = service(directory.path()).await?;
    prepare(&handle.service, directory.path(), "profile").await?;
    handle
        .service
        .observe(observation(
            "profile",
            "session/new",
            "response",
            json!({"sessionId":"root"}),
        ))
        .await?;
    for index in 0..1030 {
        sdk(&handle.service, "root", &json!({"type":"system","subtype":"session_state_changed","uuid":format!("event-{index}"),"session_id":if index == 1029 { "after-reset" } else { "root" },"state":"idle"})).await?;
    }
    let first = recover(&handle.service).await?;
    assert!(first.sdk_bindings.next.is_some());
    let reference = first
        .sdk_bindings
        .observations
        .first()
        .context("first SDK binding")?
        .observation
        .clone();
    let db = crate::db::connect(&crate::db::anchor_url(
        "sqlite:evidence.db?mode=rwc",
        &directory.path().join("router"),
    ))
    .await?;
    db.execute(Statement::from_sql_and_values(
        DbBackend::Sqlite,
        "UPDATE native_evidence_records SET digest = 'damaged' WHERE id = ?",
        [reference.record_id.into()],
    ))
    .await?;
    let mut tail_seen = 0;
    let mut gap_seen = false;
    for _ in 0..80 {
        let snapshot = handle.service.reconcile().await?;
        gap_seen |= snapshot.gaps.contains("native_sdk_scope_unresolved");
        tail_seen += usize::from(snapshot.sdk_bindings.observations.iter().any(|binding| {
            binding
                .node
                .as_ref()
                .is_some_and(|node| node.native_id == "after-reset")
        }));
    }
    assert!(gap_seen);
    assert!(
        tail_seen >= 2,
        "later observations must be revisited for late native copies"
    );
    Ok(())
}

#[tokio::test]
async fn removing_derived_facts_cannot_turn_two_raw_copies_into_one_process() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let handle = service(directory.path()).await?;
    let prepared = prepare(&handle.service, directory.path(), "profile").await?;
    let message = init("root", "duplicated-event");
    sdk(&handle.service, "root", &message).await?;
    process(&prepared, std::slice::from_ref(&message)).await?;
    process(&prepared, &[message]).await?;
    let snapshot = recover(&handle.service).await?;
    assert!(
        snapshot.sdk_bindings.observations[0]
            .gaps
            .contains("native_sdk_process_ambiguous")
    );
    let source = snapshot
        .processes
        .last()
        .context("second process")?
        .source_id
        .clone();
    let db = crate::db::connect(&crate::db::anchor_url(
        "sqlite:evidence.db?mode=rwc",
        &directory.path().join("router"),
    ))
    .await?;
    db.execute(Statement::from_sql_and_values(DbBackend::Sqlite, "DELETE FROM native_execution_facts WHERE record_id IN (SELECT id FROM native_evidence_records WHERE source_id = ?)", [source.into()])).await?;
    let after = handle
        .service
        .sdk_bindings(&snapshot.processes, &mut BTreeSet::new())
        .await;
    assert!(after.observations[0].node.is_none());
    assert!(
        after.observations[0]
            .gaps
            .contains("native_sdk_process_ambiguous")
    );
    assert_eq!(after.process_ranges.len(), 2);
    let source_ids = snapshot
        .processes
        .iter()
        .map(|process| process.source_id.clone())
        .collect();
    let partial = handle
        .service
        .store
        .native_message_copies(&source_ids, &BTreeSet::new(), 2)
        .await;
    assert!(partial.gaps.contains("native_sdk_inventory_limit"));
    assert_eq!(partial.ranges.len(), 1);
    Ok(())
}

#[tokio::test]
async fn early_sdk_pages_all_reach_a_completed_inventory_and_unpublished_pages_do_not_advance()
-> Result<()> {
    let directory = tempfile::tempdir()?;
    let handle = service(directory.path()).await?;
    let prepared = prepare(&handle.service, directory.path(), "profile").await?;
    let mut messages = Vec::new();
    for index in 0..130 {
        let message = json!({"type":"system","subtype":"session_state_changed","uuid":format!("early-{index}"),"session_id":"root","state":"idle"});
        sdk(&handle.service, "root", &message).await?;
        messages.push(message);
    }
    // The first cycle finishes registration before these two directory pages.
    // The next cycle needs more registration pages than directory pages.
    for message in &messages {
        process(&prepared, std::slice::from_ref(message)).await?;
    }
    let initial = handle.service.reconcile().await?;
    assert!(!initial.gaps.contains("native_recovery_backlog"));
    assert!(initial.gaps.contains("native_spool_backlog"));
    let first = handle.service.reconcile().await?;
    assert!(!first.gaps.contains("native_recovery_backlog"));
    assert!(!first.gaps.contains("native_spool_backlog"));
    assert!(first.sdk_bindings.next.is_some());
    let cursor = serde_json::to_value(&handle.service.state.lock().await.sdk_cursor)?;
    for reason in ["native_recovery_backlog", "native_spool_backlog"] {
        let mut backlog = BTreeSet::from([reason.into()]);
        let unpublished = handle
            .service
            .sdk_bindings(&first.processes, &mut backlog)
            .await;
        assert_eq!(serde_json::to_value(&unpublished.next)?, cursor);
        assert!(
            unpublished
                .observations
                .iter()
                .all(|binding| binding.process_id.is_none())
        );
    }
    // Discarding a completed candidate page models cancellation before the
    // reconcile publication lock. Its cursor and bindings are not published.
    let candidate = handle
        .service
        .sdk_bindings(&first.processes, &mut BTreeSet::new())
        .await;
    assert!(!candidate.observations.is_empty());
    drop(candidate);
    assert_eq!(
        serde_json::to_value(&handle.service.state.lock().await.sdk_cursor)?,
        cursor
    );
    let mut seen = BTreeSet::new();
    for binding in first.sdk_bindings.observations {
        if binding.process_id.is_some() {
            seen.insert(binding.observation.record_id);
        }
    }
    let mut registration_waited = false;
    for _ in 0..24 {
        let snapshot = handle.service.reconcile().await?;
        registration_waited |= snapshot.gaps.contains("native_recovery_backlog")
            && !snapshot.gaps.contains("native_spool_backlog");
        for binding in snapshot.sdk_bindings.observations {
            if binding.process_id.is_some() {
                seen.insert(binding.observation.record_id);
            }
        }
        if seen.len() == 130 {
            break;
        }
    }
    assert_eq!(
        seen.len(),
        130,
        "every early observation must get an inventory cut"
    );
    assert!(registration_waited);
    Ok(())
}

async fn append_message(path: &Path, message: &Value) -> Result<()> {
    let body = tokio::fs::read_to_string(path).await?;
    let header: Value = serde_json::from_str(body.lines().next().context("process header")?)?;
    let row = json!({"method":"runtime/message","phase":"notification","payload":message,
        "sequence":body.lines().count(),"namespace":header["namespace"],
        "process_id":header["process_id"],"scope_valid":true});
    let mut file = tokio::fs::OpenOptions::new()
        .append(true)
        .open(path)
        .await?;
    file.write_all(format!("{row}\n").as_bytes()).await?;
    file.sync_data().await?;
    Ok(())
}

async fn prepared_root(service: &ControllerEvidence, prepared: &Value) -> Result<RootContext> {
    service
        .state
        .lock()
        .await
        .roots
        .get(
            prepared["_meta"]["claudeCode"]["options"]["env"][NAMESPACE_ENV]
                .as_str()
                .context("profile namespace")?,
        )
        .cloned()
        .context("registered profile")
}

#[tokio::test]
async fn a_cancelled_zero_record_process_remains_unknown_live_and_after_restart() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let handle = service(directory.path()).await?;
    let prepared = prepare(&handle.service, directory.path(), "profile").await?;
    let message = init("root", "same-event");
    sdk(&handle.service, "root", &message).await?;
    process(&prepared, std::slice::from_ref(&message)).await?;
    let original = recover(&handle.service).await?;
    assert!(original.sdk_bindings.observations[0].process_id.is_some());
    let lost = process(&prepared, std::slice::from_ref(&message)).await?;
    sdk(&handle.service, "root", &message).await?;
    let root = prepared_root(&handle.service, &prepared).await?;
    let id = handle.service.store.source_id(&SourceDescriptor {
        namespace: root.collector.root().namespace.clone(),
        harness: Harness::ClaudeCode,
        format: SourceFormat::ClaudeCli,
        locator: format!("spool:{}", lost.to_string_lossy()),
        node: None,
    })?;
    let mut nodes = BTreeSet::new();
    let mut gaps = BTreeSet::new();
    // A manually paused SQL future may retain a pooled connection. Observe
    // its committed state through an independent SQLite connection so the
    // test itself cannot wait for that paused future to release its connection.
    let reader = EvidenceStore::new(
        crate::db::connect(&crate::db::anchor_url(
            "sqlite:evidence.db?mode=ro",
            &directory.path().join("router"),
        ))
        .await?,
        handle.service.store.owner(),
    )?;
    let mut pending = Box::pin(handle.service.reconcile_spool_file(
        root.collector.root(),
        &lost,
        true,
        &mut nodes,
        &mut gaps,
    ));
    let mut polls = 0;
    let mut observed_cursor = None;
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            polls += 1;
            std::future::poll_fn(|cx| {
                use std::future::Future;
                match pending.as_mut().poll(cx) {
                    std::task::Poll::Pending => std::task::Poll::Ready(Ok(())),
                    std::task::Poll::Ready(_) => std::task::Poll::Ready(Err(anyhow::anyhow!(
                        "import completed before the cancellation window"
                    ))),
                }
            })
            .await?;
            if let Some(source) = reader.source(&id).await? {
                observed_cursor = Some(source.cursor.next_sequence);
                if reader
                    .spool_extent(&source)
                    .await?
                    .is_some_and(|end| end > 0)
                {
                    assert_eq!(source.cursor.next_sequence, 0);
                    return anyhow::Ok(());
                }
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .with_context(|| {
        format!("no committed extent after {polls} polls; cursor {observed_cursor:?}")
    })??;
    drop(pending);
    tokio::fs::remove_file(lost).await?;
    let snapshot = recover(&handle.service).await?;
    assert!(
        snapshot
            .processes
            .iter()
            .any(|process| process.source_id == id && process.process_id.is_none())
    );
    assert!(snapshot.gaps.contains("native_spool_tail_uncollected"));
    assert!(
        snapshot
            .sdk_bindings
            .observations
            .iter()
            .all(|binding| binding.process_id.is_none())
    );
    drop(reader);
    drop(handle);
    let restarted = service(directory.path()).await?;
    let restored = recover(&restarted.service).await?;
    assert!(
        restored
            .processes
            .iter()
            .any(|process| process.source_id == id && process.process_id.is_none())
    );
    assert!(restored.gaps.contains("native_spool_tail_uncollected"));
    assert!(!restored.sdk_bindings.observations.is_empty());
    assert!(
        restored
            .sdk_bindings
            .observations
            .iter()
            .all(|binding| binding.process_id.is_none())
    );
    Ok(())
}

#[tokio::test]
async fn a_process_inventory_limit_survives_removal_of_the_excluded_spool() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let handle = service(directory.path()).await?;
    let prepared = prepare(&handle.service, directory.path(), "profile").await?;
    let message = init("root", "same-event");
    sdk(&handle.service, "root", &message).await?;
    process(&prepared, std::slice::from_ref(&message)).await?;
    let original = recover(&handle.service).await?;
    assert!(original.sdk_bindings.observations[0].process_id.is_some());
    {
        let mut state = handle.service.state.lock().await;
        for index in 1..MAX_GRAPH_ITEMS {
            state
                .process_sources
                .insert(canonical_digest(&("other-process", index))?);
        }
        assert_eq!(state.process_sources.len(), MAX_GRAPH_ITEMS);
    }
    let excluded = process(&prepared, std::slice::from_ref(&message)).await?;
    sdk(&handle.service, "root", &message).await?;
    let root = prepared_root(&handle.service, &prepared).await?;
    let mut transient = BTreeSet::new();
    handle
        .service
        .reconcile_spool_file(
            root.collector.root(),
            &excluded,
            true,
            &mut BTreeSet::new(),
            &mut transient,
        )
        .await?;
    assert!(transient.contains("native_process_source_limit"));
    tokio::fs::remove_file(excluded).await?;
    handle.service.begin_inventory().await?;
    // Recompute a page with the intact, verified process records and a fresh
    // gap set. The previously observed inventory overflow cannot be forgotten.
    let mut fresh = BTreeSet::new();
    let page = handle
        .service
        .sdk_bindings(&original.processes, &mut fresh)
        .await;
    assert!(fresh.contains("native_process_source_limit"));
    assert!(!page.observations.is_empty());
    assert!(
        page.observations
            .iter()
            .all(|binding| binding.process_id.is_none()
                && binding.gaps.contains("native_sdk_inventory_incomplete"))
    );
    Ok(())
}

#[tokio::test]
async fn a_lost_uncollected_tail_allows_page_progress_but_stays_incomplete_after_restart()
-> Result<()> {
    let directory = tempfile::tempdir()?;
    let handle = service(directory.path()).await?;
    let prepared = prepare(&handle.service, directory.path(), "profile").await?;
    let messages: Vec<_> = (0..130)
        .map(|index| init("root", &format!("event-{index}")))
        .collect();
    for message in &messages {
        sdk(&handle.service, "root", message).await?;
    }
    process(&prepared, &messages).await?;
    let mut delayed: Vec<_> = (0..129)
        .map(|index| init("other", &format!("padding-{index}")))
        .collect();
    delayed.push(messages[0].clone());
    let lost = process(&prepared, &delayed).await?;
    let first = handle.service.reconcile().await?;
    assert!(first.gaps.contains("native_spool_backlog"));
    let source = handle
        .service
        .store
        .sources(None, 128)
        .await?
        .into_iter()
        .find(|source| source.descriptor.locator == format!("spool:{}", lost.to_string_lossy()))
        .context("partially imported process")?;
    assert_eq!(source.cursor.next_sequence, 128);
    assert!(
        handle
            .service
            .store
            .spool_extent(&source)
            .await?
            .is_some_and(|end| end > source.cursor.offset)
    );
    tokio::fs::remove_file(lost).await?;
    let advanced = handle.service.reconcile().await?;
    assert!(!advanced.gaps.contains("native_spool_backlog"));
    assert!(advanced.gaps.contains("native_spool_tail_uncollected"));
    assert!(advanced.sdk_bindings.next.is_some());
    assert!(
        advanced
            .sdk_bindings
            .observations
            .iter()
            .all(|binding| binding.process_id.is_none())
    );
    assert!(!handle.service.state.lock().await.inventory_cycle_active);
    drop(handle);
    let restarted = service(directory.path()).await?;
    let snapshot = recover(&restarted.service).await?;
    assert!(snapshot.gaps.contains("native_spool_tail_uncollected"));
    assert!(!snapshot.gaps.contains("native_spool_backlog"));
    assert!(!snapshot.sdk_bindings.observations.is_empty());
    assert!(
        snapshot
            .sdk_bindings
            .observations
            .iter()
            .all(|binding| binding.process_id.is_none())
    );
    Ok(())
}

#[tokio::test]
async fn late_notifications_wait_for_new_processes_and_appended_records_in_completed_directories()
-> Result<()> {
    for append in [false, true] {
        let directory = tempfile::tempdir()?;
        let handle = service(directory.path()).await?;
        let fast = prepare(&handle.service, directory.path(), "fast").await?;
        let slow = prepare(&handle.service, directory.path(), "slow").await?;
        let before = init("root", "before");
        let late = init("root", "late");
        sdk(&handle.service, "root", &before).await?;
        process(&fast, &[before, late.clone()]).await?;
        let existing = process(&fast, &[]).await?;
        let delayed: Vec<_> = (0..257)
            .map(|index| init("other", &format!("padding-{index}")))
            .collect();
        process(&slow, &delayed).await?;
        let first = handle.service.reconcile().await?;
        assert!(first.gaps.contains("native_spool_backlog"));
        let fast_spool = PathBuf::from(
            fast["_meta"]["claudeCode"]["options"]["env"][SPOOL_ENV]
                .as_str()
                .context("fast spool")?,
        );
        let cut = {
            let state = handle.service.state.lock().await;
            assert!(state.completed_spools.contains(&fast_spool));
            let sdk_id = state.sdk_sources.first().context("SDK journal")?;
            state
                .sdk_inventory
                .sources
                .iter()
                .find(|source| &source.source_id == sdk_id)
                .context("SDK cut")?
                .clone()
        };
        // Match the proxy's ordering: the native event is durable before the
        // SDK notification. Both arrive after this directory's completed cut.
        if append {
            append_message(&existing, &late).await?;
        } else {
            process(&fast, std::slice::from_ref(&late)).await?;
        }
        sdk(&handle.service, "root", &late).await?;
        let mut completed = false;
        for _ in 0..8 {
            let snapshot = handle.service.reconcile().await?;
            assert!(snapshot.sdk_bindings.observations.iter().all(|binding| {
                binding.observation.range.source_id != cut.source_id
                    || binding.observation.range.end <= cut.end
            }));
            if !handle.service.state.lock().await.inventory_cycle_active {
                completed = true;
                break;
            }
        }
        assert!(completed);
        let next = recover(&handle.service).await?;
        let binding = next
            .sdk_bindings
            .observations
            .iter()
            .find(|binding| {
                binding.observation.range.source_id == cut.source_id
                    && binding.observation.range.start == cut.end
            })
            .context("late notification in next inventory")?;
        assert!(binding.process_id.is_none());
        assert!(binding.node.is_none());
        assert!(binding.gaps.contains("native_sdk_process_ambiguous"));
    }
    Ok(())
}

#[tokio::test]
async fn cancelling_after_inventory_completion_preserves_observation_cuts_and_publication_cursor()
-> Result<()> {
    let directory = tempfile::tempdir()?;
    let handle = service(directory.path()).await?;
    let prepared = prepare(&handle.service, directory.path(), "profile").await?;
    let messages: Vec<_> = (0..130)
        .map(|index| init("root", &format!("event-{index}")))
        .collect();
    for message in &messages {
        sdk(&handle.service, "root", message).await?;
    }
    process(&prepared, &messages).await?;
    let epoch = handle.service.begin_inventory().await?;
    let mut gaps = BTreeSet::new();
    handle.service.recover(&mut gaps, epoch).await;
    assert!(!gaps.contains("native_recovery_backlog"));
    let root = handle
        .service
        .state
        .lock()
        .await
        .roots
        .get(
            prepared["_meta"]["claudeCode"]["options"]["env"][NAMESPACE_ENV]
                .as_str()
                .context("profile namespace")?,
        )
        .cloned()
        .context("profile root")?;
    for _ in 0..2 {
        gaps.clear();
        handle
            .service
            .reconcile_spool(
                root.collector.root(),
                &root.spool,
                true,
                &mut BTreeSet::new(),
                &mut gaps,
            )
            .await?;
    }
    assert!(gaps.is_empty(), "{gaps:?}");
    write_rows(&directory.path().join("profile/projects/work/root.jsonl"), vec![
        json!({"type":"user","sessionId":"root","uuid":"user","parentUuid":null,"message":{"content":"work"}}),
    ]).await?;
    let source_id = handle
        .service
        .state
        .lock()
        .await
        .process_sources
        .first()
        .cloned()
        .context("process source")?;
    let process = handle.service.process_binding(source_id.clone()).await;
    let expected = handle
        .service
        .sdk_bindings(&[process], &mut BTreeSet::new())
        .await;
    assert!(expected.next.is_some());
    assert!(!expected.observations.is_empty());
    // The completed inventory and SDK page only read the database. The new
    // transcript is collected afterwards, and its write must wait for SQLite.
    let db = crate::db::connect(&crate::db::anchor_url(
        "sqlite:evidence.db?mode=rwc",
        &directory.path().join("router"),
    ))
    .await?;
    let transaction = db.begin().await?;
    transaction
        .execute(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "UPDATE native_evidence_sources SET revision = revision WHERE id = ?",
            [source_id.into()],
        ))
        .await?;
    let cancelled = tokio::time::timeout(Duration::from_secs(1), handle.service.reconcile()).await;
    transaction.rollback().await?;
    assert!(
        cancelled.is_err(),
        "reconciliation must be cancelled at a real database await"
    );
    {
        let state = handle.service.state.lock().await;
        assert_eq!(state.inventory_epoch, epoch);
        assert!(state.inventory_cycle_active);
        assert!(state.sdk_cursor.is_none());
        assert!(state.snapshot.reconciled_at.is_none());
    }
    let published = handle.service.reconcile().await?;
    assert_eq!(
        serde_json::to_value(&published.sdk_bindings.observation_ranges)?,
        serde_json::to_value(&expected.observation_ranges)?
    );
    assert_eq!(
        serde_json::to_value(&published.sdk_bindings.next)?,
        serde_json::to_value(&expected.next)?
    );
    assert_eq!(handle.service.state.lock().await.inventory_epoch, epoch);
    assert_eq!(
        serde_json::to_value(&handle.service.state.lock().await.sdk_cursor)?,
        serde_json::to_value(&expected.next)?
    );
    Ok(())
}
