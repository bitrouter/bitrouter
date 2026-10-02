//! Same-owner transport reconciliation and evidence-only late model outcomes.

use super::*;
use crate::core::accounting::work::{CostWorkKind, CostWorkState, report_digest};
use crate::core::protocol::{OwnershipGrant, PendingProviderEvidence, ProviderAttemptEvidence};

impl CoreSession {
    /// The host authenticates/reconnects the existing HarnessPort first. A new
    /// owner or process must use restore instead. Retry Busy after the old SDK
    /// execution has finished cancellation/settlement; never infer quiescence
    /// merely from a disconnected socket or a dropped drive future.
    pub async fn reconnect(
        &self,
        grant: &OwnershipGrant,
        durable: &DurableHead,
    ) -> Result<DurableHead, CoreError> {
        {
            let mut live = self.shared.live.lock().await;
            if live.gate.grant() != grant {
                return Err(reject(
                    ErrorCode::UnauthorizedScope,
                    "reconnect must preserve the exact ownership grant",
                ));
            }
            live.gate.disconnect();
            live.disconnected.cancel();
        }
        let _input = self.shared.inputs.lock().await;
        let _driver =
            self.shared.driver.try_lock().map_err(|_| {
                reject(ErrorCode::Busy, "previous session driver is still settling")
            })?;
        {
            let _commit = self.shared.commits.lock().await;
            let mut live = self.shared.live.lock().await;
            live.model_controls
                .retain(|control| control.strong_count() > 0);
            if !live.model_controls.is_empty() {
                return Err(reject(
                    ErrorCode::Busy,
                    "previous SDK work is still settling",
                ));
            }
            if live.provider_evidence.overflowed {
                return Err(reject(
                    ErrorCode::RecoveryRequired,
                    "provider evidence exceeded the volatile bound; explicit restoration is required",
                ));
            }
            live.reconnecting = true;
            live.gate.block_dispatch();
            if let Some(payload) = live.gate.reconnect(durable)? {
                adopt_pending(&mut live, &payload)?;
            }
            live.connection_generation =
                live.connection_generation.checked_add(1).ok_or_else(|| {
                    reject(ErrorCode::LimitExceeded, "connection generation exhausted")
                })?;
            live.disconnected = CancellationToken::new();
        }
        let result = self.finish_reconnect().await;
        let mut live = self.shared.live.lock().await;
        live.reconnecting = false;
        if result.is_err() {
            live.gate.disconnect();
            live.disconnected.cancel();
        } else {
            // Reads and cancellation are safe to repeat with their original
            // identities. An uncertain tool execution requires reconciliation.
            live.sent_materials.clear();
            live.cancelled_tools.clear();
            let resolved = live
                .provisional_blocks
                .iter()
                .filter(|id| {
                    live.state.operations.contains_key(*id)
                        || live
                            .state
                            .agents
                            .values()
                            .filter_map(|agent| agent.turn.as_ref())
                            .flat_map(|turn| &turn.core_calls)
                            .any(|call| &call.invocation_id == *id && call.result.is_some())
                })
                .cloned()
                .collect::<Vec<_>>();
            for id in resolved {
                live.provisional_blocks.remove(&id);
            }
            if live.provisional_blocks.is_empty() {
                live.gate.clear_dispatch_block();
            }
            self.shared.changed.notify_one();
        }
        result.map(|()| live.gate.head().clone())
    }

