use super::*;

pub(super) async fn write_rows(path: &Path, rows: Vec<Value>) -> Result<()> {
    tokio::fs::create_dir_all(path.parent().context("fixture parent")?).await?;
    let mut bytes = vec![];
    for row in rows {
        serde_json::to_writer(&mut bytes, &row)?;
        bytes.push(b'\n');
    }
    tokio::fs::write(path, bytes).await?;
    Ok(())
}

pub(super) async fn claude_service(directory: &Path) -> Result<EvidenceHandle> {
    let mut env = HashMap::from([
        (
            "CLAUDE_CONFIG_DIR".into(),
            directory.join("default").to_string_lossy().into_owned(),
        ),
        (
            "CLAUDE_MODEL_CONFIG".into(),
            json!({"modelOverrides":{"sonnet":"fixture-model"},
            "availableModels":["sonnet"]})
            .to_string(),
        ),
    ]);
    EvidenceHandle::open(EvidenceLaunch {
        home: &directory.join("router"),
        database_url: "sqlite:evidence.db?mode=rwc",
        identity: &ControllerIdentity::new(
            "claude-acp",
            "@agentclientprotocol/claude-agent-acp",
            "0.70.0",
        ),
        env: &mut env,
        strip_inherited_env: &[],
    })
    .await?
    .context("Claude service")
}

pub(super) fn observation(
    operation: &str,
    method: &str,
    phase: &str,
    payload: Value,
) -> SessionObservation {
    SessionObservation {
        operation_id: operation.into(),
        method: method.into(),
        phase: phase.into(),
        payload,
    }
}

