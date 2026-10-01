mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use bitrouter_orchestrator::core::checkpoint::{
    CheckpointAck, CheckpointBatch, DurableHead, sha256,
};
use bitrouter_orchestrator::core::protocol::{
    ArtifactRef, Bind, Capabilities, CommitStatus, CoreError, ErrorCode, HarnessManifest,
    HarnessTool, Limits, OwnershipGrant, RoutingSettings, ServerMessage, TaskInput, ToolEffect,
    ToolExecute, ToolOutcome, ToolResult, Verification,
};
use bitrouter_orchestrator::core::session::{CoreSession, HarnessPort, RunStatus, SessionSnapshot};
use bitrouter_sdk::App;
use bitrouter_sdk::caller::CallerContext;
use bitrouter_sdk::language_model::context::{PipelineContext, StreamContext};
use bitrouter_sdk::language_model::executor::{
    Executor, MockExecutor, MockResponse, StreamPartStream,
};
use bitrouter_sdk::language_model::hooks::{ObserveHook, Phase, RequestOutcome};
use bitrouter_sdk::language_model::routing::StaticRoutingTable;
use bitrouter_sdk::language_model::settlement::{SettlementContext, SettlementRecorder};
use bitrouter_sdk::language_model::types::{
    ApiProtocol, AuthScheme, Content, ExecutionResult, FinishReason, GenerateResult, Prompt,
    RoutingTarget, StreamPart, ToolChoice, Usage,
};
use serde_json::json;
use support::DurableHarness;
use tokio::sync::{Mutex, Semaphore};

type TestResult = Result<(), Box<dyn std::error::Error>>;

struct Harness {
    store: Mutex<DurableHarness>,
    sent: Mutex<Vec<ToolExecute>>,
    fail_kind: Option<&'static str>,
    hold_kind: Option<&'static str>,
    wrong_ack: Option<bool>,
    hold_send: bool,
    hold_after_send: bool,
    seen: Semaphore,
    resume: Semaphore,
    delivered: Semaphore,
}

impl Harness {
    fn new(fail_kind: Option<&'static str>, hold_kind: Option<&'static str>) -> Self {
        Self {
            store: Mutex::new(DurableHarness::new(grant())),
            sent: Mutex::new(Vec::new()),
            fail_kind,
            hold_kind,
            wrong_ack: None,
            hold_send: false,
            hold_after_send: false,
            seen: Semaphore::new(0),
            resume: Semaphore::new(0),
            delivered: Semaphore::new(0),
        }
    }

    async fn committed_kinds(&self) -> Result<Vec<String>, CoreError> {
        let store = self.store.lock().await;
        store
            .batches
            .iter()
            .map(|batch| {
                batch.decode(&store.limits).map(|payload| {
                    payload
                        .events
                        .into_iter()
                        .map(|event| event.kind)
                        .collect::<Vec<_>>()
                })
            })
            .collect::<Result<Vec<_>, _>>()
            .map(|groups| groups.into_iter().flatten().collect())
    }
}

#[async_trait]
impl HarnessPort for Harness {
    async fn commit(&self, batch: CheckpointBatch) -> Result<CheckpointAck, CoreError> {
        let payload = batch.decode(&Limits::default())?;
        if self
            .hold_kind
            .is_some_and(|kind| payload.events.iter().any(|event| event.kind == kind))
        {
            self.seen.add_permits(1);
            self.resume
                .acquire()
                .await
                .map_err(|_| {
                    CoreError::rejected(ErrorCode::CheckpointUnavailable, "fixture stopped")
                })?
                .forget();
        }
        if self
            .fail_kind
            .is_some_and(|kind| payload.events.iter().any(|event| event.kind == kind))
        {
            return Err(CoreError::rejected(
                ErrorCode::CheckpointUnavailable,
                "injected commit failure",
            ));
        }
        let mut store = self.store.lock().await;
        let prior = store
            .batches
            .last()
            .and_then(|batch| store.acknowledgements.get(&batch.identity.batch_id))
            .cloned();
        let mut ack = store.commit(&batch)?;
        if payload
            .events
            .iter()
            .any(|event| event.kind == "input.accepted")
        {
            match self.wrong_ack {
                Some(true) => {
                    return prior.ok_or_else(|| {
                        CoreError::rejected(
                            ErrorCode::CheckpointConflict,
                            "fixture has no prior ACK",
                        )
                    });
                }
                Some(false) => ack.payload_sha256 = "wrong-digest".into(),
                None => {}
            }
        }
        Ok(ack)
    }

