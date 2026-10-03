//! Deterministic context allocation before shared model preparation. Admission
//! here proves context eligibility, not provider capability or token capacity.

use bitrouter_sdk::language_model::types::Message;
use serde::{Deserialize, Serialize};

use super::collaboration::{Assignment, Work};
use super::protocol::{CoreError, ErrorCode, MaterialRef, TaskInput};
use super::routing::ContextManifest;
use super::session::{AgentState, SessionSnapshot};

/// Facts on which retained history depends. Keep all distinct sources across
/// turns: a later model step cannot certify older evidence as newly observed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContextSource {
    pub permission_revision: u64,
    pub workspace_revision: Option<String>,
    pub tool_manifest_digest: String,
    pub materials: Vec<MaterialRef>,
}

impl ContextSource {
    pub fn capture(context: &ContextManifest) -> Self {
        Self {
            permission_revision: context.permission_revision,
            workspace_revision: context.workspace_revision.clone(),
            tool_manifest_digest: context.tool_manifest_digest.clone(),
            materials: context.materials.clone(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextKind {
    Reuse,
    Fresh,
    Inherited,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextCandidate {
    pub candidate_id: String,
    pub kind: ContextKind,
    pub agent_id: Option<String>,
    pub context_revision: Option<u64>,
    /// Empty means eligible for allocation. Model feasibility remains a
    /// separate, mandatory gate before any provider attempt.
    pub rejection_reasons: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextAllocation {
    pub allocation_id: String,
    pub policy_id: String,
    pub source: String,
    pub input_state_revision: u64,
    pub signal_revision: u64,
    pub actor_id: String,
    pub actor_context_revision: u64,
    pub assignment_id: String,
    pub task: Work,
    pub input: TaskInput,
    pub candidates: Vec<ContextCandidate>,
    pub selected_candidate_id: Option<String>,
    pub selected_agent_id: Option<String>,
    pub error: Option<CoreError>,
    pub applied_state_revision: Option<u64>,
    pub application_error: Option<CoreError>,
}

pub(crate) enum Intent<'a> {
    Spawn,
    Delegate(Option<&'a str>),
}

pub(crate) fn choose(
    state: &SessionSnapshot,
    actor_id: &str,
    task: &Work,
    work: &Assignment,
    intent: Intent<'_>,
    revision: u64,
) -> Result<ContextAllocation, CoreError> {
    let actor = state.agents.get(actor_id).ok_or_else(|| {
        CoreError::rejected(ErrorCode::UnauthorizedScope, "unknown allocation actor")
    })?;
    let run = state
        .run
        .as_ref()
        .ok_or_else(|| CoreError::rejected(ErrorCode::Busy, "allocation requires an active run"))?;
    let mut common = Vec::new();
    if run.model_attempts >= run.limits.model_attempts {
        common.push("model_attempt_budget_exhausted".into());
    }
    if run.active_ms >= run.limits.active_seconds.saturating_mul(1000) {
        common.push("active_time_budget_exhausted".into());
    }
    common.extend(material_rejections(state, &work.input));
    let source = match intent {
        Intent::Spawn => "spawn_agent",
        Intent::Delegate(_) => "delegate_task",
    };
    let mut candidates = Vec::new();
    if let Intent::Delegate(target) = intent {
        let ids: Vec<&str> = match target {
            Some(target) => vec![target],
            None => state
                .agents
                .keys()
                .filter(|id| **id != state.agent_id && id.as_str() != actor_id)
                .map(String::as_str)
                .collect(),
        };
        for id in ids {
            let agent = state.agents.get(id);
            let mut reasons = common.clone();
            if task.fresh_context || task.independent_review {
                reasons.push("isolated_context_required".into());
            }
            if id == state.agent_id || id == actor_id {
                reasons.push("non_root_worker_required".into());
            }
            if let Some(agent) = agent {
                reasons.extend(reuse_rejections(state, actor_id, task, &work.input, agent));
            } else {
                reasons.push("unknown_target".into());
            }
            candidates.push(ContextCandidate {
                candidate_id: format!("reuse:{id}"),
                kind: ContextKind::Reuse,
                agent_id: Some(id.into()),
                context_revision: agent.map(|agent| agent.context_revision),
                rejection_reasons: reasons,
            });
        }
    }
    if !matches!(intent, Intent::Delegate(Some(_))) {
        let kind =
            if matches!(intent, Intent::Spawn) && !task.fresh_context && !task.independent_review {
                ContextKind::Inherited
            } else {
                // Ambiguous automatic reuse always falls back to a fresh context.
                ContextKind::Fresh
            };
        let mut reasons = common;
        if state.agents.len() >= run.limits.agents as usize {
            reasons.push("agent_capacity_exhausted".into());
        }
        if actor.depth >= run.limits.child_depth {
            reasons.push("child_depth_exhausted".into());
        }
        if kind == ContextKind::Inherited && !valid_history(inherited_history(actor)) {
            reasons.push("unpaired_history".into());
        }
        candidates.push(ContextCandidate {
            candidate_id: format!("new:{}", work.assignment_id),
            kind,
            agent_id: None,
            context_revision: None,
            rejection_reasons: reasons,
        });
    }
    // BTreeMap iteration gives stable agent-ID tie breaking. A fresh candidate
    // follows every eligible existing worker; an exact target has no fallback.
    let selected = candidates
        .iter()
        .find(|candidate| candidate.rejection_reasons.is_empty());
    let error = selected.is_none().then(|| {
        let resource_only = candidates.iter().all(|candidate| {
            candidate.rejection_reasons.iter().all(|reason| {
                matches!(
                    reason.as_str(),
                    "agent_capacity_exhausted"
                        | "child_depth_exhausted"
                        | "model_attempt_budget_exhausted"
                        | "active_time_budget_exhausted"
                )
            })
        });
        CoreError::rejected(
            if resource_only {
                ErrorCode::LimitExceeded
            } else {
                ErrorCode::NoFeasibleRoute
            },
            "no eligible context allocation; inspect the retained allocation candidates",
        )
    });
    Ok(ContextAllocation {
        allocation_id: format!("allocation_{}", uuid::Uuid::new_v4()),
        policy_id: "core_rules_v1".into(),
        source: source.into(),
        input_state_revision: revision,
        signal_revision: state.signals.revision,
        actor_id: actor_id.into(),
        actor_context_revision: actor.context_revision,
        assignment_id: work.assignment_id.clone(),
        task: task.clone(),
        input: work.input.clone(),
        selected_candidate_id: selected.map(|candidate| candidate.candidate_id.clone()),
        selected_agent_id: selected.and_then(|candidate| candidate.agent_id.clone()),
        candidates,
        error,
        applied_state_revision: None,
        application_error: None,
    })
}

fn reuse_rejections(
    state: &SessionSnapshot,
    actor_id: &str,
    task: &Work,
    input: &TaskInput,
    agent: &AgentState,
) -> Vec<String> {
    let mut reasons = Vec::new();
    if task
        .task_scope
        .as_ref()
        .is_none_or(|scope| scope.trim().is_empty())
        || agent.task_scope != task.task_scope
    {
        reasons.push("task_scope_mismatch".into());
    }
    if !agent.queue.is_empty()
        || agent.turn.as_ref().is_none_or(|turn| {
            !turn.status.terminal()
                || !turn.notified
                || turn.steps.iter().any(|step| {
                    !step.settled
                        || step
                            .attempts
                            .iter()
                            .any(|attempt| attempt.receipt.is_none())
                })
                || turn
                    .invocations
                    .iter()
                    .any(|call| !call.consumed || call.result.is_none())
                || turn
                    .core_calls
                    .iter()
                    .any(|call| !call.consumed || call.result.is_none())
        })
        || super::session::pending_dependencies(state, &agent.agent_id)
    {
        reasons.push("worker_not_idle".into());
    }
    if super::collaboration::assignment_cycle(state, &agent.agent_id, actor_id) {
        reasons.push("dependency_cycle".into());
    }
    if !valid_history(&agent.history) {
        reasons.push("unpaired_history".into());
    }
    reasons.extend(source_rejections(state, agent));
    let input = reuse_input(input, agent);
    reasons.extend(material_rejections(state, &input));
    if state
        .run
        .as_ref()
        .is_some_and(|run| super::session::validate_input(&input, &run.limits).is_err())
    {
        reasons.push("retained_context_input_limit".into());
    }
    reasons
}

pub(crate) fn reuse_input(input: &TaskInput, agent: &AgentState) -> TaskInput {
    let mut input = input.clone();
    for material in agent
        .context_sources
        .iter()
        .flat_map(|source| &source.materials)
    {
        if !input.required_materials.contains(&material.material_id) {
            input.required_materials.push(material.material_id.clone());
        }
    }
    input
}

fn material_rejections(state: &SessionSnapshot, input: &TaskInput) -> Vec<String> {
    let Ok(materials) = super::session::selected_materials(state, input) else {
        return vec!["required_material_missing".into()];
    };
    if materials.iter().any(|material| {
        material.content.is_none()
            && state.signals.requests.values().any(|request| {
                request.signal_revision == state.signals.revision
                    && request.unavailable_reason.is_some()
                    && super::signals::same_reference(&request.reference, material)
            })
    }) {
        vec!["required_material_unavailable".into()]
    } else {
        Vec::new()
    }
}

fn source_rejections(state: &SessionSnapshot, agent: &AgentState) -> Vec<String> {
    let mut reasons = Vec::new();
    if agent.context_sources.is_empty() {
        reasons.push("context_provenance_unknown".into());
    }
    if state.manifest.workspace_revision.is_none()
        || agent
            .context_sources
            .iter()
            .any(|source| source.workspace_revision.is_none())
    {
        reasons.push("workspace_revision_unknown".into());
    } else if agent
        .context_sources
        .iter()
        .any(|source| source.workspace_revision != state.manifest.workspace_revision)
    {
        reasons.push("workspace_revision_changed".into());
    }
    if agent
        .context_sources
        .iter()
        .any(|source| source.permission_revision != state.manifest.permission_revision)
    {
        reasons.push("permission_revision_changed".into());
    }
    if agent
        .context_sources
        .iter()
        .any(|source| source.tool_manifest_digest != state.manifest.tool_manifest_digest)
    {
        reasons.push("tool_manifest_changed".into());
    }
    if agent
        .context_sources
        .iter()
        .flat_map(|source| &source.materials)
        .any(|reference| {
            state
                .signals
                .materials
                .get(&reference.material_id)
                .is_none_or(|current| !super::signals::same_reference(reference, current))
        })
    {
        reasons.push("material_version_changed".into());
    }
    reasons
}

/// A reservation does not make its source facts permanently valid. Check the
/// selected worker again at activation and before its first model dispatch.
pub(crate) fn validate_reuse(
    state: &SessionSnapshot,
    agent: &AgentState,
    allocation_id: &str,
    input: &TaskInput,
    activating: bool,
) -> Result<(), CoreError> {
    let allocation = state.allocations.get(allocation_id).ok_or_else(|| {
        CoreError::rejected(ErrorCode::CheckpointConflict, "missing task allocation")
    })?;
    let candidate = allocation
        .candidates
        .iter()
        .find(|candidate| {
            Some(&candidate.candidate_id) == allocation.selected_candidate_id.as_ref()
        })
        .ok_or_else(|| {
            CoreError::rejected(ErrorCode::CheckpointConflict, "missing selected context")
        })?;
    if candidate.kind != ContextKind::Reuse {
        return Ok(());
    }
    let mut reasons = source_rejections(state, agent);
    // Signals can pin additional material IDs after the decision was made.
    // Its original input remains evidence; admission uses the effective work.
    reasons.extend(material_rejections(state, input));
    if state
        .run
        .as_ref()
        .is_some_and(|run| super::session::validate_input(input, &run.limits).is_err())
    {
        reasons.push("retained_context_input_limit".into());
    }
    if activating && candidate.context_revision != Some(agent.context_revision) {
        reasons.push("context_revision_changed".into());
    }
    if allocation.selected_agent_id.as_ref() != Some(&agent.agent_id) {
        reasons.push("selected_agent_changed".into());
    }
    if reasons.is_empty() {
        Ok(())
    } else {
        Err(CoreError::rejected(
            ErrorCode::NoFeasibleRoute,
            format!(
                "reserved context is no longer eligible: {}",
                reasons.join(", ")
            ),
        ))
    }
}

pub(crate) fn inherited_history(agent: &AgentState) -> &[Message] {
    agent
        .turn
        .as_ref()
        .and_then(|turn| {
            // A reconstruction candidate is durable before it is authorized.
            // Runtime delegation must inherit the last activated source instead.
            turn.steps.iter().rev().find(|step| {
                step.reconstructed_from.is_none()
                    || step
                        .context_validation
                        .as_ref()
                        .is_some_and(|record| record.applied)
            })
        })
        .map_or(&[], |step| &step.input_history)
}

fn valid_history(history: &[Message]) -> bool {
    crate::context::validate_history(history).is_ok()
}
