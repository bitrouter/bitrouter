//! Original Core tool intents load only files in the selected immutable source.

use super::*;
use crate::core::skills::{SkillSource, files};

impl CoreSession {
    pub(super) async fn dispatch_skill_file(&self, agent_id: &str) -> Result<bool, CoreError> {
        let snapshot = self.snapshot().await;
        let Some(turn) = snapshot
            .agents
            .get(agent_id)
            .and_then(|agent| agent.turn.as_ref())
        else {
            return Ok(false);
        };
        let now = now_ms()?;
        let Some(call) = turn
            .core_calls
            .iter()
            .find(|call| {
                call.result.is_none()
                    && call.wait.as_ref().is_none_or(|wait| {
                        collaboration::wait_ready(&snapshot, agent_id, wait, now)
                    })
            })
            .cloned()
        else {
            return Ok(false);
        };
        let Action::SkillRead {
            skill_id,
            reference,
        } = &call.action
        else {
            return Ok(false);
        };
        let admitted = (|| {
            files::enabled(&snapshot)?;
            if serde_json::to_vec(&call.action).map_err(json_error)?.len() as u64
                > snapshot
                    .run
                    .as_ref()
                    .map_or(0, |run| run.limits.input_bytes)
            {
                return Err(reject(
                    ErrorCode::LimitExceeded,
                    "skill file request exceeds input budget",
                ));
            }
            let selection = snapshot
                .skill_selection(&turn.agent_turn_id)
                .ok_or_else(|| {
                    reject(
                        ErrorCode::ArtifactUnavailable,
                        "no skill selection for this Turn",
                    )
                })?;
            let candidate = selection
                .candidates
                .iter()
                .find(|candidate| candidate.source.identity().is_ok_and(|id| id == *skill_id))
                .ok_or_else(|| {
                    reject(
                        ErrorCode::UnauthorizedScope,
                        "skill is not bound to this Turn",
                    )
                })?;
            if selection.failure.is_some()
                || candidate.content.is_none()
                || !skills::candidate_for_model(&snapshot, selection, candidate)?
            {
                return Err(reject(
                    ErrorCode::UnauthorizedScope,
                    "skill has no current approved activation",
                ));
            }
            let file = candidate.source.file(reference)?;
            let file_limit = selection.file_limit_bytes.ok_or_else(|| {
                reject(
                    ErrorCode::UnsupportedCapability,
                    "this original Turn has no skill file budget",
                )
            })?;
            let limit = if candidate.source.root_file(&file.reference) {
                selection.instruction_limit_bytes
            } else {
                file_limit.min(snapshot.manifest.max_tool_output_bytes)
            };
            if file.bytes > limit {
                return Err(reject(
                    ErrorCode::LimitExceeded,
                    "skill file exceeds the original host byte budget",
                ));
            }
            if !candidate.source.root_file(&file.reference)
                && !candidate.files.contains_key(&file.reference)
                && files::cached_bytes(selection)?
                    .saturating_add(files::prefix(candidate, &file)?.len() as u64)
                    .saturating_add(file.bytes)
                    > file_limit
            {
                return Err(reject(
                    ErrorCode::LimitExceeded,
                    "supporting data exceeds the frozen context byte budget",
                ));
            }
            Ok((candidate.clone(), file, limit))
        })();
        let (candidate, file, limit) = match admitted {
            Ok(value) => value,
            Err(error) => {
                return self
                    .settle_skill_file(agent_id, &snapshot, &call, None, Err(error))
                    .await;
            }
        };
        self.authorize_dispatch().await?;
        let cache = if matches!(candidate.source, SkillSource::Mcp(_)) {
            if candidate.source.root_file(&file.reference) {
                candidate.content.as_deref().map(str::as_bytes)
            } else {
                candidate
                    .files
                    .get(&file.reference)
                    .map(|content| content.as_bytes())
            }
        } else {
            None
        };
        let cancel = self.shared.live.lock().await.disconnected.child_token();
        let bytes = tokio::select! {
            biased;
            () = self.skill_load_cancelled(agent_id, &snapshot) => { cancel.cancel(); return Ok(true); },
            result = self.shared.harness.skill_file(&candidate.source, &file.reference, limit, cache, &cancel) => result,
        };
        let content = bytes.and_then(|bytes| {
            candidate.source.verify_file(&file, &bytes)?;
            let content = String::from_utf8(bytes).map_err(|_| {
                reject(
                    ErrorCode::ArtifactUnavailable,
                    "supporting file requires UTF-8 model data",
                )
            })?;
            if content.contains('\0') {
                return Err(reject(
                    ErrorCode::ArtifactUnavailable,
                    "binary skill files are unsupported in model context",
                ));
            }
            Ok(content)
        });
        let result = content.map(|content| (file, content));
        if let Ok((file, content)) = &result {
            let mut projected = snapshot.clone();
            retain_file(&mut projected, agent_id, &candidate.source, file, content)?;
            let projected_turn = agent_turn(&mut projected, agent_id)?;
            let projected_call = projected_turn
                .core_calls
                .iter_mut()
                .find(|pending| pending.invocation_id == call.invocation_id)
                .ok_or_else(|| reject(ErrorCode::CheckpointConflict, "file intent disappeared"))?;
            projected_call.result = Some(receipt(&candidate.source, file)?);
            let complete = projected_turn
                .core_calls
                .iter()
                .all(|call| call.consumed || call.result.is_some())
                && projected_turn
                    .invocations
                    .iter()
                    .all(|call| call.consumed || call.result.is_some());
            // Use the same paired history as execution. A mixed tool batch is
            // checked at final model preparation after every result is known.
            let exceeds_context =
                if let Some(limit) = turn.input.context_limit_bytes.filter(|_| complete) {
                    let native_values = projected
                        .manifest
                        .required_features
                        .iter()
                        .any(|feature| feature == crate::core::context_router::NATIVE_TOOLS);
                    pairing::consume(agent_mut(&mut projected, agent_id)?, native_values)?;
                    serde_json::to_vec(&build_prompt(&projected, agent_id)?)
                        .map_err(json_error)?
                        .len() as u64
                        > limit
                } else {
                    false
                };
            if exceeds_context {
                return self
                    .settle_skill_file(
                        agent_id,
                        &snapshot,
                        &call,
                        None,
                        Err(reject(
                            ErrorCode::LimitExceeded,
                            "supporting data exceeds the original Turn context budget",
                        )),
                    )
                    .await;
            }
        }
        self.settle_skill_file(agent_id, &snapshot, &call, Some(&candidate.source), result)
            .await
    }

