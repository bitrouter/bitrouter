use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use bitrouter_ai::types::ReasoningEffort;
use bitrouter_ai::types::{Message, Role, ToolResultOutput, Usage};
use bitrouter_sdk::App;
use bitrouter_sdk::caller::CallerContext;
use tokio::sync::{Semaphore, mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::control::TurnControl;
use crate::store::{CommitRequest, ExecutionRecord};
use crate::tools::WorkspaceTools;

pub(crate) mod native;

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
    #[serde(default, skip_serializing_if = "fixed_model_mode")]
    pub model_mode: crate::core::protocol::ModelMode,
    pub effort: Option<ReasoningEffort>,
    /// Per-step output reservation, retained with the Thread configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u32>,
    pub instructions: String,
    pub max_steps: u32,
    pub max_tool_calls: u32,
    pub max_duration: Duration,
    pub max_context_bytes: usize,
    pub max_spend_microusd: Option<u64>,
    pub estimate_rates: Option<EstimateRates>,
    tool_mode: ToolMode,
}

fn fixed_model_mode(mode: &crate::core::protocol::ModelMode) -> bool {
    *mode == crate::core::protocol::ModelMode::Fixed
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
            model_mode: crate::core::protocol::ModelMode::Fixed,
            effort,
            max_output_tokens: None,
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

    pub fn with_model_mode(mut self, mode: crate::core::protocol::ModelMode) -> Self {
        self.model_mode = mode;
        self
    }

    pub fn with_output_reservation(mut self, tokens: Option<u32>) -> Self {
        self.max_output_tokens = tokens;
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
    pub(crate) native: Option<native::Saved>,
    pub(crate) cleanup_unconfirmed: bool,
    pub resources: Option<crate::harness::HarnessInventory>,
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
    native: Option<native::Saved>,
    native_record_bytes: usize,
    app: Arc<App>,
    caller: CallerContext,
    tools: WorkspaceTools,
    resource_config: Arc<crate::harness::HarnessConfig>,
    resources: Option<Arc<crate::harness::HarnessResources>>,
    instruction_snapshot: Option<crate::harness::instructions::InstructionSnapshot>,
    refresh_instructions: bool,
    instruction_root: PathBuf,
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
    pub(crate) fn with_native(mut self, saved: Option<native::Saved>) -> Self {
        self.native = saved;
        self
    }

    pub(crate) fn with_native_record_limit(mut self, bytes: usize) -> Self {
        self.native_record_bytes = bytes;
        self
    }
    pub(crate) fn with_instructions(
        mut self,
        snapshot: Option<crate::harness::instructions::InstructionSnapshot>,
        refresh: bool,
        root: PathBuf,
    ) -> Self {
        self.instruction_snapshot = snapshot;
        self.refresh_instructions = refresh;
        self.instruction_root = root;
        self
    }

    async fn startup_instructions(
        &self,
    ) -> Result<crate::harness::instructions::InstructionSnapshot, String> {
        if !self.refresh_instructions
            && let Some(snapshot) = &self.instruction_snapshot
        {
            return Ok(snapshot.clone());
        }
        let cwd = self.tools.root().to_path_buf();
        let root = self.instruction_root.clone();
        let config = self.resource_config.instructions.clone();
        tokio::task::spawn_blocking(move || {
            crate::harness::instructions::InstructionSnapshot::load(&cwd, &root, &config)
        })
        .await
        .map_err(|error| error.to_string())?
    }

    pub(crate) fn with_resources(mut self, config: Arc<crate::harness::HarnessConfig>) -> Self {
        self.resource_config = config;
        self
    }

    fn declarations(&self) -> Vec<bitrouter_ai::types::Tool> {
        let mut tools = self.tools.declarations();
        if let Some(resources) = &self.resources {
            tools.extend(
                resources
                    .inventory
                    .tools
                    .iter()
                    .map(crate::harness::McpTool::declaration),
            );
        }
        tools
    }

    fn validate_call(&self, name: &str, arguments: &str) -> Result<(), String> {
        if let Some(resources) = &self.resources
            && resources.contains(name)
        {
            resources.validate_call(name, arguments)
        } else {
            WorkspaceTools::validate(name, arguments)
        }
    }

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
            || config.max_output_tokens == Some(0)
        {
            return Err(
                "model and positive step, time, context, and output bounds are required".into(),
            );
        }
        if config.max_spend_microusd.is_some() && config.estimate_rates.is_none() {
            return Err("a spend bound requires explicit estimate rates".into());
        }
        let tools =
            WorkspaceTools::new(workspace, config.tool_mode).map_err(|error| error.to_string())?;
        let instruction_root = tools.root().to_path_buf();
        Ok(Self {
            native: None,
            native_record_bytes: 4 * 1024 * 1024,
            app,
            caller,
            tools,
            resource_config: Arc::new(crate::harness::HarnessConfig::default()),
            resources: None,
            instruction_snapshot: None,
            refresh_instructions: true,
            instruction_root,
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
        let native_text = input.prompt.clone();
        let mut prepared = self.clone();
        if !self
            .resource_config
            .instructions
            .project_doc_fallback_filenames
            .is_empty()
        {
            prepared.config.instructions.push_str(&format!(
                "\n\nConfigured instruction fallback filenames: {:?}",
                self.resource_config
                    .instructions
                    .project_doc_fallback_filenames
            ));
        }
        let mut report = if let Some(mut checkpoint) = input.checkpoint {
            checkpoint.final_answer = None;
            checkpoint.status = RunStatus::Failed;
            checkpoint
        } else {
            let user_message = Message::text(Role::User, input.prompt);
            let mut messages = input.messages;
            messages.push(user_message.clone());
            let mut report = RunReport {
                native: self.native.clone(),
                cleanup_unconfirmed: false,
                resources: None,
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
        let mut instruction_record = None;
        let initial_bound = self.bound_status(&report, started, &cancel);
        let preparation = if let Some((_, detail)) = &initial_bound {
            Err(crate::harness::ResourceError::from(detail.clone()))
        } else {
            match self.startup_instructions().await {
                Err(error) => Err(crate::harness::ResourceError::from(error)),
                Ok(snapshot) => crate::harness::HarnessResources::discover(
                    self.tools.root(),
                    &self.resource_config,
                    self.config.tool_mode,
                    &cancel,
                    self.config.max_duration.saturating_sub(started.elapsed()),
                )
                .await
                .map(|resources| (resources, snapshot)),
            }
        };
        let preparation = match preparation {
            Ok((resources, snapshot)) => {
                let error = if report
                    .resources
                    .as_ref()
                    .is_some_and(|old| old != &resources.inventory)
                {
                    Some(
                        "harness resources changed; refuse continuation with a different inventory"
                            .to_string(),
                    )
                } else if report.resources.is_none()
                    && report.steps > 0
                    && !resources.inventory.tools.is_empty()
                {
                    Some("legacy continuation has no frozen harness inventory".to_string())
                } else if resources
                    .inventory
                    .tools
                    .iter()
                    .any(|tool| WorkspaceTools::allowed(ToolMode::Coding, &tool.name))
                {
                    Some("MCP tool collides with a native tool".to_string())
                } else {
                    None
                };
                if let Some(error) = error {
                    let cleanup = resources.shutdown().await;
                    report.cleanup_unconfirmed |= cleanup.is_err();
                    report.unknown_effect |= report.cleanup_unconfirmed;
                    Err(error)
                } else {
                    if self.refresh_instructions || self.instruction_snapshot.is_none() {
                        let message = snapshot.message(self.instruction_snapshot.as_ref());
                        instruction_record = Some(ExecutionRecord::InstructionContext {
                            context_version: report
                                .context_version
                                .saturating_add(u64::from(message.is_some())),
                            snapshot: Box::new(snapshot),
                            message,
                            prepend: self.instruction_snapshot.is_none(),
                        });
                    }
                    report.resources = Some(resources.inventory.clone());
                    prepared.resources = Some(Arc::new(resources));
                    Ok(())
                }
            }
            Err(error) => {
                report.cleanup_unconfirmed |= error.cleanup_unknown;
                report.unknown_effect |= report.cleanup_unconfirmed;
                Err(error.message)
            }
        };
        let self_ = &prepared;
        let preparation_status = initial_bound.map_or_else(
            || {
                if cancel.is_cancelled() {
                    RunStatus::Cancelled
                } else if started.elapsed() >= self.config.max_duration {
                    RunStatus::BoundExceeded
                } else {
                    RunStatus::Failed
                }
            },
            |(status, _)| status,
        );
        let (mut status, mut detail) = match preparation {
            Err(error) => (preparation_status, error),
            Ok(()) => {
                let mut records = Vec::new();
                if let Some(record) = &instruction_record {
                    records.push(record.clone());
                }
                if let Some(inventory) = &report.resources {
                    records.push(ExecutionRecord::HarnessInventory {
                        context_version: match &instruction_record {
                            Some(ExecutionRecord::InstructionContext {
                                context_version, ..
                            }) => *context_version,
                            _ => report.context_version,
                        },
                        inventory: Box::new(inventory.clone()),
                    });
                }
                match commit_execution(&commits, records).await {
                    Err(error) => (RunStatus::Failed, error),
                    Ok(()) => {
                        if let Some(ExecutionRecord::InstructionContext {
                            context_version,
                            message,
                            prepend,
                            ..
                        }) = instruction_record
                        {
                            crate::harness::instructions::apply_message(
                                &mut report.messages,
                                message,
                                prepend,
                            );
                            report.context_version = context_version;
                        }
                        report.active_duration_ms = prior_duration.saturating_add(
                            u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
                        );
                        self_
                            .run_native(
                                &mut report,
                                &native_text,
                                &cancel,
                                RunChannels {
                                    events: events.clone(),
                                    approvals,
                                    commits: commits.clone(),
                                    control,
                                },
                                started,
                            )
                            .await
                    }
                }
            }
        };
        let cleanup_started = Instant::now();
        if let Some(resources) = &prepared.resources
            && let Err(error) = resources.shutdown().await
        {
            report.cleanup_unconfirmed = true;
            report.unknown_effect = true;
            status = RunStatus::Failed;
            detail = error;
        }
        report.active_duration_ms = report.active_duration_ms.saturating_add(
            u64::try_from(cleanup_started.elapsed().as_millis()).unwrap_or(u64::MAX),
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