    async fn send(&self, message: ServerMessage) -> Result<(), CoreError> {
        if let ServerMessage::ToolExecute(command) = message {
            if self.hold_send {
                self.seen.add_permits(1);
                self.resume
                    .acquire()
                    .await
                    .map_err(|_| {
                        CoreError::rejected(ErrorCode::CheckpointUnavailable, "fixture stopped")
                    })?
                    .forget();
            }
            let store = self.store.lock().await;
            let batch = store.batches.last().ok_or_else(|| {
                CoreError::rejected(
                    ErrorCode::CheckpointUnavailable,
                    "tool has no durable state",
                )
            })?;
            let payload = batch.decode(&store.limits)?;
            let snapshot: SessionSnapshot = serde_json::from_value(payload.checkpoint.state)
                .map_err(|error| {
                    CoreError::rejected(ErrorCode::CheckpointConflict, error.to_string())
                })?;
            let pending = snapshot.run.as_ref().is_some_and(|run| {
                run.invocations
                    .iter()
                    .any(|invocation| invocation.dispatch == command && invocation.result.is_none())
            });
            if !pending
                || command.authorizing_event_seq > store.head.event_seq
                || command.execution_epoch != store.grant.execution_epoch
            {
                return Err(CoreError::rejected(
                    ErrorCode::UnauthorizedScope,
                    "tool has no exact durable dispatch authorization",
                ));
            }
            self.sent.lock().await.push(command);
            self.delivered.add_permits(1);
            drop(store);
            if self.hold_after_send {
                self.resume
                    .acquire()
                    .await
                    .map_err(|_| {
                        CoreError::rejected(ErrorCode::CheckpointUnavailable, "fixture stopped")
                    })?
                    .forget();
            }
        }
        Ok(())
    }
}

struct RecordingExecutor {
    mock: MockExecutor,
    prompts: Mutex<Vec<Prompt>>,
    calls: AtomicUsize,
}

#[async_trait]
impl Executor for RecordingExecutor {
    async fn execute(
        &self,
        target: &RoutingTarget,
        prompt: &Prompt,
        ctx: &PipelineContext,
    ) -> bitrouter_sdk::Result<ExecutionResult> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.prompts.lock().await.push(prompt.clone());
        self.mock.execute(target, prompt, ctx).await
    }
    async fn execute_stream(
        &self,
        _: &RoutingTarget,
        _: &Prompt,
        _: &PipelineContext,
    ) -> bitrouter_sdk::Result<StreamPartStream> {
        Err(bitrouter_sdk::BitrouterError::internal(
            "unexpected streaming execution",
        ))
    }
}

struct Recorder(Arc<AtomicUsize>);
#[async_trait]
impl SettlementRecorder for Recorder {
    async fn record(&self, _: &mut SettlementContext) -> bitrouter_sdk::Result<()> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

struct DisconnectOnHop(Arc<Mutex<Option<CoreSession>>>);

#[async_trait]
impl ObserveHook for DisconnectOnHop {
    async fn after_phase(&self, _: Phase, _: &PipelineContext) {}
    async fn on_stream_part(&self, _: &StreamContext, _: &StreamPart) {}
    async fn on_request_end(&self, _: &PipelineContext, _: &RequestOutcome) {}

