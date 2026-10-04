use std::collections::{HashSet, VecDeque};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bitrouter_sdk::App;
use bitrouter_sdk::caller::CallerContext;
use bitrouter_sdk::language_model::types::ReasoningEffort;
use bitrouter_sdk::language_model::{Content, Message, Role, ToolResultOutput, Usage};
use tokio::sync::{Semaphore, mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use self::batch::{BatchControl, Invocation, PendingCall};
use self::stream::{AssistantAttempt, StreamCollector};
use crate::context;
use crate::control::{ModelBoundary, TurnControl};
use crate::item::{CallOrigin, CallRecord};
use crate::store::{CommitRequest, EffectStatus, ExecutionRecord};
use crate::tools::WorkspaceTools;

mod batch;
mod stream;

const DEFAULT_INSTRUCTIONS: &str = "You are BRO, a coding agent. Work in the selected server workspace. Use read, glob, and grep to inspect code; use write and edit to change it, and the shell tool to run commands and checks. For edit, supply unique oldText values from the original file. Report what actually happened; do not claim a check passed unless its tool result shows it.";
const READ_ONLY_INSTRUCTIONS: &str = "You are BRO, a read-only coding agent. Inspect the selected server workspace using read, glob, and grep. Do not change files or run commands. Report what you actually observed.";

#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
pub struct EstimateRates {
    /// Estimated micro-USD per million prompt tokens.
    pub prompt: u64,
    /// Estimated micro-USD per million completion tokens.
    pub completion: u64,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Completed,
    Failed,
    Cancelled,
    BoundExceeded,
}

#[derive(Debug, Clone)]
pub enum RunEvent {
    UserMessage {
        item_id: String,
        message: Message,
    },
    AssistantStarted {
        step_id: String,
        item_id: String,
    },
    ModelTurn {
        step_id: String,
        item_id: String,
        request_id: String,
        requested_model: String,
        usage: Option<Usage>,
    },
    AssistantMessage {
        item_id: String,
        message: Message,
    },
    AssistantInterrupted {
        item_id: String,
        partial: Message,
        detail: String,
    },
    /// Display-only delta; the complete assistant message is recorded later.
    AssistantDelta {
        item_id: String,
        text: String,
    },
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
    pub context_version: u64,
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

#[derive(Clone)]
pub struct Agent {
    app: Arc<App>,
    caller: CallerContext,
    tools: WorkspaceTools,
    config: AgentConfig,
    workers: Arc<Semaphore>,
    parallel_tools: usize,
}

pub(crate) struct RunInput {
    pub prompt: String,
    pub messages: Vec<Message>,
    pub user_item_id: String,
    pub context_version: u64,
    pub checkpoint: Option<RunReport>,
    pub complete_checkpoint: bool,
    pub restored_verification: Option<(
        crate::turn::VerificationStatus,
        crate::turn::VerificationEvidence,
    )>,
}

pub(crate) struct RunChannels {
    pub events: Option<mpsc::Sender<RunEvent>>,
    pub approvals: Option<mpsc::Sender<ApprovalRequest>>,
    pub commits: Option<mpsc::Sender<CommitRequest>>,
    pub control: Option<TurnControl>,
}

pub(crate) struct ApprovalRequest {
    pub(crate) id: String,
    pub(crate) tool_id: String,
    pub(crate) tool_name: String,
    pub(crate) arguments: String,
    pub(crate) response: oneshot::Sender<bool>,
}

impl Agent {
    pub(crate) fn verification_limits(&self) -> (Duration, u32) {
        (self.config.max_duration, self.config.max_tool_calls)
    }

    pub(crate) fn with_tool_workers(
        mut self,
        workers: Arc<Semaphore>,
        parallel_tools: usize,
    ) -> Self {
        self.workers = workers;
        self.parallel_tools = parallel_tools;
        self
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
            workers: Arc::new(Semaphore::new(16)),
            parallel_tools: 4,
        })
    }

    pub async fn run(
        &self,
        task: impl Into<String>,
        cancel: CancellationToken,
        events: Option<mpsc::Sender<RunEvent>>,
    ) -> RunReport {
        self.run_with_approvals(task, cancel, events, None, None, None)
            .await
    }

    pub(crate) async fn run_with_approvals(
        &self,
        task: impl Into<String>,
        cancel: CancellationToken,
        events: Option<mpsc::Sender<RunEvent>>,
        approvals: Option<mpsc::Sender<ApprovalRequest>>,
        commits: Option<mpsc::Sender<CommitRequest>>,
        user_item_id: Option<String>,
    ) -> RunReport {
        self.run_context(
            RunInput {
                prompt: task.into(),
                messages: Vec::new(),
                user_item_id: user_item_id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
                context_version: 0,
                checkpoint: None,
                complete_checkpoint: false,
                restored_verification: None,
            },
            cancel,
            RunChannels {
                events,
                approvals,
                commits,
                control: None,
            },
        )
        .await
    }

    pub(crate) async fn run_context(
        &self,
        input: RunInput,
        cancel: CancellationToken,
        channels: RunChannels,
    ) -> RunReport {
        if input.complete_checkpoint
            && let Some(report) = input.checkpoint
        {
            return report;
        }
        let prior_duration = input
            .checkpoint
            .as_ref()
            .map_or(0, |report| report.active_duration_ms);
        let mut limited = self.clone();
        limited.config.max_duration = self
            .config
            .max_duration
            .saturating_sub(Duration::from_millis(prior_duration));
        limited
            .run_pass(input, cancel, channels, prior_duration)
            .await
    }

    async fn run_pass(
        &self,
        input: RunInput,
        cancel: CancellationToken,
        channels: RunChannels,
        prior_duration: u64,
    ) -> RunReport {
        let RunChannels {
            events,
            approvals,
            commits,
            control,
        } = channels;
        let mut report = if let Some(mut checkpoint) = input.checkpoint {
            checkpoint.final_answer = None;
            checkpoint.status = RunStatus::Failed;
            checkpoint
        } else {
            let user_message = Message::text(Role::User, input.prompt);
            let mut messages = input.messages;
            messages.push(user_message.clone());
            let mut report = RunReport {
                context_version: input.context_version.saturating_add(1),
                status: RunStatus::Failed,
                final_answer: None,
                detail: String::new(),
                messages,
                events: Vec::new(),
                steps: 0,
                estimated_spend_microusd: 0,
                tool_calls: 0,
                active_duration_ms: 0,
                unknown_effect: false,
            };
            record(
                &mut report,
                &events,
                RunEvent::UserMessage {
                    item_id: input.user_item_id,
                    message: user_message,
                },
            )
            .await;
            report
        };
        let started = Instant::now();
        let mut approval_wait = Duration::ZERO;
        let mut active_model = None;
        let (status, detail) = 'execution: loop {
            report.active_duration_ms = prior_duration.saturating_add(
                u64::try_from(started.elapsed().saturating_sub(approval_wait).as_millis())
                    .unwrap_or(u64::MAX),
            );
            if let Err(error) = commit_execution(
                &commits,
                vec![ExecutionRecord::RunCheckpoint {
                    context_version: report.context_version,
                    messages: report.messages.clone(),
                    model_steps: report.steps,
                    tool_calls: report.tool_calls,
                    estimated_spend_microusd: report.estimated_spend_microusd,
                    active_duration_ms: report.active_duration_ms,
                }],
            )
            .await
            {
                break (RunStatus::Failed, error);
            }
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
            let item_id = uuid::Uuid::new_v4().to_string();
            let (prompt, version) = if let Some(control) = &control {
                let (response, receive) = oneshot::channel();
                if control
                    .models
                    .send(ModelBoundary {
                        prompt,
                        step_id: step_id.clone(),
                        item_id: item_id.clone(),
                        context_version: report.context_version,
                        max_bytes: self.config.max_context_bytes,
                        response,
                    })
                    .await
                    .is_err()
                {
                    break (RunStatus::Failed, "model boundary owner unavailable".into());
                }
                match receive.await {
                    Ok(Ok(value)) => value,
                    Ok(Err(error)) => break (RunStatus::Failed, error),
                    Err(_) => break (RunStatus::Failed, "model boundary owner lost".into()),
                }
            } else {
                if let Err(error) = commit_execution(
                    &commits,
                    vec![ExecutionRecord::ModelRequest {
                        step_id: step_id.clone(),
                        item_id: item_id.clone(),
                        context_version: report.context_version,
                        prompt: Box::new(prompt.clone()),
                    }],
                )
                .await
                {
                    break (RunStatus::Failed, error);
                }
                (prompt, report.context_version)
            };
            report.messages = prompt.messages.clone();
            report.context_version = version;
            report.steps += 1;
            record(
                &mut report,
                &events,
                RunEvent::AssistantStarted {
                    step_id: step_id.clone(),
                    item_id: item_id.clone(),
                },
            )
            .await;
            let attempt = active_model.insert(AssistantAttempt {
                step_id: step_id.clone(),
                item_id: item_id.clone(),
                collector: StreamCollector::default(),
            });
            let remaining = self
                .config
                .max_duration
                .saturating_sub(active_started.elapsed());
            let response = tokio::select! {
                biased;
                _ = cancel.cancelled() => break (RunStatus::Cancelled, "cancelled during model request".into()),
                result = tokio::time::timeout(remaining, self.execute_turn(prompt, attempt, events.as_ref())) => match result {
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
                    origin: CallOrigin::Model,
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
                    item_id: item_id.clone(),
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
            active_model = None;
            record(
                &mut report,
                &events,
                RunEvent::ModelTurn {
                    step_id: step_id.clone(),
                    item_id: item_id.clone(),
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
            report.context_version = report.context_version.saturating_add(1);
            record(
                &mut report,
                &events,
                RunEvent::AssistantMessage {
                    item_id,
                    message: assistant,
                },
            )
            .await;
            let mut stop = if usage_unavailable {
                Some((
                    RunStatus::Failed,
                    "model usage unavailable for the configured spend bound".into(),
                ))
            } else {
                self.effect_bound_status(&report, started + approval_wait, &cancel)
            };
            if calls.is_empty()
                && control.as_ref().is_some_and(|value| value.fence.pending())
                && stop.is_none()
            {
                continue;
            }
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
            let mut pending = calls.into_iter().zip(call_records).collect::<VecDeque<_>>();
            while let Some((call, call_record)) = pending.pop_front() {
                if stop.is_none() && control.as_ref().is_some_and(|value| value.fence.pending()) {
                    stop = Some((RunStatus::Failed, "not_executed_due_to_steer".into()));
                    ordinary_batch_error = true;
                }
                if stop.is_none() && WorkspaceTools::read_only(&call.name) {
                    let mut group = vec![Invocation {
                        call,
                        record: call_record,
                    }];
                    while pending
                        .front()
                        .is_some_and(|(call, _)| WorkspaceTools::read_only(&call.name))
                    {
                        if let Some((call, record)) = pending.pop_front() {
                            group.push(Invocation { call, record });
                        }
                    }
                    let control = BatchControl {
                        cancel: &cancel,
                        events: &events,
                        commits: &commits,
                        started: started + approval_wait,
                        steering: &control,
                    };
                    match self
                        .execute_shared_group(&step_id, group, &mut report, control)
                        .await
                    {
                        Ok(outcome) => {
                            stop = outcome.stop;
                            ordinary_batch_error |= outcome.ordinary_error;
                        }
                        Err(error) => break 'execution (RunStatus::Failed, error),
                    }
                    continue;
                }
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
                            let control = BatchControl {
                                cancel: &cancel,
                                events: &events,
                                commits: &commits,
                                started: started + approval_wait,
                                steering: &control,
                            };
                            match self
                                .execute_exclusive(
                                    &step_id,
                                    &call,
                                    &call_record,
                                    &mut report,
                                    control,
                                )
                                .await
                            {
                                Ok((result, status)) => {
                                    effect = status;
                                    report.unknown_effect |= effect == EffectStatus::Unknown;
                                    result
                                }
                                Err(error) => break 'execution (RunStatus::Failed, error),
                            }
                        }
                        Ok(true) => not_executed("cancelled before tool execution"),
                        Ok(false)
                            if control.as_ref().is_some_and(|value| value.fence.pending()) =>
                        {
                            not_executed("not_executed_due_to_steer")
                        }
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
                report.context_version = report.context_version.saturating_add(1);
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
                } else if let Some(outcome) =
                    self.effect_bound_status(&report, started + approval_wait, &cancel)
                {
                    stop = Some(outcome);
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
        let (status, detail) = if let Some(attempt) = active_model {
            let partial = attempt.collector.partial();
            let terminal = ExecutionRecord::ModelInterrupted {
                step_id: attempt.step_id,
                item_id: attempt.item_id.clone(),
                request_id: attempt.collector.request_id,
                usage: attempt.collector.usage,
                partial: partial.clone(),
                detail: detail.clone(),
            };
            match commit_execution(&commits, vec![terminal]).await {
                Ok(()) => {
                    record(
                        &mut report,
                        &events,
                        RunEvent::AssistantInterrupted {
                            item_id: attempt.item_id,
                            partial,
                            detail: detail.clone(),
                        },
                    )
                    .await;
                    (status, detail)
                }
                Err(error) => (
                    RunStatus::Failed,
                    format!("{detail}; interrupted Item commit failed: {error}"),
                ),
            }
        } else {
            (status, detail)
        };
        report.active_duration_ms = prior_duration.saturating_add(
            u64::try_from(started.elapsed().saturating_sub(approval_wait).as_millis())
                .unwrap_or(u64::MAX),
        );
        let settlement = ExecutionRecord::Settled {
            outcome: Some(crate::store::SettlementOutcome {
                status,
                final_answer: report.final_answer.clone(),
                detail: detail.clone(),
            }),
            context_version: report.context_version,
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
mod tests;
