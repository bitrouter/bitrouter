//! Durable decision attempts and context application before SDK preparation.

use super::*;
use crate::core::context_router::evidence::EvidenceSource;
use crate::core::context_router::planner::{CandidateSet, CompileOptions, SourceRevision};
use crate::core::context_router::{DecisionReceipt, ExecutionPlan, FEATURE, WorkUnit};
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
            return Ok(Some(
                bitrouter_sdk::routing::preparation::Prepared::from_prompt(
                    &original, None, step_id,
                )
                .await,
            ));
        }
        let task_id = turn.agent_turn_id.clone();
        let run_id = turn.run_id.clone();
        let source = SourceRevision::capture(
            agent.context_revision,
            snapshot.signals.revision,
            &snapshot.manifest,
            &original,
        )?;
        let runtime = self.shared.app.decision_model().cloned();
        let policy = runtime
            .as_ref()
            .map(|runtime| runtime.policy.clone())
            .unwrap_or_default();
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
        self.transition_for(Some(agent_id), "context.catalogued", |state, _| {
            validate_decision_source(state, agent_id, step_id, &source)?;
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
                            },
                        )
                    })
                })
                .collect();
            let blocks = state.context_store.capture_with_sources(
                &original.messages,
                &provenance,
                &call_sources,
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
        let own = store.capture(&original.messages, &provenance)?;
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
        let mut candidates =
            CandidateSet::prepare(&store, &task_id, ordered, &snapshot.manifest, &policy)?;
        let mut semantic_prompt = original.clone();
        semantic_prompt.system = Some(agent.required_instructions.join("\n"));
        let mut input = bitrouter_sdk::routing::input::Input::from_prompt(
            &semantic_prompt,
            policy.max_request_bytes / 4,
        );
        input.task = Some(bitrouter_sdk::routing::input::Task {
            objective: turn.input.text.clone(),
            acceptance_criteria: turn.input.acceptance_criteria.clone(),
        });
        let mut decision_id = None;
        let mut response = None;
        let mut reason = "conservative_context";
        let needs_assessment = turn.input.routing.model == crate::core::protocol::ModelMode::Policy
            || (turn.input.routing.context == crate::core::protocol::ContextMode::Auto
                && !candidates.candidates.is_empty());
        if needs_assessment
            && let Some(runtime) = runtime
            && let Some(request) = candidates.request(&store, &runtime.model, &policy, &input)?
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
                let attempt_id = id("context_decision");
                let response_limit_bytes = policy
                    .max_request_bytes
                    .saturating_mul(2)
                    .saturating_add(8192);
                self.transition_for(Some(agent_id), "context.decision.intent", |state, head| {
                    budget::ensure(state)?;
                    validate_decision_source(state, agent_id, step_id, &source)?;
                    let limit = state.run.as_ref().map_or(0, |run| run.limits.model_attempts);
                    if state.context_store.decisions.values().filter(|receipt| receipt.run_id == run_id).count() >= limit as usize {
                        return Err(reject(ErrorCode::LimitExceeded, "run decision attempt limit reached"));
                    }
                    state.context_store.decisions.insert(attempt_id.clone(), DecisionReceipt {
                        decision_id: attempt_id.clone(), run_id: run_id.clone(), task_id: task_id.clone(),
                        agent_id: agent_id.into(), source: source.clone(), candidates: candidates.clone(),
                        request: request.clone(), request_sha256: request_sha256.clone(),
                        policy: policy.clone(), pricing: runtime.pricing.clone(),
                        intent_state_revision: head.state_revision + 1, response_limit_bytes,
                        outcome: None, elapsed_ms: None, stale: false, view_ids: Vec::new(),
                    });
                    Ok(json!({"decision_id":attempt_id,"task_id":task_id,"request_sha256":request_sha256}))
                }).await?;
                self.ensure_dispatch_with_budget(agent_id, step_id, attempt_id.clone(), true)
                    .await?;
                let started = Instant::now();
                let mut outcome = tokio::select! {
                    biased;
                    () = control.provider_cancellation.cancelled() => Err(DecisionError {
                        kind: DecisionFailure::Cancelled, message: "decision execution cancelled".into(),
                        usage: None, may_have_run: true,
                    }),
                    outcome = runtime.executor.execute(&request, &control.provider_cancellation) => outcome,
                };
                // Custom executors obey the same wire/usage contract as HTTP.
                if let Ok(result) = &outcome
                    && let Err(error) = result.validate(&request)
                {
                    outcome = Err(error);
                }
                if serde_json::to_vec(&outcome).map_err(json_error)?.len() > response_limit_bytes {
                    let usage = match &outcome {
                        Ok(result) => Some(result.usage),
                        Err(error) => error.usage,
                    };
                    outcome = Err(DecisionError {
                        kind: DecisionFailure::ResponseTooLarge,
                        message: "decision outcome exceeds reserved checkpoint bytes".into(),
                        usage,
                        may_have_run: true,
                    });
                }
                let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
                let active_ms = self.shared.live.lock().await.activity.finish(&attempt_id);
                let mut stale = false;
                let recorded = self
                    .transition_for(Some(agent_id), "context.decision.outcome", |state, _| {
                        stale =
                            validate_decision_source(state, agent_id, step_id, &source).is_err();
                        let receipt = state
                            .context_store
                            .decisions
                            .get_mut(&attempt_id)
                            .filter(|receipt| receipt.outcome.is_none())
                            .ok_or_else(|| {
                                reject(
                                    ErrorCode::OperationConflict,
                                    "decision intent is not pending",
                                )
                            })?;
                        receipt.outcome = Some(outcome.clone());
                        receipt.elapsed_ms = Some(elapsed_ms);
                        receipt.stale = stale;
                        let run = active_run(state)?;
                        run.active_ms = run.active_ms.max(active_ms);
                        Ok(json!({"decision_id":attempt_id,"stale":stale,"elapsed_ms":elapsed_ms}))
                    })
                    .await;
                if let Err(error) = recorded {
                    self.disconnect().await;
                    return Err(error);
                }
                if stale {
                    return Err(reject(
                        ErrorCode::StaleRevision,
                        "decision source changed before application",
                    ));
                }
                reason = if outcome.is_ok() {
                    "typed_context_decision"
                } else {
                    "conservative_decision_failure"
                };
                response = outcome.ok();
                decision_id = Some(attempt_id);
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
        let mut offered = vec![("routed", routed)];
        if let Some(full) = full {
            offered.push(("full", full));
        }
        let views = offered
            .iter()
            .map(|(id, (view, prompt))| crate::core::context_router::models::view(id, view, prompt))
            .collect();
        let current = self.snapshot().await;
        let receipt = decision_id
            .as_ref()
            .and_then(|id| current.context_store.decisions.get(id))
            .map(|receipt| {
                let outcome = receipt.outcome.as_ref();
                let usage = outcome.and_then(|outcome| match outcome {
                    Ok(response) => Some(response.usage),
                    Err(error) => error.usage,
                });
                let decoded = outcome
                    .and_then(|outcome| outcome.as_ref().ok())
                    .map(|response| {
                        bitrouter_sdk::routing::assessment::decode(
                            &receipt.request,
                            response,
                            policy.confidence_threshold,
                        )
                    });
                let (assessment, error) = match decoded {
                    Some(Ok(value)) => (Some(value), None),
                    Some(Err(error)) => (None, Some(error)),
                    None => (
                        None,
                        outcome.and_then(|outcome| outcome.as_ref().err()).cloned(),
                    ),
                };
                bitrouter_sdk::routing::preparation::Receipt {
                    id: receipt.decision_id.clone(),
                    assessment,
                    error,
                    usage,
                }
            });
        *control.routing_pending.lock().await = Some(Pending {
            source,
            task_id,
            decision_id,
            reason: reason.into(),
            views: offered
                .into_iter()
                .map(|(id, (view, _))| (id.into(), view))
                .collect(),
        });
        Ok(Some(bitrouter_sdk::routing::preparation::Prepared {
            receipt,
            views,
            policy,
            previous,
            hard_limit_bytes,
            capabilities: if turn.input.routing.context == super::super::protocol::ContextMode::Auto
            {
                std::collections::BTreeSet::from([
                    bitrouter_sdk::routing::ContextCapability::OmitEvidence,
                    bitrouter_sdk::routing::ContextCapability::UseExtract,
                    bitrouter_sdk::routing::ContextCapability::UseSummary,
                    bitrouter_sdk::routing::ContextCapability::RecallEvidence,
                ])
            } else {
                Default::default()
            },
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
            decision_id,
            reason,
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
            validate_decision_source(state, agent_id, step_id, &source)?;
            let manifest = ContextManifest::capture(state, agent_id, prompt)?;
            current_step(state, agent_id, step_id)?.context = manifest;
            let work =
                state.context_store.work.get_mut(&task_id).ok_or_else(|| {
                    reject(ErrorCode::CheckpointConflict, "context task vanished")
                })?;
            work.last_view_id = Some(view.view_id.clone());
            if let Some(decision_id) = &decision_id {
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
                    decision_id: decision_id.clone(),
                    model: None,
                    routing: routing.clone(),
                },
            );
            Ok(
                json!({"task_id":task_id,"view_id":view.view_id,"decision_id":decision_id,
                "selected_groups":view.selected.len(),"omitted_groups":view.omitted.len(),
                "prompt_bytes":view.prompt_bytes,"reason":reason}),
            )
        })
        .await?;
        Ok(())
    }
}

pub(super) struct Pending {
    source: SourceRevision,
    task_id: String,
    decision_id: Option<String>,
    reason: String,
    views: std::collections::BTreeMap<String, crate::core::context_router::planner::ContextView>,
}

fn validate_decision_source(
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
            "decision evidence inventory changed",
        ));
    }
    Ok(())
}
