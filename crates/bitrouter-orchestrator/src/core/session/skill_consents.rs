//! Explicit MCP skill consent uses the original checkpoint/operation ledger.
//! Metadata delivery never fetches instructions or authorizes a workspace tool.

use super::*;
use crate::core::skills::{SkillConsent, SkillConsentAnswer, SkillConsentRequest, SkillSource};

impl SessionSnapshot {
    /// A cold projection of original consent facts, including answered and
    /// superseded requests so an operator can retry an unknown answer ACK.
    /// No body read, source discovery or new execution is authorized here.
    pub fn skill_consent_requests(&self, run_id: &str) -> Vec<SkillConsentRequest> {
        let mut requests = Vec::new();
        for work in self
            .context_store
            .work
            .values()
            .filter(|work| work.run_id == run_id)
        {
            if let Some(selection) = &work.skills {
                for candidate in &selection.candidates {
                    if let SkillSource::Mcp(source) = &candidate.source {
                        requests.extend(candidate.consents.iter().map(|consent| {
                            SkillConsentRequest {
                                session_id: self.session_id.clone(),
                                run_id: work.run_id.clone(),
                                agent_id: work.agent_id.clone(),
                                agent_turn_id: work.task_id.clone(),
                                consent: consent.clone(),
                                source: source.clone(),
                            }
                        }));
                    }
                }
            }
        }
        requests
    }
}

impl CoreSession {
    pub async fn skill_consent_pending(&self, request: &SkillConsentRequest) -> bool {
        let live = self.shared.live.lock().await;
        let state = &live.state;
        if live.disconnected.is_cancelled()
            || release::released(&live)
            || live.gate.grant().execution_epoch != request.consent.execution_epoch
            || state.session_id != request.session_id
        {
            return false;
        }
        let Some(turn) = state
            .agents
            .get(&request.agent_id)
            .and_then(|agent| agent.turn.as_ref())
            .filter(|turn| {
                turn.agent_turn_id == request.agent_turn_id
                    && turn.run_id == request.run_id
                    && turn.status == AgentStatus::WaitingMaterial
            })
        else {
            return false;
        };
        state.skill_selection(&turn.agent_turn_id).is_some_and(|selection| selection.candidates.iter().any(|candidate| {
            matches!(&candidate.source, SkillSource::Mcp(source) if source == &request.source)
                && candidate.consents.last().is_some_and(|consent| consent == &request.consent && consent.approved.is_none() && skills::consent_current(state, candidate, consent).unwrap_or(false))
        }))
    }

    /// Observe the existing session notification stream; this creates no
    /// scheduler, timer, execution, or independent source of approval truth.
    pub async fn skill_consent_expired(&self, request: &SkillConsentRequest) {
        loop {
            let changed = self.shared.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if !self.skill_consent_pending(request).await {
                return;
            }
            let disconnected = self.shared.live.lock().await.disconnected.clone();
            tokio::select! { () = disconnected.cancelled() => return, () = &mut changed => {} }
        }
    }

