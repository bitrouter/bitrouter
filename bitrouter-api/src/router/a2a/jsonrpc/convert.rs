//! JSON-RPC conversion helpers: response builders, ID generation, and helper traits.

use std::sync::atomic::{AtomicU64, Ordering};

use bitrouter_a2a::error::A2aError;
use bitrouter_a2a::jsonrpc::{JsonRpcError, JsonRpcResponse};
use bitrouter_a2a::registry::AgentCardRegistry;

pub(crate) fn deserialize_params<T: serde::de::DeserializeOwned>(
    params: &serde_json::Value,
) -> Result<T, Box<JsonRpcResponse>> {
    serde_json::from_value::<T>(params.clone())
        .map_err(|e| Box::new(error_response("", -32602, format!("invalid params: {e}"))))
}

pub(crate) fn success_response<T: serde::Serialize>(id: &str, result: &T) -> JsonRpcResponse {
    let value = serde_json::to_value(result).unwrap_or(serde_json::Value::Null);
    JsonRpcResponse {
        jsonrpc: "2.0".to_string(),
        id: id.to_string(),
        result: Some(value),
        error: None,
    }
}

pub(crate) fn error_response(id: &str, code: i64, message: String) -> JsonRpcResponse {
    JsonRpcResponse {
        jsonrpc: "2.0".to_string(),
        id: id.to_string(),
        result: None,
        error: Some(JsonRpcError {
            code,
            message,
            data: None,
        }),
    }
}

pub(crate) fn execution_error_response(id: &str, err: &A2aError) -> JsonRpcResponse {
    let code = match err {
        A2aError::TaskNotFound { .. } => -32001,
        A2aError::Execution(_) => -32000,
        _ => -32603,
    };
    error_response(id, code, err.to_string())
}

pub(crate) fn generate_id(prefix: &str) -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    format!("{prefix}-{}", COUNTER.fetch_add(1, Ordering::Relaxed))
}

pub(crate) fn generate_streaming_id(prefix: &str) -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    format!("{prefix}-s-{}", COUNTER.fetch_add(1, Ordering::Relaxed))
}

/// Helper to set the id on an error response built without one.
pub(crate) trait WithId {
    fn with_id(self, id: &str) -> Self;
}

impl WithId for JsonRpcResponse {
    fn with_id(mut self, id: &str) -> Self {
        self.id = id.to_string();
        self
    }
}

/// Helper trait for `GetExtendedAgentCard` — finds first registered agent.
pub(crate) trait GetExtendedByFirst {
    fn get_extended_by_first(
        &self,
        registry: &dyn AgentCardRegistry,
    ) -> Result<Option<bitrouter_a2a::registry::AgentRegistration>, A2aError>;
}

impl<R: AgentCardRegistry> GetExtendedByFirst for R {
    fn get_extended_by_first(
        &self,
        _registry: &dyn AgentCardRegistry,
    ) -> Result<Option<bitrouter_a2a::registry::AgentRegistration>, A2aError> {
        let list = self.list()?;
        if let Some(first) = list.into_iter().next() {
            self.get_extended(&first.card.name)
        } else {
            Ok(None)
        }
    }
}
