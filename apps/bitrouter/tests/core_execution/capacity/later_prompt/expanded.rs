//! Prepared prompt bytes must be admitted before counting or model execution.

use super::*;
use bitrouter_ai::types::{Message, Role};
use bitrouter_sdk::language_model::native::{InputTokenCounting, NativeInputCount};
use bitrouter_sdk::language_model::native_preparation::NativePreparationWorkKind;

const EXPANSION_BYTES: usize = 512 * 1024;

#[derive(Default)]
struct ExpandPrompt {
    enabled: AtomicBool,
    calls: AtomicUsize,
}

impl bitrouter_sdk::app::PromptTransform for ExpandPrompt {
    fn apply(&self, prompt: &mut Prompt) {
        if self.enabled.load(Ordering::SeqCst) {
            self.calls.fetch_add(1, Ordering::SeqCst);
            prompt
                .messages
                .push(Message::text(Role::User, "x".repeat(EXPANSION_BYTES)));
        }
    }
}

struct CountedExecutor {
    inner: Arc<RecordingExecutor>,
    counts: AtomicUsize,
}

#[async_trait]
impl Executor for CountedExecutor {
    async fn count_input_tokens(
        &self,
        _: &RoutingTarget,
        prompt: &Prompt,
        _: &PipelineContext,
    ) -> bitrouter_sdk::Result<NativeInputCount> {
        self.counts.fetch_add(1, Ordering::SeqCst);
        let bytes = serde_json::to_vec(prompt)
            .map_err(|error| bitrouter_sdk::BitrouterError::internal(error.to_string()))?;
        Ok(NativeInputCount::Counted {
            input_tokens: 100,
            request_sha256: sha256(&bytes),
            source: "fixture_provider_count".into(),
        })
    }

    async fn execute(
        &self,
        target: &RoutingTarget,
        prompt: &Prompt,
        ctx: &PipelineContext,
    ) -> bitrouter_sdk::Result<ExecutionResult> {
        self.inner.execute(target, prompt, ctx).await
    }

    async fn execute_stream(
        &self,
        target: &RoutingTarget,
        prompt: &Prompt,
        ctx: &PipelineContext,
    ) -> bitrouter_sdk::Result<StreamPartStream> {
        self.inner.execute_stream(target, prompt, ctx).await
    }
}

struct Fixture {
    session: CoreSession,
    executor: Arc<CountedExecutor>,
    transform: Arc<ExpandPrompt>,
    settlements: Arc<AtomicUsize>,
}

async fn fixture(
    port: Arc<dyn HarnessPort>,
    counted: bool,
    existing_tools: bool,
) -> Result<Fixture, Box<dyn std::error::Error>> {
    let table = StaticRoutingTable::new();
    let mut route = target("prepared");
    if counted {
        route.model_constraints.input_token_counting = Some(InputTokenCounting::Responses);
    }
    table.insert("fixture-model", vec![route]);
    let executor = Arc::new(CountedExecutor {
        inner: Arc::new(RecordingExecutor {
            mock: MockExecutor::new(scripted_outputs(existing_tools)),
            agent_once: Mutex::new(Default::default()),
            prompts: Mutex::new(Vec::new()),
            calls: AtomicUsize::new(0),
        }),
        counts: AtomicUsize::new(0),
    });
    let transform = Arc::new(ExpandPrompt::default());
    let settlements = Arc::new(AtomicUsize::new(0));
    let app = App::builder()
        .prompt_transform(transform.clone())
        .language_model(|builder| {
            builder
                .routing_table(Arc::new(table))
                .executor(executor.clone())
                .settlement_recorder(Recorder(settlements.clone()));
        })
        .build()?;
    Ok(Fixture {
        session: bind_app(Arc::new(app), port).await?,
        executor,
        transform,
        settlements,
    })
}

