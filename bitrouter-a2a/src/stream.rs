//! A2A v1.0 streaming response types.
//!
//! Defines the event types for `SendStreamingMessage` and `SubscribeToTask`
//! SSE streams per the A2A v1.0 specification.

use serde::{Deserialize, Serialize};

use crate::message::{Artifact, Message};
use crate::task::{Task, TaskStatus};

/// A streaming response event from the server.
///
/// Serializes as `{"message": {...}}`, `{"task": {...}}`,
/// `{"statusUpdate": {...}}`, or `{"artifactUpdate": {...}}`,
/// matching the A2A v1.0 `StreamResponse` wire format.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum StreamResponse {
    /// Complete task snapshot.
    Task(Task),
    /// Direct message response.
    Message(Message),
    /// Task status change notification.
    StatusUpdate(TaskStatusUpdateEvent),
    /// Artifact data chunk or complete artifact.
    ArtifactUpdate(TaskArtifactUpdateEvent),
}

/// Notification of a task status change during streaming.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskStatusUpdateEvent {
    /// Task ID this event pertains to.
    pub task_id: String,

    /// Context ID for the task.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_id: Option<String>,

    /// New task status.
    pub status: TaskStatus,

    /// Extension metadata.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
}

/// Notification of an artifact update during streaming.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskArtifactUpdateEvent {
    /// Task ID this event pertains to.
    pub task_id: String,

    /// Context ID for the task.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_id: Option<String>,

    /// The artifact being produced or updated.
    pub artifact: Artifact,

    /// Whether this chunk should be appended to a previous artifact.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub append: Option<bool>,

    /// Whether this is the final chunk for this artifact.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_chunk: Option<bool>,

    /// Extension metadata.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task::{TaskState, TaskStatus};

    #[test]
    fn status_update_event_round_trip() {
        let event = TaskStatusUpdateEvent {
            task_id: "task-1".to_string(),
            context_id: Some("ctx-1".to_string()),
            status: TaskStatus {
                state: TaskState::Working,
                timestamp: "2026-03-19T00:00:00Z".to_string(),
                message: None,
            },
            metadata: None,
        };

        let json = serde_json::to_string(&event).expect("serialize");
        let parsed: TaskStatusUpdateEvent = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed.task_id, "task-1");
    }

    #[test]
    fn stream_response_tagged_serialization() {
        let event = StreamResponse::StatusUpdate(TaskStatusUpdateEvent {
            task_id: "t-1".to_string(),
            context_id: None,
            status: TaskStatus {
                state: TaskState::Completed,
                timestamp: "2026-03-19T00:00:00Z".to_string(),
                message: None,
            },
            metadata: None,
        });

        let json = serde_json::to_string(&event).expect("serialize");
        // Externally tagged: {"statusUpdate": {...}}
        assert!(json.contains("\"statusUpdate\""));
    }
}
