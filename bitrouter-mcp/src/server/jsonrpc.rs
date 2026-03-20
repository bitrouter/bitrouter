//! JSON-RPC 2.0 envelope types for the MCP server protocol.
//!
//! MCP uses JSON-RPC 2.0 with two inbound message shapes:
//! - **Requests** — have an `id` field, expect a response.
//! - **Notifications** — no `id` field, fire-and-forget.
//!
//! [`JsonRpcMessage`] discriminates between them using a custom
//! deserializer that checks for the presence of `id`.

use serde::{Deserialize, Deserializer, Serialize};

/// A JSON-RPC 2.0 request ID — may be a number or a string.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum JsonRpcId {
    Number(i64),
    Str(String),
}

/// A JSON-RPC 2.0 request (has `id`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcRequest {
    pub jsonrpc: String,
    pub id: JsonRpcId,
    pub method: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<serde_json::Value>,
}

/// A JSON-RPC 2.0 notification (no `id`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcNotification {
    pub jsonrpc: String,
    pub method: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<serde_json::Value>,
}

/// A JSON-RPC 2.0 response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcResponse {
    pub jsonrpc: String,
    pub id: JsonRpcId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
}

/// A JSON-RPC 2.0 error object.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcError {
    pub code: i64,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

/// An inbound JSON-RPC 2.0 message — either a [`JsonRpcRequest`] or a
/// [`JsonRpcNotification`].
///
/// Discrimination is based on presence of `id` using a custom
/// deserializer rather than `#[serde(untagged)]` for better error
/// messages.
#[derive(Debug, Clone)]
pub enum JsonRpcMessage {
    Request(JsonRpcRequest),
    Notification(JsonRpcNotification),
}

impl<'de> Deserialize<'de> for JsonRpcMessage {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw: serde_json::Value = serde_json::Value::deserialize(deserializer)?;

        let Some(obj) = raw.as_object() else {
            return Err(serde::de::Error::custom("expected a JSON object"));
        };

        if obj.contains_key("id") {
            let req: JsonRpcRequest =
                serde_json::from_value(raw).map_err(serde::de::Error::custom)?;
            Ok(JsonRpcMessage::Request(req))
        } else {
            let notif: JsonRpcNotification =
                serde_json::from_value(raw).map_err(serde::de::Error::custom)?;
            Ok(JsonRpcMessage::Notification(notif))
        }
    }
}

impl JsonRpcResponse {
    /// Build a success response.
    pub fn success(id: JsonRpcId, result: serde_json::Value) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            id,
            result: Some(result),
            error: None,
        }
    }

    /// Build an error response.
    pub fn error(
        id: JsonRpcId,
        code: i64,
        message: String,
        data: Option<serde_json::Value>,
    ) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            id,
            result: None,
            error: Some(JsonRpcError {
                code,
                message,
                data,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_round_trip() {
        let req = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: JsonRpcId::Number(1),
            method: "tools/list".to_string(),
            params: None,
        };
        let json = serde_json::to_string(&req).expect("serialize");
        let parsed: JsonRpcRequest = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed.id, JsonRpcId::Number(1));
        assert_eq!(parsed.method, "tools/list");
    }

    #[test]
    fn notification_round_trip() {
        let notif = JsonRpcNotification {
            jsonrpc: "2.0".to_string(),
            method: "notifications/initialized".to_string(),
            params: None,
        };
        let json = serde_json::to_string(&notif).expect("serialize");
        let parsed: JsonRpcNotification = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed.method, "notifications/initialized");
    }

    #[test]
    fn message_discriminates_request() {
        let json = r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#;
        let msg: JsonRpcMessage = serde_json::from_str(json).expect("parse");
        assert!(matches!(msg, JsonRpcMessage::Request(_)));
    }

    #[test]
    fn message_discriminates_notification() {
        let json = r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#;
        let msg: JsonRpcMessage = serde_json::from_str(json).expect("parse");
        assert!(matches!(msg, JsonRpcMessage::Notification(_)));
    }

    #[test]
    fn message_rejects_non_object() {
        let json = r#""hello""#;
        let result: Result<JsonRpcMessage, _> = serde_json::from_str(json);
        assert!(result.is_err());
    }

    #[test]
    fn string_id_round_trip() {
        let req = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: JsonRpcId::Str("abc-123".to_string()),
            method: "tools/call".to_string(),
            params: Some(serde_json::json!({"name": "test"})),
        };
        let json = serde_json::to_string(&req).expect("serialize");
        let parsed: JsonRpcRequest = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed.id, JsonRpcId::Str("abc-123".to_string()));
    }

    #[test]
    fn response_success_omits_error() {
        let resp = JsonRpcResponse::success(JsonRpcId::Number(1), serde_json::json!({"tools": []}));
        let json = serde_json::to_string(&resp).expect("serialize");
        assert!(!json.contains("error"));
    }

    #[test]
    fn response_error_omits_result() {
        let resp = JsonRpcResponse::error(
            JsonRpcId::Number(1),
            -32601,
            "method not found".to_string(),
            None,
        );
        let json = serde_json::to_string(&resp).expect("serialize");
        assert!(!json.contains("\"result\""));
        assert!(json.contains("-32601"));
    }
}
