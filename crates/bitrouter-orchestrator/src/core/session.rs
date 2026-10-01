//! Managed session transitions. Provider calls and harness commits run outside
//! the state lock. A separate commit serializer preserves the one-batch rule.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use async_trait::async_trait;
use bitrouter_sdk::App;
use bitrouter_sdk::caller::CallerContext;
use bitrouter_sdk::language_model::native::{
    NativeAttemptReport, NativeExecutionControl, NativePlan,
};
use bitrouter_sdk::language_model::types::{
    Content, FinishReason, GenerationParams, Message, Prompt, ReasoningEffort, Role, Tool,
    ToolChoice, ToolResultOutput,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use super::checkpoint::{
    BatchIdentity, Checkpoint, CheckpointAck, CheckpointBatch, CheckpointPayload, CommitGate,
    DurableEvent, DurableHead, sha256,
};
use super::protocol::{
    Bind, Capabilities, CommitStatus, CoreError, ErrorCode, HarnessManifest, Limits,
    OperationDisposition, OperationReceipt, ServerMessage, TaskInput, ToolExecute, ToolOutcome,
    ToolResult, VERSION, validate_id,
};

/// Implemented by the authenticated durable harness connection, including an
/// in-process harness. Returning an ACK means the atomic append is durable.
#[async_trait]
pub trait HarnessPort: Send + Sync {
    async fn commit(&self, batch: CheckpointBatch) -> Result<CheckpointAck, CoreError>;
    async fn send(&self, message: ServerMessage) -> Result<(), CoreError>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Running,
    Waiting,
    RecoveryRequired,
    Completed,
    Failed,
    Cancelled,
}

impl RunStatus {
    fn terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttemptRecord {
    pub attempt_id: String,
    pub index: u32,
    pub report: Option<NativeAttemptReport>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelStep {
    pub step_id: String,
    pub decision_id: String,
    pub context_revision: u64,
    pub plan: Option<NativePlan>,
    pub attempts: Vec<AttemptRecord>,
    pub settled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Invocation {
    pub dispatch: ToolExecute,
    pub public_call_id: String,
    pub provider_call_id: String,
    pub result: Option<ToolResult>,
    pub consumed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RootRun {
    pub run_id: String,
    pub agent_turn_id: String,
    pub input: TaskInput,
    pub limits: Limits,
    pub status: RunStatus,
    pub model_attempts: u32,
    pub active_ms: u64,
    pub final_answer: Option<String>,
    pub terminal_reason: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentStatus {
    Runnable,
    ModelRunning,
    WaitingTool,
    WaitingMessage,
    Interrupted,
    RecoveryRequired,
    Completed,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentTurn {
    pub run_id: String,
    pub agent_turn_id: String,
    pub input: TaskInput,
    pub status: AgentStatus,
    pub steps: Vec<ModelStep>,
    pub invocations: Vec<Invocation>,
    pub final_answer: Option<String>,
    pub terminal_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentState {
    pub agent_id: String,
    pub parent_id: Option<String>,
    pub display_path: String,
    pub depth: u32,
    pub context_revision: u64,
    pub history: Vec<Message>,
    pub turn: Option<AgentTurn>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSnapshot {
    pub session_id: String,
    pub agent_id: String,
    pub manifest: HarnessManifest,
    pub agents: BTreeMap<String, AgentState>,
    pub run: Option<RootRun>,
    pub operations: BTreeMap<String, OperationReceipt>,
}

impl SessionSnapshot {
    pub fn root_turn(&self) -> Option<&AgentTurn> {
        self.agents
            .get(&self.agent_id)
            .and_then(|agent| agent.turn.as_ref())
    }
}

struct LiveSession {
    state: SessionSnapshot,
    gate: CommitGate,
    pending: Option<SessionSnapshot>,
    sent_tools: BTreeSet<String>,
    disconnected: CancellationToken,
}

struct Shared {
    live: Mutex<LiveSession>,
    commits: Mutex<()>,
    driver: Mutex<()>,
    inputs: Mutex<()>,
    app: Arc<App>,
    caller: CallerContext,
    harness: Arc<dyn HarnessPort>,
    limits: Limits,
}

#[derive(Clone)]
pub struct CoreSession {
    shared: Arc<Shared>,
}

impl CoreSession {
    pub async fn bind(
        binding: Bind,
        capabilities: &Capabilities,
        app: Arc<App>,
        caller: CallerContext,
        harness: Arc<dyn HarnessPort>,
    ) -> Result<Self, CoreError> {
        binding.grant.validate()?;
        binding.manifest.validate(capabilities, &binding.limits)?;
        if binding.grant.core_instance_id != capabilities.core_instance_id {
            return Err(reject(
                ErrorCode::UnauthorizedScope,
                "grant names a different core instance",
            ));
        }
        if binding.durable_head != DurableHead::default() || binding.checkpoint.is_some() {
            return Err(reject(
                ErrorCode::RecoveryRequired,
                "a nonempty session requires explicit restoration",
            ));
        }
        let agent_id = id("agent");
        let root = AgentState {
            agent_id: agent_id.clone(),
            parent_id: None,
            display_path: "/root".into(),
            depth: 0,
            context_revision: 0,
            history: Vec::new(),
            turn: None,
        };
        let state = SessionSnapshot {
            session_id: binding.grant.session_id.clone(),
            agent_id: agent_id.clone(),
            manifest: binding.manifest,
            agents: BTreeMap::from([(agent_id, root)]),
            run: None,
            operations: BTreeMap::new(),
        };
        let session = Self {
            shared: Arc::new(Shared {
                live: Mutex::new(LiveSession {
                    state,
                    gate: CommitGate::new(
                        binding.grant,
                        binding.durable_head,
                        binding.limits.clone(),
                    )?,
                    pending: None,
                    sent_tools: BTreeSet::new(),
                    disconnected: CancellationToken::new(),
                }),
                commits: Mutex::new(()),
                driver: Mutex::new(()),
                inputs: Mutex::new(()),
                app,
                caller,
                harness,
                limits: binding.limits,
            }),
        };
        session
            .transition("session.bound", |_, _| Ok(json!({})))
            .await?;
        Ok(session)
    }

    pub async fn snapshot(&self) -> SessionSnapshot {
        self.shared.live.lock().await.state.clone()
    }
    pub async fn head(&self) -> DurableHead {
        self.shared.live.lock().await.gate.head().clone()
    }

    pub async fn disconnect(&self) {
        let mut live = self.shared.live.lock().await;
        live.gate.disconnect();
        live.disconnected.cancel();
    }

    /// The host authenticates the session before calling this method. The
    /// expected revision is checked only for new input, never before replay.
    pub async fn start(
        &self,
        operation_id: &str,
        expected_revision: u64,
        input: TaskInput,
    ) -> Result<OperationReceipt, CoreError> {
        let _input = self.shared.inputs.lock().await;
        validate_id(operation_id)?;
        let fingerprint = digest(
            &json!({"type":"start", "expected_state_revision":expected_revision,"input":input}),
        )?;
        if let Some(receipt) = self.replay(operation_id, &fingerprint).await? {
            return Ok(receipt);
        }
        validate_input(&input, &self.shared.limits)?;
        let operation_id = operation_id.to_owned();
        self.transition("input.accepted", |state, head| {
            validate_verification(&input, &state.manifest)?;
            if head.state_revision != expected_revision {
                return Err(reject(
                    ErrorCode::StaleRevision,
                    "root input revision is stale",
                ));
            }
            if state.run.as_ref().is_some_and(|run| !run.status.terminal()) {
                return Err(reject(
                    ErrorCode::Busy,
                    "session already owns an active root run",
                ));
            }
            let run_id = id("run");
            let turn_id = id("turn");
            let limits = input
                .limits
                .clone()
                .unwrap_or_else(|| self.shared.limits.clone());
            let agent = state
                .agents
                .get_mut(&state.agent_id)
                .ok_or_else(|| reject(ErrorCode::CheckpointConflict, "root agent is absent"))?;
            agent.history.push(Message::text(Role::User, &input.text));
            agent.context_revision += 1;
            agent.turn = Some(AgentTurn {
                run_id: run_id.clone(),
                agent_turn_id: turn_id.clone(),
                input: input.clone(),
                status: AgentStatus::Runnable,
                steps: Vec::new(),
                invocations: Vec::new(),
                final_answer: None,
                terminal_reason: None,
            });
            state.run = Some(RootRun {
                run_id: run_id.clone(),
                agent_turn_id: turn_id.clone(),
                input,
                limits,
                status: RunStatus::Running,
                model_attempts: 0,
                active_ms: 0,
                final_answer: None,
                terminal_reason: None,
            });
            let receipt = OperationReceipt {
                operation_id: operation_id.clone(),
                request_sha256: fingerprint.clone(),
                disposition: OperationDisposition::Accepted,
                assigned_ids: BTreeMap::from([
                    ("run_id".into(), run_id),
                    ("agent_id".into(), state.agent_id.clone()),
                    ("agent_turn_id".into(), turn_id),
                ]),
                state_revision: head.state_revision + 1,
                error: None,
            };
            state
                .operations
                .insert(operation_id.clone(), receipt.clone());
            encode(&receipt)
        })
        .await?;
        self.operation(&operation_id).await.ok_or_else(|| {
            reject(
                ErrorCode::CheckpointUnavailable,
                "accepted operation was not committed",
            )
        })
    }

    pub async fn operation(&self, operation_id: &str) -> Option<OperationReceipt> {
        self.shared
            .live
            .lock()
            .await
            .state
            .operations
            .get(operation_id)
            .cloned()
    }

    async fn replay(
        &self,
        operation_id: &str,
        fingerprint: &str,
    ) -> Result<Option<OperationReceipt>, CoreError> {
        let live = self.shared.live.lock().await;
        let receipt = live.state.operations.get(operation_id).or_else(|| {
            live.pending
                .as_ref()
                .and_then(|state| state.operations.get(operation_id))
        });
        if let Some(receipt) = receipt {
            if receipt.request_sha256 != fingerprint {
                return Err(reject(
                    ErrorCode::OperationConflict,
                    "operation identity has different content",
                ));
            }
            if live.state.operations.contains_key(operation_id) {
                return Ok(Some(receipt.clone()));
            }
            return Err(CoreError {
                code: ErrorCode::CheckpointUnavailable,
                message: "operation is awaiting durable acknowledgement; reconcile the head".into(),
                commit_status: CommitStatus::Unknown,
            });
        }
        Ok(None)
    }

    /// Drive the root until it needs external tools or reaches a committed
    /// terminal. A second driver cannot dispatch the same agent concurrently.
    pub async fn drive(&self) -> Result<SessionSnapshot, CoreError> {
        let _driver = self
            .shared
            .driver
            .try_lock()
            .map_err(|_| reject(ErrorCode::Busy, "session driver is already active"))?;
        loop {
            let state = self.snapshot().await;
            let run = state
                .run
                .as_ref()
                .ok_or_else(|| reject(ErrorCode::Busy, "no root task is accepted"))?;
            if run.status.terminal() || run.status == RunStatus::RecoveryRequired {
                return Ok(state);
            }
            let agent_id = state.agent_id.clone();
            let turn = state
                .root_turn()
                .ok_or_else(|| reject(ErrorCode::CheckpointConflict, "root turn is absent"))?;
            if turn.invocations.iter().any(|call| !call.consumed) {
                if turn
                    .invocations
                    .iter()
                    .filter(|call| !call.consumed)
                    .all(|call| call.result.is_some())
                {
                    self.consume_results(&agent_id).await?;
                    continue;
                }
                self.dispatch_tools(&agent_id).await?;
                return Ok(self.snapshot().await);
            }
            if turn.final_answer.is_some() {
                let failed = turn.invocations.iter().any(|call| {
                    call.dispatch.verification
                        && call
                            .result
                            .as_ref()
                            .is_some_and(|result| result.status != ToolOutcome::Succeeded)
                });
                self.transition(
                    if failed {
                        "run.failed"
                    } else {
                        "run.completed"
                    },
                    |state, _| {
                        let turn = agent_turn(state, &agent_id)?;
                        turn.status = if failed {
                            AgentStatus::Failed
                        } else {
                            AgentStatus::Completed
                        };
                        let answer = turn.final_answer.clone();
                        let run = active_run(state)?;
                        run.final_answer = answer;
                        run.status = if failed {
                            RunStatus::Failed
                        } else {
                            RunStatus::Completed
                        };
                        run.terminal_reason = Some(
                            if failed {
                                "harness verification failed"
                            } else {
                                "root and all tool effects settled"
                            }
                            .into(),
                        );
                        Ok(json!({"final_answer":run.final_answer,"reason":run.terminal_reason}))
                    },
                )
                .await?;
                continue;
            }
            self.execute_agent_step(&agent_id).await?;
        }
    }

    async fn execute_agent_step(&self, agent_id: &str) -> Result<(), CoreError> {
        let state = self.snapshot().await;
        let run = state
            .run
            .as_ref()
            .ok_or_else(|| reject(ErrorCode::Busy, "no root task is accepted"))?;
        let turn = state
            .agents
            .get(agent_id)
            .and_then(|agent| agent.turn.as_ref())
            .ok_or_else(|| reject(ErrorCode::Busy, "agent has no turn"))?;
        if turn.steps.last().is_some_and(|step| !step.settled) {
            self.transition("run.recovery_required", |state, _| {
                agent_turn(state, agent_id)?.status = AgentStatus::RecoveryRequired;
                let run = active_run(state)?;
                run.status = RunStatus::RecoveryRequired;
                run.terminal_reason =
                    Some("previous model driver ended before settlement was applied".into());
                Ok(json!({"reason":run.terminal_reason}))
            })
            .await?;
            return Err(reject(
                ErrorCode::RecoveryRequired,
                "an interrupted model driver must be reconciled before retry",
            ));
        }
        if run.model_attempts >= run.limits.model_attempts
            || run.active_ms >= run.limits.active_seconds.saturating_mul(1000)
        {
            self.fail(agent_id, "run model or active-time limit reached")
                .await?;
            return Ok(());
        }
        let prompt = match build_prompt(&state, agent_id) {
            Ok(prompt) => prompt,
            Err(error) => {
                self.fail(agent_id, &error.message).await?;
                return Err(error);
            }
        };
        let step_id = id("step");
        self.transition("model.step.preparing", |state, _| {
            let agent = agent_mut(state, agent_id)?;
            let revision = agent.context_revision;
            let turn = agent
                .turn
                .as_mut()
                .ok_or_else(|| reject(ErrorCode::Busy, "agent has no turn"))?;
            turn.status = AgentStatus::ModelRunning;
            turn.steps.push(ModelStep {
                step_id: step_id.clone(),
                decision_id: id("decision"),
                context_revision: revision,
                plan: None,
                attempts: Vec::new(),
                settled: false,
            });
            Ok(json!({"step_id":step_id}))
        })
        .await?;
        let control = Arc::new(StepControl {
            session: self.clone(),
            agent_id: agent_id.to_owned(),
            step_id: step_id.clone(),
        });
        let response = self
            .shared
            .app
            .execute_native_controlled(prompt, self.shared.caller.clone(), control)
            .await;
        match response {
            Ok(response) => {
                if let Err(error) = self
                    .apply_output(agent_id, &step_id, &response.request_id, &response.result)
                    .await
                {
                    if self.shared.live.lock().await.gate.can_dispatch() {
                        self.fail(agent_id, &error.message).await?;
                    }
                    return Err(error);
                }
            }
            Err(error) => {
                if self.shared.live.lock().await.gate.can_dispatch() {
                    self.fail(agent_id, &error.to_string()).await?;
                } else {
                    return Err(reject(
                        ErrorCode::CheckpointUnavailable,
                        "model outcome awaits durable reconciliation",
                    ));
                }
            }
        }
        Ok(())
    }

    pub async fn tool_result(
        &self,
        operation_id: &str,
        result: ToolResult,
    ) -> Result<OperationReceipt, CoreError> {
        let _input = self.shared.inputs.lock().await;
        validate_id(operation_id)?;
        let fingerprint = digest(&result)?;
        if let Some(receipt) = self.replay(operation_id, &fingerprint).await? {
            return Ok(receipt);
        }
        if !self
            .shared
            .live
            .lock()
            .await
            .sent_tools
            .contains(&result.invocation_id)
        {
            return Err(reject(
                ErrorCode::InvalidToolResult,
                "tool result has no dispatched invocation",
            ));
        }
        if result.output.len() as u64 > self.snapshot().await.manifest.max_tool_output_bytes {
            return Err(reject(
                ErrorCode::LimitExceeded,
                "tool result exceeds manifest output bound",
            ));
        }
        self.transition("tool.result", |state, head| {
            let turn = state
                .agents
                .values_mut()
                .filter_map(|agent| agent.turn.as_mut())
                .find(|turn| {
                    turn.invocations
                        .iter()
                        .any(|call| call.dispatch.invocation_id == result.invocation_id)
                })
                .ok_or_else(|| reject(ErrorCode::InvalidToolResult, "unknown tool invocation"))?;
            let call = turn
                .invocations
                .iter_mut()
                .find(|call| call.dispatch.invocation_id == result.invocation_id)
                .ok_or_else(|| reject(ErrorCode::InvalidToolResult, "unknown tool invocation"))?;
            if call.dispatch.attempt_id != result.attempt_id {
                return Err(reject(
                    ErrorCode::InvalidToolResult,
                    "tool attempt does not match invocation",
                ));
            }
            if let Some(previous) = &call.result {
                if previous != &result {
                    return Err(reject(
                        ErrorCode::OperationConflict,
                        "tool outcome was already recorded with different content",
                    ));
                }
            } else {
                call.result = Some(result.clone());
            }
            if result.status == ToolOutcome::EffectUnknown {
                turn.status = AgentStatus::RecoveryRequired;
                active_run(state)?.status = RunStatus::RecoveryRequired;
            }
            state.manifest.workspace_revision = result.workspace_revision.clone();
            let receipt = OperationReceipt {
                operation_id: operation_id.to_owned(),
                request_sha256: fingerprint,
                disposition: OperationDisposition::Accepted,
                assigned_ids: BTreeMap::from([(
                    "invocation_id".into(),
                    result.invocation_id.clone(),
                )]),
                state_revision: head.state_revision + 1,
                error: None,
            };
            state.operations.insert(operation_id.to_owned(), receipt);
            encode(&result)
        })
        .await?;
        self.operation(operation_id).await.ok_or_else(|| {
            reject(
                ErrorCode::CheckpointUnavailable,
                "tool result was not committed",
            )
        })
    }

    async fn apply_output(
        &self,
        agent_id: &str,
        step_id: &str,
        request_id: &str,
        output: &bitrouter_sdk::language_model::types::GenerateResult,
    ) -> Result<(), CoreError> {
        self.transition("model.output.applied", |state, head| {
            let manifest = state.manifest.clone();
            let limit = active_run(state)?.limits.outstanding_tools;
            let agent = agent_mut(state, agent_id)?;
            let context_revision = agent.context_revision;
            let turn = agent.turn.as_mut().ok_or_else(|| reject(ErrorCode::Busy, "agent has no turn"))?;
            let step = turn
                .steps
                .last_mut()
                .filter(|step| step.step_id == step_id)
                .ok_or_else(|| {
                    reject(ErrorCode::StaleRevision, "model step is no longer current")
                })?;
            let observed = step
                .attempts
                .last()
                .and_then(|attempt| attempt.report.as_ref())
                .ok_or_else(|| {
                    reject(
                        ErrorCode::CheckpointUnavailable,
                        "complete model output is not committed",
                    )
                })?;
            if observed.request_id != request_id || observed.result.as_ref() != Some(output) {
                return Err(reject(
                    ErrorCode::OperationConflict,
                    "settled result differs from committed provider output",
                ));
            }
            if !matches!(
                output.finish_reason,
                Some(FinishReason::Stop | FinishReason::ToolCalls)
            ) {
                return Err(reject(
                    ErrorCode::UnsupportedCapability,
                    "incomplete or refused model output cannot authorize actions",
                ));
            }
            let mut ids = BTreeSet::new();
            let mut calls = Vec::new();
            let mut message = Message {
                role: Role::Assistant,
                content: output.content.clone(),
            };
            for content in &mut message.content {
                if let Content::ToolCall {
                    id: call_id,
                    name,
                    arguments,
                    provider_executed,
                    ..
                } = content
                {
                    let planned = step.plan.as_ref().is_some_and(|plan| {
                        plan.prompt.tools.iter().any(|tool| matches!(tool, Tool::Function { name: planned_name, .. } if planned_name == name))
                            && match &plan.prompt.tool_choice {
                                Some(ToolChoice::None) => false,
                                Some(ToolChoice::Tool { name: required }) => required == name,
                                _ => true,
                            }
                    });
                    if *provider_executed || !planned || !manifest.tools.iter().any(|tool| tool.name == *name) {
                        return Err(reject(
                            ErrorCode::UnsupportedCapability,
                            "model called a tool outside the frozen harness manifest",
                        ));
                    }
                    if call_id.is_empty() {
                        *call_id = id("provider_call");
                    }
                    if !ids.insert(call_id.clone()) {
                        return Err(reject(
                            ErrorCode::InvalidToolResult,
                            "duplicate provider call ID within one model step",
                        ));
                    }
                    let arguments: Value = serde_json::from_str(arguments).map_err(|_| {
                        reject(
                            ErrorCode::InvalidToolResult,
                            "tool arguments are not complete JSON",
                        )
                    })?;
                    if !arguments.is_object() {
                        return Err(reject(
                            ErrorCode::InvalidToolResult,
                            "tool arguments must be an object",
                        ));
                    }
                    calls.push(Invocation {
                        dispatch: ToolExecute {
                            invocation_id: id("invocation"),
                            attempt_id: id("tool_attempt"),
                            run_id: turn.run_id.clone(),
                            agent_id: agent_id.to_owned(),
                            agent_turn_id: turn.agent_turn_id.clone(),
                            step_id: step_id.to_owned(),
                            context_revision,
                            tool: name.clone(),
                            arguments,
                            tool_manifest_digest: manifest.tool_manifest_digest.clone(),
                            permission_revision: manifest.permission_revision,
                            workspace_id: manifest.workspace_id.clone(),
                            execution_epoch: head.execution_epoch,
                            authorizing_event_seq: head.event_seq + 1,
                            verification: false,
                        },
                        public_call_id: id("call"),
                        provider_call_id: call_id.clone(),
                        result: None,
                        consumed: false,
                    });
                }
            }
            if step.plan.as_ref().is_some_and(|plan| {
                (calls.is_empty() && matches!(plan.prompt.tool_choice, Some(ToolChoice::Required | ToolChoice::Tool { .. })))
                    || (calls.len() > 1 && plan.prompt.params.parallel_tool_calls == Some(false))
            }) {
                return Err(reject(ErrorCode::InvalidToolResult, "model output violates its frozen tool choice"));
            }
            if calls.len() as u32 > limit {
                return Err(reject(
                    ErrorCode::LimitExceeded,
                    "model output exceeds outstanding tool limit",
                ));
            }
            step.settled = true;
            if calls.is_empty() {
                let answer = message
                    .content
                    .iter()
                    .filter_map(|content| {
                        if let Content::Text { text, .. } = content {
                            Some(text.as_str())
                        } else {
                            None
                        }
                    })
                    .collect::<String>();
                turn.final_answer = Some(answer);
                if let Some(verification) = &turn.input.verification {
                    calls.push(Invocation {
                        dispatch: ToolExecute {
                            invocation_id: id("invocation"),
                            attempt_id: id("tool_attempt"),
                            run_id: turn.run_id.clone(),
                            agent_id: agent_id.to_owned(),
                            agent_turn_id: turn.agent_turn_id.clone(),
                            step_id: step_id.to_owned(),
                            context_revision,
                            tool: verification.tool.clone(),
                            arguments: verification.arguments.clone(),
                            tool_manifest_digest: manifest.tool_manifest_digest,
                            permission_revision: manifest.permission_revision,
                            workspace_id: manifest.workspace_id,
                            execution_epoch: head.execution_epoch,
                            authorizing_event_seq: head.event_seq + 1,
                            verification: true,
                        },
                        public_call_id: id("verification"),
                        provider_call_id: String::new(),
                        result: None,
                        consumed: false,
                    });
                }
            }
            turn.status = if calls.is_empty() {
                AgentStatus::Runnable
            } else {
                AgentStatus::WaitingTool
            };
            let waiting = turn.status == AgentStatus::WaitingTool;
            turn.invocations.extend(calls);
            agent.history.push(message);
            agent.context_revision += 1;
            active_run(state)?.status = if waiting { RunStatus::Waiting } else { RunStatus::Running };
            Ok(json!({"step_id":step_id,"request_id":request_id}))
        })
        .await
    }

    async fn consume_results(&self, agent_id: &str) -> Result<(), CoreError> {
        self.transition("tool.results.consumed", |state, _| {
            let agent = agent_mut(state, agent_id)?;
            let turn = agent
                .turn
                .as_mut()
                .ok_or_else(|| reject(ErrorCode::Busy, "agent has no turn"))?;
            let mut messages = Vec::new();
            let mut verified = None;
            for call in turn.invocations.iter_mut().filter(|call| !call.consumed) {
                let result = call
                    .result
                    .as_ref()
                    .ok_or_else(|| reject(ErrorCode::Busy, "tool batch is not complete"))?;
                if result.status == ToolOutcome::EffectUnknown {
                    return Err(reject(
                        ErrorCode::RecoveryRequired,
                        "tool effect is unknown",
                    ));
                }
                if call.dispatch.verification {
                    verified = Some(result.status == ToolOutcome::Succeeded);
                } else {
                    let output = if result.status == ToolOutcome::Succeeded {
                        ToolResultOutput::Text {
                            value: result.output.clone(),
                        }
                    } else {
                        ToolResultOutput::ErrorText {
                            value: format!("{:?}: {}", result.status, result.output),
                        }
                    };
                    messages.push(Message {
                        role: Role::Tool,
                        content: vec![Content::ToolResult {
                            call_id: call.provider_call_id.clone(),
                            tool_name: Some(call.dispatch.tool.clone()),
                            dynamic: false,
                            output,
                            provider_metadata: Default::default(),
                        }],
                    });
                }
                call.consumed = true;
            }
            turn.status = AgentStatus::Runnable;
            if let Some(success) = verified {
                turn.terminal_reason = Some(format!("harness verification succeeded: {success}"));
            }
            agent.history.extend(messages);
            agent.context_revision += 1;
            active_run(state)?.status = RunStatus::Running;
            Ok(json!({}))
        })
        .await
    }

    async fn dispatch_tools(&self, agent_id: &str) -> Result<(), CoreError> {
        loop {
            let (command, disconnected) = {
                let mut live = self.shared.live.lock().await;
                if !live.gate.can_dispatch() {
                    return Err(reject(
                        ErrorCode::CheckpointUnavailable,
                        "tool dispatch is blocked by durable state",
                    ));
                }
                if live.state.run.as_ref().is_none_or(|run| {
                    !matches!(run.status, RunStatus::Running | RunStatus::Waiting)
                }) {
                    return Err(reject(
                        ErrorCode::RecoveryRequired,
                        "run no longer permits tool dispatch",
                    ));
                }
                let command = live
                    .state
                    .agents
                    .get(agent_id)
                    .and_then(|agent| agent.turn.as_ref())
                    .filter(|turn| {
                        matches!(
                            turn.status,
                            AgentStatus::Runnable | AgentStatus::WaitingTool
                        )
                    })
                    .and_then(|turn| {
                        turn.invocations.iter().find(|call| {
                            call.result.is_none()
                                && !live.sent_tools.contains(&call.dispatch.invocation_id)
                        })
                    })
                    .map(|call| call.dispatch.clone());
                if let Some(command) = &command {
                    live.sent_tools.insert(command.invocation_id.clone());
                }
                (command, live.disconnected.clone())
            };
            let Some(command) = command else {
                return Ok(());
            };
            // Sending owns the delivery attempt independently of the caller's
            // drive future. Dropping that future must not strand a sent marker
            // before the harness actually receives the command.
            let session = self.clone();
            let sending = tokio::spawn(async move {
                let delivered = tokio::select! {
                    biased;
                    _ = disconnected.cancelled() => Err(reject(ErrorCode::CheckpointUnavailable, "harness disconnected before tool delivery")),
                    delivered = session.shared.harness.send(ServerMessage::ToolExecute(command)) => delivered,
                };
                if delivered.is_err() {
                    session.disconnect().await;
                }
                delivered
            });
            match sending.await {
                Ok(delivered) => delivered?,
                Err(error) => {
                    self.disconnect().await;
                    return Err(reject(ErrorCode::RecoveryRequired, &error.to_string()));
                }
            }
        }
    }

    async fn fail(&self, agent_id: &str, reason: &str) -> Result<(), CoreError> {
        self.transition("run.failed", |state, _| {
            let turn = agent_turn(state, agent_id)?;
            turn.status = AgentStatus::Failed;
            turn.terminal_reason = Some(reason.to_owned());
            let run = active_run(state)?;
            run.status = RunStatus::Failed;
            run.terminal_reason = Some(reason.to_owned());
            Ok(json!({"reason":reason}))
        })
        .await
    }

    async fn ensure_dispatch(&self, agent_id: &str) -> Result<(), CoreError> {
        let live = self.shared.live.lock().await;
        if !live.gate.can_dispatch()
            || live
                .state
                .run
                .as_ref()
                .is_none_or(|run| run.status != RunStatus::Running)
            || live
                .state
                .agents
                .get(agent_id)
                .and_then(|agent| agent.turn.as_ref())
                .is_none_or(|turn| turn.status != AgentStatus::ModelRunning)
        {
            return Err(reject(
                ErrorCode::CheckpointUnavailable,
                "model dispatch is no longer authorized",
            ));
        }
        Ok(())
    }

    async fn transition<F>(&self, kind: &str, change: F) -> Result<(), CoreError>
    where
        F: FnOnce(&mut SessionSnapshot, &DurableHead) -> Result<Value, CoreError> + Send,
    {
        let _commit = self.shared.commits.lock().await;
        let (batch, disconnected) = {
            let mut live = self.shared.live.lock().await;
            let mut next = live.state.clone();
            let head = live.gate.head().clone();
            let payload = change(&mut next, &head)?;
            let artifacts = next
                .agents
                .values()
                .filter_map(|agent| agent.turn.as_ref())
                .flat_map(|turn| &turn.invocations)
                .filter_map(|call| call.result.as_ref())
                .flat_map(|result| result.evidence.iter().cloned())
                .map(|reference| (reference.artifact_id.clone(), reference))
                .collect::<BTreeMap<_, _>>()
                .into_values()
                .collect();
            let proposed = CheckpointPayload {
                identity: BatchIdentity {
                    batch_id: id("batch"),
                    session_id: next.session_id.clone(),
                    execution_epoch: live.gate.grant().execution_epoch,
                    core_instance_id: live.gate.grant().core_instance_id.clone(),
                },
                base_event_seq: head.event_seq,
                base_state_revision: head.state_revision,
                events: vec![DurableEvent {
                    event_seq: head.event_seq + 1,
                    kind: kind.to_owned(),
                    run_id: next.run.as_ref().map(|run| run.run_id.clone()),
                    agent_id: Some(next.agent_id.clone()),
                    payload,
                }],
                checkpoint: Checkpoint {
                    schema_version: VERSION,
                    state_revision: head.state_revision + 1,
                    artifact_refs: artifacts,
                    state: encode(&next)?,
                },
            };
            let batch = live.gate.propose(proposed)?.clone();
            live.pending = Some(next);
            (batch, live.disconnected.clone())
        };
        let acknowledgement = tokio::select! {
            biased;
            _ = disconnected.cancelled() => Err(reject(ErrorCode::CheckpointUnavailable, "durable authority disconnected during commit")),
            acknowledgement = self.shared.harness.commit(batch) => acknowledgement,
        };
        let mut live = self.shared.live.lock().await;
        let result = match acknowledgement {
            Ok(ack) => match live.gate.acknowledge(&ack) {
                Ok(Some(_)) => {
                    live.state = live.pending.take().ok_or_else(|| {
                        reject(ErrorCode::CheckpointConflict, "missing tentative state")
                    })?;
                    Ok(())
                }
                Ok(None) => Err(reject(
                    ErrorCode::CheckpointConflict,
                    "unexpected replayed ACK for pending transition",
                )),
                Err(error) => Err(error),
            },
            Err(error) => Err(error),
        };
        result.map_err(|mut error| {
            live.gate.disconnect();
            live.disconnected.cancel();
            // Once submitted, a batch may be durable even when its matching
            // ACK is missing, stale, or malformed. Only head reconciliation
            // can resolve that uncertainty.
            error.commit_status = CommitStatus::Unknown;
            error
        })
    }
}

struct StepControl {
    session: CoreSession,
    agent_id: String,
    step_id: String,
}

#[async_trait]
impl NativeExecutionControl for StepControl {
    async fn plan(&self, plan: NativePlan) -> bitrouter_sdk::Result<()> {
        self.session
            .transition("model.plan", |state, _| {
                let step = current_step(state, &self.agent_id, &self.step_id)?;
                if step.plan.is_some() {
                    return Err(reject(
                        ErrorCode::OperationConflict,
                        "model plan is already frozen",
                    ));
                }
                step.plan = Some(plan.clone());
                encode(&plan)
            })
            .await
            .map_err(sdk_error)
    }

    async fn before_attempt(
        &self,
        request_id: &str,
        attempt_index: u32,
    ) -> bitrouter_sdk::Result<()> {
        self.session.transition("model.attempt.intent", |state, _| {
            let run = active_run(state)?;
            if run.status != RunStatus::Running || run.model_attempts >= run.limits.model_attempts || run.active_ms >= run.limits.active_seconds.saturating_mul(1000) { return Err(reject(ErrorCode::LimitExceeded, "model attempt is no longer admitted")); }
            let turn = agent_turn(state, &self.agent_id)?;
            if turn.status != AgentStatus::ModelRunning { return Err(reject(ErrorCode::Busy, "agent is no longer running this model step")); }
            let step = turn.steps.last_mut().filter(|step| step.step_id == self.step_id).ok_or_else(|| reject(ErrorCode::StaleRevision, "attempt step changed"))?;
            let plan = step.plan.as_ref().ok_or_else(|| reject(ErrorCode::CheckpointUnavailable, "model plan is not committed"))?;
            if plan.request_id != request_id || attempt_index as usize != step.attempts.len() || plan.routes.get(attempt_index as usize).is_none()
                || step.attempts.last().is_some_and(|attempt| attempt.report.is_none()) {
                return Err(reject(ErrorCode::OperationConflict, "attempt does not follow its immutable model plan"));
            }
            let attempt_id = id("attempt");
            step.attempts.push(AttemptRecord { attempt_id: attempt_id.clone(), index: attempt_index, report: None });
            active_run(state)?.model_attempts += 1;
            Ok(json!({"attempt_id":attempt_id,"request_id":request_id,"attempt_index":attempt_index}))
        }).await.map_err(sdk_error)?;
        self.session
            .ensure_dispatch(&self.agent_id)
            .await
            .map_err(sdk_error)
    }

    async fn after_attempt(&self, report: NativeAttemptReport) {
        let recorded = self
            .session
            .transition("model.attempt.outcome", |state, _| {
                let turn = agent_turn(state, &self.agent_id)?;
                let step = turn
                    .steps
                    .last_mut()
                    .filter(|step| step.step_id == self.step_id)
                    .ok_or_else(|| reject(ErrorCode::StaleRevision, "outcome step changed"))?;
                let plan = step.plan.as_ref().ok_or_else(|| {
                    reject(ErrorCode::CheckpointUnavailable, "outcome has no plan")
                })?;
                if plan.request_id != report.request_id
                    || plan.routes.get(report.attempt_index as usize) != Some(&report.route)
                {
                    return Err(reject(
                        ErrorCode::OperationConflict,
                        "outcome does not match selected attempt",
                    ));
                }
                let attempt = step
                    .attempts
                    .get_mut(report.attempt_index as usize)
                    .ok_or_else(|| {
                        reject(
                            ErrorCode::OperationConflict,
                            "outcome has no committed attempt intent",
                        )
                    })?;
                if attempt.report.is_some() {
                    return Err(reject(
                        ErrorCode::OperationConflict,
                        "attempt already has an outcome",
                    ));
                }
                attempt.report = Some(report.clone());
                let run = active_run(state)?;
                run.active_ms = run.active_ms.saturating_add(report.elapsed_ms);
                encode(&report)
            })
            .await;
        if recorded.is_err() {
            self.session.disconnect().await;
        }
    }
}

fn current_step<'a>(
    state: &'a mut SessionSnapshot,
    agent_id: &str,
    step_id: &str,
) -> Result<&'a mut ModelStep, CoreError> {
    agent_turn(state, agent_id)?
        .steps
        .last_mut()
        .filter(|step| step.step_id == step_id)
        .ok_or_else(|| reject(ErrorCode::StaleRevision, "model step changed"))
}

fn agent_mut<'a>(
    state: &'a mut SessionSnapshot,
    agent_id: &str,
) -> Result<&'a mut AgentState, CoreError> {
    state
        .agents
        .get_mut(agent_id)
        .ok_or_else(|| reject(ErrorCode::UnauthorizedScope, "unknown agent"))
}

fn agent_turn<'a>(
    state: &'a mut SessionSnapshot,
    agent_id: &str,
) -> Result<&'a mut AgentTurn, CoreError> {
    agent_mut(state, agent_id)?
        .turn
        .as_mut()
        .ok_or_else(|| reject(ErrorCode::Busy, "agent has no turn"))
}

fn active_run(state: &mut SessionSnapshot) -> Result<&mut RootRun, CoreError> {
    state
        .run
        .as_mut()
        .ok_or_else(|| reject(ErrorCode::Busy, "no root task is accepted"))
}

fn validate_input(input: &TaskInput, limits: &Limits) -> Result<(), CoreError> {
    if input.text.is_empty() || input.model.is_empty() {
        return Err(reject(
            ErrorCode::NoFeasibleRoute,
            "task text and model are required",
        ));
    }
    if serde_json::to_vec(input).map_err(json_error)?.len() as u64 > limits.input_bytes {
        return Err(reject(
            ErrorCode::LimitExceeded,
            "root input exceeds negotiated bound",
        ));
    }
    if let Some(requested) = &input.limits {
        requested.within(limits)?;
        if serde_json::to_vec(input).map_err(json_error)?.len() as u64 > requested.input_bytes {
            return Err(reject(
                ErrorCode::LimitExceeded,
                "root input exceeds requested bound",
            ));
        }
    }
    parse_effort(input.effort.as_deref())?;
    if !input.required_materials.is_empty() {
        return Err(reject(
            ErrorCode::ArtifactUnavailable,
            "required material has not been supplied to this session",
        ));
    }
    Ok(())
}

fn validate_verification(input: &TaskInput, manifest: &HarnessManifest) -> Result<(), CoreError> {
    if let Some(verification) = &input.verification
        && (!manifest
            .tools
            .iter()
            .any(|tool| tool.name == verification.tool)
            || !verification.arguments.is_object())
    {
        return Err(reject(
            ErrorCode::NoFeasibleRoute,
            "verification requires an allowed tool and object arguments",
        ));
    }
    Ok(())
}

fn parse_effort(effort: Option<&str>) -> Result<Option<ReasoningEffort>, CoreError> {
    effort
        .map(|effort| {
            serde_json::from_value(json!(effort))
                .map_err(|_| reject(ErrorCode::NoFeasibleRoute, "unsupported reasoning effort"))
        })
        .transpose()
}

fn build_prompt(state: &SessionSnapshot, agent_id: &str) -> Result<Prompt, CoreError> {
    let agent = state
        .agents
        .get(agent_id)
        .ok_or_else(|| reject(ErrorCode::UnauthorizedScope, "unknown agent"))?;
    let turn = agent
        .turn
        .as_ref()
        .ok_or_else(|| reject(ErrorCode::Busy, "agent has no turn"))?;
    crate::context::validate_history(&agent.history)
        .map_err(|message| reject(ErrorCode::InvalidToolResult, &message))?;
    if let Some(verification) = &turn.input.verification
        && !state
            .manifest
            .tools
            .iter()
            .any(|tool| tool.name == verification.tool)
    {
        return Err(reject(
            ErrorCode::NoFeasibleRoute,
            "verification tool is not permitted by manifest",
        ));
    }
    Ok(Prompt {
        model: turn.input.model.clone(),
        system: Some(format!(
            "You are the root agent for this task. Use only declared tools. Preserve user constraints and report observed results.\nAcceptance criteria:\n{}",
            turn.input.acceptance_criteria.join("\n")
        )),
        system_provider_metadata: Default::default(),
        messages: agent.history.clone(),
        tools: state
            .manifest
            .tools
            .iter()
            .map(|tool| Tool::Function {
                name: tool.name.clone(),
                description: Some(tool.description.clone()),
                parameters: tool.parameters.clone(),
                strict: None,
                provider_metadata: Default::default(),
            })
            .collect(),
        params: GenerationParams {
            reasoning_effort: parse_effort(turn.input.effort.as_deref())?,
            ..Default::default()
        },
        response_format: None,
        tool_choice: Some(ToolChoice::Auto),
        stream: false,
    })
}

fn id(prefix: &str) -> String {
    format!("{prefix}_{}", uuid::Uuid::new_v4().simple())
}
fn reject(code: ErrorCode, message: &str) -> CoreError {
    CoreError::rejected(code, message)
}
fn json_error(error: serde_json::Error) -> CoreError {
    CoreError::rejected(ErrorCode::CheckpointConflict, error.to_string())
}
fn encode(value: &impl Serialize) -> Result<Value, CoreError> {
    serde_json::to_value(value).map_err(json_error)
}
fn digest(value: &impl Serialize) -> Result<String, CoreError> {
    serde_json::to_vec(value)
        .map(|bytes| sha256(&bytes))
        .map_err(json_error)
}
fn sdk_error(error: CoreError) -> bitrouter_sdk::BitrouterError {
    bitrouter_sdk::BitrouterError::internal(error.to_string())
}
