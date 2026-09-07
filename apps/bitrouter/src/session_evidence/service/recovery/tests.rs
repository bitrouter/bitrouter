use super::*;
use crate::session_evidence::service::tests::{claude_service, observation, write_rows};
use sea_orm::{ConnectionTrait, DbBackend, Statement, TransactionTrait};

async fn stop_worker(handle: &mut EvidenceHandle) -> Result<()> {
    handle.worker.abort();
    if let Err(error) = (&mut handle.worker).await {
        ensure!(error.is_cancelled(), "fixture collector worker failed");
    }
    Ok(())
}

async fn finish_recovery(service: &ControllerEvidence) -> Result<CollectionSnapshot> {
    for _ in 0..256 {
        let snapshot = service.reconcile().await?;
        if !snapshot.gaps.contains("native_recovery_backlog") {
            return Ok(snapshot);
        }
    }
    anyhow::bail!("fixture recovery did not converge")
}

async fn transcript(native: &Path, id: &str) -> Result<()> {
    write_rows(
        &native.join(format!("projects/work/{id}.jsonl")),
        vec![
            json!({"type":"user","sessionId":id,"uuid":format!("user-{id}"),
            "parentUuid":null,"version":"2.1.220","message":{"content":"same prompt"}}),
        ],
    )
    .await
}

async fn native_start(spool: &Path, id: &str) -> Result<()> {
    write_rows(
        &spool.join(format!("hook-{id}.jsonl")),
        vec![json!({"payload":{
            "hook_event_name":"SessionStart", "session_id":id
        }})],
    )
    .await
}

#[tokio::test]
async fn committed_prompt_is_visible_after_observer_cancellation_on_the_same_controller()
-> Result<()> {
    let directory = tempfile::tempdir()?;
    let mut handle = claude_service(directory.path()).await?;
    stop_worker(&mut handle).await?;
    let service = &handle.service;
    service
        .observe(observation(
            "new",
            "session/new",
            "response",
            json!({"sessionId":"public"}),
        ))
        .await?;
    let source = service
        .store
        .sources(None, 128)
        .await?
        .into_iter()
        .find(|source| source.descriptor.format == SourceFormat::Acp)
        .context("controller journal")?;
    let reader = EvidenceStore::new(
        crate::db::connect(&crate::db::anchor_url(
            "sqlite:evidence.db?mode=ro",
            &directory.path().join("router"),
        ))
        .await?,
        service.store.owner(),
    )?;
    let mut pending = Box::pin(service.observe(observation(
        "cancelled",
        "session/prompt",
        "request",
        json!({"sessionId":"public","prompt":[]}),
    )));
    std::future::poll_fn(|cx| {
        use std::future::Future;
        match pending.as_mut().poll(cx) {
            std::task::Poll::Pending => std::task::Poll::Ready(Ok(())),
            std::task::Poll::Ready(_) => {
                std::task::Poll::Ready(Err(anyhow::anyhow!("observer must reach a database await")))
            }
        }
    })
    .await?;
    let state = service.state.lock().await;
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            tokio::select! {
                result = &mut pending => {
                    result?;
                    anyhow::bail!("observer finished through a locked state");
                }
                current = reader.source(&source.id) => {
                    if current?.context("committed source")?.cursor.next_sequence == source.cursor.next_sequence + 1 {
                        return anyhow::Ok(());
                    }
                }
            }
            tokio::task::yield_now().await;
        }
    }).await??;
    assert!(!state.pending.contains_key("cancelled"));
    drop(pending);
    drop(state);
    let snapshot = service.reconcile().await?;
    assert_eq!(snapshot.attempts.len(), 1);
    assert_eq!(snapshot.attempts[0].session.session_id, "public");
    assert!(snapshot.attempts[0].members.is_empty());
    assert_eq!(
        snapshot.attempts[0].phase,
        super::super::super::types::AttemptPhase::Collecting
    );
    assert!(snapshot.histories.is_empty());
    assert!(!service.state.lock().await.pending.contains_key("cancelled"));
    drop(reader);
    Ok(())
}

