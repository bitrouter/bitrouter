use super::*;
use bitrouter_orchestrator::core::protocol::{ContextMode, DiscardableHistory};
use bitrouter_sdk::language_model::native::{InputTokenCounting, NativeInputCount};
use bitrouter_sdk::language_model::types::Role;

struct RebuildExecutor {
    mock: MockExecutor,
    counts: Mutex<Vec<Prompt>>,
    generated: Mutex<Vec<Prompt>>,
    mandatory_too_large: bool,
}

#[async_trait]
impl Executor for RebuildExecutor {
    async fn count_input_tokens(
        &self,
        _: &RoutingTarget,
        prompt: &Prompt,
        _: &PipelineContext,
    ) -> bitrouter_sdk::Result<NativeInputCount> {
        self.counts.lock().await.push(prompt.clone());
        let wire = serde_json::to_string(prompt)
            .map_err(|error| bitrouter_sdk::BitrouterError::internal(error.to_string()))?;
        // Scripted provider observations, not a byte-to-token estimate.
        let large = wire.contains("current-result")
            && (wire.contains("historical-result") || self.mandatory_too_large);
        Ok(NativeInputCount::Counted {
            input_tokens: if large { 1000 } else { 100 },
            request_sha256: sha256(wire.as_bytes()),
            source: "fixture_provider_count".into(),
        })
    }

