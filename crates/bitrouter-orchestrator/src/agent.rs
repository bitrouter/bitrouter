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
use crate::store::{CallRecord, CommitRequest, EffectStatus, ExecutionRecord};
use crate::tools::WorkspaceTools;

const DEFAULT_INSTRUCTIONS: &str = "You are BRO, a coding agent. Work in the selected server workspace. Use read, glob, and grep to inspect code; use write and edit to change it, and the shell tool to run commands and checks. For edit, supply unique oldText values from the original file. Report what actually happened; do not claim a check passed unless its tool result shows it.";
const READ_ONLY_INSTRUCTIONS: &str = "You are BRO, a read-only coding agent. Inspect the selected server workspace using read, glob, and grep. Do not change files or run commands. Report what you actually observed.";
const MAX_MODEL_CONTENT_BYTES: usize = 512 * 1024;
const MAX_LIVE_DELTAS: usize = 8_192;

#[derive(Clone, Copy, serde::Serialize, serde::Deserialize)]
pub struct EstimateRates {
    /// Estimated micro-USD per million prompt tokens.
    pub prompt: u64,
    /// Estimated micro-USD per million completion tokens.
    pub completion: u64,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct AgentConfig {
    pub model: String,
    pub effort: Option<ReasoningEffort>,
    pub instructions: String,
    pub max_steps: u32,
    pub max_tool_calls: u32,
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
            max_tool_calls: 128,
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
    pub tool_calls: u32,
    pub active_duration_ms: u64,
    pub unknown_effect: bool,
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
                if let Some(index) = self.tool_indices.get(&id)
                    && name.is_some()
                    && let Content::ToolCall {
                        arguments: previous,
                        ..
                    } = &self.content[*index]
                    && serde_json::from_str::<serde_json::Value>(previous).is_ok()
                    && serde_json::from_str::<serde_json::Value>(&arguments).is_ok()
                {
                    return Err("duplicate complete tool call ID in provider stream".into());
                }
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

    pub(crate) fn workspace_tools(&self) -> WorkspaceTools {
        self.tools.clone()
    }

    pub fn new(
        app: Arc<App>,
        caller: CallerContext,
        workspace: &Path,
        config: AgentConfig,
    ) -> Result<Self, String> {
        if config.model.trim().is_empty()
            || config.max_steps == 0
            || config.max_tool_calls == 0
            || config.max_duration.is_zero()
            || config.max_context_bytes == 0
        {
            return Err("model and positive step, time, and context bounds are required".into());
        }
        if config.max_spend_microusd.is_some() && config.estimate_rates.is_none() {
            return Err("a spend bound requires explicit estimate rates".into());
        }
        let tools =
            WorkspaceTools::new(workspace, config.tool_mode).map_err(|error| error.to_string())?;
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
        self.run_with_approvals(task, cancel, events, None, None)
            .await
    }

    pub(crate) async fn run_with_approvals(
        &self,
        task: impl Into<String>,
        cancel: CancellationToken,
        events: Option<mpsc::Sender<RunEvent>>,
        approvals: Option<mpsc::Sender<ApprovalRequest>>,
        commits: Option<mpsc::Sender<CommitRequest>>,
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
            tool_calls: 0,
            active_duration_ms: 0,
            unknown_effect: false,
        };
        let user_message = report.messages[0].clone();
        record(&mut report, &events, RunEvent::UserMessage(user_message)).await;
        let started = Instant::now();
        let mut approval_wait = Duration::ZERO;
        let (status, detail) = 'execution: loop {
            let active_started = started + approval_wait;
            if let Some(outcome) = self.bound_status(&report, active_started, &cancel) {
                break outcome;
            }
            let prompt = match context::build(
                &self.config.model,
                self.config.effort,
                &self.config.instructions,
                &report.messages,
                self.tools.declarations(),
                self.config.max_context_bytes,
            ) {
                Ok(prompt) => prompt,
                Err(error) => break (RunStatus::BoundExceeded, error),
            };
            let step_id = uuid::Uuid::new_v4().to_string();
            if let Err(error) = commit_execution(
                &commits,
                vec![ExecutionRecord::ModelRequest {
                    step_id: step_id.clone(),
                    context_version: u64::from(report.steps),
                    prompt: Box::new(prompt.clone()),
                }],
            )
            .await
            {
                break (RunStatus::Failed, error);
            }
            report.steps += 1;
            let remaining = self
                .config
                .max_duration
                .saturating_sub(active_started.elapsed());
            let response = tokio::select! {
                _ = cancel.cancelled() => break (RunStatus::Cancelled, "cancelled during model request".into()),
                result = tokio::time::timeout(remaining, self.execute_turn(prompt, events.as_ref())) => match result {
                    Ok(Ok(response)) => response,
                    Ok(Err(error)) => break (RunStatus::Failed, error),
                    Err(_) => break (RunStatus::BoundExceeded, "time bound reached during model request".into()),
                }
            };
            let mut usage_unavailable = false;
            if let Some(rates) = self.config.estimate_rates {
                match response.result.usage.as_ref() {
                    Some(usage) => {
                        report.estimated_spend_microusd = report
                            .estimated_spend_microusd
                            .saturating_add(estimate_cost(
                                usage.prompt_tokens,
                                usage.completion_tokens,
                                rates,
                            ))
                    }
                    None if self.config.max_spend_microusd.is_some() => usage_unavailable = true,
                    None => {}
                }
            }
            let assistant = Message {
                role: Role::Assistant,
                content: response.result.content,
            };
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
            let mut ids = HashSet::new();
            if calls
                .iter()
                .any(|call| call.id.is_empty() || !ids.insert(call.id.clone()))
            {
                break (
                    RunStatus::Failed,
                    "missing or duplicate tool call ID in model response".into(),
                );
            }
            if report
                .tool_calls
                .saturating_add(u32::try_from(calls.len()).unwrap_or(u32::MAX))
                > self.config.max_tool_calls
            {
                break (
                    RunStatus::BoundExceeded,
                    "tool call bound exceeded before response admission".into(),
                );
            }
            let mut admission = report.messages.clone();
            admission.push(assistant.clone());
            let size = serde_json::to_vec(&admission).map(|value| value.len());
            let reserve = calls.len().saturating_mul(1024);
            if size.map_or(true, |size| {
                size.saturating_add(reserve) > self.config.max_context_bytes
            }) {
                break (
                    RunStatus::BoundExceeded,
                    "complete model response exceeds context settlement capacity".into(),
                );
            }
            let call_records = calls
                .iter()
                .map(|call| CallRecord {
                    item_id: uuid::Uuid::new_v4().to_string(),
                    provider_call_id: call.id.clone(),
                    name: call.name.clone(),
                    arguments: call.arguments.clone(),
                })
                .collect::<Vec<_>>();
            if let Err(error) = commit_execution(
                &commits,
                vec![ExecutionRecord::ModelResponse {
                    step_id: step_id.clone(),
                    item_id: uuid::Uuid::new_v4().to_string(),
                    request_id: response.request_id.clone(),
                    requested_model: self.config.model.clone(),
                    usage: response.result.usage.clone(),
                    estimated_spend_microusd: report.estimated_spend_microusd,
                    message: assistant.clone(),
                    calls: call_records.clone(),
                }],
            )
            .await
            {
                break (RunStatus::Failed, error);
            }
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
            let mut stop = if usage_unavailable {
                Some((
                    RunStatus::Failed,
                    "model usage unavailable for the configured spend bound".into(),
                ))
            } else {
                self.effect_bound_status(&report, started + approval_wait, &cancel)
            };
            if calls.is_empty() {
                if let Some(outcome) = stop {
                    break outcome;
                }
                if final_text.trim().is_empty() {
                    break (
                        RunStatus::Failed,
                        "model returned no final answer or client tool call".into(),
                    );
                }
                report.final_answer = Some(final_text);
                break (RunStatus::Completed, "final answer recorded".into());
            }
            let mut ordinary_batch_error = false;
            for (call, call_record) in calls.into_iter().zip(call_records) {
                report.tool_calls += 1;
                if stop.is_none() {
                    stop = self.effect_bound_status(&report, started + approval_wait, &cancel);
                }
                let mut effect = EffectStatus::NotExecuted;
                let output = if let Some((_, reason)) = &stop {
                    not_executed(reason)
                } else if !WorkspaceTools::allowed(self.config.tool_mode, &call.name) {
                    not_executed("tool is unavailable in this task mode")
                } else if let Err(error) = WorkspaceTools::validate(&call.name, &call.arguments) {
                    not_executed(&error)
                } else {
                    let approved = if WorkspaceTools::read_only(&call.name) {
                        Ok(true)
                    } else if let Some(approvals) = &approvals {
                        let (response, receiver) = oneshot::channel();
                        let request = ApprovalRequest {
                            id: uuid::Uuid::new_v4().to_string(),
                            tool_id: call_record.item_id.clone(),
                            tool_name: call.name.clone(),
                            arguments: call.arguments.clone(),
                            response,
                        };
                        let wait_started = Instant::now();
                        let result = if approvals.send(request).await.is_err() {
                            Err("approval service unavailable".to_string())
                        } else {
                            tokio::select! {
                                _ = cancel.cancelled() => Err("approval cancelled".into()),
                                result = receiver => result.map_err(|_| "approval channel closed".to_string()),
                            }
                        };
                        approval_wait += wait_started.elapsed();
                        result
                    } else {
                        Ok(true)
                    };
                    match approved {
                        Ok(true) if !cancel.is_cancelled() => {
                            if let Err(error) = commit_execution(
                                &commits,
                                vec![ExecutionRecord::ToolIntent {
                                    step_id: step_id.clone(),
                                    call: call_record.clone(),
                                }],
                            )
                            .await
                            {
                                break 'execution (RunStatus::Failed, error);
                            }
                            record(
                                &mut report,
                                &events,
                                RunEvent::ToolStarted {
                                    id: call_record.item_id.clone(),
                                    name: call.name.clone(),
                                },
                            )
                            .await;
                            let result = self
                                .tools
                                .execute(
                                    &call.name,
                                    &call.arguments,
                                    &cancel,
                                    &call_record.item_id,
                                    events.as_ref(),
                                )
                                .await;
                            effect = if !WorkspaceTools::read_only(&call.name)
                                && (cancel.is_cancelled()
                                    || matches!(result, ToolResultOutput::ErrorJson { .. }))
                            {
                                report.unknown_effect = true;
                                EffectStatus::Unknown
                            } else {
                                EffectStatus::Completed
                            };
                            result
                        }
                        Ok(true) => not_executed("cancelled before tool execution"),
                        Ok(false) => ToolResultOutput::ExecutionDenied {
                            reason: Some("user denied tool execution".into()),
                        },
                        Err(error) => not_executed(&error),
                    }
                };
                let message = Message {
                    role: Role::Tool,
                    content: vec![Content::ToolResult {
                        call_id: call.id,
                        tool_name: Some(call.name.clone()),
                        output: output.clone(),
                        dynamic: false,
                        provider_metadata: call.provider_metadata,
                    }],
                };
                report.messages.push(message.clone());
                if let Err(error) = commit_execution(
                    &commits,
                    vec![ExecutionRecord::ToolResult {
                        step_id: step_id.clone(),
                        item_id: call_record.item_id.clone(),
                        message,
                        effect,
                    }],
                )
                .await
                {
                    report.unknown_effect |= effect != EffectStatus::NotExecuted;
                    break 'execution (RunStatus::Failed, error);
                }
                record(
                    &mut report,
                    &events,
                    RunEvent::ToolFinished {
                        id: call_record.item_id,
                        name: call.name,
                        output: output.clone(),
                    },
                )
                .await;
                if report.unknown_effect {
                    stop = Some((
                        RunStatus::Failed,
                        "tool effects require investigation before continuation".into(),
                    ));
                } else if cancel.is_cancelled() {
                    stop = Some((
                        RunStatus::Cancelled,
                        "cancelled during tool execution".into(),
                    ));
                } else if matches!(
                    output,
                    ToolResultOutput::ErrorJson { .. } | ToolResultOutput::ExecutionDenied { .. }
                ) && stop.is_none()
                {
                    ordinary_batch_error = true;
                    // Settle later calls in this batch without launching them;
                    // the model may react to the ordinary error in a new step.
                    stop = Some((
                        RunStatus::Failed,
                        "remaining batch not executed after tool error or denial".into(),
                    ));
                }
            }
            if let Some(outcome) = stop
                && (!ordinary_batch_error
                    || report.unknown_effect
                    || outcome.0 != RunStatus::Failed
                    || cancel.is_cancelled()
                    || usage_unavailable)
            {
                break outcome;
            }
        };
        report.active_duration_ms =
            u64::try_from(started.elapsed().saturating_sub(approval_wait).as_millis())
                .unwrap_or(u64::MAX);
        let settlement = ExecutionRecord::Settled {
            messages: report.messages.clone(),
            model_steps: report.steps,
            tool_calls: report.tool_calls,
            estimated_spend_microusd: report.estimated_spend_microusd,
            active_duration_ms: report.active_duration_ms,
        };
        match commit_execution(&commits, vec![settlement]).await {
            Ok(()) => finish(report, &events, status, detail).await,
            Err(error) => {
                finish(
                    report,
                    &events,
                    RunStatus::Failed,
                    format!("{detail}; settlement commit failed: {error}"),
                )
                .await
            }
        }
    }

    fn bound_status(
        &self,
        report: &RunReport,
        started: Instant,
        cancel: &CancellationToken,
    ) -> Option<(RunStatus, String)> {
        self.effect_bound_status(report, started, cancel)
            .or_else(|| {
                (report.steps >= self.config.max_steps)
                    .then(|| (RunStatus::BoundExceeded, "step bound reached".into()))
            })
    }

    fn effect_bound_status(
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

async fn commit_execution(
    sink: &Option<mpsc::Sender<CommitRequest>>,
    records: Vec<ExecutionRecord>,
) -> Result<(), String> {
    let Some(sink) = sink else {
        return Ok(());
    };
    let (response, receiver) = oneshot::channel();
    sink.send(CommitRequest { records, response })
        .await
        .map_err(|_| "execution commit owner unavailable".to_string())?;
    receiver
        .await
        .map_err(|_| "execution commit acknowledgement lost".to_string())?
}

fn not_executed(reason: &str) -> ToolResultOutput {
    ToolResultOutput::ErrorJson {
        value: serde_json::json!({"error": reason, "execution_status": "not_executed"}),
    }
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
            model_constraints: Default::default(),
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
            [
                "File \"first.txt\"\nL1: first.txt\n",
                "File \"second.txt\"\nL1: second.txt\n",
                "File \"third.txt\"\nL1: third.txt\n"
            ]
        );
        assert!(ids.contains("provider-3"));
        let prompt = context::build(
            "fixture",
            None,
            "inspect",
            &report.messages,
            agent.tools.declarations(),
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
    async fn ordered_read_edit_shell_then_final_answer() -> Result<(), Box<dyn std::error::Error>> {
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
                    "shell",
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
        assert_eq!(names, ["read", "edit", "shell"]);
        assert_eq!(report.messages.len(), 7);
        assert!(matches!(
            &report.messages[2].content[0],
            Content::ToolResult { call_id, output: ToolResultOutput::Text { value }, .. }
                if call_id == "read-1" && value == "File \"note.txt\"\nL1: old\n"
        ));
        let rebuilt = context::build(
            "fixture-model",
            None,
            "fixture instructions",
            &report.messages,
            agent.tools.declarations(),
            512 * 1024,
        )
        .map_err(std::io::Error::other)?;
        assert_eq!(rebuilt.system.as_deref(), Some("fixture instructions"));
        assert_eq!(rebuilt.messages, report.messages);
        Ok(())
    }

    #[tokio::test]
    async fn failed_batch_settles_and_provider_ids_can_repeat_in_later_steps()
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
            "changed"
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
        assert!(outputs[2].is_error());
        assert!(!outputs[3].is_error());
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
                        "shell",
                        serde_json::json!({"command":"touch created.txt"}),
                    ),
                ]),
                turn(vec![text("Inspection complete.")]),
            ],
            |config| *config = AgentConfig::fixed("fixture-model", None).read_only(),
        )?;
        let tools = agent.tools.declarations();
        let names = tools
            .iter()
            .filter_map(|tool| match tool {
                bitrouter_sdk::language_model::Tool::Function { name, .. } => Some(name.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(names, ["read", "glob", "grep"]);
        let (approvals, mut approval_requests) = mpsc::channel(64);
        let report = agent
            .run_with_approvals(
                "inspect",
                CancellationToken::new(),
                None,
                Some(approvals),
                None,
            )
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
        assert_eq!(step_limited.steps, 1);
        assert_eq!(step_limited.tool_calls, 1);
        context::validate_history(&step_limited.messages)?;
        std::fs::remove_file(workspace.path().join("created.txt"))?;

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
            tool_calls: 0,
            active_duration_ms: 0,
            unknown_effect: false,
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

    #[tokio::test]
    async fn cancellation_after_response_commit_settles_all_unstarted_calls()
    -> Result<(), Box<dyn std::error::Error>> {
        let workspace = TempDir::new()?;
        let runner = agent(
            &workspace,
            vec![turn(vec![
                call(
                    "one",
                    "write",
                    serde_json::json!({"path":"one.txt","content":"one"}),
                ),
                call(
                    "two",
                    "write",
                    serde_json::json!({"path":"two.txt","content":"two"}),
                ),
            ])],
            |_| {},
        )?;
        let cancellation = CancellationToken::new();
        let control = cancellation.clone();
        let (commits, mut requests) = mpsc::channel::<CommitRequest>(1);
        let owner = tokio::spawn(async move {
            let mut results = 0;
            while let Some(request) = requests.recv().await {
                if request
                    .records
                    .iter()
                    .any(|record| matches!(record, ExecutionRecord::ModelResponse { .. }))
                {
                    control.cancel();
                }
                for record in &request.records {
                    if let ExecutionRecord::ToolResult { effect, .. } = record {
                        assert_eq!(*effect, EffectStatus::NotExecuted);
                        results += 1;
                    }
                }
                let _ = request.response.send(Ok(()));
            }
            results
        });
        let report = runner
            .run_with_approvals("write both", cancellation, None, None, Some(commits))
            .await;
        assert_eq!(report.status, RunStatus::Cancelled);
        assert_eq!(owner.await?, 2);
        context::validate_history(&report.messages)?;
        assert!(!workspace.path().join("one.txt").exists());
        assert!(!workspace.path().join("two.txt").exists());
        Ok(())
    }

    #[tokio::test]
    async fn duplicate_complete_call_ids_reject_the_response_before_any_effect()
    -> Result<(), Box<dyn std::error::Error>> {
        let workspace = TempDir::new()?;
        let runner = agent(
            &workspace,
            vec![turn(vec![
                call(
                    "earlier",
                    "write",
                    serde_json::json!({"path":"earlier.txt","content":"one"}),
                ),
                call(
                    "duplicate",
                    "write",
                    serde_json::json!({"path":"one.txt","content":"one"}),
                ),
                call(
                    "duplicate",
                    "write",
                    serde_json::json!({"path":"two.txt","content":"two"}),
                ),
            ])],
            |_| {},
        )?;
        let report = runner.run("write", CancellationToken::new(), None).await;
        assert_eq!(report.status, RunStatus::Failed);
        assert!(report.detail.contains("duplicate"));
        assert!(!workspace.path().join("earlier.txt").exists());
        assert!(!workspace.path().join("one.txt").exists());
        assert!(!workspace.path().join("two.txt").exists());
        Ok(())
    }
}
