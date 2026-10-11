//! Bind approval decisions to the active Turn and commit before authorization.

use super::state::PendingInput;
use super::{ErrorCode, ServiceError, ThreadService, unknown_turn};
use crate::agent::ApprovalRequest;
use crate::store::ExecutionRecord;
use crate::turn::{TurnEventPayload, TurnStatus};

impl ThreadService {
    pub(super) async fn answer_input_serialized(
        &self,
        turn_id: &str,
        request_id: &str,
        approved: bool,
        extra_facts: &[ExecutionRecord],
    ) -> Result<(), ServiceError> {
        {
            let state = self.lock_state();
            let record = state.turns.get(turn_id).ok_or_else(unknown_turn)?;
            if record.snapshot.status != TurnStatus::WaitingForInput
                || record.snapshot.pending_input_id.as_deref() != Some(request_id)
                || record.cancel.is_cancelled()
            {
                return Err(ServiceError::new(
                    ErrorCode::Conflict,
                    "input is not pending for this task",
                ));
            }
        }
        self.append_facts_serialized(
            turn_id,
            TurnEventPayload::InputResolved {
                request_id: request_id.into(),
                approved,
            },
            extra_facts,
        )
        .await?;
        let sender = self
            .lock_state()
            .turns
            .get_mut(turn_id)
            .and_then(|record| record.pending.take())
            .ok_or_else(|| "pending input channel is unavailable".to_string())?
            .response;
        sender.send(approved).map_err(|_| {
            ServiceError::new(ErrorCode::Conflict, "task stopped before accepting input")
        })
    }

    pub(super) async fn request_approval(
        &self,
        turn_id: &str,
        request: ApprovalRequest,
    ) -> Result<(), ServiceError> {
        let gate = self.commit_gate(turn_id)?;
        let _guard = gate.lock().await;
        {
            let mut state = self.lock_state();
            let record = state.turns.get_mut(turn_id).ok_or_else(unknown_turn)?;
            if record.cancel.is_cancelled() || record.fence.pending() {
                let _ = request.response.send(false);
                return Ok(());
            }
            if record.snapshot.status.terminal() || record.pending.is_some() {
                return Err("task cannot accept another pending input".into());
            }
            record.pending = Some(PendingInput {
                response: request.response,
            });
        }
        self.append_serialized(
            turn_id,
            TurnEventPayload::InputRequested {
                request_id: request.id,
                tool_id: request.tool_id,
                tool_name: request.tool_name,
                arguments: request.arguments,
            },
        )
        .await
    }
}