    async fn execute(
        &self,
        target: &RoutingTarget,
        prompt: &Prompt,
        ctx: &PipelineContext,
    ) -> bitrouter_sdk::Result<ExecutionResult> {
        self.generated.lock().await.push(prompt.clone());
        self.mock.execute(target, prompt, ctx).await
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

struct ReferToPriorPlan;

impl bitrouter_sdk::app::PromptTransform for ReferToPriorPlan {
    fn apply(&self, prompt: &mut Prompt) {
        prompt
            .messages
            .push(bitrouter_sdk::language_model::types::Message::text(
                Role::User,
                "Follow the prior assistant plan and use the artifact from the old tool result.",
            ));
    }
}

async fn historical_task(
    harness: Arc<Harness>,
    mandatory_too_large: bool,
    with_material: bool,
    with_transform: bool,
) -> Result<(CoreSession, Arc<RebuildExecutor>, TaskInput), Box<dyn std::error::Error>> {
    let executor = Arc::new(RebuildExecutor {
        mock: MockExecutor::new(vec![
            output(vec![call("old-call")]),
            output(vec![text(
                "Implementation plan: use artifact-42 from the old result.",
            )]),
            output(vec![call("current-call")]),
            output(vec![text("Current task completed with current-result.")]),
        ]),
        counts: Mutex::new(Vec::new()),
        generated: Mutex::new(Vec::new()),
        mandatory_too_large,
    });
    let mut route = target("counted");
    route.model_constraints.input_token_counting = Some(InputTokenCounting::Responses);
    route.model_constraints.token_limits.max_input_tokens = Some(500);
    let table = StaticRoutingTable::new();
    table.insert("fixture-model", vec![route]);
    let mut app_builder = App::builder();
    if with_transform {
        app_builder = app_builder.prompt_transform(Arc::new(ReferToPriorPlan));
    }
    let app = app_builder
        .language_model(|builder| {
            builder
                .routing_table(Arc::new(table))
                .executor(executor.clone());
        })
        .build()?;
    let session = bind_app(Arc::new(app), harness.clone()).await?;
    if with_material {
        session
            .signals(
                "material",
                signal_update(
                    &session,
                    vec![material("v1", "Unrelated required README", true)],
                )
                .await,
            )
            .await?;
    }
    session
        .start("first", session.head().await.state_revision, input())
        .await?;
    session.drive().await?;
    let mut report = result(&harness.sent.lock().await[0]);
    report.output = "historical-result with artifact-42".into();
    session.tool_result("old-result", report).await?;
    let first = session.drive().await?;
    assert_eq!(
        first.run.as_ref().map(|run| run.status),
        Some(RunStatus::Completed)
    );
    let history = &first.agents[&first.agent_id].history;
    let mut task = input();
    task.text = "Read the current file and complete this self-contained task.".into();
    task.routing.context = ContextMode::Auto;
    task.discardable_history = Some(DiscardableHistory {
        history_sha256: sha256(&serde_json::to_vec(history)?),
        message_indices: history
            .iter()
            .enumerate()
            .filter_map(|(index, message)| {
                matches!(message.role, Role::Assistant | Role::Tool).then_some(index)
            })
            .collect(),
    });
    Ok((session, executor, task))
}

async fn current_result(session: &CoreSession, harness: &Harness, task: TaskInput) -> TestResult {
    session
        .start("second", session.head().await.state_revision, task)
        .await?;
    assert_eq!(
        session.drive().await?.run.map(|run| run.status),
        Some(RunStatus::Waiting)
    );
    let mut report = result(&harness.sent.lock().await[1]);
    report.output = "current-result".into();
    session.tool_result("current-result", report).await?;
    Ok(())
}

#[tokio::test]
async fn explicit_optional_history_rebuild_is_committed_recounted_and_auditable() -> TestResult {
    let harness = Arc::new(Harness::new(None, None));
    let (session, executor, task) = historical_task(harness.clone(), false, true, false).await?;
    let before = session.snapshot().await;
    let original_history = before.agents[&before.agent_id].history.clone();
    let claim = task.discardable_history.clone();
    current_result(&session, &harness, task).await?;
    let state = session.drive().await?;
    assert_eq!(
        state.run.as_ref().map(|run| run.status),
        Some(RunStatus::Completed)
    );
    let turn = state.root_turn().ok_or("missing turn")?;
    assert_eq!(turn.input.discardable_history, claim);
    assert_eq!(turn.steps.len(), 3);
    let rejected = &turn.steps[1];
    let rebuilt = &turn.steps[2];
    assert!(rejected.attempts.is_empty());
    assert!(rejected.input_history.starts_with(&original_history));
    let record = rejected
        .rebuild
        .as_ref()
        .ok_or("missing reconstruction receipt")?;
    assert!(record.error.is_none());
    assert_eq!(record.removed_history_messages, 3);
    assert_eq!(record.rebuilt_step_id.as_ref(), Some(&rebuilt.step_id));
    assert_eq!(rebuilt.reconstructed_from.as_ref(), Some(&rejected.step_id));
    assert_eq!(rebuilt.context_revision, rejected.context_revision + 1);
    assert_eq!(rebuilt.attempts.len(), 1);
    let counts = executor.counts.lock().await;
    let generated = executor.generated.lock().await;
    assert_eq!(counts.len(), 5);
    assert_eq!(generated.len(), 4);
    assert_eq!(counts[4], generated[3]);
    assert_eq!(counts[3].params, counts[4].params);
    assert_eq!(counts[3].model, counts[4].model);
    assert_eq!(counts[3].system, counts[4].system);
    assert_eq!(counts[3].tools, counts[4].tools);
    let wire = serde_json::to_string(&counts[4])?;
    assert!(wire.contains("Unrelated required README"));
    assert!(wire.contains("current-result"));
    assert!(wire.contains("current-call"));
    assert!(!wire.contains("historical-result"));
    assert!(!wire.contains("artifact-42"));
    for message in original_history
        .iter()
        .filter(|message| message.role == Role::User)
    {
        assert!(counts[4].messages.contains(message));
    }
    assert_eq!(
        rebuilt
            .decision
            .as_ref()
            .ok_or("decision")?
            .candidate_ids
            .len(),
        2
    );
    let kinds = harness.committed_kinds().await?;
    let rebuild = kinds
        .iter()
        .position(|kind| kind == "context.rebuild")
        .ok_or("rebuild event")?;
    assert_eq!(kinds[rebuild + 1], "model.input_count.intent");
    Ok(())
}

#[tokio::test]
async fn ambiguous_stale_or_invalid_history_permissions_never_drop_old_evidence() -> TestResult {
    for case in [
        "missing",
        "stale",
        "user",
        "partial-pair",
        "unordered",
        "current",
        "fixed",
        "no-material",
        "preparation",
    ] {
        let harness = Arc::new(Harness::new(None, None));
        let (session, executor, mut task) = historical_task(
            harness.clone(),
            false,
            case != "no-material",
            case == "preparation",
        )
        .await?;
        match case {
            "missing" => {
                task.discardable_history = None;
                task.text =
                    "Implement the previous plan using the artifact from the previous result."
                        .into();
            }
            "fixed" => task.routing.context = ContextMode::Fixed,
            _ => {
                let claim = task.discardable_history.as_mut().ok_or("claim")?;
                match case {
                    "stale" => claim.history_sha256 = "stale".into(),
                    "user" => claim.message_indices.insert(0, 0),
                    "partial-pair" => claim.message_indices = vec![1],
                    "unordered" => claim.message_indices.reverse(),
                    "current" => claim.message_indices.push(5),
                    _ => {}
                }
            }
        }
        current_result(&session, &harness, task).await?;
        let _outcome = session.drive().await;
        let state = session.snapshot().await;
        assert_eq!(
            state.run.as_ref().map(|run| run.status),
            Some(RunStatus::Failed),
            "case={case}"
        );
        let turn = state.root_turn().ok_or("turn")?;
        assert_eq!(turn.steps.len(), 2, "case={case}");
        assert!(
            turn.steps[1]
                .rebuild
                .as_ref()
                .is_some_and(|record| record.error.is_some()),
            "case={case}"
        );
        assert_eq!(executor.counts.lock().await.len(), 4, "case={case}");
        assert_eq!(executor.generated.lock().await.len(), 3, "case={case}");
        assert!(
            serde_json::to_string(&state.agents[&state.agent_id].history)?.contains("artifact-42")
        );
    }
    Ok(())
}

#[tokio::test]
async fn mandatory_history_over_capacity_rejects_after_one_rebuild() -> TestResult {
    let harness = Arc::new(Harness::new(None, None));
    let (session, executor, task) = historical_task(harness.clone(), true, true, false).await?;
    current_result(&session, &harness, task).await?;
    let _outcome = session.drive().await;
    let state = session.snapshot().await;
    assert_eq!(
        state.run.as_ref().map(|run| run.status),
        Some(RunStatus::Failed)
    );
    let turn = state.root_turn().ok_or("turn")?;
    assert_eq!(turn.steps.len(), 3);
    assert!(turn.steps[2].rebuild.is_none());
    assert!(turn.steps[2].attempts.is_empty());
    assert_eq!(executor.counts.lock().await.len(), 5);
    assert_eq!(executor.generated.lock().await.len(), 3);
    assert!(serde_json::to_string(&turn.steps[2].input_history)?.contains("current-result"));
    Ok(())
}

#[tokio::test]
async fn reconstruction_ack_is_a_barrier_for_recount_and_generation() -> TestResult {
    for disconnect in [false, true] {
        let harness = Arc::new(Harness::new(None, Some("context.rebuild")));
        let (session, executor, task) =
            historical_task(harness.clone(), false, true, false).await?;
        current_result(&session, &harness, task).await?;
        let driver = tokio::spawn({
            let session = session.clone();
            async move { session.drive().await }
        });
        tokio::time::timeout(Duration::from_secs(5), harness.seen.acquire())
            .await??
            .forget();
        assert_eq!(executor.counts.lock().await.len(), 4);
        assert_eq!(executor.generated.lock().await.len(), 3);
        assert_eq!(
            session
                .snapshot()
                .await
                .root_turn()
                .ok_or("turn")?
                .steps
                .len(),
            2
        );
        if disconnect {
            session.disconnect().await;
        }
        harness.resume.add_permits(1);
        let outcome = tokio::time::timeout(Duration::from_secs(5), driver).await??;
        if disconnect {
            assert!(outcome.is_err());
            assert_eq!(executor.counts.lock().await.len(), 4);
            assert_eq!(executor.generated.lock().await.len(), 3);
        } else {
            assert_eq!(
                outcome?.run.map(|run| run.status),
                Some(RunStatus::Completed)
            );
        }
    }
    Ok(())
}

#[tokio::test]
async fn failed_reconstruction_commit_keeps_the_original_context() -> TestResult {
    let harness = Arc::new(Harness::new(Some("context.rebuild"), None));
    let (session, executor, task) = historical_task(harness.clone(), false, true, false).await?;
    current_result(&session, &harness, task).await?;
    assert!(session.drive().await.is_err());
    let state = session.snapshot().await;
    assert_eq!(state.root_turn().ok_or("turn")?.steps.len(), 2);
    assert!(serde_json::to_string(&state.agents[&state.agent_id].history)?.contains("artifact-42"));
    assert_eq!(executor.counts.lock().await.len(), 4);
    assert_eq!(executor.generated.lock().await.len(), 3);
    Ok(())
}

#[tokio::test]
async fn history_permissions_are_task_scoped_and_never_inherited_by_assignments() -> TestResult {
    let harness = Arc::new(Harness::new(None, None));
    let (session, _, task) = historical_task(harness, false, true, false).await?;
    let revision = session.head().await.state_revision;
    let root = session
        .start("second", revision, task.clone())
        .await?
        .assigned_ids["agent_id"]
        .clone();
    let mut changed = task;
    changed.discardable_history = None;
    assert_eq!(
        session
            .start("second", revision, changed)
            .await
            .err()
            .map(|error| error.code),
        Some(ErrorCode::OperationConflict)
    );
    let child = session
        .collaborate(
            "spawn",
            session.head().await.state_revision,
            &root,
            Action::Spawn {
                task: work("independent child"),
            },
        )
        .await?
        .assigned_ids["agent_id"]
        .clone();
    let mut followup = work("follow-up child");
    followup.fresh_context = false;
    session
        .collaborate(
            "followup",
            session.head().await.state_revision,
            &root,
            Action::Followup {
                agent_id: child.clone(),
                task: followup,
            },
        )
        .await?;
    let snapshot = session.snapshot().await;
    assert!(
        snapshot
            .root_turn()
            .ok_or("root")?
            .input
            .discardable_history
            .is_some()
    );
    let child = &snapshot.agents[&child];
    assert!(
        child
            .turn
            .as_ref()
            .ok_or("child")?
            .input
            .discardable_history
            .is_none()
    );
    assert!(child.queue[0].input.discardable_history.is_none());
    Ok(())
}