#[tokio::test]
async fn claude_public_session_survives_native_reset_without_becoming_a_transcript_identity()
-> Result<()> {
    let directory = tempfile::tempdir()?;
    let mut handle = claude_service(directory.path()).await?;
    stop_worker(&mut handle).await?;
    let service = &handle.service;
    service
        .observe(observation("new", "session/new", "request", json!({})))
        .await?;
    service
        .observe(observation(
            "new",
            "session/new",
            "response",
            json!({"sessionId":"public"}),
        ))
        .await?;
    for id in ["public", "native-before", "native-after"] {
        transcript(&directory.path().join("default"), id).await?;
    }
    service
        .observe(observation(
            "one",
            "session/prompt",
            "request",
            json!({"sessionId":"public","prompt":[]}),
        ))
        .await?;
    let before_native = service.reconcile().await?;
    assert!(before_native.histories.is_empty());
    let original = before_native
        .attempts
        .first()
        .context("logical task without native evidence")?
        .clone();
    assert_eq!(original.session.session_id, "public");
    assert!(original.members.is_empty());
    assert!(
        before_native
            .gaps
            .contains("native_attempt_membership_unavailable")
    );
    native_start(&service.spool, "native-before").await?;
    service
        .observe(observation(
            "reset",
            "_claude/sdkMessage",
            "notification",
            json!({
                "sessionId":"public","message":{"type":"conversation_reset","uuid":"reset-event",
                "session_id":"native-before","new_conversation_id":"native-after"}
            }),
        ))
        .await?;
    service
        .observe(observation(
            "one",
            "session/prompt",
            "response",
            json!({"stopReason":"end_turn"}),
        ))
        .await?;
    service
        .observe(observation(
            "two",
            "session/prompt",
            "request",
            json!({"sessionId":"public","prompt":[]}),
        ))
        .await?;
    // Publishing the task must not depend on finding either native id.
    let mut live = service.reconcile().await?;
    // The earlier snapshot fixed an inventory cut before these observations.
    // A subsequent epoch must replay the newly committed journal tail.
    for _ in 0..32 {
        if live.attempts.len() == 1 && live.histories.len() == 2 {
            break;
        }
        live = service.reconcile().await?;
    }
    assert_eq!(live.attempts.len(), 1);
    assert_eq!(live.attempts[0].id, original.id);
    assert!(live.attempts[0].members.is_empty());
    assert_eq!(
        live.histories
            .iter()
            .map(|history| history.node.native_id.as_str())
            .collect::<BTreeSet<_>>(),
        BTreeSet::from(["native-before", "native-after"])
    );
    assert!(
        !live
            .graph
            .nodes
            .iter()
            .any(|node| node.native_id == "public")
    );
    drop(handle);
    let mut resumed = claude_service(directory.path()).await?;
    stop_worker(&mut resumed).await?;
    let recovered = finish_recovery(&resumed.service).await?;
    assert_eq!(recovered.attempts, live.attempts);
    assert_eq!(
        recovered
            .histories
            .iter()
            .map(|history| history.node.native_id.as_str())
            .collect::<BTreeSet<_>>(),
        BTreeSet::from(["native-before", "native-after"])
    );
    assert!(recovered.gaps.contains("native_prompt_response_unobserved"));
    assert!(resumed.service.state.lock().await.loaded.is_empty());
    Ok(())
}

#[tokio::test]
async fn corrupt_lifecycle_index_keeps_later_raw_sessions_recoverable_after_restart() -> Result<()>
{
    let directory = tempfile::tempdir()?;
    let mut origin = claude_service(directory.path()).await?;
    stop_worker(&mut origin).await?;
    for id in ["early", "later"] {
        origin
            .service
            .observe(observation(
                id,
                "session/new",
                "request",
                json!({"cwd":directory.path()}),
            ))
            .await?;
        origin
            .service
            .observe(observation(
                id,
                "session/new",
                "response",
                json!({"sessionId":id}),
            ))
            .await?;
        transcript(&directory.path().join("default"), id).await?;
        native_start(&origin.service.spool, id).await?;
    }
    let source = origin
        .service
        .store
        .sources(None, 128)
        .await?
        .into_iter()
        .find(|source| source.descriptor.format == SourceFormat::Acp)
        .context("original controller journal")?;
    let controller = source
        .descriptor
        .locator
        .strip_prefix("controller:")
        .context("original controller id")?
        .to_owned();
    let db = crate::db::connect(&crate::db::anchor_url(
        "sqlite:evidence.db?mode=rwc",
        &directory.path().join("router"),
    ))
    .await?;
    let early_key = crate::eval::types::canonical_digest(&(&controller, "early"))?;
    let later_key = crate::eval::types::canonical_digest(&(&controller, "later"))?;
    let changed = db
        .execute(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "UPDATE native_evidence_objects SET object_json = '{}' WHERE kind = 'lifecycle_request' AND object_key = ?",
            [early_key.clone().into()],
        ))
        .await?;
    assert_eq!(changed.rows_affected(), 1);
    // The later operation must be rebuilt from raw evidence, not merely read
    // from the healthy index left behind by its original controller.
    let removed = db
        .execute(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "DELETE FROM native_evidence_objects WHERE kind IN ('lifecycle_request', 'lifecycle_response') AND object_key = ?",
            [later_key.into()],
        ))
        .await?;
    assert_eq!(removed.rows_affected(), 2);
    drop(origin);

    for _ in 0..2 {
        let mut resumed = claude_service(directory.path()).await?;
        stop_worker(&mut resumed).await?;
        let snapshot = finish_recovery(&resumed.service).await?;
        assert!(snapshot.gaps.contains("native_lifecycle_index_invalid"));
        assert!(!snapshot.gaps.contains("native_recovery_source_failed"));
        assert_eq!(
            snapshot
                .histories
                .iter()
                .map(|history| history.node.native_id.as_str())
                .collect::<BTreeSet<_>>(),
            BTreeSet::from(["early", "later"])
        );
        let later = resumed
            .service
            .store
            .lifecycle_operation(&controller, "later")
            .await?;
        assert!(later.request.is_some());
        assert!(later.response.is_some());
        assert!(
            resumed
                .service
                .store
                .lifecycle_operation(&controller, "early")
                .await
                .is_err()
        );
        let damaged = db
            .query_one(Statement::from_sql_and_values(
                DbBackend::Sqlite,
                "SELECT object_json FROM native_evidence_objects WHERE kind = 'lifecycle_request' AND object_key = ?",
                [early_key.clone().into()],
            ))
            .await?
            .context("original damaged index")?;
        assert_eq!(damaged.try_get::<String>("", "object_json")?, "{}");
    }
    Ok(())
}