    async fn on_hop_start(&self, _: &PipelineContext, _: &RoutingTarget) {
        if let Some(session) = self.0.lock().await.clone() {
            session.disconnect().await;
        }
    }
}

struct RemoveTools;
impl bitrouter_sdk::app::PromptTransform for RemoveTools {
    fn apply(&self, prompt: &mut Prompt) {
        prompt.tools.clear();
    }
}

struct ForceToolChoice(ToolChoice);
impl bitrouter_sdk::app::PromptTransform for ForceToolChoice {
    fn apply(&self, prompt: &mut Prompt) {
        prompt.tool_choice = Some(self.0.clone());
        prompt.params.parallel_tool_calls = Some(false);
    }
}

fn grant() -> OwnershipGrant {
    OwnershipGrant {
        session_id: "session_1".into(),
        harness_id: "harness_1".into(),
        core_instance_id: "core_1".into(),
        execution_epoch: 1,
    }
}

fn target(provider: &str) -> RoutingTarget {
    RoutingTarget {
        provider_name: provider.into(),
        service_id: "fixture-model".into(),
        api_base: "https://example.invalid".into(),
        api_key: "do-not-checkpoint-this-key".into(),
        api_protocol: ApiProtocol::ChatCompletions,
        chat_token_limit_field: None,
        chat_supports_store: None,
        chat_supports_stream_options: None,
        reasoning_effort: None,
        account_label: None,
        api_key_override: None,
        api_base_override: None,
        auth_scheme: AuthScheme::Bearer,
        headers: Vec::new(),
    }
}

fn input() -> TaskInput {
    TaskInput {
        text: "Inspect the file and report the result".into(),
        model: "fixture-model".into(),
        effort: None,
        routing: RoutingSettings::default(),
        acceptance_criteria: vec!["Use the actual tool result".into()],
        required_materials: Vec::new(),
        verification: None,
        limits: None,
    }
}

fn text(value: &str) -> Content {
    Content::Text {
        text: value.into(),
        provider_metadata: Default::default(),
    }
}
fn call(call_id: &str) -> Content {
    Content::ToolCall {
        id: call_id.into(),
        name: "read".into(),
        arguments: "{\"path\":\"file.txt\"}".into(),
        provider_executed: false,
        dynamic: false,
        provider_metadata: Default::default(),
    }
}
fn output(content: Vec<Content>) -> MockResponse {
    let calls = content
        .iter()
        .any(|part| matches!(part, Content::ToolCall { .. }));
    MockResponse::Generate(GenerateResult {
        content,
        usage: Some(Usage {
            prompt_tokens: 7,
            completion_tokens: 3,
            ..Default::default()
        }),
        finish_reason: Some(if calls {
            FinishReason::ToolCalls
        } else {
            FinishReason::Stop
        }),
        response_id: None,
        stop_details: None,
        provider_metadata: Default::default(),
    })
}

async fn setup(
    responses: Vec<MockResponse>,
    harness: Arc<Harness>,
    fallback: bool,
) -> Result<(CoreSession, Arc<RecordingExecutor>, Arc<AtomicUsize>), Box<dyn std::error::Error>> {
    let table = StaticRoutingTable::new();
    let mut targets = vec![target("first")];
    if fallback {
        targets.push(target("second"));
    }
    table.insert("fixture-model", targets);
    let executor = Arc::new(RecordingExecutor {
        mock: MockExecutor::new(responses),
        prompts: Mutex::new(Vec::new()),
        calls: AtomicUsize::new(0),
    });
    let settlements = Arc::new(AtomicUsize::new(0));
    let app = App::builder()
        .language_model(|builder| {
            builder
                .routing_table(Arc::new(table))
                .executor(executor.clone())
                .settlement_recorder(Recorder(settlements.clone()));
        })
        .build()?;
    let session = bind_app(Arc::new(app), harness).await?;
    Ok((session, executor, settlements))
}

async fn bind_app(
    app: Arc<App>,
    harness: Arc<Harness>,
) -> Result<CoreSession, Box<dyn std::error::Error>> {
    let tools = vec![HarnessTool {
        name: "read".into(),
        description: "Read a file".into(),
        parameters: json!({"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}),
        effect: ToolEffect::Read,
        approval_required: false,
    }];
    let manifest = HarnessManifest {
        tool_manifest_digest: HarnessManifest::digest(&tools)?,
        tools,
        workspace_id: "workspace_1".into(),
        workspace_revision: None,
        permission_revision: 1,
        max_tool_output_bytes: 8192,
        artifact_quota_bytes: 1024 * 1024,
        max_artifact_chunk_bytes: 8192,
        required_features: Vec::new(),
    };
    let caps = Capabilities {
        version: 1,
        core_instance_id: "core_1".into(),
        operations: Vec::new(),
        transports: vec!["in_process".into()],
        unsupported_features: Vec::new(),
        limits: Limits::default(),
        max_sessions: 16,
        max_host_model_attempts: 16,
    };
    let session = CoreSession::bind(
        Bind {
            grant: grant(),
            durable_head: DurableHead::default(),
            checkpoint: None,
            manifest,
            limits: Limits::default(),
        },
        &caps,
        app,
        CallerContext::local(),
        harness,
    )
    .await?;
    Ok(session)
}

fn result(command: &ToolExecute) -> ToolResult {
    ToolResult {
        invocation_id: command.invocation_id.clone(),
        attempt_id: command.attempt_id.clone(),
        status: ToolOutcome::Succeeded,
        output: "actual file contents".into(),
        evidence: Vec::new(),
        workspace_revision: Some("workspace-v2".into()),
    }
}

#[tokio::test]
async fn root_tool_roundtrip_preserves_attribution_and_durable_order() -> TestResult {
    let harness = Arc::new(Harness::new(None, None));
    let (session, executor, settlements) = setup(
        vec![
            output(vec![call("provider-call")]),
            output(vec![text("verified report")]),
        ],
        harness.clone(),
        false,
    )
    .await?;
    let receipt = session.start("input_1", 1, input()).await?;
    let waiting = session.drive().await?;
    assert_eq!(
        waiting.run.as_ref().map(|run| run.status),
        Some(RunStatus::Waiting)
    );
    assert_eq!(executor.calls.load(Ordering::SeqCst), 1);
    let command = harness
        .sent
        .lock()
        .await
        .first()
        .cloned()
        .ok_or("missing tool dispatch")?;
    assert_eq!(
        receipt.assigned_ids.get("agent_id"),
        Some(&command.agent_id)
    );
    assert_eq!(receipt.assigned_ids.get("run_id"), Some(&command.run_id));
    let mut report = result(&command);
    let evidence = b"tool evidence";
    let artifact = ArtifactRef {
        artifact_id: "evidence_1".into(),
        sha256: sha256(evidence),
        bytes: evidence.len() as u64,
        media_type: "text/plain".into(),
    };
    harness
        .store
        .lock()
        .await
        .put_artifact(artifact.clone(), evidence)?;
    report.evidence.push(artifact);
    let result_receipt = session.tool_result("result_1", report.clone()).await?;
    assert_eq!(
        session.tool_result("result_1", report).await?,
        result_receipt
    );
    let done = session.drive().await?;
    assert_eq!(
        done.run.as_ref().map(|run| run.status),
        Some(RunStatus::Completed)
    );
    assert_eq!(executor.calls.load(Ordering::SeqCst), 2);
    assert_eq!(settlements.load(Ordering::SeqCst), 2);
    assert_eq!(harness.sent.lock().await.len(), 1);
    let prompts = executor.prompts.lock().await;
    assert!(prompts[1].messages.iter().flat_map(|message| &message.content).any(|part| matches!(part, Content::ToolResult { call_id, .. } if call_id == "provider-call")));
    let kinds = harness.committed_kinds().await?;
    assert_eq!(kinds.last().map(String::as_str), Some("run.completed"));
    let outcome = kinds
        .iter()
        .position(|kind| kind == "model.attempt.outcome")
        .ok_or("no output event")?;
    let apply = kinds
        .iter()
        .position(|kind| kind == "model.output.applied")
        .ok_or("no application event")?;
    assert!(outcome < apply);
    let store = harness.store.lock().await;
    for batch in &store.batches {
        assert!(
            !serde_json::to_string(&batch.decode(&store.limits)?)?
                .contains("do-not-checkpoint-this-key")
        );
    }
    Ok(())
}

#[tokio::test]
async fn model_dispatch_waits_for_attempt_ack_and_state_reads_stay_live() -> TestResult {
    let harness = Arc::new(Harness::new(None, Some("model.attempt.intent")));
    let (session, executor, _) =
        setup(vec![output(vec![text("answer")])], harness.clone(), false).await?;
    session.start("input_1", 1, input()).await?;
    let running = tokio::spawn({
        let session = session.clone();
        async move { session.drive().await }
    });
    tokio::time::timeout(Duration::from_secs(5), harness.seen.acquire())
        .await??
        .forget();
    assert_eq!(executor.calls.load(Ordering::SeqCst), 0);
    let snapshot = tokio::time::timeout(Duration::from_secs(1), session.snapshot()).await?;
    assert_eq!(snapshot.run.as_ref().map(|run| run.model_attempts), Some(0));
    assert_eq!(
        session.drive().await.err().map(|error| error.code),
        Some(ErrorCode::Busy)
    );
    harness.resume.add_permits(1);
    assert_eq!(
        running.await??.run.map(|run| run.status),
        Some(RunStatus::Completed)
    );
    Ok(())
}

#[tokio::test]
async fn failed_attempt_intent_never_reaches_provider() -> TestResult {
    let harness = Arc::new(Harness::new(Some("model.attempt.intent"), None));
    let (session, executor, _) = setup(vec![output(vec![text("answer")])], harness, false).await?;
    session.start("input_1", 1, input()).await?;
    assert!(session.drive().await.is_err());
    assert_eq!(executor.calls.load(Ordering::SeqCst), 0);
    Ok(())
}

#[tokio::test]
async fn failed_output_commit_blocks_tools_but_sdk_still_settles_usage() -> TestResult {
    let harness = Arc::new(Harness::new(Some("model.attempt.outcome"), None));
    let (session, executor, settlements) =
        setup(vec![output(vec![call("call_1")])], harness.clone(), false).await?;
    session.start("input_1", 1, input()).await?;
    assert!(session.drive().await.is_err());
    assert_eq!(executor.calls.load(Ordering::SeqCst), 1);
    assert_eq!(settlements.load(Ordering::SeqCst), 1);
    assert!(harness.sent.lock().await.is_empty());
    assert!(
        session
            .snapshot()
            .await
            .run
            .as_ref()
            .and_then(|run| run.steps.last())
            .and_then(|step| step.attempts.last())
            .is_some_and(|attempt| attempt.report.is_none())
    );
    Ok(())
}

#[tokio::test]
async fn fallbacks_have_separate_committed_attempts_and_actual_providers() -> TestResult {
    let harness = Arc::new(Harness::new(None, None));
    let (session, executor, _) = setup(
        vec![
            MockResponse::Error(bitrouter_sdk::BitrouterError::Upstream {
                status: 503,
                message: "fixture retry".into(),
            }),
            output(vec![text("fallback answer")]),
        ],
        harness.clone(),
        true,
    )
    .await?;
    session.start("input_1", 1, input()).await?;
    let state = session.drive().await?;
    let run = state.run.ok_or("missing run")?;
    assert_eq!(run.status, RunStatus::Completed);
    assert_eq!(run.model_attempts, 2);
    assert_eq!(run.steps.len(), 1);
    assert_ne!(
        run.steps[0].attempts[0].attempt_id,
        run.steps[0].attempts[1].attempt_id
    );
    assert_eq!(
        run.steps[0].attempts[1]
            .report
            .as_ref()
            .map(|report| report.route.provider.as_str()),
        Some("second")
    );
    assert_eq!(executor.calls.load(Ordering::SeqCst), 2);
    let kinds = harness.committed_kinds().await?;
    assert_eq!(
        kinds
            .iter()
            .filter(|kind| kind.as_str() == "model.plan")
            .count(),
        1
    );
    assert_eq!(
        kinds
            .iter()
            .filter(|kind| kind.as_str() == "model.attempt.intent")
            .count(),
        2
    );
    Ok(())
}

#[tokio::test]
async fn concurrent_identical_input_is_accepted_once_and_conflicts_reject() -> TestResult {
    let harness = Arc::new(Harness::new(None, None));
    let (session, _, _) = setup(vec![output(vec![text("done")])], harness.clone(), false).await?;
    let (first, second) = tokio::join!(
        session.start("input_1", 1, input()),
        session.start("input_1", 1, input())
    );
    assert_eq!(first?, second?);
    assert_eq!(session.head().await.state_revision, 2);
    let mut changed = input();
    changed.text = "different".into();
    assert_eq!(
        session
            .start("input_1", 1, changed)
            .await
            .err()
            .map(|error| error.code),
        Some(ErrorCode::OperationConflict)
    );
    assert_eq!(
        harness
            .committed_kinds()
            .await?
            .iter()
            .filter(|kind| kind.as_str() == "input.accepted")
            .count(),
        1
    );
    Ok(())
}

#[tokio::test]
async fn malformed_or_partial_calls_never_dispatch_tools() -> TestResult {
    for content in [
        vec![call("same"), call("same")],
        vec![Content::ToolCall {
            id: "id".into(),
            name: "read".into(),
            arguments: "{".into(),
            provider_executed: false,
            dynamic: false,
            provider_metadata: Default::default(),
        }],
    ] {
        let harness = Arc::new(Harness::new(None, None));
        let (session, _, _) = setup(vec![output(content)], harness.clone(), false).await?;
        session.start("input_1", 1, input()).await?;
        assert!(session.drive().await.is_err());
        assert_eq!(
            session.snapshot().await.run.map(|run| run.status),
            Some(RunStatus::Failed)
        );
        assert!(harness.sent.lock().await.is_empty());
    }
    Ok(())
}

#[tokio::test]
async fn tool_attempt_mismatch_and_conflicting_results_do_not_consume() -> TestResult {
    let harness = Arc::new(Harness::new(None, None));
    let (session, _, _) = setup(
        vec![output(vec![call("provider_1")])],
        harness.clone(),
        false,
    )
    .await?;
    session.start("input_1", 1, input()).await?;
    session.drive().await?;
    let command = harness.sent.lock().await[0].clone();
    let mut bad = result(&command);
    bad.attempt_id = "wrong".into();
    assert_eq!(
        session
            .tool_result("bad", bad)
            .await
            .err()
            .map(|error| error.code),
        Some(ErrorCode::InvalidToolResult)
    );
    session.tool_result("result_1", result(&command)).await?;
    let mut conflict = result(&command);
    conflict.output = "different".into();
    assert_eq!(
        session
            .tool_result("result_2", conflict)
            .await
            .err()
            .map(|error| error.code),
        Some(ErrorCode::OperationConflict)
    );
    Ok(())
}

#[tokio::test]
async fn unknown_effect_blocks_next_model_and_verification_uses_tool_path() -> TestResult {
    let harness = Arc::new(Harness::new(None, None));
    let (session, executor, _) = setup(
        vec![output(vec![text("provisional answer")])],
        harness.clone(),
        false,
    )
    .await?;
    let mut task = input();
    task.verification = Some(Verification {
        tool: "read".into(),
        arguments: json!({"path":"verification.txt"}),
    });
    session.start("input_1", 1, task).await?;
    let waiting = session.drive().await?;
    assert_eq!(waiting.run.map(|run| run.status), Some(RunStatus::Waiting));
    let command = harness.sent.lock().await[0].clone();
    assert!(command.verification);
    let mut unknown = result(&command);
    unknown.status = ToolOutcome::EffectUnknown;
    session.tool_result("result_1", unknown).await?;
    assert_eq!(
        session.drive().await?.run.map(|run| run.status),
        Some(RunStatus::RecoveryRequired)
    );
    assert_eq!(executor.calls.load(Ordering::SeqCst), 1);
    Ok(())
}

#[tokio::test]
async fn terminal_commit_failure_never_publishes_completed_state() -> TestResult {
    let harness = Arc::new(Harness::new(Some("run.completed"), None));
    let (session, _, _) = setup(
        vec![output(vec![text("provisional")])],
        harness.clone(),
        false,
    )
    .await?;
    session.start("input_1", 1, input()).await?;
    assert!(session.drive().await.is_err());
    assert_eq!(
        session.snapshot().await.run.map(|run| run.status),
        Some(RunStatus::Running)
    );
    assert!(
        !harness
            .committed_kinds()
            .await?
            .iter()
            .any(|kind| kind == "run.completed")
    );
    Ok(())
}

#[tokio::test]
async fn disconnect_during_attempt_ack_blocks_provider_dispatch() -> TestResult {
    let harness = Arc::new(Harness::new(None, Some("model.attempt.intent")));
    let (session, executor, _) =
        setup(vec![output(vec![text("answer")])], harness.clone(), false).await?;
    session.start("input_1", 1, input()).await?;
    let running = tokio::spawn({
        let session = session.clone();
        async move { session.drive().await }
    });
    tokio::time::timeout(Duration::from_secs(5), harness.seen.acquire())
        .await??
        .forget();
    session.disconnect().await;
    harness.resume.add_permits(1);
    assert!(running.await?.is_err());
    assert_eq!(executor.calls.load(Ordering::SeqCst), 0);
    Ok(())
}

#[tokio::test]
async fn abandoned_driver_cannot_start_a_duplicate_model_step() -> TestResult {
    let harness = Arc::new(Harness::new(None, Some("model.attempt.intent")));
    let (session, executor, _) =
        setup(vec![output(vec![text("answer")])], harness.clone(), false).await?;
    let mut task = input();
    task.limits = Some(Limits {
        model_attempts: 1,
        ..Limits::default()
    });
    session.start("input_1", 1, task).await?;
    let running = tokio::spawn({
        let session = session.clone();
        async move { session.drive().await }
    });
    tokio::time::timeout(Duration::from_secs(5), harness.seen.acquire())
        .await??
        .forget();
    running.abort();
    assert!(running.await.is_err());
    harness.resume.add_permits(1);
    let resumed = tokio::time::timeout(Duration::from_secs(5), session.drive()).await?;
    assert!(resumed.is_err());
    assert!(executor.calls.load(Ordering::SeqCst) <= 1);
    assert_eq!(
        session
            .snapshot()
            .await
            .run
            .as_ref()
            .map(|run| run.steps.len()),
        Some(1)
    );
    Ok(())
}

#[tokio::test]
async fn frozen_tool_choice_restricts_outputs_but_not_explicit_verification() -> TestResult {
    for (choice, content, verify) in [
        (ToolChoice::None, vec![call("call_1")], false),
        (
            ToolChoice::Tool {
                name: "other".into(),
            },
            vec![call("call_1")],
            false,
        ),
        (ToolChoice::Required, vec![text("answer")], false),
        (
            ToolChoice::Tool {
                name: "read".into(),
            },
            vec![text("answer")],
            false,
        ),
        (
            ToolChoice::Auto,
            vec![call("call_1"), call("call_2")],
            false,
        ),
        (ToolChoice::None, vec![text("answer")], true),
    ] {
        let table = StaticRoutingTable::new();
        table.insert("fixture-model", vec![target("first")]);
        let app = App::builder()
            .prompt_transform(Arc::new(ForceToolChoice(choice)))
            .language_model(|builder| {
                builder
                    .routing_table(Arc::new(table))
                    .executor(Arc::new(MockExecutor::new(vec![output(content)])));
            })
            .build()?;
        let harness = Arc::new(Harness::new(None, None));
        let session = bind_app(Arc::new(app), harness.clone()).await?;
        let mut task = input();
        if verify {
            task.verification = Some(Verification {
                tool: "read".into(),
                arguments: json!({"path":"verification.txt"}),
            });
        }
        session.start("input_1", 1, task).await?;
        let driven = session.drive().await;
        if verify {
            assert_eq!(driven?.run.map(|run| run.status), Some(RunStatus::Waiting));
            assert!(harness.sent.lock().await[0].verification);
        } else {
            assert!(driven.is_err());
            assert_eq!(
                session.snapshot().await.run.map(|run| run.status),
                Some(RunStatus::Failed)
            );
            assert!(harness.sent.lock().await.is_empty());
        }
    }
    Ok(())
}

#[tokio::test]
async fn observer_disconnect_prevents_attempt_and_transformed_tools_remain_frozen() -> TestResult {
    for disconnect in [true, false] {
        let table = StaticRoutingTable::new();
        table.insert("fixture-model", vec![target("first")]);
        let executor = Arc::new(RecordingExecutor {
            mock: MockExecutor::new(vec![output(vec![call("call_1")])]),
            prompts: Mutex::new(Vec::new()),
            calls: AtomicUsize::new(0),
        });
        let holder = Arc::new(Mutex::new(None));
        let app = App::builder()
            .prompt_transform(Arc::new(RemoveTools))
            .language_model(|builder| {
                builder
                    .routing_table(Arc::new(table))
                    .executor(executor.clone());
                if disconnect {
                    builder.observe_hook(DisconnectOnHop(holder.clone()));
                }
            })
            .build()?;
        let harness = Arc::new(Harness::new(None, None));
        let session = bind_app(Arc::new(app), harness.clone()).await?;
        *holder.lock().await = Some(session.clone());
        session.start("input_1", 1, input()).await?;
        assert!(session.drive().await.is_err());
        assert_eq!(
            executor.calls.load(Ordering::SeqCst),
            usize::from(!disconnect)
        );
        assert!(harness.sent.lock().await.is_empty());
        if !disconnect {
            assert!(executor.prompts.lock().await[0].tools.is_empty());
            assert_eq!(
                session.snapshot().await.run.map(|run| run.status),
                Some(RunStatus::Failed)
            );
        }
        *holder.lock().await = None;
    }
    Ok(())
}

#[tokio::test]
async fn disconnected_output_ack_releases_usage_settlement() -> TestResult {
    let harness = Arc::new(Harness::new(None, Some("model.attempt.outcome")));
    let (session, executor, settlements) =
        setup(vec![output(vec![call("call_1")])], harness.clone(), false).await?;
    session.start("input_1", 1, input()).await?;
    let running = tokio::spawn({
        let session = session.clone();
        async move { session.drive().await }
    });
    tokio::time::timeout(Duration::from_secs(5), harness.seen.acquire())
        .await??
        .forget();
    assert_eq!(executor.calls.load(Ordering::SeqCst), 1);
    session.disconnect().await;
    assert!(
        tokio::time::timeout(Duration::from_secs(5), running)
            .await??
            .is_err()
    );
    assert_eq!(settlements.load(Ordering::SeqCst), 1);
    assert!(harness.sent.lock().await.is_empty());
    assert_eq!(
        session.snapshot().await.run.map(|run| run.status),
        Some(RunStatus::Running)
    );
    Ok(())
}

#[tokio::test]
async fn invalid_verification_and_input_bounds_reject_before_acceptance() -> TestResult {
    let harness = Arc::new(Harness::new(None, None));
    let (session, executor, _) = setup(vec![output(vec![text("answer")])], harness, false).await?;
    for verification in [
        Verification {
            tool: "undeclared".into(),
            arguments: json!({}),
        },
        Verification {
            tool: "read".into(),
            arguments: json!("not an object"),
        },
    ] {
        let mut task = input();
        task.verification = Some(verification);
        let rejected = session
            .start("invalid", 1, task)
            .await
            .err()
            .ok_or("accepted invalid verification")?;
        assert_eq!(rejected.code, ErrorCode::NoFeasibleRoute);
        assert_eq!(rejected.commit_status, CommitStatus::NotCommitted);
    }
    let mut task = input();
    task.limits = Some(Limits {
        input_bytes: 1,
        ..Limits::default()
    });
    assert_eq!(
        session
            .start("too_large", 1, task)
            .await
            .err()
            .map(|error| error.code),
        Some(ErrorCode::LimitExceeded)
    );
    assert_eq!(session.head().await.state_revision, 1);
    assert!(session.snapshot().await.run.is_none());
    assert_eq!(executor.calls.load(Ordering::SeqCst), 0);
    session.start("valid", 1, input()).await?;
    assert_eq!(
        session.drive().await?.run.map(|run| run.status),
        Some(RunStatus::Completed)
    );
    Ok(())
}

#[tokio::test]
async fn cancelling_driver_during_send_preserves_the_delivery_attempt() -> TestResult {
    let mut harness = Harness::new(None, None);
    harness.hold_send = true;
    let harness = Arc::new(harness);
    let (session, _, _) = setup(vec![output(vec![call("call_1")])], harness.clone(), false).await?;
    session.start("input_1", 1, input()).await?;
    let running = tokio::spawn({
        let session = session.clone();
        async move { session.drive().await }
    });
    tokio::time::timeout(Duration::from_secs(5), harness.seen.acquire())
        .await??
        .forget();
    running.abort();
    assert!(running.await.is_err());
    assert!(harness.sent.lock().await.is_empty());
    assert_eq!(
        session.drive().await?.run.map(|run| run.status),
        Some(RunStatus::Waiting)
    );
    harness.resume.add_permits(1);
    tokio::time::timeout(Duration::from_secs(5), harness.delivered.acquire())
        .await??
        .forget();
    let sent = harness.sent.lock().await;
    assert_eq!(sent.len(), 1);
    session.tool_result("result_1", result(&sent[0])).await?;
    Ok(())
}

#[tokio::test]
async fn unknown_effect_stops_remaining_commands_in_the_same_tool_batch() -> TestResult {
    let mut harness = Harness::new(None, None);
    harness.hold_after_send = true;
    let harness = Arc::new(harness);
    let (session, _, _) = setup(
        vec![output(vec![call("call_1"), call("call_2")])],
        harness.clone(),
        false,
    )
    .await?;
    session.start("input_1", 1, input()).await?;
    let running = tokio::spawn({
        let session = session.clone();
        async move { session.drive().await }
    });
    tokio::time::timeout(Duration::from_secs(5), harness.delivered.acquire())
        .await??
        .forget();
    let mut unknown = result(&harness.sent.lock().await[0]);
    unknown.status = ToolOutcome::EffectUnknown;
    session.tool_result("unknown", unknown).await?;
    harness.resume.add_permits(1);
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), running)
            .await??
            .err()
            .map(|error| error.code),
        Some(ErrorCode::RecoveryRequired)
    );
    assert_eq!(harness.sent.lock().await.len(), 1);
    assert_eq!(
        session.snapshot().await.run.map(|run| run.status),
        Some(RunStatus::RecoveryRequired)
    );
    Ok(())
}