    async fn finish_reconnect(&self) -> Result<(), CoreError> {
        // A head equal to the previous local head means the outstanding batch
        // was not adopted. Resubmit exactly its original identity and bytes.
        {
            let _commit = self.shared.commits.lock().await;
            let (pending, disconnected) = {
                let live = self.shared.live.lock().await;
                (live.gate.pending().cloned(), live.disconnected.clone())
            };
            if let Some(batch) = pending {
                let ack = tokio::select! {
                    biased;
                    _ = disconnected.cancelled() => return Err(reject(ErrorCode::CheckpointUnavailable, "reconnect interrupted during batch retransmission")),
                    ack = self.shared.harness.commit(batch) => ack,
                }.map_err(unknown_commit)?;
                let mut live = self.shared.live.lock().await;
                let payload = live
                    .gate
                    .acknowledge(&ack)
                    .map_err(unknown_commit)?
                    .ok_or_else(|| {
                        unknown_commit(reject(
                            ErrorCode::CheckpointConflict,
                            "retransmission ACK did not adopt the pending batch",
                        ))
                    })?;
                adopt_pending(&mut live, &payload)?;
            }
        }
        let active_ms = {
            let live = self.shared.live.lock().await;
            if live
                .state
                .agents
                .values()
                .filter_map(|agent| agent.turn.as_ref())
                .flat_map(|turn| &turn.invocations)
                .any(|call| {
                    live.unresolved_tool_deliveries
                        .contains(&call.dispatch.invocation_id)
                        && call
                            .result
                            .as_ref()
                            .is_none_or(|result| result.status == ToolOutcome::EffectUnknown)
                })
            {
                return Err(reject(
                    ErrorCode::RecoveryRequired,
                    "tool delivery is uncertain; authenticated restoration must reconcile its status",
                ));
            }
            live.activity.elapsed_ms()
        };
        let mut outputs = Vec::new();
        self.transition("session.reconnected", |state, _| {
            if let Some(run) = &mut state.run {
                run.active_ms = run.active_ms.max(active_ms);
            }
            // Only already committed complete output can be applied. Buffered
            // reports imported below are evidence for closed interrupted steps.
            outputs = recovery::resume_model_steps(state);
            Ok(json!({"same_owner":true}))
        })
        .await?;
        {
            let mut live = self.shared.live.lock().await;
            // Running workspace work continues while the reconnect checkpoint
            // awaits ACK. Rebuilding the clock must not erase that interval.
            let active_ms = live
                .activity
                .elapsed_ms()
                .max(live.state.run.as_ref().map_or(0, |run| run.active_ms));
            live.activity = Activity::restored(active_ms);
            let tools = tool_status::activity_ids(&live.state);
            live.activity.synchronize_tools(&tools);
        }
        let reports = self
            .shared
            .live
            .lock()
            .await
            .provider_evidence
            .reports
            .clone();
        for report in reports {
            self.import_provider_evidence_locked(
                &format!("evidence:{}", report.attempt_id),
                report.clone(),
            )
            .await?;
            self.shared
                .live
                .lock()
                .await
                .provider_evidence
                .reports
                .retain(|pending| pending != &report);
        }
        for (agent_id, step_id, request_id, output) in outputs {
            if self
                .snapshot()
                .await
                .agents
                .get(&agent_id)
                .and_then(|agent| agent.turn.as_ref())
                .is_some_and(|turn| turn.status == AgentStatus::RecoveryRequired)
            {
                continue;
            }
            if let Err(error) = self
                .apply_output(&agent_id, &step_id, &request_id, &output)
                .await
            {
                if error.commit_status == CommitStatus::NotCommitted {
                    self.fail(&agent_id, &error.message).await?;
                } else {
                    return Err(error);
                }
            }
        }
        Ok(())
    }

    /// Export bounded, unacknowledged reports for an authenticated harness to
    /// preserve before replacement. Reading this is not a durable commit.
    pub async fn pending_provider_evidence(&self) -> PendingProviderEvidence {
        self.shared.live.lock().await.provider_evidence.clone()
    }

    /// Import only evidence for this session's retained, admitted attempt.
    /// The host authenticates its source. Neither output nor tools are applied,
    /// and importing late evidence never invokes SDK settlement a second time.
    pub async fn provider_evidence(
        &self,
        operation_id: &str,
        evidence: ProviderAttemptEvidence,
    ) -> Result<OperationReceipt, CoreError> {
        let _input = self.shared.inputs.lock().await;
        self.import_provider_evidence_locked(operation_id, evidence)
            .await
    }

