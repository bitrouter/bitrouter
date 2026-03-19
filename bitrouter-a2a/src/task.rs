//! A2A v1.0 Task types.
//!
//! Defines the task lifecycle primitives per the
//! [A2A v1.0 specification](https://a2a-protocol.org/latest/definitions/).

use serde::{Deserialize, Serialize};

use crate::message::{Artifact, Message};

/// Task lifecycle states per A2A v1.0.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum TaskState {
    /// Task accepted, awaiting processing.
    #[serde(rename = "TASK_STATE_SUBMITTED")]
    Submitted,
    /// Task actively executing.
    #[serde(rename = "TASK_STATE_WORKING")]
    Working,
    /// Task completed successfully.
    #[serde(rename = "TASK_STATE_COMPLETED")]
    Completed,
    /// Task execution failed.
    #[serde(rename = "TASK_STATE_FAILED")]
    Failed,
    /// Task canceled by client.
    #[serde(rename = "TASK_STATE_CANCELED")]
    Canceled,
    /// Agent declined the task.
    #[serde(rename = "TASK_STATE_REJECTED")]
    Rejected,
    /// Waiting for additional client input.
    #[serde(rename = "TASK_STATE_INPUT_REQUIRED")]
    InputRequired,
    /// Authentication needed to proceed.
    #[serde(rename = "TASK_STATE_AUTH_REQUIRED")]
    AuthRequired,
}

/// Current status of a task.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TaskStatus {
    /// Current lifecycle state.
    pub state: TaskState,

    /// ISO 8601 timestamp of the status change.
    pub timestamp: String,

    /// Optional agent message accompanying the status change.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<Message>,
}

/// A stateful unit of work in the A2A protocol.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Task {
    /// Unique task identifier.
    pub id: String,

    /// Logical conversation grouping across related tasks.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_id: Option<String>,

    /// Current task status.
    pub status: TaskStatus,

    /// Output artifacts produced by the task.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artifacts: Vec<Artifact>,

    /// Interaction history (messages exchanged during the task).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub history: Vec<Message>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::{MessageRole, Part};

    #[test]
    fn task_state_serializes_v1_format() {
        let json = serde_json::to_string(&TaskState::InputRequired).expect("serialize");
        assert_eq!(json, "\"TASK_STATE_INPUT_REQUIRED\"");

        let parsed: TaskState =
            serde_json::from_str("\"TASK_STATE_AUTH_REQUIRED\"").expect("deserialize");
        assert_eq!(parsed, TaskState::AuthRequired);

        let json = serde_json::to_string(&TaskState::Submitted).expect("serialize");
        assert_eq!(json, "\"TASK_STATE_SUBMITTED\"");
    }

    #[test]
    fn task_round_trip() {
        let task = Task {
            id: "task-001".to_string(),
            context_id: Some("ctx-abc".to_string()),
            status: TaskStatus {
                state: TaskState::Completed,
                timestamp: "2026-03-17T10:30:00Z".to_string(),
                message: Some(Message {
                    role: MessageRole::Agent,
                    parts: vec![Part::text("Done reviewing")],
                    message_id: "msg-resp".to_string(),
                    context_id: None,
                    task_id: Some("task-001".to_string()),
                    reference_task_ids: Vec::new(),
                    metadata: None,
                }),
            },
            artifacts: vec![Artifact {
                artifact_id: "art-001".to_string(),
                name: Some("review".to_string()),
                description: None,
                parts: vec![Part::text("LGTM")],
                metadata: None,
            }],
            history: Vec::new(),
        };

        let json = serde_json::to_string_pretty(&task).expect("serialize");
        let parsed: Task = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(task, parsed);
    }

    #[test]
    fn minimal_task_round_trip() {
        let task = Task {
            id: "task-002".to_string(),
            context_id: None,
            status: TaskStatus {
                state: TaskState::Submitted,
                timestamp: "2026-03-17T10:00:00Z".to_string(),
                message: None,
            },
            artifacts: Vec::new(),
            history: Vec::new(),
        };

        let json = serde_json::to_string(&task).expect("serialize");
        let parsed: Task = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(task, parsed);
        // Verify empty vecs are omitted.
        assert!(!json.contains("artifacts"));
        assert!(!json.contains("history"));
    }
}
