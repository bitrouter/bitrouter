//! Persistent Goal admission. Execution belongs to the daemon, never the observer.

use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub mod accounting;
pub mod budget;
pub mod completion;
pub mod decision;
pub mod input;
pub mod resources;
pub mod verification;
pub mod workflow;

/// Maximum original Goal input, before JSON encoding.
pub const MAX_INPUT_BYTES: usize = 64 * 1024;
/// Maximum number of records in a directory or history page.
pub const MAX_PAGE_ITEMS: usize = 16;
/// Bound on a serialized snapshot, operation or query page.
pub const MAX_RECORD_BYTES: usize = 1024 * 1024;
/// Goal requests are a separate local protocol from native Thread operations.
pub const PROTOCOL_VERSION: u32 = 3;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunIntent {
    #[default]
    Run,
    Pause,
    Stop,
}

/// Effective requirements are separate from received conversation inputs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GoalSnapshot {
    pub id: String,
    pub owner: String,
    pub title: String,
    pub workspace: PathBuf,
    pub objective: String,
    pub revision: u64,
    pub requirements_revision: u64,
    pub run_intent: RunIntent,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
    #[serde(default)]
    pub workflow: workflow::Workflow,
    #[serde(default)]
    pub inputs: input::InputProgress,
    #[serde(default)]
    pub control_source_revision: u64,
    #[serde(default)]
    pub accounting: accounting::Accounting,
    #[serde(default)]
    pub budget: budget::Budget,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum OperationKind {
    BudgetUpdated {
        limits: budget::Limits,
    },
    DeliveryAccepted {
        attempt_id: String,
        delivery_sha256: String,
    },
    DecisionResponse {
        decision_id: String,
        option_id: String,
    },
    Created,
    Input {
        text: String,
        task_reference: Option<String>,
    },
    InputWithdrawn {
        input_id: String,
        source_requirements_revision: u64,
    },
    Control {
        intent: RunIntent,
    },
    Workflow {
        change: Box<workflow::Change>,
    },
}

