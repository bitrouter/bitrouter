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

// ── Resource types ──────────────────────────────────────────────────

/// An MCP resource exposed to downstream clients.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpResource {
    /// Namespaced URI, e.g. `"github+file:///readme.md"`.
    pub uri: String,
    /// Human-readable name.
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
}

/// An MCP resource template exposed to downstream clients.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpResourceTemplate {
    /// Namespaced URI template, e.g. `"github+file:///{path}"`.
    pub uri_template: String,
    /// Human-readable name.
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
}

/// Content of a resource read response.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpResourceContent {
    pub uri: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blob: Option<String>,
}

// ── Prompt types ────────────────────────────────────────────────────

/// An MCP prompt exposed to downstream clients.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpPrompt {
    /// Namespaced prompt name, e.g. `"github/summarize"`.
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub arguments: Vec<McpPromptArgument>,
}

/// An argument that a prompt accepts.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpPromptArgument {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub required: Option<bool>,
}

/// A message returned as part of a prompt.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpPromptMessage {
    pub role: McpRole,
    pub content: McpPromptContent,
}

/// Role of a prompt message sender.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum McpRole {
    User,
    Assistant,
}

/// Content of a prompt message.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum McpPromptContent {
    Text { text: String },
    Resource { resource: McpResourceContent },
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

    #[test]
    fn resource_round_trip() {
        let resource = McpResource {
            uri: "github+file:///readme.md".to_string(),
            name: "readme".to_string(),
            description: Some("Project readme".to_string()),
            mime_type: Some("text/markdown".to_string()),
        };
        let json = serde_json::to_string(&resource).expect("serialize");
        let parsed: McpResource = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed.uri, "github+file:///readme.md");
        assert!(json.contains("mimeType"));
    }

    #[test]
    fn resource_content_text_round_trip() {
        let content = McpResourceContent {
            uri: "file:///test.txt".to_string(),
            mime_type: Some("text/plain".to_string()),
            text: Some("hello".to_string()),
            blob: None,
        };
        let json = serde_json::to_string(&content).expect("serialize");
        let parsed: McpResourceContent = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed.text.as_deref(), Some("hello"));
        assert!(!json.contains("blob"));
    }

    #[test]
    fn resource_template_round_trip() {
        let tmpl = McpResourceTemplate {
            uri_template: "github+file:///{path}".to_string(),
            name: "files".to_string(),
            description: None,
            mime_type: None,
        };
        let json = serde_json::to_string(&tmpl).expect("serialize");
        let parsed: McpResourceTemplate = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed.uri_template, "github+file:///{path}");
        assert!(!json.contains("description"));
    }

    #[test]
    fn prompt_round_trip() {
        let prompt = McpPrompt {
            name: "github/summarize".to_string(),
            description: Some("Summarize a PR".to_string()),
            arguments: vec![McpPromptArgument {
                name: "pr_url".to_string(),
                description: Some("The PR URL".to_string()),
                required: Some(true),
            }],
        };
        let json = serde_json::to_string(&prompt).expect("serialize");
        let parsed: McpPrompt = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed.name, "github/summarize");
        assert_eq!(parsed.arguments.len(), 1);
    }

    #[test]
    fn prompt_message_text_round_trip() {
        let msg = McpPromptMessage {
            role: McpRole::User,
            content: McpPromptContent::Text {
                text: "hello".to_string(),
            },
        };
        let json = serde_json::to_string(&msg).expect("serialize");
        let parsed: McpPromptMessage = serde_json::from_str(&json).expect("deserialize");
        assert!(matches!(parsed.content, McpPromptContent::Text { .. }));
    }
}
