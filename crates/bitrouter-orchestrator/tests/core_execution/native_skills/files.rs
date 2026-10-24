use super::*;
use bitrouter_ai::types::Role;
use bitrouter_orchestrator::core::skills::files::TOOL;
use bitrouter_sdk::language_model::native::{InputTokenCounting, NativeInputCount};

struct FileCounter {
    mock: MockExecutor,
    counts: Mutex<Vec<Prompt>>,
    generated: Mutex<Vec<Prompt>>,
}

#[async_trait]
impl Executor for FileCounter {
    async fn count_input_tokens(
        &self,
        _: &RoutingTarget,
        prompt: &Prompt,
        _: &PipelineContext,
    ) -> bitrouter_sdk::Result<NativeInputCount> {
        self.counts.lock().await.push(prompt.clone());
        let bytes = serde_json::to_vec(prompt)
            .map_err(|error| bitrouter_sdk::BitrouterError::internal(error.to_string()))?;
        // Scripted provider counts prove admission of the full prepared input;
        // these numbers are not byte-to-token estimates.
        Ok(NativeInputCount::Counted {
            input_tokens: if String::from_utf8_lossy(&bytes).contains("support bytes") {
                1000
            } else {
                100
            },
            request_sha256: sha256(&bytes),
            source: "fixture_provider_count".into(),
        })
    }
    async fn execute(
        &self,
        target: &RoutingTarget,
        prompt: &Prompt,
        context: &PipelineContext,
    ) -> bitrouter_sdk::Result<ExecutionResult> {
        self.generated.lock().await.push(prompt.clone());
        self.mock.execute(target, prompt, context).await
    }
    async fn execute_stream(
        &self,
        _: &RoutingTarget,
        _: &Prompt,
        _: &PipelineContext,
    ) -> bitrouter_sdk::Result<StreamPartStream> {
        Err(bitrouter_sdk::BitrouterError::internal("unexpected stream"))
    }
}

async fn read_call(host: &NativeSkills, reference: &str, id: &str) -> Result<Content, CoreError> {
    let source = host
        .catalog
        .lock()
        .await
        .sources()
        .into_iter()
        .next()
        .ok_or_else(|| {
            CoreError::rejected(ErrorCode::ArtifactUnavailable, "fixture source absent")
        })?;
    Ok(core_call(
        TOOL,
        json!({"skill_id":source.identity()?,"reference":reference}),
        id,
    ))
}

async fn approve(session: &CoreSession, host: &NativeSkills) -> TestResult {
    session.drive().await?;
    assert_eq!(host.reads.load(Ordering::SeqCst), 0);
    assert_eq!(host.file_reads.load(Ordering::SeqCst), 0);
    let request = host
        .consent_requests
        .lock()
        .await
        .first()
        .cloned()
        .ok_or("consent")?;
    session
        .answer_skill_consent("file-consent", request.answer(true))
        .await?;
    Ok(())
}

