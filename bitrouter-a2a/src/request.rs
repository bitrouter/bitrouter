//! Typed request structs for A2A v1.0 JSON-RPC methods.

use serde::{Deserialize, Serialize};

use crate::message::Message;

/// Request parameters for `SendMessage` / `SendStreamingMessage`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SendMessageRequest {
    /// Tenant scope.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tenant: Option<String>,

    /// The user message to send.
    pub message: Message,

    /// Client-side configuration for the request.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub configuration: Option<SendMessageConfiguration>,

    /// Extension metadata.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
}

/// Client configuration for a `SendMessage` request.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SendMessageConfiguration {
    /// Accepted output media types.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub accepted_output_modes: Option<Vec<String>>,

    /// Push notification configuration for async updates.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_push_notification_config: Option<TaskPushNotificationConfig>,

    /// Maximum number of history messages to include in the response.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub history_length: Option<u32>,

    /// Return immediately without waiting for completion.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub return_immediately: Option<bool>,
}

/// Request parameters for `CancelTask`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CancelTaskRequest {
    /// Task ID to cancel.
    pub id: String,

    /// Tenant scope.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tenant: Option<String>,
}

/// Request parameters for `SubscribeToTask`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SubscribeToTaskRequest {
    /// Task ID to subscribe to.
    pub task_id: String,

    /// Tenant scope.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tenant: Option<String>,
}

/// Push notification configuration for a task.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct TaskPushNotificationConfig {
    /// Tenant scope.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tenant: Option<String>,

    /// Config ID (generated if not provided).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,

    /// Task ID this config applies to.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,

    /// Webhook URL to receive push notifications.
    pub url: String,

    /// Bearer token for the webhook.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,

    /// Authentication info for the webhook.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub authentication: Option<AuthenticationInfo>,
}

/// Authentication credentials for push notification webhooks.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AuthenticationInfo {
    /// Authentication scheme (e.g., "Bearer").
    pub scheme: String,

    /// Credentials value.
    pub credentials: String,
}

/// Request parameters for `GetTaskPushNotificationConfig`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct GetTaskPushNotificationConfigRequest {
    /// Tenant scope.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tenant: Option<String>,

    /// Config ID.
    pub id: String,

    /// Task ID.
    pub task_id: String,
}

/// Request parameters for `ListTaskPushNotificationConfigs`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ListTaskPushNotificationConfigsRequest {
    /// Tenant scope.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tenant: Option<String>,

    /// Task ID.
    pub task_id: String,
}

/// Request parameters for `DeleteTaskPushNotificationConfig`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct DeleteTaskPushNotificationConfigRequest {
    /// Tenant scope.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tenant: Option<String>,

    /// Config ID.
    pub id: String,

    /// Task ID.
    pub task_id: String,
}

/// Response for `ListTaskPushNotificationConfigs`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ListTaskPushNotificationConfigsResponse {
    /// Push notification configurations for the task.
    pub configs: Vec<TaskPushNotificationConfig>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::{MessageRole, Part};

    #[test]
    fn send_message_request_round_trip() {
        let req = SendMessageRequest {
            tenant: None,
            message: Message {
                role: MessageRole::User,
                parts: vec![Part::text("hello")],
                message_id: "msg-1".to_string(),
                context_id: None,
                task_id: None,
                reference_task_ids: Vec::new(),
                metadata: None,
                extensions: Vec::new(),
            },
            configuration: Some(SendMessageConfiguration {
                accepted_output_modes: Some(vec!["text/plain".to_string()]),
                task_push_notification_config: None,
                history_length: Some(5),
                return_immediately: None,
            }),
            metadata: None,
        };

        let json = serde_json::to_string(&req).expect("serialize");
        let parsed: SendMessageRequest = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(req, parsed);
    }

    #[test]
    fn cancel_task_request_round_trip() {
        let req = CancelTaskRequest {
            id: "task-1".to_string(),
            tenant: Some("t1".to_string()),
        };

        let json = serde_json::to_string(&req).expect("serialize");
        let parsed: CancelTaskRequest = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(req, parsed);
    }

    #[test]
    fn push_notification_config_round_trip() {
        let config = TaskPushNotificationConfig {
            tenant: None,
            id: Some("cfg-1".to_string()),
            task_id: Some("task-1".to_string()),
            url: "https://example.com/webhook".to_string(),
            token: None,
            authentication: Some(AuthenticationInfo {
                scheme: "Bearer".to_string(),
                credentials: "tok-123".to_string(),
            }),
        };

        let json = serde_json::to_string(&config).expect("serialize");
        let parsed: TaskPushNotificationConfig = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(config, parsed);
    }
}
