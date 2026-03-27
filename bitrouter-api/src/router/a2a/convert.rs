//! JSON-RPC conversion helpers for A2A gateway responses.

use bitrouter_core::api::a2a::error::A2aGatewayError;
use bitrouter_core::api::a2a::types::JsonRpcResponse;

/// Deserialize JSON-RPC params into a typed request.
pub(crate) fn deserialize_params<T: serde::de::DeserializeOwned>(
    params: &serde_json::Value,
) -> Result<T, Box<JsonRpcResponse>> {
    serde_json::from_value(params.clone()).map_err(|e| {
        Box::new(JsonRpcResponse {
            jsonrpc: "2.0".to_string(),
            id: String::new(),
            result: None,
            error: Some(bitrouter_core::api::a2a::types::JsonRpcError {
                code: -32602,
                message: format!("invalid params: {e}"),
                data: None,
            }),
        })
    })
}

/// Build a successful JSON-RPC response.
pub(crate) fn success_response(id: &str, result: &impl serde::Serialize) -> JsonRpcResponse {
    JsonRpcResponse {
        jsonrpc: "2.0".to_string(),
        id: id.to_string(),
        result: serde_json::to_value(result).ok(),
        error: None,
    }
}

/// Build an error JSON-RPC response.
pub(crate) fn error_response(id: &str, code: i64, message: String) -> JsonRpcResponse {
    JsonRpcResponse {
        jsonrpc: "2.0".to_string(),
        id: id.to_string(),
        result: None,
        error: Some(bitrouter_core::api::a2a::types::JsonRpcError {
            code,
            message,
            data: None,
        }),
    }
}

/// Map an [`A2aGatewayError`] to a JSON-RPC error response.
pub(crate) fn gateway_error_response(id: &str, err: &A2aGatewayError) -> JsonRpcResponse {
    let code = match err {
        A2aGatewayError::AgentNotFound { .. } => -32001,
        A2aGatewayError::InvalidConfig { .. } => -32602,
        A2aGatewayError::UpstreamCall { .. }
        | A2aGatewayError::UpstreamConnect { .. }
        | A2aGatewayError::UpstreamClosed { .. } => -32000,
        A2aGatewayError::Client(_) => -32603,
    };
    error_response(id, code, err.to_string())
}

/// Set the id field on a JsonRpcResponse.
pub(crate) trait WithId {
    fn with_id(self, id: &str) -> Self;
}

impl WithId for JsonRpcResponse {
    fn with_id(mut self, id: &str) -> Self {
        self.id = id.to_string();
        self
    }
}