#[tokio::test]
async fn supporting_file_is_lazy_verified_user_data_and_does_not_leak_to_the_next_turn()
-> TestResult {
    let host =
        Arc::new(NativeSkills::with_mcp(Arc::new(Harness::new(None, None)), "FILE_PARENT").await?);
    let read = read_call(&host, "references/check.md", "read-support").await?;
    let (session, executor, _) = setup_features(
        host.clone(),
        vec![
            output(vec![read]),
            output(vec![text("done")]),
            output(vec![text("next")]),
        ],
        false,
        true,
    )
    .await?;
    session
        .start(
            "file-start",
            session.head().await.state_revision,
            required_mcp_task(),
        )
        .await?;
    approve(&session, &host).await?;
    let snapshot = session.drive().await?;
    assert_eq!(
        snapshot.run.as_ref().map(|run| run.status),
        Some(RunStatus::Completed)
    );
    assert_eq!(host.reads.load(Ordering::SeqCst), 1);
    assert_eq!(host.file_reads.load(Ordering::SeqCst), 1);
    let selection = root_selection(&snapshot).ok_or("selection")?;
    let candidate = &selection.candidates[0];
    assert_eq!(
        candidate.files["skill://review/references/check.md"],
        "support bytes\n"
    );
    let prompts = executor.prompts.lock().await;
    assert_eq!(prompts.len(), 2);
    assert!(!serde_json::to_string(&prompts[0])?.contains("support bytes"));
    assert!(
        !prompts[1]
            .system
            .as_deref()
            .unwrap_or_default()
            .contains("support bytes")
    );
    let data = bitrouter_orchestrator::core::skills::files::text(
        candidate,
        "skill://review/references/check.md",
        "support bytes\n",
    )?;
    assert!(prompts[1].messages.iter().any(|message| {
        message.role == Role::User
            && message
                .content
                .iter()
                .any(|content| matches!(content, Content::Text{text,..} if text == &data))
    }));
    assert!(
        !prompts[1]
            .messages
            .iter()
            .filter(|message| message.role != Role::User)
            .any(|message| serde_json::to_string(message)
                .is_ok_and(|value| value.contains("support bytes")))
    );
    let parent_id = candidate.source.identity()?;
    assert!(
        snapshot
            .context_store
            .evidence
            .values()
            .any(|block| block.protected
                && block.source.skill_binding.as_ref() == Some(&parent_id)
                && block
                    .messages
                    .iter()
                    .any(|message| message.content.iter().any(
                        |content| matches!(content, Content::Text{text,..} if text == &data)
                    )))
    );
    assert!(
        !prompts[1]
            .tools
            .iter()
            .any(|tool| matches!(tool, Tool::Function{name,..} if name == "shell"))
    );
    let explained = bitrouter_orchestrator::core::skills::explain::capture(
        &snapshot,
        &snapshot.run.as_ref().ok_or("run")?.run_id,
    )?;
    let explanation = &explained
        .work_units
        .first()
        .and_then(|work| work.selection.as_ref())
        .ok_or("explanation")?
        .candidates[0];
    assert_eq!(explanation.supporting_files.len(), 1);
    assert_eq!(
        explanation
            .prepared_model_requests
            .iter()
            .filter(|request| request.supporting_files.len() == 1)
            .count(),
        1
    );
    assert!(!serde_json::to_string(&explained)?.contains("support bytes"));
    drop(prompts);
    host.catalog.lock().await.mcp.skills.clear();
    session
        .start(
            "next-file-turn",
            session.head().await.state_revision,
            selected_input(&host).await,
        )
        .await?;
    session.drive().await?;
    let prompts = executor.prompts.lock().await;
    assert_eq!(prompts.len(), 3);
    assert!(!serde_json::to_string(&prompts[2])?.contains("support bytes"));
    assert_eq!(host.file_reads.load(Ordering::SeqCst), 1);
    Ok(())
}

#[tokio::test]
async fn unlisted_or_escaping_supporting_paths_settle_without_a_resource_read() -> TestResult {
    for reference in [
        "../other/SKILL.md",
        "skill://other/references/check.md",
        "missing.md",
    ] {
        let host = Arc::new(
            NativeSkills::with_mcp(Arc::new(Harness::new(None, None)), "PATH_PARENT").await?,
        );
        let read = read_call(&host, reference, "invalid-file").await?;
        let (session, executor, _) = setup_features(
            host.clone(),
            vec![output(vec![read]), output(vec![text("done")])],
            false,
            true,
        )
        .await?;
        session
            .start(
                "invalid-file-start",
                session.head().await.state_revision,
                required_mcp_task(),
            )
            .await?;
        approve(&session, &host).await?;
        let snapshot = session.drive().await?;
        assert_eq!(
            snapshot.run.as_ref().map(|run| run.status),
            Some(RunStatus::Completed)
        );
        assert_eq!(host.file_reads.load(Ordering::SeqCst), 0);
        assert_eq!(executor.calls.load(Ordering::SeqCst), 2);
        assert!(
            root_selection(&snapshot)
                .ok_or("selection")?
                .failure
                .is_none()
        );
        assert!(
            serde_json::to_string(&executor.prompts.lock().await[1].messages)?
                .contains("\\\"ok\\\":false")
        );
    }
    Ok(())
}

