//! MCP protocol request parameters and response types.
//!
//! Covers the `initialize`, `tools/list`, and `tools/call` methods.

use serde::{Deserialize, Serialize};

use super::types::{McpTool, McpToolCallResult};

// ── Initialize ───────────────────────────────────────────────────────

/// Parameters for the `initialize` request from the client.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeParams {
    pub protocol_version: String,
    pub capabilities: ClientCapabilities,
    pub client_info: ClientInfo,
}

/// Client capabilities declared during initialization.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ClientCapabilities {}

/// Client identity sent during initialization.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientInfo {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

/// Response to the `initialize` request.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeResult {
    pub protocol_version: String,
    pub capabilities: ServerCapabilities,
    pub server_info: ServerInfo,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
}

/// Server capabilities advertised during initialization.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerCapabilities {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<ToolsCapability>,
}

/// Capability flags for the `tools` feature.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolsCapability {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub list_changed: Option<bool>,
}

/// Server identity returned during initialization.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerInfo {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

// ── tools/list ───────────────────────────────────────────────────────

/// Parameters for the `tools/list` request.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ListToolsParams {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
}

/// Response to the `tools/list` request.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListToolsResult {
    pub tools: Vec<McpTool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

// ── tools/call ───────────────────────────────────────────────────────

/// Parameters for the `tools/call` request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CallToolParams {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arguments: Option<serde_json::Map<String, serde_json::Value>>,
}

/// Re-export the call result type for convenience.
pub type CallToolResult = McpToolCallResult;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initialize_params_round_trip() {
        let params = InitializeParams {
            protocol_version: "2025-03-26".to_string(),
            capabilities: ClientCapabilities {},
            client_info: ClientInfo {
                name: "test-client".to_string(),
                version: Some("1.0".to_string()),
            },
        };
        let json = serde_json::to_string(&params).expect("serialize");
        assert!(json.contains("protocolVersion"));
        assert!(json.contains("clientInfo"));
        let parsed: InitializeParams = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed.protocol_version, "2025-03-26");
    }

    #[test]
    fn initialize_result_round_trip() {
        let result = InitializeResult {
            protocol_version: "2025-03-26".to_string(),
            capabilities: ServerCapabilities {
                tools: Some(ToolsCapability {
                    list_changed: Some(true),
                }),
            },
            server_info: ServerInfo {
                name: "bitrouter".to_string(),
                version: Some("0.10.0".to_string()),
            },
            instructions: Some("BitRouter MCP Gateway".to_string()),
        };
        let json = serde_json::to_string(&result).expect("serialize");
        assert!(json.contains("protocolVersion"));
        assert!(json.contains("serverInfo"));
        assert!(json.contains("listChanged"));
        let parsed: InitializeResult = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed.server_info.name, "bitrouter");
    }

    #[test]
    fn list_tools_result_round_trip() {
        let result = ListToolsResult {
            tools: vec![],
            next_cursor: None,
        };
        let json = serde_json::to_string(&result).expect("serialize");
        let parsed: ListToolsResult = serde_json::from_str(&json).expect("deserialize");
        assert!(parsed.tools.is_empty());
        assert!(!json.contains("nextCursor"));
    }

    #[test]
    fn call_tool_params_round_trip() {
        let params = CallToolParams {
            name: "github/search".to_string(),
            arguments: Some(serde_json::Map::from_iter([(
                "query".to_string(),
                serde_json::Value::String("rust".to_string()),
            )])),
        };
        let json = serde_json::to_string(&params).expect("serialize");
        let parsed: CallToolParams = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed.name, "github/search");
        assert!(parsed.arguments.is_some());
    }

    #[test]
    fn call_tool_params_no_arguments() {
        let json = r#"{"name":"test/tool"}"#;
        let parsed: CallToolParams = serde_json::from_str(json).expect("deserialize");
        assert_eq!(parsed.name, "test/tool");
        assert!(parsed.arguments.is_none());
    }
}