    pub(super) async fn prepare_skill_consents(&self, agent_id: &str) -> Result<bool, CoreError> {
        let snapshot = self.snapshot().await;
        let turn = snapshot
            .agents
            .get(agent_id)
            .and_then(|agent| agent.turn.as_ref())
            .ok_or_else(|| reject(ErrorCode::Busy, "skill consent Turn is absent"))?;
        if !matches!(
            turn.status,
            AgentStatus::Runnable | AgentStatus::WaitingMaterial
        ) {
            return Ok(false);
        }
        let selection = snapshot
            .skill_selection(&turn.agent_turn_id)
            .ok_or_else(|| {
                reject(
                    ErrorCode::CheckpointConflict,
                    "skill consent selection is absent",
                )
            })?;
        let epoch = self.head().await.execution_epoch;
        for candidate in selection
            .candidates
            .iter()
            .filter(|candidate| candidate.selected && candidate.failure.is_none())
        {
            let SkillSource::Mcp(source) = &candidate.source else {
                continue;
            };
            let skill_id = candidate.source.identity()?;
            if let Some(consent) = candidate.consents.last() {
                if consent.approved == Some(false)
                    && !consent.revoked
                    && candidate.content.is_none()
                {
                    if selection
                        .constraints
                        .include
                        .contains(&candidate.source.selector())
                    {
                        return Err(reject(
                            ErrorCode::ArtifactUnavailable,
                            "required MCP skill consent was denied",
                        ));
                    }
                    continue;
                }
                if skills::consent_current(&snapshot, candidate, consent)?
                    && consent.approved == Some(true)
                {
                    continue;
                }
                if !turn.steps.is_empty() {
                    return Err(reject(
                        ErrorCode::ArtifactUnavailable,
                        "MCP skill consent no longer authorizes this Turn",
                    ));
                }
                if skills::consent_current(&snapshot, candidate, consent)?
                    && consent.approved.is_none()
                    && consent.execution_epoch == epoch
                {
                    return self
                        .deliver_skill_consent(agent_id, consent.request_id.as_str())
                        .await;
                }
            }
            let consent = SkillConsent {
                request_id: id("skill_consent"),
                execution_epoch: epoch,
                permission_revision: snapshot.manifest.permission_revision,
                tool_manifest_digest: snapshot.manifest.tool_manifest_digest.clone(),
                skill_id: skill_id.clone(),
                entry_digest: source.entry_digest.clone(),
                approved: None,
                answer_operation_id: None,
                revoked: false,
            };
            self.transition_for(Some(agent_id), "skill.consent.requested", |state, head| {
                budget::ensure(state)?;
                steering::ensure_ready(state, agent_id)?;
                let current = agent_turn(state, agent_id)?;
                if current.agent_turn_id != turn.agent_turn_id || !matches!(current.status, AgentStatus::Runnable | AgentStatus::WaitingMaterial)
                    || head.execution_epoch != epoch || state.manifest != snapshot.manifest {
                    return Err(reject(ErrorCode::StaleRevision, "skill consent admission changed"));
                }
                let current = skills::selection_slot(state, agent_id)?.as_mut()
                    .ok_or_else(|| reject(ErrorCode::CheckpointConflict, "skill selection disappeared"))?;
                let candidate = current.candidates.iter_mut().find(|candidate| candidate.source.identity().is_ok_and(|id| id == skill_id))
                    .ok_or_else(|| reject(ErrorCode::CheckpointConflict, "skill binding disappeared"))?;
                if let Some(previous) = candidate.consents.last_mut() { previous.revoked = true; }
                candidate.consents.push(consent.clone());
                current.validated_step_count = None;
                agent_turn(state, agent_id)?.status = AgentStatus::WaitingMaterial;
                Ok(json!({"request_id":consent.request_id,"skill_id":skill_id,"entry_digest":source.entry_digest}))
            }).await?;
            self.deliver_skill_consent(agent_id, &consent.request_id)
                .await?;
            return Ok(true);
        }
        Ok(false)
    }

    async fn deliver_skill_consent(
        &self,
        agent_id: &str,
        request_id: &str,
    ) -> Result<bool, CoreError> {
        let (request, disconnected, generation) = {
            let _input = self.shared.inputs.lock().await;
            let _admission = self.shared.commits.lock().await;
            self.authorize_dispatch().await?;
            let mut live = self.shared.live.lock().await;
            budget::ensure_live(&live)?;
            if !live.gate.can_dispatch() {
                return Err(reject(
                    ErrorCode::CheckpointUnavailable,
                    "skill consent awaits committed authority",
                ));
            }
            if steering::has_pending(&live.state, agent_id) {
                return Ok(false);
            }
            let Some(turn) = live
                .state
                .agents
                .get(agent_id)
                .and_then(|agent| agent.turn.as_ref())
                .filter(|turn| turn.status == AgentStatus::WaitingMaterial)
            else {
                // A status reader can answer the acknowledged request before
                // this notification acquires the input lock. That answer is
                // authoritative; a stale notification must not abort its run.
                return Ok(true);
            };
            let Some(selection) = live.state.skill_selection(&turn.agent_turn_id) else {
                return Ok(true);
            };
            let Some(candidate) = selection.candidates.iter().find(|candidate| {
                candidate
                    .consents
                    .last()
                    .is_some_and(|consent| consent.request_id == request_id)
            }) else {
                return Ok(true);
            };
            let consent = candidate
                .consents
                .last()
                .ok_or_else(|| reject(ErrorCode::CheckpointConflict, "skill consent is absent"))?;
            let SkillSource::Mcp(source) = &candidate.source else {
                return Err(reject(
                    ErrorCode::CheckpointConflict,
                    "local skills do not use MCP consent",
                ));
            };
            if consent.approved.is_some()
                || !skills::consent_current(&live.state, candidate, consent)?
            {
                return Ok(false);
            }
            let request = SkillConsentRequest {
                session_id: live.state.session_id.clone(),
                run_id: turn.run_id.clone(),
                agent_id: agent_id.into(),
                agent_turn_id: turn.agent_turn_id.clone(),
                consent: consent.clone(),
                source: source.clone(),
            };
            if !live.sent_skill_consents.insert(request_id.into()) {
                return Ok(false);
            }
            (
                request,
                live.disconnected.clone(),
                live.connection_generation,
            )
        };
        let session = self.clone();
        let sending = tokio::spawn(async move {
            let sent = tokio::select! {
                biased;
                _ = disconnected.cancelled() => Err(reject(ErrorCode::CheckpointUnavailable, "owner disconnected before skill consent delivery")),
                sent = session.shared.harness.send(ServerMessage::SkillConsentRequest(Box::new(request))) => sent,
            };
            if sent.is_err() {
                session.disconnect_generation(generation).await;
            }
            sent
        });
        match sending.await {
            Ok(sent) => sent?,
            Err(error) => {
                self.disconnect_generation(generation).await;
                return Err(reject(ErrorCode::RecoveryRequired, &error.to_string()));
            }
        }
        Ok(false)
    }

