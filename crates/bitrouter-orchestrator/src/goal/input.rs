//! Ordered Goal input understanding and atomic requirement/control commits.
pub mod scope;

use std::collections::{BTreeMap, BTreeSet};

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
    #[serde(default)]
    pub unresolved: Vec<UnresolvedInput>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClarificationKind {
    #[default]
    Unknown,
    Query,
    Change,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnresolvedInput {
    pub input_id: String,
    pub source_requirements_revision: u64,
    pub task_reference: Option<String>,
    pub permission_decision: Option<String>,
    pub question: String,
    pub kind: ClarificationKind,
}

impl InputProgress {
    /// Older snapshots kept only the latest question. Preserve it when the
    /// next input is processed instead of losing it to the latest-result slot.
    pub fn unresolved_inputs(&self) -> Vec<UnresolvedInput> {
        let mut inputs = self.unresolved.clone();
        if let Some(latest) = &self.latest
            && let Some(question) = &latest.clarification
            && !inputs.iter().any(|input| input.input_id == latest.input_id)
        {
            inputs.push(UnresolvedInput {
                input_id: latest.input_id.clone(),
                source_requirements_revision: latest.source_requirements_revision,
                task_reference: None,
                permission_decision: None,
                question: question.clone(),
                kind: latest.clarification_kind,
            });
        }
        inputs
    }

    pub fn completion_waits_for_input(&self) -> bool {
        self.unresolved
            .iter()
            .any(|input| input.kind != ClarificationKind::Query)
            || self.latest.as_ref().is_some_and(|latest| {
                latest.clarification.is_some()
                    && latest.clarification_kind != ClarificationKind::Query
                    && !self
                        .unresolved
                        .iter()
                        .any(|input| input.input_id == latest.input_id)
            })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResolutionKind {
    Applied,
    Answered,
    Withdrawn,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Resolution {
    pub input_id: String,
    pub source_requirements_revision: u64,
    pub kind: ResolutionKind,
}

/// Original user bytes come from the immutable operation receipt.
#[derive(Debug, Clone, Serialize)]
pub struct ClarificationContext {
    pub unresolved: UnresolvedInput,
    pub original_input: GoalOperation,
}

pub fn clarification_reference(text: &str) -> Result<Option<&str>, GoalError> {
    let mut words = text.split_whitespace();
    if words.next() != Some("/input") {
        return Ok(None);
    }
    let id = words
        .next()
        .ok_or_else(|| invalid("/input requires an unresolved input id"))?;
    identifier(id)?;
    Ok(Some(id))
}

/// Only an exact finite command bypasses model understanding. Natural language,
/// quotes and additional words keep the ordinary interpretation path.
pub fn withdrawal_reference(text: &str) -> Result<Option<&str>, GoalError> {
    let mut words = text.split_whitespace();
    if words.next() != Some("/input") {
        return Ok(None);
    }
    let id = words.next();
    if words.next() != Some("/withdraw") || words.next().is_some() {
        return Ok(None);
    }
    let id = id.ok_or_else(|| invalid("withdrawal requires an unresolved input id"))?;
    identifier(id)?;
    Ok(Some(id))
}

pub(super) fn withdraw(
    goal: &mut GoalSnapshot,
    id: &str,
    source_revision: u64,
    operation_id: &str,
) -> Result<(), GoalError> {
    let mut unresolved = goal.inputs.unresolved_inputs();
    let index = unresolved
        .iter()
        .position(|input| input.input_id == id)
        .ok_or(GoalError::NotFound)?;
    if unresolved[index].source_requirements_revision != source_revision {
        return Err(GoalError::Conflict);
    }
    unresolved.remove(index);
    goal.inputs.unresolved = unresolved;
    goal.inputs.latest = Some(InputResult {
        input_id: operation_id.into(),
        source_requirements_revision: goal.requirements_revision,
        effective_requirements_revision: goal.requirements_revision,
        answer: None,
        clarification: None,
        requirements_saved: false,
        affected_tasks: Vec::new(),
        control_saved: None,
        controlled_tasks: Vec::new(),
        control_superseded: false,
        permission_response_saved: None,
        classification: None,
        reasoning_request_id: None,
        clarification_kind: ClarificationKind::Unknown,
        modification_scope: None,
        resolutions: vec![Resolution {
            input_id: id.into(),
            source_requirements_revision: source_revision,
            kind: ResolutionKind::Withdrawn,
        }],
    });
    // Do not advance the input cursor past other accepted or in-flight inputs.
    Ok(())
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
    /// None preserves the current selection; Some replaces the complete set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resources: Option<super::resources::Selection>,
    /// Add named machine checks while retaining existing requirements. A
    /// matching existing id must have the exact same specification.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub additional_checks: Vec<super::verification::CheckSpec>,
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
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Interpretation {
    pub answer: Option<String>,
    pub objective: Option<String>,
    pub task_updates: Vec<TaskUpdate>,
    pub control: Option<ControlProposal>,
    #[serde(default)]
    pub permission_response: Option<PermissionResponse>,
    pub clarification: Option<String>,
    #[serde(default)]
    pub clarification_kind: Option<ClarificationKind>,
    #[serde(default)]
    pub resolutions: Vec<Resolution>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modification_scope: Option<Box<scope::Binding>>,
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
    #[serde(default)]
    pub clarification_kind: ClarificationKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modification_scope: Option<scope::Binding>,
    #[serde(default)]
    pub resolutions: Vec<Resolution>,
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
                    || !proposal.resolutions.is_empty()
                    || proposal.modification_scope.is_some()
                    || proposal
                        .clarification_kind
                        .is_some_and(|kind| kind != ClarificationKind::Unknown)
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
                    || proposal.permission_response.is_some()
                    || !proposal.resolutions.is_empty()
                    || proposal.modification_scope.is_some()
                    || proposal
                        .clarification_kind
                        .is_some_and(|kind| kind != ClarificationKind::Unknown))
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
            if let Some(binding) = &proposal.modification_scope {
                binding.validate(goal, &pending, &proposal)?;
            }
            let modification_only_ambiguity =
                proposal.modification_scope.as_ref().is_some_and(|binding| {
                    matches!(binding.selection, scope::Selection::Unknown { .. })
                }) && proposal.objective.is_none()
                    && proposal.task_updates.is_empty()
                    && proposal.permission_response.is_none();
            if proposal.clarification.is_some()
                && proposal.clarification_kind != Some(ClarificationKind::Query)
                && !modification_only_ambiguity
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
                || proposal.permission_response.is_some()
                || !proposal.resolutions.is_empty();
            if changed && pending.requirements_revision != goal.requirements_revision {
                proposal.objective = None;
                proposal.task_updates.clear();
                proposal.control = None;
                proposal.permission_response = None;
                proposal.resolutions.clear();
                proposal.modification_scope = None;
                proposal.clarification_kind = Some(ClarificationKind::Unknown);
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
            let unresolved = goal.inputs.unresolved_inputs();
            let explicit_input = clarification_reference(text)?;
            let mut resolved_ids = BTreeSet::new();
            for resolution in &proposal.resolutions {
                identifier(&resolution.input_id)?;
                let original = unresolved
                    .iter()
                    .find(|input| input.input_id == resolution.input_id)
                    .ok_or_else(|| invalid("resolution targets an input that is not unresolved"))?;
                if !resolved_ids.insert(resolution.input_id.clone())
                    || original.source_requirements_revision
                        != resolution.source_requirements_revision
                    || explicit_input.is_some_and(|id| id != resolution.input_id)
                    || task_reference.as_ref().is_some_and(|id| {
                        original
                            .task_reference
                            .as_ref()
                            .is_some_and(|original| original != id)
                    })
                {
                    return Err(invalid("resolution has a stale or different input scope"));
                }
                match resolution.kind {
                    ResolutionKind::Applied => {
                        let targets_original = if let Some(decision) = &original.permission_decision
                        {
                            proposal
                                .permission_response
                                .as_ref()
                                .is_some_and(|response| &response.decision_id == decision)
                        } else if let Some(task) = &original.task_reference {
                            proposal
                                .task_updates
                                .iter()
                                .any(|update| &update.task_id == task)
                                || proposal.control.as_ref().is_some_and(|control| {
                                    control.target.strip_prefix("task:") == Some(task.as_str())
                                })
                        } else {
                            proposal.objective.is_some()
                                || !proposal.task_updates.is_empty()
                                || proposal.control.is_some()
                                || proposal.permission_response.is_some()
                        };
                        if original.kind == ClarificationKind::Query || !targets_original {
                            return Err(invalid(
                                "an applied resolution requires an actual change in its original scope",
                            ));
                        }
                    }
                    ResolutionKind::Answered => {
                        if original.kind == ClarificationKind::Change || proposal.answer.is_none() {
                            return Err(invalid(
                                "a requested change cannot be resolved by an answer alone",
                            ));
                        }
                    }
                    ResolutionKind::Withdrawn => {
                        if proposal.answer.is_none() {
                            return Err(invalid("withdrawal requires a recorded explanation"));
                        }
                    }
                }
            }
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
            let mut updated_checks = BTreeMap::new();
            for update in &proposal.task_updates {
                message(&update.instruction)?;
                if let Some(resources) = &update.resources {
                    resources.validate()?;
                }
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
                if !update.additional_checks.is_empty() {
                    let task = goal
                        .workflow
                        .tasks
                        .iter()
                        .find(|task| task.spec.id == update.task_id)
                        .ok_or(GoalError::NotFound)?;
                    let mut checks = task.spec.checks.clone();
                    let mut additions = BTreeSet::new();
                    for check in &update.additional_checks {
                        check.validate()?;
                        if !additions.insert(&check.id) {
                            return Err(invalid("additional check ids must be unique"));
                        }
                        if let Some(existing) =
                            checks.iter().find(|existing| existing.id == check.id)
                        {
                            if existing != check {
                                return Err(invalid(
                                    "additional checks cannot replace or weaken an existing check",
                                ));
                            }
                        } else {
                            checks.push(check.clone());
                        }
                    }
                    if checks.len() > 16 {
                        return Err(invalid("task machine checks exceed their bound"));
                    }
                    updated_checks.insert(update.task_id.clone(), checks);
                }
            }
            // Goal-wide requirements apply to every existing task. A model
            // may omit instruction rewrites, but that cannot leave workers
            // authorized under the previous Goal contract. Scoped task edits
            // still invalidate only that task and its dependents.
            if proposal
                .objective
                .as_ref()
                .is_some_and(|objective| objective != &goal.objective)
            {
                affected.extend(goal.workflow.tasks.iter().map(|task| task.spec.id.clone()));
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
                    if let Some(resources) = &update.resources {
                        task.spec.resources = resources.clone();
                    }
                    if let Some(checks) = updated_checks.remove(&update.task_id) {
                        task.spec.checks = checks;
                    }
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
            if control_superseded
                && !requirements_saved
                && proposal.permission_response.is_none()
                && proposal
                    .resolutions
                    .iter()
                    .any(|resolution| resolution.kind == ResolutionKind::Applied)
            {
                return Err(invalid(
                    "a superseded control did not resolve the original request",
                ));
            }
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
            let mut retained = unresolved
                .into_iter()
                .filter(|input| !resolved_ids.contains(&input.input_id))
                .collect::<Vec<_>>();
            if let Some(question) = &proposal.clarification {
                retained.push(UnresolvedInput {
                    input_id: pending.operation.id.clone(),
                    source_requirements_revision: pending.requirements_revision,
                    task_reference: task_reference.clone(),
                    permission_decision: permission_reference(text)?
                        .map(|reference| reference.decision_id.to_owned()),
                    question: question.clone(),
                    kind: proposal.clarification_kind.unwrap_or_default(),
                });
            }
            goal.inputs.unresolved = retained;
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
                clarification_kind: proposal.clarification_kind.unwrap_or_default(),
                modification_scope: proposal.modification_scope.map(|binding| *binding),
                resolutions: proposal.resolutions,
            });
            goal.inputs.pending = None;
        }
    }
    Ok(())
}
