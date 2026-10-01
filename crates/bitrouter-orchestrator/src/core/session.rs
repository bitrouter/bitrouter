//! Managed session transitions. Provider calls and harness commits run outside
//! the state lock. A separate commit serializer preserves the one-batch rule.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

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
use tokio::sync::{Mutex, Notify};
use tokio_util::sync::CancellationToken;

use super::activity::Activity;
use super::checkpoint::{
    BatchIdentity, Checkpoint, CheckpointAck, CheckpointBatch, CheckpointPayload, CommitGate,
    DurableEvent, DurableHead, sha256,
};
use super::collaboration::{self, Action, Applied, Assignment, Call, Mail, RuntimeWait};
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
    Cancelling,
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
    pub cancellation: Option<String>,
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
    Cancelling,
    Interrupted,
    RecoveryRequired,
    Completed,
    Failed,
    Cancelled,
}

impl AgentStatus {
    pub(crate) fn terminal(self) -> bool {
        matches!(
            self,
            Self::Interrupted | Self::Completed | Self::Failed | Self::Cancelled
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentTurn {
    pub run_id: String,
    pub agent_turn_id: String,
    pub assigned_by: String,
    pub input: TaskInput,
    pub status: AgentStatus,
    pub steps: Vec<ModelStep>,
    pub invocations: Vec<Invocation>,
    pub core_calls: Vec<Call>,
    pub final_answer: Option<String>,
    pub terminal_reason: Option<String>,
    pub notified: bool,
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
    pub queue: VecDeque<Assignment>,
    pub mailbox: Vec<Mail>,
    pub task_scope: Option<String>,
    pub permission_revision: u64,
    pub workspace_revision: Option<String>,
    pub last_scheduled: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSnapshot {
    pub session_id: String,
    pub agent_id: String,
    pub manifest: HarnessManifest,
    pub agents: BTreeMap<String, AgentState>,
    pub run: Option<RootRun>,
    pub operations: BTreeMap<String, OperationReceipt>,
    pub waits: BTreeMap<String, RuntimeWait>,
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
    cancelled_tools: BTreeSet<String>,
    provisional_blocks: BTreeSet<String>,
    disconnected: CancellationToken,
    activity: Activity,
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
    changed: Notify,
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
            queue: VecDeque::new(),
            mailbox: Vec::new(),
            task_scope: None,
            permission_revision: binding.manifest.permission_revision,
            workspace_revision: binding.manifest.workspace_revision.clone(),
            last_scheduled: 0,
        };
        let state = SessionSnapshot {
            session_id: binding.grant.session_id.clone(),
            agent_id: agent_id.clone(),
            manifest: binding.manifest,
            agents: BTreeMap::from([(agent_id, root)]),
            waits: BTreeMap::new(),
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
                    cancelled_tools: BTreeSet::new(),
                    provisional_blocks: BTreeSet::new(),
                    disconnected: CancellationToken::new(),
                    activity: Activity::default(),
                }),
                commits: Mutex::new(()),
                driver: Mutex::new(()),
                inputs: Mutex::new(()),
                app,
                caller,
                harness,
                limits: binding.limits,
                changed: Notify::new(),
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
                assigned_by: agent.agent_id.clone(),
                input: input.clone(),
                status: AgentStatus::Runnable,
                steps: Vec::new(),
                invocations: Vec::new(),
                core_calls: Vec::new(),
                final_answer: None,
                terminal_reason: None,
                notified: false,
            });
            state.run = Some(RootRun {
                run_id: run_id.clone(),
                agent_turn_id: turn_id.clone(),
                input,
                limits,
                status: RunStatus::Running,
                model_attempts: 0,
                active_ms: 0,
                cancellation: None,
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

    /// Drive the shared agent scheduler until terminal, blocked, or awaiting
    /// external tools. One driver owns a session; admitted model steps overlap.
    pub async fn drive(&self) -> Result<SessionSnapshot, CoreError> {
        let _driver = self
            .shared
            .driver
            .try_lock()
            .map_err(|_| reject(ErrorCode::Busy, "session driver is already active"))?;
        let mut jobs = tokio::task::JoinSet::new();
        let mut running = BTreeSet::new();
        let mut first_error = None;
        loop {
            self.advance_runtime_waits().await?;
            let state = self.snapshot().await;
            let run = state
                .run
                .as_ref()
                .ok_or_else(|| reject(ErrorCode::Busy, "no root task is accepted"))?;
            if run.status.terminal() || run.status == RunStatus::RecoveryRequired {
                if jobs.is_empty() {
                    return first_error.map_or(Ok(state), Err);
                }
            } else {
                let mut progressed = false;
                for agent in state.agents.values().filter(|agent| {
                    agent
                        .queue
                        .front()
                        .is_some_and(|work| work.run_id == run.run_id)
                        || agent
                            .turn
                            .as_ref()
                            .is_some_and(|turn| turn.run_id == run.run_id)
                }) {
                    if running.contains(&agent.agent_id) {
                        continue;
                    }
                    let revision = self.head().await.state_revision;
                    match self.advance_agent(&agent.agent_id).await {
                        Ok(changed) => progressed |= changed,
                        Err(error)
                            if error.code == ErrorCode::Busy
                                && self.head().await.state_revision != revision =>
                        {
                            progressed = true;
                        }
                        Err(error) => return Err(error),
                    }
                }
                if progressed {
                    continue;
                }
                let state = self.snapshot().await;
                if jobs.is_empty() {
                    match self.finish_run_if_settled(&state).await {
                        Ok(true) => return first_error.map_or(Ok(self.snapshot().await), Err),
                        Err(error) if error.code == ErrorCode::Busy => continue,
                        Err(error) => return Err(error),
                        Ok(false) => {}
                    }
                }
                let run = state
                    .run
                    .as_ref()
                    .ok_or_else(|| reject(ErrorCode::Busy, "no root task is accepted"))?;
                let mut ready = state
                    .agents
                    .values()
                    .filter(|agent| {
                        !running.contains(&agent.agent_id)
                            && agent.turn.as_ref().is_some_and(|turn| {
                                turn.run_id == run.run_id
                                    && turn.status == AgentStatus::Runnable
                                    && turn.final_answer.is_none()
                            })
                    })
                    .collect::<Vec<_>>();
                ready.sort_by_key(|agent| {
                    (
                        agent.last_scheduled,
                        agent.agent_id != state.agent_id,
                        &agent.agent_id,
                    )
                });
                for agent in ready
                    .into_iter()
                    .take((run.limits.active_models as usize).saturating_sub(running.len()))
                {
                    let agent_id = agent.agent_id.clone();
                    running.insert(agent_id.clone());
                    let session = self.clone();
                    jobs.spawn(async move {
                        let outcome = session.execute_agent_step(&agent_id).await;
                        (agent_id, outcome)
                    });
                }
                if jobs.is_empty() {
                    // No runnable producer exists. Expose the blocked reason
                    // once; do not poll an all-waiting graph or spend model work.
                    let waiting_agents = state
                        .agents
                        .values()
                        .filter_map(|agent| agent.turn.as_ref())
                        .any(|turn| {
                            turn.run_id == run.run_id && turn.status == AgentStatus::WaitingMessage
                        });
                    let pending_tools = state
                        .agents
                        .values()
                        .filter_map(|agent| agent.turn.as_ref())
                        .any(|turn| {
                            turn.run_id == run.run_id
                                && turn.invocations.iter().any(|call| call.result.is_none())
                        });
                    if waiting_agents
                        && !pending_tools
                        && run.terminal_reason.as_deref()
                            != Some("blocked: no runnable agent or external tool producer")
                    {
                        self.transition("run.blocked", |state, _| {
                            active_run(state)?.terminal_reason =
                                Some("blocked: no runnable agent or external tool producer".into());
                            Ok(json!({"reason":"no runnable agent or external tool producer"}))
                        })
                        .await?;
                    }
                    return first_error.map_or(Ok(self.snapshot().await), Err);
                }
            }
            let snapshot = self.snapshot().await;
            let deadline = snapshot
                .agents
                .values()
                .filter(|_| {
                    snapshot.run.as_ref().is_some_and(|run| {
                        matches!(run.status, RunStatus::Running | RunStatus::Waiting)
                    })
                })
                .filter_map(|agent| agent.turn.as_ref())
                .filter(|turn| turn.status == AgentStatus::WaitingMessage)
                .flat_map(|turn| &turn.core_calls)
                .filter(|call| call.result.is_none())
                .filter_map(|call| call.wait.as_ref().map(|wait| wait.deadline_ms))
                .chain(
                    snapshot
                        .waits
                        .values()
                        .filter(|wait| wait.result.is_none())
                        .map(|wait| wait.state.deadline_ms),
                )
                .min();
            let current_ms = now_ms()?;
            let remaining_ms =
                deadline.map_or(60_000, |deadline| deadline.saturating_sub(current_ms));
            let joined = tokio::select! {
                joined=jobs.join_next()=>joined,
                _=self.shared.changed.notified()=>continue,
                _=tokio::time::sleep(std::time::Duration::from_millis(remaining_ms)), if deadline.is_some()=>continue,
            };
            match joined {
                Some(Ok((agent_id, outcome))) => {
                    running.remove(&agent_id);
                    if let Err(error) = outcome {
                        let state = self.snapshot().await;
                        // A rejected preparation leaves the agent runnable.
                        // Retrying unchanged state would repeatedly submit the
                        // same rejected work, or spin on a pending checkpoint.
                        if state
                            .agents
                            .get(&agent_id)
                            .and_then(|agent| agent.turn.as_ref())
                            .is_some_and(|turn| turn.status == AgentStatus::Runnable)
                            || !self.shared.live.lock().await.gate.can_dispatch()
                        {
                            return Err(error);
                        }
                        if agent_id == state.agent_id
                            || state
                                .run
                                .as_ref()
                                .is_some_and(|run| run.status == RunStatus::RecoveryRequired)
                            || !self.shared.live.lock().await.gate.can_dispatch()
                        {
                            first_error.get_or_insert(error);
                        }
                    }
                }
                Some(Err(error)) => {
                    self.disconnect().await;
                    return Err(reject(
                        ErrorCode::RecoveryRequired,
                        error.to_string().as_str(),
                    ));
                }
                None => return first_error.map_or(Ok(self.snapshot().await), Err),
            }
        }
    }

    async fn advance_agent(&self, agent_id: &str) -> Result<bool, CoreError> {
        let state = self.snapshot().await;
        let agent = state
            .agents
            .get(agent_id)
            .ok_or_else(|| reject(ErrorCode::UnauthorizedScope, "unknown agent"))?;
        let turn = agent
            .turn
            .as_ref()
            .ok_or_else(|| reject(ErrorCode::Busy, "agent has no turn"))?;
        if turn.status == AgentStatus::RecoveryRequired {
            return Ok(false);
        }
        if turn.status == AgentStatus::Cancelling {
            return self.cleanup_interruption(agent_id).await;
        }
        if turn.status.terminal() {
            if !turn.notified && agent.parent_id.is_some() {
                let delivery=self.transition_for(Some(agent_id), "agent.result.delivered", |state, _| {
                    let child = agent_mut(state, agent_id)?;
                    let turn = child.turn.as_ref().ok_or_else(|| reject(ErrorCode::Busy, "agent has no turn"))?;
                    let parent=turn.assigned_by.clone();
                    let content = json!({"agent_id":agent_id,"agent_turn_id":turn.agent_turn_id,"status":turn.status,"answer":turn.final_answer,"reason":turn.terminal_reason,"provenance":"agent_conclusion"});
                    collaboration::enqueue_mail(state, agent_id, &parent, "agent_result", content).map_err(|error| if error.code==ErrorCode::LimitExceeded {reject(ErrorCode::Busy,"child result awaits mailbox capacity")} else {error})?;
                    agent_turn(state, agent_id)?.notified = true;
                    Ok(json!({"parent_id":parent}))
                }).await;
                return match delivery {
                    Ok(()) => Ok(true),
                    Err(error) if error.code == ErrorCode::Busy => Ok(false),
                    Err(error) => Err(error),
                };
            }
            if !agent.queue.is_empty() {
                self.transition_for(Some(agent_id), "agent.followup.started", |state, _| {
                    let run_id = active_run(state)?.run_id.clone();
                    let agent = agent_mut(state, agent_id)?;
                    if agent
                        .turn
                        .as_ref()
                        .is_none_or(|turn| !turn.status.terminal() || !turn.notified)
                        || agent.queue.front().is_none_or(|work| work.run_id != run_id)
                    {
                        return Err(reject(ErrorCode::Busy, "follow-up boundary changed"));
                    }
                    let work = agent
                        .queue
                        .pop_front()
                        .ok_or_else(|| reject(ErrorCode::Busy, "follow-up queue is empty"))?;
                    agent
                        .history
                        .push(Message::text(Role::User, &work.input.text));
                    agent.context_revision += 1;
                    agent.turn = Some(collaboration::new_turn(work));
                    Ok(json!({}))
                })
                .await?;
                return Ok(true);
            }
            return Ok(false);
        }
        if turn.steps.last().is_some_and(|step| !step.settled) {
            self.transition_for(Some(agent_id), "run.recovery_required", |state, _| {
                agent_turn(state, agent_id)?.status = AgentStatus::RecoveryRequired;
                active_run(state)?.terminal_reason =
                    Some("previous model driver ended before settlement was applied".into());
                Ok(json!({"reason":"abandoned model step"}))
            })
            .await?;
            return Err(reject(
                ErrorCode::RecoveryRequired,
                "an interrupted model driver must be reconciled before retry",
            ));
        }
        if self.dispatch_collaboration(agent_id).await? {
            return Ok(true);
        }
        let state = self.snapshot().await;
        let agent = state
            .agents
            .get(agent_id)
            .ok_or_else(|| reject(ErrorCode::UnauthorizedScope, "unknown agent"))?;
        let turn = agent
            .turn
            .as_ref()
            .ok_or_else(|| reject(ErrorCode::Busy, "agent has no turn"))?;
        let pending = turn.invocations.iter().any(|call| !call.consumed)
            || turn.core_calls.iter().any(|call| !call.consumed);
        if pending {
            if turn
                .invocations
                .iter()
                .filter(|call| !call.consumed)
                .all(|call| call.result.is_some())
                && turn
                    .core_calls
                    .iter()
                    .filter(|call| !call.consumed)
                    .all(|call| call.result.is_some())
            {
                self.consume_results(agent_id).await?;
                return Ok(true);
            }
            self.dispatch_tools(agent_id).await?;
            return Ok(false);
        }
        if agent.mailbox.iter().any(|mail| !mail.consumed) {
            self.transition_for(Some(agent_id), "mailbox.consumed", |state, _| {
                let agent = agent_mut(state, agent_id)?;
                if agent.turn.as_ref().is_none_or(|turn| turn.status != AgentStatus::Runnable) {
                    return Err(reject(ErrorCode::Busy, "mailbox safe boundary changed"));
                }
                let mail = agent.mailbox.iter_mut().filter(|mail| !mail.consumed).map(|mail| { mail.consumed = true; mail.clone() }).collect::<Vec<_>>();
                agent.history.push(Message::text(Role::User, format!("Messages from agents (unverified conclusions retain provenance):\n{}", serde_json::to_string(&mail).map_err(json_error)?)));
                agent.context_revision += 1;
                if let Some(turn) = &mut agent.turn { turn.final_answer = None; turn.status = AgentStatus::Runnable; }
                Ok(json!({"message_ids":mail.iter().map(|mail| &mail.message_id).collect::<Vec<_>>()}))
            }).await?;
            return Ok(true);
        }
        if turn.final_answer.is_some() {
            if pending_dependencies(&state, agent_id) {
                return Ok(false);
            }
            if turn.input.verification.is_some() && final_verification(turn).is_none() {
                self.schedule_verification(agent_id).await?;
                return Ok(true);
            }
            self.transition_for(Some(agent_id), "agent.completed", |state, _| {
                if pending_dependencies(state, agent_id) {
                    return Err(reject(
                        ErrorCode::Busy,
                        "agent still has dependent work or messages",
                    ));
                }
                let turn = agent_turn(state, agent_id)?;
                if turn.status != AgentStatus::Runnable || turn.final_answer.is_none() {
                    return Err(reject(ErrorCode::Busy, "agent completion boundary changed"));
                }
                let verification = final_verification(turn);
                if turn.input.verification.is_some()
                    && verification.is_none_or(|call| !call.consumed || call.result.is_none())
                {
                    return Err(reject(
                        ErrorCode::Busy,
                        "final answer verification is not settled",
                    ));
                }
                let failed = verification
                    .and_then(|call| call.result.as_ref())
                    .is_some_and(|result| result.status != ToolOutcome::Succeeded);
                turn.status = if failed {
                    AgentStatus::Failed
                } else {
                    AgentStatus::Completed
                };
                turn.terminal_reason = Some(
                    if failed {
                        "harness verification failed"
                    } else {
                        "agent task and effects settled"
                    }
                    .into(),
                );
                Ok(json!({"status":turn.status,"answer":turn.final_answer}))
            })
            .await?;
            return Ok(true);
        }
        Ok(false)
    }

    async fn finish_run_if_settled(&self, state: &SessionSnapshot) -> Result<bool, CoreError> {
        let run = state
            .run
            .as_ref()
            .ok_or_else(|| reject(ErrorCode::Busy, "no active run"))?;
        let Some(root) = state.root_turn() else {
            return Ok(false);
        };
        if !root.status.terminal()
            || state.agents.values().any(|agent| {
                !agent.queue.is_empty()
                    || agent.turn.as_ref().is_some_and(|turn| {
                        turn.run_id == run.run_id
                            && (!turn.status.terminal()
                                || (agent.parent_id.is_some() && !turn.notified)
                                || turn.invocations.iter().any(|call| call.result.is_none()))
                    })
            })
        {
            return Ok(false);
        }
        let failed = root.status != AgentStatus::Completed;
        let cancelled = run.cancellation.is_some();
        self.transition(if cancelled { "run.cancelled" } else if failed { "run.failed" } else { "run.completed" }, |state, _| {
            let current=state.run.as_ref().ok_or_else(|| reject(ErrorCode::Busy,"no run"))?;
            if current.run_id!=run.run_id || current.cancellation.is_some()!=cancelled || state.agents.values().any(|agent| !agent.queue.is_empty() || agent.turn.as_ref().is_some_and(|turn| turn.run_id==current.run_id && (!turn.status.terminal() || (agent.parent_id.is_some()&&!turn.notified) || turn.invocations.iter().any(|call|call.result.is_none())))) {return Err(reject(ErrorCode::Busy,"run terminal boundary changed"));}
            let root = state.root_turn().ok_or_else(|| reject(ErrorCode::Busy, "no root turn"))?;
            if (root.status!=AgentStatus::Completed)!=failed {return Err(reject(ErrorCode::Busy,"root terminal changed"));}
            let answer = root.final_answer.clone();
            let run = active_run(state)?;
            run.status = if cancelled { RunStatus::Cancelled } else if failed { RunStatus::Failed } else { RunStatus::Completed };
            run.final_answer = answer;
            run.terminal_reason = Some(if cancelled { "run cancellation and owned effects settled" } else if failed { "root failed after descendants and effects settled" } else { "root and all descendants and effects settled" }.into());
            Ok(json!({"status":run.status,"answer":run.final_answer,"reason":run.terminal_reason}))
        }).await?;
        Ok(true)
    }

    async fn dispatch_collaboration(&self, agent_id: &str) -> Result<bool, CoreError> {
        let state = self.snapshot().await;
        let Some(turn) = state
            .agents
            .get(agent_id)
            .and_then(|agent| agent.turn.as_ref())
        else {
            return Ok(false);
        };
        let now = now_ms()?;
        let Some(call) =
            turn.core_calls
                .iter()
                .find(|call| {
                    call.result.is_none()
                        && call.wait.as_ref().is_none_or(|wait| {
                            collaboration::wait_ready(&state, agent_id, wait, now)
                        })
                })
                .cloned()
        else {
            return Ok(false);
        };
        let interrupt = matches!(call.action, Action::Interrupt { .. });
        if interrupt {
            self.block_dispatch(&call.invocation_id).await;
        }
        let committed = self.transition_for(Some(agent_id), "collaboration.applied", |state, _| {
            let limit = active_run(state)?.limits.input_bytes;
            let applied = if let Some(wait) = &call.wait {
                Ok(Applied::Complete(collaboration::wait_result(state,wait,now)))
            } else if serde_json::to_vec(&call.action).map_err(json_error)?.len() as u64 > limit {
                Err(reject(ErrorCode::LimitExceeded, "collaboration input exceeds bound"))
            } else {
                let mut candidate = state.clone();
                match collaboration::apply(&mut candidate, agent_id, &call.action, now) {
                    Ok(applied) => { *state = candidate; Ok(applied) }
                    Err(error) => Err(error),
                }
            };
            let turn = agent_turn(state, agent_id)?;
            let retained = turn.core_calls.iter_mut().find(|item| item.invocation_id == call.invocation_id).ok_or_else(|| reject(ErrorCode::OperationConflict, "collaboration intent changed"))?;
            match applied {
                Ok(Applied::Complete(value)) => retained.result = Some(json!({"ok":true,"value":value})),
                Ok(Applied::Waiting(wait)) => retained.wait = Some(wait),
                Err(error) => retained.result = Some(json!({"ok":false,"error":error})),
            }
            let result = retained.result.clone();
            if turn.status != AgentStatus::Cancelling {
                turn.status = if turn.core_calls.iter().any(|call| call.wait.is_some() && call.result.is_none()) { AgentStatus::WaitingMessage } else { AgentStatus::WaitingTool };
            }
            Ok(json!({"source":"model","invocation_id":call.invocation_id,"operation":call.action.name(),"result":result}))
        }).await;
        if interrupt {
            self.resolve_dispatch_block(&call.invocation_id, &committed)
                .await;
        }
        committed?;
        Ok(true)
    }

    /// Explicit runtime intents use the same action dispatcher and durable
    /// receipts as model calls. The host authorizes the session and actor.
    pub async fn collaborate(
        &self,
        operation_id: &str,
        expected_revision: u64,
        actor_id: &str,
        action: Action,
    ) -> Result<OperationReceipt, CoreError> {
        validate_id(operation_id)?;
        let fingerprint = digest(
            &json!({"source":"runtime","actor_id":actor_id,"expected_state_revision":expected_revision,"action":action}),
        )?;
        if let Some(receipt) = self.replay(operation_id, &fingerprint).await? {
            return Ok(receipt);
        }
        let interrupt = matches!(action, Action::Interrupt { .. });
        if interrupt {
            self.block_dispatch(operation_id).await;
        }
        let _input = self.shared.inputs.lock().await;
        match self.replay(operation_id, &fingerprint).await {
            Ok(Some(receipt)) => {
                if interrupt {
                    self.resolve_dispatch_block(operation_id, &Ok(())).await;
                }
                return Ok(receipt);
            }
            Err(error) => {
                if interrupt {
                    self.resolve_dispatch_block(operation_id, &Err(error.clone()))
                        .await;
                }
                return Err(error);
            }
            Ok(None) => {}
        }
        let result = self
            .transition_for(Some(actor_id), "collaboration.runtime", |state, head| {
                if expected_revision != head.state_revision {
                    return Err(reject(
                        ErrorCode::StaleRevision,
                        "runtime action revision is stale",
                    ));
                }
                if serde_json::to_vec(&action).map_err(json_error)?.len() as u64
                    > active_run(state)?.limits.input_bytes
                {
                    return Err(reject(
                        ErrorCode::LimitExceeded,
                        "runtime action exceeds input bound",
                    ));
                }
                let applied = collaboration::apply(state, actor_id, &action, now_ms()?)?;
                let (value, disposition) = match applied {
                    Applied::Complete(value) => (value, OperationDisposition::Applied),
                    Applied::Waiting(wait) => {
                        state.waits.insert(
                            operation_id.into(),
                            RuntimeWait {
                                actor_id: actor_id.into(),
                                state: wait,
                                result: None,
                            },
                        );
                        (
                            json!({"wait_id":operation_id}),
                            OperationDisposition::Accepted,
                        )
                    }
                };
                let mut assigned_ids = BTreeMap::new();
                if let Some(object) = value.as_object() {
                    for (key, value) in object {
                        if key.ends_with("_id")
                            && let Some(value) = value.as_str()
                        {
                            assigned_ids.insert(key.clone(), value.to_owned());
                        }
                    }
                }
                state.operations.insert(
                    operation_id.into(),
                    OperationReceipt {
                        operation_id: operation_id.into(),
                        request_sha256: fingerprint.clone(),
                        disposition,
                        assigned_ids,
                        state_revision: head.state_revision + 1,
                        error: None,
                    },
                );
                Ok(json!({"source":"runtime","action":action,"result":value}))
            })
            .await;
        if interrupt {
            self.resolve_dispatch_block(operation_id, &result).await;
        }
        result?;
        self.operation(operation_id).await.ok_or_else(|| {
            reject(
                ErrorCode::CheckpointUnavailable,
                "runtime action receipt missing",
            )
        })
    }

    /// Runtime waits observe committed state without taking an agent model
    /// slot or fabricating model tool calls. Their completion is durable and
    /// exposed in the snapshot under the accepted operation identity.
    async fn advance_runtime_waits(&self) -> Result<(), CoreError> {
        let snapshot = self.snapshot().await;
        let now = now_ms()?;
        let ready = snapshot
            .waits
            .iter()
            .filter(|(_, wait)| {
                wait.result.is_none()
                    && collaboration::wait_ready(&snapshot, &wait.actor_id, &wait.state, now)
            })
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
        if ready.is_empty() {
            return Ok(());
        }
        self.transition("collaboration.wait.completed", |state, _| {
            for id in &ready {
                let result = state
                    .waits
                    .get(id)
                    .filter(|wait| wait.result.is_none())
                    .map(|wait| collaboration::wait_result(state, &wait.state, now));
                if let Some(result) = result
                    && let Some(wait) = state.waits.get_mut(id)
                {
                    wait.result = Some(result);
                }
            }
            Ok(json!({"source":"runtime","operation_ids":ready}))
        })
        .await
    }

    pub async fn cancel_run(
        &self,
        operation_id: &str,
        expected_revision: u64,
        run_id: &str,
    ) -> Result<OperationReceipt, CoreError> {
        validate_id(operation_id)?;
        let fingerprint = digest(
            &json!({"type":"run.cancel","expected_state_revision":expected_revision,"run_id":run_id}),
        )?;
        if let Some(receipt) = self.replay(operation_id, &fingerprint).await? {
            return Ok(receipt);
        }
        self.block_dispatch(operation_id).await;
        let _input = self.shared.inputs.lock().await;
        let result = async {
            if let Some(receipt) = self.replay(operation_id, &fingerprint).await? {
                return Ok(receipt);
            }
            self.transition("run.cancelling", |state, head| {
                if head.state_revision != expected_revision {
                    return Err(reject(
                        ErrorCode::StaleRevision,
                        "run cancellation revision is stale",
                    ));
                }
                let run = active_run(state)?;
                if run.run_id != run_id || run.status.terminal() {
                    return Err(reject(
                        ErrorCode::Busy,
                        "run no longer accepts cancellation",
                    ));
                }
                run.cancellation = Some(operation_id.into());
                for agent in state.agents.values_mut() {
                    agent.queue.clear();
                    if let Some(turn) = &mut agent.turn
                        && turn.run_id == run_id
                        && !turn.status.terminal()
                    {
                        turn.status = AgentStatus::Cancelling;
                    }
                }
                state.operations.insert(
                    operation_id.into(),
                    OperationReceipt {
                        operation_id: operation_id.into(),
                        request_sha256: fingerprint.clone(),
                        disposition: OperationDisposition::Accepted,
                        assigned_ids: BTreeMap::from([("run_id".into(), run_id.into())]),
                        state_revision: head.state_revision + 1,
                        error: None,
                    },
                );
                Ok(json!({"run_id":run_id,"operation_id":operation_id}))
            })
            .await?;
            self.operation(operation_id)
                .await
                .ok_or_else(|| reject(ErrorCode::CheckpointUnavailable, "cancel receipt missing"))
        }
        .await;
        self.resolve_dispatch_block(
            operation_id,
            &result.as_ref().map(|_| ()).map_err(Clone::clone),
        )
        .await;
        result
    }

    async fn cleanup_interruption(&self, agent_id: &str) -> Result<bool, CoreError> {
        let state = self.snapshot().await;
        let turn = state
            .agents
            .get(agent_id)
            .and_then(|agent| agent.turn.as_ref())
            .ok_or_else(|| reject(ErrorCode::Busy, "agent has no turn"))?;
        if turn.steps.last().is_some_and(|step| !step.settled) {
            self.transition_for(Some(agent_id), "run.recovery_required", |state, _| {
                agent_turn(state, agent_id)?.status = AgentStatus::RecoveryRequired;
                Ok(json!({"reason":"interrupted model driver is no longer tracked"}))
            })
            .await?;
            return Err(reject(
                ErrorCode::RecoveryRequired,
                "interrupted model driver requires reconciliation",
            ));
        }
        let unsent = {
            let live = self.shared.live.lock().await;
            turn.invocations
                .iter()
                .filter(|call| {
                    call.result.is_none() && !live.sent_tools.contains(&call.dispatch.invocation_id)
                })
                .map(|call| call.dispatch.invocation_id.clone())
                .collect::<BTreeSet<_>>()
        };
        if !unsent.is_empty() {
            self.transition_for(
                Some(agent_id),
                "tool.cancelled_before_dispatch",
                |state, _| {
                    for call in &mut agent_turn(state, agent_id)?.invocations {
                        if unsent.contains(&call.dispatch.invocation_id) {
                            call.result = Some(ToolResult {
                                invocation_id: call.dispatch.invocation_id.clone(),
                                attempt_id: call.dispatch.attempt_id.clone(),
                                status: ToolOutcome::NotExecuted,
                                output: "cancelled before dispatch".into(),
                                evidence: Vec::new(),
                                workspace_revision: None,
                            });
                        }
                    }
                    Ok(json!({"invocation_ids":unsent}))
                },
            )
            .await?;
            return Ok(true);
        }
        for call in turn.invocations.iter().filter(|call| call.result.is_none()) {
            let send = {
                let _admission = self.shared.commits.lock().await;
                let mut live = self.shared.live.lock().await;
                if live
                    .state
                    .agents
                    .get(agent_id)
                    .and_then(|agent| agent.turn.as_ref())
                    .is_none_or(|turn| {
                        turn.status != AgentStatus::Cancelling
                            || turn.invocations.iter().any(|current| {
                                current.dispatch.invocation_id == call.dispatch.invocation_id
                                    && current.result.is_some()
                            })
                    })
                {
                    continue;
                }
                if !live.gate.can_dispatch() {
                    return Err(reject(
                        ErrorCode::CheckpointUnavailable,
                        "cancellation delivery awaits durable state",
                    ));
                }
                live.cancelled_tools
                    .insert(call.dispatch.invocation_id.clone())
            };
            if send {
                let session = self.clone();
                let message = ServerMessage::ToolCancel {
                    invocation_id: call.dispatch.invocation_id.clone(),
                    attempt_id: call.dispatch.attempt_id.clone(),
                    execution_epoch: call.dispatch.execution_epoch,
                };
                // Cleanup delivery has the same cancellation safety as execute.
                tokio::spawn(async move {
                    if session.shared.harness.send(message).await.is_err() {
                        session.disconnect().await;
                    }
                });
            }
        }
        if turn.invocations.iter().any(|call| call.result.is_none()) {
            return Ok(false);
        }
        self.transition_for(Some(agent_id), "agent.interruption.results", |state, _| {
            let turn = agent_turn(state, agent_id)?;
            for call in &mut turn.core_calls {
                if call.result.is_none() {
                    call.result = Some(json!({"ok":false,"reason":"agent interrupted"}));
                }
            }
            Ok(json!({}))
        })
        .await?;
        // Preserve paired history before this context can receive follow-up.
        if turn.invocations.iter().any(|call| !call.consumed)
            || turn.core_calls.iter().any(|call| !call.consumed)
        {
            self.consume_results(agent_id).await?;
        }
        self.transition_for(Some(agent_id), "agent.interrupted", |state, _| {
            let turn = agent_turn(state, agent_id)?;
            turn.status = AgentStatus::Interrupted;
            turn.terminal_reason = Some("interruption and outstanding effects settled".into());
            Ok(json!({}))
        })
        .await?;
        Ok(true)
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
        self.transition_for(Some(agent_id), "model.step.preparing", |state, head| {
            let agent = agent_mut(state, agent_id)?;
            agent.last_scheduled = head.event_seq + 1;
            let revision = agent.context_revision;
            let turn = agent
                .turn
                .as_mut()
                .ok_or_else(|| reject(ErrorCode::Busy, "agent has no turn"))?;
            if turn.status != AgentStatus::Runnable || turn.final_answer.is_some() {
                return Err(reject(ErrorCode::Busy, "agent is no longer runnable"));
            }
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
                if self
                    .snapshot()
                    .await
                    .agents
                    .get(agent_id)
                    .and_then(|agent| agent.turn.as_ref())
                    .is_some_and(|turn| turn.status == AgentStatus::Cancelling)
                {
                    self.transition_for(Some(agent_id), "model.output.discarded", |state, _| {
                        current_step(state, agent_id, &step_id)?.settled = true;
                        Ok(json!({"reason":"agent interrupted","step_id":step_id}))
                    })
                    .await?;
                    return Ok(());
                }
                if let Err(error) = self
                    .apply_output(agent_id, &step_id, &response.request_id, &response.result)
                    .await
                {
                    if self.can_progress().await {
                        self.fail(agent_id, &error.message).await?;
                    }
                    return Err(error);
                }
            }
            Err(error) => {
                if self
                    .snapshot()
                    .await
                    .agents
                    .get(agent_id)
                    .and_then(|agent| agent.turn.as_ref())
                    .is_some_and(|turn| turn.status == AgentStatus::Cancelling)
                {
                    self.transition_for(Some(agent_id), "model.output.discarded", |state, _| {
                        current_step(state, agent_id, &step_id)?.settled = true;
                        Ok(json!({"reason":"interrupted attempt ended","step_id":step_id}))
                    })
                    .await?;
                    return Ok(());
                }
                if self.can_progress().await {
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
        let target_agent = self
            .snapshot()
            .await
            .agents
            .values()
            .find(|agent| {
                agent.turn.as_ref().is_some_and(|turn| {
                    turn.invocations
                        .iter()
                        .any(|call| call.dispatch.invocation_id == result.invocation_id)
                })
            })
            .map(|agent| agent.agent_id.clone())
            .ok_or_else(|| reject(ErrorCode::InvalidToolResult, "unknown tool invocation"))?;
        self.transition_for(Some(&target_agent), "tool.result", |state, head| {
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
        self.transition_for(Some(agent_id), "model.output.applied", |state, head| {
            let manifest = state.manifest.clone();
            let limit = active_run(state)?.limits.outstanding_tools;
            let outstanding = state.agents.values().filter_map(|agent| agent.turn.as_ref()).flat_map(|turn| &turn.invocations).filter(|call| call.result.is_none()).count();
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
            if turn.status==AgentStatus::Cancelling {
                step.settled=true;
                return Ok(json!({"step_id":step_id,"discarded":"agent interrupted"}));
            }
            if turn.status!=AgentStatus::ModelRunning {return Err(reject(ErrorCode::RecoveryRequired,"agent no longer admits output"));}
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
            let mut core_calls = Vec::new();
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
                    let core_owned = super::protocol::COLLABORATION_TOOLS.contains(&name.as_str());
                    if *provider_executed || !planned || (!core_owned && !manifest.tools.iter().any(|tool| tool.name == *name)) {
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
                    if core_owned {
                        core_calls.push(Call {
                            invocation_id: id("collaboration"), public_call_id: id("call"), provider_call_id: call_id.clone(), step_id: step_id.to_owned(),
                            action: Action::parse(name, arguments)?, wait: None, result: None, consumed: false,
                        });
                        continue;
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
                (calls.is_empty() && core_calls.is_empty() && matches!(plan.prompt.tool_choice, Some(ToolChoice::Required | ToolChoice::Tool { .. })))
                    || (calls.len() + core_calls.len() > 1 && plan.prompt.params.parallel_tool_calls == Some(false))
            }) {
                return Err(reject(ErrorCode::InvalidToolResult, "model output violates its frozen tool choice"));
            }
            if calls.len() + outstanding > limit as usize {
                return Err(reject(
                    ErrorCode::LimitExceeded,
                    "model output exceeds outstanding tool limit",
                ));
            }
            step.settled = true;
            if calls.is_empty() && core_calls.is_empty() {
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
            }

            turn.status = if calls.is_empty() && core_calls.is_empty() {
                AgentStatus::Runnable
            } else {
                AgentStatus::WaitingTool
            };
            turn.invocations.extend(calls);
            turn.core_calls.extend(core_calls);
            agent.history.push(message);
            agent.context_revision += 1;
            Ok(json!({"step_id":step_id,"request_id":request_id}))
        })
        .await
    }

    async fn schedule_verification(&self, agent_id: &str) -> Result<(), CoreError> {
        self.transition_for(Some(agent_id), "tool.verification.intent", |state, head| {
            if pending_dependencies(state, agent_id) {
                return Err(reject(
                    ErrorCode::Busy,
                    "verification awaits dependent work or messages",
                ));
            }
            let manifest = state.manifest.clone();
            let limit = active_run(state)?.limits.outstanding_tools as usize;
            let outstanding = state
                .agents
                .values()
                .filter_map(|agent| agent.turn.as_ref())
                .flat_map(|turn| &turn.invocations)
                .filter(|call| call.result.is_none())
                .count();
            if outstanding >= limit {
                return Err(reject(
                    ErrorCode::LimitExceeded,
                    "verification awaits tool capacity",
                ));
            }
            let agent = agent_mut(state, agent_id)?;
            let turn = agent
                .turn
                .as_mut()
                .ok_or_else(|| reject(ErrorCode::Busy, "agent has no turn"))?;
            if turn.status != AgentStatus::Runnable
                || turn.final_answer.is_none()
                || final_verification(turn).is_some()
            {
                return Err(reject(ErrorCode::Busy, "verification boundary changed"));
            }
            let verification = turn
                .input
                .verification
                .as_ref()
                .ok_or_else(|| reject(ErrorCode::Busy, "no verification configured"))?;
            let step = turn
                .steps
                .last()
                .ok_or_else(|| reject(ErrorCode::Busy, "no final model step"))?;
            turn.invocations.push(Invocation {
                dispatch: ToolExecute {
                    invocation_id: id("invocation"),
                    attempt_id: id("tool_attempt"),
                    run_id: turn.run_id.clone(),
                    agent_id: agent_id.into(),
                    agent_turn_id: turn.agent_turn_id.clone(),
                    step_id: step.step_id.clone(),
                    context_revision: agent.context_revision,
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
            turn.status = AgentStatus::WaitingTool;
            Ok(json!({}))
        })
        .await
    }

    async fn consume_results(&self, agent_id: &str) -> Result<(), CoreError> {
        self.transition_for(Some(agent_id), "tool.results.consumed", |state, _| {
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
            for call in turn.core_calls.iter_mut().filter(|call| !call.consumed) {
                let result = call.result.as_ref().ok_or_else(|| {
                    reject(ErrorCode::Busy, "collaboration batch is not complete")
                })?;
                messages.push(Message {
                    role: Role::Tool,
                    content: vec![Content::ToolResult {
                        call_id: call.provider_call_id.clone(),
                        tool_name: Some(call.action.name().into()),
                        dynamic: false,
                        output: ToolResultOutput::Text {
                            value: serde_json::to_string(result).map_err(json_error)?,
                        },
                        provider_metadata: Default::default(),
                    }],
                });
                call.consumed = true;
            }
            if turn.status != AgentStatus::Cancelling {
                turn.status = AgentStatus::Runnable;
            }
            if let Some(success) = verified {
                turn.terminal_reason = Some(format!("harness verification succeeded: {success}"));
            }
            agent.history.extend(messages);
            agent.context_revision += 1;
            Ok(json!({}))
        })
        .await
    }

    async fn dispatch_tools(&self, agent_id: &str) -> Result<(), CoreError> {
        loop {
            let (command, disconnected) = {
                let _admission = self.shared.commits.lock().await;
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
                    if live
                        .state
                        .run
                        .as_ref()
                        .is_some_and(|run| run.status == RunStatus::RecoveryRequired)
                    {
                        return Err(reject(
                            ErrorCode::RecoveryRequired,
                            "run has an unresolved execution effect",
                        ));
                    }
                    return Ok(());
                }
                let command = live
                    .state
                    .agents
                    .get(agent_id)
                    .and_then(|agent| agent.turn.as_ref())
                    .filter(|turn| {
                        matches!(
                            turn.status,
                            AgentStatus::Runnable
                                | AgentStatus::WaitingTool
                                | AgentStatus::WaitingMessage
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
        self.transition_for(Some(agent_id), "agent.failed", |state, _| {
            let turn = agent_turn(state, agent_id)?;
            if turn.status == AgentStatus::Cancelling {
                if let Some(step) = turn.steps.last_mut()
                    && step.attempts.iter().all(|attempt| attempt.report.is_some())
                {
                    step.settled = true;
                }
                return Ok(json!({"reason":reason,"interrupted":true}));
            }
            turn.status = AgentStatus::Failed;
            turn.terminal_reason = Some(reason.to_owned());
            if agent_id == state.agent_id {
                for (target, agent) in &mut state.agents {
                    if target != agent_id
                        && let Some(turn) = &mut agent.turn
                        && !turn.status.terminal()
                    {
                        turn.status = AgentStatus::Cancelling;
                    }
                    if target != agent_id {
                        agent.queue.clear();
                    }
                }
            }
            Ok(json!({"reason":reason}))
        })
        .await
    }

    async fn ensure_dispatch(&self, agent_id: &str, activity_id: String) -> Result<(), CoreError> {
        let _admission = self.shared.commits.lock().await;
        let mut live = self.shared.live.lock().await;
        if !live.gate.can_dispatch()
            || live
                .state
                .run
                .as_ref()
                .is_none_or(|run| !matches!(run.status, RunStatus::Running | RunStatus::Waiting))
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
        if live.state.run.as_ref().is_some_and(|run| {
            live.activity.elapsed_ms() >= run.limits.active_seconds.saturating_mul(1000)
        }) {
            return Err(reject(
                ErrorCode::LimitExceeded,
                "active wall-time budget exhausted",
            ));
        }
        live.activity.start(activity_id);
        Ok(())
    }

    async fn can_progress(&self) -> bool {
        let _commit = self.shared.commits.lock().await;
        self.shared.live.lock().await.gate.can_dispatch()
    }

    async fn block_dispatch(&self, operation_id: &str) {
        let mut live = self.shared.live.lock().await;
        live.provisional_blocks.insert(operation_id.into());
        live.gate.block_dispatch();
    }

    async fn resolve_dispatch_block(&self, operation_id: &str, result: &Result<(), CoreError>) {
        if result.is_ok()
            || result
                .as_ref()
                .is_err_and(|error| error.commit_status == CommitStatus::NotCommitted)
        {
            let mut live = self.shared.live.lock().await;
            live.provisional_blocks.remove(operation_id);
            if live.provisional_blocks.is_empty() {
                live.gate.clear_dispatch_block();
            }
        }
    }

    async fn transition<F>(&self, kind: &str, change: F) -> Result<(), CoreError>
    where
        F: FnOnce(&mut SessionSnapshot, &DurableHead) -> Result<Value, CoreError> + Send,
    {
        self.transition_for(None, kind, change).await
    }

    async fn transition_for<F>(
        &self,
        agent_id: Option<&str>,
        kind: &str,
        change: F,
    ) -> Result<(), CoreError>
    where
        F: FnOnce(&mut SessionSnapshot, &DurableHead) -> Result<Value, CoreError> + Send,
    {
        let _commit = self.shared.commits.lock().await;
        let (batch, disconnected) = {
            let mut live = self.shared.live.lock().await;
            let mut next = live.state.clone();
            let head = live.gate.head().clone();
            let payload = change(&mut next, &head)?;
            if !matches!(kind, "run.completed" | "run.failed" | "run.cancelled") {
                refresh_run(&mut next);
            }
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
                    agent_id: Some(agent_id.unwrap_or(&next.agent_id).to_owned()),
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
                    if kind == "input.accepted" {
                        live.activity = Activity::default();
                    }
                    self.shared.changed.notify_one();
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
            .transition_for(Some(&self.agent_id), "model.plan", |state, _| {
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
        self.session.transition_for(Some(&self.agent_id), "model.attempt.intent", |state, _| {
            let run = active_run(state)?;
            if !matches!(run.status, RunStatus::Running | RunStatus::Waiting) || run.model_attempts >= run.limits.model_attempts || run.active_ms >= run.limits.active_seconds.saturating_mul(1000) { return Err(reject(ErrorCode::LimitExceeded, "model attempt is no longer admitted")); }
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
            .ensure_dispatch(&self.agent_id, format!("{request_id}/{attempt_index}"))
            .await
            .map_err(sdk_error)
    }

    async fn after_attempt(&self, report: NativeAttemptReport) {
        let active_ms = self
            .session
            .shared
            .live
            .lock()
            .await
            .activity
            .finish(&format!("{}/{}", report.request_id, report.attempt_index));
        let recorded = self
            .session
            .transition_for(Some(&self.agent_id), "model.attempt.outcome", |state, _| {
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
                run.active_ms = run.active_ms.max(active_ms);
                encode(&report)
            })
            .await;
        if recorded.is_err() {
            self.session.disconnect().await;
        }
    }
}

fn final_verification(turn: &AgentTurn) -> Option<&Invocation> {
    let step = turn.steps.last()?;
    turn.invocations
        .iter()
        .find(|call| call.dispatch.verification && call.dispatch.step_id == step.step_id)
}

fn pending_dependencies(state: &SessionSnapshot, agent_id: &str) -> bool {
    let descendants = collaboration::subtree(state, agent_id);
    state.agents.values().any(|agent| {
        if agent.agent_id == agent_id {
            return agent.mailbox.iter().any(|mail| !mail.consumed);
        }
        let descendant = descendants.contains(&agent.agent_id);
        agent
            .queue
            .iter()
            .any(|work| descendant || work.sender_id == agent_id)
            || agent.turn.as_ref().is_some_and(|turn| {
                (descendant || turn.assigned_by == agent_id)
                    && (!turn.status.terminal() || !turn.notified)
            })
    })
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

pub(super) fn validate_input(input: &TaskInput, limits: &Limits) -> Result<(), CoreError> {
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
            "You are agent {} for this task. Use only declared tools. Preserve user constraints and report observed results.\nAcceptance criteria:\n{}",
            agent_id,
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
            .chain(collaboration::declarations())
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

fn now_ms() -> Result<u64, CoreError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(u64::MAX as u128) as u64)
        .map_err(|_| reject(ErrorCode::RecoveryRequired, "clock precedes Unix epoch"))
}

fn refresh_run(state: &mut SessionSnapshot) {
    let Some(run) = &mut state.run else {
        return;
    };
    if run.status.terminal() {
        return;
    }
    let turns = state
        .agents
        .values()
        .filter_map(|agent| agent.turn.as_ref())
        .filter(|turn| turn.run_id == run.run_id)
        .collect::<Vec<_>>();
    run.status = if turns
        .iter()
        .any(|turn| turn.status == AgentStatus::RecoveryRequired)
    {
        RunStatus::RecoveryRequired
    } else if run.cancellation.is_some() {
        RunStatus::Cancelling
    } else if turns.iter().all(|turn| turn.status.terminal())
        || turns.iter().any(|turn| {
            turn.status == AgentStatus::ModelRunning
                || (turn.status == AgentStatus::Runnable && turn.final_answer.is_none())
        })
    {
        RunStatus::Running
    } else {
        RunStatus::Waiting
    };
}
