//! A2A v0.3.0 Task types.
//!
//! Defines the task lifecycle primitives per the
//! [A2A v0.3.0 specification](https://a2a-protocol.org/latest/definitions/).

use serde::{Deserialize, Serialize};

use crate::message::{Artifact, Message};

/// Task lifecycle states per A2A v0.3.0.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum TaskState {
    /// Task accepted, awaiting processing.
    #[serde(rename = "submitted")]
    Submitted,
    /// Task actively executing.
    #[serde(rename = "working")]
    Working,
    /// Task completed successfully.
    #[serde(rename = "completed")]
    Completed,
    /// Task execution failed.
    #[serde(rename = "failed")]
    Failed,
    /// Task canceled by client.
    #[serde(rename = "canceled")]
    Canceled,
    /// Agent declined the task.
    #[serde(rename = "rejected")]
    Rejected,
    /// Waiting for additional client input.
    #[serde(rename = "input-required")]
    InputRequired,
    /// Authentication needed to proceed.
    #[serde(rename = "auth-required")]
    AuthRequired,
    /// Unknown or unrecognized state.
    #[serde(rename = "unknown")]
    Unknown,
}

/// Current status of a task.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TaskStatus {
    /// Current lifecycle state.
    pub state: TaskState,

    /// ISO 8601 timestamp of the status change.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<String>,

    /// Optional agent message accompanying the status change.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<Message>,
}

/// Request parameters for the `tasks/get` JSON-RPC method.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct GetTaskRequest {
    /// Task ID to retrieve.
    pub id: String,

    /// Maximum number of history messages to return.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub history_length: Option<u32>,
}

/// Request parameters for the `tasks/list` JSON-RPC method.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ListTasksRequest {
    /// Filter by context ID.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_id: Option<String>,

    /// Filter by task state.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<TaskState>,

    /// Filter tasks with status timestamp after this ISO 8601 value.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status_timestamp_after: Option<String>,

    /// Maximum number of tasks per page.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub page_size: Option<u32>,

    /// Cursor for pagination.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub page_token: Option<String>,

    /// Maximum number of history messages per task.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub history_length: Option<u32>,

    /// Whether to include artifacts in the response.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub include_artifacts: Option<bool>,
}

/// Response for the `tasks/list` JSON-RPC method.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ListTasksResponse {
    /// Tasks matching the query.
    pub tasks: Vec<Task>,

    /// Cursor for the next page, if more results exist.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_page_token: Option<String>,

    /// Number of tasks in this page.
    pub page_size: u32,

    /// Total number of tasks matching the query.
    pub total_size: u32,
}

/// A stateful unit of work in the A2A protocol.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Task {
    /// Object kind — always `"task"`.
    #[serde(default = "default_task_kind")]
    pub kind: String,

    /// Unique task identifier.
    pub id: String,

    /// Logical conversation grouping across related tasks.
    pub context_id: String,

    /// Current task status.
    pub status: TaskStatus,

    /// Output artifacts produced by the task.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artifacts: Vec<Artifact>,

    /// Interaction history (messages exchanged during the task).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub history: Vec<Message>,

    /// Extension metadata.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
}

fn default_task_kind() -> String {
    "task".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::{MessageRole, Part};

    #[test]
    fn task_state_serializes_v03_format() {
        let json = serde_json::to_string(&TaskState::InputRequired).expect("serialize");
        assert_eq!(json, "\"input-required\"");

        let parsed: TaskState = serde_json::from_str("\"auth-required\"").expect("deserialize");
        assert_eq!(parsed, TaskState::AuthRequired);

        let json = serde_json::to_string(&TaskState::Submitted).expect("serialize");
        assert_eq!(json, "\"submitted\"");

        let json = serde_json::to_string(&TaskState::Unknown).expect("serialize");
        assert_eq!(json, "\"unknown\"");
    }

    #[test]
    fn task_round_trip() {
        let task = Task {
            kind: "task".to_string(),
            id: "task-001".to_string(),
            context_id: "ctx-abc".to_string(),
            status: TaskStatus {
                state: TaskState::Completed,
                timestamp: Some("2026-03-17T10:30:00Z".to_string()),
                message: Some(Message {
                    kind: "message".to_string(),
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
            metadata: None,
        };

        let json = serde_json::to_string_pretty(&task).expect("serialize");
        let parsed: Task = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(task, parsed);
    }

    #[test]
    fn minimal_task_round_trip() {
        let task = Task {
            kind: "task".to_string(),
            id: "task-002".to_string(),
            context_id: "ctx-default".to_string(),
            status: TaskStatus {
                state: TaskState::Submitted,
                timestamp: Some("2026-03-17T10:00:00Z".to_string()),
                message: None,
            },
            artifacts: Vec::new(),
            history: Vec::new(),
            metadata: None,
        };

        let json = serde_json::to_string(&task).expect("serialize");
        let parsed: Task = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(task, parsed);
        // Verify empty vecs are omitted.
        assert!(!json.contains("artifacts"));
        assert!(!json.contains("history"));
    }
}