fn task(sufficient: bool) -> TaskInput {
    let mut task = input();
    let checkpoint_bytes = if sufficient {
        8 * 1024 * 1024
    } else {
        EXPANSION_BYTES as u64
    };
    task.limits = Some(Limits {
        input_bytes: 8192,
        checkpoint_bytes,
        unacknowledged_bytes: checkpoint_bytes * 2,
        ..Limits::default()
    });
    task
}

async fn rejected(
    counted: bool,
    existing_tools: bool,
    kind: &'static str,
    persisted: bool,
) -> TestResult {
    let mut fixture_harness = Harness::new(None, None);
    fixture_harness.wait_for_approval = existing_tools;
    let harness = Arc::new(fixture_harness);
    let port = Arc::new(reconnect::FaultPort::new(harness.clone(), kind, persisted));
    let fixture = fixture(port.clone(), counted, existing_tools).await?;
    let session = &fixture.session;
    let task = task(false);
    let revision = session.head().await.state_revision;
    let accepted = session.start("input", revision, task.clone()).await?;
    let root = &accepted.assigned_ids["agent_id"];
    let (actor, old_tools) = if existing_tools {
        tool_intents::occupy_root_and_spawn(session, &harness, root).await?
    } else {
        (root.clone(), Vec::new())
    };
    let replies = accept_history(session, &harness, &actor).await?;
    let before = session.snapshot().await;
    let calls = 3 + usize::from(existing_tools);
    let counts = fixture.executor.counts.load(Ordering::SeqCst);
    assert_eq!(fixture.executor.inner.calls.load(Ordering::SeqCst), calls);
    assert_eq!(counts, if counted { calls } else { 0 });
    fixture.transform.enabled.store(true, Ordering::SeqCst);
    let outcome = session.drive().await;
    assert_eq!(fixture.transform.calls.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.executor.counts.load(Ordering::SeqCst), counts);
    assert_eq!(fixture.executor.inner.calls.load(Ordering::SeqCst), calls);
    assert_eq!(fixture.settlements.load(Ordering::SeqCst), calls + 1);
    if kind == "run.capacity_reached" || !existing_tools {
        let error = outcome.err().ok_or("missing failure ACK loss")?;
        assert_eq!(
            (error.code, error.commit_status),
            (ErrorCode::CheckpointUnavailable, CommitStatus::Unknown)
        );
        tool_intents::resolve_fault(session, &harness, &port, kind, persisted).await?;
    } else if let Err(error) = outcome {
        assert_eq!(
            (error.code, error.commit_status),
            (ErrorCode::LimitExceeded, CommitStatus::NotCommitted)
        );
    }
    let failed = session.snapshot().await;
    tool_intents::failed_capacity(&failed)?;
    let turn = failed.agents[&actor].turn.as_ref().ok_or("turn")?;
    let step = turn.steps.last().ok_or("preparing step")?;
    assert_eq!(turn.steps.len(), 4);
    assert!(step.plan.is_none());
    assert!(step.count_plan.is_none());
    assert!(step.input_counts.is_empty());
    assert!(step.attempts.is_empty());
    assert!(step.preparation_work.iter().any(|work| {
        work.work.kind == NativePreparationWorkKind::PromptTransform
            && work
                .report
                .as_ref()
                .is_some_and(|report| report.error_code.is_none())
    }));
    assert!(
        step.preparation_work
            .iter()
            .all(|work| work.report.is_some())
    );
    for reply in &replies {
        paired_reply(&failed, &actor, reply)?;
        assert!(step.input_history.iter().flat_map(|message| &message.content).any(|part|
            matches!(part, Content::ToolResult { output: ToolResultOutput::Text { value }, .. }
                if value == &reply.result.output)));
    }
    retained_model_outcomes(&before, &failed)?;
    session.drive().await?;
    let old_replies = if existing_tools {
        let replies = drain_original_tools(session, &harness, &old_tools).await?;
        let cleanup = session.drive().await;
        if kind == "run.failed" {
            let error = cleanup.err().ok_or("missing terminal ACK loss")?;
            assert_eq!(
                (error.code, error.commit_status),
                (ErrorCode::CheckpointUnavailable, CommitStatus::Unknown)
            );
            tool_intents::resolve_fault(session, &harness, &port, kind, persisted).await?;
        } else {
            cleanup?;
        }
        replies
    } else {
        Vec::new()
    };
    let done = session.drive().await?;
    assert_eq!(done.run.as_ref().ok_or("run")?.status, RunStatus::Failed);
    retained_model_outcomes(&before, &done)?;
    for reply in &replies {
        paired_reply(&done, &actor, reply)?;
    }
    for reply in &old_replies {
        paired_reply(&done, root, reply)?;
    }
    let head = session.head().await;
    assert_eq!(session.start("input", revision, task).await?, accepted);
    for reply in replies.iter().chain(&old_replies) {
        assert_eq!(
            session
                .tool_result(&reply.receipt.operation_id, reply.result.clone())
                .await?,
            reply.receipt
        );
    }
    session.drive().await?;
    assert_eq!(session.head().await, head);
    assert_eq!(fixture.transform.calls.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.executor.counts.load(Ordering::SeqCst), counts);
    assert_eq!(fixture.executor.inner.calls.load(Ordering::SeqCst), calls);
    assert_eq!(fixture.settlements.load(Ordering::SeqCst), calls + 1);
    assert_eq!(harness.sent.lock().await.len(), old_tools.len() + 3);
    let kinds = harness.committed_kinds().await?;
    for kind in ["run.capacity_reached", "run.failed"] {
        assert_eq!(kinds.iter().filter(|entry| *entry == kind).count(), 1);
    }
    session.release("release", head.state_revision).await?;
    Ok(())
}

