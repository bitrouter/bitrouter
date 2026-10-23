//! One accepted user input and its execution, approval and steering contracts.

use bitrouter_ai::harness::turn::{
    InputRequest, TurnEvent, TurnEventPayload, TurnLifecycle, TurnSnapshot, TurnStatus,
};

use serde::{Deserialize, Serialize};

use crate::harness::item::MAX_LIVE_BYTES;
use bitrouter_ai::harness::item::LiveActivity;

/// Application-owned helpers for the public TurnSnapshot contract.
pub(crate) trait TurnSnapshotExt: Sized {
    fn apply(&mut self, event: &TurnEvent);
    fn apply_payload(&mut self, payload: &TurnEventPayload);
    fn live(&mut self, item_id: Option<&str>, kind: &str, text: &str);
}

impl TurnSnapshotExt for TurnSnapshot {
    fn apply(&mut self, event: &TurnEvent) {
        if event.server_instance_id != self.server_instance_id
            || event.turn_id != self.turn_id
            || event.thread_id != self.thread_id
            || event.seq <= self.cursor
                && !matches!(
                    event.payload,
                    TurnEventPayload::AssistantDelta { .. }
                        | TurnEventPayload::ToolOutputDelta { .. }
                )
        {
            return;
        }
        if !matches!(
            event.payload,
            TurnEventPayload::AssistantDelta { .. } | TurnEventPayload::ToolOutputDelta { .. }
        ) {
            self.cursor = event.seq;
        }
        self.apply_payload(&event.payload);
    }

    fn apply_payload(&mut self, payload: &TurnEventPayload) {
        match payload {
            TurnEventPayload::TurnQueued { .. } => self.status = TurnStatus::Queued,
            TurnEventPayload::SteeringUpdated { receipt, .. } => {
                if let Some(current) = self
                    .steering
                    .iter_mut()
                    .find(|current| current.input_id == receipt.input_id)
                {
                    *current = receipt.clone();
                } else {
                    self.steering.push(receipt.clone());
                }
            }
            TurnEventPayload::Accepted { .. } => self.status = TurnStatus::Accepted,
            TurnEventPayload::Started | TurnEventPayload::InputResolved { .. } => {
                self.status = TurnStatus::Running;
                self.pending_input_id = None;
                self.pending_input = None;
            }
            TurnEventPayload::InputRequested {
                request_id,
                tool_id,
                tool_name,
                arguments,
            } => {
                self.status = TurnStatus::WaitingForInput;
                self.pending_input_id = Some(request_id.clone());
                self.pending_input = Some(InputRequest {
                    request_id: request_id.clone(),
                    tool_id: tool_id.clone(),
                    tool_name: tool_name.clone(),
                    arguments: arguments.clone(),
                });
            }
            TurnEventPayload::Finished {
                status,
                detail,
                final_answer,
                verification,
                verification_evidence,
                unknown_effect,
            } => {
                self.status = *status;
                self.detail = Some(detail.clone());
                self.unknown_effect = *unknown_effect;
                self.final_answer = final_answer.clone();
                self.verification = *verification;
                self.verification_evidence = verification_evidence.clone();
                self.pending_input_id = None;
                self.pending_input = None;
                self.live = None;
            }
            TurnEventPayload::AssistantDelta { item_id, text } => {
                self.live(Some(item_id), "assistant", text)
            }
            TurnEventPayload::ToolOutputDelta { id, source, text } => self.live(
                Some(id),
                &format!("shell {id}"),
                &format!("[{source}] {text}"),
            ),
            TurnEventPayload::CancelRequested => {}
        }
    }
    fn live(&mut self, item_id: Option<&str>, kind: &str, text: &str) {
        let live = self.live.get_or_insert_with(|| LiveActivity {
            item_id: item_id.map(str::to_owned),
            kind: kind.into(),
            text: String::new(),
            truncated: false,
        });
        if live.kind != kind || live.item_id.as_deref() != item_id {
            live.item_id = item_id.map(str::to_owned);
            live.kind = kind.into();
            live.text.clear();
            live.truncated = false;
        }
        live.text.push_str(text);
        if live.text.len() > MAX_LIVE_BYTES {
            let mut start = live.text.len() - MAX_LIVE_BYTES;
            while !live.text.is_char_boundary(start) {
                start += 1;
            }
            live.text.drain(..start);
            live.truncated = true;
        }
    }
}

#[derive(Serialize, Deserialize)]
pub struct TurnRequest {
    pub prompt: String,
    pub idempotency_key: String,
}

#[derive(Serialize, Deserialize)]
pub struct CancelTurnRequest {
    pub turn_id: String,
    pub idempotency_key: String,
}

#[derive(Serialize, Deserialize)]
pub struct ApprovalAnswer {
    pub turn_id: String,
    pub request_id: String,
    pub approved: bool,
    pub idempotency_key: String,
}

#[derive(Serialize, Deserialize)]
pub struct SteeringRequest {
    pub expected_turn_id: String,
    pub text: String,
    pub idempotency_key: String,
}

/// Application-owned helpers for the public TurnLifecycle contract.
pub(crate) trait TurnLifecycleExt: Sized {
    fn payload(&self) -> bitrouter_ai::harness::turn::TurnEventPayload;
}

impl TurnLifecycleExt for TurnLifecycle {
    fn payload(&self) -> bitrouter_ai::harness::turn::TurnEventPayload {
        use bitrouter_ai::harness::turn::TurnEventPayload as P;
        match self {
            Self::Started => P::Started,
            Self::InputRequested {
                request_id,
                tool_id,
                tool_name,
                arguments,
            } => P::InputRequested {
                request_id: request_id.clone(),
                tool_id: tool_id.clone(),
                tool_name: tool_name.clone(),
                arguments: arguments.clone(),
            },
            Self::InputResolved {
                request_id,
                approved,
            } => P::InputResolved {
                request_id: request_id.clone(),
                approved: *approved,
            },
            Self::CancelRequested => P::CancelRequested,
            Self::SteeringUpdated { receipt, text } => P::SteeringUpdated {
                receipt: receipt.clone(),
                text: text.clone(),
            },
            Self::Finished {
                status,
                detail,
                final_answer,
                verification,
                verification_evidence,
                unknown_effect,
            } => P::Finished {
                status: *status,
                detail: detail.clone(),
                final_answer: final_answer.clone(),
                verification: *verification,
                verification_evidence: verification_evidence.clone(),
                unknown_effect: *unknown_effect,
            },
        }
    }
}
