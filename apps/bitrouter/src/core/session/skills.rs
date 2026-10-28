//! Native skills are frozen before model/context preparation, in this Turn's
//! original checkpoint. Only explicitly included sources are selected.

use super::*;
use crate::storage::checkpoint::snapshot::context::view::SourceRevision;
use crate::storage::skills::state::*;
use crate::storage::skills::{SkillCandidate, SkillSelection, SkillSource};
use bitrouter_ai::core::skills::SkillSelector;

impl CoreSession {
    pub(super) async fn prepare_skills(&self, agent_id: &str) -> Result<bool, CoreError> {
        let snapshot = self.snapshot().await;
        let agent = snapshot
            .agents
            .get(agent_id)
            .ok_or_else(|| reject(ErrorCode::Busy, "skill agent is absent"))?;
        let turn = agent
            .turn
            .as_ref()
            .ok_or_else(|| reject(ErrorCode::Busy, "skill Turn is absent"))?;
        if turn.status != AgentStatus::Runnable || turn.final_answer.is_some() {
            return Ok(false);
        }
        if !context_decisions::enabled(&snapshot)
            || !snapshot
                .manifest
                .required_features
                .iter()
                .any(|feature| feature == crate::storage::skills::FEATURE)
        {
            if turn
                .input
                .routing
                .skills
                .as_ref()
                .is_some_and(|constraints| !constraints.include.is_empty())
            {
                return Err(reject(
                    ErrorCode::UnsupportedCapability,
                    "skill selection requires native context negotiation",
                ));
            }
            return Ok(false);
        }
        let Some(selection) = snapshot.skill_selection(&turn.agent_turn_id) else {
            // Legacy work that already dispatched must not acquire new skills
            // retrospectively. Its existing frozen prompt remains authoritative.
            if !turn.steps.is_empty() {
                return Ok(false);
            }
            self.authorize_dispatch().await?;
            let Some(mut catalog) = self.shared.harness.skill_catalog().await? else {
                if turn
                    .input
                    .routing
                    .skills
                    .as_ref()
                    .is_some_and(|constraints| !constraints.include.is_empty())
                {
                    return Err(reject(
                        ErrorCode::UnsupportedCapability,
                        "this host cannot satisfy required skill constraints",
                    ));
                }
                return Ok(false);
            };
            let constraints = turn.input.routing.skills.clone().unwrap_or_default();
            // A partial listing is not proof of absence. Resolve only explicit
            // required URIs before freezing this Turn's complete metadata.
            // An impossible required count must not trigger needless lookups.
            if constraints.include.len() <= 4 {
                for selector in &constraints.include {
                    let SkillSelector::Mcp { server, uri } = selector else {
                        continue;
                    };
                    if catalog
                        .mcp
                        .skills
                        .iter()
                        .any(|held| held.origin.server == *server && held.entry.uri == *uri)
                    {
                        continue;
                    }
                    self.authorize_dispatch().await?;
                    let cancel = self.shared.live.lock().await.disconnected.child_token();
                    let resolved = tokio::select! {
                        biased;
                        () = self.skill_load_cancelled(agent_id, &snapshot) => {
                            cancel.cancel();
                            return Ok(true);
                        },
                        result = self.shared.harness.resolve_mcp_skill(server, uri, &cancel) => result,
                    };
                    let resolved = resolved.and_then(|held| {
                        held.validate()
                            .map_err(|error| reject(ErrorCode::ArtifactUnavailable, &error))?;
                        if held.origin.server != *server || held.entry.uri != *uri {
                            return Err(reject(
                                ErrorCode::ArtifactUnavailable,
                                "explicit skill metadata resolved another source or URI",
                            ));
                        }
                        Ok(held)
                    });
                    match resolved {
                        Ok(held) => catalog.mcp.skills.push(held),
                        Err(error) if skill_resource_failure(&error) => {
                            catalog.mcp.excluded.push(
                                bitrouter_ai::harness::skills::McpSkillProblem {
                                    server: server.clone(),
                                    uri: Some(uri.clone()),
                                    reason: crate::core::context_router::planner::truncate(
                                        &error.message,
                                        2048,
                                    )
                                    .to_owned(),
                                },
                            );
                        }
                        Err(error) => return Err(error),
                    }
                }
            }
            let candidates = catalog
                .sources()
                .into_iter()
                .map(|source| SkillCandidate {
                    source,
                    reason: "awaiting_selection".into(),
                    selected: false,
                    content: None,
                    files: BTreeMap::new(),
                    failure: None,
                    consents: Vec::new(),
                })
                .collect();
            let base_prompt = build_prompt(&snapshot, agent_id)?;
            let base_bytes = serde_json::to_vec(&base_prompt).map_err(json_error)?.len() as u64;
            let source = SourceRevision::capture(
                agent.context_revision,
                snapshot.signals.revision,
                &snapshot.manifest,
                &base_prompt,
            )?;
            let selection = SkillSelection {
                source,
                catalog,
                constraints,
                max_selected: 4,
                instruction_limit_bytes: (64 * 1024).min(
                    turn.input
                        .context_limit_bytes
                        .unwrap_or(u64::MAX)
                        .saturating_sub(base_bytes),
                ),
                file_limit_bytes: snapshot
                    .run
                    .as_ref()
                    .filter(|_| {
                        snapshot
                            .manifest
                            .required_features
                            .iter()
                            .any(|feature| feature == crate::storage::skills::FILES_FEATURE)
                    })
                    .map(|run| run.limits.input_bytes),
                candidates,
                selected: false,
                failure: None,
                validated_step_count: None,
            };
            self.transition_for(Some(agent_id), "skills.catalogued", |state, _| {
                budget::ensure(state)?;
                steering::ensure_ready(state, agent_id)?;
                let current = state
                    .agents
                    .get(agent_id)
                    .and_then(|agent| agent.turn.as_ref())
                    .ok_or_else(|| reject(ErrorCode::Busy, "skill Turn changed"))?;
                if current.agent_turn_id != turn.agent_turn_id
                    || state.skill_selection(&current.agent_turn_id).is_some()
                    || current.status != AgentStatus::Runnable
                    || state.signals.revision != snapshot.signals.revision
                    || state.manifest != snapshot.manifest
                    || state
                        .agents
                        .get(agent_id)
                        .map(|agent| agent.context_revision)
                        != Some(agent.context_revision)
                {
                    return Err(reject(
                        ErrorCode::StaleRevision,
                        "skill catalog boundary changed",
                    ));
                }
                *selection_slot(state, agent_id)? = Some(selection);
                Ok(json!({"agent_turn_id":turn.agent_turn_id}))
            })
            .await?;
            return Ok(true);
        };
        if !selection.selected
            && selection.failure.is_none()
            && validate_selection_source(&snapshot, agent_id).is_err()
        {
            steering::ensure_ready(&snapshot, agent_id)?;
            if !turn.steps.is_empty() {
                return Err(reject(
                    ErrorCode::CheckpointConflict,
                    "unselected skills cannot reassess after model preparation",
                ));
            }
            let prompt = build_prompt(&snapshot, agent_id)?;
            let source = SourceRevision::capture(
                agent.context_revision,
                snapshot.signals.revision,
                &snapshot.manifest,
                &prompt,
            )?;
            let instruction_limit_bytes = (64 * 1024).min(
                turn.input
                    .context_limit_bytes
                    .unwrap_or(u64::MAX)
                    .saturating_sub(serde_json::to_vec(&prompt).map_err(json_error)?.len() as u64),
            );
            self.transition_for(Some(agent_id), "skills.assessment.rebased", |state, _| {
                budget::ensure(state)?;
                steering::ensure_ready(state, agent_id)?;
                let current = agent_turn(state, agent_id)?;
                if current.agent_turn_id != turn.agent_turn_id
                    || current.status != AgentStatus::Runnable
                    || !current.steps.is_empty()
                    || selection_source(state, agent_id)? != source
                {
                    return Err(reject(
                        ErrorCode::StaleRevision,
                        "skill reassessment boundary changed",
                    ));
                }
                let current = selection_slot(state, agent_id)?.as_mut().ok_or_else(|| {
                    reject(ErrorCode::CheckpointConflict, "skill catalog disappeared")
                })?;
                if current.selected || current.source != selection.source {
                    return Err(reject(
                        ErrorCode::StaleRevision,
                        "skill selection already advanced",
                    ));
                }
                // The catalog and explicit constraints stay intact while the
                // unfinished selection is rebound to the current context.
                current.source = source.clone();
                current.instruction_limit_bytes = instruction_limit_bytes;
                Ok(json!({"agent_turn_id":turn.agent_turn_id,"source":source}))
            })
            .await?;
            return Ok(true);
        }
        if !selection.selected
            || selection.failure.is_some()
            || awaits_skill_consent(&snapshot, agent_id)
        {
            if let Some(error) = &selection.failure {
                self.fail(
                    agent_id,
                    &format!("skill execution admission failed: {}", error.message),
                )
                .await?;
                return Ok(true);
            }
            let result = if selection.selected {
                self.prepare_skill_consents(agent_id).await
            } else {
                self.select_skills(agent_id, &snapshot).await
            };
            if result.as_ref().is_err_and(|error| {
                error.code == ErrorCode::StaleRevision
                    && error.commit_status == CommitStatus::NotCommitted
            }) {
                // Let the existing scheduler apply steering/cancellation or
                // rebase an unfinished selection at its next safe boundary.
                return Ok(true);
            }
            if let Err(error) = &result
                && error.commit_status == CommitStatus::NotCommitted
                && matches!(
                    error.code,
                    ErrorCode::ArtifactUnavailable
                        | ErrorCode::UnsupportedCapability
                        | ErrorCode::NoFeasibleRoute
                        | ErrorCode::LimitExceeded
                )
            {
                self.transition_for(Some(agent_id), "skills.admission.rejected", |state, _| {
                    let current = agent_turn(state, agent_id)?;
                    if current.agent_turn_id != turn.agent_turn_id
                        || current.status != AgentStatus::Runnable
                    {
                        return Err(reject(
                            ErrorCode::StaleRevision,
                            "skill rejection Turn changed",
                        ));
                    }
                    let selection = selection_slot(state, agent_id)?.as_mut().ok_or_else(|| {
                        reject(ErrorCode::CheckpointConflict, "skill selection disappeared")
                    })?;
                    selection.failure = Some(error.clone());
                    Ok(json!({"agent_turn_id":turn.agent_turn_id,"error":error}))
                })
                .await?;
                self.fail(
                    agent_id,
                    &format!("skill execution admission failed: {}", error.message),
                )
                .await?;
                // Let the original scheduler settle this failed Turn and its
                // parent/queue. Acceptance identity remains unchanged.
                return Ok(true);
            }
            return result;
        }
        if !selection
            .candidates
            .iter()
            .any(|candidate| candidate.selected)
        {
            return Ok(false);
        }
        if selection.validated_step_count == Some(turn.steps.len()) {
            return Ok(false);
        }
        self.authorize_dispatch().await?;
        let mut loaded = Vec::new();
        for candidate in selection
            .candidates
            .iter()
            .filter(|candidate| candidate.selected)
        {
            if !candidate_for_model(&snapshot, selection, candidate)? {
                continue;
            }
            let cancel = self.shared.live.lock().await.disconnected.child_token();
            let cached_bytes = match &candidate.source {
                SkillSource::Mcp(_) => candidate.content.as_deref().map(str::as_bytes),
                SkillSource::Local(_) => None,
            };
            let bytes = tokio::select! {
                biased;
                () = self.skill_load_cancelled(agent_id, &snapshot) => {
                    cancel.cancel();
                    return Ok(true);
                },
                bytes = self.shared.harness.skill_instructions(&candidate.source, selection.instruction_limit_bytes, cached_bytes, &cancel) => bytes,
            };
            let verified = bytes.and_then(|bytes| {
                candidate.source.verify(&bytes)?;
                let text = String::from_utf8(bytes).map_err(|_| {
                    reject(
                        ErrorCode::ArtifactUnavailable,
                        "skill instructions require UTF-8",
                    )
                })?;
                if candidate
                    .content
                    .as_ref()
                    .is_some_and(|prior| prior != &text)
                {
                    return Err(reject(
                        ErrorCode::ArtifactUnavailable,
                        "original skill instructions changed",
                    ));
                }
                Ok(text)
            });
            let text = match verified {
                Ok(text) => text,
                Err(error) => {
                    return self
                        .record_skill_load_failure(
                            agent_id,
                            &snapshot,
                            Some(&candidate.source),
                            error,
                        )
                        .await;
                }
            };
            for (reference, content) in &candidate.files {
                let file = candidate.source.file(reference)?;
                let cached =
                    matches!(candidate.source, SkillSource::Mcp(_)).then_some(content.as_bytes());
                let verified = tokio::select! {
                    biased;
                    () = self.skill_load_cancelled(agent_id, &snapshot) => { cancel.cancel(); return Ok(true); },
                    bytes = self.shared.harness.skill_file(&candidate.source, reference,
                        selection.file_limit_bytes.unwrap_or(0).min(snapshot.manifest.max_tool_output_bytes), cached, &cancel) => bytes,
                }.and_then(|bytes| {
                    candidate.source.verify_file(&file, &bytes)?;
                    if bytes != content.as_bytes() { return Err(reject(ErrorCode::ArtifactUnavailable, "original supporting bytes changed")); }
                    Ok(())
                });
                if let Err(error) = verified {
                    return self
                        .record_skill_load_failure(
                            agent_id,
                            &snapshot,
                            Some(&candidate.source),
                            error,
                        )
                        .await;
                }
            }
            loaded.push((candidate.source.identity()?, text));
        }
        let committed = self.transition_for(Some(agent_id), "skills.instructions.verified", |state, _| {
            budget::ensure(state)?;
            steering::ensure_ready(state, agent_id)?;
            if !skill_load_current(state, &snapshot, agent_id) {
                return Err(reject(ErrorCode::StaleRevision, "skill loading source changed"));
            }
            let current = agent_turn(state, agent_id)?;
            if current.agent_turn_id != turn.agent_turn_id || current.status != AgentStatus::Runnable || current.steps.len() != turn.steps.len() {
                return Err(reject(ErrorCode::StaleRevision, "skill loading Turn changed"));
            }
            let current = selection_slot(state, agent_id)?.as_mut().ok_or_else(|| reject(ErrorCode::CheckpointConflict, "skill binding disappeared"))?;
            for (identity, text) in &loaded {
                let candidate = current.candidates.iter_mut().find(|candidate| candidate.source.identity().is_ok_and(|id| id == *identity))
                    .ok_or_else(|| reject(ErrorCode::CheckpointConflict, "selected skill disappeared"))?;
                candidate.content = Some(text.clone());
            }
            current.validated_step_count = Some(turn.steps.len());
            // Account for the actual rendered instruction envelope, including
            // exact provenance/receipt; never silently truncate a bundle root.
            let current_turn = state.agents.get(agent_id).and_then(|agent| agent.turn.as_ref()).ok_or_else(|| reject(ErrorCode::Busy, "skill Turn changed"))?;
            instructions(state, current_turn, String::new())?;
            Ok(json!({"agent_turn_id":turn.agent_turn_id,"loaded":loaded.len(),"step_count":turn.steps.len()}))
        }).await;
        match committed {
            Ok(_) => Ok(true),
            Err(error) if error.code == ErrorCode::StaleRevision => Ok(true),
            Err(error) => {
                self.record_skill_load_failure(agent_id, &snapshot, None, error)
                    .await
            }
        }
    }