#[tokio::test]
async fn corrupt_supporting_bytes_fail_the_original_turn_before_another_model_request() -> TestResult
{
    let host = Arc::new(
        NativeSkills::with_mcp(Arc::new(Harness::new(None, None)), "CORRUPT_PARENT").await?,
    );
    *host.supporting_bytes.lock().await = b"tampered bytes".to_vec();
    let read = read_call(&host, "references/check.md", "corrupt-file").await?;
    let (session, executor, _) = setup_features(
        host.clone(),
        vec![output(vec![read]), output(vec![text("must not run")])],
        false,
        true,
    )
    .await?;
    session
        .start(
            "corrupt-file-start",
            session.head().await.state_revision,
            required_mcp_task(),
        )
        .await?;
    approve(&session, &host).await?;
    let snapshot = session.drive().await?;
    assert_eq!(
        snapshot.run.as_ref().map(|run| run.status),
        Some(RunStatus::Failed)
    );
    assert_eq!(executor.calls.load(Ordering::SeqCst), 1);
    let selection = root_selection(&snapshot).ok_or("selection")?;
    assert_eq!(
        selection.failure.as_ref().map(|error| error.code),
        Some(ErrorCode::ArtifactUnavailable)
    );
    assert!(selection.candidates[0].files.is_empty());
    assert_eq!(host.file_reads.load(Ordering::SeqCst), 1);
    Ok(())
}

#[tokio::test]
async fn supporting_file_over_the_host_limit_settles_without_reading_or_retrying() -> TestResult {
    let host = Arc::new(
        NativeSkills::with_mcp_file(
            Arc::new(Harness::new(None, None)),
            "BUDGET_PARENT",
            &vec![b'x'; 8193],
        )
        .await?,
    );
    let read = read_call(&host, "references/check.md", "oversized-file").await?;
    let (session, executor, _) = setup_features(
        host.clone(),
        vec![output(vec![read]), output(vec![text("done")])],
        false,
        true,
    )
    .await?;
    session
        .start(
            "file-budget-start",
            session.head().await.state_revision,
            required_mcp_task(),
        )
        .await?;
    approve(&session, &host).await?;
    let snapshot = session.drive().await?;
    assert_eq!(
        snapshot.run.as_ref().map(|run| run.status),
        Some(RunStatus::Completed)
    );
    assert_eq!(host.file_reads.load(Ordering::SeqCst), 0);
    assert_eq!(executor.calls.load(Ordering::SeqCst), 2);
    let selection = root_selection(&snapshot).ok_or("selection")?;
    assert!(selection.failure.is_none() && selection.candidates[0].files.is_empty());
    assert!(serde_json::to_string(&executor.prompts.lock().await[1])?.contains("limit_exceeded"));
    Ok(())
}

#[tokio::test]
async fn cancelling_a_supporting_read_discards_bytes_and_never_prepares_another_model() -> TestResult
{
    let host = Arc::new(
        NativeSkills::with_mcp(Arc::new(Harness::new(None, None)), "CANCEL_FILE_PARENT").await?,
    );
    let read = read_call(&host, "references/check.md", "blocked-file").await?;
    let (session, executor, _) =
        setup_features(host.clone(), vec![output(vec![read])], false, true).await?;
    session
        .start(
            "cancel-file-start",
            session.head().await.state_revision,
            required_mcp_task(),
        )
        .await?;
    approve(&session, &host).await?;
    *host.file_gate.lock().await = Some(Arc::new(tokio::sync::Semaphore::new(0)));
    let driving = {
        let session = session.clone();
        tokio::spawn(async move { session.drive().await })
    };
    tokio::time::timeout(Duration::from_secs(5), host.file_seen.acquire())
        .await??
        .forget();
    let snapshot = session.snapshot().await;
    session
        .cancel_run(
            "cancel-support-read",
            session.head().await.state_revision,
            &snapshot.run.ok_or("run")?.run_id,
        )
        .await?;
    let snapshot = tokio::time::timeout(Duration::from_secs(5), driving).await???;
    assert_eq!(
        snapshot.run.as_ref().map(|run| run.status),
        Some(RunStatus::Cancelled)
    );
    assert!(
        host.file_token
            .lock()
            .await
            .as_ref()
            .is_some_and(CancellationToken::is_cancelled)
    );
    assert_eq!(executor.calls.load(Ordering::SeqCst), 1);
    assert!(
        root_selection(&snapshot).ok_or("selection")?.candidates[0]
            .files
            .is_empty()
    );
    Ok(())
}

