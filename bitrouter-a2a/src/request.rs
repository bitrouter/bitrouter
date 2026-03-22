//! Typed request structs for A2A v0.3.0 JSON-RPC methods.

use serde::{Deserialize, Serialize};

use crate::message::Message;

/// Request parameters for `message/send` / `message/stream`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SendMessageRequest {
    /// The user message to send.
    pub message: Message,

    /// Client-side configuration for the request.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub configuration: Option<SendMessageConfiguration>,

    /// Extension metadata.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
}

/// Client configuration for a `message/send` request.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SendMessageConfiguration {
    /// Accepted output media types.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub accepted_output_modes: Option<Vec<String>>,

    /// Push notification configuration for async updates.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub push_notification_config: Option<PushNotificationConfig>,

    /// Maximum number of history messages to include in the response.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub history_length: Option<u32>,

    /// Whether the call should block until completion.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blocking: Option<bool>,
}

/// Request parameters for `tasks/cancel`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CancelTaskRequest {
    /// Task ID to cancel.
    pub id: String,
}

/// Request parameters for `tasks/resubscribe`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SubscribeToTaskRequest {
    /// Task ID to subscribe to.
    pub task_id: String,
}

/// Push notification configuration associated with a task.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct TaskPushNotificationConfig {
    /// Task ID this config applies to.
    pub task_id: String,

    /// The push notification configuration.
    pub push_notification_config: PushNotificationConfig,
}

/// Push notification endpoint configuration.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PushNotificationConfig {
    /// Config ID (generated if not provided).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,

    /// Webhook URL to receive push notifications.
    pub url: String,

    /// Bearer token for the webhook.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,

    /// Authentication info for the webhook.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub authentication: Option<PushNotificationAuthenticationInfo>,
}

/// Authentication credentials for push notification webhooks.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PushNotificationAuthenticationInfo {
    /// Supported authentication schemes.
    pub schemes: Vec<String>,

    /// Credentials value.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub credentials: Option<String>,
}

/// Request parameters for `tasks/pushNotificationConfig/get`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct GetTaskPushNotificationConfigRequest {
    /// Task ID.
    pub id: String,

    /// Optional push notification config ID to get a specific config.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub push_notification_config_id: Option<String>,
}

/// Request parameters for `tasks/pushNotificationConfig/list`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ListTaskPushNotificationConfigsRequest {
    /// Task ID.
    pub id: String,
}

/// Request parameters for `tasks/pushNotificationConfig/delete`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct DeleteTaskPushNotificationConfigRequest {
    /// Task ID.
    pub id: String,

    /// Push notification config ID to delete.
    pub push_notification_config_id: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::{MessageRole, Part};

    #[test]
    fn send_message_request_round_trip() {
        let req = SendMessageRequest {
            message: Message {
                kind: "message".to_string(),
                role: MessageRole::User,
                parts: vec![Part::text("hello")],
                message_id: "msg-1".to_string(),
                context_id: None,
                task_id: None,
                reference_task_ids: Vec::new(),
                metadata: None,
            },
            configuration: Some(SendMessageConfiguration {
                accepted_output_modes: Some(vec!["text/plain".to_string()]),
                push_notification_config: None,
                history_length: Some(5),
                blocking: None,
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
        };

        let json = serde_json::to_string(&req).expect("serialize");
        let parsed: CancelTaskRequest = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(req, parsed);
    }

    #[test]
    fn push_notification_config_round_trip() {
        let config = TaskPushNotificationConfig {
            task_id: "task-1".to_string(),
            push_notification_config: PushNotificationConfig {
                id: Some("cfg-1".to_string()),
                url: "https://example.com/webhook".to_string(),
                token: None,
                authentication: Some(PushNotificationAuthenticationInfo {
                    schemes: vec!["Bearer".to_string()],
                    credentials: Some("tok-123".to_string()),
                }),
            },
        };

        let json = serde_json::to_string(&config).expect("serialize");
        let parsed: TaskPushNotificationConfig = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(config, parsed);
    }
}