#[tokio::test]
async fn incorrect_ack_reports_unknown_and_keeps_submitted_input_tentative() -> TestResult {
    for replay_prior in [false, true] {
        let mut harness = Harness::new(None, None);
        harness.wrong_ack = Some(replay_prior);
        let harness = Arc::new(harness);
        let (session, executor, _) =
            setup(vec![output(vec![text("answer")])], harness.clone(), false).await?;
        let error = session
            .start("input_1", 1, input())
            .await
            .err()
            .ok_or("accepted wrong ACK")?;
        assert_eq!(error.commit_status, CommitStatus::Unknown);
        assert_eq!(session.head().await.state_revision, 1);
        assert!(session.snapshot().await.run.is_none());
        assert_eq!(harness.store.lock().await.head.state_revision, 2);
        assert_eq!(
            session
                .start("input_1", 1, input())
                .await
                .err()
                .map(|error| error.commit_status),
            Some(CommitStatus::Unknown)
        );
        assert!(session.drive().await.is_err());
        assert_eq!(executor.calls.load(Ordering::SeqCst), 0);
    }
    Ok(())
}

#[tokio::test]
async fn truncated_output_cannot_authorize_an_otherwise_valid_tool() -> TestResult {
    let harness = Arc::new(Harness::new(None, None));
    let response = MockResponse::Generate(GenerateResult {
        content: vec![call("call_1")],
        finish_reason: Some(FinishReason::Length),
        usage: None,
        response_id: None,
        stop_details: None,
        provider_metadata: Default::default(),
    });
    let (session, _, settlements) = setup(vec![response], harness.clone(), false).await?;
    session.start("input_1", 1, input()).await?;
    assert!(session.drive().await.is_err());
    assert_eq!(
        session.snapshot().await.run.map(|run| run.status),
        Some(RunStatus::Failed)
    );
    assert!(harness.sent.lock().await.is_empty());
    assert_eq!(settlements.load(Ordering::SeqCst), 1);
    Ok(())
}