async fn codex_service(directory: &Path) -> Result<EvidenceHandle> {
    let mut env = HashMap::from([
        (
            "CODEX_HOME".into(),
            directory.join("codex").to_string_lossy().into_owned(),
        ),
        ("CODEX_PATH".into(), "/fixture/codex".into()),
    ]);
    let mut handle = EvidenceHandle::open(EvidenceLaunch {
        home: &directory.join("router"),
        database_url: "sqlite:evidence.db?mode=rwc",
        identity: &ControllerIdentity::new("codex-acp", "@agentclientprotocol/codex-acp", "1.7.0"),
        env: &mut env,
        strip_inherited_env: &[],
    })
    .await?
    .context("Codex service")?;
    stop_worker(&mut handle).await?;
    Ok(handle)
}

#[tokio::test]
async fn codex_restart_recovers_load_identity_and_unimported_native_agent_events() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let origin = codex_service(directory.path()).await?;
    origin
        .service
        .observe(observation(
            "load",
            "session/load",
            "request",
            json!({"sessionId":"loaded-only","cwd":directory.path()}),
        ))
        .await?;
    origin
        .service
        .observe(observation("load", "session/load", "response", json!({})))
        .await?;
    for id in ["loaded-only", "parent", "child"] {
        write_rows(&directory.path().join(format!("codex/sessions/rollout-{id}.jsonl")), vec![
            json!({"type":"session_meta","payload":{"id":id,"cli_version":"0.148.0"}}),
            json!({"type":"response_item","payload":{"type":"message","role":"user","content":"work"}}),
        ]).await?;
    }
    let tap = origin.service.spool.join("native-process.jsonl");
    write_rows(
        &tap,
        vec![
            json!({"method":"runtime/started","version":"0.148.0"}),
            json!({"direction":"server","phase":"notification","method":"item/completed",
            "payload":{"threadId":"parent","turnId":"turn","item":{
                "id":"spawn","type":"collabAgentToolCall","tool":"spawnAgent","status":"completed",
                "senderThreadId":"parent","receiverThreadIds":["child"],"agentsStates":{}
            }}}),
        ],
    )
    .await?;
    drop(origin);
    let resumed = codex_service(directory.path()).await?;
    let snapshot = finish_recovery(&resumed.service).await?;
    assert!(snapshot.gaps.is_empty(), "{:?}", snapshot.gaps);
    assert_eq!(
        snapshot
            .histories
            .iter()
            .map(|history| history.node.native_id.as_str())
            .collect::<BTreeSet<_>>(),
        BTreeSet::from(["loaded-only", "parent", "child"])
    );
    assert!(snapshot.graph.facts.iter().any(|fact| {
        matches!(
            fact.event,
            crate::session_evidence::execution::FactKind::Relation {
                relation: crate::session_evidence::types::EdgeKind::Spawn
            }
        ) && fact
            .node
            .as_ref()
            .is_some_and(|node| node.native_id == "child")
            && fact
                .related_node
                .as_ref()
                .is_some_and(|node| node.native_id == "parent")
    }));
    assert!(resumed.service.state.lock().await.sessions.is_empty());
    assert!(resumed.service.state.lock().await.pending.is_empty());
    assert!(tap.exists());
    let replay = finish_recovery(&resumed.service).await?;
    assert_eq!(snapshot.graph.facts, replay.graph.facts);
    Ok(())
}

