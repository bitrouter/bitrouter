//! Full context preparation and historical decision reconciliation.

use super::*;
use crate::core::context_router::evidence::EvidenceSource;
use crate::core::context_router::planner::{self, SourceRevision};
use crate::core::context_router::{ExecutionPlan, FEATURE, WorkUnit};
use bitrouter_sdk::decision_model::types::{DecisionError, DecisionFailure};

pub(super) fn enabled(state: &SessionSnapshot) -> bool {
    state
        .manifest
        .required_features
        .iter()
        .any(|feature| feature == FEATURE)
}

/// Stopped-owner recovery resolves reservation ownership, not provider billing.
/// Unknown attempts remain durable failures and cannot authorize omission.
pub(super) fn reconcile(state: &mut SessionSnapshot) {
    for receipt in state
        .context_store
        .decisions
        .values_mut()
        .filter(|receipt| receipt.outcome.is_none())
    {
        receipt.outcome = Some(Err(DecisionError {
            kind: DecisionFailure::Interrupted,
            message: "previous owner stopped before recording the decision outcome".into(),
            usage: None,
            may_have_run: true,
        }));
    }
}

pub(super) fn reserved(state: &SessionSnapshot) -> Result<u64, CoreError> {
    state
        .context_store
        .decisions
        .values()
        .filter(|receipt| receipt.outcome.is_none())
        .try_fold(0_u64, |total, receipt| {
            let bytes = u64::try_from(receipt.response_limit_bytes)
                .ok()
                .and_then(|bytes| bytes.checked_add(4096));
            bytes
                .and_then(|bytes| total.checked_add(bytes))
                .ok_or_else(|| {
                    reject(
                        ErrorCode::LimitExceeded,
                        "decision outcome reservation overflow",
                    )
                })
        })
}

pub(super) fn synchronize(state: &mut SessionSnapshot) -> Result<(), CoreError> {
    if !enabled(state) {
        return Ok(());
    }
    for (agent_id, agent) in &state.agents {
        let Some(turn) = &agent.turn else { continue };
        let parent_task_id = state
            .agents
            .get(&turn.assigned_by)
            .and_then(|parent| parent.turn.as_ref())
            .filter(|parent| parent.agent_turn_id != turn.agent_turn_id)
            .map(|turn| turn.agent_turn_id.clone());
        let shared = crate::core::context_router::tasks::inherited(state, turn);
        let work = state
            .context_store
            .work
            .entry(turn.agent_turn_id.clone())
            .or_insert_with(|| WorkUnit {
                task_id: turn.agent_turn_id.clone(),
                run_id: turn.run_id.clone(),
                agent_id: agent_id.clone(),
                parent_task_id,
                text: turn.input.text.clone(),
                acceptance_criteria: turn.input.acceptance_criteria.clone(),
                instructions: agent.required_instructions.clone(),
                evidence: shared.clone(),
                shared_evidence: shared,
                recalled: Vec::new(),
                last_view_id: None,
                status: turn.status,
                result_evidence: Vec::new(),
                skills: None,
            });
        if work.agent_id != *agent_id || work.run_id != turn.run_id {
            return Err(reject(
                ErrorCode::CheckpointConflict,
                "task identity changed",
            ));
        }
        work.status = turn.status;
        work.instructions = agent.required_instructions.clone();
        for call in &turn.invocations {
            if let Some(result) = &call.result {
                for reference in &result.evidence {
                    let evidence = crate::core::context_router::evidence::EvidenceArtifact {
                        reference: reference.clone(),
                        source: EvidenceSource {
                            task_id: turn.agent_turn_id.clone(),
                            agent_id: agent_id.clone(),
                            workspace_id: call.dispatch.workspace_id.clone(),
                            workspace_revision: result.workspace_revision.clone(),
                            permission_revision: call.dispatch.permission_revision,
                            tool_manifest_digest: call.dispatch.tool_manifest_digest.clone(),
                            skill_binding: None,
                        },
                    };
                    let key = format!("{}:{}", turn.agent_turn_id, reference.artifact_id);
                    if let Some(prior) = state.context_store.artifacts.insert(key, evidence.clone())
                        && prior != evidence
                    {
                        return Err(reject(
                            ErrorCode::CheckpointConflict,
                            "tool evidence artifact changed its provenance",
                        ));
                    }
                }
            }
        }
    }
    Ok(())
}

