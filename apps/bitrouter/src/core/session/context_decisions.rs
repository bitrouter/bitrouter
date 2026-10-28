//! Full ordinary context and sealed, tenancy-owned Choice preparation.

use super::*;
#[cfg(feature = "harness")]
use crate::core::context_router::owned::CompileOptions;
use crate::core::context_router::planner;
use crate::gateway::model::decision::types::{DecisionError, DecisionFailure};
use crate::storage::checkpoint::snapshot::context::evidence::EvidenceSource;
#[cfg(feature = "harness")]
use crate::storage::checkpoint::snapshot::context::view::CandidateSet;
use crate::storage::checkpoint::snapshot::context::view::SourceRevision;
use crate::storage::checkpoint::snapshot::context::{ExecutionPlan, FEATURE, WorkUnit};

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
                    let evidence =
                        crate::storage::checkpoint::snapshot::context::evidence::EvidenceArtifact {
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
    ) -> Result<Option<crate::gateway::routing::preparation::Prepared>, CoreError> {
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
            let mut prepared = crate::gateway::routing::preparation::Prepared::from_prompt(
                &original, None, step_id,
            )
            .await;
            if portable_reasoning(&snapshot, turn) {
                prepared
                    .capabilities
                    .insert(crate::gateway::routing::ContextCapability::OmitPrivateReasoning);
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

        let policy = crate::gateway::model::decision::policy::DecisionPolicy::default();
        #[cfg(feature = "harness")]
        let owned_profile = control
            .owned_step
            .as_ref()
            .map(|step| step.choice_profile())
            .transpose()
            .map_err(crate::tenancy::execution::session::core_error)?
            .flatten();
        #[cfg(feature = "harness")]
        let policy = owned_profile
            .as_ref()
            .map_or(policy, |profile| profile.policy.clone());
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
        let skill_sources = crate::storage::skills::state::message_bindings(&snapshot, turn)?;
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
        let mut capabilities = std::collections::BTreeSet::new();
        if portable_reasoning(&snapshot, turn) {
            capabilities.insert(crate::gateway::routing::ContextCapability::OmitPrivateReasoning);
        }
        // Ordinary execution never consults a host decision model or historical
        // decision outcome. Only the sealed owned profile admits the Choice path.
        let full = || -> Result<_, CoreError> {
            let (view, prompt) = planner::compile_full(
                &store,
                &task_id,
                ordered.clone(),
                &original,
                source.clone(),
                hard_limit_bytes,
            )?;
            Ok((vec![("full", (view, prompt))], None))
        };
        #[cfg(feature = "harness")]
        let (offered, receipt) = if let Some(profile) = owned_profile {
            let mut candidates = CandidateSet::prepare(
                &store,
                &task_id,
                ordered.clone(),
                &snapshot.manifest,
                &policy,
            )?;
            let mut semantic_prompt = original.clone();
            semantic_prompt.system = Some(agent.required_instructions.join("\n"));
            let mut input = crate::gateway::routing::input::Input::from_prompt(
                &semantic_prompt,
                policy.max_request_bytes / 4,
            );
            input.task = Some(crate::gateway::routing::input::Task {
                objective: turn.input.text.clone(),
                acceptance_criteria: turn.input.acceptance_criteria.clone(),
            });
            let mut decision_id = None;
            let mut response = None;
            let mut reason = "conservative_context";
            let needs_assessment = turn.input.routing.model
                == bitrouter_ai::core::protocol::ModelMode::Policy
                || (turn.input.routing.context == bitrouter_ai::core::protocol::ContextMode::Auto
                    && !candidates.candidates.is_empty());
            if needs_assessment
                && let Some((request, native)) =
                    choice::request(&mut candidates, &store, &profile, &input)?
            {
                let request_sha256 = digest(&request)?;
                let prior = store.decisions.values().find(|receipt| {
                    receipt.task_id == task_id
                        && receipt.source == source
                        && receipt.request_sha256 == request_sha256
                        && receipt.policy == policy
                        && !receipt.stale
                });
                if let Some(prior) = prior {
                    self.validate_owned_choice_reuse(prior, &profile).await?;
                    decision_id = Some(prior.decision_id.clone());
                    response = prior
                        .outcome
                        .as_ref()
                        .and_then(|outcome| outcome.as_ref().ok())
                        .cloned();
                    reason = if response.is_some() {
                        "reused_committed_decision"
                    } else {
                        "conservative_interrupted_decision"
                    };
                } else {
                    #[cfg(all(test, unix, feature = "harness"))]
                    if std::env::var("BITROUTER_TEST_STOPPED_NATIVE_MODE").as_deref()
                        == Ok("shared-parent")
                    {
                        // Both real callback invocations must observe absence before
                        // either commits intent. This changes scheduling only in the
                        // isolated child; the ordinary reuse and send gates still run.
                        static READY: std::sync::LazyLock<tokio::sync::Barrier> =
                            std::sync::LazyLock::new(|| tokio::sync::Barrier::new(2));
                        READY.wait().await;
                    }
                    let completed = self
                        .owned_choice(
                            control,
                            step_id,
                            source.clone(),
                            candidates.clone(),
                            (request, native),
                            profile,
                        )
                        .await?;
                    decision_id = Some(completed.id);
                    response = completed.response;
                    reason = if response.is_some() {
                        "typed_context_decision"
                    } else {
                        "conservative_decision_failure"
                    };
                }
            }
            let routed = candidates.compile(
                &store,
                &original,
                CompileOptions {
                    source: source.clone(),
                    policy: &policy,
                    response: response.as_ref(),
                    decision_id: decision_id.clone(),
                    reason,
                    hard_limit_bytes,
                },
            )?;
            let full = match candidates.compile(
                &store,
                &original,
                CompileOptions {
                    source: source.clone(),
                    policy: &policy,
                    response: None,
                    decision_id: decision_id.clone(),
                    reason: "full_context_candidate",
                    hard_limit_bytes,
                },
            ) {
                Ok(full) => Some(full),
                Err(error) if error.code == ErrorCode::NoFeasibleRoute => None,
                Err(error) => return Err(error),
            };
            let mut offered = vec![("routed", routed)];
            if let Some(full) = full {
                offered.push(("full", full));
            }
            let current = self.snapshot().await;
            let receipt = decision_id
                .as_ref()
                .and_then(|id| current.context_store.decisions.get(id))
                .map(crate::core::context_router::models::receipt);
            if turn.input.routing.context == bitrouter_ai::core::protocol::ContextMode::Auto {
                capabilities.extend([
                    crate::gateway::routing::ContextCapability::OmitEvidence,
                    crate::gateway::routing::ContextCapability::UseExtract,
                    crate::gateway::routing::ContextCapability::UseSummary,
                    crate::gateway::routing::ContextCapability::RecallEvidence,
                ]);
            }
            (offered, receipt)
        } else {
            full()?
        };
        #[cfg(not(feature = "harness"))]
        let (offered, receipt) = full()?;
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
        let views = offered
            .iter()
            .map(|(id, (view, prompt))| crate::core::context_router::models::view(id, view, prompt))
            .collect();
        *control.routing_pending.lock().await = Some(Pending {
            source,
            task_id,
            views: offered
                .into_iter()
                .map(|(id, (view, _))| (id.into(), view))
                .collect(),
        });
        Ok(Some(crate::gateway::routing::preparation::Prepared {
            receipt,
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
        plan: &crate::gateway::routing::plan::Plan,
    ) -> Result<(), CoreError> {
        let Pending {
            source,
            task_id,
            mut views,
        } = pending;
        let mut view = views
            .remove(&plan.selection.selected_context)
            .ok_or_else(|| {
                reject(
                    ErrorCode::OperationConflict,
                    "selected context was not offered",
                )
            })?;
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
            if let Some(decision_id) = &view.decision_id {
                let receipt = state
                    .context_store
                    .decisions
                    .get_mut(decision_id)
                    .ok_or_else(|| {
                        reject(ErrorCode::CheckpointConflict, "context receipt vanished")
                    })?;
                if !receipt.view_ids.contains(&view.view_id) {
                    receipt.view_ids.push(view.view_id.clone());
                }
            }
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
                    decision_id: view.decision_id.clone(),
                    model: None,
                    routing: routing.clone(),
                },
            );
            Ok(
                json!({"task_id":task_id,"view_id":view.view_id,"decision_id":view.decision_id,
                "selected_groups":view.selected.len(),"omitted_groups":view.omitted.len(),
                "prompt_bytes":view.prompt_bytes,"reason":view.reason}),
            )
        })
        .await?;
        Ok(())
    }
}

fn portable_reasoning(state: &SessionSnapshot, turn: &AgentTurn) -> bool {
    turn.input.routing.context == bitrouter_ai::core::protocol::ContextMode::Auto
        && state.manifest.required_features.iter().any(|feature| {
            feature == crate::storage::checkpoint::snapshot::context::PORTABLE_RESPONSES_REASONING
        })
}

pub(super) struct Pending {
    source: SourceRevision,
    task_id: String,
    views: BTreeMap<String, crate::storage::checkpoint::snapshot::context::view::ContextView>,
}

#[cfg(feature = "harness")]
pub(super) fn validate_decision_source(
    state: &SessionSnapshot,
    agent_id: &str,
    step_id: &str,
    source: &SourceRevision,
) -> Result<(), CoreError> {
    validate_context_source(state, agent_id, step_id, source)
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
