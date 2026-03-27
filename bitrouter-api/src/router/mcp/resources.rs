//! JSON-RPC handlers for the `resources/*` MCP methods.

use super::tools::gateway_error_to_jsonrpc;
use super::types::{
    JsonRpcId, JsonRpcResponse, ListResourceTemplatesResult, ListResourcesResult,
    McpResourceServer, ReadResourceParams, ReadResourceResult, error_codes,
};
use bitrouter_core::api::mcp::types::{ListResourceTemplatesParams, ListResourcesParams};

pub async fn handle_resources_list<T: McpResourceServer>(
    id: &JsonRpcId,
    params: Option<serde_json::Value>,
    server: &T,
) -> JsonRpcResponse {
    let cursor = params
        .and_then(|v| serde_json::from_value::<ListResourcesParams>(v).ok())
        .and_then(|p| p.cursor);
    let (resources, next_cursor) = server.list_resources(cursor.as_deref()).await;
    let result = ListResourcesResult {
        resources,
        next_cursor,
    };
    let value = serde_json::to_value(&result).unwrap_or_default();
    JsonRpcResponse::success(id.clone(), value)
}

pub async fn handle_resources_read<T: McpResourceServer>(
    id: &JsonRpcId,
    params: Option<serde_json::Value>,
    server: &T,
) -> JsonRpcResponse {
    let Some(params_value) = params else {
        return JsonRpcResponse::error(
            id.clone(),
            error_codes::INVALID_PARAMS,
            "resources/read requires params".to_string(),
            None,
        );
    };

    let read_params: ReadResourceParams = match serde_json::from_value(params_value) {
        Ok(p) => p,
        Err(e) => {
            return JsonRpcResponse::error(
                id.clone(),
                error_codes::INVALID_PARAMS,
                format!("invalid params: {e}"),
                None,
            );
        }
    };

    match server.read_resource(&read_params.uri).await {
        Ok(contents) => {
            let result = ReadResourceResult { contents };
            let value = serde_json::to_value(&result).unwrap_or_default();
            JsonRpcResponse::success(id.clone(), value)
        }
        Err(err) => {
            let (code, message) = gateway_error_to_jsonrpc(&err);
            JsonRpcResponse::error(id.clone(), code, message, None)
        }
    }
}

pub async fn handle_resource_templates_list<T: McpResourceServer>(
    id: &JsonRpcId,
    params: Option<serde_json::Value>,
    server: &T,
) -> JsonRpcResponse {
    let cursor = params
        .and_then(|v| serde_json::from_value::<ListResourceTemplatesParams>(v).ok())
        .and_then(|p| p.cursor);
    let (resource_templates, next_cursor) = server.list_resource_templates(cursor.as_deref()).await;
    let result = ListResourceTemplatesResult {
        resource_templates,
        next_cursor,
    };
    let value = serde_json::to_value(&result).unwrap_or_default();
    JsonRpcResponse::success(id.clone(), value)
}
