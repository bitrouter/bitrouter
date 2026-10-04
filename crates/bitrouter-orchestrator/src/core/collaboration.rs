//! Core-owned collaboration intents. Every application happens inside the same
//! acknowledged session transition as model-originated or runtime-originated
//! actions. These functions never call a provider or execute workspace tools.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use bitrouter_sdk::language_model::types::{Message, Role, Tool};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::allocation::{self, ContextKind, ContextSource, Intent};
use super::protocol::{CoreError, ErrorCode, TaskInput};
use super::session::{AgentState, AgentStatus, AgentTurn, RunStatus, SessionSnapshot};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Work {
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    #[serde(default)]
    pub acceptance_criteria: Vec<String>,
    #[serde(default)]
    pub required_materials: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_scope: Option<String>,
    #[serde(default)]
    pub fresh_context: bool,
    #[serde(default)]
    pub independent_review: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "name", content = "arguments", deny_unknown_fields)]
pub enum Action {
    #[serde(rename = "spawn_agent")]
    Spawn { task: Work },
    #[serde(rename = "delegate_task")]
    Delegate {
        task: Work,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent_id: Option<String>,
    },
    #[serde(rename = "send_message")]
    Message { agent_id: String, text: String },
    #[serde(rename = "followup_task")]
    Followup { agent_id: String, task: Work },
    #[serde(rename = "wait_agent")]
    Wait {
        agent_ids: Vec<String>,
        #[serde(default = "default_wait")]
        timeout_ms: u64,
    },
    #[serde(rename = "interrupt_agent")]
    Interrupt { agent_id: String },
    #[serde(rename = "list_agents")]
    List {},
}

fn default_wait() -> u64 {
    30_000
}

impl Action {
    pub fn parse(name: &str, arguments: Value) -> Result<Self, CoreError> {
        serde_json::from_value(json!({"name":name,"arguments":arguments}))
            .map_err(|error| reject(ErrorCode::InvalidToolResult, error.to_string()))
    }

