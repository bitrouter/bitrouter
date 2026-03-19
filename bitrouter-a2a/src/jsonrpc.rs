//! JSON-RPC 2.0 types for the A2A protocol wire format.
//!
//! A2A uses JSON-RPC 2.0 over HTTP(S) as its transport. All A2A methods
//! (`SendMessage`, `GetTask`, `CancelTask`) are encoded as JSON-RPC
//! requests POSTed to the agent's endpoint.

use serde::{Deserialize, Serialize};

/// A JSON-RPC 2.0 request.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct JsonRpcRequest {
    /// Protocol version — always `"2.0"`.
    pub jsonrpc: String,

    /// Request identifier.
    pub id: String,

    /// Method name (e.g., `"SendMessage"`, `"GetTask"`).
    pub method: String,

    /// Method parameters.
    pub params: serde_json::Value,
}

/// A JSON-RPC 2.0 response.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct JsonRpcResponse {
    /// Protocol version — always `"2.0"`.
    pub jsonrpc: String,

    /// Request identifier (matches the request).
    pub id: String,

    /// Successful result (mutually exclusive with `error`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,

    /// Error result (mutually exclusive with `result`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
}

/// A JSON-RPC 2.0 error object.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct JsonRpcError {
    /// Error code.
    pub code: i64,
    /// Human-readable error message.
    pub message: String,
    /// Additional error data.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

impl JsonRpcRequest {
    /// Create a new JSON-RPC 2.0 request.
    pub fn new(id: &str, method: &str, params: serde_json::Value) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            id: id.to_string(),
            method: method.to_string(),
            params,
        }
    }
}

impl JsonRpcResponse {
    /// Returns the result value, or an error if the response is an error.
    pub fn into_result(self) -> Result<serde_json::Value, JsonRpcError> {
        if let Some(err) = self.error {
            return Err(err);
        }
        self.result.ok_or_else(|| JsonRpcError {
            code: -32603,
            message: "response contains neither result nor error".to_string(),
            data: None,
        })
    }
}

impl std::fmt::Display for JsonRpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "JSON-RPC error {}: {}", self.code, self.message)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_round_trip() {
        let req = JsonRpcRequest::new(
            "req-1",
            "SendMessage",
            serde_json::json!({
                "message": {
                    "role": "ROLE_USER",
                    "parts": [{"text": "hello"}],
                    "messageId": "msg-1"
                }
            }),
        );

        let json = serde_json::to_string(&req).expect("serialize");
        let parsed: JsonRpcRequest = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(req, parsed);
        assert_eq!(parsed.jsonrpc, "2.0");
    }

    #[test]
    fn success_response_into_result() {
        let resp = JsonRpcResponse {
            jsonrpc: "2.0".to_string(),
            id: "req-1".to_string(),
            result: Some(serde_json::json!({"id": "task-1"})),
            error: None,
        };
        let val = resp.into_result().expect("should succeed");
        assert_eq!(val["id"], "task-1");
    }

    #[test]
    fn error_response_into_result() {
        let resp = JsonRpcResponse {
            jsonrpc: "2.0".to_string(),
            id: "req-1".to_string(),
            result: None,
            error: Some(JsonRpcError {
                code: -32600,
                message: "Invalid Request".to_string(),
                data: None,
            }),
        };
        let err = resp.into_result().expect_err("should be error");
        assert_eq!(err.code, -32600);
    }

    #[test]
    fn empty_response_into_result() {
        let resp = JsonRpcResponse {
            jsonrpc: "2.0".to_string(),
            id: "req-1".to_string(),
            result: None,
            error: None,
        };
        let err = resp.into_result().expect_err("should be error");
        assert_eq!(err.code, -32603);
    }
}