#[tokio::test]
async fn restart_recovers_registered_profiles_and_consumed_hooks_without_live_queries() -> Result<()>
{
    let directory = tempfile::tempdir()?;
    let native = directory.path().join("profile-b");
    let mut old = claude_service(directory.path()).await?;
    stop_worker(&mut old).await?;
    let params = json!({"sessionId":"loaded-root","cwd":directory.path(),
        "_meta":{"claudeCode":{"options":{"env":{"CLAUDE_CONFIG_DIR":native}}}}});
    old.service
        .observe(observation(
            "load",
            "session/load",
            "request",
            params.clone(),
        ))
        .await?;
    old.service
        .prepare_session_request("load", "session/load", params)
        .await?;
    // Load may return no id; recovery must use the prepared operation in this
    // profile's journal, even though its request was in the default journal.
    old.service
        .observe(observation("load", "session/load", "response", json!({})))
        .await?;
    let namespace = old
        .service
        .state
        .lock()
        .await
        .sessions
        .get("loaded-root")
        .cloned()
        .context("prepared profile")?;
    let context = old
        .service
        .state
        .lock()
        .await
        .roots
        .get(&namespace)
        .cloned()
        .context("registered profile")?;
    for id in ["loaded-root", "consumed-root", "late-root", "unrelated"] {
        transcript(&native, id).await?;
    }
    native_start(&context.spool, "loaded-root").await?;
    let consumed = context.spool.join("hook-consumed.jsonl");
    write_rows(
        &consumed,
        vec![json!({"payload":{
            "hook_event_name":"SessionStart","session_id":"consumed-root"
        }})],
    )
    .await?;
    let mut found = BTreeSet::new();
    old.service
        .reconcile_spool_file(
            context.collector.root(),
            &consumed,
            true,
            &mut found,
            &mut BTreeSet::new(),
        )
        .await?;
    assert_eq!(found.len(), 1);
    assert!(!consumed.exists());
    // The native file exists, but no reconciliation or live node update ran.
    // This identity must be found through the now-deleted hook's stored source.
    assert!(
        !old.service
            .state
            .lock()
            .await
            .nodes
            .iter()
            .any(|node| node.native_id == "consumed-root")
    );
    let late = context.spool.join("hook-late.jsonl");
    write_rows(
        &late,
        vec![json!({"payload":{
            "hook_event_name":"SessionStart","session_id":"late-root"
        }})],
    )
    .await?;
    for index in 0..20 {
        old.service
            .store
            .register(SourceDescriptor {
                namespace: "different-harness".into(),
                harness: Harness::Codex,
                format: SourceFormat::CodexRollout,
                locator: format!("padding-{index}"),
                node: None,
            })
            .await?;
    }
    let journal_source = old
        .service
        .store
        .sources(None, 128)
        .await?
        .into_iter()
        .find(|source| {
            source.descriptor.namespace == namespace
                && source.descriptor.format == SourceFormat::Acp
        })
        .context("old journal source")?;
    drop(context);
    drop(old);

    let mut resumed = claude_service(directory.path()).await?;
    stop_worker(&mut resumed).await?;
    let first = resumed.service.reconcile().await?;
    assert!(first.gaps.contains("native_recovery_backlog"));
    let snapshot = finish_recovery(&resumed.service).await?;
    assert_eq!(
        snapshot.gaps,
        if cfg!(unix) {
            BTreeSet::new()
        } else {
            BTreeSet::from(["native_process_capture_unavailable".into()])
        }
    );
    assert_eq!(
        snapshot
            .histories
            .iter()
            .map(|history| history.node.native_id.as_str())
            .collect::<BTreeSet<_>>(),
        BTreeSet::from(["loaded-root", "consumed-root", "late-root"])
    );
    assert!(
        snapshot
            .histories
            .iter()
            .all(|history| history.node.namespace == namespace)
    );
    assert!(
        late.exists(),
        "recovery must not retire another controller's spool"
    );
    let source = resumed
        .service
        .store
        .source(&journal_source.id)
        .await?
        .context("retained journal")?;
    assert_eq!(
        source, journal_source,
        "recovery cannot append to an old controller journal"
    );
    {
        let state = resumed.service.state.lock().await;
        assert!(state.sessions.is_empty());
        assert!(state.loaded.is_empty());
        assert!(state.pending.is_empty());
        assert!(state.uncertain_queries.is_empty());
        assert!(
            !state.roots.contains_key(&namespace),
            "historical profile is not live configuration"
        );
        assert!(
            state.nodes.is_empty(),
            "historical candidates must not occupy the live node budget"
        );
    }
    let replay = finish_recovery(&resumed.service).await?;
    assert_eq!(snapshot.graph.facts, replay.graph.facts);
    // A missing historical directory is a visible gap, but does not destroy
    // identities that are already backed by durable controller/hook records.
    tokio::fs::remove_dir_all(late.parent().context("old scoped spool")?).await?;
    drop(resumed);
    let mut reopened = claude_service(directory.path()).await?;
    stop_worker(&mut reopened).await?;
    let recovered = finish_recovery(&reopened.service).await?;
    assert!(recovered.gaps.contains("native_recovery_spool_failed"));
    assert_eq!(
        recovered
            .histories
            .iter()
            .map(|history| history.node.clone())
            .collect::<BTreeSet<_>>(),
        snapshot
            .histories
            .iter()
            .map(|history| history.node.clone())
            .collect()
    );
    Ok(())
}

#[tokio::test]
async fn retained_spool_pages_make_progress_and_keep_prior_page_failures_visible() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let mut handle = claude_service(directory.path()).await?;
    stop_worker(&mut handle).await?;
    let service = &handle.service;
    let broken = service.spool.join("hook-000.jsonl");
    tokio::fs::write(&broken, b"invalid JSON\n").await?;
    for index in 1..130 {
        write_rows(
            &service.spool.join(format!("hook-{index:03}.jsonl")),
            vec![json!({
                "payload":{"hook_event_name":"SessionStart","session_id":format!("root-{index}")}
            })],
        )
        .await?;
    }
    let mut found = BTreeSet::new();
    let mut gaps = BTreeSet::new();
    service
        .reconcile_spool(
            service.collector.root(),
            &service.spool,
            false,
            &mut found,
            &mut gaps,
        )
        .await?;
    assert_eq!(found.len(), 127);
    assert!(gaps.contains("native_spool_backlog"));
    assert!(gaps.contains("native_spool_invalid_json"));
    gaps.clear();
    service
        .reconcile_spool(
            service.collector.root(),
            &service.spool,
            false,
            &mut found,
            &mut gaps,
        )
        .await?;
    assert_eq!(
        found.len(),
        129,
        "retained first-page files cannot starve later files"
    );
    assert!(
        gaps.contains("native_spool_invalid_json"),
        "an unvisited failed page is still incomplete"
    );
    write_rows(
        &broken,
        vec![json!({"payload":{
            "hook_event_name":"SessionStart","session_id":"repaired"
        }})],
    )
    .await?;
    for _ in 0..2 {
        gaps.clear();
        service
            .reconcile_spool(
                service.collector.root(),
                &service.spool,
                false,
                &mut found,
                &mut gaps,
            )
            .await?;
    }
    assert_eq!(found.len(), 130);
    assert!(gaps.is_empty(), "{:?}", gaps);
    assert!(broken.exists());
    Ok(())
}