#[tokio::test]
async fn native_sdk_lifecycle_is_durable_in_the_confirmed_session_profile() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let mut handle = claude_service(directory.path()).await?;
    let service = &handle.service;
    service
        .observe(observation(
            "new",
            "session/new",
            "response",
            json!({"sessionId":"root"}),
        ))
        .await?;
    let namespace = service
        .state
        .lock()
        .await
        .sessions
        .get("root")
        .cloned()
        .context("root namespace")?;
    for (id, message) in [
        (
            "sdk-init",
            json!({"type":"system","subtype":"init","session_id":"root","uuid":"init","claude_code_version":"2.1.220","capabilities":["msg_lifecycle_v1"],"apiKeySource":"fixture-secret"}),
        ),
        (
            "sdk-task",
            json!({"type":"system","subtype":"task_started","session_id":"root","task_id":"shell-task","task_type":"local_bash","tool_use_id":"tool"}),
        ),
        (
            "sdk-background",
            json!({"type":"system","subtype":"background_tasks_changed","session_id":"root","tasks":[{"task_id":"shell-task","description":"private-description"}]}),
        ),
        (
            "sdk-idle",
            json!({"type":"system","subtype":"session_state_changed","session_id":"root","state":"idle"}),
        ),
    ] {
        let payload = service
            .notification_fields(
                super::super::claude_sdk::METHOD,
                &json!({"sessionId":"root","message":message}),
            )
            .context("selected lifecycle")?;
        service
            .observe(observation(
                id,
                super::super::claude_sdk::METHOD,
                "notification",
                payload,
            ))
            .await?;
    }
    let rows = journal_rows(service, &namespace).await?;
    let serialized = serde_json::to_string(&rows)?;
    assert!(!serialized.contains("fixture-secret"));
    assert!(!serialized.contains("private-description"));
    let root = NodeKey {
        namespace,
        harness: Harness::ClaudeCode,
        native_id: "root".into(),
        agent_id: None,
    };
    let graph = service
        .store
        .execution_graph(&BTreeSet::from([root.clone()]))
        .await?;
    assert_eq!(graph.nodes, BTreeSet::from([root]));
    assert!(graph.facts.iter().any(|fact| matches!(&fact.event, super::super::execution::FactKind::Runtime { version, .. } if version == "2.1.220")));
    assert!(graph.facts.iter().any(|fact| matches!(&fact.event, super::super::execution::FactKind::SessionState { state } if state == "idle")));
    assert!(!graph.facts.iter().any(|fact| matches!(
        fact.event,
        super::super::execution::FactKind::RunFinished { .. }
    )));
    handle.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn native_notifications_during_query_rebuild_do_not_use_the_old_profile() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let mut handle = claude_service(directory.path()).await?;
    let service = &handle.service;
    let first_cwd = directory.path().join("first");
    let second_cwd = directory.path().join("second");
    let params = |cwd: &Path| {
        json!({"sessionId":"root","cwd":cwd,"mcpServers":[],
        "_meta":{"claudeCode":{"options":{"env":{"CLAUDE_CONFIG_DIR":"profile"}}}}})
    };
    service
        .observe(observation(
            "new",
            "session/new",
            "request",
            json!({"cwd":first_cwd}),
        ))
        .await?;
    service
        .prepare_session_request("new", "session/new", params(&first_cwd))
        .await?;
    service
        .observe(observation(
            "new",
            "session/new",
            "response",
            json!({"sessionId":"root"}),
        ))
        .await?;
    let first = service
        .state
        .lock()
        .await
        .sessions
        .get("root")
        .cloned()
        .context("first namespace")?;
    service
        .observe(observation(
            "load",
            "session/load",
            "request",
            json!({"sessionId":"root","cwd":second_cwd}),
        ))
        .await?;
    service
        .prepare_session_request("load", "session/load", params(&second_cwd))
        .await?;
    let second = service
        .state
        .lock()
        .await
        .pending
        .get("load")
        .map(|scope| scope.namespace.clone())
        .context("second namespace")?;
    assert_ne!(first, second);
    for (id, message) in [
        (
            "during-init",
            json!({"type":"system","subtype":"init","session_id":"root","claude_code_version":"2.1.220"}),
        ),
        (
            "during-task",
            json!({"type":"system","subtype":"task_started","session_id":"root","task_id":"task"}),
        ),
        (
            "during-result",
            json!({"type":"result","subtype":"success","session_id":"root","uuid":"result","is_error":false}),
        ),
    ] {
        let payload = service
            .notification_fields(
                super::super::claude_sdk::METHOD,
                &json!({"sessionId":"root","message":message}),
            )
            .context("native fields")?;
        service
            .observe(observation(
                id,
                super::super::claude_sdk::METHOD,
                "notification",
                payload,
            ))
            .await?;
    }
    let raw = journal_rows(service, &service.collector.root().namespace).await?;
    let during: Vec<_> = raw
        .iter()
        .filter(|row| {
            row["operation_id"]
                .as_str()
                .is_some_and(|id| id.starts_with("during-"))
        })
        .collect();
    assert_eq!(during.len(), 3);
    assert!(during.iter().all(|row| row["native_scope"] == "unresolved"));
    for namespace in [&first, &second] {
        let node = NodeKey {
            namespace: namespace.clone(),
            harness: Harness::ClaudeCode,
            native_id: "root".into(),
            agent_id: None,
        };
        assert!(service.store.node_facts(&node, None, 100).await?.is_empty());
    }
    service
        .observe(observation(
            "load",
            "session/load",
            "response",
            json!({"sessionId":"root"}),
        ))
        .await?;
    assert_eq!(
        service.state.lock().await.sessions.get("root"),
        Some(&second)
    );
    handle.shutdown().await?;
    Ok(())
}

async fn journal_rows(service: &ControllerEvidence, namespace: &str) -> Result<Vec<Value>> {
    let sources = service.store.sources(None, 128).await?;
    let source = sources
        .iter()
        .find(|source| {
            source.descriptor.namespace == namespace
                && source.descriptor.format == SourceFormat::Acp
        })
        .context("profile journal")?;
    let mut rows = vec![];
    for start in (0..source.cursor.next_sequence).step_by(RECORD_PAGE_SIZE as usize) {
        rows.extend(
            service
                .store
                .records(&SourceRange {
                    source_id: source.id.clone(),
                    generation: source.cursor.generation.clone(),
                    start,
                    end: (start + RECORD_PAGE_SIZE).min(source.cursor.next_sequence),
                })
                .await?
                .into_iter()
                .map(|record| record.input.raw),
        );
    }
    Ok(rows)
}

