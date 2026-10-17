//! Durable workflow facts. A settled ACP turn is a candidate, never verification.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::{GoalError, GoalOperation, GoalService, OperationKind, RunIntent, identifier, invalid};

pub const MAX_TASKS: usize = 32;
pub const OWNER_LEASE_MS: u64 = 30_000;
pub const SUPERSEDED_STOP_GRACE_MS: u64 = 10_000;

/// Reconstruct the same owner admission after an interrupted cross-store link.
/// This is a single action key, not the key for every child admission.
pub fn owner_thread_key(goal_id: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut digest = Sha256::new();
    digest.update(b"bro-goal-owner-thread-v1");
    digest.update(goal_id.as_bytes());
    format!("goal-owner-{}", hex::encode(digest.finalize()))
}

pub fn input_thread_key(goal_id: &str) -> String {
    format!("input-{}", owner_thread_key(goal_id))
}

/// Local edits do not revoke an unrelated task's execution. Explicit controls,
/// replacement, task version changes and stopping do revoke that attempt.
pub fn worker_requested(goal: &super::GoalSnapshot, source: &crate::thread::GoalWork) -> bool {
    goal.id == source.goal_id
        && goal.workflow.owner_thread_id.as_ref() == Some(&source.owner_thread_id)
        && goal.run_intent == RunIntent::Run
        && goal.workflow.tasks.iter().any(|task| {
            task.spec.id == source.task_id
                && task.requirements_revision == source.task_revision
                && task.run_intent == RunIntent::Run
                && goal
                    .control_source_revision
                    .max(task.control_source_revision)
                    == source.control_source_revision
                && task.executions.last().is_some_and(|execution| {
                    execution.id == source.execution_id
                        && execution.task_revision == source.task_revision
                        && execution.dispatch_requested
                        && execution.stop.is_none()
                        && matches!(
                            execution.observation,
                            Observation::Requested | Observation::Running { .. }
                        )
                })
        })
}

/// A model proposal belongs to a still-pending domain action, not just a Goal
/// whose broad run intent happens to remain enabled.
pub fn generation_requested(
    goal: &super::GoalSnapshot,
    origin: &crate::turn::GenerationOrigin,
) -> bool {
    use crate::turn::GenerationPurpose as P;
    if goal.id != origin.goal_id || goal.requirements_revision != origin.requirements_revision {
        return false;
    }
    match origin.purpose {
        P::Planning => matches!(&goal.workflow.planning,
            Planning::Requested { attempt_id } if *attempt_id == origin.source_id),
        P::InputScope => goal.inputs.pending.as_ref().is_some_and(|pending| pending.operation.id == origin.source_id),
        P::Input => goal.inputs.pending.as_ref().is_some_and(|pending| pending.attempt_id == origin.source_id),
        P::Review => goal.workflow.tasks.iter().flat_map(|task| &task.executions).any(|execution|
            matches!(&execution.review, Some(ReviewState::Requested { attempt_id, .. }) if *attempt_id == origin.source_id)),
        P::Completion => goal.workflow.completion.attempt.as_ref().is_some_and(|attempt|
            attempt.id == origin.source_id && attempt.state == super::completion::State::Evaluating),
    }
}

/// Stable receipt key makes retirement recoverable and prevents identity reuse.
pub fn archive_operation_id(goal_id: &str, execution_id: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut digest = Sha256::new();
    digest.update(b"bro-goal-execution-archive-v1");
    digest.update((goal_id.len() as u64).to_le_bytes());
    digest.update(goal_id.as_bytes());
    digest.update(execution_id.as_bytes());
    format!("archive-{}", hex::encode(digest.finalize()))
}

/// Only superseded, reconciled records without active references may leave the head.
pub fn can_archive(goal: &super::GoalSnapshot, task: &Task, execution: &Execution) -> bool {
    execution.task_revision < task.requirements_revision
        && task.executions.last() != Some(execution)
        && execution.effects_reconciled()
        && !matches!(execution.review, Some(ReviewState::Requested { .. }))
        && !execution.checks.iter().any(|check| {
            matches!(
                check.state,
                CheckState::Requested
                    | CheckState::Observed {
                        completed: false,
                        ..
                    }
            )
        })
        && !goal.workflow.decisions.iter().any(|decision| {
            decision.execution_id == execution.id
                && !matches!(
                    decision.state,
                    super::decision::PermissionState::Submitted { .. }
                        | super::decision::PermissionState::Obsolete { .. }
                )
        })
        && !goal
            .workflow
            .completion
            .attempt
            .as_ref()
            .is_some_and(|attempt| {
                attempt
                    .sources
                    .iter()
                    .any(|source| source.execution_id == execution.id)
            })
        && !goal
            .workflow
            .completion
            .last_completed
            .as_ref()
            .is_some_and(|completed| {
                completed
                    .sources
                    .iter()
                    .any(|source| source.execution_id == execution.id)
            })
        && !goal
            .accounting
            .in_flight
            .iter()
            .any(|work| work.requirements_revision == execution.requirements_revision)
        && goal.inputs.pending.is_none()
        && !goal.inputs.unresolved_inputs().iter().any(|input| {
            input.source_requirements_revision == execution.requirements_revision
                || input.permission_decision.as_ref().is_some_and(|id| {
                    goal.workflow
                        .decisions
                        .iter()
                        .any(|decision| decision.id == *id && decision.execution_id == execution.id)
                })
        })
}

/// Content identity, rather than an inline transcript in every Goal snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactRef {
    pub path: PathBuf,
    pub sha256: String,
    pub bytes: u64,
}