#[tokio::test]
async fn http_provider_request_and_fixture_file_tool_complete_a_root_task() -> TestResult {
    use bitrouter_sdk::language_model::executor::HttpExecutor;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let provider = tokio::spawn(async move {
        let mut requests = Vec::new();
        for turn in 0..2 {
            let (mut socket, _) = listener.accept().await?;
            let mut bytes = Vec::new();
            let (header_end, body_len) = loop {
                let mut part = [0u8; 4096];
                let count = socket.read(&mut part).await?;
                if count == 0 {
                    return Err(std::io::Error::other("incomplete fixture request"));
                }
                bytes.extend_from_slice(&part[..count]);
                if bytes.len() > 128 * 1024 {
                    return Err(std::io::Error::other("oversized fixture request"));
                }
                if let Some(end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
                    let headers =
                        std::str::from_utf8(&bytes[..end]).map_err(std::io::Error::other)?;
                    assert!(headers.starts_with("POST /v1/chat/completions HTTP/1.1"));
                    let length = headers
                        .lines()
                        .find_map(|line| {
                            line.split_once(':')
                                .filter(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                                .map(|(_, value)| value.trim().parse::<usize>())
                        })
                        .transpose()
                        .map_err(std::io::Error::other)?
                        .ok_or_else(|| std::io::Error::other("missing request length"))?;
                    break (end + 4, length);
                }
            };
            while bytes.len() < header_end + body_len {
                let mut part = [0u8; 4096];
                let count = socket.read(&mut part).await?;
                if count == 0 {
                    return Err(std::io::Error::other("incomplete request body"));
                }
                bytes.extend_from_slice(&part[..count]);
            }
            let body: serde_json::Value =
                serde_json::from_slice(&bytes[header_end..header_end + body_len])
                    .map_err(std::io::Error::other)?;
            assert!(body.get("multi_agent").is_none());
            assert!(body.get("bitrouter").is_none());
            requests.push(body);
            // The existing SDK Chat Completions adapter consumes this fixture:
            // https://platform.openai.com/docs/api-reference/chat/object
            let message = if turn == 0 {
                json!({"role":"assistant","content":null,"tool_calls":[{"id":"wire_call_1","type":"function","function":{"name":"read","arguments":"{\"path\":\"file.txt\"}"}}]})
            } else {
                json!({"role":"assistant","content":"The file says real fixture content."})
            };
            let response = json!({"id":format!("chatcmpl-{turn}"),"object":"chat.completion","created":0,"model":"fixture-model","choices":[{"index":0,"message":message,"finish_reason":if turn == 0 { "tool_calls" } else { "stop" }}],"usage":{"prompt_tokens":12,"completion_tokens":4,"total_tokens":16}}).to_string();
            socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", response.len(), response).as_bytes()).await?;
        }
        Ok::<_, std::io::Error>(requests)
    });
    let table = StaticRoutingTable::new();
    let mut route = target("wire-fixture");
    route.api_base = format!("http://{address}/v1");
    table.insert("fixture-model", vec![route]);
    let executor = HttpExecutor::with_defaults()?;
    let app = App::builder()
        .language_model(|builder| {
            builder
                .routing_table(Arc::new(table))
                .executor(Arc::new(executor));
        })
        .build()?;
    let harness = Arc::new(Harness::new(None, None));
    let session = bind_app(Arc::new(app), harness.clone()).await?;
    session.start("input_1", 1, input()).await?;
    session.drive().await?;
    let command = harness.sent.lock().await[0].clone();
    assert_eq!(command.tool, "read");
    assert_eq!(command.arguments, json!({"path":"file.txt"}));
    let workspace = tempfile::tempdir()?;
    tokio::fs::write(workspace.path().join("file.txt"), "real fixture content").await?;
    let mut report = result(&command);
    report.output = tokio::fs::read_to_string(workspace.path().join("file.txt")).await?;
    session.tool_result("result_1", report).await?;
    let done = session.drive().await?;
    assert_eq!(
        done.run.as_ref().map(|run| run.status),
        Some(RunStatus::Completed)
    );
    let requests = tokio::time::timeout(Duration::from_secs(5), provider).await???;
    assert_eq!(requests.len(), 2);
    assert!(
        requests[1]["messages"]
            .as_array()
            .is_some_and(
                |messages| messages.iter().any(|message| message["role"] == "tool"
                    && message["content"] == "real fixture content"
                    && message["tool_call_id"] == "wire_call_1")
            )
    );
    Ok(())
}