    pub(super) async fn record_skill_load_failure(
        &self,
        agent_id: &str,
        expected: &SessionSnapshot,
        source: Option<&SkillSource>,
        mut error: CoreError,
    ) -> Result<bool, CoreError> {
        if !skill_resource_failure(&error) {
            return Err(error);
        }
        // Remote diagnostics must not consume the capacity reserved for a
        // durable failure fact and normal task settlement.
        error.message =
            crate::core::context_router::planner::truncate(&error.message, 2048).to_owned();
        let committed = self.transition_for(Some(agent_id), "skills.instructions.unavailable", |state, _| {
            if !skill_load_current(state, expected, agent_id) {
                return Err(reject(ErrorCode::StaleRevision, "failed skill read belongs to a stale loading source"));
            }
            let turn = agent_turn(state, agent_id)?;
            let turn_id = turn.agent_turn_id.clone();
            let step_count = turn.steps.len();
            let context_revision = state.agents.get(agent_id).map(|agent| agent.context_revision);
            let selection = selection_slot(state, agent_id)?.as_mut().ok_or_else(|| reject(ErrorCode::CheckpointConflict, "skill binding disappeared"))?;
            let mut fatal = true;
            if let Some(source) = source {
                let required = selection.constraints.include.contains(&source.selector());
                let candidate = selection.candidates.iter_mut().find(|candidate| candidate.source == *source)
                    .ok_or_else(|| reject(ErrorCode::CheckpointConflict, "failed skill binding disappeared"))?;
                fatal = required || candidate.content.is_some() || step_count != 0;
                if !fatal { candidate.failure = Some(error.clone()); }
            }
            if fatal { selection.failure = Some(error.clone()); }
            selection.validated_step_count = None;
            Ok(json!({"agent_turn_id":turn_id,"skill_id":source.map(SkillSource::identity).transpose()?,"fatal":fatal,"error":error,
                "step_count":step_count,"context_revision":context_revision,"signal_revision":state.signals.revision,
                "permission_revision":state.manifest.permission_revision,"tool_manifest_digest":state.manifest.tool_manifest_digest}))
        }).await;
        match committed {
            Ok(_) => Ok(true),
            Err(error) if error.code == ErrorCode::StaleRevision => Ok(true),
            Err(error) => Err(error),
        }
    }

