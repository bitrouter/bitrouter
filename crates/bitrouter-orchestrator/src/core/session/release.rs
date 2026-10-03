//! Durable ownership release. A transport reconnect cannot renew a released
//! grant; only authenticated restoration under a higher epoch can do that.

use super::*;
use crate::core::protocol::OwnershipGrant;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReleaseRecord {
    pub operation_id: String,
    pub grant: OwnershipGrant,
    pub state_revision: u64,
}

fn fingerprint(expected_revision: u64) -> Result<String, CoreError> {
    digest(&json!({"type":"session.release","expected_state_revision":expected_revision}))
}

fn assigned_ids(grant: &OwnershipGrant) -> BTreeMap<String, String> {
    BTreeMap::from([
        ("session_id".into(), grant.session_id.clone()),
        ("core_instance_id".into(), grant.core_instance_id.clone()),
        ("execution_epoch".into(), grant.execution_epoch.to_string()),
    ])
}

fn settled(state: &SessionSnapshot) -> bool {
    responses::active_id(state).is_none()
        && root_queue::settled(state)
        && state.agents.values().all(|agent| {
            agent.turn.as_ref().is_none_or(|turn| {
                turn.steps.iter().all(|step| step.settled)
                    && turn.invocations.iter().all(|call| call.consumed)
                    && turn
                        .core_calls
                        .iter()
                        .all(|call| call.result.is_some() && call.consumed)
            })
        })
}

pub(super) fn released(live: &LiveSession) -> bool {
    live.state
        .releases
        .values()
        .any(|record| &record.grant == live.gate.grant())
}

/// Called under the live lock immediately after adoption, including lost ACKs.
pub(super) fn fence(live: &mut LiveSession) {
    if released(live) {
        live.gate.release();
        live.disconnected.cancel();
    }
}

pub(super) fn validate_history(
    payload: &CheckpointPayload,
    previous: &mut BTreeMap<String, ReleaseRecord>,
) -> Result<(), CoreError> {
    let records: BTreeMap<String, ReleaseRecord> = payload
        .checkpoint
        .state
        .get("releases")
        .cloned()
        .map(serde_json::from_value)
        .transpose()
        .map_err(json_error)?
        .unwrap_or_default();
    for (operation_id, record) in previous.iter() {
        if records.get(operation_id) != Some(record)
            || record.grant.execution_epoch >= payload.identity.execution_epoch
        {
            return Err(reject(
                ErrorCode::CheckpointConflict,
                "release history was rewritten or its epoch appended",
            ));
        }
    }
    let current = records
        .values()
        .filter(|record| record.grant.execution_epoch == payload.identity.execution_epoch)
        .collect::<Vec<_>>();
    let events = payload
        .events
        .iter()
        .filter(|event| event.kind == "session.released")
        .collect::<Vec<_>>();
    if current.len() != events.len() || current.len() > 1 {
        return Err(reject(
            ErrorCode::CheckpointConflict,
            "release event and snapshot disagree",
        ));
    }
    if let Some(record) = current.first()
        && (record.state_revision != payload.checkpoint.state_revision
            || record.grant.core_instance_id != payload.identity.core_instance_id
            || record.grant.session_id != payload.identity.session_id
            || events[0].payload["operation_id"].as_str() != Some(&record.operation_id)
            || events[0].payload != payload.checkpoint.state["operations"][&record.operation_id]
            || payload
                .events
                .last()
                .is_none_or(|event| event.kind != "session.released"))
    {
        return Err(reject(
            ErrorCode::CheckpointConflict,
            "release is not the final state of its owner",
        ));
    }
    *previous = records;
    Ok(())
}