#[tokio::test]
async fn recovery_rejects_foreign_and_misbound_root_registrations() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let mut handle = claude_service(directory.path()).await?;
    stop_worker(&mut handle).await?;
    let service = &handle.service;
    let root = service.collector.root();
    let controllers = service.spool.parent().context("controller root")?;
    let outside = directory.path().join("outside");
    tokio::fs::create_dir_all(&outside).await?;
    write_rows(
        &outside.join("hook-forged.jsonl"),
        vec![json!({"payload":{
            "hook_event_name":"SessionStart","session_id":"forged"
        }})],
    )
    .await?;
    for (case, payload) in [
        (
            "outside",
            json!({"native_root":root.directory,"namespace":root.namespace,
            "harness":root.harness,"spool":outside}),
        ),
        (
            "namespace",
            json!({"native_root":root.directory,"namespace":"wrong",
            "harness":root.harness,"spool":outside}),
        ),
        (
            "harness",
            json!({"native_root":root.directory,"namespace":root.namespace,
            "harness":"codex","spool":outside}),
        ),
    ] {
        let id = uuid::Uuid::new_v4();
        let journal = Journal::new(
            service.store.clone(),
            SourceDescriptor {
                namespace: root.namespace.clone(),
                harness: root.harness,
                format: SourceFormat::Acp,
                locator: format!("controller:{id}"),
                node: None,
            },
            format!("fixture-{case}"),
        )
        .await?;
        journal
            .append(json!({"method":"controller/started","phase":"metadata","payload":payload}))
            .await?;
    }
    let db = crate::db::connect(&crate::db::anchor_url(
        "sqlite:evidence.db?mode=rwc",
        &directory.path().join("router"),
    ))
    .await?;
    let foreign_store = EvidenceStore::new(db, "foreign-owner")?;
    let id = uuid::Uuid::new_v4();
    let foreign_spool = controllers.join(id.to_string());
    private_directory(&foreign_spool).await?;
    let foreign = Journal::new(
        foreign_store,
        SourceDescriptor {
            namespace: root.namespace.clone(),
            harness: root.harness,
            format: SourceFormat::Acp,
            locator: format!("controller:{id}"),
            node: None,
        },
        "fixture".into(),
    )
    .await?;
    foreign.append(json!({"method":"controller/started","phase":"metadata","payload":{
        "native_root":root.directory,"namespace":root.namespace,"harness":root.harness,"spool":foreign_spool
    }})).await?;
    write_rows(
        &foreign_spool.join("hook-foreign.jsonl"),
        vec![json!({"payload":{
            "hook_event_name":"SessionStart","session_id":"foreign"
        }})],
    )
    .await?;
    let snapshot = finish_recovery(service).await?;
    assert!(snapshot.histories.is_empty());
    assert!(snapshot.gaps.contains("native_recovery_root_invalid"));
    assert!(service.recovery.lock().await.roots.is_empty());
    assert!(
        service
            .store
            .sources(None, 128)
            .await?
            .iter()
            .all(|source| source.descriptor.format == SourceFormat::Acp)
    );
    Ok(())
}

#[test]
fn acp_recovery_requires_correlated_operations_and_leaves_unscoped_sdk_to_binding() -> Result<()> {
    let root = NativeRoot {
        harness: Harness::Codex,
        namespace: "fixture".into(),
        directory: "/fixture/projects".into(),
    };
    let mut pending = BTreeMap::new();
    let mut gaps = BTreeSet::new();
    replay_acp(
        &json!({"operation_id":"load", "method":"session/load","phase":"prepared",
        "payload":{"sessionId":"root","namespace":root.namespace,"native_root":root.directory}}),
        &root,
        &mut pending,
        &mut gaps,
    )?;
    let unrelated = replay_acp(
        &json!({"operation_id":"different", "method":"session/load",
        "phase":"response","native_scope":"operation","payload":{}}),
        &root,
        &mut pending,
        &mut gaps,
    )?;
    assert!(unrelated.is_empty());
    assert!(gaps.contains("native_recovery_session_id_missing"));
    let unresolved = replay_acp(
        &json!({"operation_id":"load", "method":"session/load",
        "phase":"response","native_scope":"unresolved","payload":{"sessionId":"root"}}),
        &root,
        &mut pending,
        &mut gaps,
    )?;
    assert!(unresolved.is_empty());
    assert!(pending.is_empty());
    let notification = replay_acp(
        &json!({"operation_id":"notice", "method":"_claude/sdkMessage",
        "phase":"notification","native_scope":"controller",
        "payload":{"sessionId":"synthetic","message":{"type":"system","subtype":"init"}}}),
        &root,
        &mut pending,
        &mut gaps,
    )?;
    assert!(notification.is_empty());
    // SDK binding inspects the owned source and raw record separately; this
    // lifecycle replay cannot promote the envelope's synthetic ACP id.
    assert!(!gaps.contains("native_sdk_scope_unresolved"));
    Ok(())
}

#[tokio::test]
async fn cancelled_spool_result_is_recovered_after_replay_and_hook_retirement() -> Result<()> {
    for retire in [false, true] {
        let directory = tempfile::tempdir()?;
        let mut handle = claude_service(directory.path()).await?;
        stop_worker(&mut handle).await?;
        transcript(&directory.path().join("default"), "root").await?;
        let service = &handle.service;
        let hook = service.spool.join("hook-cancelled.jsonl");
        write_rows(
            &hook,
            vec![json!({"payload":{
                "hook_event_name":"SessionStart","session_id":"root"
            }})],
        )
        .await?;
        let mut temporary = BTreeSet::new();
        service
            .reconcile_spool_file(
                service.collector.root(),
                &hook,
                retire,
                &mut temporary,
                &mut BTreeSet::new(),
            )
            .await?;
        assert_eq!(temporary.len(), 1);
        assert_eq!(hook.exists(), !retire);
        assert!(service.state.lock().await.nodes.is_empty());
        // Drop the caller's not-yet-published result at the cancellation window.
        // Retained files have advanced replay cursors; retired hooks are gone.
        drop(temporary);
        let snapshot = service.reconcile().await?;
        assert!(
            snapshot
                .histories
                .iter()
                .any(|history| history.node.native_id == "root")
        );
        assert!(
            service
                .state
                .lock()
                .await
                .nodes
                .iter()
                .any(|node| node.native_id == "root")
        );
    }
    Ok(())
}