#[tokio::test]
async fn supporting_cache_restores_with_original_identity_and_requires_its_feature() -> TestResult {
    let durable = Arc::new(Harness::new(None, None));
    let host = Arc::new(NativeSkills::with_mcp(durable.clone(), "RESTORE_FILE_PARENT").await?);
    let read = read_call(&host, "references/check.md", "restore-file").await?;
    let (session, _, _) = setup_features(
        host.clone(),
        vec![output(vec![read]), output(vec![text("done")])],
        false,
        true,
    )
    .await?;
    session
        .start(
            "restore-file-start",
            session.head().await.state_revision,
            required_mcp_task(),
        )
        .await?;
    approve(&session, &host).await?;
    session.drive().await?;
    let prefix = recovery::prefix(&*durable.store.lock().await, "skills.file.settled")?;
    session.disconnect().await;
    let replacement = recovery::harness_at(prefix).await;
    let new_host =
        Arc::new(NativeSkills::with_mcp(replacement.clone(), "REPLACEMENT_MUST_NOT_LOAD").await?);
    new_host.bytes.lock().await.clear();
    *new_host.supporting_bytes.lock().await = b"REPLACEMENT_SUPPORT_MUST_NOT_LOAD".to_vec();
    let request = recovery::request(&*replacement.store.lock().await, true)?;
    let mut caps = recovery::capabilities(&request.binding.grant.core_instance_id);
    caps.operations = vec![
        FEATURE.into(),
        bitrouter_orchestrator::core::skills::FEATURE.into(),
        bitrouter_orchestrator::core::skills::FILES_FEATURE.into(),
    ];
    let (app, executor) = recovery::application(vec![output(vec![text("restored")])])?;
    let mut old_caps = caps.clone();
    old_caps
        .operations
        .retain(|feature| feature != bitrouter_orchestrator::core::skills::FILES_FEATURE);
    assert!(matches!(
        CoreSession::restore(
            request.clone(),
            &old_caps,
            app.clone(),
            CallerContext::local(),
            new_host.clone()
        )
        .await,
        Err(CoreError {
            code: ErrorCode::UnsupportedCapability,
            ..
        })
    ));
    let restored = CoreSession::restore(
        request,
        &caps,
        app,
        CallerContext::local(),
        new_host.clone(),
    )
    .await?;
    let snapshot = restored.drive().await?;
    assert_eq!(
        snapshot.run.as_ref().map(|run| run.status),
        Some(RunStatus::Completed)
    );
    assert_eq!(new_host.file_reads.load(Ordering::SeqCst), 0);
    assert_eq!(new_host.reads.load(Ordering::SeqCst), 0);
    assert_eq!(new_host.metadata_reads.load(Ordering::SeqCst), 0);
    let prompts = executor.prompts.lock().await;
    assert_eq!(prompts.len(), 1);
    let model = serde_json::to_string(&prompts[0])?;
    assert!(
        model.contains("support bytes")
            && model.contains("RESTORE_FILE_PARENT")
            && !model.contains("REPLACEMENT")
    );
    drop(prompts);
    let mut tampered = snapshot.clone();
    let turn_id = snapshot.root_turn().ok_or("turn")?.agent_turn_id.clone();
    tampered
        .context_store
        .work
        .get_mut(&turn_id)
        .and_then(|work| work.skills.as_mut())
        .ok_or("selection")?
        .candidates[0]
        .files
        .insert(
            "skill://review/references/check.md".into(),
            "corrupted cache".into(),
        );
    assert!(
        bitrouter_orchestrator::core::skills::explain::capture(
            &tampered,
            &snapshot.run.as_ref().ok_or("run")?.run_id
        )
        .is_err()
    );
    // Restore the complete journal too: its second model context has already
    // committed a protected supporting-file evidence block, not just a cache.
    let complete = replacement.store.lock().await.clone();
    restored.disconnect().await;
    let terminal_store = recovery::harness_at(complete).await;
    let terminal_host = Arc::new(
        NativeSkills::with_mcp(terminal_store.clone(), "UNRELATED_TERMINAL_SOURCE").await?,
    );
    terminal_host.bytes.lock().await.clear();
    let request = recovery::request(&*terminal_store.store.lock().await, true)?;
    let (app, executor) = recovery::application(vec![])?;
    let reopened = CoreSession::restore(
        request,
        &caps,
        app,
        CallerContext::local(),
        terminal_host.clone(),
    )
    .await?;
    let recovered = reopened.drive().await?;
    assert_eq!(
        recovered.run.as_ref().map(|run| run.status),
        Some(RunStatus::Completed)
    );
    assert_eq!(executor.calls.load(Ordering::SeqCst), 0);
    assert_eq!(terminal_host.file_reads.load(Ordering::SeqCst), 0);
    assert_eq!(terminal_host.reads.load(Ordering::SeqCst), 0);
    assert_eq!(
        root_selection(&recovered)
            .ok_or("restored selection")?
            .candidates[0]
            .files,
        root_selection(&snapshot)
            .ok_or("original selection")?
            .candidates[0]
            .files
    );
    Ok(())
}

