use std::sync::Arc;

use bitrouter_sdk::caller::CallerContext;
use bitrouter_sdk::language_model::{Message, Role};

use super::admission::{fingerprint, key_scope};
use super::commit::lifecycle_fact;
use super::{ErrorCode, ServiceError, ThreadService, now_ms, unknown_turn};
use crate::control::ModelBoundary;
use crate::store::ExecutionRecord;
use crate::thread::ThreadTarget;
use crate::turn::{SteeringReceipt, SteeringRequest, SteeringStatus, TurnEvent, TurnEventPayload};

pub(super) struct SteeringInput {
    pub(super) receipt: SteeringReceipt,
    pub(super) text: String,
}

impl ThreadService {
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
            text: request.text,
        });
        if let Some(pending) = task.pending.take() {
            let _ = pending.response.send(false);
        }
        Ok(receipt)
    }

    pub(super) async fn prepare_model(
        &self,
        turn_id: &str,
        request: &mut ModelBoundary,
    ) -> Result<(bitrouter_sdk::language_model::Prompt, u64), String> {
        let gate = self
            .commit_gate(turn_id)
            .map_err(|error| error.to_string())?;
        let _guard = gate.lock().await;
        let pending = {
            let state = self.lock_state();
            let task = state
                .turns
                .get(turn_id)
                .ok_or_else(unknown_turn)
                .map_err(|error| error.to_string())?;
            if task.cancel.is_cancelled() {
                return Err("Turn cancelled before model step".into());
            }
            task.steering
                .iter()
                .filter(|entry| entry.receipt.status == SteeringStatus::Received)
                .map(|entry| (entry.receipt.clone(), entry.text.clone()))
                .collect::<Vec<_>>()
        };
        let mut facts = Vec::new();
        let mut payloads = Vec::new();
        for (mut receipt, text) in pending {
            request
                .prompt
                .messages
                .push(Message::text(Role::User, text));
            request.context_version = request.context_version.saturating_add(1);
            receipt.status = SteeringStatus::Applied;
            receipt.context_version = Some(request.context_version);
            receipt.next_step_id = Some(request.step_id.clone());
            facts.push(ExecutionRecord::SteeringResolved {
                receipt: receipt.clone(),
            });
            payloads.push(TurnEventPayload::SteeringUpdated {
                receipt,
                text: None,
            });
        }
        crate::context::validate_history(&request.prompt.messages)?;
        if serde_json::to_vec(&request.prompt)
            .map_err(|error| error.to_string())?
            .len()
            > request.max_bytes
        {
            return Err("steering cannot be applied within model context capacity".into());
        }
        facts.push(ExecutionRecord::ModelRequest {
            step_id: request.step_id.clone(),
            item_id: request.item_id.clone(),
            context_version: request.context_version,
            prompt: Box::new(request.prompt.clone()),
        });
        let events = {
            let state = self.lock_state();
            let task = state
                .turns
                .get(turn_id)
                .ok_or_else(unknown_turn)
                .map_err(|error| error.to_string())?;
            payloads
                .into_iter()
                .enumerate()
                .map(|(index, payload)| TurnEvent {
                    thread_id: task.thread_id.clone(),
                    server_instance_id: self.inner.instance_id.clone(),
                    turn_id: turn_id.into(),
                    seq: task.snapshot.cursor + index as u64 + 1,
                    timestamp_ms: now_ms(),
                    payload,
                })
                .collect::<Vec<_>>()
        };
        facts.extend(
            events
                .iter()
                .cloned()
                .filter_map(|event| lifecycle_fact(&event)),
        );
        self.commit_serialized(turn_id, &facts)
            .await
            .map_err(|error| error.to_string())?;
        let mut state = self.lock_state();
        for fact in &facts {
            if let ExecutionRecord::SteeringResolved { receipt } = fact
                && let Some(task) = state.turns.get_mut(turn_id)
                && let Some(entry) = task
                    .steering
                    .iter_mut()
                    .find(|entry| entry.receipt.input_id == receipt.input_id)
            {
                entry.receipt = receipt.clone();
            }
        }
        for event in events {
            self.append_locked(&mut state, turn_id, event.payload)
                .map_err(|error| error.to_string())?;
        }
        if let Some(task) = state.turns.get(turn_id) {
            task.fence.set(false);
        }
        Ok((request.prompt.clone(), request.context_version))
    }
}
