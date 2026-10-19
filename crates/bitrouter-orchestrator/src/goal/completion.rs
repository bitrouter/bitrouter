//! Versioned delivery, actual integration checks, and final completion authority.

use std::collections::BTreeSet;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::decision::PermissionState;
use super::verification::{CheckOutput, CheckSpec};
use super::workflow::{
    ArtifactRef, CheckRun, CheckState, Observation, Planning, ReviewState, StopState,
};
use super::{GoalError, GoalSnapshot, RunIntent, identifier, invalid, message};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Condition {
    pub id: String,
    pub description: String,
}

/// A planning proposal retains the full original objective for final review.
/// Conditions are conjunctive; an empty contract cannot complete a Goal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Specification {
    pub conditions: Vec<Condition>,
    pub checks: Vec<CheckSpec>,
    pub human_acceptance_required: bool,
}

impl Specification {
    pub fn validate(&self) -> Result<(), GoalError> {
        if self.conditions.is_empty() || self.conditions.len() > 32 || self.checks.len() > 16 {
            return Err(invalid(
                "completion requires 1–32 conditions and at most 16 checks",
            ));
        }
        let mut ids = BTreeSet::new();
        for condition in &self.conditions {
            identifier(&condition.id)?;
            message(&condition.description)?;
            if !ids.insert(&condition.id) {
                return Err(invalid("duplicate completion condition"));
            }
        }
        ids.clear();
        for check in &self.checks {
            check.validate()?;
            if !ids.insert(&check.id) {
                return Err(invalid("duplicate completion check"));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Progress {
    pub specification: Option<Specification>,
    pub attempt: Option<Attempt>,
    pub last_completed: Option<Completed>,
}

impl Progress {
    pub fn completed_for(&self, revision: u64) -> bool {
        self.last_completed
            .as_ref()
            .is_some_and(|record| record.requirements_revision == revision)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Source {
    pub task_id: String,
    pub task_revision: u64,
    pub execution_id: String,
    pub workspace: ArtifactRef,
    pub review: ArtifactRef,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attempt {
    pub id: String,
    pub requirements_revision: u64,
    pub sources: Vec<Source>,
    pub workspace: PathBuf,
    pub base_commit: Option<String>,
    pub delivery: Option<ArtifactRef>,
    pub checks: Vec<CheckRun>,
    pub assessment: Option<Assessment>,
    pub accepted: bool,
    pub state: State,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum State {
    Preparing,
    Checking,
    Evaluating,
    Ready,
    AwaitingAcceptance,
    Blocked { reason: String },
    Completed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Satisfaction {
    Met,
    Unmet,
    Unverified,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConditionResult {
    pub id: String,
    pub status: Satisfaction,
    pub reason: String,
    /// Identifies supplied material/check records, rather than invented logs.
    pub evidence: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Verdict {
    pub conditions: Vec<ConditionResult>,
    pub unmet_original_requirements: Vec<String>,
    pub constraints_preserved: bool,
    pub human_acceptance_required: bool,
    pub summary: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Assessment {
    pub report: ArtifactRef,
    pub request_id: String,
    pub verdict: Verdict,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Completed {
    pub attempt_id: String,
    pub requirements_revision: u64,
    pub delivery: ArtifactRef,
    pub assessment: Assessment,
    pub sources: Vec<Source>,
    pub checks: Vec<CheckRun>,
    pub human_accepted: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    Begin {
        attempt_id: String,
        sources: Vec<Source>,
        workspace: PathBuf,
    },
    Delivery {
        attempt_id: String,
        base_commit: String,
        artifact: ArtifactRef,
    },
    RequestCheck {
        attempt_id: String,
        check_id: String,
        run_id: String,
    },
    ObserveCheck {
        attempt_id: String,
        run_id: String,
        report: ArtifactRef,
        output: CheckOutput,
        workspace_after: ArtifactRef,
    },
    RequestAssessment {
        attempt_id: String,
    },
    ObserveAssessment {
        attempt_id: String,
        report: ArtifactRef,
        request_id: String,
        verdict: Verdict,
        workspace_after: ArtifactRef,
    },
    Block {
        attempt_id: String,
        reason: String,
    },
    Commit {
        attempt_id: String,
        workspace_after: ArtifactRef,
    },
}

/// Reconcile all relevant executions, not only the last successful reply.
pub fn sources(goal: &GoalSnapshot) -> Result<Vec<Source>, GoalError> {
    if !matches!(goal.workflow.planning, Planning::Planned { .. })
        || goal.workflow.tasks.is_empty()
        || goal.inputs.pending.is_some()
        || !goal.accounting.in_flight.is_empty()
        || goal.workflow.decisions.iter().any(|decision| {
            matches!(
                decision.state,
                PermissionState::Unknown { .. }
                    | PermissionState::Requested {
                        dispatch_requested: true,
                        ..
                    }
            )
        })
    {
        return Err(GoalError::Conflict);
    }
    let mut result = Vec::new();
    for task in &goal.workflow.tasks {
        for execution in &task.executions {
            let known_no_dispatch = !execution.dispatch_requested
                && matches!(
                    execution.observation,
                    Observation::Failed { run_id: None, .. }
                );
            let settled = execution.dispatch_requested
                && matches!(execution.stop, Some(StopState::Confirmed { .. }))
                && (execution.effects_reconciled()
                    || matches!(
                        execution.observation,
                        Observation::Candidate { .. } | Observation::Failed { .. }
                    ));
            if !(known_no_dispatch || settled)
                || matches!(execution.review, Some(ReviewState::Requested { .. }))
                || execution.checks.iter().any(|check| {
                    matches!(
                        check.state,
                        CheckState::Requested
                            | CheckState::Observed {
                                completed: false,
                                ..
                            }
                    )
                })
            {
                return Err(GoalError::Conflict);
            }
        }
        let execution = task.executions.last().ok_or(GoalError::Conflict)?;
        let handoff = execution.handoff.as_ref().ok_or(GoalError::Conflict)?;
        if handoff.task_revision != task.requirements_revision {
            return Err(GoalError::Conflict);
        }
        result.push(Source {
            task_id: task.spec.id.clone(),
            task_revision: task.requirements_revision,
            execution_id: execution.id.clone(),
            workspace: handoff.workspace.clone(),
            review: handoff.review.clone(),
        });
    }
    Ok(result)
}

fn current(goal: &GoalSnapshot, attempt: &Attempt) -> Result<(), GoalError> {
    if attempt.requirements_revision != goal.requirements_revision
        || sources(goal)? != attempt.sources
    {
        return Err(GoalError::Conflict);
    }
    Ok(())
}

fn checks_pass(specification: &Specification, attempt: &Attempt) -> bool {
    specification.checks.iter().all(|spec| {
        attempt.checks.iter().any(|check| {
            check.spec == *spec
                && Some(&check.workspace) == attempt.delivery.as_ref()
                && check.requirements_revision == attempt.requirements_revision
                && matches!(
                    check.state,
                    CheckState::Observed {
                        passed: true,
                        completed: true,
                        ..
                    }
                )
        })
    })
}

pub(super) fn accept(goal: &mut GoalSnapshot, id: &str, sha256: &str) -> Result<(), GoalError> {
    let attempt = goal
        .workflow
        .completion
        .attempt
        .as_ref()
        .ok_or(GoalError::Conflict)?;
    current(goal, attempt)?;
    if attempt.id != id
        || attempt.state != State::AwaitingAcceptance
        || !attempt
            .delivery
            .as_ref()
            .is_some_and(|artifact| artifact.sha256 == sha256)
    {
        return Err(GoalError::Conflict);
    }
    let attempt = goal
        .workflow
        .completion
        .attempt
        .as_mut()
        .ok_or(GoalError::Conflict)?;
    attempt.accepted = true;
    attempt.state = State::Ready;
    Ok(())
}

pub(super) fn apply(
    goal: &mut GoalSnapshot,
    revision: u64,
    event: &Event,
) -> Result<(), GoalError> {
    let specification = goal
        .workflow
        .completion
        .specification
        .as_ref()
        .ok_or(GoalError::Conflict)?
        .clone();
    if let Event::Begin {
        attempt_id,
        sources: proposed,
        workspace,
    } = event
    {
        identifier(attempt_id)?;
        if goal.run_intent != RunIntent::Run
            || !workspace.is_absolute()
            || goal.workflow.completion.completed_for(revision)
            || goal
                .workflow
                .completion
                .attempt
                .as_ref()
                .is_some_and(|attempt| attempt.requirements_revision >= revision)
            || sources(goal)? != *proposed
        {
            return Err(GoalError::Conflict);
        }
        goal.workflow.completion.attempt = Some(Attempt {
            id: attempt_id.clone(),
            requirements_revision: revision,
            sources: proposed.clone(),
            workspace: workspace.clone(),
            base_commit: None,
            delivery: None,
            checks: Vec::new(),
            assessment: None,
            accepted: false,
            state: State::Preparing,
        });
        return Ok(());
    }
    let attempt = goal
        .workflow
        .completion
        .attempt
        .as_ref()
        .ok_or(GoalError::Conflict)?
        .clone();
    if attempt.requirements_revision != revision {
        return Err(GoalError::Conflict);
    }
    if matches!(
        event,
        Event::Delivery { .. }
            | Event::RequestCheck { .. }
            | Event::RequestAssessment { .. }
            | Event::Commit { .. }
    ) {
        current(goal, &attempt)?;
    }
    if matches!(event, Event::Commit { .. })
        && goal
            .budget
            .limits
            .managed_token_estimates_micro_usd
            .is_some_and(|limit| {
                goal.budget
                    .reserved_micro_usd()
                    .ok()
                    .and_then(|reserved| {
                        goal.accounting
                            .estimated_model_tokens_micro_usd
                            .checked_add(reserved)
                    })
                    .is_none_or(|exposure| exposure > limit)
            })
    {
        return Err(GoalError::BudgetExceeded {
            message: "Current token-estimate exposure violates the delivery budget constraint"
                .into(),
        });
    }
    let next = goal
        .workflow
        .completion
        .attempt
        .as_mut()
        .ok_or(GoalError::Conflict)?;
    match event {
        Event::Begin { .. } => return Err(invalid("completion begin was not handled")),
        Event::Delivery {
            attempt_id,
            base_commit,
            artifact,
        } => {
            artifact.validate()?;
            if next.id != *attempt_id
                || next.state != State::Preparing
                || !(base_commit.len() == 40 || base_commit.len() == 64)
                || !base_commit.bytes().all(|byte| byte.is_ascii_hexdigit())
            {
                return Err(GoalError::Conflict);
            }
            next.base_commit = Some(base_commit.clone());
            next.delivery = Some(artifact.clone());
            next.state = State::Checking;
        }
        Event::RequestCheck {
            attempt_id,
            check_id,
            run_id,
        } => {
            identifier(run_id)?;
            let spec = specification
                .checks
                .iter()
                .find(|spec| spec.id == *check_id)
                .ok_or(GoalError::NotFound)?;
            if next.id != *attempt_id
                || next.state != State::Checking
                || goal.run_intent != RunIntent::Run
                || next
                    .checks
                    .iter()
                    .any(|check| check.spec.id == *check_id || check.id == *run_id)
            {
                return Err(GoalError::Conflict);
            }
            next.checks.push(CheckRun {
                id: run_id.clone(),
                spec: spec.clone(),
                requirements_revision: revision,
                task_revision: revision,
                workspace: next.delivery.clone().ok_or(GoalError::Conflict)?,
                state: CheckState::Requested,
            });
        }
        Event::ObserveCheck {
            attempt_id,
            run_id,
            report,
            output,
            workspace_after,
        } => {
            report.validate()?;
            workspace_after.validate()?;
            if next.id != *attempt_id || next.state != State::Checking {
                return Err(GoalError::Conflict);
            }
            let check = next
                .checks
                .iter_mut()
                .find(|check| check.id == *run_id)
                .ok_or(GoalError::NotFound)?;
            if check.state != CheckState::Requested {
                return Err(GoalError::Conflict);
            }
            check.state = CheckState::Observed {
                report: report.clone(),
                completed: output.completed,
                passed: output.supports(&check.spec) && check.workspace == *workspace_after,
            };
        }
        Event::RequestAssessment { attempt_id } => {
            if next.id != *attempt_id
                || next.state != State::Checking
                || goal.run_intent != RunIntent::Run
                || !checks_pass(&specification, next)
            {
                return Err(GoalError::Conflict);
            }
            next.state = State::Evaluating;
        }
        Event::ObserveAssessment {
            attempt_id,
            report,
            request_id,
            verdict,
            workspace_after,
        } => {
            report.validate()?;
            workspace_after.validate()?;
            identifier(request_id)?;
            message(&verdict.summary)?;
            if next.id != *attempt_id || next.state != State::Evaluating {
                return Err(GoalError::Conflict);
            }
            let expected: BTreeSet<_> = specification
                .conditions
                .iter()
                .map(|condition| condition.id.as_str())
                .collect();
            let actual: BTreeSet<_> = verdict
                .conditions
                .iter()
                .map(|condition| condition.id.as_str())
                .collect();
            if expected != actual || actual.len() != verdict.conditions.len() {
                return Err(invalid(
                    "assessment omitted or invented a completion condition",
                ));
            }
            let mut allowed = BTreeSet::from(["integrated_material".to_owned()]);
            allowed.extend(
                next.sources
                    .iter()
                    .map(|source| format!("task:{}", source.task_id)),
            );
            allowed.extend(
                next.checks
                    .iter()
                    .map(|check| format!("check:{}", check.spec.id)),
            );
            for condition in &verdict.conditions {
                message(&condition.reason)?;
                if condition.evidence.iter().any(|id| !allowed.contains(id))
                    || condition.status == Satisfaction::Met && condition.evidence.is_empty()
                {
                    return Err(invalid("assessment references absent evidence"));
                }
            }
            for requirement in &verdict.unmet_original_requirements {
                message(requirement)?;
            }
            let ready = verdict
                .conditions
                .iter()
                .all(|condition| condition.status == Satisfaction::Met)
                && verdict.unmet_original_requirements.is_empty()
                && verdict.constraints_preserved
                && Some(workspace_after) == next.delivery.as_ref();
            next.assessment = Some(Assessment {
                report: report.clone(),
                request_id: request_id.clone(),
                verdict: verdict.clone(),
            });
            next.state = if !ready {
                State::Blocked {
                    reason: verdict.summary.clone(),
                }
            } else if specification.human_acceptance_required || verdict.human_acceptance_required {
                State::AwaitingAcceptance
            } else {
                State::Ready
            };
        }
        Event::Block { attempt_id, reason } => {
            message(reason)?;
            if next.id != *attempt_id || next.state == State::Completed {
                return Err(GoalError::Conflict);
            }
            next.state = State::Blocked {
                reason: reason.clone(),
            };
        }
        Event::Commit {
            attempt_id,
            workspace_after,
        } => {
            workspace_after.validate()?;
            let assessment = next.assessment.clone().ok_or(GoalError::Conflict)?;
            if next.id != *attempt_id
                || next.state != State::Ready
                || Some(workspace_after) != next.delivery.as_ref()
                || !checks_pass(&specification, next)
                || (specification.human_acceptance_required
                    || assessment.verdict.human_acceptance_required)
                    && !next.accepted
            {
                return Err(GoalError::Conflict);
            }
            next.state = State::Completed;
            goal.workflow.completion.last_completed = Some(Completed {
                attempt_id: next.id.clone(),
                requirements_revision: revision,
                delivery: workspace_after.clone(),
                assessment,
                sources: next.sources.clone(),
                checks: next.checks.clone(),
                human_accepted: next.accepted,
            });
        }
    }
    Ok(())
}
