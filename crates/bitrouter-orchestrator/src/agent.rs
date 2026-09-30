use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bitrouter_sdk::App;
use bitrouter_sdk::caller::CallerContext;
use bitrouter_sdk::language_model::types::ReasoningEffort;
use bitrouter_sdk::language_model::{
    Content, FinishReason, GenerateResult, Message, PipelineResponse, Prompt, ProviderMetadata,
    Role, StreamPart, ToolResultOutput, Usage,
};
use futures::StreamExt;
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::context;
use crate::tools::WorkspaceTools;

const DEFAULT_INSTRUCTIONS: &str = "You are BRO, a coding agent. Work in the selected server workspace. Use read, ls, find, and grep to inspect code; use write and edit to change it, and the available shell tool to run commands and checks. For edit, supply unique oldText values from the original file. Report what actually happened; do not claim a check passed unless its tool result shows it.";
const READ_ONLY_INSTRUCTIONS: &str = "You are BRO, a read-only coding agent. Inspect the selected server workspace using read, ls, find, and grep. Do not change files or run commands. Report what you actually observed.";
const MAX_MODEL_CONTENT_BYTES: usize = 512 * 1024;
const MAX_LIVE_DELTAS: usize = 8_192;

#[derive(Clone, Copy)]
pub struct EstimateRates {
    /// Estimated micro-USD per million prompt tokens.
    pub prompt: u64,
    /// Estimated micro-USD per million completion tokens.
    pub completion: u64,
}

pub struct AgentConfig {
    pub model: String,
    pub effort: Option<ReasoningEffort>,
    pub instructions: String,
    pub max_steps: u32,
    pub max_duration: Duration,
    pub max_context_bytes: usize,
    pub max_spend_microusd: Option<u64>,
    pub estimate_rates: Option<EstimateRates>,
    tool_mode: ToolMode,
}

#[derive(Default, Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolMode {
    #[default]
    Coding,
    ReadOnly,
}

impl AgentConfig {
    pub fn fixed(model: impl Into<String>, effort: Option<ReasoningEffort>) -> Self {
        Self {
            model: model.into(),
            effort,
            instructions: DEFAULT_INSTRUCTIONS.into(),
            max_steps: 32,
            max_duration: Duration::from_secs(600),
            max_context_bytes: 512 * 1024,
            max_spend_microusd: None,
            estimate_rates: None,
            tool_mode: ToolMode::Coding,
        }
    }

    pub fn read_only(mut self) -> Self {
        self.tool_mode = ToolMode::ReadOnly;
        self.instructions = READ_ONLY_INSTRUCTIONS.into();
        self
    }

