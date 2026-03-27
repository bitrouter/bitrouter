//! MCP protocol types — tool, resource, prompt definitions, JSON-RPC
//! envelope types, protocol messages, and error codes.
//!
//! These are `rmcp`-free pure serde structs that match the MCP wire format,
//! allowing `bitrouter-api` to serve the protocol without depending on `rmcp`.

use serde::{Deserialize, Deserializer, Serialize};

// ── Tool types ─────────────────────────────────────────────────────

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
    Text {
        text: String,
    },
    Image {
        data: String,
        #[serde(rename = "mimeType")]
        mime_type: String,
    },
    Resource {
        resource: McpResourceContent,
    },
}

// ── Resource types ─────────────────────────────────────────────────

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

// ── Prompt types ───────────────────────────────────────────────────

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

// ── Core conversion ────────────────────────────────────────────────

impl From<McpTool> for crate::routers::registry::ToolEntry {
    fn from(t: McpTool) -> Self {
        let provider = t
            .name
            .split_once('/')
            .map(|(s, _)| s)
            .unwrap_or("unknown")
            .to_owned();
        Self {
            id: t.name,
            name: None,
            provider,
            description: t.description,
            input_schema: Some(t.input_schema),
        }
    }
}

// ── Initialize ─────────────────────────────────────────────────────

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
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ServerCapabilities {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<ToolsCapability>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resources: Option<ResourcesCapability>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompts: Option<PromptsCapability>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub logging: Option<LoggingCapability>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completions: Option<CompletionsCapability>,
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

// ── tools/list ─────────────────────────────────────────────────────

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

// ── tools/call ─────────────────────────────────────────────────────

/// Parameters for the `tools/call` request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CallToolParams {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arguments: Option<serde_json::Map<String, serde_json::Value>>,
}

/// Re-export the call result type for convenience.
pub type CallToolResult = McpToolCallResult;

// ── resources/list ─────────────────────────────────────────────────

/// Parameters for the `resources/list` request.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ListResourcesParams {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
}

/// Response to the `resources/list` request.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListResourcesResult {
    pub resources: Vec<McpResource>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

// ── resources/read ─────────────────────────────────────────────────

/// Parameters for the `resources/read` request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReadResourceParams {
    pub uri: String,
}

/// Response to the `resources/read` request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReadResourceResult {
    pub contents: Vec<McpResourceContent>,
}

// ── resources/templates/list ───────────────────────────────────────

/// Parameters for the `resources/templates/list` request.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ListResourceTemplatesParams {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
}

/// Response to the `resources/templates/list` request.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListResourceTemplatesResult {
    pub resource_templates: Vec<McpResourceTemplate>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

/// Capability flags for the `resources` feature.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResourcesCapability {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub list_changed: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subscribe: Option<bool>,
}

// ── prompts/list ───────────────────────────────────────────────────

/// Parameters for the `prompts/list` request.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ListPromptsParams {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
}

/// Response to the `prompts/list` request.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListPromptsResult {
    pub prompts: Vec<McpPrompt>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

// ── prompts/get ────────────────────────────────────────────────────

/// Parameters for the `prompts/get` request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetPromptParams {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arguments: Option<std::collections::HashMap<String, String>>,
}

/// Response to the `prompts/get` request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpGetPromptResult {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub messages: Vec<McpPromptMessage>,
}

/// Capability flags for the `prompts` feature.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptsCapability {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub list_changed: Option<bool>,
}

// ── Logging types ──────────────────────────────────────────────────

/// Logging severity levels defined by the MCP 2025-11-25 spec.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LoggingLevel {
    Debug,
    Info,
    Notice,
    Warning,
    Error,
    Critical,
    Alert,
    Emergency,
}

/// Parameters for the `logging/setLevel` request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SetLoggingLevelParams {
    pub level: LoggingLevel,
}

/// Capability flags for the `logging` feature.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LoggingCapability {}

// ── Resource subscription types ────────────────────────────────────

/// Parameters for the `resources/subscribe` request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubscribeResourceParams {
    pub uri: String,
}

/// Parameters for the `resources/unsubscribe` request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UnsubscribeResourceParams {
    pub uri: String,
}

// ── Completion types ───────────────────────────────────────────────

/// Parameters for the `completion/complete` request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompleteParams {
    /// Reference to the prompt or resource to complete.
    #[serde(rename = "ref")]
    pub reference: CompletionRef,
    /// The argument to complete.
    pub argument: CompletionArgument,
}