#[tokio::test]
async fn concurrent_claude_profiles_bind_reverse_results_and_keep_hooks_scoped() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let mut handle = claude_service(directory.path()).await?;
    let service = &handle.service;
    let first_cwd = directory.path().join("first");
    let second_cwd = directory.path().join("second");
    for (cwd, id) in [(&first_cwd, "first"), (&second_cwd, "second")] {
        write_rows(
            &cwd.join(format!("profile/projects/work/{id}.jsonl")),
            vec![
                json!({"type":"user","sessionId":id,"uuid":"u","parentUuid":null,
                "version":"2.1.220","message":{"content":id}}),
            ],
        )
        .await?;
        service
            .observe(observation(
                id,
                "session/new",
                "request",
                json!({"cwd":cwd}),
            ))
            .await?;
    }
    let params = |cwd: &Path| {
        json!({"cwd":cwd,"mcpServers":[],"_meta":{"claudeCode":{"options":{
        "env":{"CLAUDE_CONFIG_DIR":"profile","PROVIDER_TOKEN":"fixture-secret"}}}}})
    };
    let (first, second) = tokio::join!(
        service.prepare_session_request("first", "session/new", params(&first_cwd)),
        service.prepare_session_request("second", "session/new", params(&second_cwd)),
    );
    let first = first?;
    let second = second?;
    assert_eq!(
        first.pointer("/_meta/claudeCode/options/settings/modelOverrides/sonnet"),
        Some(&json!("fixture-model"))
    );
    assert_eq!(
        first.pointer("/_meta/claudeCode/options/settings/availableModels"),
        Some(&json!(["sonnet"]))
    );
    let spool = |value: &Value| {
        value
            .pointer("/_meta/claudeCode/options/settings/hooks/Stop/0/hooks/0/args/2")
            .and_then(Value::as_str)
            .map(PathBuf::from)
            .context("scoped hook spool")
    };
    assert_ne!(spool(&first)?, spool(&second)?);
    for id in ["second", "first"] {
        service
            .observe(observation(
                id,
                "session/new",
                "response",
                json!({"sessionId":id}),
            ))
            .await?;
    }
    let namespaces = service.state.lock().await.sessions.clone();
    assert_ne!(namespaces.get("first"), namespaces.get("second"));
    assert!(service.state.lock().await.pending.is_empty());
    write_rows(&first_cwd.join("profile/projects/work/first/subagents/agent-child.jsonl"),
        vec![json!({"type":"user","sessionId":"first","agentId":"child","uuid":"cu","parentUuid":null,
            "version":"2.1.220","message":{"content":"child"}})]).await?;
    write_rows(
        &spool(&first)?.join("hook-child.jsonl"),
        vec![json!({"method":"SubagentStart",
        "payload":{"session_id":"first","agent_id":"child"}})],
    )
    .await?;
    service
        .observe(observation(
            "prompt",
            "session/prompt",
            "request",
            json!({"sessionId":"first","prompt":[]}),
        ))
        .await?;
    service
        .observe(observation(
            "prompt",
            "session/prompt",
            "response",
            json!({"stopReason":"end_turn"}),
        ))
        .await?;
    let missing_metadata = service.reconcile().await?;
    assert!(
        missing_metadata
            .gaps
            .contains("native_parent_agent_unknown")
    );
    assert_eq!(missing_metadata.histories.len(), 3);
    tokio::fs::write(
        first_cwd.join("profile/projects/work/first/subagents/agent-child.meta.json"),
        serde_json::to_vec(&json!({"parentAgentId":null,"toolUseId":"spawn-child"}))?,
    )
    .await?;
    let snapshot = service.reconcile().await?;
    assert!(snapshot.gaps.is_empty(), "{:?}", snapshot.gaps);
    assert_eq!(snapshot.histories.len(), 3);
    assert!(snapshot.graph.facts.iter().any(|fact| {
        matches!(
            fact.event,
            super::super::execution::FactKind::Relation {
                relation: super::super::types::EdgeKind::Spawn
            }
        ) && fact.node.as_ref().is_some_and(|node| {
            node.native_id == "first" && node.agent_id.as_deref() == Some("child")
        }) && fact
            .related_node
            .as_ref()
            .is_some_and(|node| node.native_id == "first" && node.agent_id.is_none())
    }));
    for history in snapshot.histories {
        assert_eq!(
            Some(&history.node.namespace),
            namespaces.get(&history.node.native_id)
        );
    }
    for id in ["first", "second"] {
        let rows = journal_rows(service, namespaces.get(id).context("namespace")?).await?;
        assert!(
            rows.iter()
                .any(|row| row["operation_id"] == id && row["phase"] == "response")
        );
        assert!(!serde_json::to_string(&rows)?.contains("fixture-secret"));
        assert_eq!(
            rows.iter()
                .filter(|row| row["operation_id"] == "prompt")
                .count(),
            if id == "first" { 2 } else { 0 }
        );
    }
    handle.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn claude_reuse_failure_and_close_follow_actual_query_lifetime() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let mut handle = claude_service(directory.path()).await?;
    let service = &handle.service;
    let cwd = directory.path();
    let original = json!({"cwd":cwd,"mcpServers":[],"_meta":{"claudeCode":{"options":{
        "env":{"CLAUDE_CONFIG_DIR":"profile-a"},"settings":{"env":{"EXISTING":"kept"}}}}}});
    service
        .observe(observation(
            "new",
            "session/new",
            "request",
            json!({"cwd":cwd}),
        ))
        .await?;
    let prepared = service
        .prepare_session_request("new", "session/new", original)
        .await?;
    assert!(
        prepared
            .pointer("/_meta/claudeCode/options/settings/modelOverrides")
            .is_none()
    );
    service
        .observe(observation(
            "new",
            "session/new",
            "response",
            json!({"sessionId":"session"}),
        ))
        .await?;
    let initial = service
        .state
        .lock()
        .await
        .sessions
        .get("session")
        .cloned()
        .context("initial scope")?;
    // The adapter may ignore new settings on reuse, even a file that does not
    // exist. The wrapper leaves it unconsumed; either Query uses profile A.
    let reuse = json!({"cwd":cwd,"sessionId":"session","_meta":{"claudeCode":{"options":{
        "env":{"CLAUDE_CONFIG_DIR":"profile-a"},"settings":"missing-settings.json"}}}});
    service
        .observe(observation(
            "reuse",
            "session/load",
            "request",
            json!({"cwd":cwd,"sessionId":"session"}),
        ))
        .await?;
    assert_eq!(
        service
            .prepare_session_request("reuse", "session/load", reuse.clone())
            .await?,
        reuse
    );
    assert!(
        service
            .observe(observation(
                "overlap",
                "session/resume",
                "request",
                json!({"sessionId":"session"})
            ))
            .await
            .is_err()
    );
    assert!(!service.state.lock().await.pending.contains_key("overlap"));
    service
        .observe(observation("reuse", "session/load", "response", json!({})))
        .await?;
    assert_eq!(
        service.state.lock().await.sessions.get("session"),
        Some(&initial)
    );
    assert!(service.state.lock().await.ambiguous_sessions.is_empty());
    let rebuild = json!({"cwd":cwd.join("missing"),"sessionId":"session"});
    service
        .observe(observation(
            "failed",
            "session/load",
            "request",
            rebuild.clone(),
        ))
        .await?;
    service
        .prepare_session_request("failed", "session/load", rebuild)
        .await?;
    service
        .observe(observation(
            "failed",
            "session/load",
            "response",
            json!({"error_code":-32603}),
        ))
        .await?;
    assert!(!service.state.lock().await.loaded.contains_key("session"));
    let retry = json!({"cwd":cwd,"sessionId":"session","_meta":{"claudeCode":{"options":{
        "env":{"CLAUDE_CONFIG_DIR":"profile-c"}}}}});
    service
        .observe(observation(
            "retry",
            "session/resume",
            "request",
            json!({"sessionId":"session"}),
        ))
        .await?;
    let retry = service
        .prepare_session_request("retry", "session/resume", retry)
        .await?;
    assert!(
        retry
            .pointer("/_meta/claudeCode/options/settings/hooks/Stop")
            .is_none()
    );
    service
        .observe(observation(
            "retry",
            "session/resume",
            "response",
            json!({}),
        ))
        .await?;
    assert!(!service.state.lock().await.sessions.contains_key("session"));
    assert!(
        service
            .state
            .lock()
            .await
            .uncertain_queries
            .contains("session")
    );
    assert!(
        service
            .reconcile()
            .await?
            .gaps
            .contains("native_query_scope_unknown")
    );
    let rows = journal_rows(service, &service.collector.root().namespace).await?;
    assert!(rows.iter().any(|row| row["operation_id"] == "retry"
        && row["phase"] == "response"
        && row["native_scope"] == "unresolved"));
    let mut opaque = retry.clone();
    opaque["_meta"]["claudeCode"]["options"]["settings"] = json!("missing-settings.json");
    service
        .observe(observation(
            "opaque",
            "session/load",
            "request",
            json!({"sessionId":"session"}),
        ))
        .await?;
    assert_eq!(
        service
            .prepare_session_request("opaque", "session/load", opaque.clone())
            .await?,
        opaque
    );
    service
        .observe(observation("opaque", "session/load", "response", json!({})))
        .await?;
    // Fork always creates a separate Query. An unknown parent's scope must not
    // replace the child's explicitly prepared profile with the default root.
    service
        .observe(observation(
            "fork",
            "session/fork",
            "request",
            json!({"sessionId":"session"}),
        ))
        .await?;
    service
        .prepare_session_request("fork", "session/fork", retry.clone())
        .await?;
    let fork_namespace = service
        .state
        .lock()
        .await
        .pending
        .get("fork")
        .context("fork scope")?
        .namespace
        .clone();
    service
        .observe(observation(
            "fork",
            "session/fork",
            "response",
            json!({"sessionId":"forked"}),
        ))
        .await?;
    assert_eq!(
        service.state.lock().await.sessions.get("forked"),
        Some(&fork_namespace)
    );
    assert_ne!(fork_namespace, service.collector.root().namespace);
    // Closing an active prompt is a supported cancellation path, not an
    // overlapping reconstruction. It blocks later work while it drains.
    service
        .observe(observation(
            "active",
            "session/prompt",
            "request",
            json!({"sessionId":"session"}),
        ))
        .await?;
    service
        .observe(observation(
            "close",
            "session/close",
            "request",
            json!({"sessionId":"session"}),
        ))
        .await?;
    assert!(
        service
            .observe(observation(
                "late",
                "session/prompt",
                "request",
                json!({"sessionId":"session"})
            ))
            .await
            .is_err()
    );
    service
        .observe(observation(
            "active",
            "session/prompt",
            "response",
            json!({"stopReason":"cancelled"}),
        ))
        .await?;
    service
        .observe(observation("close", "session/close", "response", json!({})))
        .await?;
    assert!(!service.state.lock().await.loaded.contains_key("session"));
    assert!(!service.state.lock().await.sessions.contains_key("session"));
    assert!(
        !service
            .state
            .lock()
            .await
            .uncertain_queries
            .contains("session")
    );
    service
        .observe(observation(
            "confirmed",
            "session/resume",
            "request",
            json!({"sessionId":"session"}),
        ))
        .await?;
    service
        .prepare_session_request("confirmed", "session/resume", retry)
        .await?;
    service
        .observe(observation(
            "confirmed",
            "session/resume",
            "response",
            json!({"sessionId":"session"}),
        ))
        .await?;
    assert!(service.state.lock().await.sessions.contains_key("session"));
    assert_ne!(
        service.state.lock().await.sessions.get("session"),
        Some(&initial)
    );
    assert_eq!(service.state.lock().await.nodes.len(), 3);
    assert!(service.state.lock().await.pending.is_empty());
    handle.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn idle_query_recreation_cannot_silently_rebind_a_profile() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let mut handle = claude_service(directory.path()).await?;
    let service = &handle.service;
    let params = json!({"cwd":directory.path(),"_meta":{"claudeCode":{"options":{
        "env":{"CLAUDE_CONFIG_DIR":"profile-a"}}}}});
    service
        .observe(observation(
            "new",
            "session/new",
            "request",
            json!({"cwd":directory.path()}),
        ))
        .await?;
    service
        .prepare_session_request("new", "session/new", params)
        .await?;
    service
        .observe(observation(
            "new",
            "session/new",
            "response",
            json!({"sessionId":"root"}),
        ))
        .await?;
    // No observed error distinguishes a still-loaded A from idle eviction and
    // recreation in B. The same session id proves neither native profile.
    let reload = json!({"cwd":directory.path(),"sessionId":"root","_meta":{"claudeCode":{"options":{
        "env":{"CLAUDE_CONFIG_DIR":"profile-b"},"settings":"ignored-if-reused.json"}}}});
    service
        .observe(observation(
            "reload",
            "session/resume",
            "request",
            json!({"sessionId":"root"}),
        ))
        .await?;
    assert_eq!(
        service
            .prepare_session_request("reload", "session/resume", reload.clone())
            .await?,
        reload
    );
    service
        .observe(observation(
            "reload",
            "session/resume",
            "response",
            json!({"sessionId":"root"}),
        ))
        .await?;
    assert!(!service.state.lock().await.sessions.contains_key("root"));
    assert!(
        service
            .state
            .lock()
            .await
            .uncertain_queries
            .contains("root")
    );
    assert_eq!(service.state.lock().await.nodes.len(), 1);
    assert!(
        service
            .reconcile()
            .await?
            .gaps
            .contains("native_query_scope_unknown")
    );
    handle.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn controller_service_collects_native_children_and_survives_resume() -> Result<()> {
    for harness in [Harness::Codex, Harness::ClaudeCode] {
        let directory = tempfile::tempdir()?;
        let native = directory.path().join("native");
        let home = directory.path().join("router");
        let (key, identity) = match harness {
            Harness::Codex => (
                "CODEX_HOME",
                ControllerIdentity::new("codex-acp", "@agentclientprotocol/codex-acp", "1.7.0"),
            ),
            Harness::ClaudeCode => (
                "CLAUDE_CONFIG_DIR",
                ControllerIdentity::new(
                    "claude-acp",
                    "@agentclientprotocol/claude-agent-acp",
                    "0.70.0",
                ),
            ),
        };
        let mut env = HashMap::from([
            (key.into(), native.to_string_lossy().into_owned()),
            ("CODEX_PATH".into(), "/fixture/codex".into()),
        ]);
        match harness {
            Harness::Codex => {
                for id in ["root", "child"] {
                    write_rows(&native.join(format!("sessions/rollout-{id}.jsonl")), vec![
                        json!({"type":"session_meta","payload":{"id":id,"cli_version":"0.148.0"}}),
                        json!({"type":"response_item","payload":{"type":"message","role":"user","content":"work"}}),
                    ]).await?;
                }
            }
            Harness::ClaudeCode => {
                write_rows(&native.join("projects/work/root.jsonl"), vec![json!({"type":"user","sessionId":"root","uuid":"u","parentUuid":null,"version":"2.1.220","message":{"content":"work"}})]).await?;
                write_rows(&native.join("projects/work/root/subagents/agent-child.jsonl"), vec![json!({"type":"user","sessionId":"root","agentId":"child","uuid":"cu","parentUuid":null,"version":"2.1.220","message":{"content":"child work"}})]).await?;
                tokio::fs::write(
                    native.join("projects/work/root/subagents/agent-child.meta.json"),
                    serde_json::to_vec(&json!({"parentAgentId":null,"toolUseId":"spawn-child"}))?,
                )
                .await?;
            }
        }
        let mut handle = EvidenceHandle::open(EvidenceLaunch {
            home: &home,
            database_url: "sqlite:evidence.db?mode=rwc",
            identity: &identity,
            env: &mut env,
            strip_inherited_env: &[],
        })
        .await?
        .context("evidence service")?;
        if harness == Harness::Codex {
            assert_eq!(
                env.get(super::super::codex_proxy::UPSTREAM_ENV)
                    .map(String::as_str),
                Some("/fixture/codex")
            );
            write_rows(&handle.service.spool.join("fixture.jsonl"), vec![
                json!({"method":"runtime/started","version":"0.148.0"}),
                json!({"direction":"server","phase":"notification","method":"item/completed","payload":{"threadId":"root","turnId":"turn-root","item":{"id":"spawn-child","type":"collabAgentToolCall","tool":"spawnAgent","status":"completed","senderThreadId":"root","receiverThreadIds":["child"],"agentsStates":{}}}}),
            ]).await?;
        }
        handle
            .service
            .observe(SessionObservation {
                operation_id: "new".into(),
                method: "session/new".into(),
                phase: "response".into(),
                payload: json!({"sessionId":"root"}),
            })
            .await?;
        let first = handle.service.reconcile().await?;
        assert_eq!(first.histories.len(), 2);
        assert!(first.gaps.is_empty(), "{:?}", first.gaps);
        assert_eq!(first.graph.nodes.len(), 2);
        assert!(first.graph.facts.iter().any(|fact| matches!(
            fact.event,
            super::super::execution::FactKind::Relation {
                relation: super::super::types::EdgeKind::Spawn
            }
        )));
        assert!(!first.graph.facts.iter().any(|fact| matches!(
            fact.event,
            super::super::execution::FactKind::RunFinished { .. }
        )));
        handle.service.observe(SessionObservation {
            operation_id: "child-view-update".into(), method: "session/update".into(), phase: "notification".into(),
            payload: json!({"sessionId":"child:generation:2","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"resumed child"}}}),
        }).await?;
        let sources: BTreeSet<_> = first
            .histories
            .iter()
            .filter_map(|history| {
                history
                    .source
                    .as_ref()
                    .map(|source| source.source.id.clone())
            })
            .collect();
        let replay = handle.service.reconcile().await?;
        assert_eq!(replay.histories.len(), first.histories.len());
        assert_eq!(
            replay
                .histories
                .iter()
                .filter_map(|history| history
                    .source
                    .as_ref()
                    .map(|source| source.source.id.clone()))
                .collect::<BTreeSet<_>>(),
            sources
        );
        if harness == Harness::Codex {
            let archives = native.join("archived_sessions");
            tokio::fs::create_dir_all(&archives).await?;
            tokio::fs::rename(
                native.join("sessions/rollout-root.jsonl"),
                archives.join("rollout-root.jsonl"),
            )
            .await?;
            let archived = handle.service.reconcile().await?;
            assert!(archived.gaps.is_empty(), "{:?}", archived.gaps);
            assert_eq!(
                archived
                    .histories
                    .iter()
                    .filter_map(|history| history
                        .source
                        .as_ref()
                        .map(|source| source.source.id.clone()))
                    .collect::<BTreeSet<_>>(),
                sources
            );
        }
        handle.shutdown().await?;
        // The same native root and source identity survives another controller.
        env.insert("CODEX_PATH".into(), "/fixture/codex".into());
        let mut resumed = EvidenceHandle::open(EvidenceLaunch {
            home: &home,
            database_url: "sqlite:evidence.db?mode=rwc",
            identity: &identity,
            env: &mut env,
            strip_inherited_env: &[],
        })
        .await?
        .context("resumed service")?;
        resumed
            .service
            .observe(SessionObservation {
                operation_id: "resume".into(),
                method: "session/resume".into(),
                phase: "response".into(),
                payload: json!({"sessionId":"root"}),
            })
            .await?;
        let snapshot = resumed.service.reconcile().await?;
        let root = snapshot
            .histories
            .iter()
            .find(|history| history.node.native_id == "root" && history.node.agent_id.is_none())
            .context("resumed root")?;
        assert!(sources.contains(&root.source.as_ref().context("root source")?.source.id));
        resumed.shutdown().await?;
    }
    Ok(())
}

#[test]
fn explicit_native_roots_override_inherited_user_home() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let env = HashMap::from([
        (
            "HOME".into(),
            directory
                .path()
                .join("unused")
                .to_string_lossy()
                .into_owned(),
        ),
        (
            "CODEX_HOME".into(),
            directory
                .path()
                .join("codex")
                .to_string_lossy()
                .into_owned(),
        ),
        (
            "CLAUDE_CONFIG_DIR".into(),
            directory
                .path()
                .join("claude")
                .to_string_lossy()
                .into_owned(),
        ),
    ]);
    assert_eq!(
        native_root(Harness::Codex, &env, &[])?.directory,
        std::fs::canonicalize(directory.path())?.join("codex/sessions")
    );
    assert_eq!(
        native_root(Harness::ClaudeCode, &env, &[])?.directory,
        std::fs::canonicalize(directory.path())?.join("claude/projects")
    );
    Ok(())
}

#[cfg(unix)]
#[test]
fn creating_a_native_root_beneath_a_symlink_keeps_its_namespace() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let real = directory.path().join("real");
    std::fs::create_dir(&real)?;
    let alias = directory.path().join("alias");
    std::os::unix::fs::symlink(&real, &alias)?;
    let env = HashMap::from([(
        "CODEX_HOME".into(),
        alias.join("new-profile").to_string_lossy().into_owned(),
    )]);
    let before = native_root(Harness::Codex, &env, &[])?;
    std::fs::create_dir_all(real.join("new-profile/sessions"))?;
    let after = native_root(Harness::Codex, &env, &[])?;
    assert_eq!(before.directory, after.directory);
    assert_eq!(before.namespace, after.namespace);
    Ok(())
}
