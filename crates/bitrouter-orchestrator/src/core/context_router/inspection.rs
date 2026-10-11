//! Bounded public task/view observations. These are never execution authority.

use serde::{Deserialize, Serialize};

use crate::core::session::{AgentStatus, SessionSnapshot};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Inspection {
    pub run_id: String,
    pub tasks: Vec<TaskView>,
    pub tasks_truncated: bool,
    pub evidence_blocks: usize,
    pub retained_evidence_bytes: usize,
    pub decisions: DecisionUsage,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
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
}

#[derive(Clone, Debug, Serialize, Deserialize)]
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
