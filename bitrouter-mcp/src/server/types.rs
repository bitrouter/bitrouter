//! MCP tool definition and call result types.
//!
//! These mirror the shapes used by `rmcp::model::Tool` and
//! `rmcp::model::CallToolResult` but as pure serde types, so
//! `bitrouter-mcp` stays free of the `rmcp` dependency.

use serde::{Deserialize, Serialize};

/// An MCP tool definition exposed to downstream clients.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpTool {
    /// Namespaced tool name, e.g. `"github/search"`.
    pub name: String,
    /// Human-readable description.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// JSON Schema describing the tool's input parameters.
    pub input_schema: serde_json::Value,
}

/// The result of invoking an MCP tool.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpToolCallResult {
    pub content: Vec<McpContent>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_error: Option<bool>,
}

/// A single content block in a tool call result.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum McpContent {
    Text { text: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_round_trip() {
        let tool = McpTool {
            name: "github/search".to_string(),
            description: Some("Search GitHub".to_string()),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string"}
                }
            }),
        };
        let json = serde_json::to_string(&tool).expect("serialize");
        let parsed: McpTool = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed.name, "github/search");
        assert!(json.contains("inputSchema"));
    }

    #[test]
    fn tool_omits_none_description() {
        let tool = McpTool {
            name: "test".to_string(),
            description: None,
            input_schema: serde_json::json!({}),
        };
        let json = serde_json::to_string(&tool).expect("serialize");
        assert!(!json.contains("description"));
    }

    #[test]
    fn call_result_round_trip() {
        let result = McpToolCallResult {
            content: vec![McpContent::Text {
                text: "hello".to_string(),
            }],
            is_error: None,
        };
        let json = serde_json::to_string(&result).expect("serialize");
        let parsed: McpToolCallResult = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed.content.len(), 1);
        assert!(!json.contains("isError"));
    }

    #[test]
    fn call_result_with_error() {
        let result = McpToolCallResult {
            content: vec![McpContent::Text {
                text: "something went wrong".to_string(),
            }],
            is_error: Some(true),
        };
        let json = serde_json::to_string(&result).expect("serialize");
        assert!(json.contains("isError"));
        assert!(json.contains("true"));
    }

    #[test]
    fn content_text_variant_tagged() {
        let content = McpContent::Text {
            text: "hello".to_string(),
        };
        let json = serde_json::to_string(&content).expect("serialize");
        assert!(json.contains(r#""type":"text""#));
    }
}