    async fn select_skills(
        &self,
        agent_id: &str,
        snapshot: &SessionSnapshot,
    ) -> Result<bool, CoreError> {
        let turn = snapshot
            .agents
            .get(agent_id)
            .and_then(|agent| agent.turn.as_ref())
            .ok_or_else(|| reject(ErrorCode::Busy, "skill Turn is absent"))?;
        let selection = snapshot
            .skill_selection(&turn.agent_turn_id)
            .ok_or_else(|| reject(ErrorCode::CheckpointConflict, "skill catalog is absent"))?;
        let mut next = selection.clone();
        crate::storage::skills::selection::select(&mut next.candidates, &next.constraints)?;
        let (count, bytes) = crate::storage::skills::selection::bound(
            &next.candidates,
            next.max_selected,
            next.instruction_limit_bytes,
            instruction_size,
        )?;
        next.selected = true;
        self.transition_for(Some(agent_id), "skills.selected", |state, _| {
            validate_selection_source(state, agent_id)?;
            budget::ensure(state)?;
            *selection_slot(state, agent_id)? = Some(next);
            Ok(json!({"agent_turn_id":turn.agent_turn_id,"selected_count":count,"instruction_bytes":bytes}))
        }).await?;
        Ok(true)
    }

    pub(super) async fn skill_load_cancelled(&self, agent_id: &str, expected: &SessionSnapshot) {
        loop {
            let changed = self.shared.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            let live = self.shared.live.lock().await;
            if live.disconnected.is_cancelled()
                || !skill_load_current(&live.state, expected, agent_id)
            {
                return;
            }
            drop(live);
            let disconnected = self.shared.live.lock().await.disconnected.clone();
            tokio::select! { () = disconnected.cancelled() => return, () = &mut changed => {} }
        }
    }
}