/// A reference to a prompt or resource for completion.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum CompletionRef {
    #[serde(rename = "ref/prompt")]
    Prompt { name: String },
    #[serde(rename = "ref/resource")]
    Resource { uri: String },
}

/// An argument being completed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompletionArgument {
    /// The name of the argument.
    pub name: String,
    /// The current partial value.
    pub value: String,
}

/// Response to the `completion/complete` request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompleteResult {
    pub completion: Completion,
}

/// Completion suggestions.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Completion {
    /// Suggested completion values.
    pub values: Vec<String>,
    /// Whether there are more completions available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub has_more: Option<bool>,
    /// Total number of available completions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total: Option<u32>,
}

/// Capability flags for the `completions` feature.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CompletionsCapability {}

// ── Notification parameter types ───────────────────────────────────

/// Parameters for `notifications/cancelled`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CancelledNotificationParams {
    /// The ID of the request being cancelled.
    pub request_id: JsonRpcId,
    /// Optional human-readable reason.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// A progress token — may be a string or a number.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ProgressToken {
    Str(String),
    Number(i64),
}

/// Parameters for `notifications/progress`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProgressNotificationParams {
    /// Token matching the progress token from the original request.
    pub progress_token: ProgressToken,
    /// Current progress value.
    pub progress: f64,
    /// Total expected value.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total: Option<f64>,
    /// Human-readable progress message.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// Parameters for `notifications/resources/updated`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResourceUpdatedNotificationParams {
    pub uri: String,
}

/// Parameters for `notifications/message` (log messages).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogMessageNotificationParams {
    /// Severity level.
    pub level: LoggingLevel,
    /// Optional logger name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub logger: Option<String>,
    /// Arbitrary log data.
    pub data: serde_json::Value,
}

// ── JSON-RPC 2.0 envelope types ───────────────────────────────────

/// A JSON-RPC 2.0 request ID -- may be a number or a string.
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

/// An inbound JSON-RPC 2.0 message -- either a [`JsonRpcRequest`] or a
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

// ── Error codes ────────────────────────────────────────────────────

/// Standard JSON-RPC 2.0 error codes used by the MCP server protocol.
pub mod error_codes {
    /// Invalid JSON was received.
    pub const PARSE_ERROR: i64 = -32700;

    /// The JSON sent is not a valid JSON-RPC request.
    pub const INVALID_REQUEST: i64 = -32600;

    /// The method does not exist or is not available.
    pub const METHOD_NOT_FOUND: i64 = -32601;

    /// Invalid method parameter(s).
    pub const INVALID_PARAMS: i64 = -32602;

    /// Internal JSON-RPC error.
    pub const INTERNAL_ERROR: i64 = -32603;

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn codes_are_negative() {
            assert!(PARSE_ERROR < 0);
            assert!(INVALID_REQUEST < 0);
            assert!(METHOD_NOT_FOUND < 0);
            assert!(INVALID_PARAMS < 0);
            assert!(INTERNAL_ERROR < 0);
        }

        #[test]
        fn codes_match_spec() {
            assert_eq!(PARSE_ERROR, -32700);
            assert_eq!(INVALID_REQUEST, -32600);
            assert_eq!(METHOD_NOT_FOUND, -32601);
            assert_eq!(INVALID_PARAMS, -32602);
            assert_eq!(INTERNAL_ERROR, -32603);
        }
    }
}

// ── Tests ──────────────────────────────────────────────────────────

#[cfg(test)]
mod tool_tests {
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
    fn content_image_variant_tagged() {
        let content = McpContent::Image {
            data: "iVBORw0KGgo=".to_string(),
            mime_type: "image/png".to_string(),
        };
        let json = serde_json::to_string(&content).expect("serialize");
        assert!(json.contains(r#""type":"image""#));
        assert!(json.contains(r#""mimeType":"image/png""#));
        let parsed: McpContent = serde_json::from_str(&json).expect("deserialize");
        assert!(matches!(parsed, McpContent::Image { .. }));
    }

    #[test]
    fn content_resource_variant_round_trip() {
        let content = McpContent::Resource {
            resource: McpResourceContent {
                uri: "file:///test.txt".to_string(),
                mime_type: Some("text/plain".to_string()),
                text: Some("hello".to_string()),
                blob: None,
            },
        };
        let json = serde_json::to_string(&content).expect("serialize");
        assert!(json.contains(r#""type":"resource""#));
        let parsed: McpContent = serde_json::from_str(&json).expect("deserialize");
        assert!(matches!(parsed, McpContent::Resource { .. }));
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

#[cfg(test)]
mod protocol_tests {
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
                ..Default::default()
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

#[cfg(test)]
mod jsonrpc_tests {
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