    async fn settle_skill_file(
        &self,
        agent_id: &str,
        expected: &SessionSnapshot,
        call: &Call,
        source: Option<&SkillSource>,
        mut result: Result<(files::File, String), CoreError>,
    ) -> Result<bool, CoreError> {
        if result.as_ref().is_err_and(|error| {
            error.commit_status != CommitStatus::NotCommitted
                || !matches!(
                    error.code,
                    ErrorCode::ArtifactUnavailable
                        | ErrorCode::UnsupportedCapability
                        | ErrorCode::NoFeasibleRoute
                        | ErrorCode::LimitExceeded
                        | ErrorCode::UnauthorizedScope
                )
        }) {
            return Err(result
                .err()
                .ok_or_else(|| reject(ErrorCode::CheckpointConflict, "missing file failure"))?);
        }
        if let Err(error) = &mut result {
            error.message =
                crate::core::context_router::planner::truncate(&error.message, 2048).to_owned();
        }
        let _input = self.shared.inputs.lock().await;
        let settled = self.transition_for(Some(agent_id), "skills.file.settled", |state, _| {
            if !skills::skill_load_current(state, expected, agent_id) {
                return Err(reject(ErrorCode::StaleRevision, "skill file belongs to a stale input/permission boundary"));
            }
            budget::ensure(state)?;
            let turn_id = agent_turn(state, agent_id)?.agent_turn_id.clone();
            let value = match &result {
                Err(error) => {
                    if source.is_some() {
                        let selection = skills::selection_slot(state, agent_id)?.as_mut().ok_or_else(|| reject(ErrorCode::CheckpointConflict, "file parent disappeared"))?;
                        selection.failure = Some(error.clone());
                        selection.validated_step_count = None;
                    }
                    json!({"ok":false,"error":error})
                },
                Ok((file, content)) => {
                    let source = source.ok_or_else(|| reject(ErrorCode::CheckpointConflict, "file has no original source"))?;
                    retain_file(state, agent_id, source, file, content)?;
                    receipt(source, file)?
                }
            };
            let turn = agent_turn(state, agent_id)?;
            let retained = turn.core_calls.iter_mut().find(|retained| retained.invocation_id == call.invocation_id && retained.result.is_none()).ok_or_else(|| reject(ErrorCode::OperationConflict, "file read intent already settled"))?;
            retained.result = Some(value.clone());
            turn.status = if turn.core_calls.iter().any(|pending| pending.wait.is_some() && pending.result.is_none()) {
                AgentStatus::WaitingMessage
            } else {
                AgentStatus::WaitingTool
            };
            if source.is_some() && result.is_err() {
                for target in collaboration::subtree(state, agent_id) {
                    if let Some(agent) = state.agents.get_mut(&target) {
                        agent.queue.clear();
                        if let Some(turn) = &mut agent.turn && !turn.status.terminal() {
                            turn.status = AgentStatus::Cancelling;
                            turn.cancellation_requested = true;
                        }
                    }
                }
            }
            Ok(json!({"agent_turn_id":turn_id,"invocation_id":call.invocation_id,"result":value}))
        }).await;
        match settled {
            Ok(()) => Ok(true),
            Err(error)
                if error.code == ErrorCode::StaleRevision
                    && error.commit_status == CommitStatus::NotCommitted =>
            {
                Ok(true)
            }
            Err(error) => Err(error),
        }
    }
}

