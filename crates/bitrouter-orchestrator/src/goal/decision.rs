//! Permission requests retain their actual run, options and version bindings.

use serde::{Deserialize, Serialize};

use super::workflow::{ArtifactRef, Observation, Workflow};
use super::{GoalError, GoalSnapshot, RunIntent, identifier, invalid, message};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionOption {
    pub id: String,
    pub label: String,
    pub allows: bool,
    pub persistent: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionDecision {
    pub id: String,
    pub execution_id: String,
    pub run_id: String,
    pub permission_id: String,
    pub requirements_revision: u64,
    pub task_revision: u64,
    pub request: ArtifactRef,
    pub options: Vec<PermissionOption>,
    pub state: PermissionState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum PermissionState {
    Pending,
    Requested {
        operation_id: String,
        option_id: String,
        automatic: bool,
        dispatch_requested: bool,
    },
    Submitted {
        operation_id: String,
        option_id: String,
    },
    Unknown {
        operation_id: String,
        option_id: String,
        reason: String,
    },
    Obsolete {
        reason: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    RequestDelivery {
        decision_id: String,
        operation_id: String,
    },
    Observe {
        decision: PermissionDecision,
    },
    Respond {
        decision_id: String,
        operation_id: String,
        option_id: String,
        automatic: bool,
    },
    Result {
        decision_id: String,
        operation_id: String,
        submitted: bool,
        detail: String,
    },
    Obsolete {
        decision_id: String,
        reason: String,
    },
}

pub(super) fn validate_response(
    goal: &GoalSnapshot,
    decision_id: &str,
    option_id: &str,
    automatic: bool,
) -> Result<(), GoalError> {
    let decision = goal
        .workflow
        .decisions
        .iter()
        .find(|decision| decision.id == decision_id)
        .ok_or(GoalError::NotFound)?;
    let option = decision
        .options
        .iter()
        .find(|option| option.id == option_id)
        .ok_or_else(|| invalid("permission option was not offered"))?;
    let task = goal
        .workflow
        .tasks
        .iter()
        .find(|task| {
            task.executions
                .iter()
                .any(|execution| execution.id == decision.execution_id)
        })
        .ok_or(GoalError::NotFound)?;
    let execution = task
        .executions
        .iter()
        .find(|execution| execution.id == decision.execution_id)
        .ok_or(GoalError::NotFound)?;
    if decision.state != PermissionState::Pending
        || decision.requirements_revision != goal.requirements_revision
        || decision.task_revision != task.requirements_revision
        || execution.task_revision != task.requirements_revision
        || !matches!(&execution.observation, Observation::Running { run_id, .. } if run_id == &decision.run_id)
        || execution.stop.is_some()
        || option.allows && (goal.run_intent != RunIntent::Run || task.run_intent != RunIntent::Run)
        || automatic && (!option.allows || option.persistent)
    {
        return Err(GoalError::Conflict);
    }
    Ok(())
}

pub(super) fn apply(goal: &mut GoalSnapshot, event: &Event) -> Result<(), GoalError> {
    match event {
        Event::Observe { decision } => {
            identifier(&decision.id)?;
            identifier(&decision.permission_id)?;
            decision.request.validate()?;
            if decision.state != PermissionState::Pending
                || decision.options.is_empty()
                || decision.options.len() > 16
            {
                return Err(invalid("invalid permission observation"));
            }
            let mut ids = std::collections::BTreeSet::new();
            for option in &decision.options {
                message(&option.id)?;
                message(&option.label)?;
                if !ids.insert(&option.id) {
                    return Err(invalid("duplicate permission option"));
                }
            }
            if goal
                .workflow
                .decisions
                .iter()
                .any(|existing| existing.id == decision.id)
            {
                return Err(GoalError::Conflict);
            }
            let task = goal
                .workflow
                .tasks
                .iter()
                .find(|task| {
                    task.executions
                        .iter()
                        .any(|execution| execution.id == decision.execution_id)
                })
                .ok_or(GoalError::NotFound)?;
            let execution = task
                .executions
                .iter()
                .find(|execution| execution.id == decision.execution_id)
                .ok_or(GoalError::NotFound)?;
            if decision.requirements_revision != goal.requirements_revision
                || decision.task_revision != execution.task_revision
                || !matches!(&execution.observation, Observation::Running { run_id, .. } if run_id == &decision.run_id)
            {
                return Err(GoalError::Conflict);
            }
            // Closed decisions remain in immutable receipts; the live projection
            // only needs current and unresolved requests.
            goal.workflow.decisions.retain(|decision| {
                !matches!(
                    decision.state,
                    PermissionState::Submitted { .. } | PermissionState::Obsolete { .. }
                )
            });
            if goal.workflow.decisions.len() >= 64 {
                return Err(invalid("too many unresolved permission requests"));
            }
            goal.workflow.decisions.push(decision.clone());
        }
        Event::Respond {
            decision_id,
            operation_id,
            option_id,
            automatic,
        } => {
            identifier(operation_id)?;
            validate_response(goal, decision_id, option_id, *automatic)?;
            let decision = decision_mut(&mut goal.workflow, decision_id)?;
            decision.state = PermissionState::Requested {
                operation_id: operation_id.clone(),
                option_id: option_id.clone(),
                automatic: *automatic,
                dispatch_requested: false,
            };
        }
        Event::RequestDelivery {
            decision_id,
            operation_id,
        } => {
            let decision = goal
                .workflow
                .decisions
                .iter()
                .find(|decision| &decision.id == decision_id)
                .ok_or(GoalError::NotFound)?;
            let PermissionState::Requested {
                operation_id: expected,
                option_id,
                dispatch_requested,
                ..
            } = &decision.state
            else {
                return Err(GoalError::Conflict);
            };
            if expected != operation_id || *dispatch_requested {
                return Err(GoalError::Conflict);
            }
            let option_id = option_id.clone();
            let mut pending_view = goal.clone();
            decision_mut(&mut pending_view.workflow, decision_id)?.state = PermissionState::Pending;
            validate_response(&pending_view, decision_id, &option_id, false)?;
            let decision = decision_mut(&mut goal.workflow, decision_id)?;
            let PermissionState::Requested {
                dispatch_requested, ..
            } = &mut decision.state
            else {
                return Err(GoalError::Conflict);
            };
            *dispatch_requested = true;
        }
        Event::Result {
            decision_id,
            operation_id,
            submitted,
            detail,
        } => {
            message(detail)?;
            let decision = decision_mut(&mut goal.workflow, decision_id)?;
            let (expected, option_id) = match &decision.state {
                PermissionState::Requested {
                    operation_id,
                    option_id,
                    ..
                }
                | PermissionState::Unknown {
                    operation_id,
                    option_id,
                    ..
                } => (operation_id, option_id),
                _ => return Err(GoalError::Conflict),
            };
            if expected != operation_id {
                return Err(GoalError::Conflict);
            }
            decision.state = if *submitted {
                PermissionState::Submitted {
                    operation_id: operation_id.clone(),
                    option_id: option_id.clone(),
                }
            } else {
                PermissionState::Unknown {
                    operation_id: operation_id.clone(),
                    option_id: option_id.clone(),
                    reason: detail.clone(),
                }
            };
        }
        Event::Obsolete {
            decision_id,
            reason,
        } => {
            message(reason)?;
            let decision = decision_mut(&mut goal.workflow, decision_id)?;
            if decision.state != PermissionState::Pending {
                return Err(GoalError::Conflict);
            }
            decision.state = PermissionState::Obsolete {
                reason: reason.clone(),
            };
        }
    }
    Ok(())
}

fn decision_mut<'a>(
    workflow: &'a mut Workflow,
    id: &str,
) -> Result<&'a mut PermissionDecision, GoalError> {
    workflow
        .decisions
        .iter_mut()
        .find(|decision| decision.id == id)
        .ok_or(GoalError::NotFound)
}
