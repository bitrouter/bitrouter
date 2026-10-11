//! Shared fixtures for service behavior and fault tests.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use bitrouter_ai::types::AuthScheme;
use bitrouter_ai::types::{
    ApiProtocol, Content, FinishReason, GenerateResult, Prompt, StreamPart, Usage,
};
use bitrouter_sdk::App;
use bitrouter_sdk::caller::CallerContext;
use bitrouter_sdk::language_model::{
    ExecutionResult, Executor, MockExecutor, MockResponse, PipelineContext, RoutingTarget,
    StaticRoutingTable, StreamPartStream,
};
use tempfile::TempDir;

use crate::agent::{AgentConfig, ToolMode};
use crate::service::{ErrorCode, ServiceError, ThreadService, unknown_turn};
use crate::store::{ExecutionRecord, ExecutionStore};
use crate::thread::{PermissionProfile, ThreadRequest, ThreadSnapshot, ThreadTarget};
use crate::turn::{TurnEventPayload, TurnRequest, TurnSnapshot, TurnStatus};

fn routing_target() -> RoutingTarget {
    RoutingTarget {
        provider_name: "fixture".into(),
        service_id: "fixture-model".into(),
        api_base: "https://example.invalid".into(),
        api_key: "fixture-key".into(),
        api_protocol: ApiProtocol::ChatCompletions,
        chat_token_limit_field: None,
        chat_supports_store: None,
        chat_supports_stream_options: None,
        chat_google_extensions: false,
        reasoning_effort: None,
        account_label: None,
        api_key_override: None,
        api_base_override: None,
        auth_scheme: AuthScheme::Bearer,
        headers: Vec::new(),
    }
}

pub(super) fn app(turns: Vec<GenerateResult>) -> std::io::Result<Arc<App>> {
    app_with_executor(Arc::new(MockExecutor::new(
        turns.into_iter().map(mock_stream).collect(),
    )))
}

pub(super) fn app_with_executor(
    executor: Arc<dyn bitrouter_sdk::language_model::Executor>,
) -> std::io::Result<Arc<App>> {
    let table = StaticRoutingTable::new();
    table.insert("fixture-model", vec![routing_target()]);
    App::builder()
        .language_model(|builder| {
            builder.routing_table(Arc::new(table)).executor(executor);
        })
        .build()
        .map(Arc::new)
        .map_err(std::io::Error::other)
}

pub(super) fn turn(parts: Vec<Content>) -> GenerateResult {
    GenerateResult {
        finish_reason: Some(
            if parts
                .iter()
                .any(|part| matches!(part, Content::ToolCall { .. }))
            {
                FinishReason::ToolCalls
            } else {
                FinishReason::Stop
            },
        ),
        content: parts,
        usage: Some(Usage {
            prompt_tokens: 10,
            completion_tokens: 5,
            ..Default::default()
        }),
        response_id: None,
        stop_details: None,
        provider_metadata: Default::default(),
    }
}

pub(super) fn mock_stream(turn: GenerateResult) -> MockResponse {
    let mut parts = Vec::new();
    for content in turn.content {
        match content {
            Content::Text { text, .. } => parts.push(StreamPart::TextDelta { text }),
            Content::ToolCall {
                id,
                name,
                arguments,
                provider_metadata,
                ..
            } => {
                parts.push(StreamPart::ToolCallDelta {
                    id,
                    name: Some(name),
                    arguments,
                    provider_metadata,
                });
            }
            _ => {}
        }
    }
    if let Some(usage) = turn.usage {
        parts.push(StreamPart::Usage { usage });
    }
    if let Some(reason) = turn.finish_reason {
        parts.push(StreamPart::Finish { reason });
    }
    MockResponse::Stream(parts)
}

pub(super) fn tool_call(id: &str, name: &str, arguments: serde_json::Value) -> Content {
    Content::ToolCall {
        id: id.into(),
        name: name.into(),
        arguments: arguments.to_string(),
        provider_executed: false,
        dynamic: false,
        provider_metadata: Default::default(),
    }
}

pub(super) fn final_turn() -> GenerateResult {
    turn(vec![Content::Text {
        text: "done".into(),
        provider_metadata: Default::default(),
    }])
}

pub(super) fn request(workspace: &TempDir) -> TurnFixture {
    TurnFixture {
        prompt: "change the file".into(),
        workspace: workspace.path().to_path_buf(),
        caller: CallerContext::local(),
        config: AgentConfig::fixed("fixture-model", None),
        verification_command: None,
        idempotency_key: None,
    }
}

