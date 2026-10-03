//! Managed Responses profile; hosted actions remain BitRouter extensions.
//! Reference: <https://developers.openai.com/api/docs/guides/responses-multi-agent>

use std::collections::VecDeque;
use std::convert::Infallible;

use axum::body::{Body, to_bytes};
use axum::extract::{Request, State};
use axum::middleware::Next;
use axum::response::sse::{Event, KeepAlive};
use axum::response::{IntoResponse, Response, Sse};
use bitrouter_orchestrator::core::protocol::{
    ArtifactRef, BETA, CoreError, DiscardableHistory, ErrorCode, Limits, RoutingSettings,
    TaskInput, ToolOutcome, ToolResult, VERSION, Verification,
};
use bitrouter_orchestrator::core::session::responses::{ResponseExchange, ResponseToolResult};
use bitrouter_orchestrator::core::session::{CoreSession, RunStatus};
use bitrouter_sdk::language_model::types::Content;
use http::{Method, header};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::{OwnedSemaphorePermit, watch};

use super::{ApiError, ManagedCoreApi, auth::Principal};

#[derive(Clone, Default)]
struct Progress {
    response_id: Option<String>,
    outcome: Option<Result<(), CoreError>>,
}

pub(super) struct Job {
    operation_id: String,
    fingerprint: String,
    progress: watch::Receiver<Progress>,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct MultiAgent {
    enabled: bool,
    max_concurrent_subagents: Option<u32>,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Reasoning {
    effort: String,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Extension {
    version: u32,
    execution: String,
    session_id: String,
    execution_epoch: u64,
    operation_id: String,
    expected_state_revision: Option<u64>,
    routing: Option<RoutingSettings>,
    limits: Option<Limits>,
    #[serde(default)]
    acceptance_criteria: Vec<String>,
    #[serde(default)]
    required_materials: Vec<String>,
    verification: Option<Verification>,
    discardable_history: Option<DiscardableHistory>,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Create {
    model: String,
    input: Value,
    previous_response_id: Option<String>,
    #[serde(default)]
    stream: bool,
    multi_agent: MultiAgent,
    bitrouter: Extension,
    reasoning: Option<Reasoning>,
    max_output_tokens: Option<u32>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ResultMetadata {
    operation_id: String,
    status: ToolOutcome,
    #[serde(default)]
    evidence: Vec<ArtifactRef>,
    workspace_revision: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FunctionOutput {
    #[serde(rename = "type")]
    kind: String,
    call_id: String,
    output: String,
    bitrouter: ResultMetadata,
}

pub(super) async fn intercept(
    State(api): State<ManagedCoreApi>,
    request: Request,
    next: Next,
) -> Response {
    if request.method() != Method::POST || request.uri().path() != "/v1/responses" {
        return next.run(request).await;
    }
    let (parts, body) = request.into_parts();
    // Match the existing inference body bound, then restore its original bytes
    // for unmanaged requests so provider and SDK validation remain authoritative.
    let bytes = match to_bytes(body, 16 * 1024 * 1024).await {
        Ok(bytes) => bytes,
        Err(_) => {
            return ApiError::core(
                ErrorCode::LimitExceeded,
                "request body exceeds inference limit",
            )
            .into_response();
        }
    };
    let value = serde_json::from_slice::<Value>(&bytes);
    let managed = parts.headers.contains_key("bitrouter-beta")
        || value.as_ref().is_ok_and(|body| {
            body.get("bitrouter").is_some() || body["multi_agent"]["enabled"] == true
        });
    if !managed {
        return next
            .run(Request::from_parts(parts, Body::from(bytes)))
            .await;
    }
    let result = async {
        if parts
            .headers
            .get("bitrouter-beta")
            .and_then(|value| value.to_str().ok())
            != Some(BETA)
        {
            return Err(ApiError::core(
                ErrorCode::UnsupportedVersion,
                "managed responses require BitRouter-Beta: orchestrator_core=v1",
            ));
        }
        let permit = api
            .shared
            .requests
            .clone()
            .try_acquire_owned()
            .map_err(|_| {
                ApiError::core(
                    ErrorCode::Busy,
                    "managed response consumer capacity reached",
                )
            })?;
        let principal = Principal::authenticate(&api.shared.db, &parts.headers).await?;
        let create: Create = serde_json::from_slice(&bytes).map_err(|_| {
            ApiError::core(
                ErrorCode::UnsupportedCapability,
                "invalid or unsupported managed request field",
            )
        })?;
        if !create.multi_agent.enabled
            || create.bitrouter.version != VERSION
            || create.bitrouter.execution != "managed"
        {
            return Err(ApiError::core(
                ErrorCode::UnsupportedVersion,
                "managed execution and protocol version 1 are required",
            ));
        }
        let (session, limits) = api
            .session(
                &principal,
                &create.bitrouter.session_id,
                create.bitrouter.execution_epoch,
            )
            .await?;
        if bytes.len() as u64 > limits.input_bytes {
            return Err(ApiError::core(
                ErrorCode::LimitExceeded,
                "managed request exceeds negotiated input bound",
            ));
        }
        let stream = create.stream;
        let progress = start(&api, principal, session.clone(), create).await?;
        if stream {
            Ok(stream_response(session, progress, permit))
        } else {
            let exchange = wait(session, progress).await?;
            Ok(axum::Json(project(&exchange)?).into_response())
        }
    }
    .await;
    result.unwrap_or_else(IntoResponse::into_response)
}

async fn start(
    api: &ManagedCoreApi,
    principal: Principal,
    session: CoreSession,
    mut create: Create,
) -> Result<watch::Receiver<Progress>, ApiError> {
    create.stream = false;
    let fingerprint = bitrouter_orchestrator::core::checkpoint::sha256(
        &serde_json::to_vec(&create).map_err(|_| {
            ApiError::core(
                ErrorCode::UnsupportedCapability,
                "cannot encode managed request",
            )
        })?,
    );
    let mut sessions = api.shared.sessions.lock().await;
    let entry = sessions
        .get_mut(&ManagedCoreApi::key(
            &principal,
            &create.bitrouter.session_id,
        ))
        .filter(|entry| entry.connected)
        .ok_or_else(ApiError::unauthorized)?;
    if let Some(job) = &entry.job {
        if job.operation_id == create.bitrouter.operation_id
            && job
                .progress
                .borrow()
                .outcome
                .as_ref()
                .is_none_or(Result::is_ok)
        {
            if job.fingerprint != fingerprint {
                return Err(ApiError::core(
                    ErrorCode::OperationConflict,
                    "active response operation has different content",
                ));
            }
            return Ok(job.progress.clone());
        }
        if job.progress.borrow().outcome.is_none() {
            return Err(ApiError::core(
                ErrorCode::Busy,
                "a managed response operation is active",
            ));
        }
    }
    let (sender, progress) = watch::channel(Progress::default());
    entry.job = Some(Job {
        operation_id: create.bitrouter.operation_id.clone(),
        fingerprint,
        progress: progress.clone(),
    });
    let shutdown = api.shared.shutdown.clone();
    // Session ownership, not the lifetime of an HTTP body, owns acceptance and
    // driving. Repeated consumers attach to this bounded per-session job.
    tokio::spawn(async move {
        let result = async {
            principal.revalidate().await?;
            let response_id = accept(&session, &create).await?;
            sender.send_modify(|progress| progress.response_id = Some(response_id.clone()));
            loop {
                principal.revalidate().await?;
                match session.drive_response(&response_id).await {
                    Ok(_) => return Ok(()),
                    Err(error) if error.code == ErrorCode::Busy => {
                        tokio::select! {
                            _ = shutdown.cancelled() => return Err(CoreError::rejected(ErrorCode::CheckpointUnavailable, "core host stopped")),
                            _ = tokio::time::sleep(std::time::Duration::from_millis(20)) => {}
                        }
                    }
                    Err(error) => return Err(error),
                }
            }
        }.await;
        sender.send_modify(|progress| progress.outcome = Some(result));
    });
    Ok(progress)
}

async fn accept(session: &CoreSession, create: &Create) -> Result<String, CoreError> {
    let extension = &create.bitrouter;
    let revision = match extension.expected_state_revision {
        Some(revision) => revision,
        None => match session.operation(&extension.operation_id).await {
            Some(receipt) => receipt.state_revision.saturating_sub(1),
            None => session.head().await.state_revision,
        },
    };
    let receipt = if let Some(previous_id) = &create.previous_response_id {
        let previous = session.response(previous_id).await.ok_or_else(|| {
            CoreError::rejected(ErrorCode::UnauthorizedScope, "unknown session response")
        })?;
        let input = previous.input.as_ref().ok_or_else(|| {
            CoreError::rejected(
                ErrorCode::UnsupportedCapability,
                "legacy response lacks frozen request settings",
            )
        })?;
        if create.model != input.model
            || create
                .multi_agent
                .max_concurrent_subagents
                .is_some_and(|value| Some(value) != input.max_concurrent_subagents)
            || create
                .reasoning
                .as_ref()
                .is_some_and(|value| Some(&value.effort) != input.effort.as_ref())
            || create
                .max_output_tokens
                .is_some_and(|value| Some(value) != input.max_output_tokens)
            || extension
                .routing
                .as_ref()
                .is_some_and(|value| value != &input.routing)
            || extension
                .limits
                .as_ref()
                .is_some_and(|value| Some(value) != input.limits.as_ref())
            || !extension.acceptance_criteria.is_empty()
            || !extension.required_materials.is_empty()
            || extension.verification.is_some()
            || extension.discardable_history.is_some()
        {
            return Err(CoreError::rejected(
                ErrorCode::OperationConflict,
                "continuation cannot replace frozen run settings",
            ));
        }
        let outputs: Vec<FunctionOutput> = serde_json::from_value(create.input.clone())
            .map_err(|_| CoreError::rejected(ErrorCode::InvalidToolResult, "continuation input must contain function outputs with BitRouter result metadata"))?;
        let results = outputs
            .into_iter()
            .map(|output| {
                let command = previous
                    .pending
                    .get(&output.call_id)
                    .filter(|_| output.kind == "function_call_output")
                    .ok_or_else(|| {
                        CoreError::rejected(
                            ErrorCode::InvalidToolResult,
                            "unknown pending function call",
                        )
                    })?;
                Ok(ResponseToolResult {
                    operation_id: output.bitrouter.operation_id,
                    call_id: output.call_id,
                    result: ToolResult {
                        invocation_id: command.invocation_id.clone(),
                        attempt_id: command.attempt_id.clone(),
                        status: output.bitrouter.status,
                        output: output.output,
                        evidence: output.bitrouter.evidence,
                        workspace_revision: output.bitrouter.workspace_revision,
                    },
                })
            })
            .collect::<Result<Vec<_>, CoreError>>()?;
        session
            .continue_response_with_results(&extension.operation_id, revision, previous_id, results)
            .await?
    } else {
        let text = create.input.as_str().ok_or_else(|| {
            CoreError::rejected(
                ErrorCode::UnsupportedCapability,
                "initial managed input must be a task string",
            )
        })?;
        let input = TaskInput {
            text: text.into(),
            model: create.model.clone(),
            effort: create.reasoning.as_ref().map(|value| value.effort.clone()),
            max_output_tokens: create.max_output_tokens,
            max_concurrent_subagents: Some(
                create.multi_agent.max_concurrent_subagents.unwrap_or(3),
            ),
            routing: extension.routing.clone().unwrap_or_default(),
            limits: extension.limits.clone(),
            acceptance_criteria: extension.acceptance_criteria.clone(),
            required_materials: extension.required_materials.clone(),
            verification: extension.verification.clone(),
            discardable_history: extension.discardable_history.clone(),
        };
        session
            .start_response(&extension.operation_id, revision, input)
            .await?
    };
    receipt
        .assigned_ids
        .get("response_id")
        .cloned()
        .ok_or_else(|| {
            CoreError::rejected(
                ErrorCode::OperationConflict,
                "operation does not identify a response",
            )
        })
}

async fn wait(
    session: CoreSession,
    mut progress: watch::Receiver<Progress>,
) -> Result<ResponseExchange, ApiError> {
    loop {
        let current = progress.borrow_and_update().clone();
        if let Some(outcome) = current.outcome {
            outcome?;
            return session
                .response(
                    current
                        .response_id
                        .as_deref()
                        .ok_or_else(ApiError::unavailable)?,
                )
                .await
                .ok_or_else(ApiError::unavailable);
        }
        progress
            .changed()
            .await
            .map_err(|_| ApiError::unavailable())?;
    }
}

fn project(exchange: &ResponseExchange) -> Result<Value, ApiError> {
    let created_at = exchange.created_at.ok_or_else(|| {
        ApiError::core(
            ErrorCode::UnsupportedCapability,
            "legacy response lacks durable HTTP projection metadata",
        )
    })?;
    let final_step = exchange.final_answer.as_ref().and_then(|answer| {
        exchange
            .output
            .iter()
            .rev()
            .find(|output| {
                output.agent_name == "/root"
                    && output
                        .message
                        .content
                        .iter()
                        .filter_map(|part| match part {
                            Content::Text { text, .. } => Some(text.as_str()),
                            _ => None,
                        })
                        .collect::<String>()
                        == *answer
            })
            .map(|output| output.step_id.as_str())
    });
    let mut items = Vec::new();
    for output in &exchange.output {
        for (index, part) in output.message.content.iter().enumerate() {
            let id = format!("{}_{}", output.step_id, index);
            let agent = json!({"agent_name":output.agent_name});
            let item = match part {
                Content::Text { text, .. } => {
                    json!({"id":id,"type":"message","role":"assistant","status":"completed",
                    "agent":agent,"phase":if final_step == Some(output.step_id.as_str()) { "final_answer" } else { "commentary" },"content":[{"type":"output_text","text":text,"annotations":[]}]})
                }
                Content::ToolCall {
                    id: provider_id,
                    name,
                    arguments,
                    provider_executed,
                    ..
                } => {
                    let call_id = output.call_ids.get(provider_id).ok_or_else(|| {
                        ApiError::core(
                            ErrorCode::CheckpointConflict,
                            "response call has no public mapping",
                        )
                    })?;
                    let kind = if *provider_executed {
                        "bitrouter.provider_call"
                    } else if bitrouter_orchestrator::core::protocol::COLLABORATION_TOOLS
                        .contains(&name.as_str())
                    {
                        "bitrouter.collaboration_call"
                    } else {
                        "function_call"
                    };
                    json!({"id":id,"type":kind,"status":"completed","agent":agent,"call_id":call_id,"name":name,"arguments":arguments})
                }
                Content::Reasoning { text, .. } => {
                    json!({"id":id,"type":"bitrouter.reasoning","agent":agent,"text":text})
                }
                _ => {
                    return Err(ApiError::core(
                        ErrorCode::UnsupportedCapability,
                        "response contains an unsupported output part",
                    ));
                }
            };
            items.push(item);
        }
    }
    for (call_id, command) in &exchange.pending {
        if command.verification && command.response_id.as_ref() == Some(&exchange.response_id) {
            items.push(json!({"id":format!("function_{}",command.invocation_id),"type":"function_call",
                "status":"completed","agent":{"agent_name":"/root"},"call_id":call_id,
                "name":command.tool,"arguments":command.arguments.to_string(),"bitrouter":{"verification":true}}));
        }
    }
    if final_step.is_none()
        && let Some(answer) = &exchange.final_answer
    {
        // Verification can finish in an exchange without another model call.
        items.push(json!({"id":format!("{}_final",exchange.response_id),"type":"message","role":"assistant",
            "status":"completed","agent":{"agent_name":"/root"},"phase":"final_answer",
            "content":[{"type":"output_text","text":answer,"annotations":[]}]}));
    }
    let status = if exchange.completed_state_revision.is_none() {
        "in_progress"
    } else if matches!(
        exchange.run_status,
        Some(RunStatus::Failed | RunStatus::Cancelled | RunStatus::RecoveryRequired)
    ) {
        "failed"
    } else {
        "completed"
    };
    Ok(
        json!({"id":exchange.response_id,"object":"response","created_at":created_at,"model":exchange.model,
        "status":status,"output":items,"previous_response_id":exchange.previous_response_id,"usage":null,
        "error":if status=="failed" { json!({"code":"managed_run_stopped","message":"inspect the attributed run disposition"}) } else { Value::Null },
        "bitrouter":{"version":VERSION,"run_id":exchange.run_id,"run_status":exchange.run_status,
            "state_revision":exchange.completed_state_revision.unwrap_or(exchange.created_state_revision),"pending_invocations":exchange.pending,"events":exchange.events}}),
    )
}

struct Streaming {
    session: CoreSession,
    progress: watch::Receiver<Progress>,
    created: bool,
    terminal: bool,
    events: VecDeque<Value>,
    sequence: u64,
    _permit: OwnedSemaphorePermit,
}

fn lifecycle(exchange: &ResponseExchange) -> Result<Value, ApiError> {
    let created_at = exchange.created_at.ok_or_else(ApiError::unavailable)?;
    Ok(
        json!({"id":exchange.response_id,"object":"response","created_at":created_at,
        "model":exchange.model,"status":"in_progress","output":[],"usage":null,"error":null,
        "previous_response_id":exchange.previous_response_id,
        "bitrouter":{"version":VERSION,"run_id":exchange.run_id,"state_revision":exchange.created_state_revision}}),
    )
}

fn item_events(events: &mut VecDeque<Value>, index: usize, item: &Value) {
    let mut initial = item.clone();
    initial["status"] = json!("in_progress");
    if item["type"] == "message" {
        initial["content"] = json!([]);
    }
    if item["type"] == "function_call" {
        initial["arguments"] = json!("");
    }
    events.push_back(json!({"type":"response.output_item.added","output_index":index,"item":initial,"agent":item["agent"]}));
    if item["type"] == "message" {
        if let Some(content) = item["content"].as_array() {
            for (part_index, part) in content.iter().enumerate() {
                events.push_back(json!({"type":"response.content_part.added","item_id":item["id"],"output_index":index,"content_index":part_index,
                    "part":{"type":"output_text","text":"","annotations":[]},"agent":item["agent"]}));
                events.push_back(json!({"type":"response.output_text.delta","item_id":item["id"],"output_index":index,"content_index":part_index,
                    "delta":part["text"],"agent":item["agent"]}));
                events.push_back(json!({"type":"response.output_text.done","item_id":item["id"],"output_index":index,"content_index":part_index,
                    "text":part["text"],"agent":item["agent"]}));
                events.push_back(json!({"type":"response.content_part.done","item_id":item["id"],"output_index":index,"content_index":part_index,
                    "part":part,"agent":item["agent"]}));
            }
        }
    } else if item["type"] == "function_call" {
        events.push_back(json!({"type":"response.function_call_arguments.delta","item_id":item["id"],"output_index":index,
            "delta":item["arguments"],"agent":item["agent"]}));
        events.push_back(json!({"type":"response.function_call_arguments.done","item_id":item["id"],"output_index":index,
            "arguments":item["arguments"],"agent":item["agent"]}));
    }
    events.push_back(json!({"type":"response.output_item.done","output_index":index,"item":item,"agent":item["agent"]}));
}

impl Streaming {
    async fn fill(&mut self) -> Result<(), ApiError> {
        if !self.created {
            loop {
                let current = self.progress.borrow_and_update().clone();
                if let Some(id) = current.response_id {
                    let exchange = self
                        .session
                        .response(&id)
                        .await
                        .ok_or_else(ApiError::unavailable)?;
                    let initial = lifecycle(&exchange)?;
                    self.events
                        .push_back(json!({"type":"response.created","response":initial}));
                    self.events
                        .push_back(json!({"type":"response.in_progress","response":initial}));
                    self.created = true;
                    return Ok(());
                }
                if let Some(result) = current.outcome {
                    result?;
                    return Err(ApiError::unavailable());
                }
                self.progress
                    .changed()
                    .await
                    .map_err(|_| ApiError::unavailable())?;
            }
        }
        let exchange = wait(self.session.clone(), self.progress.clone()).await?;
        let response = project(&exchange)?;
        if let Some(items) = response["output"].as_array() {
            for (index, item) in items.iter().enumerate() {
                item_events(&mut self.events, index, item);
            }
        }
        for retained in &exchange.events {
            self.events
                .push_back(json!({"type":format!("bitrouter.{}", retained.event.kind),
                "agent":{"agent_name":retained.agent_name},"event":retained.event}));
        }
        let kind = if response["status"] == "completed" {
            "response.completed"
        } else {
            "response.failed"
        };
        self.events
            .push_back(json!({"type":kind,"response":response}));
        self.terminal = true;
        Ok(())
    }
}

fn stream_response(
    session: CoreSession,
    progress: watch::Receiver<Progress>,
    permit: OwnedSemaphorePermit,
) -> Response {
    let state = Streaming {
        session,
        progress,
        created: false,
        terminal: false,
        events: VecDeque::new(),
        sequence: 0,
        _permit: permit,
    };
    let stream = futures::stream::unfold(state, |mut state| async move {
        if state.events.is_empty()
            && !state.terminal
            && let Err(error) = state.fill().await
        {
            state
                .events
                .push_back(json!({"type":"error","error":error.0}));
            state.terminal = true;
        }
        let mut value = state.events.pop_front()?;
        value["sequence_number"] = json!(state.sequence);
        state.sequence += 1;
        let event = Event::default()
            .event(value["type"].as_str().unwrap_or("error"))
            .data(value.to_string());
        Some((Ok::<_, Infallible>(event), state))
    });
    let mut response = Sse::new(stream)
        .keep_alive(KeepAlive::default())
        .into_response();
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        http::HeaderValue::from_static("no-store"),
    );
    response
}