#[tokio::test]
async fn expanded_prompt_admission_preserves_history_and_cleanup_across_ack_loss() -> TestResult {
    for counted in [false, true] {
        for existing_tools in [false, true] {
            for kind in ["run.capacity_reached", "run.failed"] {
                for persisted in [false, true] {
                    rejected(counted, existing_tools, kind, persisted).await.map_err(|error| format!("counted={counted}, tools={existing_tools}, {kind}, persisted={persisted}: {error}"))?;
                }
            }
        }
    }
    Ok(())
}

#[tokio::test]
async fn the_same_expanded_prompt_dispatches_with_sufficient_capacity() -> TestResult {
    for counted in [false, true] {
        let harness = Arc::new(Harness::new(None, None));
        let fixture = fixture(harness.clone(), counted, false).await?;
        let session = &fixture.session;
        let accepted = session
            .start("input", session.head().await.state_revision, task(true))
            .await?;
        let actor = &accepted.assigned_ids["agent_id"];
        let replies = accept_history(session, &harness, actor).await?;
        fixture.transform.enabled.store(true, Ordering::SeqCst);
        let done = session.drive().await?;
        assert_eq!(done.run.as_ref().ok_or("run")?.status, RunStatus::Completed);
        assert_eq!(fixture.executor.inner.calls.load(Ordering::SeqCst), 4);
        assert_eq!(
            fixture.executor.counts.load(Ordering::SeqCst),
            if counted { 4 } else { 0 }
        );
        assert_eq!(fixture.transform.calls.load(Ordering::SeqCst), 1);
        let prompts = fixture.executor.inner.prompts.lock().await;
        let prompt = prompts.last().ok_or("executed expanded prompt")?;
        assert!(prompt.messages.iter().flat_map(|message| &message.content).any(|part| matches!(part, Content::Text { text, .. } if text.len() == EXPANSION_BYTES)));
        for reply in &replies {
            paired_reply(&done, actor, reply)?;
            assert!(prompt.messages.iter().flat_map(|message| &message.content).any(|part|
                matches!(part, Content::ToolResult { output: ToolResultOutput::Text { value }, .. }
                    if value == &reply.result.output)));
        }
        session
            .release("release", session.head().await.state_revision)
            .await?;
    }
    Ok(())
}