pub(super) fn skill_load_current(
    state: &SessionSnapshot,
    expected: &SessionSnapshot,
    agent_id: &str,
) -> bool {
    let Some(original) = expected.agents.get(agent_id) else {
        return false;
    };
    let Some(agent) = state.agents.get(agent_id) else {
        return false;
    };
    let Some(turn) = agent.turn.as_ref() else {
        return false;
    };
    !steering::has_pending(state, agent_id)
        && state.manifest == expected.manifest
        && state.signals.revision == expected.signals.revision
        && agent.context_revision == original.context_revision
        && original.turn.as_ref().is_some_and(|previous| {
            turn.agent_turn_id == previous.agent_turn_id
                && turn.steps.len() == previous.steps.len()
                && turn.status == previous.status
        })
        && matches!(
            turn.status,
            AgentStatus::Runnable | AgentStatus::WaitingTool | AgentStatus::WaitingMessage
        )
}

pub(super) fn validate_selection_source(
    state: &SessionSnapshot,
    agent_id: &str,
) -> Result<(), CoreError> {
    steering::ensure_ready(state, agent_id)?;
    let agent = state
        .agents
        .get(agent_id)
        .ok_or_else(|| reject(ErrorCode::Busy, "skill agent is absent"))?;
    let turn = agent
        .turn
        .as_ref()
        .ok_or_else(|| reject(ErrorCode::Busy, "skill Turn is absent"))?;
    let selection = state
        .skill_selection(&turn.agent_turn_id)
        .ok_or_else(|| reject(ErrorCode::Busy, "skill selection is absent"))?;
    if turn.status != AgentStatus::Runnable
        || selection.selected
        || !turn.steps.is_empty()
        || selection_source(state, agent_id)? != selection.source
    {
        return Err(reject(
            ErrorCode::StaleRevision,
            "skill selection source changed",
        ));
    }
    Ok(())
}

fn selection_source(state: &SessionSnapshot, agent_id: &str) -> Result<SourceRevision, CoreError> {
    let agent = state
        .agents
        .get(agent_id)
        .ok_or_else(|| reject(ErrorCode::Busy, "skill agent is absent"))?;
    SourceRevision::capture(
        agent.context_revision,
        state.signals.revision,
        &state.manifest,
        &build_prompt(state, agent_id)?,
    )
}
