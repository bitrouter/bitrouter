//! Ordered Goal input understanding and atomic requirement/control commits.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use super::workflow::ArtifactRef;
use super::{
    GoalError, GoalOperation, GoalSnapshot, OperationKind, RunIntent, identifier, invalid, message,
};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct InputProgress {
    pub after_revision: u64,
    pub pending: Option<PendingInput>,
    pub latest: Option<InputResult>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Stage {
    Typed,
    Reasoning,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingInput {
    pub operation: GoalOperation,
    pub requirements_revision: u64,
    pub attempt_id: String,
    pub stage: Stage,
    pub classification: Option<ArtifactRef>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskUpdate {
    pub task_id: String,
    pub instruction: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ControlProposal {
    pub target: String,
    pub intent: RunIntent,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PermissionResponse {
    pub decision_id: String,
    pub option_id: String,
}

/// An explicit reference in the original user input, never inferred from a model.
pub struct PermissionReference<'a> {
    pub decision_id: &'a str,
    pub option_id: Option<&'a str>,
}

pub fn permission_reference(text: &str) -> Result<Option<PermissionReference<'_>>, GoalError> {
    let mut words = text.split_whitespace();
    if words.next() != Some("/decision") {
        return Ok(None);
    }
    let decision_id = words
        .next()
        .ok_or_else(|| invalid("/decision requires an actual Decision id"))?;
    identifier(decision_id)?;
    let option_id = if words.next() == Some("/option") {
        let option_id = words
            .next()
            .ok_or_else(|| invalid("/option requires an offered option id"))?;
        identifier(option_id)?;
        Some(option_id)
    } else {
        None
    };
    Ok(Some(PermissionReference {
        decision_id,
        option_id,
    }))
}

/// A model proposal, never permission to mutate or dispatch by itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Interpretation {
    pub answer: Option<String>,
    pub objective: Option<String>,
    pub task_updates: Vec<TaskUpdate>,
    pub control: Option<ControlProposal>,
    #[serde(default)]
    pub permission_response: Option<PermissionResponse>,
    pub clarification: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InputResult {
    pub input_id: String,
    pub source_requirements_revision: u64,
    pub effective_requirements_revision: u64,
    pub answer: Option<String>,
    pub clarification: Option<String>,
    pub requirements_saved: bool,
    pub affected_tasks: Vec<String>,
    pub control_saved: Option<ControlProposal>,
    pub controlled_tasks: Vec<String>,
    pub control_superseded: bool,
    #[serde(default)]
    pub permission_response_saved: Option<PermissionResponse>,
    pub classification: Option<ArtifactRef>,
    pub reasoning_request_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    Begin {
        operation: Box<GoalOperation>,
        attempt_id: String,
        stage: Stage,
    },
    Classified {
        attempt_id: String,
        evidence: ArtifactRef,
        reasoning_attempt_id: String,
    },
    Finish {
        attempt_id: String,
        interpretation: Interpretation,
        reasoning_request_id: Option<String>,
    },
}

pub(super) fn apply(goal: &mut GoalSnapshot, event: &Event, now_ms: u64) -> Result<(), GoalError> {
    match event {
        Event::Begin {
            operation,
            attempt_id,
            stage,
        } => {
            identifier(attempt_id)?;
            if goal.inputs.pending.is_some()
                || operation.goal_id != goal.id
                || operation.revision <= goal.inputs.after_revision
                || !matches!(operation.kind, OperationKind::Input { .. })
            {
                return Err(GoalError::Conflict);
            }
            goal.inputs.pending = Some(PendingInput {
                operation: operation.as_ref().clone(),
                requirements_revision: goal.requirements_revision,
                attempt_id: attempt_id.clone(),
                stage: *stage,
                classification: None,
            });
        }
        Event::Classified {
            attempt_id,
            evidence,
            reasoning_attempt_id,
        } => {
            evidence.validate()?;
            identifier(reasoning_attempt_id)?;
            let pending = goal.inputs.pending.as_mut().ok_or(GoalError::Conflict)?;
            if pending.attempt_id != *attempt_id || pending.stage != Stage::Typed {
                return Err(GoalError::Conflict);
            }
            pending.classification = Some(evidence.clone());
            pending.attempt_id = reasoning_attempt_id.clone();
            pending.stage = Stage::Reasoning;
        }
        Event::Finish {
            attempt_id,
            interpretation,
            reasoning_request_id,
        } => {
            let pending = goal
                .inputs
                .pending
                .as_ref()
                .ok_or(GoalError::Conflict)?
                .clone();
            if pending.attempt_id != *attempt_id {
                return Err(GoalError::Conflict);
            }
            if let Some(id) = reasoning_request_id {
                identifier(id)?;
            }
            let mut proposal = interpretation.clone();
            if pending.stage == Stage::Typed
                && (proposal.answer.is_some()
                    || proposal.objective.is_some()
                    || !proposal.task_updates.is_empty()
                    || proposal.control.is_some()
                    || proposal.permission_response.is_some()
                    || proposal.clarification.is_none())
            {
                return Err(invalid(
                    "typed classification cannot directly commit semantic effects",
                ));
            }
            if reasoning_request_id.is_none()
                && (proposal.answer.is_some()
                    || proposal.objective.is_some()
                    || !proposal.task_updates.is_empty()
                    || proposal.control.is_some()
                    || proposal.permission_response.is_some())
            {
                return Err(invalid(
                    "semantic effects require a recorded reasoning request",
                ));
            }
            for text in [
                &proposal.answer,
                &proposal.clarification,
                &proposal.objective,
            ]
            .into_iter()
            .flatten()
            {
                message(text)?;
            }
            if proposal.clarification.is_some()
                && (proposal.objective.is_some()
                    || !proposal.task_updates.is_empty()
                    || proposal.control.is_some()
                    || proposal.permission_response.is_some())
            {
                return Err(invalid("unresolved input cannot commit proposed effects"));
            }
            let changed = proposal.objective.is_some()
                || !proposal.task_updates.is_empty()
                || proposal.control.is_some()
                || proposal.permission_response.is_some();
            if changed && pending.requirements_revision != goal.requirements_revision {
                proposal.objective = None;
                proposal.task_updates.clear();
                proposal.control = None;
                proposal.permission_response = None;
                proposal.clarification = Some(
                    "Requirements changed during understanding; no proposed effects were committed"
                        .into(),
                );
            }
            let OperationKind::Input {
                text,
                task_reference,
            } = &pending.operation.kind
            else {
                return Err(GoalError::Conflict);
            };
            if changed {
                let reference = permission_reference(text)?;
                if reference.is_some()
                    && (task_reference.is_some()
                        || proposal.objective.is_some()
                        || !proposal.task_updates.is_empty()
                        || proposal.control.is_some())
                {
                    return Err(invalid(
                        "an explicit Decision reply cannot change another scope",
                    ));
                }
                if let Some(response) = &proposal.permission_response {
                    let reference = reference.ok_or_else(|| {
                        invalid(
                            "permission response requires an explicit original Decision reference",
                        )
                    })?;
                    if reference.decision_id != response.decision_id
                        || reference
                            .option_id
                            .is_some_and(|id| id != response.option_id)
                        || task_reference.is_some()
                    {
                        return Err(invalid(
                            "permission response does not match the original reference",
                        ));
                    }
                    super::decision::validate_response(
                        goal,
                        &response.decision_id,
                        &response.option_id,
                        false,
                    )?;
                    let option = goal
                        .workflow
                        .decisions
                        .iter()
                        .find(|decision| decision.id == response.decision_id)
                        .and_then(|decision| {
                            decision
                                .options
                                .iter()
                                .find(|option| option.id == response.option_id)
                        })
                        .ok_or(GoalError::NotFound)?;
                    if option.allows {
                        goal.budget.admission(goal.created_at_ms, now_ms)?;
                        if option.persistent && reference.option_id.is_none() {
                            return Err(invalid(
                                "persistent permission requires the user's explicit offered option id or the permission card",
                            ));
                        }
                    }
                }
            }
            if task_reference.is_some() && proposal.objective.is_some() {
                return Err(invalid(
                    "an explicit task input cannot replace Goal requirements",
                ));
            }
            let control_task = if let Some(control) = &proposal.control {
                if control.target == "goal" {
                    if task_reference.is_some() {
                        return Err(invalid(
                            "an explicit task input cannot control the whole Goal",
                        ));
                    }
                    None
                } else {
                    let id = control
                        .target
                        .strip_prefix("task:")
                        .ok_or_else(|| invalid("unknown control target"))?;
                    identifier(id)?;
                    if task_reference
                        .as_ref()
                        .is_some_and(|reference| reference != id)
                    {
                        return Err(invalid(
                            "control does not match the explicit task reference",
                        ));
                    }
                    Some(
                        goal.workflow
                            .tasks
                            .iter()
                            .position(|task| task.spec.id == id)
                            .ok_or(GoalError::NotFound)?,
                    )
                }
            } else {
                None
            };
            if proposal.answer.is_none()
                && proposal.clarification.is_none()
                && proposal.objective.is_none()
                && proposal.task_updates.is_empty()
                && proposal.control.is_none()
                && proposal.permission_response.is_none()
            {
                return Err(invalid("understanding produced no answer or disposition"));
            }
            let mut affected = BTreeSet::new();
            for update in &proposal.task_updates {
                message(&update.instruction)?;
                if update.instruction.len() > 16 * 1024
                    || !goal
                        .workflow
                        .tasks
                        .iter()
                        .any(|task| task.spec.id == update.task_id)
                    || task_reference
                        .as_ref()
                        .is_some_and(|id| id != &update.task_id)
                    || !affected.insert(update.task_id.clone())
                {
                    return Err(invalid(
                        "task modification has an invalid or ambiguous target",
                    ));
                }
            }
            loop {
                let before = affected.len();
                for task in &goal.workflow.tasks {
                    if task
                        .spec
                        .dependencies
                        .iter()
                        .any(|id| affected.contains(id))
                    {
                        affected.insert(task.spec.id.clone());
                    }
                }
                if before == affected.len() {
                    break;
                }
            }
            let requirements_saved =
                proposal.objective.is_some() || !proposal.task_updates.is_empty();
            if let Some(response) = &proposal.permission_response {
                super::decision::apply(
                    goal,
                    &super::decision::Event::Respond {
                        decision_id: response.decision_id.clone(),
                        operation_id: pending.operation.id.clone(),
                        option_id: response.option_id.clone(),
                        automatic: false,
                    },
                )?;
            }
            if requirements_saved {
                goal.requirements_revision = goal
                    .requirements_revision
                    .checked_add(1)
                    .ok_or_else(|| invalid("requirements revision exhausted"))?;
                if let Some(objective) = proposal.objective {
                    goal.objective = objective;
                }
                for update in &proposal.task_updates {
                    let task = goal
                        .workflow
                        .tasks
                        .iter_mut()
                        .find(|task| task.spec.id == update.task_id)
                        .ok_or(GoalError::NotFound)?;
                    task.spec.instruction = update.instruction.clone();
                }
                for task in &mut goal.workflow.tasks {
                    if affected.contains(&task.spec.id) {
                        task.requirements_revision = goal.requirements_revision;
                    }
                }
            }
            // A later explicit control must dominate an older natural-language
            // input, even when its model response arrives later.
            let control_source_revision = control_task
                .map(|index| goal.workflow.tasks[index].control_source_revision)
                .unwrap_or(goal.control_source_revision);
            let control_superseded =
                proposal.control.is_some() && control_source_revision > pending.operation.revision;
            let control_saved = proposal
                .control
                .as_ref()
                .cloned()
                .filter(|_| !control_superseded);
            let mut controlled_tasks = BTreeSet::new();
            if let Some(control) = &control_saved {
                if let Some(index) = control_task {
                    controlled_tasks.insert(goal.workflow.tasks[index].spec.id.clone());
                    loop {
                        let before = controlled_tasks.len();
                        for task in &goal.workflow.tasks {
                            if task
                                .spec
                                .dependencies
                                .iter()
                                .any(|id| controlled_tasks.contains(id))
                            {
                                controlled_tasks.insert(task.spec.id.clone());
                            }
                        }
                        if before == controlled_tasks.len() {
                            break;
                        }
                    }
                    controlled_tasks.retain(|id| {
                        goal.workflow
                            .tasks
                            .iter()
                            .find(|task| &task.spec.id == id)
                            .is_some_and(|task| {
                                task.control_source_revision <= pending.operation.revision
                            })
                    });
                    for task in &mut goal.workflow.tasks {
                        if controlled_tasks.contains(&task.spec.id) {
                            task.run_intent = control.intent;
                            task.control_source_revision = pending.operation.revision;
                        }
                    }
                } else {
                    goal.run_intent = control.intent;
                    goal.control_source_revision = pending.operation.revision;
                }
            }
            goal.inputs.after_revision = pending.operation.revision;
            goal.inputs.latest = Some(InputResult {
                input_id: pending.operation.id,
                source_requirements_revision: pending.requirements_revision,
                effective_requirements_revision: goal.requirements_revision,
                answer: proposal.answer,
                clarification: proposal.clarification,
                requirements_saved,
                affected_tasks: affected.into_iter().collect(),
                control_saved,
                controlled_tasks: controlled_tasks.into_iter().collect(),
                control_superseded,
                permission_response_saved: proposal.permission_response,
                classification: pending.classification,
                reasoning_request_id: reasoning_request_id.clone(),
            });
            goal.inputs.pending = None;
        }
    }
    Ok(())
}