/// Immutable admission receipt. It does not claim delivery or execution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GoalOperation {
    pub id: String,
    pub fingerprint: String,
    pub goal_id: String,
    pub revision: u64,
    pub requirements_revision: u64,
    pub accepted_at_ms: u64,
    pub kind: OperationKind,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum GoalCommand {
    WithdrawInput {
        operation_id: String,
        goal_id: String,
        expected_revision: u64,
        input_id: String,
        source_requirements_revision: u64,
    },
    SetBudget {
        operation_id: String,
        goal_id: String,
        expected_revision: u64,
        limits: budget::Limits,
    },
    AcceptDelivery {
        operation_id: String,
        goal_id: String,
        expected_requirements_revision: u64,
        attempt_id: String,
        delivery_sha256: String,
    },
    RespondDecision {
        operation_id: String,
        goal_id: String,
        decision_id: String,
        expected_requirements_revision: u64,
        option_id: String,
    },
    Create {
        operation_id: String,
        title: String,
        workspace: PathBuf,
        objective: String,
        #[serde(default)]
        budget_limits: budget::Limits,
    },
    Input {
        operation_id: String,
        goal_id: String,
        expected_revision: u64,
        text: String,
        task_reference: Option<String>,
    },
    Control {
        operation_id: String,
        goal_id: String,
        expected_revision: u64,
        intent: RunIntent,
    },
    Get {
        goal_id: String,
    },
    Operation {
        operation_id: String,
    },
    List {
        after: u64,
        cutoff: Option<u64>,
        limit: usize,
    },
    History {
        goal_id: String,
        after: u64,
        cutoff: Option<u64>,
        limit: usize,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GoalPage {
    pub goals: Vec<GoalSnapshot>,
    pub cutoff: u64,
    pub next_after: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryPage {
    pub operations: Vec<GoalOperation>,
    pub cutoff: u64,
    pub next_after: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum GoalReply {
    Accepted { operation: GoalOperation },
    Snapshot { goal: Box<GoalSnapshot> },
    Directory { page: GoalPage },
    History { page: HistoryPage },
}

#[derive(Debug, Clone, thiserror::Error, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "error", rename_all = "snake_case")]
pub enum GoalError {
    #[error("Goal budget prevents new work: {message}")]
    BudgetExceeded { message: String },
    #[error("Goal or operation not found")]
    NotFound,
    #[error("Goal revision changed; refresh before submitting")]
    Conflict,
    #[error("operation identity already belongs to different input")]
    OperationConflict,
    #[error("invalid Goal request: {message}")]
    Invalid { message: String },
    #[error("Goal storage failed: {message}")]
    Storage { message: String },
}

#[async_trait::async_trait]
pub trait GoalStore: Send + Sync {
    async fn get(&self, owner: &str, id: &str) -> Result<GoalSnapshot, GoalError>;
    async fn operation(&self, owner: &str, id: &str) -> Result<Option<GoalOperation>, GoalError>;
    async fn list(
        &self,
        owner: &str,
        after: u64,
        cutoff: Option<u64>,
        limit: usize,
    ) -> Result<GoalPage, GoalError>;
    async fn history(
        &self,
        owner: &str,
        id: &str,
        after: u64,
        cutoff: Option<u64>,
        limit: usize,
    ) -> Result<HistoryPage, GoalError>;
    /// Commit the head and receipt atomically. Repeated matching identities
    /// return the original receipt, including during competing admissions.
    async fn commit(
        &self,
        expected_revision: Option<u64>,
        goal: GoalSnapshot,
        operation: GoalOperation,
    ) -> Result<GoalOperation, GoalError>;
}

pub struct GoalService {
    store: Arc<dyn GoalStore>,
}

impl GoalService {
    pub fn new(store: Arc<dyn GoalStore>) -> Self {
        Self { store }
    }

    pub async fn execute(
        &self,
        owner: &str,
        command: GoalCommand,
        now_ms: u64,
    ) -> Result<GoalReply, GoalError> {
        identifier(owner)?;
        match &command {
            GoalCommand::Get { goal_id } => {
                identifier(goal_id)?;
                return Ok(GoalReply::Snapshot {
                    goal: Box::new(self.store.get(owner, goal_id).await?),
                });
            }
            GoalCommand::Operation { operation_id } => {
                identifier(operation_id)?;
                return Ok(GoalReply::Accepted {
                    operation: self
                        .store
                        .operation(owner, operation_id)
                        .await?
                        .ok_or(GoalError::NotFound)?,
                });
            }
            GoalCommand::List {
                after,
                cutoff,
                limit,
            } => {
                page_bounds(*after, *cutoff, *limit)?;
                return Ok(GoalReply::Directory {
                    page: self.store.list(owner, *after, *cutoff, *limit).await?,
                });
            }
            GoalCommand::History {
                goal_id,
                after,
                cutoff,
                limit,
            } => {
                identifier(goal_id)?;
                page_bounds(*after, *cutoff, *limit)?;
                return Ok(GoalReply::History {
                    page: self
                        .store
                        .history(owner, goal_id, *after, *cutoff, *limit)
                        .await?,
                });
            }
            _ => {}
        }
        let bytes = serde_json::to_vec(&command).map_err(storage_error)?;
        let fingerprint = hex::encode(Sha256::digest(&bytes));
        let operation_id = match &command {
            GoalCommand::SetBudget { operation_id, .. }
            | GoalCommand::AcceptDelivery { operation_id, .. }
            | GoalCommand::Create { operation_id, .. }
            | GoalCommand::Input { operation_id, .. }
            | GoalCommand::Control { operation_id, .. }
            | GoalCommand::WithdrawInput { operation_id, .. } => operation_id.clone(),
            GoalCommand::RespondDecision { operation_id, .. } => operation_id.clone(),
            _ => return Err(invalid("expected a Goal mutation")),
        };
        identifier(&operation_id)?;
        if let Some(original) = self.store.operation(owner, &operation_id).await? {
            return replay(original, &fingerprint)
                .map(|operation| GoalReply::Accepted { operation });
        }
        let (mut goal, expected_revision, kind) = match command {
            GoalCommand::WithdrawInput {
                goal_id,
                expected_revision,
                input_id,
                source_requirements_revision,
                ..
            } => {
                identifier(&goal_id)?;
                identifier(&input_id)?;
                let mut goal = self.store.get(owner, &goal_id).await?;
                input::withdraw(
                    &mut goal,
                    &input_id,
                    source_requirements_revision,
                    &operation_id,
                )?;
                (
                    goal,
                    Some(expected_revision),
                    OperationKind::InputWithdrawn {
                        input_id,
                        source_requirements_revision,
                    },
                )
            }
            GoalCommand::SetBudget {
                goal_id,
                expected_revision,
                limits,
                ..
            } => {
                identifier(&goal_id)?;
                let mut goal = self.store.get(owner, &goal_id).await?;
                goal.budget.limits = limits.clone();
                if goal.budget.admission(goal.created_at_ms, now_ms).is_err()
                    || limits
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
                    goal.budget.resume_required = true;
                    goal.budget.halt = Some("Updated limits do not admit progression, or the previous budget halt still requires explicit resume".into());
                }
                (
                    goal,
                    Some(expected_revision),
                    OperationKind::BudgetUpdated { limits },
                )
            }
            GoalCommand::AcceptDelivery {
                goal_id,
                expected_requirements_revision,
                attempt_id,
                delivery_sha256,
                ..
            } => {
                identifier(&goal_id)?;
                let mut goal = self.store.get(owner, &goal_id).await?;
                if goal.requirements_revision != expected_requirements_revision {
                    return Err(GoalError::Conflict);
                }
                completion::accept(&mut goal, &attempt_id, &delivery_sha256)?;
                let expected = goal.revision;
                (
                    goal,
                    Some(expected),
                    OperationKind::DeliveryAccepted {
                        attempt_id,
                        delivery_sha256,
                    },
                )
            }
            GoalCommand::RespondDecision {
                goal_id,
                decision_id,
                expected_requirements_revision,
                option_id,
                ..
            } => {
                identifier(&goal_id)?;
                let mut goal = self.store.get(owner, &goal_id).await?;
                if goal.requirements_revision != expected_requirements_revision {
                    return Err(GoalError::Conflict);
                }
                if goal
                    .workflow
                    .decisions
                    .iter()
                    .find(|decision| decision.id == decision_id)
                    .and_then(|decision| {
                        decision
                            .options
                            .iter()
                            .find(|option| option.id == option_id)
                    })
                    .is_some_and(|option| option.allows)
                {
                    goal.budget.admission(goal.created_at_ms, now_ms)?;
                }
                decision::apply(
                    &mut goal,
                    &decision::Event::Respond {
                        decision_id: decision_id.clone(),
                        operation_id: operation_id.clone(),
                        option_id: option_id.clone(),
                        automatic: false,
                    },
                )?;
                let expected = goal.revision;
                (
                    goal,
                    Some(expected),
                    OperationKind::DecisionResponse {
                        decision_id,
                        option_id,
                    },
                )
            }
            GoalCommand::Create {
                title,
                workspace,
                objective,
                budget_limits,
                ..
            } => {
                if title.trim().is_empty()
                    || title.len() > 256
                    || title.chars().any(char::is_control)
                {
                    return Err(invalid(
                        "title must contain 1–256 bytes without control characters",
                    ));
                }
                message(&objective)?;
                if !workspace.is_absolute() {
                    return Err(invalid("workspace must be absolute"));
                }
                (
                    GoalSnapshot {
                        id: uuid::Uuid::new_v4().to_string(),
                        owner: owner.into(),
                        title,
                        workspace,
                        objective,
                        revision: 0,
                        requirements_revision: 1,
                        run_intent: RunIntent::Run,
                        created_at_ms: now_ms,
                        updated_at_ms: now_ms,
                        workflow: workflow::Workflow::default(),
                        inputs: input::InputProgress::default(),
                        control_source_revision: 1,
                        accounting: accounting::Accounting::default(),
                        budget: budget::Budget {
                            limits: budget_limits,
                            ..Default::default()
                        },
                    },
                    None,
                    OperationKind::Created,
                )
            }
            GoalCommand::Input {
                goal_id,
                expected_revision,
                text,
                task_reference,
                ..
            } => {
                identifier(&goal_id)?;
                message(&text)?;
                input::permission_reference(&text)?;
                if let Some(reference) = &task_reference {
                    identifier(reference)?;
                }
                let goal = self.store.get(owner, &goal_id).await?;
                if let Some(id) = input::clarification_reference(&text)?
                    && !goal
                        .inputs
                        .unresolved_inputs()
                        .iter()
                        .any(|input| input.input_id == id)
                {
                    return Err(GoalError::NotFound);
                }
                (
                    goal,
                    Some(expected_revision),
                    OperationKind::Input {
                        text,
                        task_reference,
                    },
                )
            }
            GoalCommand::Control {
                goal_id,
                expected_revision,
                intent,
                ..
            } => {
                identifier(&goal_id)?;
                let mut goal = self.store.get(owner, &goal_id).await?;
                if intent == RunIntent::Run && goal.budget.resume_required {
                    if goal.budget.limits.elapsed_ms.is_some_and(|limit| {
                        goal.budget.elapsed(goal.created_at_ms, now_ms) >= limit
                    }) || goal
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
                        return Err(GoalError::BudgetExceeded { message: "Current limits still do not cover retained cumulative usage/exposure".into() });
                    }
                    goal.budget.resume_required = false;
                    goal.budget.halt = None;
                }
                goal.run_intent = intent;
                goal.control_source_revision = expected_revision
                    .checked_add(1)
                    .ok_or_else(|| invalid("revision exhausted"))?;
                (
                    goal,
                    Some(expected_revision),
                    OperationKind::Control { intent },
                )
            }
            _ => return Err(invalid("expected a Goal mutation")),
        };
        if expected_revision.is_some_and(|expected| expected != goal.revision) {
            return Err(GoalError::Conflict);
        }
        goal.revision = goal
            .revision
            .checked_add(1)
            .ok_or_else(|| invalid("revision exhausted"))?;
        goal.updated_at_ms = now_ms;
        let operation = GoalOperation {
            id: operation_id,
            fingerprint,
            goal_id: goal.id.clone(),
            revision: goal.revision,
            requirements_revision: goal.requirements_revision,
            accepted_at_ms: now_ms,
            kind,
        };
        Ok(GoalReply::Accepted {
            operation: self
                .store
                .commit(expected_revision, goal, operation)
                .await?,
        })
    }
}

pub fn replay(original: GoalOperation, fingerprint: &str) -> Result<GoalOperation, GoalError> {
    if original.fingerprint == fingerprint {
        Ok(original)
    } else {
        Err(GoalError::OperationConflict)
    }
}

pub fn identifier(value: &str) -> Result<(), GoalError> {
    if value.is_empty() || value.len() > 128 || value.chars().any(char::is_control) {
        return Err(invalid(
            "identity must contain 1–128 bytes without control characters",
        ));
    }
    Ok(())
}

pub fn page_bounds(after: u64, cutoff: Option<u64>, limit: usize) -> Result<(), GoalError> {
    if !(1..=MAX_PAGE_ITEMS).contains(&limit) || cutoff.is_some_and(|cutoff| after > cutoff) {
        return Err(invalid("invalid Goal page bounds"));
    }
    Ok(())
}

fn message(value: &str) -> Result<(), GoalError> {
    if value.trim().is_empty() || value.len() > MAX_INPUT_BYTES {
        return Err(invalid("input must contain 1–65536 bytes"));
    }
    Ok(())
}

fn invalid(message: &str) -> GoalError {
    GoalError::Invalid {
        message: message.into(),
    }
}

pub fn storage_error(error: impl std::fmt::Display) -> GoalError {
    GoalError::Storage {
        message: error.to_string(),
    }
}