#[tokio::test]
async fn later_controller_sweeps_find_new_records_already_retired_by_their_origin() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let mut origin = claude_service(directory.path()).await?;
    stop_worker(&mut origin).await?;
    let mut follower = claude_service(directory.path()).await?;
    stop_worker(&mut follower).await?;
    assert!(
        finish_recovery(&follower.service)
            .await?
            .histories
            .is_empty()
    );
    transcript(&directory.path().join("default"), "after-sweep").await?;
    let hook = origin.service.spool.join("hook-after-sweep.jsonl");
    write_rows(
        &hook,
        vec![json!({"payload":{
            "hook_event_name":"SessionStart","session_id":"after-sweep"
        }})],
    )
    .await?;
    origin
        .service
        .reconcile_spool_file(
            origin.service.collector.root(),
            &hook,
            true,
            &mut BTreeSet::new(),
            &mut BTreeSet::new(),
        )
        .await?;
    assert!(!hook.exists());
    let snapshot = finish_recovery(&follower.service).await?;
    assert!(
        snapshot
            .histories
            .iter()
            .any(|history| history.node.native_id == "after-sweep")
    );
    assert!(follower.service.state.lock().await.sessions.is_empty());
    Ok(())
}

#[tokio::test]
async fn corrupt_registry_and_recovery_root_limit_do_not_block_live_collection() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let mut origin = claude_service(directory.path()).await?;
    stop_worker(&mut origin).await?;
    let registered = origin
        .service
        .store
        .sources(None, 128)
        .await?
        .into_iter()
        .find(|source| source.descriptor.format == SourceFormat::Acp)
        .context("origin root")?;
    let broken = origin
        .service
        .store
        .register(SourceDescriptor {
            namespace: "broken".into(),
            harness: Harness::Codex,
            format: SourceFormat::CodexRollout,
            locator: "broken-registry".into(),
            node: None,
        })
        .await?;
    let db = crate::db::connect(&crate::db::anchor_url(
        "sqlite:evidence.db?mode=rwc",
        &directory.path().join("router"),
    ))
    .await?;
    db.execute(Statement::from_sql_and_values(
        DbBackend::Sqlite,
        "UPDATE native_evidence_sources SET descriptor_json = ? WHERE id = ?",
        ["{".into(), broken.id.into()],
    ))
    .await?;
    let mut live = claude_service(directory.path()).await?;
    stop_worker(&mut live).await?;
    let recovered_root = live.service.recovered_root(&registered).await?;
    {
        // Exercise a full recovery cache without creating 1024 OS processes.
        let mut recovery = live.service.recovery.lock().await;
        for index in 0..MAX_GRAPH_ITEMS {
            recovery
                .roots
                .insert(format!("cached-{index}"), recovered_root.clone());
        }
    }
    transcript(&directory.path().join("default"), "live").await?;
    let hook = live.service.spool.join("hook-live.jsonl");
    write_rows(
        &hook,
        vec![json!({"payload":{
            "hook_event_name":"SessionStart","session_id":"live"
        }})],
    )
    .await?;
    let snapshot = live.service.reconcile().await?;
    assert!(snapshot.gaps.contains("native_recovery_registry_invalid"));
    assert!(snapshot.gaps.contains("native_recovery_root_limit"));
    assert!(
        !hook.exists(),
        "live hook collection and retirement must still run"
    );
    assert!(
        snapshot
            .histories
            .iter()
            .any(|history| history.node.native_id == "live" && history.projection.is_some())
    );
    assert!(
        live.service
            .state
            .lock()
            .await
            .nodes
            .iter()
            .any(|node| node.native_id == "live")
    );
    Ok(())
}