    /// An answer covers one exact entry and the original accepted Turn. It
    /// cannot be reused for another source, caller, epoch or tool permission.
    pub async fn answer_skill_consent(
        &self,
        operation_id: &str,
        answer: SkillConsentAnswer,
    ) -> Result<OperationReceipt, CoreError> {
        let _input = self.shared.inputs.lock().await;
        validate_id(operation_id)?;
        for identity in [
            &answer.session_id,
            &answer.run_id,
            &answer.agent_id,
            &answer.agent_turn_id,
            &answer.request_id,
            &answer.skill_id,
        ] {
            validate_id(identity)?;
        }
        super::super::checkpoint::validate_digest(&answer.entry_digest)?;
        let fingerprint = answer.fingerprint()?;
        if let Some(receipt) = self.replay(operation_id, &fingerprint).await? {
            return Ok(receipt);
        }
        self.transition_for(
            Some(&answer.agent_id),
            "skill.consent.answered",
            |state, head| {
                if state.session_id != answer.session_id
                    || head.execution_epoch != answer.execution_epoch
                {
                    return Err(reject(
                        ErrorCode::StaleEpoch,
                        "skill consent answer belongs to another owner",
                    ));
                }
                let turn = state
                    .agents
                    .get(&answer.agent_id)
                    .and_then(|agent| agent.turn.as_ref())
                    .filter(|turn| {
                        turn.agent_turn_id == answer.agent_turn_id
                            && turn.run_id == answer.run_id
                            && turn.status == AgentStatus::WaitingMaterial
                    })
                    .ok_or_else(|| {
                        reject(
                            ErrorCode::StaleRevision,
                            "skill consent answer belongs to an inactive Turn",
                        )
                    })?;
                let selection = state.skill_selection(&turn.agent_turn_id).ok_or_else(|| {
                    reject(
                        ErrorCode::CheckpointConflict,
                        "skill consent selection is absent",
                    )
                })?;
                let candidate = selection
                    .candidates
                    .iter()
                    .find(|candidate| {
                        candidate
                            .source
                            .identity()
                            .is_ok_and(|id| id == answer.skill_id)
                    })
                    .ok_or_else(|| {
                        reject(
                            ErrorCode::UnauthorizedScope,
                            "skill consent answer names another binding",
                        )
                    })?;
                let consent = candidate
                    .consents
                    .last()
                    .filter(|consent| {
                        consent.request_id == answer.request_id
                            && consent.entry_digest == answer.entry_digest
                            && consent.execution_epoch == answer.execution_epoch
                    })
                    .ok_or_else(|| {
                        reject(
                            ErrorCode::StaleRevision,
                            "skill consent request was replaced",
                        )
                    })?;
                if !skills::consent_current(state, candidate, consent)?
                    || consent.approved.is_some()
                {
                    return Err(reject(
                        ErrorCode::OperationConflict,
                        "skill consent is no longer pending",
                    ));
                }
                let selection = skills::selection_slot(state, &answer.agent_id)?
                    .as_mut()
                    .ok_or_else(|| {
                        reject(ErrorCode::CheckpointConflict, "skill selection disappeared")
                    })?;
                let consent = selection
                    .candidates
                    .iter_mut()
                    .flat_map(|candidate| &mut candidate.consents)
                    .find(|consent| consent.request_id == answer.request_id)
                    .ok_or_else(|| {
                        reject(ErrorCode::CheckpointConflict, "skill consent disappeared")
                    })?;
                consent.approved = Some(answer.approved);
                consent.answer_operation_id = Some(operation_id.into());
                selection.validated_step_count = None;
                agent_turn(state, &answer.agent_id)?.status = AgentStatus::Runnable;
                let receipt = OperationReceipt {
                    operation_id: operation_id.into(),
                    request_sha256: fingerprint.clone(),
                    disposition: OperationDisposition::Accepted,
                    assigned_ids: BTreeMap::from([
                        ("request_id".into(), answer.request_id.clone()),
                        ("agent_turn_id".into(), answer.agent_turn_id.clone()),
                        ("skill_id".into(), answer.skill_id.clone()),
                    ]),
                    state_revision: head.state_revision + 1,
                    error: None,
                };
                state
                    .operations
                    .insert(operation_id.into(), receipt.clone());
                encode(&receipt)
            },
        )
        .await?;
        self.shared
            .live
            .lock()
            .await
            .state
            .operations
            .get(operation_id)
            .cloned()
            .ok_or_else(|| {
                reject(
                    ErrorCode::CheckpointConflict,
                    "skill consent receipt disappeared",
                )
            })
    }
}