impl CoreSession {
    pub(super) async fn prepare_context(
        &self,
        agent_id: &str,
        step_id: &str,
        original: Prompt,
        control: &StepControl,
    ) -> Result<Option<bitrouter_sdk::routing::preparation::Prepared>, CoreError> {
        let snapshot = self.snapshot().await;
        let agent = snapshot
            .agents
            .get(agent_id)
            .ok_or_else(|| reject(ErrorCode::Busy, "context agent is absent"))?;
        let turn = agent
            .turn
            .as_ref()
            .ok_or_else(|| reject(ErrorCode::Busy, "context task is absent"))?;
        if !enabled(&snapshot) {
            let mut prepared = bitrouter_sdk::routing::preparation::Prepared::from_prompt(
                &original, None, step_id,
            )
            .await;
            if portable_reasoning(&snapshot, turn) {
                prepared
                    .capabilities
                    .insert(bitrouter_sdk::routing::ContextCapability::OmitPrivateReasoning);
            }
            return Ok(Some(prepared));
        }
        let task_id = turn.agent_turn_id.clone();
        let source = SourceRevision::capture(
            agent.context_revision,
            snapshot.signals.revision,
            &snapshot.manifest,
            &original,
        )?;

        let policy = bitrouter_sdk::decision_model::policy::DecisionPolicy::default();
        policy
            .validate()
            .map_err(|message| reject(ErrorCode::NoFeasibleRoute, &message))?;
        let hard_limit_bytes = turn.input.context_limit_bytes.unwrap_or(512 * 1024);
        let hard_limit_bytes = usize::try_from(hard_limit_bytes).map_err(|_| {
            reject(
                ErrorCode::LimitExceeded,
                "context limit exceeds address space",
            )
        })?;
        let skill_sources = skills::message_bindings(&snapshot, turn)?;
        self.transition_for(Some(agent_id), "context.catalogued", |state, _| {
            validate_context_source(state, agent_id, step_id, &source)?;
            let provenance = EvidenceSource::from_manifest(&task_id, agent_id, &state.manifest);
            let call_sources = state
                .agents
                .get(agent_id)
                .and_then(|agent| agent.turn.as_ref())
                .into_iter()
                .flat_map(|turn| &turn.invocations)
                .filter_map(|call| {
                    call.result.as_ref().map(|result| {
                        (
                            call.provider_call_id.clone(),
                            EvidenceSource {
                                task_id: task_id.clone(),
                                agent_id: agent_id.into(),
                                workspace_id: call.dispatch.workspace_id.clone(),
                                workspace_revision: result.workspace_revision.clone(),
                                permission_revision: call.dispatch.permission_revision,
                                tool_manifest_digest: call.dispatch.tool_manifest_digest.clone(),
                                skill_binding: None,
                            },
                        )
                    })
                })
                .collect();
            let blocks = state.context_store.capture_with_sources(
                &original.messages,
                &provenance,
                &call_sources,
                &skill_sources,
            )?;
            let work = state.context_store.work.get_mut(&task_id).ok_or_else(|| {
                reject(ErrorCode::CheckpointConflict, "context work unit is absent")
            })?;
            for block in &blocks {
                if !work.evidence.contains(block) {
                    work.evidence.push(block.clone());
                }
            }
            Ok(json!({"task_id":task_id,"blocks":blocks}))
        })
        .await?;
        let snapshot = self.snapshot().await;
        let provenance = EvidenceSource::from_manifest(&task_id, agent_id, &snapshot.manifest);
        let mut store = snapshot.context_store.clone();
        let own = store.capture_with_sources(
            &original.messages,
            &provenance,
            &BTreeMap::new(),
            &skill_sources,
        )?;
        let mut ordered = store
            .work
            .get(&task_id)
            .map(|work| work.shared_evidence.clone())
            .unwrap_or_default();
        for block in own {
            if !ordered.contains(&block) {
                ordered.push(block);
            }
        }
        if let Some(work) = store.work.get(&task_id) {
            for block in &work.recalled {
                if !ordered.contains(block) {
                    ordered.push(block.clone());
                }
            }
        }
        let (view, prompt) = planner::compile_full(
            &store,
            &task_id,
            ordered,
            &original,
            source.clone(),
            hard_limit_bytes,
        )?;
        let previous = store
            .work
            .get(&task_id)
            .and_then(|work| work.last_view_id.as_ref())
            .and_then(|view| {
                store
                    .executions
                    .values()
                    .find(|execution| &execution.view_id == view)
            })
            .and_then(|execution| execution.model.clone());
        let views = vec![crate::core::context_router::models::view(
            "full", &view, &prompt,
        )];
        *control.routing_pending.lock().await = Some(Pending {
            source,
            task_id,
            view,
        });
        let mut capabilities = std::collections::BTreeSet::new();
        if portable_reasoning(&snapshot, turn) {
            capabilities.insert(bitrouter_sdk::routing::ContextCapability::OmitPrivateReasoning);
        }
        Ok(Some(bitrouter_sdk::routing::preparation::Prepared {
            receipt: None,
            views,
            policy,
            previous,
            hard_limit_bytes,
            capabilities,
        }))
    }