#[tokio::test]
async fn supporting_data_is_counted_before_model_admission_and_cannot_be_dropped() -> TestResult {
    let host = Arc::new(
        NativeSkills::with_mcp(Arc::new(Harness::new(None, None)), "COUNT_FILE_PARENT").await?,
    );
    let read = read_call(&host, "references/check.md", "count-file").await?;
    let executor = Arc::new(FileCounter {
        mock: MockExecutor::new(vec![output(vec![read])]),
        counts: Default::default(),
        generated: Default::default(),
    });
    let mut route = target("counted-skill");
    route.model_constraints.input_token_counting = Some(InputTokenCounting::Responses);
    route.model_constraints.token_limits.max_input_tokens = Some(500);
    let table = StaticRoutingTable::new();
    table.insert("fixture-model", vec![route]);
    let app = App::builder()
        .language_model(|builder| {
            builder
                .routing_table(Arc::new(table))
                .executor(executor.clone());
        })
        .build()?;
    let session = bind_app_with_features(
        Arc::new(app),
        host.clone(),
        Limits::default(),
        vec![
            FEATURE.into(),
            bitrouter_orchestrator::core::skills::FEATURE.into(),
            bitrouter_orchestrator::core::skills::FILES_FEATURE.into(),
        ],
    )
    .await?;
    session
        .start(
            "count-file-start",
            session.head().await.state_revision,
            required_mcp_task(),
        )
        .await?;
    approve(&session, &host).await?;
    let snapshot = session.drive().await?;
    assert_eq!(
        snapshot.run.as_ref().map(|run| run.status),
        Some(RunStatus::Failed)
    );
    assert_eq!(executor.generated.lock().await.len(), 1);
    let counts = executor.counts.lock().await;
    assert!(counts.len() >= 2);
    assert!(!serde_json::to_string(&counts[0])?.contains("support bytes"));
    assert!(counts.iter().skip(1).all(|prompt| {
        serde_json::to_string(prompt).is_ok_and(|text| text.contains("support bytes"))
    }));
    let turn = snapshot.root_turn().ok_or("turn")?;
    assert!(
        turn.steps
            .iter()
            .skip(1)
            .all(|step| step.attempts.is_empty())
    );
    assert!(
        turn.steps
            .iter()
            .filter_map(|step| step.decision.as_ref())
            .any(|decision| decision.routes.iter().any(|route| route
                .rejection_reasons
                .iter()
                .any(|reason| reason == "input_limit_exceeded")))
    );
    assert_eq!(
        root_selection(&snapshot).ok_or("selection")?.candidates[0].files["skill://review/references/check.md"],
        "support bytes\n"
    );
    assert_eq!(host.file_reads.load(Ordering::SeqCst), 1);
    Ok(())
}

