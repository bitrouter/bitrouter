//! One-time command permission bound to an admitted check and its exact artifact.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::CheckSpec;
use crate::goal::decision::{PermissionDecision, PermissionState};
use crate::goal::workflow::{ArtifactRef, CheckState};
use crate::goal::{GoalError, GoalSnapshot, RunIntent};

/// Immutable scope shown in a Goal command-check permission card.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckPermission {
    /// Exact executable recipe, including its time bound.
    pub specification: CheckSpec,
    /// Managed workspace where the command will run.
    pub workspace: PathBuf,
    /// Checked workspace content before execution.
    pub artifact: ArtifactRef,
    /// Coordinator that owns this live, non-replayable attempt.
    pub coordinator_id: String,
    /// Goal/task control version; pause/resume does not restore old consent.
    pub control_source_revision: u64,
}

impl CheckPermission {
    /// Validate against the admitted task or delivery check.
    ///
    /// # Errors
    /// Rejects a changed source, artifact, recipe, control version or owner.
    pub fn validate(
        &self,
        goal: &GoalSnapshot,
        decision: &PermissionDecision,
    ) -> Result<(), GoalError> {
        let (check, workspace, task_revision, control_revision, running) = if let Some(task) =
            goal.workflow.tasks.iter().find(|task| {
                task.executions
                    .last()
                    .is_some_and(|execution| execution.id == decision.execution_id)
            }) {
            let execution = task.executions.last().ok_or(GoalError::NotFound)?;
            if execution.task_revision != task.requirements_revision {
                return Err(GoalError::Conflict);
            }
            (
                execution
                    .checks
                    .iter()
                    .find(|check| check.id == decision.permission_id),
                &execution.workspace,
                task.requirements_revision,
                goal.control_source_revision
                    .max(task.control_source_revision),
                task.run_intent == RunIntent::Run,
            )
        } else {
            let attempt = goal
                .workflow
                .completion
                .attempt
                .as_ref()
                .ok_or(GoalError::NotFound)?;
            if attempt.id != decision.execution_id
                || attempt.state != crate::goal::completion::State::Checking
                || attempt.requirements_revision != goal.requirements_revision
            {
                return Err(GoalError::Conflict);
            }
            (
                attempt
                    .checks
                    .iter()
                    .find(|check| check.id == decision.permission_id),
                &attempt.workspace,
                0,
                goal.control_source_revision,
                true,
            )
        };
        let check = check.ok_or(GoalError::NotFound)?;
        if decision.run_id.is_some()
            || decision.requirements_revision != goal.requirements_revision
            || decision.task_revision != task_revision
            || check.requirements_revision != goal.requirements_revision
            || check.state != CheckState::Requested
            || check.spec != self.specification
            || check.workspace != self.artifact
            || workspace != &self.workspace
            || self.specification.command().is_none()
            || self.control_source_revision != control_revision
            || goal.run_intent != RunIntent::Run
            || !running
            || !goal
                .workflow
                .coordinator
                .as_ref()
                .is_some_and(|lease| lease.id == self.coordinator_id)
        {
            return Err(GoalError::Conflict);
        }
        Ok(())
    }
}

/// Consumed by one command invocation; not serializable or clonable.
pub struct CommandPermit {
    pub(super) scope: CheckPermission,
    pub(super) attempt_id: String,
}

/// Admit a live command only after its explicit, one-time choice was recorded.
/// The host must re-capture the workspace and compare the approved artifact
/// before calling this function, and retain the original check outcome. A saved
/// permission is never authority to replay an interrupted check.
///
/// # Errors
/// Rejects missing/denied/automatic consent, stale ownership and exhausted budget.
pub fn authorize(
    goal: &GoalSnapshot,
    decision_id: &str,
    now_ms: u64,
) -> Result<CommandPermit, GoalError> {
    let decision = goal
        .workflow
        .decisions
        .iter()
        .find(|decision| decision.id == decision_id)
        .ok_or(GoalError::NotFound)?;
    let scope = decision.check.as_deref().ok_or(GoalError::Conflict)?;
    scope.validate(goal, decision)?;
    if !matches!(&decision.state, PermissionState::Requested {
        option_id, automatic: false, dispatch_requested: true, ..
    } if option_id == "allow-once")
        || !goal
            .workflow
            .coordinator
            .as_ref()
            .is_some_and(|lease| lease.expires_at_ms > now_ms)
    {
        return Err(GoalError::Conflict);
    }
    goal.budget.admission(goal.created_at_ms, now_ms)?;
    Ok(CommandPermit {
        scope: scope.clone(),
        attempt_id: decision.permission_id.clone(),
    })
}

#[cfg(test)]
mod tests;
