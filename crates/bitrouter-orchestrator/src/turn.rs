//! One accepted user input and its execution, approval and steering contracts.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::agent::ToolMode;
use crate::item::{LiveActivity, MAX_LIVE_BYTES};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnStatus {
    Queued,
    Accepted,
    Running,
    WaitingForInput,
    Completed,
    Failed,
    Cancelled,
    Interrupted,
    RecoveryRequired,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerificationStatus {
    Passed,
    Failed,
    Unavailable,
    NotRequested,
    Denied,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerificationEvidence {
    pub command: String,
    #[serde(default)]
    pub interpreter: Option<serde_json::Value>,
    pub exit_status: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
    pub timed_out: bool,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TurnEventPayload {
    SteeringUpdated {
        receipt: crate::turn::SteeringReceipt,
        text: Option<String>,
    },
    TurnQueued {
        user_item_id: String,
        prompt: String,
        queue_order: u64,
    },
    Accepted {
        #[serde(default)]
        user_item_id: String,
        prompt: String,
        workspace: PathBuf,
        model: String,
        #[serde(default)]
        tool_mode: ToolMode,
        #[serde(default)]
        idempotency_key: Option<String>,
        #[serde(default)]
        request_fingerprint: Option<String>,
    },
    Started,
    AssistantDelta {
        #[serde(default)]
        item_id: String,
        text: String,
    },
    ToolOutputDelta {
        id: String,
        source: String,
        text: String,
    },
    InputRequested {
        request_id: String,
        tool_id: String,
        tool_name: String,
        arguments: String,
    },
    InputResolved {
        request_id: String,
        approved: bool,
    },
    CancelRequested,
    Finished {
        status: TurnStatus,
        detail: String,
        final_answer: Option<String>,
        verification: VerificationStatus,
        verification_evidence: Option<VerificationEvidence>,
        unknown_effect: bool,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TurnEvent {
    pub thread_id: String,
    pub server_instance_id: String,
    pub turn_id: String,
    pub seq: u64,
    pub timestamp_ms: u64,
    pub payload: TurnEventPayload,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TurnSnapshot {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resources: Option<crate::harness::HarnessInventory>,
    #[serde(default)]
    pub steering: Vec<crate::turn::SteeringReceipt>,
    pub thread_id: String,
    pub server_instance_id: String,
    pub model: String,
    pub turn_id: String,
    pub status: TurnStatus,
    pub cursor: u64,
    pub workspace: PathBuf,
    #[serde(default)]
    pub tool_mode: ToolMode,
    pub final_answer: Option<String>,
    pub detail: Option<String>,
    pub unknown_effect: bool,
    pub verification: VerificationStatus,
    pub verification_evidence: Option<VerificationEvidence>,
    pub pending_input_id: Option<String>,
    pub pending_input: Option<InputRequest>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub live: Option<LiveActivity>,
}

/// Complete approval metadata is available even when its event was evicted.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InputRequest {
    pub request_id: String,
    pub tool_id: String,
    pub tool_name: String,
    pub arguments: String,
}

impl TurnStatus {
    pub fn terminal(self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Failed | Self::Cancelled | Self::Interrupted
        )
    }
}

impl TurnSnapshot {
    pub fn apply(&mut self, event: &TurnEvent) {
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

    pub(crate) fn apply_payload(&mut self, payload: &TurnEventPayload) {
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TurnReceipt {
    pub thread_id: String,
    pub turn_id: String,
    pub queue_order: u64,
    pub status: TurnStatus,
}

/// Small control facts; model and tool content lives only in canonical facts.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TurnLifecycle {
    Started,
    InputRequested {
        request_id: String,
        tool_id: String,
        tool_name: String,
        arguments: String,
    },
    InputResolved {
        request_id: String,
        approved: bool,
    },
    CancelRequested,
    SteeringUpdated {
        receipt: SteeringReceipt,
        text: Option<String>,
    },
    Finished {
        status: TurnStatus,
        detail: String,
        final_answer: Option<String>,
        verification: crate::turn::VerificationStatus,
        verification_evidence: Option<crate::turn::VerificationEvidence>,
        unknown_effect: bool,
    },
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SteeringStatus {
    Received,
    Applied,
    NotApplied,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SteeringReceipt {
    pub input_id: String,
    pub turn_id: String,
    pub order: u64,
    pub status: SteeringStatus,
    pub context_version: Option<u64>,
    pub next_step_id: Option<String>,
    pub reason: Option<String>,
}

impl TurnLifecycle {
    pub(crate) fn payload(&self) -> crate::turn::TurnEventPayload {
        use crate::turn::TurnEventPayload as P;
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