#[tokio::test]
async fn cancelling_database_replay_preserves_the_previous_operation_checkpoint() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let mut origin = claude_service(directory.path()).await?;
    stop_worker(&mut origin).await?;
    origin
        .service
        .observe(observation(
            "new",
            "session/new",
            "response",
            json!({"sessionId":"root"}),
        ))
        .await?;
    origin
        .service
        .observe(observation(
            "runtime",
            "_claude/sdkMessage",
            "notification",
            json!({
                "sessionId":"root","message":{"type":"system","subtype":"init","session_id":"root",
                    "claude_code_version":"2.1.220","capabilities":["msg_lifecycle_v1"]}
            }),
        ))
        .await?;
    let source = origin
        .service
        .store
        .sources(None, 128)
        .await?
        .into_iter()
        .find(|source| source.descriptor.format == SourceFormat::Acp)
        .context("origin journal")?;
    let mut follower = claude_service(directory.path()).await?;
    stop_worker(&mut follower).await?;
    let root = follower.service.recovered_root(&source).await?;
    let pending = BTreeMap::from([(
        "kept-operation".into(),
        ("session/load".into(), "kept-session".into()),
    )]);
    let mut recovery = RecoveryState {
        phase: Phase::Records,
        roots: BTreeMap::from([(source.id.clone(), root)]),
        replay: Some(Replay {
            source: source.clone(),
            next: 0,
            pending: pending.clone(),
        }),
        ..RecoveryState::default()
    };
    // Hold a real SQLite writer so replay reaches an actual pending database
    // await. Reads can proceed, but the derived-index insert cannot finish.
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
            [source.id.clone().into()],
        ))
        .await?;
    let cancelled = tokio::time::timeout(
        Duration::from_millis(30),
        follower.service.recover_records(&mut recovery),
    )
    .await;
    transaction.rollback().await?;
    assert!(
        cancelled.is_err(),
        "the fixture must cancel while database replay is pending"
    );
    let checkpoint = recovery
        .replay
        .as_ref()
        .context("cancelled replay checkpoint")?;
    assert_eq!(checkpoint.next, 0);
    assert_eq!(checkpoint.pending, pending);
    follower.service.recover_records(&mut recovery).await?;
    assert!(recovery.nodes.iter().any(|node| node.native_id == "root"));
    assert!(!recovery.gaps.contains("native_recovery_source_failed"));
    Ok(())
}

#[tokio::test]
async fn corrupt_opaque_cursor_at_a_full_page_boundary_does_not_stop_recovery() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let mut origin = claude_service(directory.path()).await?;
    stop_worker(&mut origin).await?;
    origin
        .service
        .observe(observation(
            "new",
            "session/new",
            "response",
            json!({"sessionId":"root"}),
        ))
        .await?;
    transcript(&directory.path().join("default"), "root").await?;
    native_start(&origin.service.spool, "root").await?;
    for index in 0..13 {
        origin
            .service
            .store
            .register(SourceDescriptor {
                namespace: "padding".into(),
                harness: Harness::Codex,
                format: SourceFormat::CodexRollout,
                locator: format!("padding-{index}"),
                node: None,
            })
            .await?;
    }
    let broken = origin
        .service
        .store
        .register(SourceDescriptor {
            namespace: "padding".into(),
            harness: Harness::Codex,
            format: SourceFormat::CodexRollout,
            locator: "broken-key".into(),
            node: None,
        })
        .await?;
    let db = crate::db::connect(&crate::db::anchor_url(
        "sqlite:evidence.db?mode=rwc",
        &directory.path().join("router"),
    ))
    .await?;
    let opaque = "z".repeat(513);
    db.execute(Statement::from_sql_and_values(
        DbBackend::Sqlite,
        "UPDATE native_evidence_sources SET id = ? WHERE id = ?",
        [opaque.clone().into(), broken.id.into()],
    ))
    .await?;
    let mut follower = claude_service(directory.path()).await?;
    stop_worker(&mut follower).await?;
    let first = follower.service.reconcile().await?;
    assert!(first.gaps.contains("native_recovery_registry_invalid"));
    assert_eq!(
        follower.service.recovery.lock().await.after.as_deref(),
        Some(opaque.as_str()),
        "the corrupt id must be the last row of a full inventory page"
    );
    let snapshot = finish_recovery(&follower.service).await?;
    assert!(snapshot.gaps.contains("native_recovery_registry_invalid"));
    assert!(
        snapshot
            .histories
            .iter()
            .any(|history| history.node.native_id == "root")
    );
    Ok(())
}

#[tokio::test]
async fn cli_processes_recover_early_sessions_and_reset_without_live_query_inference() -> Result<()>
{
    use crate::session_evidence::execution::FactKind;
    let directory = tempfile::tempdir()?;
    let native = directory.path().join("profile-cli");
    let mut origin = claude_service(directory.path()).await?;
    stop_worker(&mut origin).await?;
    let params = origin.service.prepare_session_request("new-cli", "session/new", json!({"cwd":directory.path(),"_meta":{"claudeCode":{"options":{"env":{"CLAUDE_CONFIG_DIR":native}}}}})).await?;
    let env = params
        .pointer("/_meta/claudeCode/options/env")
        .context("prepared env")?;
    let namespace = env[crate::session_evidence::claude_proxy::NAMESPACE_ENV]
        .as_str()
        .context("profile namespace")?
        .to_owned();
    let spool = PathBuf::from(
        env[crate::session_evidence::claude_proxy::SPOOL_ENV]
            .as_str()
            .context("spool")?,
    );
    for id in ["early-root", "after-reset"] {
        transcript(&native, id).await?;
    }
    let mut processes = BTreeSet::new();
    for _ in 0..2 {
        let process = uuid::Uuid::new_v4().to_string();
        processes.insert(process.clone());
        let mut rows = vec![
            json!({"method":"runtime/started","phase":"metadata"}),
            json!({"method":"runtime/message","payload":{"type":"system","subtype":"init","session_id":"early-root","claude_code_version":"2.1.257","capabilities":["msg_lifecycle_v1"]}}),
            json!({"method":"runtime/message","payload":{"type":"conversation_reset","session_id":"early-root","new_conversation_id":"after-reset","uuid":"reset"}}),
            json!({"method":"runtime/message","payload":{"type":"command_lifecycle","session_id":"after-reset","command_uuid":"declined-peer","state":"refused"}}),
            json!({"method":"runtime/stopped","phase":"metadata","clean":true,"exit_code":0}),
        ];
        for (sequence, row) in rows.iter_mut().enumerate() {
            row["process_id"] = json!(process);
            row["namespace"] = json!(namespace);
            row["scope_valid"] = json!(true);
            row["sequence"] = json!(sequence);
        }
        write_rows(&spool.join(format!("cli-{process}.jsonl")), rows).await?;
    }
    // No ACP lifecycle response has arrived. Native process facts independently
    // establish session identities, not a loaded Query or an application task.
    drop(origin);
    let mut resumed = claude_service(directory.path()).await?;
    stop_worker(&mut resumed).await?;
    let snapshot = finish_recovery(&resumed.service).await?;
    assert_eq!(
        snapshot
            .histories
            .iter()
            .map(|history| history.node.native_id.as_str())
            .collect::<BTreeSet<_>>(),
        BTreeSet::from(["early-root", "after-reset"])
    );
    assert_eq!(
        snapshot
            .graph
            .facts
            .iter()
            .filter_map(|fact| fact.process_id.clone())
            .collect::<BTreeSet<_>>(),
        processes
    );
    assert_eq!(
        snapshot
            .graph
            .facts
            .iter()
            .filter(|fact| matches!(fact.event, FactKind::ConversationReset { .. }))
            .count(),
        2
    );
    assert_eq!(snapshot.graph.facts.iter().filter(|fact| matches!(&fact.event, FactKind::NativeCommand { state, .. } if state == "refused")).count(), 2);
    assert!(snapshot.graph.gaps.is_empty(), "{:?}", snapshot.graph.gaps);
    assert!(snapshot.attempts.is_empty());
    assert!(resumed.service.state.lock().await.loaded.is_empty());
    assert!(resumed.service.state.lock().await.sessions.is_empty());
    let again = finish_recovery(&resumed.service).await?;
    assert_eq!(snapshot.graph.facts, again.graph.facts);
    assert_eq!(std::fs::read_dir(spool)?.count(), 2);
    Ok(())
}