    pub(super) async fn commit_context(
        &self,
        agent_id: &str,
        step_id: &str,
        pending: Pending,
        plan: &bitrouter_sdk::routing::plan::Plan,
    ) -> Result<(), CoreError> {
        let Pending {
            source,
            task_id,
            mut view,
        } = pending;
        if plan.selection.selected_context != "full" {
            return Err(reject(
                ErrorCode::OperationConflict,
                "selected context was not offered",
            ));
        }
        let prompt = &plan.prompt;
        view.prompt_sha256 = digest(prompt)?;
        view.prompt_bytes = serde_json::to_vec(prompt).map_err(json_error)?.len();
        view.view_id = crate::core::context_router::planner::view_identity(&view)?;
        let routing = Some(plan.selection.clone());
        self.transition_for(Some(agent_id), "context.view.applied", |state, _| {
            validate_context_source(state, agent_id, step_id, &source)?;
            let manifest = ContextManifest::capture(state, agent_id, prompt)?;
            current_step(state, agent_id, step_id)?.context = manifest;
            let work =
                state.context_store.work.get_mut(&task_id).ok_or_else(|| {
                    reject(ErrorCode::CheckpointConflict, "context task vanished")
                })?;
            work.last_view_id = Some(view.view_id.clone());
            state
                .context_store
                .views
                .insert(view.view_id.clone(), view.clone());
            state.context_store.executions.insert(
                step_id.into(),
                ExecutionPlan {
                    step_id: step_id.into(),
                    task_id: task_id.clone(),
                    agent_id: agent_id.into(),
                    view_id: view.view_id.clone(),
                    decision_id: None,
                    model: None,
                    routing: routing.clone(),
                },
            );
            Ok(
                json!({"task_id":task_id,"view_id":view.view_id,"decision_id":null,
                "selected_groups":view.selected.len(),"omitted_groups":view.omitted.len(),
                "prompt_bytes":view.prompt_bytes,"reason":view.reason}),
            )
        })
        .await?;
        Ok(())
    }
}

fn portable_reasoning(state: &SessionSnapshot, turn: &AgentTurn) -> bool {
    turn.input.routing.context == super::super::protocol::ContextMode::Auto
        && state
            .manifest
            .required_features
            .iter()
            .any(|feature| feature == crate::core::context_router::PORTABLE_RESPONSES_REASONING)
}

pub(super) struct Pending {
    source: SourceRevision,
    task_id: String,
    view: crate::core::context_router::planner::ContextView,
}

fn validate_context_source(
    state: &SessionSnapshot,
    agent_id: &str,
    step_id: &str,
    source: &SourceRevision,
) -> Result<(), CoreError> {
    validate_step_source(state, agent_id, step_id)?;
    if state
        .agents
        .get(agent_id)
        .is_none_or(|agent| agent.context_revision != source.context_revision)
    {
        return Err(reject(
            ErrorCode::StaleRevision,
            "context evidence inventory changed",
        ));
    }
    Ok(())
}