impl ArtifactRef {
    pub fn validate(&self) -> Result<(), GoalError> {
        if !self.path.is_absolute()
            || self.sha256.len() != 64
            || !self.sha256.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(invalid("invalid artifact identity"));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Handoff {
    pub attempt_id: String,
    pub workspace: ArtifactRef,
    pub review: ArtifactRef,
    pub review_request_id: String,
    pub candidate_sha256: String,
    pub requirements_revision: u64,
    pub task_revision: u64,
}

pub struct WorkflowActor<'a> {
    pub owner: &'a str,
    pub coordinator_id: &'a str,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CoordinatorLease {
    pub id: String,
    pub expires_at_ms: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Workflow {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_thread_id: Option<String>,
    /// An accepted Thread identity. Missing on legacy records; never inferred
    /// from an ACP run or used as evidence that execution has begun.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_thread_id: Option<String>,
    #[serde(default)]
    pub completion: super::completion::Progress,
    #[serde(default)]
    pub coordinator: Option<CoordinatorLease>,
    pub planning: Planning,
    #[serde(default)]
    pub planning_requirements_revision: u64,
    pub tasks: Vec<Task>,
    #[serde(default)]
    pub decisions: Vec<super::decision::PermissionDecision>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Planning {
    #[default]
    Unplanned,
    Requested {
        attempt_id: String,
    },
    Planned {
        attempt_id: String,
    },
    Blocked {
        attempt_id: String,
        reason: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Plan {
    pub tasks: Vec<TaskSpec>,
    pub completion: super::completion::Specification,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskSpec {
    pub id: String,
    pub title: String,
    pub instruction: String,
    /// No external harness selects BRO's built-in executor. Existing ACP IDs
    /// retain their serialized spelling and operation fingerprints.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness: Option<String>,
    pub dependencies: Vec<String>,
    #[serde(default)]
    pub checks: Vec<super::verification::CheckSpec>,
    #[serde(default, skip_serializing_if = "super::resources::Selection::is_empty")]
    pub resources: super::resources::Selection,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Task {
    pub spec: TaskSpec,
    /// Declared worker of archives written before attempts recorded their own
    /// selection. Set only on a later worker change; it proves no execution.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub legacy_worker: Option<Worker>,
    pub requirements_revision: u64,
    pub executions: Vec<Execution>,
    /// Full retired records remain in immutable, bounded Goal history pages.
    #[serde(default, skip_serializing_if = "no_archives")]
    pub archived_executions: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest_archive: Option<ArtifactRef>,
    #[serde(default)]
    pub run_intent: RunIntent,
    #[serde(default)]
    pub control_source_revision: u64,
}

/// Pure execution admission shared by scheduling, projection and durable commit.
/// Goal authorization, dependencies, budget and CAS still require fresh checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutionAdmission {
    Ready,
    Blocked,
    RetryLimit,
}

impl Task {
    /// One source rule for preparation and durable admission. Preparation-only
    /// retries keep the original handoff instead of silently dropping results.
    pub fn worker_handoff(
        &self,
        before: usize,
        goal_control_source: u64,
    ) -> Result<Option<super::resources::WorkerHandoff>, GoalError> {
        let previous = self
            .executions
            .get(..before)
            .ok_or_else(|| invalid("worker handoff cutoff is outside the task"))?;
        for execution in previous.iter().rev() {
            if self.execution_harness(execution) == self.spec.harness.as_deref() {
                if execution.task_revision == self.requirements_revision {
                    if let Some(handoff) = execution
                        .resources
                        .as_ref()
                        .and_then(|resources| resources.worker_handoff.as_ref())
                    {
                        return Ok(Some(handoff.clone()));
                    }
                    if !execution.dispatch_requested
                        || matches!(execution.observation, Observation::NotAdmitted { .. })
                    {
                        continue;
                    }
                }
                return Ok(None);
            }
            if !execution.dispatch_requested
                || matches!(execution.observation, Observation::NotAdmitted { .. })
            {
                continue;
            }
            if let Some(LocalReconciliation::Verified {
                task_revision,
                control_source_revision,
                workspace,
                transcript,
            }) = &execution.reconciliation
                && *task_revision == self.requirements_revision
                && *control_source_revision == goal_control_source.max(self.control_source_revision)
                && matches!(execution.stop, Some(StopState::Confirmed { .. }))
            {
                return Ok(Some(super::resources::WorkerHandoff {
                    execution_id: execution.id.clone(),
                    workspace: workspace.clone(),
                    transcript: transcript.clone(),
                }));
            }
            return Err(invalid(
                "worker replacement lacks current stopped-effect material",
            ));
        }
        Ok(None)
    }

    pub fn execution_harness<'a>(&'a self, execution: &'a Execution) -> Option<&'a str> {
        execution
            .worker
            .as_ref()
            .or(self.legacy_worker.as_ref())
            .map_or(self.spec.harness.as_deref(), Worker::harness)
    }

    pub fn execution_admission(
        &self,
        goal_revision: u64,
        goal_control_source: u64,
    ) -> ExecutionAdmission {
        let previous_ready = self.executions.last().is_none_or(|execution| {
            matches!(execution.observation, Observation::NotAdmitted { .. })
                || (!execution.dispatch_requested
                    && matches!(
                        execution.observation,
                        Observation::Failed { run_id: None, .. }
                    ))
                || execution.replacement_ready(
                    self.requirements_revision,
                    goal_control_source.max(self.control_source_revision),
                )
        });
        if !previous_ready {
            return ExecutionAdmission::Blocked;
        }
        if self.executions.len() >= 16
            || self
                .executions
                .iter()
                .filter(|execution| execution.task_revision == self.requirements_revision)
                .count()
                >= 3
        {
            return ExecutionAdmission::RetryLimit;
        }
        if self.run_intent != RunIntent::Run || self.requirements_revision > goal_revision {
            return ExecutionAdmission::Blocked;
        }
        ExecutionAdmission::Ready
    }
}

fn no_archives(value: &u64) -> bool {
    *value == 0
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Execution {
    pub id: String,
    pub workspace: PathBuf,
    pub observation: Observation,
    /// Accepted execution identity, distinct from any ACP session/run ID.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread: Option<ThreadExecution>,
    /// Requested executor at attempt creation, distinct from its accepted
    /// Thread/Turn and actual ACP session or model observations.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worker: Option<Worker>,
    pub requirements_revision: u64,
    pub task_revision: u64,
    #[serde(default, skip_serializing_if = "zero_revision")]
    pub control_source_revision: u64,
    #[serde(default)]
    pub dispatch_requested: bool,
    /// This attempt uses the common ThreadService admission path. Legacy
    /// supervisor requests, including choices frozen by later edits, omit it.
    #[serde(default, skip_serializing_if = "is_false")]
    pub managed_dispatch: bool,
    #[serde(default)]
    pub checks: Vec<CheckRun>,
    #[serde(default)]
    pub stop: Option<StopState>,
    /// A bounded pre-actuation wait; no stop RPC has been admitted by this fact.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_boundary_deadline_ms: Option<u64>,
    #[serde(default)]
    pub base_commit: Option<String>,
    #[serde(default)]
    pub handoff: Option<Handoff>,
    #[serde(default)]
    pub review: Option<ReviewState>,
    #[serde(default)]
    pub reconciliation: Option<LocalReconciliation>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resources: Option<super::resources::Prepared>,
    /// Acknowledged BRO session route; not proof of an upstream provider model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confirmed_model_route: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadExecution {
    pub thread_id: String,
    pub turn_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Worker {
    BuiltIn,
    Acp { harness: String },
}

impl Worker {
    pub fn from_harness(harness: Option<String>) -> Self {
        match harness {
            Some(harness) => Self::Acp { harness },
            None => Self::BuiltIn,
        }
    }

    pub fn harness(&self) -> Option<&str> {
        match self {
            Self::BuiltIn => None,
            Self::Acp { harness } => Some(harness),
        }
    }

    pub fn validate(&self) -> Result<(), GoalError> {
        match self.harness() {
            Some(harness) => identifier(harness),
            None => Ok(()),
        }
    }
}

fn zero_revision(revision: &u64) -> bool {
    *revision == 0
}

fn is_false(value: &bool) -> bool {
    !value
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum LocalReconciliation {
    Verified {
        task_revision: u64,
        control_source_revision: u64,
        workspace: ArtifactRef,
        transcript: ArtifactRef,
    },
    Blocked {
        task_revision: u64,
        control_source_revision: u64,
        reason: String,
    },
}

impl Execution {
    /// Known, current candidate failures can be repaired after effect
    /// reconciliation. Interrupted checks/reviews never grant retry authority.
    pub fn repairable_failure(&self, goal_revision: u64, task_revision: u64) -> bool {
        self.task_revision == task_revision
            && self.handoff.is_none()
            && matches!(self.observation, Observation::Candidate { .. })
            && !matches!(self.review, Some(ReviewState::Requested { .. }))
            && self.checks.iter().all(|check| {
                matches!(
                    check.state,
                    CheckState::Observed {
                        completed: true,
                        ..
                    }
                )
            })
            && (matches!(&self.review, Some(ReviewState::Rejected {
                requirements_revision, task_revision: reviewed_task, report: Some(_), ..
            }) if *requirements_revision == goal_revision && *reviewed_task == task_revision)
                || self.checks.iter().any(|check| {
                    check.task_revision == task_revision
                        && matches!(
                            check.state,
                            CheckState::Observed {
                                completed: true,
                                passed: false,
                                ..
                            }
                        )
                }))
    }

    pub fn effects_reconciled(&self) -> bool {
        matches!(self.stop, Some(StopState::Confirmed { .. }))
            && matches!(
                self.reconciliation,
                Some(LocalReconciliation::Verified { .. })
            )
    }

    pub fn replacement_ready(&self, task_revision: u64, control_source_revision: u64) -> bool {
        self.effects_reconciled()
            && matches!(&self.reconciliation, Some(LocalReconciliation::Verified {
                task_revision: verified_task, control_source_revision: verified_control, ..
            }) if *verified_task == task_revision && *verified_control == control_source_revision)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckRun {
    pub id: String,
    pub spec: super::verification::CheckSpec,
    pub requirements_revision: u64,
    pub task_revision: u64,
    pub workspace: ArtifactRef,
    pub state: CheckState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum CheckState {
    Requested,
    Observed {
        report: ArtifactRef,
        passed: bool,
        completed: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ReviewState {
    Approved {
        attempt_id: String,
        requirements_revision: u64,
        task_revision: u64,
    },
    Requested {
        attempt_id: String,
        workspace: ArtifactRef,
        requirements_revision: u64,
        task_revision: u64,
    },
    Rejected {
        attempt_id: String,
        reason: String,
        report: Option<ArtifactRef>,
        requirements_revision: u64,
        task_revision: u64,
    },
}

impl ReviewState {
    /// A fresh Goal context can require a new assessment of retained task
    /// evidence. An approved unchanged task and an in-flight request remain
    /// protected; the original request/result stays in immutable history.
    pub fn can_reopen(&self, requirements_revision: u64, task_revision: u64) -> bool {
        match self {
            Self::Requested { .. } => false,
            Self::Approved {
                task_revision: reviewed,
                ..
            } => *reviewed < task_revision,
            Self::Rejected {
                requirements_revision: reviewed_goal,
                task_revision: reviewed_task,
                ..
            } => *reviewed_task < task_revision || *reviewed_goal < requirements_revision,
        }
    }

    pub fn task_revision(&self) -> u64 {
        match self {
            Self::Approved { task_revision, .. }
            | Self::Requested { task_revision, .. }
            | Self::Rejected { task_revision, .. } => *task_revision,
        }
    }

    pub fn requirements_revision(&self) -> u64 {
        match self {
            Self::Approved {
                requirements_revision,
                ..
            }
            | Self::Requested {
                requirements_revision,
                ..
            }
            | Self::Rejected {
                requirements_revision,
                ..
            } => *requirements_revision,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum StopState {
    Requested {
        operation_id: String,
    },
    Confirmed {
        operation_id: String,
        detail: String,
    },
    Unknown {
        operation_id: String,
        detail: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Observation {
    Requested,
    /// The source was revoked and original-action reconciliation found no
    /// accepted Turn. The earlier dispatch intent remains in history.
    NotAdmitted {
        reason: String,
    },
    Running {
        run_id: Option<String>,
        native_session_id: Option<String>,
    },
    Candidate {
        run_id: Option<String>,
        native_session_id: Option<String>,
        report: ArtifactRef,
    },
    Failed {
        run_id: Option<String>,
        reason: String,
    },
    Unknown {
        run_id: Option<String>,
        reason: String,
    },
}

/// Internal service mutations; not a client-authorized plan or effect API.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "change", rename_all = "snake_case")]
pub enum Change {
    BindWorkerTurn {
        execution_id: String,
        thread: ThreadExecution,
    },
    BindInputThread {
        thread_id: String,
    },
    BindModelTurn {
        origin: crate::turn::GenerationOrigin,
        thread_id: String,
        turn_id: String,
    },
    BindOwnerThread {
        thread_id: String,
    },
    AwaitStopBoundary {
        execution_id: String,
        deadline_ms: u64,
    },
    ArchiveExecution {
        task_id: String,
        execution: Box<Execution>,
        decisions: Vec<super::decision::PermissionDecision>,
        record: ArtifactRef,
    },
    Budget {
        event: super::budget::Event,
    },
    Completion {
        event: super::completion::Event,
    },
    Accounting {
        event: super::accounting::Event,
    },
    RequestCheck {
        execution_id: String,
        attempt_id: String,
        check_id: String,
        workspace: ArtifactRef,
    },
    ObserveCheck {
        execution_id: String,
        attempt_id: String,
        report: ArtifactRef,
        output: super::verification::CheckOutput,
        workspace_after: ArtifactRef,
    },
    RearmPlan,
    Decision {
        event: super::decision::Event,
    },
    RequestDispatch {
        execution_id: String,
    },
    RequestWorkerDispatch {
        execution_id: String,
    },
    RejectWorkerDispatch {
        execution_id: String,
        reason: String,
    },
    Input {
        event: super::input::Event,
    },
    RequestReview {
        execution_id: String,
        attempt_id: String,
        workspace: ArtifactRef,
    },
    RejectReview {
        execution_id: String,
        attempt_id: String,
        reason: String,
        report: Option<ArtifactRef>,
    },
    RegisterWorkspace {
        execution_id: String,
        base_commit: String,
    },
    PrepareResources {
        execution_id: String,
        resources: super::resources::Prepared,
    },
    ConfirmModelRoute {
        execution_id: String,
        run_id: String,
        route: String,
    },
    ApproveHandoff {
        execution_id: String,
        handoff: Handoff,
    },
    ClaimCoordinator {
        id: String,
        expires_at_ms: u64,
    },
    RequestStop {
        execution_id: String,
        operation_id: String,
    },
    ObserveStop {
        execution_id: String,
        operation_id: String,
        confirmed: bool,
        detail: String,
    },
    RequestPlan {
        attempt_id: String,
    },
    InstallPlan {
        attempt_id: String,
        plan: Plan,
        available_harnesses: BTreeSet<String>,
    },
    BlockPlan {
        attempt_id: String,
        reason: String,
    },
    RequestExecution {
        task_id: String,
        execution_id: String,
        workspace: PathBuf,
    },
    ReconcileStoppedExecution {
        execution_id: String,
        reconciliation: LocalReconciliation,
    },
    ObserveExecution {
        execution_id: String,
        observation: Observation,
    },
}

impl Plan {
    pub fn validate(&self, available_harnesses: &BTreeSet<String>) -> Result<(), GoalError> {
        self.completion.validate()?;
        if self.tasks.is_empty() || self.tasks.len() > MAX_TASKS {
            return Err(invalid("plan must contain 1–32 tasks"));
        }
        let mut dependencies = BTreeMap::new();
        for task in &self.tasks {
            task.resources.validate()?;
            let mut check_ids = BTreeSet::new();
            if task.checks.len() > 16 {
                return Err(invalid("task has too many required checks"));
            }
            for check in &task.checks {
                check.validate()?;
                if !check_ids.insert(&check.id) {
                    return Err(invalid("duplicate task check"));
                }
            }
            identifier(&task.id)?;
            if let Some(harness) = &task.harness {
                identifier(harness)?;
            }
            super::message(&task.instruction)?;
            if task.instruction.len() > 16 * 1024 {
                return Err(invalid("task instruction exceeds 16 KiB"));
            }
            if task.title.trim().is_empty()
                || task.title.len() > 256
                || task.title.chars().any(char::is_control)
            {
                return Err(invalid(
                    "task title must contain 1–256 bytes without control characters",
                ));
            }
            if task
                .harness
                .as_ref()
                .is_some_and(|harness| !available_harnesses.contains(harness))
            {
                return Err(invalid("plan selected an unavailable harness"));
            }
            if dependencies
                .insert(task.id.as_str(), &task.dependencies)
                .is_some()
            {
                return Err(invalid("task identities must be unique"));
            }
        }
        for task in &self.tasks {
            let unique: BTreeSet<_> = task.dependencies.iter().collect();
            if unique.len() != task.dependencies.len()
                || task
                    .dependencies
                    .iter()
                    .any(|id| id == &task.id || !dependencies.contains_key(id.as_str()))
            {
                return Err(invalid("invalid or duplicated task dependency"));
            }
        }
        let mut visited = BTreeSet::new();
        loop {
            let before = visited.len();
            for task in &self.tasks {
                if task
                    .dependencies
                    .iter()
                    .all(|id| visited.contains(id.as_str()))
                {
                    visited.insert(task.id.as_str());
                }
            }
            if visited.len() == self.tasks.len() {
                return Ok(());
            }
            if visited.len() == before {
                return Err(invalid("task dependencies contain a cycle"));
            }
        }
    }
}

impl GoalService {
    /// Retrieve bounded original input receipts for this Goal only. A lexical
    /// reference prioritizes context; it does not grant permission or effects.
    pub async fn clarification_context(
        &self,
        owner: &str,
        goal_id: &str,
        reference: Option<&str>,
    ) -> Result<Vec<super::input::ClarificationContext>, GoalError> {
        identifier(owner)?;
        identifier(goal_id)?;
        let goal = self.store.get(owner, goal_id).await?;
        let mut unresolved = goal.inputs.unresolved_inputs();
        if let Some(id) = reference {
            identifier(id)?;
            let index = unresolved
                .iter()
                .position(|input| input.input_id == id)
                .ok_or(GoalError::NotFound)?;
            unresolved.swap(0, index);
        }
        let mut contexts = Vec::new();
        let mut bytes = 0_usize;
        for mut input in unresolved.into_iter().take(super::MAX_PAGE_ITEMS) {
            let original = self
                .store
                .operation(owner, &input.input_id)
                .await?
                .ok_or(GoalError::NotFound)?;
            let OperationKind::Input {
                text,
                task_reference,
            } = &original.kind
            else {
                return Err(invalid("clarification did not reference an original input"));
            };
            if original.goal_id != goal_id {
                return Err(GoalError::NotFound);
            }
            input.task_reference = task_reference.clone();
            input.permission_decision = super::input::permission_reference(text)?
                .map(|reference| reference.decision_id.to_owned());
            let context = super::input::ClarificationContext {
                unresolved: input,
                original_input: original,
            };
            let size = serde_json::to_vec(&context)
                .map_err(super::storage_error)?
                .len();
            if bytes.saturating_add(size) > 256 * 1024 {
                break;
            }
            bytes += size;
            contexts.push(context);
        }
        Ok(contexts)
    }

    /// Inputs are visited in acceptance order, independently of worker turns.
    pub async fn next_input(
        &self,
        owner: &str,
        goal_id: &str,
    ) -> Result<Option<GoalOperation>, GoalError> {
        identifier(owner)?;
        identifier(goal_id)?;
        let goal = self.store.get(owner, goal_id).await?;
        if goal.inputs.pending.is_some() {
            return Ok(None);
        }
        let mut after = goal.inputs.after_revision;
        let mut cutoff = None;
        loop {
            let page = self
                .store
                .history(owner, goal_id, after, cutoff, super::MAX_PAGE_ITEMS)
                .await?;
            if let Some(operation) = page
                .operations
                .into_iter()
                .find(|operation| matches!(operation.kind, OperationKind::Input { .. }))
            {
                return Ok(Some(operation));
            }
            let Some(next) = page.next_after else {
                return Ok(None);
            };
            after = next;
            cutoff = Some(page.cutoff);
        }
    }
    /// CAS a runtime fact against the latest head. Model work stays outside the
    /// transaction; requirements and transition predicates are rechecked here.
    pub async fn workflow_change(
        &self,
        actor: WorkflowActor<'_>,
        goal_id: &str,
        operation_id: &str,
        requirements_revision: u64,
        change: Change,
        now_ms: u64,
    ) -> Result<GoalOperation, GoalError> {
        use sha2::{Digest, Sha256};
        let owner = actor.owner;
        identifier(owner)?;
        identifier(actor.coordinator_id)?;
        identifier(goal_id)?;
        identifier(operation_id)?;
        let fingerprint = hex::encode(Sha256::digest(
            serde_json::to_vec(&(goal_id, requirements_revision, &change))
                .map_err(super::storage_error)?,
        ));
        if let Some(receipt) = self.store.operation(owner, operation_id).await? {
            return super::replay(receipt, &fingerprint);
        }
        for _ in 0..4 {
            let mut goal = self.store.get(owner, goal_id).await?;
            if let Change::ArchiveExecution { execution, .. } = &change
                && operation_id != archive_operation_id(goal_id, &execution.id)
            {
                return Err(invalid(
                    "execution archive requires its stable receipt identity",
                ));
            }
            if let Change::RequestExecution { execution_id, .. } = &change
                && self
                    .store
                    .operation(owner, &archive_operation_id(goal_id, execution_id))
                    .await?
                    .is_some()
            {
                return Err(invalid("an archived execution identity cannot be reused"));
            }
            if matches!(
                &change,
                Change::Input {
                    event: super::input::Event::Finish { .. }
                }
            ) {
                // Recover scope from immutable receipts for legacy latest-only
                // questions before the domain validates their resolution.
                let mut unresolved = goal.inputs.unresolved_inputs();
                for input in &mut unresolved {
                    if goal
                        .inputs
                        .unresolved
                        .iter()
                        .any(|stored| stored.input_id == input.input_id)
                    {
                        continue;
                    }
                    let original = self
                        .store
                        .operation(owner, &input.input_id)
                        .await?
                        .ok_or(GoalError::NotFound)?;
                    let OperationKind::Input {
                        text,
                        task_reference,
                    } = original.kind
                    else {
                        return Err(invalid("clarification did not reference an original input"));
                    };
                    if original.goal_id != goal_id {
                        return Err(GoalError::NotFound);
                    }
                    input.task_reference = task_reference;
                    input.permission_decision = super::input::permission_reference(&text)?
                        .map(|reference| reference.decision_id.to_owned());
                }
                goal.inputs.unresolved = unresolved;
            }
            if matches!(
                &change,
                Change::Completion {
                    event: super::completion::Event::Begin { .. }
                        | super::completion::Event::Commit { .. }
                }
            ) && self.next_input(owner, goal_id).await?.is_some()
            {
                return Err(GoalError::Conflict);
            }
            if let Change::Input {
                event: super::input::Event::Begin { operation, .. },
            } = &change
                && self.next_input(owner, goal_id).await?.as_ref() != Some(operation.as_ref())
            {
                return Err(GoalError::Conflict);
            }
            match &change {
                Change::ClaimCoordinator { id, expires_at_ms } => {
                    if id != actor.coordinator_id
                        || *expires_at_ms <= now_ms
                        || expires_at_ms.saturating_sub(now_ms) > OWNER_LEASE_MS
                    {
                        return Err(invalid("invalid coordinator lease"));
                    }
                    if goal
                        .workflow
                        .coordinator
                        .as_ref()
                        .is_some_and(|lease| lease.id != *id && lease.expires_at_ms > now_ms)
                    {
                        return Err(GoalError::Conflict);
                    }
                }
                _ => {
                    if !goal.workflow.coordinator.as_ref().is_some_and(|lease| {
                        lease.id == actor.coordinator_id && lease.expires_at_ms > now_ms
                    }) {
                        return Err(GoalError::Conflict);
                    }
                }
            }
            if goal.requirements_revision != requirements_revision
                && !matches!(
                    &change,
                    Change::Decision {
                        event: super::decision::Event::Result { .. }
                            | super::decision::Event::Obsolete { .. }
                    } | Change::Accounting {
                        event: super::accounting::Event::Observe { .. }
                            | super::accounting::Event::Begin {
                                work: super::accounting::Work {
                                    source: Some(_),
                                    ..
                                }
                            }
                    } | Change::Completion {
                        event: super::completion::Event::ObserveCheck { .. }
                            | super::completion::Event::ObserveAssessment { .. }
                            | super::completion::Event::Block { .. }
                    } | Change::ObserveCheck { .. }
                        | Change::BindWorkerTurn { .. }
                        | Change::RejectWorkerDispatch { .. }
                        | Change::BlockPlan { .. }
                        | Change::ObserveExecution { .. }
                        | Change::ConfirmModelRoute { .. }
                        | Change::RejectReview { .. }
                        | Change::ObserveStop { .. }
                        | Change::Input {
                            event: super::input::Event::Classified { .. }
                                | super::input::Event::Finish { .. }
                        }
                )
            {
                return Err(GoalError::Conflict);
            }
            apply(&mut goal, requirements_revision, &change, now_ms)?;
            let expected = goal.revision;
            goal.revision = expected
                .checked_add(1)
                .ok_or_else(|| invalid("revision exhausted"))?;
            goal.updated_at_ms = now_ms;
            let operation = GoalOperation {
                id: operation_id.into(),
                fingerprint: fingerprint.clone(),
                goal_id: goal_id.into(),
                revision: goal.revision,
                requirements_revision: goal.requirements_revision,
                accepted_at_ms: now_ms,
                kind: OperationKind::Workflow {
                    change: Box::new(change.clone()),
                },
            };
            match self.store.commit(Some(expected), goal, operation).await {
                Err(GoalError::Conflict) => {
                    if let Some(receipt) = self.store.operation(owner, operation_id).await? {
                        return super::replay(receipt, &fingerprint);
                    }
                }
                result => return result,
            }
        }
        Err(GoalError::Conflict)
    }
}

fn apply(
    goal: &mut super::GoalSnapshot,
    revision: u64,
    change: &Change,
    now_ms: u64,
) -> Result<(), GoalError> {
    if let Change::BindModelTurn { origin, .. } = change
        && !generation_requested(goal, origin)
    {
        return Err(GoalError::Conflict);
    }
    if let Change::ArchiveExecution {
        task_id,
        execution,
        decisions,
        record,
    } = change
    {
        identifier(task_id)?;
        identifier(&execution.id)?;
        record.validate()?;
        if record.bytes == 0 || record.bytes > super::MAX_RECORD_BYTES as u64 {
            return Err(invalid("invalid execution archive record bound"));
        }
        let task = goal
            .workflow
            .tasks
            .iter()
            .find(|task| &task.spec.id == task_id)
            .ok_or(GoalError::NotFound)?;
        if !task
            .executions
            .iter()
            .any(|stored| stored == execution.as_ref())
            || !can_archive(goal, task, execution)
            || goal
                .workflow
                .decisions
                .iter()
                .filter(|decision| decision.execution_id == execution.id)
                .ne(decisions.iter())
        {
            return Err(GoalError::Conflict);
        }
        let task = goal
            .workflow
            .tasks
            .iter_mut()
            .find(|task| &task.spec.id == task_id)
            .ok_or(GoalError::NotFound)?;
        task.archived_executions = task
            .archived_executions
            .checked_add(1)
            .ok_or_else(|| invalid("execution archive count exhausted"))?;
        task.executions.retain(|stored| stored.id != execution.id);
        task.latest_archive = Some(record.clone());
        goal.workflow
            .decisions
            .retain(|decision| decision.execution_id != execution.id);
        return Ok(());
    }
    if let Change::Budget { event } = change {
        return super::budget::apply(goal, event, now_ms);
    }
    if let Change::Completion { event } = change {
        if matches!(
            event,
            super::completion::Event::Begin { .. }
                | super::completion::Event::RequestCheck { .. }
                | super::completion::Event::RequestAssessment { .. }
        ) {
            goal.budget.admission(goal.created_at_ms, now_ms)?;
        }
        return super::completion::apply(goal, revision, event);
    }
    if let Change::Accounting { event } = change {
        return super::accounting::apply(goal, event, now_ms);
    }
    if let Change::Input { event } = change {
        return super::input::apply(goal, event, now_ms);
    }
    if let Change::Decision { event } = change {
        if let super::decision::Event::Respond {
            decision_id,
            option_id,
            ..
        } = event
            && goal
                .workflow
                .decisions
                .iter()
                .find(|decision| &decision.id == decision_id)
                .and_then(|decision| {
                    decision
                        .options
                        .iter()
                        .find(|option| &option.id == option_id)
                })
                .is_some_and(|option| option.allows)
        {
            goal.budget.admission(goal.created_at_ms, now_ms)?;
        }
        if let super::decision::Event::RequestDelivery { decision_id, .. } = event
            && goal
                .workflow
                .decisions
                .iter()
                .find(|decision| &decision.id == decision_id)
                .is_some_and(|decision| match &decision.state {
                    super::decision::PermissionState::Requested { option_id, .. } => decision
                        .options
                        .iter()
                        .find(|option| &option.id == option_id)
                        .is_some_and(|option| option.allows),
                    _ => false,
                })
        {
            goal.budget.admission(goal.created_at_ms, now_ms)?;
        }
        return super::decision::apply(goal, event);
    }
    let budget_stopped = goal.budget.admission(goal.created_at_ms, now_ms).is_err();
    let intent = goal.run_intent;
    let goal_control_source = goal.control_source_revision;
    if matches!(
        change,
        Change::RequestPlan { .. }
            | Change::RequestExecution { .. }
            | Change::RequestDispatch { .. }
            | Change::RequestWorkerDispatch { .. }
            | Change::PrepareResources { .. }
            | Change::RequestCheck { .. }
            | Change::RequestReview { .. }
    ) {
        goal.budget.admission(goal.created_at_ms, now_ms)?;
    }
    let workflow = &mut goal.workflow;
    match change {
        Change::BindWorkerTurn {
            execution_id,
            thread,
        } => {
            identifier(&thread.thread_id)?;
            identifier(&thread.turn_id)?;
            if workflow.owner_thread_id.is_none()
                || workflow.owner_thread_id.as_ref() == Some(&thread.thread_id)
                || workflow.input_thread_id.as_ref() == Some(&thread.thread_id)
                || workflow
                    .tasks
                    .iter()
                    .flat_map(|task| &task.executions)
                    .any(|execution| {
                        &execution.id != execution_id && execution.thread.as_ref() == Some(thread)
                    })
            {
                return Err(GoalError::Conflict);
            }
            let execution = workflow
                .tasks
                .iter_mut()
                .flat_map(|task| &mut task.executions)
                .find(|execution| &execution.id == execution_id)
                .ok_or(GoalError::NotFound)?;
            if !execution.dispatch_requested
                || matches!(execution.observation, Observation::NotAdmitted { .. })
                || execution.requirements_revision != revision
                || execution
                    .thread
                    .as_ref()
                    .is_some_and(|known| known != thread)
            {
                return Err(GoalError::Conflict);
            }
            execution.thread = Some(thread.clone());
        }
        Change::BindInputThread { thread_id } => {
            identifier(thread_id)?;
            if workflow.owner_thread_id.is_none()
                || workflow.owner_thread_id.as_ref() == Some(thread_id)
                || workflow
                    .input_thread_id
                    .as_ref()
                    .is_some_and(|current| current != thread_id)
            {
                return Err(GoalError::Conflict);
            }
            workflow.input_thread_id = Some(thread_id.clone());
        }
        Change::BindModelTurn {
            origin,
            thread_id,
            turn_id,
        } => {
            identifier(&origin.source_id)?;
            identifier(thread_id)?;
            identifier(turn_id)?;
            if origin.goal_id != goal.id
                || origin.requirements_revision != revision
                || ![
                    workflow.owner_thread_id.as_ref(),
                    workflow.input_thread_id.as_ref(),
                ]
                .contains(&Some(thread_id))
            {
                return Err(GoalError::Conflict);
            }
            // The immutable operation is the association. Keeping every model
            // call in the current Goal head would duplicate unbounded history.
        }
        Change::BindOwnerThread { thread_id } => {
            identifier(thread_id)?;
            if workflow
                .owner_thread_id
                .as_ref()
                .is_some_and(|current| current != thread_id)
            {
                return Err(GoalError::Conflict);
            }
            workflow.owner_thread_id = Some(thread_id.clone());
        }
        Change::ArchiveExecution { .. } => {
            return Err(invalid("archive transition was not handled"));
        }
        Change::Budget { .. } => return Err(invalid("budget transition was not handled")),
        Change::Completion { .. } => return Err(invalid("completion transition was not handled")),
        Change::Accounting { .. } => return Err(invalid("accounting transition was not handled")),
        Change::Decision { .. } => return Err(invalid("decision transition was not handled")),
        Change::Input { .. } => return Err(invalid("input transition was not handled")),
        Change::ClaimCoordinator { id, expires_at_ms } => {
            workflow.coordinator = Some(CoordinatorLease {
                id: id.clone(),
                expires_at_ms: *expires_at_ms,
            });
        }
        Change::RequestPlan { attempt_id } => {
            identifier(attempt_id)?;
            if intent != RunIntent::Run || workflow.planning != Planning::Unplanned {
                return Err(GoalError::Conflict);
            }
            workflow.planning = Planning::Requested {
                attempt_id: attempt_id.clone(),
            };
            workflow.planning_requirements_revision = revision;
        }
        Change::RearmPlan => {
            if intent != RunIntent::Run
                || !workflow.tasks.is_empty()
                || !matches!(workflow.planning, Planning::Blocked { .. })
                || workflow.planning_requirements_revision >= revision
            {
                return Err(GoalError::Conflict);
            }
            workflow.planning = Planning::Unplanned;
        }
        Change::InstallPlan {
            attempt_id,
            plan,
            available_harnesses,
        } => {
            if workflow.planning
                != (Planning::Requested {
                    attempt_id: attempt_id.clone(),
                })
                || workflow.planning_requirements_revision != revision
            {
                return Err(GoalError::Conflict);
            }
            plan.validate(available_harnesses)?;
            workflow.completion.specification = Some(plan.completion.clone());
            workflow.tasks = plan
                .tasks
                .iter()
                .cloned()
                .map(|spec| Task {
                    spec,
                    legacy_worker: None,
                    requirements_revision: revision,
                    executions: Vec::new(),
                    archived_executions: 0,
                    latest_archive: None,
                    run_intent: RunIntent::Run,
                    control_source_revision: 0,
                })
                .collect();
            workflow.planning = Planning::Planned {
                attempt_id: attempt_id.clone(),
            };
        }
        Change::BlockPlan { attempt_id, reason } => {
            if workflow.planning
                != (Planning::Requested {
                    attempt_id: attempt_id.clone(),
                })
                || workflow.planning_requirements_revision != revision
            {
                return Err(GoalError::Conflict);
            }
            super::message(reason)?;
            workflow.planning = Planning::Blocked {
                attempt_id: attempt_id.clone(),
                reason: reason.clone(),
            };
        }
        Change::RequestExecution {
            task_id,
            execution_id,
            workspace,
        } => {
            identifier(execution_id)?;
            // Only an approved handoff for this requirements version unlocks
            // dependents. A settled turn alone is still a candidate.
            if intent != RunIntent::Run
                || !workspace.is_absolute()
                || workflow
                    .tasks
                    .iter()
                    .flat_map(|task| &task.executions)
                    .any(|execution| &execution.id == execution_id)
            {
                return Err(GoalError::Conflict);
            }
            let task_index = workflow
                .tasks
                .iter()
                .position(|task| &task.spec.id == task_id)
                .ok_or(GoalError::NotFound)?;
            for dependency in &workflow.tasks[task_index].spec.dependencies {
                let upstream = workflow
                    .tasks
                    .iter()
                    .find(|task| &task.spec.id == dependency)
                    .ok_or(GoalError::NotFound)?;
                if !upstream
                    .executions
                    .last()
                    .and_then(|execution| execution.handoff.as_ref())
                    .is_some_and(|handoff| handoff.task_revision == upstream.requirements_revision)
                {
                    return Err(GoalError::Conflict);
                }
            }
            let task = &mut workflow.tasks[task_index];
            if task.execution_admission(revision, goal_control_source) != ExecutionAdmission::Ready
            {
                return Err(GoalError::Conflict);
            }
            task.executions.push(Execution {
                id: execution_id.clone(),
                workspace: workspace.clone(),
                observation: Observation::Requested,
                thread: None,
                worker: Some(Worker::from_harness(task.spec.harness.clone())),
                requirements_revision: revision,
                task_revision: task.requirements_revision,
                control_source_revision: goal_control_source.max(task.control_source_revision),
                dispatch_requested: false,
                managed_dispatch: false,
                resources: None,
                confirmed_model_route: None,
                checks: Vec::new(),
                stop: None,
                stop_boundary_deadline_ms: None,
                base_commit: None,
                handoff: None,
                review: None,
                reconciliation: None,
            });
        }
        Change::ReconcileStoppedExecution {
            execution_id,
            reconciliation,
        } => {
            let task = workflow
                .tasks
                .iter_mut()
                .find(|task| {
                    task.executions
                        .last()
                        .is_some_and(|execution| &execution.id == execution_id)
                })
                .ok_or(GoalError::NotFound)?;
            let execution = task.executions.last_mut().ok_or(GoalError::NotFound)?;
            if !matches!(execution.stop, Some(StopState::Confirmed { .. })) {
                return Err(GoalError::Conflict);
            }
            if matches!(reconciliation, LocalReconciliation::Verified { .. })
                && (matches!(execution.review, Some(ReviewState::Requested { .. }))
                    || execution.checks.iter().any(|check| {
                        matches!(
                            check.state,
                            CheckState::Requested
                                | CheckState::Observed {
                                    completed: false,
                                    ..
                                }
                        )
                    }))
            {
                return Err(GoalError::Conflict);
            }
            let (task_revision, control_source_revision) = match reconciliation {
                LocalReconciliation::Verified {
                    task_revision,
                    control_source_revision,
                    workspace,
                    transcript,
                } => {
                    workspace.validate()?;
                    transcript.validate()?;
                    (*task_revision, *control_source_revision)
                }
                LocalReconciliation::Blocked {
                    task_revision,
                    control_source_revision,
                    reason,
                } => {
                    super::message(reason)?;
                    (*task_revision, *control_source_revision)
                }
            };
            if task_revision != task.requirements_revision
                || control_source_revision != goal_control_source.max(task.control_source_revision)
                || execution.reconciliation.as_ref() == Some(reconciliation)
            {
                return Err(GoalError::Conflict);
            }
            execution.reconciliation = Some(reconciliation.clone());
        }
        Change::RegisterWorkspace {
            execution_id,
            base_commit,
        } => {
            if !(base_commit.len() == 40 || base_commit.len() == 64)
                || !base_commit.bytes().all(|byte| byte.is_ascii_hexdigit())
            {
                return Err(invalid("invalid workspace commit identity"));
            }
            let execution = workflow
                .tasks
                .iter_mut()
                .flat_map(|task| &mut task.executions)
                .find(|execution| &execution.id == execution_id)
                .ok_or(GoalError::NotFound)?;
            if execution.requirements_revision != revision
                || execution.base_commit.is_some()
                || execution.observation != Observation::Requested
            {
                return Err(GoalError::Conflict);
            }
            execution.base_commit = Some(base_commit.clone());
        }
        Change::PrepareResources {
            execution_id,
            resources,
        } => {
            resources.validate()?;
            let task = workflow
                .tasks
                .iter_mut()
                .find(|task| {
                    task.executions
                        .iter()
                        .any(|execution| &execution.id == execution_id)
                })
                .ok_or(GoalError::NotFound)?;
            let index = task
                .executions
                .iter()
                .position(|execution| &execution.id == execution_id)
                .ok_or(GoalError::NotFound)?;
            if resources.worker_handoff != task.worker_handoff(index, goal_control_source)? {
                return Err(invalid(
                    "prepared worker handoff differs from the retained source",
                ));
            }
            let execution = task
                .executions
                .iter_mut()
                .find(|execution| &execution.id == execution_id)
                .ok_or(GoalError::NotFound)?;
            if intent != RunIntent::Run
                || task.run_intent != RunIntent::Run
                || execution.observation != Observation::Requested
                || execution.base_commit.is_none()
                || execution.dispatch_requested
                || execution.stop.is_some()
                || execution.requirements_revision != revision
                || execution.task_revision != task.requirements_revision
                || execution.resources.is_some()
                || resources.selection != task.spec.resources
            {
                return Err(GoalError::Conflict);
            }
            execution.resources = Some(resources.clone());
        }
        Change::RequestCheck {
            execution_id,
            attempt_id,
            check_id,
            workspace,
        } => {
            identifier(attempt_id)?;
            workspace.validate()?;
            let task = workflow
                .tasks
                .iter_mut()
                .find(|task| {
                    task.executions
                        .iter()
                        .any(|execution| &execution.id == execution_id)
                })
                .ok_or(GoalError::NotFound)?;
            if intent != RunIntent::Run || task.run_intent != RunIntent::Run {
                return Err(GoalError::Conflict);
            }
            let spec = task
                .spec
                .checks
                .iter()
                .find(|spec| &spec.id == check_id)
                .ok_or(GoalError::NotFound)?
                .clone();
            let execution = task
                .executions
                .iter_mut()
                .find(|execution| &execution.id == execution_id)
                .ok_or(GoalError::NotFound)?;
            if !matches!(execution.observation, Observation::Candidate { .. })
                || !matches!(execution.stop, Some(StopState::Confirmed { .. }))
                || execution.checks.iter().any(|check| {
                    &check.id == attempt_id
                        || check.spec == spec
                            && check.task_revision == task.requirements_revision
                            && check.workspace == *workspace
                })
                || execution.checks.len() >= 64
            {
                return Err(GoalError::Conflict);
            }
            execution.checks.push(CheckRun {
                id: attempt_id.clone(),
                spec,
                requirements_revision: revision,
                task_revision: task.requirements_revision,
                workspace: workspace.clone(),
                state: CheckState::Requested,
            });
        }
        Change::ObserveCheck {
            execution_id,
            attempt_id,
            report,
            output,
            workspace_after,
        } => {
            report.validate()?;
            workspace_after.validate()?;
            let check = workflow
                .tasks
                .iter_mut()
                .flat_map(|task| &mut task.executions)
                .find(|execution| &execution.id == execution_id)
                .ok_or(GoalError::NotFound)?
                .checks
                .iter_mut()
                .find(|check| &check.id == attempt_id)
                .ok_or(GoalError::NotFound)?;
            if check.state != CheckState::Requested || check.requirements_revision != revision {
                return Err(GoalError::Conflict);
            }
            check.state = CheckState::Observed {
                report: report.clone(),
                completed: output.completed,
                passed: output.supports(&check.spec) && *workspace_after == check.workspace,
            };
        }
        Change::RequestDispatch { execution_id }
        | Change::RequestWorkerDispatch { execution_id } => {
            let task = workflow
                .tasks
                .iter_mut()
                .find(|task| {
                    task.executions
                        .iter()
                        .any(|execution| &execution.id == execution_id)
                })
                .ok_or(GoalError::NotFound)?;
            if intent != RunIntent::Run || task.run_intent != RunIntent::Run {
                return Err(GoalError::Conflict);
            }
            let execution = task
                .executions
                .iter_mut()
                .find(|execution| &execution.id == execution_id)
                .ok_or(GoalError::NotFound)?;
            if execution.observation != Observation::Requested
                || execution.base_commit.is_none()
                || execution.dispatch_requested
                || execution.stop.is_some()
                || execution.requirements_revision != revision
                || execution.task_revision != task.requirements_revision
                || !task.spec.resources.is_empty() && execution.resources.is_none()
            {
                return Err(GoalError::Conflict);
            }
            execution.dispatch_requested = true;
            execution.managed_dispatch = matches!(change, Change::RequestWorkerDispatch { .. });
        }
        Change::RejectWorkerDispatch {
            execution_id,
            reason,
        } => {
            super::message(reason)?;
            let execution = workflow
                .tasks
                .iter_mut()
                .flat_map(|task| &mut task.executions)
                .find(|execution| &execution.id == execution_id)
                .ok_or(GoalError::NotFound)?;
            // Unknown revokes worker_requested before the host queries the
            // original keys under ThreadService's admission lock. A missing
            // Goal binding alone is never this evidence.
            if !execution.managed_dispatch
                || !execution.dispatch_requested
                || execution.thread.is_some()
                || execution.stop.is_some()
                || !matches!(
                    execution.observation,
                    Observation::Unknown { run_id: None, .. }
                )
            {
                return Err(GoalError::Conflict);
            }
            execution.observation = Observation::NotAdmitted {
                reason: reason.clone(),
            };
        }
        Change::ApproveHandoff {
            execution_id,
            handoff,
        } => {
            handoff.workspace.validate()?;
            handoff.review.validate()?;
            identifier(&handoff.review_request_id)?;
            let task = workflow
                .tasks
                .iter_mut()
                .find(|task| {
                    task.executions
                        .iter()
                        .any(|execution| &execution.id == execution_id)
                })
                .ok_or(GoalError::NotFound)?;
            let task_revision = task.requirements_revision;
            let execution = task
                .executions
                .iter_mut()
                .find(|execution| &execution.id == execution_id)
                .ok_or(GoalError::NotFound)?;
            let Observation::Candidate { report, .. } = &execution.observation else {
                return Err(GoalError::Conflict);
            };
            if execution.handoff.is_some()
                || execution.base_commit.is_none()
                || report.sha256 != handoff.candidate_sha256
                || handoff.requirements_revision != revision
                || handoff.task_revision != task_revision
                || !task.spec.checks.iter().all(|spec| {
                    execution.checks.iter().any(|check| {
                        check.spec == *spec
                            && check.task_revision == task_revision
                            && check.workspace == handoff.workspace
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
                || !matches!(execution.stop, Some(StopState::Confirmed { .. }))
                || execution.review
                    != Some(ReviewState::Requested {
                        attempt_id: handoff.attempt_id.clone(),
                        workspace: handoff.workspace.clone(),
                        requirements_revision: revision,
                        task_revision,
                    })
            {
                return Err(GoalError::Conflict);
            }
            execution.handoff = Some(handoff.clone());
            execution.review = Some(ReviewState::Approved {
                attempt_id: handoff.attempt_id.clone(),
                requirements_revision: revision,
                task_revision,
            });
        }
        Change::RequestReview {
            execution_id,
            attempt_id,
            workspace,
        } => {
            identifier(attempt_id)?;
            workspace.validate()?;
            let task = workflow
                .tasks
                .iter_mut()
                .find(|task| {
                    task.executions
                        .iter()
                        .any(|execution| &execution.id == execution_id)
                })
                .ok_or(GoalError::NotFound)?;
            let task_revision = task.requirements_revision;
            if intent != RunIntent::Run || task.run_intent != RunIntent::Run {
                return Err(GoalError::Conflict);
            }
            let execution = task
                .executions
                .iter_mut()
                .find(|execution| &execution.id == execution_id)
                .ok_or(GoalError::NotFound)?;
            if execution
                .review
                .as_ref()
                .is_some_and(|review| !review.can_reopen(revision, task_revision))
                || execution.base_commit.is_none()
                || !matches!(execution.observation, Observation::Candidate { .. })
                || !matches!(execution.stop, Some(StopState::Confirmed { .. }))
            {
                return Err(GoalError::Conflict);
            }
            execution.handoff = None;
            execution.review = Some(ReviewState::Requested {
                attempt_id: attempt_id.clone(),
                workspace: workspace.clone(),
                requirements_revision: revision,
                task_revision,
            });
        }
        Change::RejectReview {
            execution_id,
            attempt_id,
            reason,
            report,
        } => {
            super::message(reason)?;
            if let Some(report) = report {
                report.validate()?;
            }
            let task = workflow
                .tasks
                .iter_mut()
                .find(|task| {
                    task.executions
                        .iter()
                        .any(|execution| &execution.id == execution_id)
                })
                .ok_or(GoalError::NotFound)?;
            let execution = task
                .executions
                .iter_mut()
                .find(|execution| &execution.id == execution_id)
                .ok_or(GoalError::NotFound)?;
            let Some(ReviewState::Requested {
                attempt_id: requested,
                requirements_revision,
                task_revision,
                ..
            }) = &execution.review
            else {
                return Err(GoalError::Conflict);
            };
            if requested != attempt_id || *requirements_revision != revision {
                return Err(GoalError::Conflict);
            }
            execution.review = Some(ReviewState::Rejected {
                attempt_id: attempt_id.clone(),
                reason: reason.clone(),
                report: report.clone(),
                requirements_revision: revision,
                task_revision: *task_revision,
            });
        }
        Change::AwaitStopBoundary {
            execution_id,
            deadline_ms,
        } => {
            if budget_stopped
                || intent != RunIntent::Run
                || *deadline_ms <= now_ms
                || deadline_ms.saturating_sub(now_ms) > SUPERSEDED_STOP_GRACE_MS
            {
                return Err(GoalError::Conflict);
            }
            let task = workflow
                .tasks
                .iter_mut()
                .find(|task| {
                    task.executions
                        .iter()
                        .any(|execution| &execution.id == execution_id)
                })
                .ok_or(GoalError::NotFound)?;
            let task_revision = task.requirements_revision;
            if task.run_intent != RunIntent::Run {
                return Err(GoalError::Conflict);
            }
            let execution = task
                .executions
                .iter_mut()
                .find(|execution| &execution.id == execution_id)
                .ok_or(GoalError::NotFound)?;
            if execution.task_revision == task_revision
                || execution.stop.is_some()
                || execution.stop_boundary_deadline_ms.is_some()
                || observation_run(&execution.observation).is_none()
            {
                return Err(GoalError::Conflict);
            }
            execution.stop_boundary_deadline_ms = Some(*deadline_ms);
        }
        Change::RequestStop {
            execution_id,
            operation_id,
        } => {
            identifier(operation_id)?;
            let task = workflow
                .tasks
                .iter_mut()
                .find(|task| {
                    task.executions
                        .iter()
                        .any(|execution| &execution.id == execution_id)
                })
                .ok_or(GoalError::NotFound)?;
            let task_revision = task.requirements_revision;
            let task_intent = task.run_intent;
            let execution = task
                .executions
                .iter_mut()
                .find(|execution| &execution.id == execution_id)
                .ok_or(GoalError::NotFound)?;
            if (!budget_stopped
                && intent == RunIntent::Run
                && task_intent == RunIntent::Run
                // A mapped failed/unknown attempt may be frozen for evidence
                // reconciliation while independent authorized work continues.
                // This does not authorize replacement dispatch.
                && !matches!(execution.observation, Observation::Candidate { .. }
                    | Observation::Failed { .. }
                    | Observation::Unknown { .. })
                && execution.task_revision == task_revision)
                || execution.stop.is_some()
                || observation_run(&execution.observation).is_none() && execution.thread.is_none()
            {
                return Err(GoalError::Conflict);
            }
            execution.stop = Some(StopState::Requested {
                operation_id: operation_id.clone(),
            });
            execution.stop_boundary_deadline_ms = None;
        }
        Change::ObserveStop {
            execution_id,
            operation_id,
            confirmed,
            detail,
        } => {
            super::message(detail)?;
            let execution = workflow
                .tasks
                .iter_mut()
                .flat_map(|task| &mut task.executions)
                .find(|execution| &execution.id == execution_id)
                .ok_or(GoalError::NotFound)?;
            // Late evidence may resolve the original unknown stop. It cannot
            // introduce a different action, repeat uncertainty or downgrade
            // a confirmed result. The coordinator lease is still required.
            let original = match &execution.stop {
                Some(StopState::Requested { operation_id }) => Some(operation_id),
                Some(StopState::Unknown { operation_id, .. }) if *confirmed => Some(operation_id),
                _ => None,
            };
            if original != Some(operation_id) {
                return Err(GoalError::Conflict);
            }
            execution.stop = Some(if *confirmed {
                StopState::Confirmed {
                    operation_id: operation_id.clone(),
                    detail: detail.clone(),
                }
            } else {
                StopState::Unknown {
                    operation_id: operation_id.clone(),
                    detail: detail.clone(),
                }
            });
        }
        Change::ConfirmModelRoute {
            execution_id,
            run_id,
            route,
        } => {
            let execution = workflow
                .tasks
                .iter_mut()
                .flat_map(|task| &mut task.executions)
                .find(|execution| &execution.id == execution_id)
                .ok_or(GoalError::NotFound)?;
            if execution.requirements_revision != revision
                || observation_run(&execution.observation) != Some(run_id.as_str())
                || !execution.dispatch_requested
                || execution
                    .resources
                    .as_ref()
                    .and_then(|prepared| prepared.selection.model_route.as_ref())
                    != Some(route)
                || execution
                    .confirmed_model_route
                    .as_ref()
                    .is_some_and(|confirmed| confirmed != route)
            {
                return Err(GoalError::Conflict);
            }
            execution.confirmed_model_route = Some(route.clone());
        }
        Change::ObserveExecution {
            execution_id,
            observation,
        } => {
            let task = workflow
                .tasks
                .iter_mut()
                .find(|task| {
                    task.executions
                        .iter()
                        .any(|execution| &execution.id == execution_id)
                })
                .ok_or(GoalError::NotFound)?;
            let execution = task
                .executions
                .iter()
                .position(|execution| &execution.id == execution_id)
                .ok_or(GoalError::NotFound)?;
            let external = task
                .execution_harness(&task.executions[execution])
                .is_some();
            let execution = task
                .executions
                .iter_mut()
                .find(|execution| &execution.id == execution_id)
                .ok_or(GoalError::NotFound)?;
            if execution.requirements_revision != revision {
                return Err(GoalError::Conflict);
            }
            match &execution.observation {
                Observation::Requested => {}
                Observation::Running { run_id, .. } => {
                    if observation_run(observation) != run_id.as_deref() {
                        return Err(GoalError::Conflict);
                    }
                }
                Observation::Unknown { run_id: None, .. }
                    if execution.dispatch_requested
                        && (matches!(
                            observation,
                            Observation::Running { .. }
                                | Observation::Unknown {
                                    run_id: Some(_),
                                    ..
                                }
                        ) || execution.thread.is_some()) => {}
                _ => return Err(GoalError::Conflict),
            }
            match observation {
                Observation::Requested | Observation::NotAdmitted { .. } => {
                    return Err(invalid(
                        "execution observation cannot set an admission verdict",
                    ));
                }
                Observation::Candidate { report, .. } => report.validate()?,
                Observation::Failed { reason, .. } | Observation::Unknown { reason, .. } => {
                    super::message(reason)?
                }
                Observation::Running { .. } => {}
            }
            if !external
                && matches!(
                    observation,
                    Observation::Running {
                        native_session_id: Some(_),
                        ..
                    } | Observation::Candidate {
                        native_session_id: Some(_),
                        ..
                    }
                )
            {
                return Err(invalid(
                    "built-in execution cannot acquire an ACP session identity",
                ));
            }
            if matches!(
                observation,
                Observation::Running { .. } | Observation::Candidate { .. }
            ) && !execution.dispatch_requested
            {
                return Err(invalid(
                    "execution mapping requires a durably admitted dispatch",
                ));
            }
            if let Some(run_id) = observation_run(observation) {
                if !external {
                    return Err(invalid(
                        "built-in execution cannot acquire an ACP run identity",
                    ));
                }
                identifier(run_id)?;
            } else if matches!(
                observation,
                Observation::Running { .. } | Observation::Candidate { .. }
            ) && (external || execution.thread.is_none())
            {
                return Err(invalid(
                    "built-in execution requires its accepted Thread/Turn identity",
                ));
            }
            execution.observation = observation.clone();
        }
    }
    Ok(())
}

fn observation_run(observation: &Observation) -> Option<&str> {
    match observation {
        Observation::Running { run_id, .. } | Observation::Candidate { run_id, .. } => {
            run_id.as_deref()
        }
        Observation::Failed { run_id, .. } | Observation::Unknown { run_id, .. } => {
            run_id.as_deref()
        }
        Observation::Requested | Observation::NotAdmitted { .. } => None,
    }
}