#[tokio::test]
async fn unacknowledged_file_settlement_blocks_model_delivery_and_repeated_reads() -> TestResult {
    let host = Arc::new(
        NativeSkills::with_mcp(
            Arc::new(Harness::new(Some("skills.file.settled"), None)),
            "FILE_ACK_PARENT",
        )
        .await?,
    );
    let read = read_call(&host, "references/check.md", "unacknowledged-file").await?;
    let (session, executor, _) =
        setup_features(host.clone(), vec![output(vec![read])], false, true).await?;
    session
        .start(
            "file-ack-start",
            session.head().await.state_revision,
            required_mcp_task(),
        )
        .await?;
    approve(&session, &host).await?;
    assert!(session.drive().await.is_err());
    assert!(session.drive().await.is_err());
    assert_eq!(executor.calls.load(Ordering::SeqCst), 1);
    assert_eq!(host.file_reads.load(Ordering::SeqCst), 1);
    assert!(
        root_selection(&session.snapshot().await)
            .ok_or("acknowledged selection")?
            .candidates[0]
            .files
            .is_empty()
    );
    Ok(())
}

#[tokio::test]
async fn binary_support_is_rejected_as_model_data_even_with_matching_commitments() -> TestResult {
    for bytes in [vec![0xff], b"binary\0data".to_vec()] {
        let host = Arc::new(
            NativeSkills::with_mcp_file(
                Arc::new(Harness::new(None, None)),
                "BINARY_PARENT",
                &bytes,
            )
            .await?,
        );
        let read = read_call(&host, "references/check.md", "binary-file").await?;
        let (session, executor, _) =
            setup_features(host.clone(), vec![output(vec![read])], false, true).await?;
        session
            .start(
                "binary-file-start",
                session.head().await.state_revision,
                required_mcp_task(),
            )
            .await?;
        approve(&session, &host).await?;
        let snapshot = session.drive().await?;
        assert_eq!(
            snapshot.run.as_ref().map(|run| run.status),
            Some(RunStatus::Failed)
        );
        assert_eq!(executor.calls.load(Ordering::SeqCst), 1);
        assert!(
            root_selection(&snapshot).ok_or("selection")?.candidates[0]
                .files
                .is_empty()
        );
        assert_eq!(host.file_reads.load(Ordering::SeqCst), 1);
    }
    Ok(())
}

#[tokio::test]
async fn a_supporting_read_settles_while_the_same_tool_batch_waits_for_a_child() -> TestResult {
    use bitrouter_orchestrator::core::collaboration::Action;
    let durable = Arc::new(Harness::new(None, None));
    let host = Arc::new(NativeSkills::with_mcp(durable.clone(), "MIXED_WAIT_PARENT").await?);
    let (session, executor, _) = setup_features(
        host.clone(),
        vec![output(vec![call("hold-root")])],
        false,
        true,
    )
    .await?;
    session
        .start(
            "mixed-file-start",
            session.head().await.state_revision,
            required_mcp_task(),
        )
        .await?;
    approve(&session, &host).await?;
    session.drive().await?;
    let root = session.snapshot().await.agent_id;
    let mut task = work("held-child");
    task.fresh_context = true;
    let spawned = session
        .collaborate(
            "spawn-held-child",
            session.head().await.state_revision,
            &root,
            Action::Spawn { task },
        )
        .await?;
    let child = spawned
        .assigned_ids
        .get("agent_id")
        .ok_or("child identity")?;
    // The child reaches its own pending content consent before the parent waits.
    // It stays there, so only the file read can make progress in the mixed batch.
    session.drive().await?;
    assert_eq!(executor.calls.load(Ordering::SeqCst), 1);
    assert!(
        host.consent_requests
            .lock()
            .await
            .iter()
            .any(|request| request.agent_id == *child)
    );
    let read = read_call(&host, "references/check.md", "mixed-file").await?;
    executor.agent_once.lock().await.insert(
        root.clone(),
        vec![
            core_call(
                "wait_agent",
                json!({"agent_ids":[child],"timeout_ms":600000}),
                "wait-held-child",
            ),
            read,
        ],
    );
    let command = durable
        .sent
        .lock()
        .await
        .first()
        .cloned()
        .ok_or("held read")?;
    session
        .tool_result("root-read-complete", result(&command))
        .await?;
    let advanced = tokio::time::timeout(Duration::from_secs(5), session.drive()).await;
    let snapshot = session.snapshot().await;
    session
        .cancel_run(
            "mixed-file-cleanup",
            session.head().await.state_revision,
            &snapshot.run.as_ref().ok_or("run")?.run_id,
        )
        .await?;
    tokio::time::timeout(Duration::from_secs(5), session.drive()).await??;
    let advanced = advanced??;
    assert_eq!(
        advanced.root_turn().map(|turn| turn.status),
        Some(bitrouter_orchestrator::core::session::AgentStatus::WaitingMessage)
    );
    assert_eq!(host.file_reads.load(Ordering::SeqCst), 1);
    assert_eq!(
        root_selection(&advanced).ok_or("selection")?.candidates[0].files["skill://review/references/check.md"],
        "support bytes\n"
    );
    assert_eq!(executor.calls.load(Ordering::SeqCst), 2);
    Ok(())
}

