//! Read-only child assignments run inside the root worker's exclusive workspace grant.
//! The parent's invocation and the child's journal share one atomic root checkpoint.
use std::cell::{Cell, RefCell};
use std::collections::HashSet;

use anyhow::{Context, Result, ensure};
use bitrouter_ai::core::protocol::Limits;
use bitrouter_ai::harness::agent::ToolMode;
use bitrouter_ai::harness::runtime::EffectStatus;
use bitrouter_ai::harness::turn::TurnStatus;
use bitrouter_ai::types::ToolResultOutput;
use bitrouter_sdk::gateway::GatewayClient;
use futures::{StreamExt, stream::FuturesUnordered};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::checkpoint::{ModelState, Thread, ToolState, Turn};
use crate::runner::{self, Journal};

pub(crate) fn declaration() -> Value {
    json!({"type":"function","function":{"name":"spawn_agent",
    "description":"Assign a bounded task to a new read-only child. Submit independent calls together to run concurrently. Each result returns that child's identity and conclusion.",
    "parameters":{"type":"object","required":["task"],"additionalProperties":false,"properties":{
        "task":{"type":"object","required":["text"],"additionalProperties":false,"properties":{"text":{"type":"string"}}}
    }}}})
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Assignment {
    task: Task,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Task {
    text: String,
}

fn task(args: &str) -> Result<String> {
    let work: Assignment = serde_json::from_str(args)?;
    let text = work.task.text;
    ensure!(
        !text.trim().is_empty() && text.len() as u64 <= Limits::default().input_bytes,
        "invalid child task length"
    );
    Ok(text)
}

pub(crate) fn is_spawn(thread: &Thread, index: usize) -> Result<bool> {
    let call = thread
        .current()?
        .response()
        .and_then(|r| r.message.tool_calls.get(index))
        .context("missing call")?;
    Ok(call.function.name == "spawn_agent")
}

fn conclusion(child: &Thread) -> Result<Option<Value>> {
    let turn = child.current()?;
    Ok(turn
        .outcome
        .as_ref()
        .filter(|o| o.status.terminal())
        .map(|o| {
            json!({"parent_id":child.parent_id,"agent_id":child.thread_id,"turn_id":turn.turn_id,
            "status":o.status,"answer":o.final_answer,"detail":o.detail})
        }))
}

pub(crate) fn collect_conclusions(parent: &mut Thread) -> Result<()> {
    let turn = parent.current()?;
    let Some(response) = turn.response() else {
        return Ok(());
    };
    let mut joined = vec![];
    for (index, call) in response.calls.iter().enumerate() {
        if matches!(call.state, ToolState::Child) {
            let child = turn
                .children
                .get(&call.item_id)
                .context("missing delegated child")?;
            if let Some(value) = conclusion(child)? {
                joined.push((index, value));
            }
        }
    }
    for (index, value) in joined {
        parent.response_mut()?.calls[index].state = ToolState::ToolResult {
            output: ToolResultOutput::Json { value },
            effect: EffectStatus::Completed,
        };
    }
    Ok(())
}

struct ChildJournal<'a, 'p> {
    parent: RefCell<&'p mut Thread>,
    journal: &'a dyn Journal,
}

impl Journal for ChildJournal<'_, '_> {
    fn save(&self, child: &Thread) -> Result<()> {
        let mut parent = self
            .parent
            .try_borrow_mut()
            .context("child journal is already borrowed")?;
        let saved = parent
            .current_mut()?
            .children
            .get_mut(&child.thread_id)
            .context("unassigned child")?;
        *saved = child.clone();
        self.journal.save(&parent)
    }
}

pub(crate) struct Scheduler {
    pub fenced: Cell<bool>,
    agents: Cell<u32>,
    attempts: Cell<u32>,
    pub models: Semaphore,
    pub tools: Semaphore,
    limits: Limits,
}

impl Scheduler {
    pub fn restore(thread: &Thread) -> Result<Self> {
        let limits = Limits::default();
        let (agents, attempts) = counts(thread.current()?)?;
        ensure!(
            agents <= limits.agents && attempts <= limits.model_attempts,
            "managed tree exceeds limits"
        );
        Ok(Self {
            fenced: false.into(),
            agents: agents.into(),
            attempts: attempts.into(),
            models: Semaphore::new(limits.active_models as usize),
            tools: Semaphore::new(limits.outstanding_tools as usize),
            limits,
        })
    }
    pub fn admit_model(&self) -> bool {
        reserve(&self.attempts, self.limits.model_attempts)
    }

    pub async fn children(
        &self,
        journal: &dyn Journal,
        parent: &mut Thread,
        client: &GatewayClient,
        cancel: &CancellationToken,
        depth: u32,
        start: usize,
    ) -> Result<()> {
        // A sibling's lost model or tool intent fences the entire tree on cold resume.
        if parent.current()?.children.values().any(unresolved) {
            return runner::settle(
                journal,
                parent,
                TurnStatus::RecoveryRequired,
                "child dispatch is unresolved",
                None,
            );
        }
        let count = parent
            .current()?
            .response()
            .context("missing response")?
            .calls
            .len();
        let mut ready = vec![];
        for index in start..count {
            if !is_spawn(parent, index)? {
                break;
            }
            let r = parent.current()?.response().context("missing response")?;
            let inv = &r.calls[index];
            let id = inv.item_id;
            match inv.state {
                ToolState::ToolResult { .. } => continue,
                ToolState::Child => {
                    ready.push(
                        parent
                            .current()?
                            .children
                            .get(&id)
                            .context("missing child")?
                            .clone(),
                    );
                    continue;
                }
                ToolState::Proposed => (),
                ToolState::ToolIntent => anyhow::bail!("unresolved tool dispatch"),
            }
            let work = task(&r.message.tool_calls[index].function.arguments);
            let work = work.and_then(|work| {
                ensure!(
                    depth < self.limits.child_depth,
                    "child depth limit exceeded"
                );
                ensure!(
                    reserve(&self.agents, self.limits.agents),
                    "agent count limit exceeded"
                );
                Ok(work)
            });
            match work {
                Ok(work) => {
                    let mut child = Thread::new(
                        parent.workspace.clone(),
                        parent.endpoint.clone(),
                        parent.model.clone(),
                        parent.effort,
                        parent.max_output_tokens,
                        ToolMode::ReadOnly,
                    );
                    child.thread_id = id;
                    child.parent_id = Some(parent.thread_id);
                    child.enable_managed();
                    child.m0_instructions = parent.m0_instructions.clone();
                    child.skills = parent.skills.clone();
                    child.start(work, None)?;
                    ensure!(
                        parent
                            .current_mut()?
                            .children
                            .insert(id, child.clone())
                            .is_none(),
                        "duplicate child identity"
                    );
                    parent.response_mut()?.calls[index].state = ToolState::Child;
                    ready.push(child);
                }
                Err(error) => {
                    parent.response_mut()?.calls[index].state = ToolState::ToolResult {
                        output: ToolResultOutput::ExecutionDenied {
                            reason: Some(error.to_string()),
                        },
                        effect: EffectStatus::NotExecuted,
                    }
                }
            }
        }
        journal.save(parent)?;
        {
            let sink = ChildJournal {
                parent: RefCell::new(&mut *parent),
                journal,
            };
            let mut jobs = FuturesUnordered::new();
            for mut child in ready {
                let sink = &sink;
                jobs.push(async move {
                    Box::pin(runner::drive(
                        sink,
                        &mut child,
                        client,
                        cancel,
                        Some(self),
                        depth + 1,
                    ))
                    .await
                });
            }
            while let Some(result) = jobs.next().await {
                result?;
            }
        }
        collect_conclusions(parent)?;
        if parent
            .current()?
            .response()
            .is_some_and(|r| r.calls.iter().any(|c| matches!(c.state, ToolState::Child)))
        {
            return runner::settle(
                journal,
                parent,
                TurnStatus::RecoveryRequired,
                "child dispatch is unresolved",
                None,
            );
        }
        journal.save(parent)
    }
}

fn reserve(count: &Cell<u32>, limit: u32) -> bool {
    if count.get() >= limit {
        return false;
    }
    count.set(count.get() + 1);
    true
}

pub(crate) fn unresolved(thread: &Thread) -> bool {
    thread.current().map_or(true, |t| {
        t.next() == crate::checkpoint::Next::Blocked
            || t.outcome
                .as_ref()
                .is_some_and(|o| o.status == TurnStatus::RecoveryRequired)
            || t.children.values().any(unresolved)
    })
}

fn counts(turn: &Turn) -> Result<(u32, u32)> {
    let mut agents = 1;
    let mut attempts = u32::try_from(turn.steps.len())?;
    for child in turn.children.values() {
        let (n, steps) = counts(child.current()?)?;
        agents += n;
        attempts += steps;
    }
    Ok((agents, attempts))
}

pub(crate) fn validate(thread: &Thread) -> Result<()> {
    for turn in &thread.turns {
        let mut seen = HashSet::from([thread.thread_id]);
        validate_turn(thread, turn, &mut seen, 0)?;
    }
    Ok(())
}

fn validate_turn(parent: &Thread, turn: &Turn, seen: &mut HashSet<Uuid>, depth: u32) -> Result<()> {
    let mut referenced = HashSet::new();
    for step in &turn.steps {
        let ModelState::ModelResponse { response } = &step.state else {
            continue;
        };
        for (call, inv) in response.message.tool_calls.iter().zip(&response.calls) {
            let Some(child) = turn.children.get(&inv.item_id) else {
                ensure!(
                    !matches!(inv.state, ToolState::Child),
                    "missing child checkpoint"
                );
                ensure!(
                    !(parent.managed()
                        && call.function.name == "spawn_agent"
                        && matches!(
                            inv.state,
                            ToolState::ToolResult {
                                effect: EffectStatus::Completed,
                                ..
                            }
                        )),
                    "missing joined child checkpoint"
                );
                continue;
            };
            ensure!(
                depth < Limits::default().child_depth
                    && seen.insert(inv.item_id)
                    && seen.len() <= Limits::default().agents as usize,
                "invalid child graph"
            );
            referenced.insert(inv.item_id);
            ensure!(
                parent.managed()
                    && child.managed()
                    && call.function.name == "spawn_agent"
                    && child.thread_id == inv.item_id
                    && child.parent_id == Some(parent.thread_id)
                    && child.tool_mode == ToolMode::ReadOnly
                    && child.workspace == parent.workspace
                    && child.endpoint == parent.endpoint
                    && child.model == parent.model
                    && child.effort == parent.effort
                    && child.max_output_tokens == parent.max_output_tokens
                    && child.m0_instructions == parent.m0_instructions
                    && serde_json::to_value(&child.skills)?
                        == serde_json::to_value(&parent.skills)?
                    && child.turns.len() == 1,
                "invalid delegated child identity or grant"
            );
            let t = child.current()?;
            ensure!(
                t.sealed_messages.is_empty() && t.prompt == task(&call.function.arguments)?,
                "child assignment mismatch"
            );
            match &inv.state {
                ToolState::Child => (),
                ToolState::ToolResult {
                    output: ToolResultOutput::Json { value },
                    effect: EffectStatus::Completed,
                } => ensure!(
                    conclusion(child)?.as_ref() == Some(value),
                    "child conclusion mismatch"
                ),
                _ => anyhow::bail!("invalid child invocation"),
            }
            validate_turn(child, t, seen, depth + 1)?;
        }
    }
    ensure!(
        referenced.len() == turn.children.len(),
        "unassigned child checkpoint"
    );
    Ok(())
}