fn receipt(source: &SkillSource, file: &files::File) -> Result<Value, CoreError> {
    Ok(
        json!({"ok":true,"file":file,"parent_source_id":source.identity()?,"delivery":"source_tagged_user_data","executed":false,"nested_activation":false}),
    )
}

fn retain_file(
    state: &mut SessionSnapshot,
    agent_id: &str,
    source: &SkillSource,
    file: &files::File,
    content: &str,
) -> Result<(), CoreError> {
    let selection = skills::selection_slot(state, agent_id)?
        .as_mut()
        .ok_or_else(|| reject(ErrorCode::CheckpointConflict, "skill selection disappeared"))?;
    let candidate = selection
        .candidates
        .iter_mut()
        .find(|candidate| candidate.source == *source)
        .ok_or_else(|| reject(ErrorCode::CheckpointConflict, "file parent changed"))?;
    source.verify_file(file, content.as_bytes())?;
    if !source.root_file(&file.reference) {
        if candidate
            .files
            .get(&file.reference)
            .is_some_and(|old| old != content)
        {
            return Err(reject(
                ErrorCode::CheckpointConflict,
                "retained skill bytes changed",
            ));
        }
        candidate
            .files
            .insert(file.reference.clone(), content.to_owned());
    }
    selection.validated_step_count = None;
    if selection
        .file_limit_bytes
        .is_none_or(|limit| files::cached_bytes(selection).map_or(true, |bytes| bytes > limit))
    {
        return Err(reject(
            ErrorCode::LimitExceeded,
            "supporting data exceeds the frozen context byte budget",
        ));
    }
    Ok(())
}
