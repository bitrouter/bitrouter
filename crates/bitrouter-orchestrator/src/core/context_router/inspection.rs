//! Bounded public task/view observations. These are never execution authority.

use serde::{Deserialize, Serialize};

use crate::core::session::{AgentStatus, ModelStep, SessionSnapshot};
use bitrouter_sdk::routing::signals::{NextStepRole, ProgressState, TaskFamily};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Inspection {
    pub run_id: String,
    pub tasks: Vec<TaskView>,
    pub tasks_truncated: bool,
    pub evidence_blocks: usize,
    pub retained_evidence_bytes: usize,
    pub decisions: DecisionUsage,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskView {
    pub task_id: String,
    pub agent_id: String,
    pub parent_task_id: Option<String>,
    pub text_preview: String,
    pub status: AgentStatus,
    pub view_id: Option<String>,
    pub prompt_bytes: Option<usize>,
    #[serde(default)]
    pub selected_model: Option<String>,
    #[serde(default)]
    pub routing_reason: Option<String>,
    pub source_groups: usize,
    pub omitted_groups: usize,
    pub result_references: usize,
    /// Current-step presentation facts; absent in older observations.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub routing: Option<RoutingStatus>,
}

/// Public progress from the current task's acknowledged execution state.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoutingStatus {
    pub phase: RoutingPhase,
    /// Accepted classifier labels, including explicit abstention as unknown.
    pub workflow: Option<Workflow>,
    /// Current serving attempt, or the selected plan before dispatch.
    pub model: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoutingPhase {
    Preparing,
    Classifying,
    Routing,
    Generating,
    Tools,
    Waiting,
    Cancelling,
    Finished,
}

impl RoutingPhase {
    pub const fn key(self) -> &'static str {
        match self {
            Self::Preparing => "preparing",
            Self::Classifying => "classifying",
            Self::Routing => "routing",
            Self::Generating => "generating",
            Self::Tools => "tools",
            Self::Waiting => "waiting",
            Self::Cancelling => "cancelling",
            Self::Finished => "finished",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Workflow {
    pub task_family: TaskFamily,
    pub next_step_role: NextStepRole,
    pub progress_state: ProgressState,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecisionUsage {
    pub attempts: usize,
    pub pending: usize,
    pub failures: usize,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    /// Frozen operator-price estimate, never a reported or reconciled bill.
    pub estimated_micro_usd: Option<u64>,
}

impl Inspection {
    pub fn capture(state: &SessionSnapshot) -> Option<Self> {
        if !super::tasks::enabled(state) {
            return None;
        }
        let run = state.run.as_ref()?;
        let work: Vec<_> = state
            .context_store
            .work
            .values()
            .filter(|work| work.run_id == run.run_id)
            .collect();
        let mut tasks: Vec<_> = work
            .iter()
            .map(|work| {
                let view = work
                    .last_view_id
                    .as_ref()
                    .and_then(|id| state.context_store.views.get(id));
                let execution = view.and_then(|view| {
                    state
                        .context_store
                        .executions
                        .values()
                        .find(|execution| execution.view_id == view.view_id)
                });
                TaskView {
                    task_id: work.task_id.clone(),
                    agent_id: work.agent_id.clone(),
                    parent_task_id: work.parent_task_id.clone(),
                    text_preview: super::planner::truncate(&work.text, 256).into(),
                    status: work.status,
                    view_id: work.last_view_id.clone(),
                    prompt_bytes: view.map(|view| view.prompt_bytes),
                    selected_model: execution
                        .and_then(|execution| execution.model.as_ref())
                        .map(|model| model.effective_model.clone()),
                    routing_reason: execution
                        .and_then(|execution| execution.routing.as_ref())
                        .map(|routing| routing.reason.clone()),
                    source_groups: work.evidence.len(),
                    omitted_groups: view.map_or(0, |view| view.omitted.len()),
                    result_references: work.result_evidence.len(),
                    routing: task_routing(state, work),
                }
            })
            .collect();
        tasks.sort_by(|left, right| {
            left.parent_task_id
                .is_some()
                .cmp(&right.parent_task_id.is_some())
                .then(left.task_id.cmp(&right.task_id))
        });
        let tasks_truncated = tasks.len() > 128;
        tasks.truncate(128);
        let mut decisions = DecisionUsage {
            attempts: 0,
            pending: 0,
            failures: 0,
            input_tokens: Some(0),
            output_tokens: Some(0),
            estimated_micro_usd: Some(0),
        };
        for (id, receipt) in state
            .context_store
            .decisions
            .iter()
            .filter(|(_, receipt)| receipt.run_id == run.run_id)
        {
            decisions.attempts += 1;
            let usage = match &receipt.outcome {
                None => {
                    decisions.pending += 1;
                    None
                }
                Some(Ok(response)) => Some(response.usage),
                Some(Err(error)) => {
                    decisions.failures += 1;
                    if !error.may_have_run {
                        continue;
                    }
                    error.usage
                }
            };
            decisions.input_tokens = decisions
                .input_tokens
                .zip(usage)
                .and_then(|(total, usage)| total.checked_add(usage.input_tokens));
            decisions.output_tokens = decisions
                .output_tokens
                .zip(usage)
                .and_then(|(total, usage)| total.checked_add(usage.output_tokens));
            let amount = state
                .cost_work
                .get(&run.run_id)
                .and_then(|ledger| ledger.work.get(id))
                .and_then(|work| work.decision_estimate_micro_usd);
            decisions.estimated_micro_usd = decisions
                .estimated_micro_usd
                .zip(amount)
                .and_then(|(total, amount)| total.checked_add(amount));
        }
        Some(Self {
            run_id: run.run_id.clone(),
            tasks,
            tasks_truncated,
            evidence_blocks: state.context_store.evidence.len(),
            retained_evidence_bytes: state
                .context_store
                .evidence
                .values()
                .fold(0_usize, |bytes, block| bytes.saturating_add(block.bytes)),
            decisions,
        })
    }
}

fn task_routing(state: &SessionSnapshot, work: &super::WorkUnit) -> Option<RoutingStatus> {
    let turn = state
        .agents
        .get(&work.agent_id)?
        .turn
        .as_ref()
        .filter(|turn| turn.agent_turn_id == work.task_id)?;
    let waiting = turn.status == AgentStatus::Runnable
        && turn.final_answer.is_some()
        && crate::core::session::pending_dependencies(state, &work.agent_id);
    let step = turn
        .steps
        .last()
        .filter(|step| waiting || turn.status != AgentStatus::Runnable || !step.settled);
    let decision = step.and_then(|step| {
        state
            .context_store
            .decisions
            .values()
            .filter(|receipt| {
                receipt.task_id == work.task_id
                    && receipt.run_id == work.run_id
                    && receipt.agent_id == work.agent_id
                    && receipt.source.context_revision == step.context_revision
                    && receipt.source.signal_revision == step.signal_revision
                    && !receipt.stale
            })
            .max_by_key(|receipt| receipt.intent_state_revision)
    });
    // Applied views bind the classifier receipt to this exact execution step.
    // Pending classification can report progress without reusing old labels.
    let assessment_receipt = step
        .and_then(|step| state.context_store.executions.get(&step.step_id))
        .filter(|execution| {
            execution.task_id == work.task_id && execution.agent_id == work.agent_id
        })
        .and_then(|execution| execution.decision_id.as_ref())
        .and_then(|id| state.context_store.decisions.get(id))
        .filter(|receipt| !receipt.stale);
    let workflow = assessment_receipt.and_then(|receipt| {
        let response = receipt.outcome.as_ref()?.as_ref().ok()?;
        let assessment = bitrouter_sdk::routing::assessment::decode(
            &receipt.request,
            response,
            receipt.policy.confidence_threshold,
        )
        .ok()?;
        Some(Workflow {
            task_family: assessment.task_family,
            next_step_role: assessment.next_step_role,
            progress_state: assessment.progress_state,
        })
    });
    let phase = match turn.status {
        AgentStatus::ModelRunning if decision.is_some_and(|receipt| receipt.outcome.is_none()) => {
            RoutingPhase::Classifying
        }
        AgentStatus::ModelRunning
            if step.is_some_and(|step| !step.attempts.is_empty() && !step.settled) =>
        {
            RoutingPhase::Generating
        }
        AgentStatus::ModelRunning => RoutingPhase::Routing,
        AgentStatus::WaitingTool | AgentStatus::WaitingMaterial => RoutingPhase::Tools,
        AgentStatus::WaitingMessage => RoutingPhase::Waiting,
        AgentStatus::Runnable if waiting => RoutingPhase::Waiting,
        AgentStatus::Runnable => RoutingPhase::Preparing,
        AgentStatus::Cancelling => RoutingPhase::Cancelling,
        _ => RoutingPhase::Finished,
    };
    // A new preparation cannot display the previous step's model as its choice.
    let model = step.and_then(serving_model);
    Some(RoutingStatus {
        phase,
        workflow,
        model,
    })
}

fn serving_model(step: &ModelStep) -> Option<String> {
    let plan = step.plan.as_ref()?;
    if let Some(attempt) = step.attempts.last()
        && let Some(route) = usize::try_from(attempt.index)
            .ok()
            .and_then(|index| plan.routes.get(index))
    {
        let report = attempt.receipt.as_ref().map(|receipt| &receipt.report);
        let provider = report
            .and_then(|report| report.actual_provider.as_deref())
            .unwrap_or(&route.provider);
        let model = report
            .and_then(|report| report.actual_model.as_deref())
            .unwrap_or(&route.model);
        return Some(format!("{provider}:{model}"));
    }
    Some(plan.effective_model.clone())
}