pub(super) async fn wait_for(
    service: &ThreadService,
    turn_id: &str,
    status: TurnStatus,
) -> Result<TurnSnapshot, String> {
    tokio::time::timeout(
        std::time::Duration::from_secs(if cfg!(windows) { 10 } else { 3 }),
        async {
            loop {
                let snapshot = service.read(turn_id).map_err(|error| error.to_string())?;
                if snapshot.status == status || snapshot.status.terminal() {
                    return Ok(snapshot);
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        },
    )
    .await
    .map_err(|error| error.to_string())?
}

pub(super) fn thread_request(workspace: &TempDir, key: &str) -> ThreadRequest {
    ThreadRequest {
        caller: CallerContext::local(),
        workspace: workspace.path().to_path_buf(),
        config: AgentConfig::fixed("fixture-model", None),
        permission_profile: PermissionProfile::Ask,
        verification_command: None,
        idempotency_key: key.into(),
    }
}
pub(super) fn target(snapshot: &ThreadSnapshot) -> ThreadTarget {
    ThreadTarget {
        thread_id: snapshot.thread_id.clone(),
        server_instance_id: snapshot.server_instance_id.clone(),
    }
}
pub(super) fn input(prompt: &str, key: &str) -> TurnRequest {
    TurnRequest {
        prompt: prompt.into(),
        idempotency_key: key.into(),
    }
}
pub(super) async fn prompts(
    store: &dyn ExecutionStore,
    thread_id: &str,
    turn_id: &str,
) -> Result<Vec<Prompt>, String> {
    let saved = store.load(thread_id).await?.ok_or("Thread missing")?;
    Ok(saved
        .records
        .into_iter()
        .filter_map(|record| match record {
            ExecutionRecord::TurnRecord { turn_id: id, fact } if id == turn_id => match *fact {
                ExecutionRecord::ModelRequest { prompt, .. } => Some(*prompt),
                _ => None,
            },
            _ => None,
        })
        .collect())
}

pub(super) struct TurnFixture {
    pub prompt: String,
    pub workspace: PathBuf,
    pub caller: CallerContext,
    pub config: AgentConfig,
    pub verification_command: Option<String>,
    pub idempotency_key: Option<String>,
}

pub(super) fn turn_fact(record: &ExecutionRecord) -> &ExecutionRecord {
    match record {
        ExecutionRecord::TurnRecord { fact, .. } => turn_fact(fact),
        _ => record,
    }
}

impl ThreadService {
    pub(super) async fn submit_fixture(
        &self,
        request: TurnFixture,
    ) -> Result<TurnSnapshot, ServiceError> {
        use crate::thread::{ThreadRequest, ThreadTarget};
        use crate::turn::TurnRequest;
        let caller = request.caller.clone();
        let key = request
            .idempotency_key
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let permission_profile = if request.config.tool_mode() == ToolMode::ReadOnly {
            PermissionProfile::ReadOnly
        } else {
            PermissionProfile::Ask
        };
        let thread = self
            .create_thread(
                &self.inner.instance_id,
                ThreadRequest {
                    caller: request.caller,
                    workspace: request.workspace,
                    config: request.config,
                    permission_profile,
                    verification_command: request.verification_command,
                    idempotency_key: key.clone(),
                },
            )
            .await?;
        let target = ThreadTarget {
            thread_id: thread.thread_id,
            server_instance_id: thread.server_instance_id,
        };
        let receipt = self
            .start_turn(
                &target,
                &caller,
                TurnRequest {
                    prompt: request.prompt,
                    idempotency_key: key,
                },
            )
            .await?;
        self.read_stored_turn(&target, &caller, &receipt.turn_id)
            .await
    }

    pub(super) fn read(&self, turn_id: &str) -> Result<TurnSnapshot, ServiceError> {
        let mut state = self.lock_state();
        self.prune(&mut state);
        state
            .turns
            .get(turn_id)
            .map(|record| record.snapshot.clone())
            .ok_or_else(unknown_turn)
    }
}

impl ThreadService {
    pub(super) async fn cancel(&self, turn_id: &str) -> Result<(), ServiceError> {
        let gate = self.commit_gate(turn_id)?;
        let _guard = gate.lock().await;
        let token = {
            let state = self.lock_state();
            let record = state.turns.get(turn_id).ok_or_else(unknown_turn)?;
            if record.snapshot.status.terminal() || record.cancel.is_cancelled() {
                return Ok(());
            }
            if record.snapshot.status == TurnStatus::Queued {
                return Err(ServiceError::new(
                    ErrorCode::Conflict,
                    "use targeted queued withdrawal for an unstarted Turn",
                ));
            }
            record.cancel.clone()
        };
        self.append_serialized(turn_id, TurnEventPayload::CancelRequested)
            .await?;
        token.cancel();
        let pending = self
            .lock_state()
            .turns
            .get(turn_id)
            .and_then(|record| record.snapshot.pending_input_id.clone());
        if let Some(request_id) = pending {
            self.append_serialized(
                turn_id,
                TurnEventPayload::InputResolved {
                    request_id,
                    approved: false,
                },
            )
            .await?;
            if let Some(pending) = self
                .lock_state()
                .turns
                .get_mut(turn_id)
                .and_then(|record| record.pending.take())
            {
                let _ = pending.response.send(false);
            }
        }
        Ok(())
    }
}

impl ThreadService {
    pub(super) async fn answer_input(
        &self,
        turn_id: &str,
        request_id: &str,
        approved: bool,
    ) -> Result<(), ServiceError> {
        let gate = self.commit_gate(turn_id)?;
        let _guard = gate.lock().await;
        self.answer_input_serialized(turn_id, request_id, approved, &[])
            .await
    }
}

pub(super) struct HeldModel {
    inner: MockExecutor,
    pub(super) calls: AtomicUsize,
    entered: tokio::sync::Notify,
    pub(super) release: tokio::sync::Semaphore,
}

impl HeldModel {
    pub(super) fn new() -> Self {
        Self {
            inner: MockExecutor::new(vec![
                mock_stream(turn(vec![tool_call(
                    "stale",
                    "write",
                    serde_json::json!({"path":"stale.txt", "content":"must not execute"}),
                )])),
                mock_stream(final_turn()),
            ]),
            calls: AtomicUsize::new(0),
            entered: tokio::sync::Notify::new(),
            release: tokio::sync::Semaphore::new(0),
        }
    }
    pub(super) async fn wait(&self) -> Result<(), String> {
        tokio::time::timeout(Duration::from_secs(3), self.entered.notified())
            .await
            .map_err(|error| error.to_string())
    }
}

#[async_trait::async_trait]
impl Executor for HeldModel {
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
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            self.entered.notify_one();
            if let Ok(permit) = self.release.acquire().await {
                permit.forget();
            }
        }
        self.inner.execute_stream(target, prompt, ctx).await
    }
}