    pub fn tool_mode(&self) -> ToolMode {
        self.tool_mode
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunStatus {
    Completed,
    Failed,
    Cancelled,
    BoundExceeded,
}

#[derive(Debug, Clone)]
pub enum RunEvent {
    UserMessage(Message),
    ModelTurn {
        request_id: String,
        requested_model: String,
        usage: Option<Usage>,
    },
    AssistantMessage(Message),
    /// Display-only delta; the complete assistant message is recorded later.
    AssistantDelta(String),
    ToolStarted {
        id: String,
        name: String,
    },
    ToolFinished {
        id: String,
        name: String,
        output: ToolResultOutput,
    },
    /// Display-only output; the bounded command result is recorded later.
    ToolOutputDelta {
        id: String,
        source: String,
        text: String,
    },
    Finished {
        status: RunStatus,
        detail: String,
    },
}

pub struct RunReport {
    pub status: RunStatus,
    pub final_answer: Option<String>,
    pub detail: String,
    pub messages: Vec<Message>,
    pub events: Vec<RunEvent>,
    pub steps: u32,
    pub estimated_spend_microusd: u64,
}

pub struct Agent {
    app: Arc<App>,
    caller: CallerContext,
    tools: WorkspaceTools,
    config: AgentConfig,
}

struct PendingCall {
    id: String,
    name: String,
    arguments: String,
    provider_metadata: ProviderMetadata,
}

#[derive(Default)]
struct StreamCollector {
    content: Vec<Content>,
    tool_indices: HashMap<String, usize>,
    usage: Option<Usage>,
    finish_reason: Option<FinishReason>,
    response_id: Option<String>,
    content_bytes: usize,
    live_deltas: usize,
}

impl StreamCollector {
    async fn observe(
        &mut self,
        part: StreamPart,
        events: Option<&mpsc::Sender<RunEvent>>,
    ) -> Result<(), String> {
        match part {
            StreamPart::TextStart { .. } => self.content.push(Content::Text {
                text: String::new(),
                provider_metadata: Default::default(),
            }),
            StreamPart::TextDelta { text } => {
                self.content_bytes = self.content_bytes.saturating_add(text.len());
                if let Some(Content::Text { text: current, .. }) = self.content.last_mut() {
                    current.push_str(&text);
                } else {
                    self.content.push(Content::Text {
                        text: text.clone(),
                        provider_metadata: Default::default(),
                    });
                }
                if let Some(events) = events
                    && self.live_deltas < MAX_LIVE_DELTAS
                {
                    let _ = events.send(RunEvent::AssistantDelta(text)).await;
                    self.live_deltas += 1;
                }
            }
            StreamPart::TextEnd { .. } => {}
            StreamPart::ReasoningStart { .. } => self.content.push(Content::Reasoning {
                text: String::new(),
                provider_metadata: Default::default(),
            }),
            StreamPart::ReasoningDelta { text } => {
                self.content_bytes = self.content_bytes.saturating_add(text.len());
                if let Some(Content::Reasoning { text: current, .. }) = self.content.last_mut() {
                    current.push_str(&text);
                } else {
                    self.content.push(Content::Reasoning {
                        text,
                        provider_metadata: Default::default(),
                    });
                }
            }
            StreamPart::ReasoningEnd { signature, .. } => {
                if let Some(signature) = signature
                    && let Some(Content::Reasoning {
                        provider_metadata, ..
                    }) = self.content.last_mut()
                {
                    provider_metadata.insert(
                        "anthropic".into(),
                        serde_json::json!({"signature": signature}),
                    );
                }
            }
            StreamPart::ToolCallDelta {
                mut id,
                name,
                arguments,
                provider_metadata,
            } => {
                // Gemini sends complete calls with optional provider IDs.
                // Assign each ID-less frame its own identity before collecting
                // it; other protocols still require provider correlation IDs.
                if id.is_empty()
                    && provider_metadata
                        .get("google")
                        .and_then(|metadata| metadata.get("functionCallId"))
                        == Some(&serde_json::Value::Null)
                {
                    id = uuid::Uuid::new_v4().to_string();
                }
                self.content_bytes = self.content_bytes.saturating_add(arguments.len());
                let index = match self.tool_indices.get(&id).copied() {
                    Some(index) => index,
                    None => {
                        let index = self.content.len();
                        self.content.push(Content::ToolCall {
                            id: id.clone(),
                            name: name.clone().unwrap_or_default(),
                            arguments: String::new(),
                            provider_executed: false,
                            dynamic: false,
                            provider_metadata,
                        });
                        self.tool_indices.insert(id, index);
                        index
                    }
                };
                if let Content::ToolCall {
                    name: current_name,
                    arguments: current_arguments,
                    ..
                } = &mut self.content[index]
                {
                    if let Some(name) = name
                        && current_name.is_empty()
                    {
                        *current_name = name;
                    }
                    current_arguments.push_str(&arguments);
                }
            }
            StreamPart::ServerToolCall { .. } | StreamPart::ServerToolResult { .. } => {
                return Err("native model turn unexpectedly invoked an SDK server tool".into());
            }
            StreamPart::File { media_type, data } => self.content.push(Content::File {
                media_type,
                data,
                filename: None,
                provider_metadata: Default::default(),
            }),
            StreamPart::Source { source } => self.content.push(Content::Source {
                source,
                provider_metadata: Default::default(),
            }),
            StreamPart::Usage { usage } => self.usage = Some(usage),
            StreamPart::ResponseStarted { id, .. } => self.response_id = Some(id),
            StreamPart::Finish { reason } => self.finish_reason = Some(reason),
            StreamPart::ResponseCompleted {
                id, status, usage, ..
            } => {
                self.response_id = Some(id);
                if usage.is_some() {
                    self.usage = usage;
                }
                self.finish_reason = Some(match status.as_str() {
                    "completed" => FinishReason::Stop,
                    "incomplete" => FinishReason::Length,
                    other => FinishReason::Error(format!("response ended with status {other}")),
                });
            }
        }
        if self.content_bytes > MAX_MODEL_CONTENT_BYTES {
            return Err("model turn exceeded the 512 KiB content limit".into());
        }
        Ok(())
    }

    async fn finish(mut self, request_id: String) -> Result<PipelineResponse, String> {
        match self.finish_reason.as_ref() {
            Some(FinishReason::Stop | FinishReason::ToolCalls) => {}
            Some(reason) => return Err(format!("model turn did not complete safely: {reason:?}")),
            None => return Err("model stream ended without a finish reason".into()),
        }
        self.content.retain(|part| {
            !matches!(
                part,
                Content::Text { text, .. } | Content::Reasoning { text, .. } if text.is_empty()
            )
        });
        Ok(PipelineResponse {
            request_id,
            result: GenerateResult {
                content: self.content,
                usage: self.usage,
                finish_reason: self.finish_reason,
                response_id: self.response_id,
                stop_details: None,
                provider_metadata: Default::default(),
            },
        })
    }
}

pub(crate) struct ApprovalRequest {
    pub(crate) id: String,
    pub(crate) tool_id: String,
    pub(crate) tool_name: String,
    pub(crate) arguments: String,
    pub(crate) response: oneshot::Sender<bool>,
}

impl Agent {
    async fn execute_turn(
        &self,
        prompt: Prompt,
        events: Option<&mpsc::Sender<RunEvent>>,
    ) -> Result<PipelineResponse, String> {
        let (request_id, mut stream) = self
            .app
            .execute_native_stream(prompt, self.caller.clone())
            .await
            .map_err(|error| error.to_string())?;
        let mut collector = StreamCollector::default();
        while let Some(part) = stream.next().await {
            collector
                .observe(part.map_err(|error| error.to_string())?, events)
                .await?;
        }
        collector.finish(request_id).await
    }

    pub(crate) fn model(&self) -> &str {
        &self.config.model
    }

    pub fn new(
        app: Arc<App>,
        caller: CallerContext,
        workspace: &Path,
        config: AgentConfig,
    ) -> Result<Self, String> {
        if config.model.trim().is_empty()
            || config.max_steps == 0
            || config.max_duration.is_zero()
            || config.max_context_bytes == 0
        {
            return Err("model and positive step, time, and context bounds are required".into());
        }
        if config.max_spend_microusd.is_some() && config.estimate_rates.is_none() {
            return Err("a spend bound requires explicit estimate rates".into());
        }
        let tools = WorkspaceTools::new(workspace).map_err(|error| error.to_string())?;
        Ok(Self {
            app,
            caller,
            tools,
            config,
        })
    }

    pub async fn run(
        &self,
        task: impl Into<String>,
        cancel: CancellationToken,
        events: Option<mpsc::Sender<RunEvent>>,
    ) -> RunReport {
        self.run_with_approvals(task, cancel, events, None).await
    }

    pub(crate) async fn run_with_approvals(
        &self,
        task: impl Into<String>,
        cancel: CancellationToken,
        events: Option<mpsc::Sender<RunEvent>>,
        approvals: Option<mpsc::Sender<ApprovalRequest>>,
    ) -> RunReport {
        let task = task.into();
        let mut report = RunReport {
            status: RunStatus::Failed,
            final_answer: None,
            detail: String::new(),
            messages: vec![Message::text(Role::User, task)],
            events: Vec::new(),
            steps: 0,
            estimated_spend_microusd: 0,
        };
        let user_message = report.messages[0].clone();
        record(&mut report, &events, RunEvent::UserMessage(user_message)).await;
        let started = Instant::now();
        let mut used_call_ids = HashSet::new();
        loop {
            if let Some((status, detail)) = self.bound_status(&report, started, &cancel) {
                return finish(report, &events, status, detail).await;
            }
            let prompt = match context::build(
                &self.config.model,
                self.config.effort,
                &self.config.instructions,
                &report.messages,
                WorkspaceTools::declarations(self.config.tool_mode),
                self.config.max_context_bytes,
            ) {
                Ok(prompt) => prompt,
                Err(error) => {
                    return finish(report, &events, RunStatus::BoundExceeded, error).await;
                }
            };
            report.steps += 1;
            let remaining = self.config.max_duration.saturating_sub(started.elapsed());
            let turn = tokio::select! {
                _ = cancel.cancelled() => {
                    return finish(report, &events, RunStatus::Cancelled, "cancelled during model request".into()).await;
                }
                result = tokio::time::timeout(
                    remaining,
                    self.execute_turn(prompt, events.as_ref())
                ) => result,
            };
            let response = match turn {
                Ok(Ok(response)) => response,
                Ok(Err(error)) => {
                    return finish(report, &events, RunStatus::Failed, error.to_string()).await;
                }
                Err(_) => {
                    return finish(
                        report,
                        &events,
                        RunStatus::BoundExceeded,
                        "time bound reached during model request".into(),
                    )
                    .await;
                }
            };
            let mut usage_unavailable = false;
            if let Some(rates) = self.config.estimate_rates {
                match response.result.usage.as_ref() {
                    Some(usage) => {
                        let cost =
                            estimate_cost(usage.prompt_tokens, usage.completion_tokens, rates);
                        report.estimated_spend_microusd =
                            report.estimated_spend_microusd.saturating_add(cost);
                    }
                    None if self.config.max_spend_microusd.is_some() => {
                        usage_unavailable = true;
                    }
                    None => {}
                }
            }
            let assistant = Message {
                role: Role::Assistant,
                content: response.result.content,
            };
            record(
                &mut report,
                &events,
                RunEvent::ModelTurn {
                    request_id: response.request_id,
                    requested_model: self.config.model.clone(),
                    usage: response.result.usage,
                },
            )
            .await;
            let calls: Vec<PendingCall> = assistant
                .content
                .iter()
                .filter_map(|content| match content {
                    Content::ToolCall {
                        id,
                        name,
                        arguments,
                        provider_metadata,
                        provider_executed: false,
                        ..
                    } => Some(PendingCall {
                        id: id.clone(),
                        name: name.clone(),
                        arguments: arguments.clone(),
                        provider_metadata: provider_metadata.clone(),
                    }),
                    _ => None,
                })
                .collect();
            let final_text = assistant
                .content
                .iter()
                .filter_map(|content| match content {
                    Content::Text { text, .. } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            report.messages.push(assistant.clone());
            record(&mut report, &events, RunEvent::AssistantMessage(assistant)).await;
            if usage_unavailable {
                return finish(
                    report,
                    &events,
                    RunStatus::Failed,
                    "model usage unavailable for the configured spend bound".into(),
                )
                .await;
            }
            if self
                .config
                .max_spend_microusd
                .is_some_and(|bound| report.estimated_spend_microusd >= bound)
            {
                return finish(
                    report,
                    &events,
                    RunStatus::BoundExceeded,
                    "estimated spend bound reached".into(),
                )
                .await;
            }
            if calls.is_empty() {
                if final_text.trim().is_empty() {
                    return finish(
                        report,
                        &events,
                        RunStatus::Failed,
                        "model returned no final answer or client tool call".into(),
                    )
                    .await;
                }
                report.final_answer = Some(final_text);
                return finish(
                    report,
                    &events,
                    RunStatus::Completed,
                    "final answer recorded".into(),
                )
                .await;
            }
            for call in calls {
                if let Some((status, detail)) = self.bound_status(&report, started, &cancel) {
                    return finish(report, &events, status, detail).await;
                }
                report.steps += 1;
                record(
                    &mut report,
                    &events,
                    RunEvent::ToolStarted {
                        id: call.id.clone(),
                        name: call.name.clone(),
                    },
                )
                .await;
                let output = if call.id.is_empty() || !used_call_ids.insert(call.id.clone()) {
                    ToolResultOutput::ErrorJson {
                        value: serde_json::json!({"error": "missing or duplicate tool call id"}),
                    }
                } else if !WorkspaceTools::allowed(self.config.tool_mode, &call.name) {
                    ToolResultOutput::ErrorJson {
                        value: serde_json::json!({"error": "tool is unavailable in this task mode"}),
                    }
                } else {
                    let approved = if WorkspaceTools::read_only(&call.name) {
                        Ok(true)
                    } else {
                        match &approvals {
                            Some(approvals) => {
                                let (response, receiver) = oneshot::channel();
                                let request = ApprovalRequest {
                                    id: uuid::Uuid::new_v4().to_string(),
                                    tool_id: call.id.clone(),
                                    tool_name: call.name.clone(),
                                    arguments: call.arguments.clone(),
                                    response,
                                };
                                if approvals.send(request).await.is_err() {
                                    Err("approval service unavailable".to_string())
                                } else {
                                    let remaining =
                                        self.config.max_duration.saturating_sub(started.elapsed());
                                    tokio::select! {
                                        _ = cancel.cancelled() => Err("approval cancelled".into()),
                                        result = tokio::time::timeout(remaining, receiver) => {
                                            match result {
                                                Ok(Ok(approved)) => Ok(approved),
                                                Ok(Err(_)) => Err("approval channel closed".into()),
                                                Err(_) => Err("approval timed out".into()),
                                            }
                                        }
                                    }
                                }
                            }
                            None => Ok(true),
                        }
                    };
                    match approved {
                        Ok(true) => {
                            if cancel.is_cancelled() {
                                ToolResultOutput::ErrorJson {
                                    value: serde_json::json!({"error": "tool execution cancelled"}),
                                }
                            } else {
                                self.tools
                                    .execute(
                                        &call.name,
                                        &call.arguments,
                                        &cancel,
                                        &call.id,
                                        events.as_ref(),
                                    )
                                    .await
                            }
                        }
                        Ok(false) => ToolResultOutput::ExecutionDenied {
                            reason: Some("user denied tool execution".into()),
                        },
                        Err(error) => ToolResultOutput::ErrorJson {
                            value: serde_json::json!({"error": error}),
                        },
                    }
                };
                let tool_message = Message {
                    role: Role::Tool,
                    content: vec![Content::ToolResult {
                        call_id: call.id.clone(),
                        tool_name: Some(call.name.clone()),
                        output: output.clone(),
                        dynamic: false,
                        provider_metadata: call.provider_metadata,
                    }],
                };
                report.messages.push(tool_message);
                record(
                    &mut report,
                    &events,
                    RunEvent::ToolFinished {
                        id: call.id,
                        name: call.name,
                        output,
                    },
                )
                .await;
                if cancel.is_cancelled() {
                    return finish(
                        report,
                        &events,
                        RunStatus::Cancelled,
                        "cancelled during tool execution; command effects may have occurred".into(),
                    )
                    .await;
                }
            }
        }
    }

    fn bound_status(
        &self,
        report: &RunReport,
        started: Instant,
        cancel: &CancellationToken,
    ) -> Option<(RunStatus, String)> {
        if cancel.is_cancelled() {
            return Some((RunStatus::Cancelled, "cancelled".into()));
        }
        if started.elapsed() >= self.config.max_duration {
            return Some((RunStatus::BoundExceeded, "time bound reached".into()));
        }
        if report.steps >= self.config.max_steps {
            return Some((RunStatus::BoundExceeded, "step bound reached".into()));
        }
        if self
            .config
            .max_spend_microusd
            .is_some_and(|bound| report.estimated_spend_microusd >= bound)
        {
            return Some((
                RunStatus::BoundExceeded,
                "estimated spend bound reached".into(),
            ));
        }
        None
    }
}

fn estimate_cost(prompt_tokens: u64, completion_tokens: u64, rates: EstimateRates) -> u64 {
    let total = u128::from(prompt_tokens) * u128::from(rates.prompt)
        + u128::from(completion_tokens) * u128::from(rates.completion);
    u64::try_from(total.div_ceil(1_000_000)).unwrap_or(u64::MAX)
}

async fn record(report: &mut RunReport, sender: &Option<mpsc::Sender<RunEvent>>, event: RunEvent) {
    if let Some(sender) = sender {
        let _ = sender.send(event.clone()).await;
    }
    report.events.push(event);
}

async fn finish(
    mut report: RunReport,
    sender: &Option<mpsc::Sender<RunEvent>>,
    status: RunStatus,
    detail: String,
) -> RunReport {
    report.status = status;
    report.detail = detail.clone();
    record(&mut report, sender, RunEvent::Finished { status, detail }).await;
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitrouter_sdk::language_model::types::{AuthScheme, GenerateResult, RoutingTarget};
    use bitrouter_sdk::language_model::{
        ApiProtocol, FinishReason, MockExecutor, MockResponse, StaticRoutingTable,
        ToolResultOutput, Usage,
    };
    use tempfile::TempDir;

    fn target() -> RoutingTarget {
        RoutingTarget {
            provider_name: "fixture".into(),
            service_id: "fixture-model".into(),
            api_base: "https://example.invalid".into(),
            api_key: "fixture-key".into(),
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

    fn scripted_app(turns: Vec<GenerateResult>) -> std::io::Result<Arc<App>> {
        let table = StaticRoutingTable::new();
        table.insert("fixture-model", vec![target()]);
        let responses = turns.into_iter().map(mock_stream).collect();
        App::builder()
            .language_model(|builder| {
                builder
                    .routing_table(Arc::new(table))
                    .executor(Arc::new(MockExecutor::new(responses)));
            })
            .build()
            .map(Arc::new)
            .map_err(std::io::Error::other)
    }

    fn turn(content: Vec<Content>) -> GenerateResult {
        let finish_reason = if content
            .iter()
            .any(|part| matches!(part, Content::ToolCall { .. }))
        {
            FinishReason::ToolCalls
        } else {
            FinishReason::Stop
        };
        GenerateResult {
            content,
            usage: Some(Usage {
                prompt_tokens: 10,
                completion_tokens: 5,
                ..Default::default()
            }),
            finish_reason: Some(finish_reason),
            response_id: None,
            stop_details: None,
            provider_metadata: Default::default(),
        }
    }

    fn mock_stream(turn: GenerateResult) -> MockResponse {
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

    fn call(id: &str, name: &str, arguments: serde_json::Value) -> Content {
        Content::ToolCall {
            id: id.into(),
            name: name.into(),
            arguments: arguments.to_string(),
            provider_executed: false,
            dynamic: false,
            provider_metadata: Default::default(),
        }
    }

    fn text(value: &str) -> Content {
        Content::Text {
            text: value.into(),
            provider_metadata: Default::default(),
        }
    }

    fn agent(
        workspace: &TempDir,
        turns: Vec<GenerateResult>,
        configure: impl FnOnce(&mut AgentConfig),
    ) -> std::io::Result<Agent> {
        let app = scripted_app(turns)?;
        let mut config = AgentConfig::fixed("fixture-model", None);
        configure(&mut config);
        Agent::new(app, CallerContext::local(), workspace.path(), config)
            .map_err(std::io::Error::other)
    }

    #[tokio::test]
    async fn gemini_optional_ids_execute_separately_and_replay_provider_ids()
    -> Result<(), Box<dyn std::error::Error>> {
        use bitrouter_sdk::language_model::protocol::{
            OutboundAdapter, SseEvent, generate_content::GenerateContentAdapter,
        };

        let workspace = TempDir::new()?;
        for name in ["first.txt", "second.txt", "third.txt"] {
            std::fs::write(workspace.path().join(name), name)?;
        }
        let adapter = GenerateContentAdapter;
        let wire = serde_json::json!({
            "candidates": [{"content": {"role":"model", "parts":[
                {"functionCall":{"name":"read", "args":{"path":"first.txt"}}, "thoughtSignature":"signature"},
                {"functionCall":{"name":"read", "args":{"path":"second.txt"}}},
                {"functionCall":{"id":"provider-3", "name":"read", "args":{"path":"third.txt"}}}
            ]}, "finishReason":"STOP"}]
        });
        let mut decoder = adapter.stream_decoder();
        let mut parts = decoder.decode(&SseEvent {
            event: None,
            data: wire.to_string(),
        })?;
        parts.extend(decoder.finish()?);
        let table = StaticRoutingTable::new();
        table.insert("fixture-model", vec![target()]);
        let app = App::builder()
            .language_model(|builder| {
                builder
                    .routing_table(Arc::new(table))
                    .executor(Arc::new(MockExecutor::new(vec![
                        MockResponse::Stream(parts),
                        mock_stream(turn(vec![text("inspected")])),
                    ])));
            })
            .build()?;
        let agent = Agent::new(
            Arc::new(app),
            CallerContext::local(),
            workspace.path(),
            AgentConfig::fixed("fixture-model", None).read_only(),
        )?;
        let report = agent.run("inspect", CancellationToken::new(), None).await;
        assert_eq!(report.status, RunStatus::Completed);
        let mut ids = HashSet::new();
        let outputs: Vec<_> = report
            .messages
            .iter()
            .flat_map(|message| &message.content)
            .filter_map(|content| match content {
                Content::ToolResult {
                    call_id, output, ..
                } => {
                    assert!(!call_id.is_empty());
                    assert!(ids.insert(call_id.clone()));
                    Some(output.to_provider_string())
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            outputs,
            ["L1: first.txt\n", "L1: second.txt\n", "L1: third.txt\n"]
        );
        assert!(ids.contains("provider-3"));
        let prompt = context::build(
            "fixture",
            None,
            "inspect",
            &report.messages,
            WorkspaceTools::declarations(ToolMode::ReadOnly),
            512 * 1024,
        )?;
        let replay = adapter.render_request(&prompt)?;
        let calls = &replay["contents"][1]["parts"];
        assert!(calls[0]["functionCall"].get("id").is_none());
        assert!(calls[1]["functionCall"].get("id").is_none());
        assert_eq!(calls[0]["thoughtSignature"], "signature");
        assert_eq!(calls[2]["functionCall"]["id"], "provider-3");
        let results: Vec<_> = replay["contents"]
            .as_array()
            .ok_or("missing contents")?
            .iter()
            .flat_map(|content| content["parts"].as_array().into_iter().flatten())
            .filter_map(|part| part.get("functionResponse"))
            .collect();
        assert_eq!(results.len(), 3);
        assert!(results[0].get("id").is_none());
        assert!(results[1].get("id").is_none());
        assert_eq!(results[2]["id"], "provider-3");
        Ok(())
    }

    #[tokio::test]
    async fn ordered_read_edit_bash_then_final_answer() -> Result<(), Box<dyn std::error::Error>> {
        let workspace = TempDir::new()?;
        std::fs::write(workspace.path().join("note.txt"), "old\n")?;
        let agent = agent(
            &workspace,
            vec![
                turn(vec![
                    call("read-1", "read", serde_json::json!({"path":"note.txt"})),
                    call(
                        "patch-1",
                        "edit",
                        serde_json::json!({
                            "path":"note.txt", "edits":[{"oldText":"old", "newText":"new"}]
                        }),
                    ),
                ]),
                turn(vec![call(
                    "check-1",
                    "bash",
                    serde_json::json!({"command":"echo checked"}),
                )]),
                turn(vec![text("Changed the note and ran a check.")]),
            ],
            |_| {},
        )?;
        let report = agent
            .run("update the note", CancellationToken::new(), None)
            .await;
        assert_eq!(report.status, RunStatus::Completed);
        assert_eq!(
            report.final_answer.as_deref(),
            Some("Changed the note and ran a check.")
        );
        assert_eq!(
            std::fs::read_to_string(workspace.path().join("note.txt"))?,
            "new\n"
        );
        let names: Vec<&str> = report
            .events
            .iter()
            .filter_map(|event| match event {
                RunEvent::ToolStarted { name, .. } => Some(name.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(names, ["read", "edit", "bash"]);
        assert_eq!(report.messages.len(), 7);
        assert!(matches!(
            &report.messages[2].content[0],
            Content::ToolResult { call_id, output: ToolResultOutput::Text { value }, .. }
                if call_id == "read-1" && value == "L1: old\n"
        ));
        let rebuilt = context::build(
            "fixture-model",
            None,
            "fixture instructions",
            &report.messages,
            WorkspaceTools::declarations(ToolMode::Coding),
            512 * 1024,
        )
        .map_err(std::io::Error::other)?;
        assert_eq!(rebuilt.system.as_deref(), Some("fixture instructions"));
        assert_eq!(rebuilt.messages, report.messages);
        Ok(())
    }

    #[tokio::test]
    async fn malformed_unknown_and_duplicate_calls_return_errors_without_effects()
    -> Result<(), Box<dyn std::error::Error>> {
        let workspace = TempDir::new()?;
        std::fs::write(workspace.path().join("note.txt"), "old")?;
        let mut malformed = call("bad-json", "edit", serde_json::json!({}));
        if let Content::ToolCall { arguments, .. } = &mut malformed {
            *arguments = "not json".into();
        }
        let agent = agent(
            &workspace,
            vec![
                turn(vec![
                    call("unknown", "not_a_tool", serde_json::json!({})),
                    malformed,
                    call("dup", "read", serde_json::json!({"path":"note.txt"})),
                ]),
                turn(vec![call(
                    "dup",
                    "edit",
                    serde_json::json!({
                        "path":"note.txt", "edits":[{"oldText":"old", "newText":"changed"}]
                    }),
                )]),
                turn(vec![text("I received the tool errors.")]),
            ],
            |_| {},
        )?;
        let report = agent.run("try tools", CancellationToken::new(), None).await;
        assert_eq!(report.status, RunStatus::Completed);
        assert_eq!(
            std::fs::read_to_string(workspace.path().join("note.txt"))?,
            "old"
        );
        let outputs: Vec<&ToolResultOutput> = report
            .events
            .iter()
            .filter_map(|event| match event {
                RunEvent::ToolFinished { output, .. } => Some(output),
                _ => None,
            })
            .collect();
        assert_eq!(outputs.len(), 4);
        assert!(outputs[0].is_error());
        assert!(outputs[1].is_error());
        assert!(!outputs[2].is_error());
        assert!(outputs[3].is_error());
        Ok(())
    }

    #[tokio::test]
    async fn read_only_mode_denies_unadvertised_effects_without_approval()
    -> Result<(), Box<dyn std::error::Error>> {
        let workspace = TempDir::new()?;
        std::fs::write(workspace.path().join("note.txt"), "unchanged\n")?;
        let agent = agent(
            &workspace,
            vec![
                turn(vec![
                    call(
                        "inspect",
                        "grep",
                        serde_json::json!({"pattern":"unchanged"}),
                    ),
                    call(
                        "write",
                        "write",
                        serde_json::json!({"path":"note.txt","content":"changed"}),
                    ),
                    call(
                        "shell",
                        if cfg!(windows) { "powershell" } else { "bash" },
                        serde_json::json!({"command":"touch created.txt"}),
                    ),
                ]),
                turn(vec![text("Inspection complete.")]),
            ],
            |config| *config = AgentConfig::fixed("fixture-model", None).read_only(),
        )?;
        let tools = WorkspaceTools::declarations(agent.config.tool_mode());
        let names = tools
            .iter()
            .filter_map(|tool| match tool {
                bitrouter_sdk::language_model::Tool::Function { name, .. } => Some(name.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(names, ["read", "ls", "find", "grep"]);
        let (approvals, mut approval_requests) = mpsc::channel(64);
        let report = agent
            .run_with_approvals("inspect", CancellationToken::new(), None, Some(approvals))
            .await;
        assert!(approval_requests.try_recv().is_err());
        assert_eq!(report.status, RunStatus::Completed);
        assert_eq!(
            std::fs::read_to_string(workspace.path().join("note.txt"))?,
            "unchanged\n"
        );
        assert!(!workspace.path().join("created.txt").exists());
        let outputs = report
            .events
            .iter()
            .filter_map(|event| match event {
                RunEvent::ToolFinished { output, .. } => Some(output),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(outputs.len(), 3);
        assert!(!outputs[0].is_error());
        assert!(outputs[1].is_error());
        assert!(outputs[2].is_error());
        Ok(())
    }

    #[tokio::test]
    async fn cancellation_and_each_bound_stop_before_a_new_effect()
    -> Result<(), Box<dyn std::error::Error>> {
        let workspace = TempDir::new()?;
        let effect = turn(vec![call(
            "create",
            "write",
            serde_json::json!({"path":"created.txt", "content":"created"}),
        )]);
        let cancel = CancellationToken::new();
        cancel.cancel();
        let cancelled = agent(&workspace, vec![effect.clone()], |_| {})?
            .run("create", cancel, None)
            .await;
        assert_eq!(cancelled.status, RunStatus::Cancelled);

        let step_limited = agent(&workspace, vec![effect.clone()], |config| {
            config.max_steps = 1
        })?
        .run("create", CancellationToken::new(), None)
        .await;
        assert_eq!(step_limited.status, RunStatus::BoundExceeded);

        let spend_limited = agent(&workspace, vec![effect.clone()], |config| {
            config.max_spend_microusd = Some(1);
            config.estimate_rates = Some(EstimateRates {
                prompt: 1_000_000,
                completion: 1_000_000,
            });
        })?
        .run("create", CancellationToken::new(), None)
        .await;
        assert_eq!(spend_limited.status, RunStatus::BoundExceeded);

        let context_limited = agent(&workspace, vec![effect], |config| {
            config.max_context_bytes = 1;
        })?
        .run("create", CancellationToken::new(), None)
        .await;
        assert_eq!(context_limited.status, RunStatus::BoundExceeded);

        let time_agent = agent(&workspace, vec![], |config| {
            config.max_duration = Duration::from_millis(1);
        })?;
        let empty_report = RunReport {
            status: RunStatus::Failed,
            final_answer: None,
            detail: String::new(),
            messages: Vec::new(),
            events: Vec::new(),
            steps: 0,
            estimated_spend_microusd: 0,
        };
        assert!(matches!(
            time_agent.bound_status(
                &empty_report,
                Instant::now() - Duration::from_secs(1),
                &CancellationToken::new()
            ),
            Some((RunStatus::BoundExceeded, _))
        ));
        assert!(!workspace.path().join("created.txt").exists());
        Ok(())
    }

    #[tokio::test]
    async fn streamed_text_is_visible_before_the_complete_message()
    -> Result<(), Box<dyn std::error::Error>> {
        let workspace = TempDir::new()?;
        let agent = agent(&workspace, vec![turn(vec![text("hello")])], |_| {})?;
        let (sender, mut receiver) = mpsc::channel(64);
        let report = agent
            .run("greet", CancellationToken::new(), Some(sender))
            .await;
        assert_eq!(report.status, RunStatus::Completed);
        let mut saw_delta = false;
        while let Ok(event) = receiver.try_recv() {
            match event {
                RunEvent::AssistantDelta(text) => {
                    assert_eq!(text, "hello");
                    saw_delta = true;
                }
                RunEvent::AssistantMessage(_) => assert!(saw_delta),
                _ => {}
            }
        }
        assert!(saw_delta);
        assert!(
            report
                .events
                .iter()
                .all(|event| !matches!(event, RunEvent::AssistantDelta(_)))
        );
        Ok(())
    }

    #[tokio::test]
    async fn length_truncated_tool_call_never_executes() -> Result<(), Box<dyn std::error::Error>> {
        let workspace = TempDir::new()?;
        let table = StaticRoutingTable::new();
        table.insert("fixture-model", vec![target()]);
        let app = App::builder()
            .language_model(|builder| {
                builder
                    .routing_table(Arc::new(table))
                    .executor(Arc::new(MockExecutor::new(vec![MockResponse::Stream(
                        vec![
                            StreamPart::ToolCallDelta {
                                id: "write-1".into(),
                                name: Some("write".into()),
                                arguments: r#"{"path":"created.txt","content":"created"}"#.into(),
                                provider_metadata: Default::default(),
                            },
                            StreamPart::Finish {
                                reason: FinishReason::Length,
                            },
                        ],
                    )])));
            })
            .build()?;
        let agent = Agent::new(
            Arc::new(app),
            CallerContext::local(),
            workspace.path(),
            AgentConfig::fixed("fixture-model", None),
        )
        .map_err(std::io::Error::other)?;
        let report = agent.run("create", CancellationToken::new(), None).await;
        assert_eq!(report.status, RunStatus::Failed);
        assert!(!workspace.path().join("created.txt").exists());
        assert!(
            report
                .events
                .iter()
                .all(|event| !matches!(event, RunEvent::ToolStarted { .. }))
        );
        Ok(())
    }
}