#[tokio::test]
async fn permission_or_tool_manifest_changes_cancel_support_and_fail_the_original_turn()
-> TestResult {
    for change_tools in [false, true] {
        let host = Arc::new(
            NativeSkills::with_mcp(Arc::new(Harness::new(None, None)), "REVOKED_FILE_PARENT")
                .await?,
        );
        let read = read_call(&host, "references/check.md", "revoked-file").await?;
        let (session, executor, _) =
            setup_features(host.clone(), vec![output(vec![read])], false, true).await?;
        session
            .start(
                "revoke-file-start",
                session.head().await.state_revision,
                required_mcp_task(),
            )
            .await?;
        approve(&session, &host).await?;
        let old = host.consent_requests.lock().await[0].clone();
        *host.file_gate.lock().await = Some(Arc::new(tokio::sync::Semaphore::new(0)));
        let running = {
            let session = session.clone();
            tokio::spawn(async move { session.drive().await })
        };
        tokio::time::timeout(Duration::from_secs(5), host.file_seen.acquire())
            .await??
            .forget();
        let before = session.snapshot().await;
        let mut manifest = before.manifest.clone();
        if change_tools {
            manifest.tools[0]
                .description
                .push_str(" (revised contract)");
            manifest.tool_manifest_digest = HarnessManifest::digest(&manifest.tools)?;
        } else {
            manifest.permission_revision += 1;
        }
        session
            .signals(
                "revoke-support",
                SignalUpdate {
                    signal_revision: before.signals.revision + 1,
                    observed_at: "2".into(),
                    scope: before.session_id.clone(),
                    source: grant().harness_id,
                    workspace_revision: manifest.workspace_revision.clone(),
                    manifest,
                    materials: vec![],
                    facts: BTreeMap::new(),
                },
            )
            .await?;
        let waiting = tokio::time::timeout(Duration::from_secs(5), running).await???;
        assert!(
            host.file_token
                .lock()
                .await
                .as_ref()
                .is_some_and(CancellationToken::is_cancelled)
        );
        assert_eq!(host.consent_requests.lock().await.len(), 1);
        assert!(!session.skill_consent_pending(&old).await);
        let selection = root_selection(&waiting).ok_or("selection")?;
        assert!(selection.candidates[0].files.is_empty());
        assert_eq!(
            selection.candidates[0].source,
            SkillSource::Mcp(old.source.clone())
        );
        assert!(
            selection
                .failure
                .as_ref()
                .is_some_and(|error| error.code == ErrorCode::ArtifactUnavailable
                    && error.message.contains("no longer authorizes this Turn"))
        );
        assert_eq!(
            waiting.run.as_ref().map(|run| run.status),
            Some(RunStatus::Failed)
        );
        assert!(
            session
                .answer_skill_consent("obsolete-file-approval", old.answer(true))
                .await
                .is_err()
        );
        assert_eq!(executor.calls.load(Ordering::SeqCst), 1);
        assert_eq!(host.file_reads.load(Ordering::SeqCst), 1);
    }
    Ok(())
}