    async fn import_provider_evidence_locked(
        &self,
        operation_id: &str,
        evidence: ProviderAttemptEvidence,
    ) -> Result<OperationReceipt, CoreError> {
        validate_id(operation_id)?;
        let fingerprint = digest(&json!({"type":"model.evidence","evidence":evidence}))?;
        if let Some(receipt) = self.replay(operation_id, &fingerprint).await? {
            return Ok(receipt);
        }
        if serde_json::to_vec(&evidence).map_err(json_error)?.len() as u64
            > self.shared.limits.checkpoint_bytes
        {
            return Err(reject(
                ErrorCode::LimitExceeded,
                "provider evidence exceeds checkpoint bound",
            ));
        }
        let owner = {
            let live = self.shared.live.lock().await;
            let work = evidence_work(&live.state, &evidence)?;
            if live
                .model_controls
                .iter()
                .filter_map(std::sync::Weak::upgrade)
                .any(|control| {
                    control.run_id == evidence.run_id && control.agent_turn_id == work.agent_turn_id
                })
            {
                return Err(reject(
                    ErrorCode::Busy,
                    "original SDK execution is still settling",
                ));
            }
            work.agent_id.clone()
        };
        self.transition_scoped(
            Some(&owner),
            Some(&evidence.run_id),
            "model.evidence.recorded",
            |state, head, _| {
                apply_evidence(state, &evidence)?;
                let receipt = OperationReceipt {
                    operation_id: operation_id.into(),
                    request_sha256: fingerprint,
                    disposition: OperationDisposition::Applied,
                    assigned_ids: BTreeMap::from([
                        ("run_id".into(), evidence.run_id.clone()),
                        ("attempt_id".into(), evidence.attempt_id.clone()),
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
        if let Some(active_ms) = evidence.active_ms {
            let mut live = self.shared.live.lock().await;
            if live
                .state
                .run
                .as_ref()
                .is_some_and(|run| run.run_id == evidence.run_id)
            {
                live.activity.observe_elapsed(active_ms);
            }
        }
        self.operation(operation_id).await.ok_or_else(|| {
            reject(
                ErrorCode::CheckpointUnavailable,
                "provider evidence receipt missing",
            )
        })
    }
}

fn adopt_pending(live: &mut LiveSession, payload: &CheckpointPayload) -> Result<(), CoreError> {
    live.state = live.pending.take().ok_or_else(|| {
        reject(
            ErrorCode::CheckpointConflict,
            "reconciled batch has no retained candidate",
        )
    })?;
    if payload
        .events
        .iter()
        .any(|event| event.kind == "input.accepted")
    {
        live.activity = Activity::restored(live.state.run.as_ref().map_or(0, |run| run.active_ms));
    }
    Ok(())
}

fn unknown_commit(mut error: CoreError) -> CoreError {
    error.commit_status = CommitStatus::Unknown;
    error
}

fn evidence_work<'a>(
    state: &'a SessionSnapshot,
    evidence: &ProviderAttemptEvidence,
) -> Result<&'a crate::core::accounting::work::CostWork, CoreError> {
    state
        .cost_work
        .get(&evidence.run_id)
        .and_then(|ledger| ledger.work.get(&evidence.attempt_id))
        .filter(|work| work.kind == CostWorkKind::ProviderAttempt)
        .ok_or_else(|| {
            reject(
                ErrorCode::UnauthorizedScope,
                "evidence has no owned model attempt",
            )
        })
}

fn apply_evidence(
    state: &mut SessionSnapshot,
    evidence: &ProviderAttemptEvidence,
) -> Result<(), CoreError> {
    let work = evidence_work(state, evidence)?.clone();
    let source = work.provider_source.as_ref().ok_or_else(|| {
        reject(
            ErrorCode::RecoveryRequired,
            "legacy attempt has no retained serving admission",
        )
    })?;
    if work.request_id.as_ref() != Some(&evidence.report.request_id)
        || source.attempt_index != evidence.report.attempt_index
        || source.route != evidence.report.route
    {
        return Err(reject(
            ErrorCode::OperationConflict,
            "provider evidence differs from frozen attempt admission",
        ));
    }
    let report_sha256 = report_digest(&evidence.report)?;
    let first = work.outcome_sha256.is_none();
    if work
        .outcome_sha256
        .as_ref()
        .is_some_and(|previous| previous != &report_sha256)
        || state
            .provider_evidence
            .get(&evidence.attempt_id)
            .is_some_and(|previous| previous != evidence)
    {
        return Err(reject(
            ErrorCode::OperationConflict,
            "provider attempt already has different evidence",
        ));
    }
    if let Some(turn) = state
        .agents
        .get_mut(&work.agent_id)
        .and_then(|agent| agent.turn.as_mut())
        .filter(|turn| turn.agent_turn_id == work.agent_turn_id && turn.run_id == evidence.run_id)
        && let Some(step) = turn
            .steps
            .iter_mut()
            .find(|step| Some(&step.step_id) == work.step_id.as_ref())
    {
        if first && !step.interrupted {
            return Err(reject(
                ErrorCode::RecoveryRequired,
                "original model step is not reconciled",
            ));
        }
        let attempt = step
            .attempts
            .iter_mut()
            .find(|attempt| attempt.attempt_id == evidence.attempt_id)
            .ok_or_else(|| {
                reject(
                    ErrorCode::CheckpointConflict,
                    "retained step lost its attempt",
                )
            })?;
        let plan = step.plan.as_ref().ok_or_else(|| {
            reject(
                ErrorCode::CheckpointConflict,
                "retained attempt lost its plan",
            )
        })?;
        if first {
            attempt.receipt = Some(ExecutionReceipt::capture(
                &step.decision_id,
                &attempt.attempt_id,
                plan,
                evidence.report.clone(),
            ));
        }
    }
    let ledger = state
        .cost_work
        .get_mut(&evidence.run_id)
        .ok_or_else(|| reject(ErrorCode::UnauthorizedScope, "evidence run disappeared"))?;
    let entry = ledger
        .work
        .get_mut(&evidence.attempt_id)
        .ok_or_else(|| reject(ErrorCode::UnauthorizedScope, "evidence attempt disappeared"))?;
    entry.state = CostWorkState::OutcomeRecorded;
    entry.elapsed_ms = Some(evidence.report.elapsed_ms);
    entry.token_estimate = Some(evidence.report.token_cost.clone());
    entry.outcome_sha256 = Some(report_sha256);
    if let Some(run) = state
        .run
        .as_mut()
        .filter(|run| run.run_id == evidence.run_id)
    {
        if first && let Some(accounting) = &mut run.token_accounting {
            accounting.record(&evidence.report.token_cost);
        }
        if let Some(active_ms) = evidence.active_ms {
            run.active_ms = run.active_ms.max(active_ms);
        }
    }
    state
        .provider_evidence
        .insert(evidence.attempt_id.clone(), evidence.clone());
    Ok(())
}

impl StepControl {
    pub(super) async fn retain_provider_evidence(
        &self,
        step_id: &str,
        report: NativeAttemptReport,
        active_ms: u64,
    ) {
        let mut live = self.session.shared.live.lock().await;
        let attempt = live
            .state
            .agents
            .get(&self.agent_id)
            .and_then(|agent| agent.turn.as_ref())
            .filter(|turn| turn.agent_turn_id == self.agent_turn_id)
            .and_then(|turn| turn.steps.iter().find(|step| step.step_id == step_id))
            .and_then(|step| {
                step.attempts
                    .iter()
                    .find(|attempt| attempt.index == report.attempt_index)
            });
        let Some(attempt) = attempt else {
            live.provider_evidence.overflowed = true;
            return;
        };
        let evidence = ProviderAttemptEvidence {
            run_id: self.run_id.clone(),
            attempt_id: attempt.attempt_id.clone(),
            report,
            active_ms: Some(active_ms),
        };
        // A pending exact outcome batch already retains this report; do not
        // charge the volatile bound twice or import it later as a new outcome.
        if live
            .pending
            .as_ref()
            .into_iter()
            .chain(std::iter::once(&live.state))
            .flat_map(|state| state.agents.values())
            .filter_map(|agent| agent.turn.as_ref())
            .flat_map(|turn| &turn.steps)
            .flat_map(|step| &step.attempts)
            .any(|attempt| {
                attempt.attempt_id == evidence.attempt_id
                    && attempt
                        .receipt
                        .as_ref()
                        .is_some_and(|receipt| receipt.report == evidence.report)
            })
        {
            return;
        }
        if live.provider_evidence.reports.contains(&evidence) {
            return;
        }
        let mut candidate = live.provider_evidence.clone();
        candidate.reports.push(evidence);
        let size = serde_json::to_vec(&candidate)
            .ok()
            .map(|bytes| bytes.len() as u64);
        let pending_size = live.gate.pending().map_or(Some(0), |batch| {
            serde_json::to_vec(batch)
                .ok()
                .map(|bytes| bytes.len() as u64)
        });
        if size
            .zip(pending_size)
            .and_then(|(size, pending)| size.checked_add(pending))
            .is_some_and(|size| size <= self.session.shared.limits.unacknowledged_bytes)
        {
            live.provider_evidence = candidate;
        } else {
            live.provider_evidence.overflowed = true;
        }
    }
}