#[tokio::test]
async fn invalid_cli_envelopes_never_publish_transcript_nodes_live_or_after_restart() -> Result<()>
{
    let directory = tempfile::tempdir()?;
    let mut origin = claude_service(directory.path()).await?;
    stop_worker(&mut origin).await?;
    let namespace = origin.service.collector.root().namespace.clone();
    for fault in [
        "wrong-process",
        "wrong-sequence",
        "wrong-profile",
        "unresolved",
    ] {
        let process = uuid::Uuid::new_v4().to_string();
        let mut row = json!({"method":"runtime/message","process_id":process,"namespace":namespace,"scope_valid":true,"sequence":0,
            "payload":{"type":"system","subtype":"init","session_id":fault,"claude_code_version":"2.1.257"}});
        match fault {
            "wrong-process" => row["process_id"] = json!(uuid::Uuid::new_v4().to_string()),
            "wrong-sequence" => row["sequence"] = json!(1),
            "wrong-profile" => row["namespace"] = json!("foreign"),
            _ => row["scope_valid"] = json!(false),
        }
        transcript(&directory.path().join("default"), fault).await?;
        write_rows(
            &origin.service.spool.join(format!("cli-{process}.jsonl")),
            vec![row],
        )
        .await?;
    }
    let snapshot = origin.service.reconcile().await?;
    assert!(snapshot.gaps.contains("native_lifecycle_invalid"));
    assert!(snapshot.gaps.contains("native_process_scope_unresolved"));
    assert!(snapshot.histories.is_empty());
    assert!(snapshot.graph.nodes.is_empty());
    assert!(
        origin
            .service
            .reconcile()
            .await?
            .gaps
            .contains("native_lifecycle_invalid")
    );
    drop(origin);
    let mut resumed = claude_service(directory.path()).await?;
    stop_worker(&mut resumed).await?;
    let snapshot = finish_recovery(&resumed.service).await?;
    assert!(snapshot.gaps.contains("native_lifecycle_invalid"));
    assert!(snapshot.gaps.contains("native_process_scope_unresolved"));
    assert!(snapshot.histories.is_empty());
    assert!(snapshot.graph.nodes.is_empty());
    Ok(())
}

#[tokio::test]
async fn unsupported_process_capture_gap_survives_refresh_and_controller_restart() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let mut env = HashMap::from([
        (
            "CLAUDE_CONFIG_DIR".into(),
            directory
                .path()
                .join("default")
                .to_string_lossy()
                .into_owned(),
        ),
        ("CLAUDE_CODE_EXECUTABLE".into(), "custom-cli.js".into()),
    ]);
    let mut origin = EvidenceHandle::open(EvidenceLaunch {
        home: &directory.path().join("router"),
        database_url: "sqlite:evidence.db?mode=rwc",
        identity: &ControllerIdentity::new(
            "claude-acp",
            "@agentclientprotocol/claude-agent-acp",
            "0.75.1",
        ),
        env: &mut env,
        strip_inherited_env: &[],
    })
    .await?
    .context("controller")?;
    stop_worker(&mut origin).await?;
    assert_eq!(env["CLAUDE_CODE_EXECUTABLE"], "custom-cli.js");
    for _ in 0..2 {
        assert!(
            origin
                .service
                .reconcile()
                .await?
                .gaps
                .contains("native_process_capture_unavailable")
        );
    }
    drop(origin);
    let mut resumed = claude_service(directory.path()).await?;
    stop_worker(&mut resumed).await?;
    assert!(
        finish_recovery(&resumed.service)
            .await?
            .gaps
            .contains("native_process_capture_unavailable")
    );
    Ok(())
}
