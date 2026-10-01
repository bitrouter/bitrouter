mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use bitrouter_orchestrator::core::checkpoint::{
    CheckpointAck, CheckpointBatch, DurableHead, sha256,
};
use bitrouter_orchestrator::core::collaboration::{Action, Work};
use bitrouter_orchestrator::core::protocol::{
    ArtifactRef, Bind, Capabilities, CommitStatus, CoreError, ErrorCode, HarnessManifest,
    HarnessTool, Limits, MaterialRef, OwnershipGrant, RoutingSettings, ServerMessage, SignalUpdate,
    TaskInput, ToolEffect, ToolExecute, ToolOutcome, ToolResult, Verification,
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
    cancelled: Mutex<Vec<(String, String, u64)>>,
    cancel_seen: Semaphore,
    material_requests: Mutex<Vec<(String, String, String)>>,
    fail_kind: Option<&'static str>,
    hold_kind: Option<&'static str>,
    hold_enabled: AtomicBool,
    wrong_ack: Option<bool>,
    hold_send: bool,
    hold_after_send: bool,
    hold_material_send: bool,
    seen: Semaphore,
    resume: Semaphore,
    delivered: Semaphore,
}

impl Harness {
    fn new(fail_kind: Option<&'static str>, hold_kind: Option<&'static str>) -> Self {
        Self {
            store: Mutex::new(DurableHarness::new(grant())),
            sent: Mutex::new(Vec::new()),
            cancelled: Mutex::new(Vec::new()),
            cancel_seen: Semaphore::new(0),
            material_requests: Mutex::new(Vec::new()),
            fail_kind,
            hold_kind,
            hold_enabled: AtomicBool::new(true),
            wrong_ack: None,
            hold_send: false,
            hold_after_send: false,
            hold_material_send: false,
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
            && self.hold_enabled.load(Ordering::SeqCst)
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
        if let ServerMessage::MaterialRequest {
            request_id,
            material_id,
            version,
        } = &message
        {
            if self.hold_material_send {
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
            let payload = store
                .batches
                .last()
                .ok_or_else(|| {
                    CoreError::rejected(
                        ErrorCode::CheckpointUnavailable,
                        "missing material checkpoint",
                    )
                })?
                .decode(&store.limits)?;
            let state: SessionSnapshot =
                serde_json::from_value(payload.checkpoint.state).map_err(|error| {
                    CoreError::rejected(ErrorCode::CheckpointConflict, error.to_string())
                })?;
            if state
                .signals
                .requests
                .get(request_id)
                .is_none_or(|request| {
                    request.resolved
                        || request.reference.material_id != *material_id
                        || request.reference.version != *version
                })
            {
                return Err(CoreError::rejected(
                    ErrorCode::UnauthorizedScope,
                    "material request has no committed authorization",
                ));
            }
            self.material_requests.lock().await.push((
                request_id.clone(),
                material_id.clone(),
                version.clone(),
            ));
            return Ok(());
        }
        if let ServerMessage::ToolCancel {
            invocation_id,
            attempt_id,
            execution_epoch,
        } = &message
        {
            self.cancelled.lock().await.push((
                invocation_id.clone(),
                attempt_id.clone(),
                *execution_epoch,
            ));
            self.cancel_seen.add_permits(1);
            return Ok(());
        }
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
            let pending = snapshot
                .agents
                .values()
                .filter_map(|agent| agent.turn.as_ref())
                .any(|turn| {
                    turn.invocations.iter().any(|invocation| {
                        invocation.dispatch == command && invocation.result.is_none()
                    })
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

struct ConcurrentExecutor {
    seen: Semaphore,
    release: Semaphore,
    active: AtomicUsize,
    peak: AtomicUsize,
    root_calls: AtomicUsize,
}

struct CollaborationExecutor {
    root_calls: AtomicUsize,
}

struct WakeExecutor {
    child_seen: Semaphore,
    child_release: Semaphore,
    root_resumed: Semaphore,
    root_calls: AtomicUsize,
    child_id: Mutex<Option<String>>,
    wait: bool,
}

#[async_trait]
impl Executor for WakeExecutor {
    async fn execute(
        &self,
        target: &RoutingTarget,
        prompt: &Prompt,
        ctx: &PipelineContext,
    ) -> bitrouter_sdk::Result<ExecutionResult> {
        let child = prompt
            .messages
            .iter()
            .flat_map(|message| &message.content)
            .any(|part| matches!(part,Content::Text{text,..} if text=="held-child"));
        let content = if child {
            self.child_seen.add_permits(2);
            self.child_release
                .acquire()
                .await
                .map_err(|error| bitrouter_sdk::BitrouterError::internal(error.to_string()))?
                .forget();
            vec![text("held child finished")]
        } else if self.root_calls.fetch_add(1, Ordering::SeqCst) == 0 {
            if self.wait {
                self.child_seen
                    .acquire()
                    .await
                    .map_err(|error| bitrouter_sdk::BitrouterError::internal(error.to_string()))?
                    .forget();
                let child = self
                    .child_id
                    .lock()
                    .await
                    .clone()
                    .ok_or_else(|| bitrouter_sdk::BitrouterError::internal("missing child"))?;
                vec![core_call(
                    "wait_agent",
                    json!({"agent_ids":[child],"timeout_ms":50}),
                    "wait",
                )]
            } else {
                vec![call("root-read")]
            }
        } else {
            self.root_resumed.add_permits(1);
            vec![text("root resumed")]
        };
        MockExecutor::new(vec![output(content)])
            .execute(target, prompt, ctx)
            .await
    }
    async fn execute_stream(
        &self,
        _: &RoutingTarget,
        _: &Prompt,
        _: &PipelineContext,
    ) -> bitrouter_sdk::Result<StreamPartStream> {
        Err(bitrouter_sdk::BitrouterError::internal(
            "unexpected streaming",
        ))
    }
}

#[async_trait]
impl Executor for CollaborationExecutor {
    async fn execute(
        &self,
        target: &RoutingTarget,
        prompt: &Prompt,
        ctx: &PipelineContext,
    ) -> bitrouter_sdk::Result<ExecutionResult> {
        let child = prompt.messages.iter().any(|message| message.content.iter().any(|content| matches!(content, Content::Text { text, .. } if text == "child-a" || text == "child-b")));
        let content = if child {
            if prompt
                .messages
                .iter()
                .any(|message| message.role == bitrouter_sdk::language_model::types::Role::Tool)
            {
                vec![text("child tool completed")]
            } else {
                vec![call("same-provider-id")]
            }
        } else {
            match self.root_calls.fetch_add(1, Ordering::SeqCst) {
                0 => vec![
                    core_call("spawn_agent", json!({"task":work("child-a")}), "spawn-a"),
                    core_call("delegate_task", json!({"task":work("child-b")}), "spawn-b"),
                ],
                1 => {
                    let mut ids = Vec::new();
                    for part in prompt.messages.iter().flat_map(|message| &message.content) {
                        if let Content::ToolResult {
                            output:
                                bitrouter_sdk::language_model::types::ToolResultOutput::Text { value },
                            ..
                        } = part
                            && let Ok(value) = serde_json::from_str::<serde_json::Value>(value)
                            && let Some(id) = value["value"]["agent_id"].as_str()
                        {
                            ids.push(id.to_owned());
                        }
                    }
                    vec![core_call(
                        "wait_agent",
                        json!({"agent_ids":ids,"timeout_ms":60000}),
                        "wait-all",
                    )]
                }
                _ => vec![text("parent joined tool evidence")],
            }
        };
        MockExecutor::new(vec![output(content)])
            .execute(target, prompt, ctx)
            .await
    }
    async fn execute_stream(
        &self,
        _: &RoutingTarget,
        _: &Prompt,
        _: &PipelineContext,
    ) -> bitrouter_sdk::Result<StreamPartStream> {
        Err(bitrouter_sdk::BitrouterError::internal(
            "unexpected streaming",
        ))
    }
}

#[async_trait]
impl Executor for ConcurrentExecutor {
    async fn execute(
        &self,
        target: &RoutingTarget,
        prompt: &Prompt,
        ctx: &PipelineContext,
    ) -> bitrouter_sdk::Result<ExecutionResult> {
        let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak.fetch_max(active, Ordering::SeqCst);
        let child = prompt.messages.iter().any(|message| message.content.iter().any(|content| matches!(content, Content::Text { text, .. } if text == "child-a" || text == "child-b")));
        let answer = if child {
            self.seen.add_permits(1);
            self.release
                .acquire()
                .await
                .map_err(|error| bitrouter_sdk::BitrouterError::internal(error.to_string()))?
                .forget();
            "child completed"
        } else if self.root_calls.fetch_add(1, Ordering::SeqCst) == 0 {
            "provisional root answer"
        } else {
            "joined child evidence"
        };
        self.active.fetch_sub(1, Ordering::SeqCst);
        let mock = MockExecutor::always_text(answer);
        mock.execute(target, prompt, ctx).await
    }
    async fn execute_stream(
        &self,
        _: &RoutingTarget,
        _: &Prompt,
        _: &PipelineContext,
    ) -> bitrouter_sdk::Result<StreamPartStream> {
        Err(bitrouter_sdk::BitrouterError::internal(
            "unexpected streaming",
        ))
    }
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

fn work(text: &str) -> Work {
    Work {
        text: text.into(),
        model: None,
        effort: None,
        acceptance_criteria: Vec::new(),
        required_materials: Vec::new(),
        task_scope: Some("fixture-scope".into()),
        fresh_context: true,
        independent_review: false,
    }
}

fn material(version: &str, body: &str, inline: bool) -> MaterialRef {
    MaterialRef {
        material_id: "required_document".into(),
        version: version.into(),
        sha256: sha256(body.as_bytes()),
        media_type: "text/plain".into(),
        provenance: "harness_document".into(),
        required: true,
        artifact: None,
        content: inline.then(|| body.to_owned()),
    }
}

async fn signal_update(session: &CoreSession, materials: Vec<MaterialRef>) -> SignalUpdate {
    let snapshot = session.snapshot().await;
    SignalUpdate {
        signal_revision: snapshot.signals.revision + 1,
        observed_at: "2026-10-01T12:00:00Z".into(),
        scope: snapshot.session_id,
        source: "harness_1".into(),
        workspace_revision: snapshot.manifest.workspace_revision.clone(),
        manifest: snapshot.manifest,
        materials,
        facts: Default::default(),
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

fn core_call(name: &str, arguments: serde_json::Value, call_id: &str) -> Content {
    Content::ToolCall {
        id: call_id.into(),
        name: name.into(),
        arguments: arguments.to_string(),
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
async fn restored_material_inventory_fetches_a_resolved_version_again() -> TestResult {
    let harness = Arc::new(Harness::new(None, None));
    let (session, executor, _) = setup(
        vec![output(vec![text("first")]), output(vec![text("second")])],
        harness.clone(),
        false,
    )
    .await?;
    session
        .signals(
            "first-inventory",
            signal_update(&session, vec![material("v1", "document", false)]).await,
        )
        .await?;
    session
        .start("first", session.head().await.state_revision, input())
        .await?;
    session.drive().await?;
    let first = harness.material_requests.lock().await[0].clone();
    session
        .material_result(
            "first-material",
            &first.0,
            Some(material("v1", "document", true)),
            None,
        )
        .await?;
    session.drive().await?;
    session
        .signals("empty-inventory", signal_update(&session, Vec::new()).await)
        .await?;
    session
        .signals(
            "restored-inventory",
            signal_update(&session, vec![material("v1", "document", false)]).await,
        )
        .await?;
    session
        .start("second", session.head().await.state_revision, input())
        .await?;
    let waiting = session.drive().await?;
    assert_eq!(
        waiting.run.as_ref().map(|run| run.status),
        Some(RunStatus::Waiting)
    );
    assert_eq!(executor.calls.load(Ordering::SeqCst), 1);
    let second = harness.material_requests.lock().await[1].clone();
    assert_ne!(first.0, second.0);
    session
        .material_result(
            "second-material",
            &second.0,
            Some(material("v1", "document", true)),
            None,
        )
        .await?;
    assert_eq!(
        session.drive().await?.run.map(|run| run.status),
        Some(RunStatus::Completed)
    );
    Ok(())
}

#[tokio::test]
async fn material_send_disconnect_releases_the_driver() -> TestResult {
    let mut harness = Harness::new(None, None);
    harness.hold_material_send = true;
    let harness = Arc::new(harness);
    let (session, executor, _) = setup(Vec::new(), harness.clone(), false).await?;
    session
        .signals(
            "inventory",
            signal_update(&session, vec![material("v1", "document", false)]).await,
        )
        .await?;
    session
        .start("input", session.head().await.state_revision, input())
        .await?;
    let driving = tokio::spawn({
        let session = session.clone();
        async move { session.drive().await }
    });
    tokio::time::timeout(Duration::from_secs(5), harness.seen.acquire())
        .await??
        .forget();
    session.disconnect().await;
    assert!(
        tokio::time::timeout(Duration::from_secs(5), driving)
            .await??
            .is_err()
    );
    assert_eq!(executor.calls.load(Ordering::SeqCst), 0);
    assert!(harness.material_requests.lock().await.is_empty());
    let retried = tokio::time::timeout(Duration::from_secs(5), session.drive()).await?;
    assert_ne!(retried.err().map(|error| error.code), Some(ErrorCode::Busy));
    Ok(())
}

#[tokio::test]
async fn conflicting_material_artifact_id_rejects_before_submission() -> TestResult {
    let harness = Arc::new(Harness::new(None, None));
    let (session, _, _) = setup(Vec::new(), harness.clone(), false).await?;
    let mut first = material("v1", "one", true);
    let mut second = material("v1", "two", true);
    second.material_id = "other".into();
    for material in [&mut first, &mut second] {
        material.artifact = Some(ArtifactRef {
            artifact_id: "same-artifact".into(),
            sha256: material.sha256.clone(),
            bytes: 3,
            media_type: material.media_type.clone(),
        });
    }
    let before = session.head().await;
    let error = session
        .signals(
            "conflicting",
            signal_update(&session, vec![first, second]).await,
        )
        .await
        .err()
        .ok_or("accepted conflicting artifact identity")?;
    assert_eq!(error.code, ErrorCode::CheckpointConflict);
    assert_eq!(error.commit_status, CommitStatus::NotCommitted);
    assert_eq!(session.head().await, before);
    assert_eq!(harness.store.lock().await.batches.len(), 1);
    Ok(())
}

#[tokio::test]
async fn admitted_output_bound_and_new_workspace_signal_survive_late_result() -> TestResult {
    let harness = Arc::new(Harness::new(None, None));
    let (session, _, _) = setup(
        vec![output(vec![call("old-read")]), output(vec![text("done")])],
        harness.clone(),
        false,
    )
    .await?;
    let mut initial = signal_update(&session, Vec::new()).await;
    initial.workspace_revision = Some("w1".into());
    initial.manifest.workspace_revision = initial.workspace_revision.clone();
    session.signals("initial", initial).await?;
    session
        .start("input", session.head().await.state_revision, input())
        .await?;
    session.drive().await?;
    let command = harness.sent.lock().await[0].clone();
    let mut updated = signal_update(&session, Vec::new()).await;
    updated.workspace_revision = Some("w2".into());
    updated.manifest.workspace_revision = updated.workspace_revision.clone();
    updated.manifest.max_tool_output_bytes = 1024;
    session.signals("changed", updated).await?;
    let mut report = result(&command);
    report.output = "x".repeat(2048);
    report.workspace_revision = Some("w1".into());
    session.tool_result("late-result", report).await?;
    let snapshot = session.snapshot().await;
    assert_eq!(snapshot.manifest.workspace_revision.as_deref(), Some("w2"));
    assert_eq!(
        snapshot.root_turn().ok_or("missing root")?.invocations[0].result_limit_bytes,
        8192
    );
    assert_eq!(
        session.drive().await?.run.map(|run| run.status),
        Some(RunStatus::Completed)
    );
    Ok(())
}

#[tokio::test]
async fn duplicate_write_result_preserves_newer_workspace_facts() -> TestResult {
    let harness = Arc::new(Harness::new(None, None));
    let (session, _, _) = setup(
        vec![output(vec![call("write-once")])],
        harness.clone(),
        false,
    )
    .await?;
    let mut initial = signal_update(&session, Vec::new()).await;
    initial.manifest.tools[0].effect = ToolEffect::Write;
    initial.manifest.tool_manifest_digest = HarnessManifest::digest(&initial.manifest.tools)?;
    session.signals("initial", initial).await?;
    session
        .start("input", session.head().await.state_revision, input())
        .await?;
    session.drive().await?;
    let report = result(&harness.sent.lock().await[0]);
    session.tool_result("first-result", report.clone()).await?;
    assert_eq!(session.snapshot().await.manifest.workspace_revision, None);
    let mut update = signal_update(&session, Vec::new()).await;
    update.workspace_revision = Some("w2".into());
    update.manifest.workspace_revision = update.workspace_revision.clone();
    session.signals("new-workspace", update).await?;
    session.tool_result("duplicate-result", report).await?;
    assert_eq!(
        session
            .snapshot()
            .await
            .manifest
            .workspace_revision
            .as_deref(),
        Some("w2")
    );
    assert_eq!(harness.sent.lock().await.len(), 1);
    Ok(())
}

#[tokio::test]
async fn late_tool_removal_cannot_authorize_verification() -> TestResult {
    let harness = Arc::new(Harness::new(None, Some("model.attempt.outcome")));
    let (session, executor, _) = setup(
        vec![output(vec![text("provisional final")])],
        harness.clone(),
        false,
    )
    .await?;
    let mut task = input();
    task.verification = Some(Verification {
        tool: "read".into(),
        arguments: json!({"path":"file.txt"}),
    });
    session.start("input", 1, task).await?;
    let driving = tokio::spawn({
        let session = session.clone();
        async move { session.drive().await }
    });
    tokio::time::timeout(Duration::from_secs(5), harness.seen.acquire())
        .await??
        .forget();
    let mut update = signal_update(&session, Vec::new()).await;
    update.manifest.permission_revision += 1;
    update.manifest.tools.clear();
    update.manifest.tool_manifest_digest = HarnessManifest::digest(&update.manifest.tools)?;
    let updating = tokio::spawn({
        let session = session.clone();
        async move { session.signals("removed", update).await }
    });
    tokio::task::yield_now().await;
    harness.hold_enabled.store(false, Ordering::SeqCst);
    harness.resume.add_permits(1);
    updating.await??;
    assert!(driving.await?.is_err());
    assert!(harness.sent.lock().await.is_empty());
    assert_eq!(executor.calls.load(Ordering::SeqCst), 1);
    Ok(())
}

#[tokio::test]
async fn signal_scope_material_identity_and_aggregate_quota_reject_without_mutation() -> TestResult
{
    let harness = Arc::new(Harness::new(None, None));
    let (session, executor, _) = setup(Vec::new(), harness, false).await?;
    let mut foreign = signal_update(&session, Vec::new()).await;
    foreign.scope = "other_session".into();
    let before = session.head().await;
    assert_eq!(
        session
            .signals("foreign", foreign)
            .await
            .err()
            .map(|error| error.code),
        Some(ErrorCode::UnauthorizedScope)
    );
    assert_eq!(session.head().await, before);
    session
        .signals(
            "first",
            signal_update(&session, vec![material("v1", "original", true)]).await,
        )
        .await?;
    session
        .signals("omitted", signal_update(&session, Vec::new()).await)
        .await?;
    let before = session.head().await;
    let conflicting = signal_update(
        &session,
        vec![material("v1", "changed under the same version", true)],
    )
    .await;
    assert_eq!(
        session
            .signals("conflict", conflicting)
            .await
            .err()
            .map(|error| error.code),
        Some(ErrorCode::OperationConflict)
    );
    assert_eq!(session.head().await, before);
    let one = material("v2", &"a".repeat(5000), true);
    let mut two = material("v1", &"b".repeat(5000), true);
    two.material_id = "second_document".into();
    let mut excessive = signal_update(&session, vec![one, two]).await;
    excessive.manifest.artifact_quota_bytes = 8192;
    assert_eq!(
        session
            .signals("excessive", excessive)
            .await
            .err()
            .map(|error| error.code),
        Some(ErrorCode::LimitExceeded)
    );
    assert_eq!(session.head().await, before);
    assert_eq!(executor.calls.load(Ordering::SeqCst), 0);
    Ok(())
}

#[tokio::test]
async fn unavailable_required_material_blocks_until_a_valid_inventory_update() -> TestResult {
    let harness = Arc::new(Harness::new(None, None));
    let (session, executor, _) =
        setup(vec![output(vec![text("resolved")])], harness.clone(), false).await?;
    session
        .signals(
            "first",
            signal_update(&session, vec![material("v1", "document", false)]).await,
        )
        .await?;
    session
        .start("input", session.head().await.state_revision, input())
        .await?;
    session.drive().await?;
    let request = harness.material_requests.lock().await[0].clone();
    session
        .material_result(
            "unavailable",
            &request.0,
            None,
            Some("temporary storage failure".into()),
        )
        .await?;
    assert_eq!(
        session.drive().await.err().map(|error| error.code),
        Some(ErrorCode::ArtifactUnavailable)
    );
    assert_eq!(executor.calls.load(Ordering::SeqCst), 0);
    session
        .signals(
            "resolved",
            signal_update(&session, vec![material("v1", "document", true)]).await,
        )
        .await?;
    assert_eq!(
        session.drive().await?.run.map(|run| run.status),
        Some(RunStatus::Completed)
    );
    assert_eq!(executor.calls.load(Ordering::SeqCst), 1);
    Ok(())
}

#[tokio::test]
async fn required_material_waits_for_request_ack_and_validated_resolution() -> TestResult {
    let harness = Arc::new(Harness::new(None, Some("material.requested")));
    let (session, executor, _) = setup(
        vec![output(vec![text("used required material")])],
        harness.clone(),
        false,
    )
    .await?;
    let reference = material("v1", "required design constraints", false);
    let update = signal_update(&session, vec![reference.clone()]).await;
    let accepted = session.signals("signals", update.clone()).await?;
    assert_eq!(session.signals("signals", update.clone()).await?, accepted);
    let before = session.head().await;
    assert_eq!(
        session
            .signals("stale-signals", update)
            .await
            .err()
            .map(|error| error.code),
        Some(ErrorCode::StaleRevision)
    );
    assert_eq!(session.head().await, before);
    let mut task = input();
    task.required_materials.push(reference.material_id.clone());
    session.start("input", before.state_revision, task).await?;
    let driving = tokio::spawn({
        let session = session.clone();
        async move { session.drive().await }
    });
    tokio::time::timeout(Duration::from_secs(5), harness.seen.acquire())
        .await??
        .forget();
    assert!(harness.material_requests.lock().await.is_empty());
    assert_eq!(executor.calls.load(Ordering::SeqCst), 0);
    harness.resume.add_permits(1);
    let waiting = driving.await??;
    assert_eq!(
        waiting.run.as_ref().map(|run| run.status),
        Some(RunStatus::Waiting)
    );
    let request = harness.material_requests.lock().await[0].clone();
    session.drive().await?;
    assert_eq!(harness.material_requests.lock().await.len(), 1);
    let mut wrong = reference.clone();
    wrong.content = Some("different content".into());
    assert_eq!(
        session
            .material_result("bad-material", &request.0, Some(wrong), None)
            .await
            .err()
            .map(|error| error.code),
        Some(ErrorCode::ArtifactUnavailable)
    );
    assert_eq!(executor.calls.load(Ordering::SeqCst), 0);
    let mut complete = reference;
    complete.content = Some("required design constraints".into());
    let receipt = session
        .material_result("material", &request.0, Some(complete.clone()), None)
        .await?;
    assert_eq!(
        session
            .material_result("material", &request.0, Some(complete), None)
            .await?,
        receipt
    );
    let done = session.drive().await?;
    assert_eq!(
        done.run.as_ref().map(|run| run.status),
        Some(RunStatus::Completed)
    );
    assert_eq!(
        done.root_turn().ok_or("missing root")?.steps[0].signal_revision,
        1
    );
    let prompts = executor.prompts.lock().await;
    assert!(prompts[0].messages.iter().flat_map(|message|&message.content).any(|part|matches!(part,Content::Text{text,..} if text.contains("required design constraints") && text.contains("harness_document") && text.contains("v1"))));
    Ok(())
}

#[tokio::test]
async fn changed_material_version_rejects_old_content_before_model_dispatch() -> TestResult {
    let harness = Arc::new(Harness::new(None, None));
    let (session, executor, _) = setup(
        vec![output(vec![text("used new material")])],
        harness.clone(),
        false,
    )
    .await?;
    session
        .signals(
            "signals-v1",
            signal_update(&session, vec![material("v1", "old content", false)]).await,
        )
        .await?;
    session
        .start("input", session.head().await.state_revision, input())
        .await?;
    session.drive().await?;
    let old = harness.material_requests.lock().await[0].clone();
    session
        .signals(
            "signals-v2",
            signal_update(&session, vec![material("v2", "new content", false)]).await,
        )
        .await?;
    assert_eq!(
        session
            .material_result(
                "old-result",
                &old.0,
                Some(material("v1", "old content", true)),
                None
            )
            .await
            .err()
            .map(|error| error.code),
        Some(ErrorCode::StaleRevision)
    );
    session.drive().await?;
    assert_eq!(executor.calls.load(Ordering::SeqCst), 0);
    let current = harness.material_requests.lock().await[1].clone();
    session
        .material_result(
            "new-result",
            &current.0,
            Some(material("v2", "new content", true)),
            None,
        )
        .await?;
    let done = session.drive().await?;
    let step = &done.root_turn().ok_or("missing root")?.steps[0];
    assert_eq!(step.materials[0].version, "v2");
    assert_eq!(step.signal_revision, 2);
    let prompt = &executor.prompts.lock().await[0];
    assert!(!serde_json::to_string(prompt)?.contains("old content"));
    Ok(())
}

#[tokio::test]
async fn permission_update_preserves_frozen_calls_but_denies_unstarted_tools() -> TestResult {
    for revoke_tool in [true, false] {
        let harness = Arc::new(Harness::new(None, Some("model.attempt.outcome")));
        let (session, executor, _) = setup(
            vec![
                output(vec![call("old-permission")]),
                output(vec![text("permission change acknowledged")]),
            ],
            harness.clone(),
            false,
        )
        .await?;
        session.start("input", 1, input()).await?;
        let driving = tokio::spawn({
            let session = session.clone();
            async move { session.drive().await }
        });
        tokio::time::timeout(Duration::from_secs(5), harness.seen.acquire())
            .await??
            .forget();
        let mut update = signal_update(&session, Vec::new()).await;
        if revoke_tool {
            update.manifest.permission_revision += 1;
            update.manifest.tools.clear();
            update.manifest.tool_manifest_digest = HarnessManifest::digest(&update.manifest.tools)?;
        } else {
            update
                .materials
                .push(material("v1", "changed task constraints", true));
        }
        let updating = tokio::spawn({
            let session = session.clone();
            async move { session.signals("revoke", update).await }
        });
        tokio::task::yield_now().await;
        harness.hold_enabled.store(false, Ordering::SeqCst);
        harness.resume.add_permits(1);
        updating.await??;
        let done = driving.await??;
        assert_eq!(
            done.run.as_ref().map(|run| run.status),
            Some(RunStatus::Completed)
        );
        assert!(harness.sent.lock().await.is_empty());
        assert_eq!(executor.calls.load(Ordering::SeqCst), 2);
        let turn = done.root_turn().ok_or("missing root")?;
        assert_eq!(turn.steps[0].manifest.permission_revision, 1);
        assert_eq!(
            turn.steps[1].manifest.permission_revision,
            if revoke_tool { 2 } else { 1 }
        );
        assert_eq!(turn.invocations[0].signal_revision, 0);
        assert_eq!(turn.invocations.len(), 1);
        assert_eq!(
            turn.invocations[0]
                .result
                .as_ref()
                .map(|result| result.status),
            Some(ToolOutcome::Denied)
        );
        assert!(turn.invocations[0].consumed);
    }
    Ok(())
}

#[tokio::test]
async fn tool_result_and_wait_deadline_wake_root_while_a_child_is_running() -> TestResult {
    for wait in [false, true] {
        let harness = Arc::new(Harness::new(None, None));
        let table = StaticRoutingTable::new();
        table.insert("fixture-model", vec![target("first")]);
        let executor = Arc::new(WakeExecutor {
            child_seen: Semaphore::new(0),
            child_release: Semaphore::new(0),
            root_resumed: Semaphore::new(0),
            root_calls: AtomicUsize::new(0),
            child_id: Mutex::new(None),
            wait,
        });
        let app = App::builder()
            .language_model(|builder| {
                builder
                    .routing_table(Arc::new(table))
                    .executor(executor.clone());
            })
            .build()?;
        let session = bind_app(Arc::new(app), harness.clone()).await?;
        let mut task = input();
        task.limits = Some(Limits {
            active_models: 2,
            ..Limits::default()
        });
        let root = session.start("input", 1, task).await?.assigned_ids["agent_id"].clone();
        let child = session
            .collaborate(
                "spawn",
                session.head().await.state_revision,
                &root,
                Action::Spawn {
                    task: work("held-child"),
                },
            )
            .await?
            .assigned_ids["agent_id"]
            .clone();
        *executor.child_id.lock().await = Some(child);
        let driving = tokio::spawn({
            let session = session.clone();
            async move { session.drive().await }
        });
        tokio::time::timeout(Duration::from_secs(5), executor.child_seen.acquire())
            .await??
            .forget();
        if !wait {
            tokio::time::timeout(Duration::from_secs(5), harness.delivered.acquire())
                .await??
                .forget();
            let command = harness.sent.lock().await[0].clone();
            session.tool_result("root-result", result(&command)).await?;
        }
        tokio::time::timeout(Duration::from_secs(5), executor.root_resumed.acquire())
            .await??
            .forget();
        assert_eq!(executor.child_release.available_permits(), 0);
        if wait {
            let snapshot = session.snapshot().await;
            assert!(
                snapshot
                    .root_turn()
                    .is_some_and(|turn| turn.core_calls.iter().any(|call| call
                        .result
                        .as_ref()
                        .is_some_and(|result| result["value"]["timed_out"] == true)))
            );
        }
        executor.child_release.add_permits(1);
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(5), driving)
                .await???
                .run
                .map(|run| run.status),
            Some(RunStatus::Completed)
        );
    }
    Ok(())
}

#[tokio::test]
async fn current_run_followups_reuse_old_agent_context_in_fifo_order() -> TestResult {
    let harness = Arc::new(Harness::new(None, None));
    let (session, executor, _) = setup(
        (0..12).map(|_| output(vec![text("task done")])).collect(),
        harness,
        false,
    )
    .await?;
    let root = session.start("first", 1, input()).await?.assigned_ids["agent_id"].clone();
    let child = session
        .collaborate(
            "spawn",
            session.head().await.state_revision,
            &root,
            Action::Spawn {
                task: work("initial-child-task"),
            },
        )
        .await?
        .assigned_ids["agent_id"]
        .clone();
    assert_eq!(
        session.drive().await?.run.map(|run| run.status),
        Some(RunStatus::Completed)
    );
    let second = session
        .start("second", session.head().await.state_revision, input())
        .await?;
    let turn_before = session.snapshot().await.agents[&child]
        .turn
        .as_ref()
        .ok_or("missing child")?
        .agent_turn_id
        .clone();
    let revision = session.head().await.state_revision;
    let message = Action::Message {
        agent_id: child.clone(),
        text: "additional guidance".into(),
    };
    let receipt = session
        .collaborate("message", revision, &root, message.clone())
        .await?;
    assert_eq!(
        session
            .collaborate("message", revision, &root, message)
            .await?,
        receipt
    );
    assert_eq!(
        session.snapshot().await.agents[&child]
            .turn
            .as_ref()
            .map(|turn| &turn.agent_turn_id),
        Some(&turn_before)
    );
    for (index, text) in ["followup-one", "followup-two"].iter().enumerate() {
        let mut task = work(text);
        task.fresh_context = false;
        session
            .collaborate(
                &format!("followup_{index}"),
                session.head().await.state_revision,
                &root,
                Action::Followup {
                    agent_id: child.clone(),
                    task,
                },
            )
            .await?;
    }
    let assigned = session.snapshot().await.agents[&child]
        .queue
        .back()
        .ok_or("missing queued work")?
        .assignment_id
        .clone();
    let wait_revision = session.head().await.state_revision;
    let action = Action::Wait {
        agent_ids: vec![child.clone()],
        timeout_ms: 30_000,
    };
    let receipt = session
        .collaborate("wait-for-followup", wait_revision, &root, action.clone())
        .await?;
    assert_eq!(
        receipt.disposition,
        bitrouter_orchestrator::core::protocol::OperationDisposition::Accepted
    );
    assert_eq!(
        session
            .collaborate("wait-for-followup", wait_revision, &root, action)
            .await?,
        receipt
    );
    let snapshot = session.snapshot().await;
    let wait = &snapshot.waits["wait-for-followup"];
    assert!(wait.result.is_none());
    assert_eq!(wait.state.targets[&child].agent_turn_id, assigned);
    assert!(wait.state.targets[&child].status.is_none());
    let done = tokio::time::timeout(Duration::from_secs(5), session.drive()).await??;
    assert_eq!(
        done.run.as_ref().map(|run| run.status),
        Some(RunStatus::Completed)
    );
    let agent = &done.agents[&child];
    assert!(agent.queue.is_empty());
    assert_eq!(
        agent.turn.as_ref().map(|turn| &turn.run_id),
        second.assigned_ids.get("run_id")
    );
    assert_eq!(
        agent.turn.as_ref().map(|turn| turn.input.text.as_str()),
        Some("followup-two")
    );
    assert_eq!(agent.mailbox.len(), 1);
    assert!(agent.mailbox[0].consumed);
    let observed = done.waits["wait-for-followup"]
        .result
        .as_ref()
        .ok_or("wait did not complete")?;
    assert_eq!(observed["agents"][0]["agent_turn_id"], assigned);
    assert_ne!(observed["agents"][0]["agent_turn_id"], turn_before);
    let prompts = executor.prompts.lock().await;
    let child_prompts = prompts
        .iter()
        .filter(|prompt| {
            prompt
                .system
                .as_ref()
                .is_some_and(|system| system.contains(&child))
        })
        .collect::<Vec<_>>();
    assert_eq!(child_prompts.len(), 3);
    assert!(
        child_prompts[1]
            .messages
            .iter()
            .flat_map(|message| &message.content)
            .any(|part| matches!(part,Content::Text{text,..} if text=="followup-one"))
    );
    assert!(
        !child_prompts[1]
            .messages
            .iter()
            .flat_map(|message| &message.content)
            .any(|part| matches!(part,Content::Text{text,..} if text=="followup-two"))
    );
    Ok(())
}

#[tokio::test]
async fn assignment_cycles_include_idle_intermediate_ancestors() -> TestResult {
    let harness = Arc::new(Harness::new(None, Some("model.step.preparing")));
    harness.hold_enabled.store(false, Ordering::SeqCst);
    let (session, _, _) = setup(
        (0..20).map(|_| output(vec![text("done")])).collect(),
        harness.clone(),
        false,
    )
    .await?;
    let root = session.start("first", 1, input()).await?.assigned_ids["agent_id"].clone();
    let mut parent = root.clone();
    let mut children = Vec::new();
    for index in 0..3 {
        let receipt = session
            .collaborate(
                &format!("spawn_{index}"),
                session.head().await.state_revision,
                &parent,
                Action::Spawn {
                    task: work("child"),
                },
            )
            .await?;
        parent = receipt.assigned_ids["agent_id"].clone();
        children.push(parent.clone());
    }
    assert_eq!(
        session.drive().await?.run.map(|run| run.status),
        Some(RunStatus::Completed)
    );
    session
        .start("second", session.head().await.state_revision, input())
        .await?;
    let mut task = work("followup deep child");
    task.fresh_context = false;
    session
        .collaborate(
            "resume-deep-child",
            session.head().await.state_revision,
            &root,
            Action::Followup {
                agent_id: children[2].clone(),
                task: task.clone(),
            },
        )
        .await?;
    harness.hold_enabled.store(true, Ordering::SeqCst);
    let running = tokio::spawn({
        let session = session.clone();
        async move { session.drive().await }
    });
    tokio::time::timeout(Duration::from_secs(5), harness.seen.acquire())
        .await??
        .forget();
    // Capture the actual committed boundary after the queued deep turn starts,
    // before any new provider work. Its intermediate ancestors remain idle.
    let mut snapshot = session.snapshot().await;
    let run_id = &snapshot.run.as_ref().ok_or("missing run")?.run_id;
    assert_eq!(
        snapshot.agents[&children[2]]
            .turn
            .as_ref()
            .map(|turn| &turn.run_id),
        Some(run_id)
    );
    assert_ne!(
        snapshot.agents[&children[1]]
            .turn
            .as_ref()
            .map(|turn| &turn.run_id),
        Some(run_id)
    );
    let rejected = bitrouter_orchestrator::core::collaboration::apply(
        &mut snapshot,
        &children[2],
        &Action::Followup {
            agent_id: children[0].clone(),
            task,
        },
        0,
    );
    assert_eq!(
        rejected.err().map(|error| error.code),
        Some(ErrorCode::OperationConflict)
    );
    running.abort();
    assert!(running.await.is_err());
    Ok(())
}

#[tokio::test]
async fn implicit_assignment_cycles_reject_before_acceptance() -> TestResult {
    let harness = Arc::new(Harness::new(None, None));
    let (session, _, _) = setup(
        (0..16).map(|_| output(vec![text("done")])).collect(),
        harness,
        false,
    )
    .await?;
    let accepted = session.start("input", 1, input()).await?;
    let root = &accepted.assigned_ids["agent_id"];
    let mut children = Vec::new();
    for index in 0..2 {
        let receipt = session
            .collaborate(
                &format!("spawn_{index}"),
                session.head().await.state_revision,
                root,
                Action::Spawn {
                    task: work("child"),
                },
            )
            .await?;
        children.push(receipt.assigned_ids["agent_id"].clone());
    }
    let grandchild = session
        .collaborate(
            "grandchild",
            session.head().await.state_revision,
            &children[0],
            Action::Spawn {
                task: work("grandchild"),
            },
        )
        .await?
        .assigned_ids["agent_id"]
        .clone();
    let mut task = work("follow up");
    task.fresh_context = false;
    let before = session.head().await;
    let rejected = session
        .collaborate(
            "ancestor-cycle",
            before.state_revision,
            &grandchild,
            Action::Followup {
                agent_id: children[0].clone(),
                task: task.clone(),
            },
        )
        .await;
    assert_eq!(
        rejected.err().map(|error| error.code),
        Some(ErrorCode::OperationConflict)
    );
    assert_eq!(session.head().await, before);
    session
        .collaborate(
            "assign-sibling",
            before.state_revision,
            &children[0],
            Action::Followup {
                agent_id: children[1].clone(),
                task: task.clone(),
            },
        )
        .await?;
    let before = session.head().await;
    let rejected = session
        .collaborate(
            "sibling-cycle",
            before.state_revision,
            &children[1],
            Action::Followup {
                agent_id: children[0].clone(),
                task,
            },
        )
        .await;
    assert_eq!(
        rejected.err().map(|error| error.code),
        Some(ErrorCode::OperationConflict)
    );
    assert_eq!(session.head().await, before);
    let done = tokio::time::timeout(Duration::from_secs(5), session.drive()).await??;
    assert_eq!(
        done.run.as_ref().map(|run| run.status),
        Some(RunStatus::Completed)
    );
    Ok(())
}

#[tokio::test]
async fn cancellation_cleanup_waits_for_another_agents_outcome_ack() -> TestResult {
    let mut harness = Harness::new(None, Some("model.attempt.outcome"));
    harness.hold_enabled.store(false, Ordering::SeqCst);
    harness.hold_after_send = true;
    let harness = Arc::new(harness);
    let executor = Arc::new(WakeExecutor {
        child_seen: Semaphore::new(0),
        child_release: Semaphore::new(0),
        root_resumed: Semaphore::new(0),
        root_calls: AtomicUsize::new(0),
        child_id: Mutex::new(None),
        wait: false,
    });
    let table = StaticRoutingTable::new();
    table.insert("fixture-model", vec![target("first")]);
    let app = App::builder()
        .language_model(|builder| {
            builder
                .routing_table(Arc::new(table))
                .executor(executor.clone());
        })
        .build()?;
    let session = bind_app(Arc::new(app), harness.clone()).await?;
    let accepted = session.start("input", 1, input()).await?;
    session
        .collaborate(
            "spawn",
            session.head().await.state_revision,
            &accepted.assigned_ids["agent_id"],
            Action::Spawn {
                task: work("held-child"),
            },
        )
        .await?;
    let running = tokio::spawn({
        let session = session.clone();
        async move { session.drive().await }
    });
    tokio::time::timeout(Duration::from_secs(5), harness.delivered.acquire())
        .await??
        .forget();
    tokio::time::timeout(Duration::from_secs(5), executor.child_seen.acquire())
        .await??
        .forget();
    session
        .cancel_run(
            "cancel",
            session.head().await.state_revision,
            &accepted.assigned_ids["run_id"],
        )
        .await?;
    harness.hold_enabled.store(true, Ordering::SeqCst);
    executor.child_release.add_permits(1);
    tokio::time::timeout(Duration::from_secs(5), harness.seen.acquire())
        .await??
        .forget();
    // The earlier send waiter resumes first; the outcome ACK stays held.
    harness.resume.add_permits(1);
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(!running.is_finished());
    assert_eq!(harness.cancel_seen.available_permits(), 0);
    harness.resume.add_permits(1);
    tokio::time::timeout(Duration::from_secs(5), harness.cancel_seen.acquire())
        .await??
        .forget();
    let waiting = tokio::time::timeout(Duration::from_secs(5), running).await???;
    assert_eq!(
        waiting.run.as_ref().map(|run| run.status),
        Some(RunStatus::Cancelling)
    );
    let command = harness.sent.lock().await[0].clone();
    let mut report = result(&command);
    report.status = ToolOutcome::NotExecuted;
    session.tool_result("cancelled", report).await?;
    assert_eq!(
        session.drive().await?.run.map(|run| run.status),
        Some(RunStatus::Cancelled)
    );
    Ok(())
}

#[tokio::test]
async fn newly_accepted_child_work_invalidates_previous_final_verification() -> TestResult {
    let harness = Arc::new(Harness::new(None, None));
    let (session, executor, _) = setup(
        vec![
            output(vec![text("old final")]),
            output(vec![call("child-read")]),
            output(vec![text("child changed the evidence")]),
            output(vec![text("new final")]),
        ],
        harness.clone(),
        false,
    )
    .await?;
    let mut task = input();
    task.limits = Some(Limits {
        active_models: 1,
        ..Limits::default()
    });
    task.verification = Some(Verification {
        tool: "read".into(),
        arguments: json!({"path":"file.txt"}),
    });
    let accepted = session.start("input", 1, task).await?;
    session.drive().await?;
    let old_verification = harness.sent.lock().await[0].clone();
    assert!(old_verification.verification);
    session
        .collaborate(
            "spawn",
            session.head().await.state_revision,
            &accepted.assigned_ids["agent_id"],
            Action::Spawn {
                task: work("additional child task"),
            },
        )
        .await?;
    session.drive().await?;
    let child_read = harness.sent.lock().await[1].clone();
    assert!(!child_read.verification);
    session
        .tool_result("old-verification", result(&old_verification))
        .await?;
    session
        .tool_result("child-result", result(&child_read))
        .await?;
    let waiting = session.drive().await?;
    assert_eq!(
        waiting.run.as_ref().map(|run| run.status),
        Some(RunStatus::Waiting)
    );
    assert_eq!(executor.calls.load(Ordering::SeqCst), 4);
    let new_verification = harness.sent.lock().await[2].clone();
    assert!(new_verification.verification);
    assert_ne!(new_verification.step_id, old_verification.step_id);
    session
        .tool_result("new-verification", result(&new_verification))
        .await?;
    let done = session.drive().await?;
    assert_eq!(
        done.run.as_ref().map(|run| run.status),
        Some(RunStatus::Completed)
    );
    assert_eq!(
        done.run
            .as_ref()
            .and_then(|run| run.final_answer.as_deref()),
        Some("new final")
    );
    Ok(())
}

#[tokio::test]
async fn cancellation_waits_for_dispatched_tool_cleanup_and_preserves_pairing() -> TestResult {
    let harness = Arc::new(Harness::new(None, None));
    let (session, executor, _) = setup(
        vec![output(vec![call("read-before-cancel")])],
        harness.clone(),
        false,
    )
    .await?;
    let accepted = session.start("input", 1, input()).await?;
    session.drive().await?;
    let command = harness.sent.lock().await[0].clone();
    session
        .cancel_run(
            "cancel",
            session.head().await.state_revision,
            &accepted.assigned_ids["run_id"],
        )
        .await?;
    let waiting = session.drive().await?;
    assert_eq!(
        waiting.run.as_ref().map(|run| run.status),
        Some(RunStatus::Cancelling)
    );
    tokio::time::timeout(Duration::from_secs(5), harness.cancel_seen.acquire())
        .await??
        .forget();
    assert_eq!(
        *harness.cancelled.lock().await,
        vec![(
            command.invocation_id.clone(),
            command.attempt_id.clone(),
            command.execution_epoch
        )]
    );
    session.drive().await?;
    assert_eq!(harness.cancelled.lock().await.len(), 1);
    let mut report = result(&command);
    report.status = ToolOutcome::NotExecuted;
    report.output = "harness cancelled before actual execution".into();
    session.tool_result("cancelled-result", report).await?;
    let done = session.drive().await?;
    assert_eq!(
        done.run.as_ref().map(|run| run.status),
        Some(RunStatus::Cancelled)
    );
    assert_eq!(executor.calls.load(Ordering::SeqCst), 1);
    assert!(
        done.root_turn()
            .ok_or("missing turn")?
            .invocations
            .iter()
            .all(|call| call.consumed)
    );
    let root = &done.agents[&done.agent_id];
    assert!(root.history.iter().flat_map(|message| &message.content).any(|part| matches!(part, Content::ToolResult { call_id, .. } if call_id == "read-before-cancel")));
    let kinds = harness.committed_kinds().await?;
    let consumed = kinds
        .iter()
        .position(|kind| kind == "tool.results.consumed")
        .ok_or("missing consumption")?;
    let terminal = kinds
        .iter()
        .position(|kind| kind == "agent.interrupted")
        .ok_or("missing interruption")?;
    assert!(consumed < terminal);
    Ok(())
}

#[tokio::test]
async fn verification_waits_for_children_and_respects_shared_tool_capacity() -> TestResult {
    let harness = Arc::new(Harness::new(None, None));
    let table = StaticRoutingTable::new();
    table.insert("fixture-model", vec![target("first")]);
    let executor = Arc::new(CollaborationExecutor {
        root_calls: AtomicUsize::new(0),
    });
    let app = App::builder()
        .language_model(|builder| {
            builder.routing_table(Arc::new(table)).executor(executor);
        })
        .build()?;
    let session = bind_app(Arc::new(app), harness.clone()).await?;
    let mut task = input();
    task.limits = Some(Limits {
        active_models: 1,
        outstanding_tools: 2,
        ..Limits::default()
    });
    task.verification = Some(Verification {
        tool: "read".into(),
        arguments: json!({"path":"file.txt"}),
    });
    session.start("input", 1, task).await?;
    session.drive().await?;
    let commands = harness.sent.lock().await.clone();
    assert_eq!(commands.len(), 2);
    assert!(commands.iter().all(|command| !command.verification));
    session
        .tool_result("child-first", result(&commands[0]))
        .await?;
    session.drive().await?;
    assert_eq!(harness.sent.lock().await.len(), 2);
    session
        .tool_result("child-second", result(&commands[1]))
        .await?;
    let waiting = session.drive().await?;
    assert_eq!(
        waiting.run.as_ref().map(|run| run.status),
        Some(RunStatus::Waiting)
    );
    let verification = harness.sent.lock().await[2].clone();
    assert!(verification.verification);
    assert_eq!(verification.agent_id, waiting.agent_id);
    session
        .tool_result("verified", result(&verification))
        .await?;
    let done = session.drive().await?;
    assert_eq!(
        done.run.as_ref().map(|run| run.status),
        Some(RunStatus::Completed)
    );
    let store = harness.store.lock().await;
    for batch in &store.batches {
        let payload = batch.decode(&store.limits)?;
        let state: SessionSnapshot = serde_json::from_value(payload.checkpoint.state)?;
        let outstanding = state
            .agents
            .values()
            .filter_map(|agent| agent.turn.as_ref())
            .flat_map(|turn| &turn.invocations)
            .filter(|call| call.result.is_none())
            .count();
        assert!(outstanding <= 2);
    }
    Ok(())
}

#[tokio::test]
async fn run_cancel_archives_child_results_when_parent_mailbox_is_full() -> TestResult {
    let harness = Arc::new(Harness::new(None, None));
    let (session, executor, _) = setup(Vec::new(), harness, false).await?;
    let mut task = input();
    task.limits = Some(Limits {
        mailbox_messages: 1,
        ..Limits::default()
    });
    let accepted = session.start("input", 1, task).await?;
    let root = &accepted.assigned_ids["agent_id"];
    for (index, text) in ["child-a", "child-b"].iter().enumerate() {
        session
            .collaborate(
                &format!("spawn_{index}"),
                session.head().await.state_revision,
                root,
                Action::Spawn { task: work(text) },
            )
            .await?;
    }
    let revision = session.head().await.state_revision;
    let cancelled = session
        .cancel_run("cancel", revision, &accepted.assigned_ids["run_id"])
        .await?;
    assert_eq!(
        session
            .cancel_run("cancel", revision, &accepted.assigned_ids["run_id"])
            .await?,
        cancelled
    );
    let done = tokio::time::timeout(Duration::from_secs(5), session.drive()).await??;
    assert_eq!(
        done.run.as_ref().map(|run| run.status),
        Some(RunStatus::Cancelled)
    );
    assert_eq!(executor.calls.load(Ordering::SeqCst), 0);
    assert_eq!(done.agents[root].mailbox.len(), 2);
    assert!(done.agents[root].mailbox.iter().all(|mail| mail.consumed));
    Ok(())
}

#[tokio::test]
async fn model_collaboration_waits_release_slots_and_tools_keep_agent_attribution() -> TestResult {
    let harness = Arc::new(Harness::new(None, None));
    let table = StaticRoutingTable::new();
    table.insert("fixture-model", vec![target("first")]);
    let executor = Arc::new(CollaborationExecutor {
        root_calls: AtomicUsize::new(0),
    });
    let app = App::builder()
        .language_model(|builder| {
            builder
                .routing_table(Arc::new(table))
                .executor(executor.clone());
        })
        .build()?;
    let session = bind_app(Arc::new(app), harness.clone()).await?;
    let mut task = input();
    task.limits = Some(Limits {
        active_models: 1,
        ..Limits::default()
    });
    session.start("input", 1, task).await?;
    let waiting = tokio::time::timeout(Duration::from_secs(5), session.drive()).await??;
    assert_ne!(
        waiting.run.as_ref().map(|run| run.status),
        Some(RunStatus::Completed)
    );
    assert_eq!(waiting.agents.len(), 3);
    let commands = harness.sent.lock().await.clone();
    assert_eq!(commands.len(), 2);
    assert_ne!(commands[0].agent_id, commands[1].agent_id);
    assert_ne!(commands[0].invocation_id, commands[1].invocation_id);
    assert!(commands.iter().all(|command| command.tool == "read"));
    for (index, command) in commands.iter().enumerate() {
        session
            .tool_result(&format!("result_{index}"), result(command))
            .await?;
    }
    let done = tokio::time::timeout(Duration::from_secs(5), session.drive()).await??;
    assert_eq!(
        done.run.as_ref().map(|run| run.status),
        Some(RunStatus::Completed)
    );
    for agent in done.agents.values() {
        let mut pairs = std::collections::BTreeMap::<String, i32>::new();
        for part in agent.history.iter().flat_map(|message| &message.content) {
            match part {
                Content::ToolCall { id, .. } => *pairs.entry(id.clone()).or_default() += 1,
                Content::ToolResult { call_id, .. } => {
                    *pairs.entry(call_id.clone()).or_default() -= 1
                }
                _ => {}
            }
        }
        assert!(pairs.values().all(|count| *count == 0));
    }

    let root = done.root_turn().ok_or("missing root")?;
    assert_eq!(root.core_calls.len(), 3);
    assert!(root.core_calls.iter().all(|call| {
        call.consumed
            && call
                .result
                .as_ref()
                .is_some_and(|result| result["ok"] == true)
    }));
    let store = harness.store.lock().await;
    let attributed_results = store
        .batches
        .iter()
        .filter_map(|batch| batch.decode(&store.limits).ok())
        .flat_map(|payload| payload.events)
        .filter(|event| event.kind == "tool.result")
        .filter_map(|event| event.agent_id)
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        attributed_results,
        commands
            .iter()
            .map(|command| command.agent_id.clone())
            .collect()
    );
    Ok(())
}

#[tokio::test]
async fn interruption_keeps_billed_child_output_but_discards_new_effects() -> TestResult {
    let harness = Arc::new(Harness::new(None, None));
    let table = StaticRoutingTable::new();
    table.insert("fixture-model", vec![target("first")]);
    let executor = Arc::new(ConcurrentExecutor {
        seen: Semaphore::new(0),
        release: Semaphore::new(0),
        active: AtomicUsize::new(0),
        peak: AtomicUsize::new(0),
        root_calls: AtomicUsize::new(0),
    });
    let app = App::builder()
        .language_model(|builder| {
            builder
                .routing_table(Arc::new(table))
                .executor(executor.clone());
        })
        .build()?;
    let session = bind_app(Arc::new(app), harness.clone()).await?;
    let root = session
        .start("input", 1, input())
        .await?
        .assigned_ids
        .get("agent_id")
        .cloned()
        .ok_or("root missing")?;
    let child = session
        .collaborate(
            "spawn",
            session.head().await.state_revision,
            &root,
            Action::Spawn {
                task: work("child-a"),
            },
        )
        .await?
        .assigned_ids
        .get("agent_id")
        .cloned()
        .ok_or("child missing")?;
    let running = tokio::spawn({
        let session = session.clone();
        async move { session.drive().await }
    });
    tokio::time::timeout(Duration::from_secs(5), executor.seen.acquire())
        .await??
        .forget();
    session
        .collaborate(
            "interrupt",
            session.head().await.state_revision,
            &root,
            Action::Interrupt {
                agent_id: child.clone(),
            },
        )
        .await?;
    assert_eq!(
        session
            .snapshot()
            .await
            .agents
            .get(&child)
            .and_then(|agent| agent.turn.as_ref())
            .map(|turn| turn.status),
        Some(bitrouter_orchestrator::core::session::AgentStatus::Cancelling)
    );
    executor.release.add_permits(1);
    let done = tokio::time::timeout(Duration::from_secs(5), running).await???;
    assert_eq!(
        done.run.as_ref().map(|run| run.status),
        Some(RunStatus::Completed)
    );
    let child = done
        .agents
        .get(&child)
        .and_then(|agent| agent.turn.as_ref())
        .ok_or("child disappeared")?;
    assert_eq!(
        child.status,
        bitrouter_orchestrator::core::session::AgentStatus::Interrupted
    );
    assert!(child.final_answer.is_none());
    assert!(
        child.steps[0].attempts[0]
            .report
            .as_ref()
            .is_some_and(|report| report.result.is_some())
    );
    assert!(harness.sent.lock().await.is_empty());
    Ok(())
}

#[tokio::test]
async fn graph_capacity_and_ancestor_wait_reject_without_partial_acceptance() -> TestResult {
    let harness = Arc::new(Harness::new(None, None));
    let (session, executor, _) = setup(Vec::new(), harness, false).await?;
    let mut task = input();
    task.limits = Some(Limits {
        agents: 2,
        ..Limits::default()
    });
    let root = session
        .start("input", 1, task)
        .await?
        .assigned_ids
        .get("agent_id")
        .cloned()
        .ok_or("root missing")?;
    let revision = session.head().await.state_revision;
    let action = Action::Spawn {
        task: work("child-a"),
    };
    let accepted = session
        .collaborate("spawn", revision, &root, action.clone())
        .await?;
    assert_eq!(
        session
            .collaborate("spawn", revision, &root, action)
            .await?,
        accepted
    );
    let child = accepted
        .assigned_ids
        .get("agent_id")
        .ok_or("child missing")?;
    let before = session.head().await;
    assert_eq!(
        session
            .collaborate(
                "overflow",
                before.state_revision,
                &root,
                Action::Spawn {
                    task: work("child-b")
                }
            )
            .await
            .err()
            .map(|error| error.code),
        Some(ErrorCode::LimitExceeded)
    );
    assert_eq!(
        session
            .collaborate(
                "cycle",
                before.state_revision,
                child,
                Action::Wait {
                    agent_ids: vec![root.clone()],
                    timeout_ms: 1000
                }
            )
            .await
            .err()
            .map(|error| error.code),
        Some(ErrorCode::OperationConflict)
    );
    assert_eq!(session.head().await, before);
    assert_eq!(executor.calls.load(Ordering::SeqCst), 0);
    Ok(())
}

#[tokio::test]
async fn children_overlap_with_bounded_slots_and_root_joins_their_results() -> TestResult {
    let harness = Arc::new(Harness::new(None, None));
    let table = StaticRoutingTable::new();
    table.insert("fixture-model", vec![target("first")]);
    let executor = Arc::new(ConcurrentExecutor {
        seen: Semaphore::new(0),
        release: Semaphore::new(0),
        active: AtomicUsize::new(0),
        peak: AtomicUsize::new(0),
        root_calls: AtomicUsize::new(0),
    });
    let app = App::builder()
        .language_model(|builder| {
            builder
                .routing_table(Arc::new(table))
                .executor(executor.clone());
        })
        .build()?;
    let session = bind_app(Arc::new(app), harness.clone()).await?;
    let mut task = input();
    task.limits = Some(Limits {
        active_models: 2,
        ..Limits::default()
    });
    let accepted = session.start("input", 1, task).await?;
    let root = accepted
        .assigned_ids
        .get("agent_id")
        .ok_or("root missing")?;
    for (index, text) in ["child-a", "child-b"].iter().enumerate() {
        session
            .collaborate(
                &format!("spawn_{index}"),
                session.head().await.state_revision,
                root,
                Action::Spawn { task: work(text) },
            )
            .await?;
    }
    let running = tokio::spawn({
        let session = session.clone();
        async move { session.drive().await }
    });
    tokio::time::timeout(Duration::from_secs(5), executor.seen.acquire_many(2))
        .await??
        .forget();
    let waiting = session.snapshot().await;
    assert_ne!(
        waiting.run.as_ref().map(|run| run.status),
        Some(RunStatus::Completed)
    );
    assert_eq!(executor.active.load(Ordering::SeqCst), 2);
    assert_eq!(executor.peak.load(Ordering::SeqCst), 2);
    assert_eq!(executor.root_calls.load(Ordering::SeqCst), 1);
    // Both providers are held together, establishing a known overlapping
    // interval. The run counts its union; individual receipts keep each cost.
    tokio::time::sleep(Duration::from_millis(150)).await;
    executor.release.add_permits(2);
    let done = tokio::time::timeout(Duration::from_secs(5), running).await???;
    assert_eq!(
        done.run.as_ref().map(|run| run.status),
        Some(RunStatus::Completed)
    );
    assert_eq!(
        done.run
            .as_ref()
            .and_then(|run| run.final_answer.as_deref()),
        Some("joined child evidence")
    );
    assert_eq!(done.agents.len(), 3);
    let attempt_ms = done
        .agents
        .values()
        .filter_map(|agent| agent.turn.as_ref())
        .flat_map(|turn| &turn.steps)
        .flat_map(|step| &step.attempts)
        .filter_map(|attempt| attempt.report.as_ref())
        .map(|report| report.elapsed_ms)
        .sum::<u64>();
    let active_ms = done.run.as_ref().ok_or("missing run")?.active_ms;
    assert!(
        active_ms >= 100,
        "provider work must count toward active time"
    );
    assert!(
        attempt_ms >= active_ms + 100,
        "overlap must not be charged twice: attempts={attempt_ms}, run={active_ms}"
    );
    assert!(
        done.agents
            .values()
            .all(|agent| agent
                .turn
                .as_ref()
                .is_some_and(|turn| turn.status
                    == bitrouter_orchestrator::core::session::AgentStatus::Completed))
    );
    assert!(harness.sent.lock().await.is_empty());
    let store = harness.store.lock().await;
    for batch in &store.batches {
        let payload = batch.decode(&store.limits)?;
        for event in payload
            .events
            .iter()
            .filter(|event| event.kind == "model.attempt.intent")
        {
            let agent_id = event
                .agent_id
                .as_deref()
                .ok_or("attempt missing agent attribution")?;
            assert!(done.agents.contains_key(agent_id));
        }
    }
    Ok(())
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
async fn a_new_run_reuses_agent_context_but_gets_fresh_execution_identity() -> TestResult {
    let harness = Arc::new(Harness::new(None, None));
    let (session, executor, settlements) = setup(
        vec![
            output(vec![text("first task answer")]),
            output(vec![call("call_1")]),
            output(vec![text("second task answer")]),
        ],
        harness.clone(),
        false,
    )
    .await?;
    let first = session.start("input_1", 1, input()).await?;
    session.drive().await?;
    let mut second_input = input();
    second_input.text = "Continue using the preceding answer".into();
    let second = session
        .start("input_2", session.head().await.state_revision, second_input)
        .await?;
    assert_eq!(
        first.assigned_ids.get("agent_id"),
        second.assigned_ids.get("agent_id")
    );
    assert_ne!(
        first.assigned_ids.get("run_id"),
        second.assigned_ids.get("run_id")
    );
    assert_ne!(
        first.assigned_ids.get("agent_turn_id"),
        second.assigned_ids.get("agent_turn_id")
    );
    let accepted = session.snapshot().await;
    assert!(
        accepted
            .root_turn()
            .is_some_and(|turn| turn.steps.is_empty() && turn.invocations.is_empty())
    );
    assert_eq!(accepted.run.as_ref().map(|run| run.model_attempts), Some(0));
    session.drive().await?;
    let command = harness.sent.lock().await[0].clone();
    assert_eq!(second.assigned_ids.get("run_id"), Some(&command.run_id));
    assert_eq!(
        second.assigned_ids.get("agent_turn_id"),
        Some(&command.agent_turn_id)
    );
    session.tool_result("result_2", result(&command)).await?;
    let done = session.drive().await?;
    assert_eq!(
        done.run.as_ref().map(|run| run.status),
        Some(RunStatus::Completed)
    );
    assert_eq!(done.run.as_ref().map(|run| run.model_attempts), Some(2));
    assert_eq!(settlements.load(Ordering::SeqCst), 3);
    assert!(executor.prompts.lock().await[1].messages.iter().flat_map(|message| &message.content).any(|content| matches!(content, Content::Text { text, .. } if text == "first task answer")));
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
            .root_turn()
            .and_then(|turn| turn.steps.last())
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
    let run = state.run.as_ref().ok_or("missing run")?;
    let turn = state.root_turn().ok_or("missing root turn")?;
    assert_eq!(run.status, RunStatus::Completed);
    assert_eq!(run.model_attempts, 2);
    assert_eq!(turn.steps.len(), 1);
    assert_ne!(
        turn.steps[0].attempts[0].attempt_id,
        turn.steps[0].attempts[1].attempt_id
    );
    assert_eq!(
        turn.steps[0].attempts[1]
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
async fn abandoned_preparation_returns_without_repeated_admission() -> TestResult {
    let harness = Arc::new(Harness::new(None, Some("model.step.preparing")));
    let (session, executor, _) =
        setup(vec![output(vec![text("unused")])], harness.clone(), false).await?;
    session.start("input", 1, input()).await?;
    let running = tokio::spawn({
        let session = session.clone();
        async move { session.drive().await }
    });
    tokio::time::timeout(Duration::from_secs(5), harness.seen.acquire())
        .await??
        .forget();
    running.abort();
    assert!(running.await.is_err());
    let resumed = tokio::time::timeout(Duration::from_secs(5), session.drive()).await?;
    assert!(resumed.is_err());
    assert_eq!(executor.calls.load(Ordering::SeqCst), 0);
    assert_eq!(harness.seen.available_permits(), 0);
    assert!(
        session
            .snapshot()
            .await
            .root_turn()
            .ok_or("missing root")?
            .steps
            .is_empty()
    );
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
            .root_turn()
            .map(|turn| turn.steps.len()),
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