pub(super) fn validate(state: &SessionSnapshot, binding: &Bind) -> Result<(), CoreError> {
    let mut ordered = state.releases.values().collect::<Vec<_>>();
    ordered.sort_by_key(|record| record.state_revision);
    let mut previous_epoch = 0;
    let mut previous_revision = 0;
    for record in ordered {
        validate_id(&record.operation_id)?;
        record.grant.validate()?;
        let receipt = state
            .operations
            .get(&record.operation_id)
            .ok_or_else(|| reject(ErrorCode::CheckpointConflict, "release receipt is missing"))?;
        if !state.releases.contains_key(&record.operation_id)
            || record.grant.session_id != state.session_id
            || record.grant.harness_id != binding.grant.harness_id
            || record.grant.execution_epoch <= previous_epoch
            || record.grant.execution_epoch > binding.durable_head.execution_epoch
            || record.state_revision <= previous_revision
            || record.state_revision > binding.durable_head.state_revision
            || receipt.operation_id != record.operation_id
            || receipt.state_revision != record.state_revision
            || receipt.request_sha256 != fingerprint(record.state_revision.saturating_sub(1))?
            || receipt.disposition != OperationDisposition::Applied
            || receipt.error.is_some()
            || receipt.assigned_ids != assigned_ids(&record.grant)
        {
            return Err(reject(
                ErrorCode::CheckpointConflict,
                "release facts or receipt conflict",
            ));
        }
        previous_epoch = record.grant.execution_epoch;
        previous_revision = record.state_revision;
    }
    if previous_epoch == binding.durable_head.execution_epoch
        && previous_epoch != 0
        && (previous_revision != binding.durable_head.state_revision
            || !state.root_queue.paused
            || !settled(state))
    {
        return Err(reject(
            ErrorCode::CheckpointConflict,
            "released epoch has unsettled or later state",
        ));
    }
    if previous_epoch != 0 && binding.grant.execution_epoch <= previous_epoch {
        return Err(reject(
            ErrorCode::StaleEpoch,
            "restoration must acquire a grant newer than the released epoch",
        ));
    }
    Ok(())
}

impl CoreSession {
    /// Release only settled execution. Pending root inputs remain paused and
    /// retain their identities for a future authenticated ownership grant.
    pub async fn release(
        &self,
        operation_id: &str,
        expected_revision: u64,
    ) -> Result<OperationReceipt, CoreError> {
        let _input = self.shared.inputs.lock().await;
        validate_id(operation_id)?;
        let fingerprint = fingerprint(expected_revision)?;
        if let Some(receipt) = self.replay(operation_id, &fingerprint).await? {
            return Ok(receipt);
        }
        let _driver =
            self.shared.driver.try_lock().map_err(|_| {
                reject(ErrorCode::Busy, "session driver must settle before release")
            })?;
        let grant = {
            let live = self.shared.live.lock().await;
            if !live.gate.can_dispatch() {
                return Err(reject(
                    ErrorCode::CheckpointUnavailable,
                    "release awaits durable authority",
                ));
            }
            // A definite, paired tool result settles its effect even if the
            // original transport delivery lost its completion acknowledgement.
            if !settled(&live.state)
                || live
                    .model_controls
                    .iter()
                    .any(|control| control.strong_count() > 0)
            {
                return Err(reject(
                    ErrorCode::Busy,
                    "session execution and effects must settle before release",
                ));
            }
            if live.provider_evidence.overflowed || !live.provider_evidence.reports.is_empty() {
                return Err(reject(
                    ErrorCode::RecoveryRequired,
                    "provider evidence must be reconciled before release",
                ));
            }
            live.gate.grant().clone()
        };
        self.transition("session.released", |state, head| {
            if head.state_revision != expected_revision {
                return Err(reject(
                    ErrorCode::StaleRevision,
                    "release revision is stale",
                ));
            }
            if !settled(state) {
                return Err(reject(ErrorCode::Busy, "release boundary changed"));
            }
            let revision = head
                .state_revision
                .checked_add(1)
                .ok_or_else(|| reject(ErrorCode::LimitExceeded, "release revision exhausted"))?;
            state.root_queue.paused = true;
            state.releases.insert(
                operation_id.into(),
                ReleaseRecord {
                    operation_id: operation_id.into(),
                    grant: grant.clone(),
                    state_revision: revision,
                },
            );
            let receipt = OperationReceipt {
                operation_id: operation_id.into(),
                request_sha256: fingerprint,
                disposition: OperationDisposition::Applied,
                assigned_ids: assigned_ids(&grant),
                state_revision: revision,
                error: None,
            };
            state
                .operations
                .insert(operation_id.into(), receipt.clone());
            encode(&receipt)
        })
        .await?;
        self.operation(operation_id).await.ok_or_else(|| {
            reject(
                ErrorCode::CheckpointUnavailable,
                "release was not acknowledged",
            )
        })
    }
}