    pub fn name(&self) -> &'static str {
        match self {
            Self::Spawn { .. } => "spawn_agent",
            Self::Delegate { .. } => "delegate_task",
            Self::Message { .. } => "send_message",
            Self::Followup { .. } => "followup_task",
            Self::Wait { .. } => "wait_agent",
            Self::Interrupt { .. } => "interrupt_agent",
            Self::List {} => "list_agents",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Mail {
    pub message_id: String,
    pub sender_id: String,
    pub sender_turn_id: String,
    pub kind: String,
    pub content: Value,
    pub context_sources: Vec<ContextSource>,
    pub consumed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Assignment {
    pub assignment_id: String,
    pub run_id: String,
    pub sender_id: String,
    pub input: TaskInput,
    pub required_instructions: Vec<String>,
    pub allocation_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WaitState {
    pub targets: BTreeMap<String, WaitTarget>,
    pub deadline_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WaitTarget {
    pub agent_turn_id: String,
    /// None means the accepted assignment is still queued.
    pub status: Option<AgentStatus>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeWait {
    pub actor_id: String,
    pub state: WaitState,
    pub result: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Call {
    pub invocation_id: String,
    pub public_call_id: String,
    pub provider_call_id: String,
    pub step_id: String,
    pub action: Action,
    /// Frozen normal-wait delivery reservation; absent on legacy intents.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wait_output_version: Option<u32>,
    pub wait: Option<WaitState>,
    pub result: Option<Value>,
    pub consumed: bool,
}

pub enum Applied {
    Complete(Value),
    Waiting(WaitState),
    Rejected {
        allocation_id: String,
        error: CoreError,
    },
}

pub fn declarations() -> Vec<Tool> {
    let work = json!({"type":"object","properties":{
        "text":{"type":"string"},"model":{"type":"string"},"effort":{"type":"string"},
        "acceptance_criteria":{"type":"array","items":{"type":"string"}},
        "required_materials":{"type":"array","items":{"type":"string"}},
        "task_scope":{"type":"string"},"fresh_context":{"type":"boolean"},"independent_review":{"type":"boolean"}
    },"required":["text"],"additionalProperties":false});
    [
        ("spawn_agent", "Create a new child with a concrete bounded task. Returns its stable agent ID.", json!({"task":work}), vec!["task"]),
        ("delegate_task", "Assign a bounded task to an eligible idle worker or a new child. Exact targets and fresh-context constraints are binding.", json!({"task":work,"agent_id":{"type":"string"}}), vec!["task"]),
        ("send_message", "Enqueue a message without starting a new turn.", json!({"agent_id":{"type":"string"},"text":{"type":"string"}}), vec!["agent_id","text"]),
        ("followup_task", "Assign work to an existing non-root agent, queued FIFO if busy.", json!({"agent_id":{"type":"string"},"task":work}), vec!["agent_id","task"]),
        ("wait_agent", "Wait for target state or mailbox changes. Releases the model slot; cyclic waits are rejected.", json!({"agent_ids":{"type":"array","items":{"type":"string"}},"timeout_ms":{"type":"integer","minimum":0,"maximum":600000}}), vec!["agent_ids"]),
        ("interrupt_agent", "Interrupt a non-root agent subtree, preserving contexts and awaiting running effects.", json!({"agent_id":{"type":"string"}}), vec!["agent_id"]),
        ("list_agents", "List agents and their committed status in this session.", json!({}), vec![]),
    ].into_iter().map(|(name, description, properties, required)| Tool::Function {
        name: name.into(), description: Some(description.into()),
        parameters: json!({"type":"object","properties":properties,"required":required,"additionalProperties":false}),
        strict: None, provider_metadata: Default::default(),
    }).collect()
}

pub fn apply(
    state: &mut SessionSnapshot,
    actor_id: &str,
    action: &Action,
    now_ms: u64,
    state_revision: u64,
) -> Result<Applied, CoreError> {
    let actor = state
        .agents
        .get(actor_id)
        .ok_or_else(|| reject(ErrorCode::UnauthorizedScope, "unknown actor"))?;
    let actor_turn = actor
        .turn
        .as_ref()
        .ok_or_else(|| reject(ErrorCode::Busy, "actor has no turn"))?;
    let run = state
        .run
        .as_ref()
        .ok_or_else(|| reject(ErrorCode::Busy, "no active run"))?;
    if !matches!(run.status, RunStatus::Running | RunStatus::Waiting)
        || actor_turn.run_id != run.run_id
        || actor_turn.status.terminal()
        || actor_turn.status == AgentStatus::Cancelling
    {
        return Err(reject(
            ErrorCode::Busy,
            "actor turn no longer accepts collaboration",
        ));
    }
    let result = match action {
        Action::Spawn { task } => {
            return allocate(state, actor_id, task, Intent::Spawn, state_revision);
        }
        Action::Delegate { task, agent_id } => {
            return allocate(
                state,
                actor_id,
                task,
                Intent::Delegate(agent_id.as_deref()),
                state_revision,
            );
        }
        Action::Message { agent_id, text } => {
            if text.is_empty() {
                return Err(reject(ErrorCode::InvalidToolResult, "message is empty"));
            }
            let message_id =
                enqueue_mail(state, actor_id, agent_id, "message", json!({"text":text}))?;
            json!({"message_id":message_id,"agent_id":agent_id,"started_turn":false})
        }
        Action::Followup { agent_id, task } => {
            if run.input.max_concurrent_subagents == Some(0) {
                return Err(reject(
                    ErrorCode::LimitExceeded,
                    "descendant turns are disabled",
                ));
            }
            if task.fresh_context || task.independent_review {
                return Err(reject(
                    ErrorCode::NoFeasibleRoute,
                    "follow-up preserves existing context; use spawn for isolation",
                ));
            }
            if reaches(state, agent_id, actor_id, &mut BTreeSet::new()) {
                return Err(reject(
                    ErrorCode::OperationConflict,
                    "assignment would create a dependency cycle",
                ));
            }
            if actor_id != state.agent_id
                && !has_turn_capacity(state, Some(agent_id))
                && state.agents.get(agent_id).is_some_and(|agent| {
                    agent
                        .turn
                        .as_ref()
                        .is_none_or(|turn| turn.status.terminal())
                })
            {
                return Err(reject(
                    ErrorCode::LimitExceeded,
                    "follow-up would wait on its assigning descendant's occupied slot",
                ));
            }
            let assignment = assignment(state, actor_id, task)?;
            let max = run.limits.queued_runs as usize;
            let target = non_root_mut(state, agent_id)?;
            if target.queue.len() >= max {
                return Err(reject(ErrorCode::LimitExceeded, "agent task queue is full"));
            }
            let assignment_id = assignment.assignment_id.clone();
            target.queue.push_back(assignment);
            json!({"assignment_id":assignment_id,"agent_id":agent_id,"queued":true})
        }
        Action::Wait {
            agent_ids,
            timeout_ms,
        } => {
            if *timeout_ms > 600_000 || agent_ids.is_empty() {
                return Err(reject(
                    ErrorCode::LimitExceeded,
                    "wait requires targets and a bounded timeout",
                ));
            }
            let mut targets = BTreeMap::new();
            for target in agent_ids {
                if target == actor_id
                    || subtree(state, target).contains(actor_id)
                    || reaches(state, target, actor_id, &mut BTreeSet::new())
                {
                    return Err(reject(
                        ErrorCode::OperationConflict,
                        "agent wait would create a cycle",
                    ));
                }
                let observed = state
                    .agents
                    .get(target)
                    .and_then(wait_target)
                    .ok_or_else(|| reject(ErrorCode::UnauthorizedScope, "unknown wait target"))?;
                targets.insert(target.clone(), observed);
            }
            let wait = WaitState {
                targets,
                deadline_ms: now_ms.saturating_add(*timeout_ms),
            };
            if *timeout_ms == 0
                || wait
                    .targets
                    .values()
                    .any(|target| target.status.is_some_and(AgentStatus::terminal))
                || actor.mailbox.iter().any(|mail| !mail.consumed)
            {
                wait_result(state, &wait, now_ms)
            } else {
                return Ok(Applied::Waiting(wait));
            }
        }
        Action::Interrupt { agent_id } => {
            if agent_id == &state.agent_id {
                return Err(reject(
                    ErrorCode::UnauthorizedScope,
                    "root interruption requires run cancellation",
                ));
            }
            non_root_mut(state, agent_id)?;
            let targets = subtree(state, agent_id);
            for target in &targets {
                if let Some(agent) = state.agents.get_mut(target) {
                    agent.queue.clear();
                    if let Some(turn) = &mut agent.turn
                        && !turn.status.terminal()
                    {
                        turn.status = AgentStatus::Cancelling;
                        turn.cancellation_requested = true;
                        turn.terminal_reason = Some("agent interruption requested".into());
                    }
                }
            }
            json!({"agent_id":agent_id,"subtree":targets,"status":"cancelling"})
        }
        Action::List {} => {
            json!({"agents":state.agents.values().map(|agent| json!({"agent_id":agent.agent_id,"parent_id":agent.parent_id,"path":agent.display_path,"turn_id":agent.turn.as_ref().map(|turn| &turn.agent_turn_id),"status":agent.turn.as_ref().map(|turn| turn.status),"queued_tasks":agent.queue.len()})).collect::<Vec<_>>()})
        }
    };
    Ok(Applied::Complete(result))
}

/// Descendant-owned queued work reserves its idle target's next slot: the
/// assigning turn cannot finish and release its own slot before that work ends.
/// Root-owned queues may wait without reserving capacity and do not deadlock it.
pub(super) fn has_turn_capacity(state: &SessionSnapshot, starting: Option<&str>) -> bool {
    state.run.as_ref().is_none_or(|run| {
        run.input.max_concurrent_subagents.is_none_or(|maximum| {
            state
                .agents
                .values()
                .filter(|agent| {
                    agent.parent_id.is_some()
                        && Some(agent.agent_id.as_str()) != starting
                        && (agent.turn.as_ref().is_some_and(|turn| {
                            turn.run_id == run.run_id && !turn.status.terminal()
                        }) || agent.queue.iter().any(|work| {
                            work.run_id == run.run_id && work.sender_id != state.agent_id
                        }))
                })
                .count()
                < maximum as usize
        })
    })
}

fn allocate(
    state: &mut SessionSnapshot,
    actor_id: &str,
    task: &Work,
    intent: Intent<'_>,
    state_revision: u64,
) -> Result<Applied, CoreError> {
    if !has_turn_capacity(state, None) {
        return Err(reject(
            ErrorCode::LimitExceeded,
            "active descendant turn capacity reached",
        ));
    }
    let mut work = assignment(state, actor_id, task)?;
    let mut decision = allocation::choose(state, actor_id, task, &work, intent, state_revision)?;
    let allocation_id = decision.allocation_id.clone();
    work.allocation_id = Some(allocation_id.clone());
    if let Some(error) = &decision.error {
        let error = error.clone();
        state.allocations.insert(allocation_id.clone(), decision);
        return Ok(Applied::Rejected {
            allocation_id,
            error,
        });
    }
    let chosen = decision
        .candidates
        .iter()
        .find(|candidate| Some(&candidate.candidate_id) == decision.selected_candidate_id.as_ref())
        .ok_or_else(|| {
            reject(
                ErrorCode::CheckpointConflict,
                "allocation has no selected candidate",
            )
        })?;
    let turn_id = work.assignment_id.clone();
    let (agent_id, created) = if chosen.kind == ContextKind::Reuse {
        let target = chosen.agent_id.as_ref().ok_or_else(|| {
            reject(
                ErrorCode::CheckpointConflict,
                "reuse candidate has no agent",
            )
        })?;
        let agent = non_root_mut(state, target)?;
        // Evidence retained in the reused history remains a required dependency
        // of the new task, even when it was optional to the assigning agent.
        work.input = allocation::reuse_input(&work.input, agent);
        decision.input = work.input.clone();
        agent.queue.push_back(work);
        (target.clone(), false)
    } else {
        decision.applied_state_revision = Some(state_revision + 1);
        (spawn(state, actor_id, task, work, chosen.kind)?, true)
    };
    decision.selected_agent_id = Some(agent_id.clone());
    state.allocations.insert(allocation_id.clone(), decision);
    Ok(Applied::Complete(
        json!({"agent_id":agent_id,"agent_turn_id":turn_id,"created":created,"allocation_id":allocation_id}),
    ))
}

fn spawn(
    state: &mut SessionSnapshot,
    actor_id: &str,
    task: &Work,
    work: Assignment,
    kind: ContextKind,
) -> Result<String, CoreError> {
    let parent = state
        .agents
        .get(actor_id)
        .ok_or_else(|| reject(ErrorCode::UnauthorizedScope, "unknown parent"))?;
    let agent_id = id("agent");
    let mut history = if kind == ContextKind::Inherited {
        allocation::inherited_history(parent).to_vec()
    } else {
        Vec::new()
    };
    let context_sources = if kind == ContextKind::Inherited {
        parent.context_sources.clone()
    } else {
        Vec::new()
    };
    let required_instructions = work.required_instructions.clone();
    history.push(Message::text(Role::User, &work.input.text));
    state.agents.insert(
        agent_id.clone(),
        AgentState {
            agent_id: agent_id.clone(),
            parent_id: Some(actor_id.into()),
            display_path: format!("{}/{}", parent.display_path, agent_id),
            depth: parent.depth + 1,
            context_revision: 1,
            history,
            required_instructions,
            turn: Some(new_turn(work, Some(0))),
            queue: VecDeque::new(),
            mailbox: Vec::new(),
            task_scope: task.task_scope.clone(),
            context_sources,
            last_scheduled: 0,
        },
    );
    Ok(agent_id)
}

fn assignment(
    state: &SessionSnapshot,
    actor_id: &str,
    task: &Work,
) -> Result<Assignment, CoreError> {
    let assigning_agent = state
        .agents
        .get(actor_id)
        .ok_or_else(|| reject(ErrorCode::UnauthorizedScope, "unknown assigning agent"))?;
    let parent = assigning_agent
        .turn
        .as_ref()
        .ok_or_else(|| reject(ErrorCode::UnauthorizedScope, "assigning agent has no turn"))?;
    let run = state
        .run
        .as_ref()
        .ok_or_else(|| reject(ErrorCode::Busy, "no active run"))?;
    let mut input = parent.input.clone();
    input.discardable_history = None;
    input.text = task.text.clone();
    input.model = task.model.clone().unwrap_or(input.model);
    if task.model.is_some() {
        input.routing.model = super::protocol::ModelMode::Fixed;
    }
    if task.effort.is_some() {
        input.effort = task.effort.clone();
    }
    input.verification = None;
    for criterion in &task.acceptance_criteria {
        if !input.acceptance_criteria.contains(criterion) {
            input.acceptance_criteria.push(criterion.clone());
        }
    }
    for material in &task.required_materials {
        if !input.required_materials.contains(material) {
            input.required_materials.push(material.clone());
        }
    }
    super::session::validate_input(&input, &run.limits)?;
    super::session::pin_required_materials(state, &mut input)?;
    let mut required_instructions = assigning_agent.required_instructions.clone();
    if !required_instructions.contains(&task.text) {
        required_instructions.push(task.text.clone());
    }
    Ok(Assignment {
        assignment_id: id("turn"),
        run_id: run.run_id.clone(),
        sender_id: actor_id.into(),
        input,
        required_instructions,
        allocation_id: None,
    })
}

pub fn new_turn(work: Assignment, history_start: Option<usize>) -> AgentTurn {
    AgentTurn {
        run_id: work.run_id,
        agent_turn_id: work.assignment_id,
        assigned_by: work.sender_id,
        input: work.input,
        allocation_id: work.allocation_id,
        history_start,
        status: AgentStatus::Runnable,
        cancellation_requested: false,
        steps: Vec::new(),
        invocations: Vec::new(),
        core_calls: Vec::new(),
        final_answer: None,
        terminal_reason: None,
        notified: false,
    }
}

pub fn enqueue_mail(
    state: &mut SessionSnapshot,
    sender: &str,
    target: &str,
    kind: &str,
    content: Value,
) -> Result<String, CoreError> {
    let sender_agent = state
        .agents
        .get(sender)
        .ok_or_else(|| reject(ErrorCode::UnauthorizedScope, "unknown message sender"))?;
    let context_sources = sender_agent.context_sources.clone();
    let sender_turn_id = sender_agent
        .turn
        .as_ref()
        .map(|turn| turn.agent_turn_id.clone())
        .ok_or_else(|| reject(ErrorCode::UnauthorizedScope, "unknown message sender"))?;
    let limit = state
        .run
        .as_ref()
        .ok_or_else(|| reject(ErrorCode::Busy, "no active run"))?
        .limits
        .mailbox_messages as usize;
    let recipient = state
        .agents
        .get_mut(target)
        .ok_or_else(|| reject(ErrorCode::UnauthorizedScope, "unknown message target"))?;
    let archived = kind == "agent_result"
        && recipient
            .turn
            .as_ref()
            .is_some_and(|turn| turn.status.terminal() || turn.status == AgentStatus::Cancelling);
    if !archived
        && recipient
            .mailbox
            .iter()
            .filter(|mail| !mail.consumed)
            .count()
            >= limit
    {
        return Err(reject(
            ErrorCode::LimitExceeded,
            "recipient mailbox is full",
        ));
    }
    let message_id = id("message");
    recipient.mailbox.push(Mail {
        message_id: message_id.clone(),
        sender_id: sender.into(),
        sender_turn_id,
        kind: kind.into(),
        content,
        context_sources,
        consumed: archived,
    });
    Ok(message_id)
}

pub fn wait_ready(state: &SessionSnapshot, actor: &str, wait: &WaitState, now_ms: u64) -> bool {
    now_ms >= wait.deadline_ms
        || state
            .agents
            .get(actor)
            .is_some_and(|agent| agent.mailbox.iter().any(|mail| !mail.consumed))
        || wait.targets.iter().any(|(id, observed)| {
            state
                .agents
                .get(id)
                .and_then(wait_target)
                .is_none_or(|target| {
                    &target != observed || target.status.is_some_and(AgentStatus::terminal)
                })
        })
}

fn wait_target(agent: &AgentState) -> Option<WaitTarget> {
    if let Some(queued) = agent.queue.back() {
        return Some(WaitTarget {
            agent_turn_id: queued.assignment_id.clone(),
            status: None,
        });
    }
    agent.turn.as_ref().map(|turn| WaitTarget {
        agent_turn_id: turn.agent_turn_id.clone(),
        status: Some(turn.status),
    })
}

pub fn wait_result(state: &SessionSnapshot, wait: &WaitState, now_ms: u64) -> Value {
    let agents = wait.targets.keys().filter_map(|id| state.agents.get(id)).filter_map(|agent| {
        let target = wait_target(agent)?;
        let answer = agent.turn.as_ref().filter(|turn| turn.agent_turn_id == target.agent_turn_id).and_then(|turn| turn.final_answer.as_ref());
        Some(json!({
            "agent_id":agent.agent_id,
            "agent_turn_id":target.agent_turn_id,
            "status":target.status.map_or_else(|| json!("queued"), |status| json!(status)),
            "final_answer":answer,
            "provenance":"agent_conclusion",
            "context_sources":if answer.is_some() { agent.context_sources.as_slice() } else { &[] },
        }))
    }).collect::<Vec<_>>();
    json!({"timed_out":now_ms>=wait.deadline_ms,"agents":agents})
}

pub(crate) fn assignment_cycle(state: &SessionSnapshot, target: &str, actor: &str) -> bool {
    reaches(state, target, actor, &mut BTreeSet::new())
}

fn reaches(state: &SessionSnapshot, from: &str, target: &str, seen: &mut BTreeSet<String>) -> bool {
    if from == target {
        return true;
    }
    if !seen.insert(from.into()) {
        return false;
    }
    let explicit_wait = state
        .agents
        .get(from)
        .and_then(|agent| agent.turn.as_ref())
        .is_some_and(|turn| {
            turn.core_calls
                .iter()
                .filter(|call| call.result.is_none())
                .filter_map(|call| call.wait.as_ref())
                .flat_map(|wait| wait.targets.keys())
                .any(|next| reaches(state, next, target, seen))
        });
    let descendants = subtree(state, from);
    explicit_wait
        || state.agents.values().any(|agent| {
            let descendant = agent.agent_id != from && descendants.contains(&agent.agent_id);
            let dependent = agent.queue.iter().any(|work| work.sender_id == from)
                || agent.turn.as_ref().is_some_and(|turn| {
                    (turn.assigned_by == from || descendant)
                        && (!turn.status.terminal() || !turn.notified)
                })
                || (descendant && !agent.queue.is_empty());
            dependent && reaches(state, &agent.agent_id, target, seen)
        })
}

pub fn subtree(state: &SessionSnapshot, root: &str) -> BTreeSet<String> {
    let mut ids = BTreeSet::from([root.to_owned()]);
    loop {
        let previous = ids.len();
        for agent in state.agents.values() {
            if agent
                .parent_id
                .as_ref()
                .is_some_and(|parent| ids.contains(parent))
            {
                ids.insert(agent.agent_id.clone());
            }
        }
        if ids.len() == previous {
            return ids;
        }
    }
}

fn non_root_mut<'a>(
    state: &'a mut SessionSnapshot,
    id: &str,
) -> Result<&'a mut AgentState, CoreError> {
    if id == state.agent_id {
        return Err(reject(
            ErrorCode::UnauthorizedScope,
            "operation requires a non-root agent",
        ));
    }
    state
        .agents
        .get_mut(id)
        .ok_or_else(|| reject(ErrorCode::UnauthorizedScope, "unknown agent"))
}

fn id(prefix: &str) -> String {
    format!("{prefix}_{}", uuid::Uuid::new_v4().simple())
}
fn reject(code: ErrorCode, message: impl Into<String>) -> CoreError {
    CoreError::rejected(code, message)
}
