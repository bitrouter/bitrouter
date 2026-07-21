//! Turn-state transitions — the durable, replayable turn lifecycle.
//!
//! ACP v1 reports turn completion in the `session/prompt` **response**, which is
//! bound to the live connection: a manager that detaches mid-turn never learns
//! the outcome, and a reattaching manager has no signal that a turn is running.
//! [`TurnState`] lifts that lifecycle onto the session's durable event stream so
//! it survives detach and replays on reattach (see `REATTACH_SPEC.md`).
//!
//! The shape mirrors ACP v2's `state_update` session update field-for-field
//! (`running` / `idle{stopReason}` / `requires_action`), so the future `acp_v2`
//! wire encoding is a swap at the encoder — not a producer rewrite. Until then
//! the default encoding is a bitrouter-proprietary `_bitrouter/turn_state`
//! extension notification: custom `_`-prefixed notifications are sanctioned by
//! ACP, and a conformant client that doesn't understand it simply ignores it
//! (third-party v1 clients fall back to the `PromptResponse`), while bitrouter's
//! own managers treat this stream as the authoritative turn lifecycle.

use std::sync::Arc;

use agent_client_protocol::schema::v1::{AgentNotification, ExtNotification, StopReason};
use serde::Serialize;

/// JSON-RPC method for the bitrouter turn-state extension notification.
pub const TURN_STATE_METHOD: &str = "_bitrouter/turn_state";

/// One turn-lifecycle transition. `turn_seq` is a per-session monotonic index
/// that correlates the `running` → (`requires_action`) → `idle` transitions of
/// one turn; it is an internal index, not an ACP identity (ACP has no turn id).
///
/// Serializes as the `_bitrouter/turn_state` params, mirroring v2 `state_update`:
/// `{"state":"idle","turnSeq":3,"stopReason":"end_turn"}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum TurnState {
    /// Foreground work started (or resumed after a permission).
    Running {
        #[serde(rename = "turnSeq")]
        turn_seq: u64,
    },
    /// The turn ended. `stop_reason` is present at a work-ending idle (bitrouter
    /// always has one from the upstream `PromptResponse`); it is `None` for a
    /// turn that failed with no clean stop reason. Matches v2 `IdleStateUpdate`,
    /// whose `stopReason` is optional.
    Idle {
        #[serde(rename = "turnSeq")]
        turn_seq: u64,
        #[serde(rename = "stopReason", skip_serializing_if = "Option::is_none")]
        stop_reason: Option<StopReason>,
    },
    /// The turn is blocked awaiting a user action — a pending permission.
    RequiresAction {
        #[serde(rename = "turnSeq")]
        turn_seq: u64,
        #[serde(rename = "requestId")]
        request_id: String,
    },
}

/// The `_bitrouter/turn_state` params: the turn state plus the session id it
/// belongs to. Mirrors how ACP's `session/update` always carries `sessionId`
/// (there via the `SessionNotification` wrapper; here inline, since this is a
/// top-level custom notification rather than a wrapped session update).
#[derive(Debug, Serialize)]
struct TurnStateParams<'a> {
    #[serde(rename = "sessionId")]
    session_id: &'a str,
    #[serde(flatten)]
    state: &'a TurnState,
}

impl TurnState {
    /// Encode as the default (v1-wire) `_bitrouter/turn_state` extension
    /// notification for `session_id`, ready for `ConnectionTo::send_notification`.
    /// The `AgentNotification` is `#[serde(untagged)]` and `ExtNotification` is
    /// `#[serde(transparent)]` over its params, so this serializes on the wire as
    /// `{"method":"_bitrouter/turn_state","params":{"sessionId":…,"state":…,…}}`.
    pub fn to_notification(
        &self,
        session_id: &str,
    ) -> Result<AgentNotification, serde_json::Error> {
        let params = serde_json::value::to_raw_value(&TurnStateParams {
            session_id,
            state: self,
        })?;
        Ok(AgentNotification::ExtNotification(ExtNotification::new(
            TURN_STATE_METHOD,
            Arc::from(params),
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn running_serializes_with_turn_seq() {
        let v = serde_json::to_value(TurnState::Running { turn_seq: 2 }).unwrap();
        assert_eq!(v, serde_json::json!({ "state": "running", "turnSeq": 2 }));
    }

    #[test]
    fn idle_serializes_stop_reason_and_omits_when_none() {
        let with = serde_json::to_value(TurnState::Idle {
            turn_seq: 3,
            stop_reason: Some(StopReason::EndTurn),
        })
        .unwrap();
        assert_eq!(
            with,
            serde_json::json!({ "state": "idle", "turnSeq": 3, "stopReason": "end_turn" })
        );

        let without = serde_json::to_value(TurnState::Idle {
            turn_seq: 4,
            stop_reason: None,
        })
        .unwrap();
        assert_eq!(
            without,
            serde_json::json!({ "state": "idle", "turnSeq": 4 })
        );
    }

    #[test]
    fn requires_action_carries_request_id() {
        let v = serde_json::to_value(TurnState::RequiresAction {
            turn_seq: 5,
            request_id: "r-1".to_string(),
        })
        .unwrap();
        assert_eq!(
            v,
            serde_json::json!({ "state": "requires_action", "turnSeq": 5, "requestId": "r-1" })
        );
    }

    #[test]
    fn to_notification_uses_the_custom_method_and_carries_session_id() {
        let notif = TurnState::Running { turn_seq: 1 }
            .to_notification("rec-7")
            .unwrap();
        assert_eq!(notif.method(), TURN_STATE_METHOD);
        // The params serialize as sessionId + the flattened turn state.
        let wire = serde_json::to_value(&notif).unwrap();
        assert_eq!(
            wire,
            serde_json::json!({ "sessionId": "rec-7", "state": "running", "turnSeq": 1 })
        );
    }
}
