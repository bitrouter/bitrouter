use std::sync::Arc;

use bitrouter_sdk::caller::CallerContext;

use super::admission::{fingerprint, key_scope};
use super::{ErrorCode, ServiceError, ThreadService, now_ms, unknown_turn};
use crate::store::ExecutionRecord;
use crate::thread::ThreadTarget;
use crate::turn::{SteeringReceipt, SteeringRequest, SteeringStatus, TurnEvent, TurnEventPayload};

pub(super) struct SteeringInput {
    pub(super) receipt: SteeringReceipt,
    pub(super) text: String,
}

impl ThreadService {
    pub(super) fn native_steering_events(
        &self,
        turn_id: &str,
        records: &[ExecutionRecord],
    ) -> Result<Vec<TurnEvent>, ServiceError> {
        let mut resolved = Vec::new();
        for record in records {
            let ExecutionRecord::CoreCheckpoint { batch, limits } = record else {
                continue;
            };
            let checkpoint = batch.decode(limits).map_err(|error| error.message)?;
            let snapshot: crate::core::session::SessionSnapshot =
                serde_json::from_value(checkpoint.checkpoint.state)
                    .map_err(|error| ServiceError::storage(error.to_string()))?;
            let state = self.lock_state();
            let task = state.turns.get(turn_id).ok_or_else(unknown_turn)?;
            for pending in &task.steering {
                if pending.receipt.status != SteeringStatus::Received {
                    continue;
                }
                let Some(core) = snapshot.steering.get(&pending.receipt.input_id) else {
                    continue;
                };
                if core.text != pending.text || core.agent_id != snapshot.agent_id {
                    return Err(ServiceError::storage(
                        "native steering projection differs from accepted input",
                    ));
                }
                use crate::core::session::steering::SteeringDisposition;
                let mut receipt = pending.receipt.clone();
                match core.disposition {
                    SteeringDisposition::Received => continue,
                    SteeringDisposition::Applied => {
                        receipt.status = SteeringStatus::Applied;
                        receipt.context_version = snapshot
                            .agents
                            .get(&snapshot.agent_id)
                            .map(|agent| agent.context_revision);
                        receipt.next_step_id = snapshot
                            .root_turn()
                            .and_then(|turn| {
                                turn.steps.iter().find(|step| {
                                    core.resolved_state_revision.is_some_and(|revision| {
                                        step.input_state_revision >= revision
                                    })
                                })
                            })
                            .map(|step| step.step_id.clone());
                    }
                    SteeringDisposition::Cancelled => {
                        receipt.status = SteeringStatus::NotApplied;
                        receipt.reason =
                            Some("Core task cancelled before applying steering".into());
                    }
                }
                resolved.push(TurnEvent {
                    thread_id: task.thread_id.clone(),
                    server_instance_id: self.inner.instance_id.clone(),
                    turn_id: turn_id.into(),
                    seq: task.snapshot.cursor + resolved.len() as u64 + 1,
                    timestamp_ms: now_ms(),
                    payload: TurnEventPayload::SteeringUpdated {
                        receipt,
                        text: None,
                    },
                });
            }
        }
        Ok(resolved)
    }

    async fn existing_steering(
        &self,
        key: &crate::store::AcceptedKey,
    ) -> Result<SteeringReceipt, ServiceError> {
        let mut receipt = None;
        self.scan_receipt(&key.thread_id, |fact| match fact {
            ExecutionRecord::SteeringReceived {
                receipt: current,
                key: current_key,
                ..
            } if current_key == key.key && Some(&current.turn_id) == key.turn_id.as_ref() => {
                receipt = Some(current)
            }
            ExecutionRecord::SteeringResolved { receipt: current }
                if receipt.as_ref().is_some_and(|previous: &SteeringReceipt| {
                    previous.input_id == current.input_id
                }) =>
            {
                receipt = Some(current)
            }
            _ => {}
        })
        .await?;
        receipt.ok_or_else(|| "accepted steering identity is missing".into())
    }

    pub async fn steer(
        &self,
        target: &ThreadTarget,
        caller: &CallerContext,
        request: SteeringRequest,
    ) -> Result<SteeringReceipt, ServiceError> {
        self.ensure_instance(Some(&target.server_instance_id))?;
        let admission = self.inner.admission.lock().await;
        let scope = key_scope(
            caller,
            Some(&target.thread_id),
            "steer",
            &request.idempotency_key,
        )?;
        let hash = fingerprint(&(&request.expected_turn_id, &request.text))?;
        if let Some(key) = self
            .accepted_key(&scope, &request.idempotency_key, &hash)
            .await?
        {
            drop(admission);
            return self.existing_steering(&key).await;
        }
        self.read_thread(target, caller)?;
        if request.text.trim().is_empty() || request.text.len() > self.inner.limits.request_bytes {
            return Err("nonempty steering within the input bound is required".into());
        }
        let gate = self.thread_gate(&target.thread_id)?;
        let _guard = gate.lock().await;
        self.authorize_active_turn(target, caller, &request.expected_turn_id)?;
        let fence = {
            let state = self.lock_state();
            let task = state
                .turns
                .get(&request.expected_turn_id)
                .ok_or_else(unknown_turn)?;
            if task.cancel.is_cancelled() {
                return Err(ServiceError::new(ErrorCode::Conflict, "Turn is cancelling"));
            }
            if task.steering.len() >= self.inner.limits.steering_inputs_per_turn
                || task
                    .steering
                    .iter()
                    .map(|entry| entry.text.len())
                    .sum::<usize>()
                    .saturating_add(request.text.len())
                    > self.inner.limits.steering_bytes_per_turn
            {
                return Err(ServiceError::new(
                    ErrorCode::Overloaded,
                    "Turn steering capacity is full",
                ));
            }
            Arc::clone(&task.fence)
        };
        let order = self
            .lock_state()
            .turns
            .get(&request.expected_turn_id)
            .ok_or_else(unknown_turn)?
            .steering
            .len() as u64
            + 1;
        let receipt = SteeringReceipt {
            input_id: uuid::Uuid::new_v4().to_string(),
            turn_id: request.expected_turn_id.clone(),
            order,
            status: SteeringStatus::Received,
            context_version: None,
            next_step_id: None,
            reason: None,
        };
        let key = crate::store::AcceptedKey {
            scope,
            key: request.idempotency_key.clone(),
            fingerprint: hash,
            thread_id: target.thread_id.clone(),
            turn_id: Some(request.expected_turn_id.clone()),
        };
        // Seal before database I/O, under the same launch mutex as dispatch. A
        // failed commit stays sealed/blocked; no accepted input is invented.
        fence.set(true);
        self.append_facts_serialized(
            &request.expected_turn_id,
            TurnEventPayload::SteeringUpdated {
                receipt: receipt.clone(),
                text: Some(request.text.clone()),
            },
            &[
                ExecutionRecord::SteeringReceived {
                    receipt: receipt.clone(),
                    text: request.text.clone(),
                    key: request.idempotency_key,
                },
                ExecutionRecord::AcceptedKey { entry: key },
            ],
        )
        .await?;
        let mut state = self.lock_state();
        let task = state
            .turns
            .get_mut(&request.expected_turn_id)
            .ok_or_else(unknown_turn)?;
        task.steering.push(SteeringInput {
            receipt: receipt.clone(),
            text: request.text.clone(),
        });
        task.native.push(receipt.input_id.clone(), request.text);
        if let Some(pending) = task.pending.take() {
            let _ = pending.response.send(false);
        }
        Ok(receipt)
    }
}