#[tokio::test]
async fn a_nested_skill_file_is_data_and_never_gets_activation_or_tool_grants() -> TestResult {
    let nested = b"---\nname: child\ndescription: nested workflow\nallowed-tools: shell\n---\nNESTED_UNACTIVATED_DATA\n";
    let host = Arc::new(
        NativeSkills::with_mcp_file_at(
            Arc::new(Harness::new(None, None)),
            "NESTED_PARENT",
            "skill://review/child/SKILL.md",
            nested,
        )
        .await?,
    );
    let read = read_call(&host, "child/SKILL.md", "read-nested").await?;
    let (session, executor, _) = setup_features(
        host.clone(),
        vec![output(vec![read]), output(vec![text("done")])],
        false,
        true,
    )
    .await?;
    session
        .start(
            "nested-file-start",
            session.head().await.state_revision,
            required_mcp_task(),
        )
        .await?;
    approve(&session, &host).await?;
    let snapshot = session.drive().await?;
    assert_eq!(
        snapshot.run.as_ref().map(|run| run.status),
        Some(RunStatus::Completed)
    );
    assert_eq!(host.consent_requests.lock().await.len(), 1);
    assert_eq!(
        root_selection(&snapshot)
            .ok_or("selection")?
            .candidates
            .len(),
        1
    );
    assert_eq!(host.file_reads.load(Ordering::SeqCst), 1);
    let prompts = executor.prompts.lock().await;
    assert_eq!(prompts.len(), 2);
    assert!(
        !prompts[1]
            .system
            .as_deref()
            .unwrap_or_default()
            .contains("NESTED_UNACTIVATED_DATA")
    );
    assert!(prompts[1].messages.iter().any(|message| {
        message.role == Role::User
            && serde_json::to_string(message)
                .is_ok_and(|text| text.contains("NESTED_UNACTIVATED_DATA"))
    }));
    assert!(
        !prompts[1]
            .tools
            .iter()
            .any(|tool| matches!(tool,Tool::Function{name,..} if name=="shell"))
    );
    assert!(serde_json::to_string(&prompts[1].messages)?.contains("nested_activation\\\":false"));
    Ok(())
}

#[tokio::test]
async fn a_smaller_turn_context_rejects_support_without_caching_or_truncating_it() -> TestResult {
    let bytes = vec![b'x'; 8192];
    let baseline = Arc::new(
        NativeSkills::with_mcp_file(Arc::new(Harness::new(None, None)), "CONTEXT_PARENT", &bytes)
            .await?,
    );
    let (calibration, recorded, _) = setup_features(
        baseline.clone(),
        vec![output(vec![text("baseline")])],
        false,
        true,
    )
    .await?;
    calibration
        .start(
            "context-baseline",
            calibration.head().await.state_revision,
            required_mcp_task(),
        )
        .await?;
    approve(&calibration, &baseline).await?;
    calibration.drive().await?;
    let initial = serde_json::to_vec(&recorded.prompts.lock().await[0])?.len() as u64;
    let host = Arc::new(
        NativeSkills::with_mcp_file(Arc::new(Harness::new(None, None)), "CONTEXT_PARENT", &bytes)
            .await?,
    );
    let read = read_call(&host, "references/check.md", "context-file").await?;
    let (session, executor, _) = setup_features(
        host.clone(),
        vec![
            output(vec![read]),
            output(vec![text("without oversized support")]),
        ],
        false,
        true,
    )
    .await?;
    let mut input = required_mcp_task();
    input.context_limit_bytes = Some(initial + 2048);
    session
        .start(
            "small-context-file",
            session.head().await.state_revision,
            input,
        )
        .await?;
    approve(&session, &host).await?;
    let snapshot = session.drive().await?;
    assert_eq!(
        snapshot.run.as_ref().map(|run| run.status),
        Some(RunStatus::Completed)
    );
    assert_eq!(host.file_reads.load(Ordering::SeqCst), 1);
    let selection = root_selection(&snapshot).ok_or("selection")?;
    assert!(selection.failure.is_none() && selection.candidates[0].files.is_empty());
    let prompts = executor.prompts.lock().await;
    assert_eq!(prompts.len(), 2);
    assert!(
        serde_json::to_string(&prompts[1])?
            .contains("supporting data exceeds the original Turn context budget")
    );
    assert!(!serde_json::to_string(&prompts[1])?.contains("BEGIN VERIFIED FILE DATA"));
    Ok(())
}
