use std::sync::Arc;

use bitrouter_core::api::mcp::types::{
    McpContent, McpGetPromptResult, McpPrompt, McpPromptArgument, McpPromptContent,
    McpPromptMessage, McpResource, McpResourceContent, McpResourceTemplate, McpRole, McpTool,
    McpToolCallResult,
};
use tokio::sync::broadcast;
use warp::Filter;

use super::filters::mcp_server_filter;

use bitrouter_core::api::mcp::types::{CompleteParams, CompleteResult, Completion, LoggingLevel};

use bitrouter_core::api::mcp::error::McpGatewayError;
use bitrouter_core::api::mcp::gateway::{
    McpCompletionServer, McpLoggingServer, McpPromptServer, McpResourceServer,
    McpSubscriptionServer, McpToolServer,
};
use bitrouter_core::api::mcp::types::error_codes;

// ── Mock server ─────────────────────────────────────────────────────

struct MockServer {
    tool_change_tx: broadcast::Sender<()>,
    resource_change_tx: broadcast::Sender<()>,
    prompt_change_tx: broadcast::Sender<()>,
}

impl MockServer {
    fn new() -> Self {
        let (tool_change_tx, _) = broadcast::channel(16);
        let (resource_change_tx, _) = broadcast::channel(16);
        let (prompt_change_tx, _) = broadcast::channel(16);
        Self {
            tool_change_tx,
            resource_change_tx,
            prompt_change_tx,
        }
    }
}

impl McpToolServer for MockServer {
    async fn list_tools(&self) -> Vec<McpTool> {
        vec![McpTool {
            name: "test/echo".to_string(),
            description: Some("Echo tool".to_string()),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {"message": {"type": "string"}}
            }),
        }]
    }

    async fn call_tool(
        &self,
        name: &str,
        _arguments: Option<serde_json::Map<String, serde_json::Value>>,
    ) -> Result<McpToolCallResult, McpGatewayError> {
        if name == "test/echo" {
            Ok(McpToolCallResult {
                content: vec![McpContent::Text {
                    text: "echoed".to_string(),
                }],
                is_error: None,
            })
        } else {
            Err(McpGatewayError::ToolNotFound {
                name: name.to_string(),
            })
        }
    }

    fn subscribe_tool_changes(&self) -> broadcast::Receiver<()> {
        self.tool_change_tx.subscribe()
    }
}

impl McpResourceServer for MockServer {
    async fn list_resources(&self) -> Vec<McpResource> {
        vec![McpResource {
            uri: "test+file:///readme.md".to_string(),
            name: "readme".to_string(),
            description: Some("Test readme".to_string()),
            mime_type: Some("text/markdown".to_string()),
        }]
    }

    async fn read_resource(&self, uri: &str) -> Result<Vec<McpResourceContent>, McpGatewayError> {
        if uri == "test+file:///readme.md" {
            Ok(vec![McpResourceContent {
                uri: uri.to_string(),
                mime_type: Some("text/markdown".to_string()),
                text: Some("# Hello".to_string()),
                blob: None,
            }])
        } else {
            Err(McpGatewayError::ResourceNotFound {
                uri: uri.to_string(),
            })
        }
    }

    async fn list_resource_templates(&self) -> Vec<McpResourceTemplate> {
        vec![McpResourceTemplate {
            uri_template: "test+file:///{path}".to_string(),
            name: "files".to_string(),
            description: Some("Access files".to_string()),
            mime_type: None,
        }]
    }

    fn subscribe_resource_changes(&self) -> broadcast::Receiver<()> {
        self.resource_change_tx.subscribe()
    }
}

impl McpPromptServer for MockServer {
    async fn list_prompts(&self) -> Vec<McpPrompt> {
        vec![McpPrompt {
            name: "test/summarize".to_string(),
            description: Some("Summarize text".to_string()),
            arguments: vec![McpPromptArgument {
                name: "text".to_string(),
                description: Some("The text to summarize".to_string()),
                required: Some(true),
            }],
        }]
    }

    async fn get_prompt(
        &self,
        name: &str,
        _arguments: Option<std::collections::HashMap<String, String>>,
    ) -> Result<McpGetPromptResult, McpGatewayError> {
        if name == "test/summarize" {
            Ok(McpGetPromptResult {
                description: Some("Summarize text".to_string()),
                messages: vec![McpPromptMessage {
                    role: McpRole::User,
                    content: McpPromptContent::Text {
                        text: "Please summarize this".to_string(),
                    },
                }],
            })
        } else {
            Err(McpGatewayError::PromptNotFound {
                name: name.to_string(),
            })
        }
    }

    fn subscribe_prompt_changes(&self) -> broadcast::Receiver<()> {
        self.prompt_change_tx.subscribe()
    }
}

impl McpSubscriptionServer for MockServer {
    async fn subscribe_resource(&self, uri: &str) -> Result<(), McpGatewayError> {
        if uri.starts_with("test+") {
            Ok(())
        } else {
            Err(McpGatewayError::SubscriptionNotSupported {
                uri: uri.to_string(),
            })
        }
    }

    async fn unsubscribe_resource(&self, _uri: &str) -> Result<(), McpGatewayError> {
        Ok(())
    }
}

impl McpLoggingServer for MockServer {
    async fn set_logging_level(&self, _level: LoggingLevel) -> Result<(), McpGatewayError> {
        Ok(())
    }
}

impl McpCompletionServer for MockServer {
    async fn complete(&self, _params: CompleteParams) -> Result<CompleteResult, McpGatewayError> {
        Ok(CompleteResult {
            completion: Completion {
                values: vec!["suggestion1".to_string()],
                has_more: Some(false),
                total: Some(1),
            },
        })
    }
}

// ── Helpers ─────────────────────────────────────────────────────────

fn make_filter() -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone {
    let server = Arc::new(MockServer::new());
    mcp_server_filter(Some(server))
}

fn make_none_filter() -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone
{
    mcp_server_filter::<MockServer>(None)
}

/// Send a JSON-RPC request and return the parsed response body.
async fn jsonrpc_request(
    filter: &(impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone + 'static),
    body: serde_json::Value,
) -> serde_json::Value {
    let resp = warp::test::request()
        .method("POST")
        .path("/mcp")
        .json(&body)
        .reply(filter)
        .await;
    assert_eq!(resp.status(), 200);
    serde_json::from_slice(resp.body()).expect("parse response")
}

// ── Initialize & protocol tests ─────────────────────────────────────

#[tokio::test]
async fn initialize_returns_all_capabilities() {
    let filter = make_filter();
    let json = jsonrpc_request(
        &filter,
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-03-26",
                "capabilities": {},
                "clientInfo": {"name": "test"}
            }
        }),
    )
    .await;
    assert_eq!(json["jsonrpc"], "2.0");
    assert_eq!(json["result"]["serverInfo"]["name"], "bitrouter");

    let caps = &json["result"]["capabilities"];
    assert!(caps["tools"]["listChanged"].as_bool().unwrap_or(false));
    assert!(caps["resources"]["listChanged"].as_bool().unwrap_or(false));
    assert!(caps["prompts"]["listChanged"].as_bool().unwrap_or(false));
}

#[tokio::test]
async fn ping_returns_empty_result() {
    let filter = make_filter();
    let json = jsonrpc_request(
        &filter,
        serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "ping"}),
    )
    .await;
    assert!(json["result"].is_object());
    assert!(json["error"].is_null());
}

#[tokio::test]
async fn unknown_method_returns_error() {
    let filter = make_filter();
    let json = jsonrpc_request(
        &filter,
        serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "unknown/method"}),
    )
    .await;
    assert_eq!(json["error"]["code"], error_codes::METHOD_NOT_FOUND);
}

#[tokio::test]
async fn notification_returns_accepted() {
    let filter = make_filter();
    let resp = warp::test::request()
        .method("POST")
        .path("/mcp")
        .json(&serde_json::json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized"
        }))
        .reply(&filter)
        .await;
    assert_eq!(resp.status(), 202);
}

#[tokio::test]
async fn none_server_returns_404() {
    let filter = make_none_filter();
    let resp = warp::test::request()
        .method("POST")
        .path("/mcp")
        .json(&serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "initialize"}))
        .reply(&filter)
        .await;
    assert_eq!(resp.status(), 404);
}

#[tokio::test]
async fn malformed_json_returns_400() {
    let filter = make_filter();
    let resp = warp::test::request()
        .method("POST")
        .path("/mcp")
        .header("content-type", "application/json")
        .body("not json")
        .reply(&filter)
        .await;
    assert_eq!(resp.status(), 400);
}

#[tokio::test]
async fn bad_jsonrpc_returns_parse_error() {
    let filter = make_filter();
    let json = jsonrpc_request(&filter, serde_json::json!({"foo": "bar"})).await;
    assert_eq!(json["error"]["code"], error_codes::PARSE_ERROR);
}

#[tokio::test]
async fn string_id_preserved() {
    let filter = make_filter();
    let json = jsonrpc_request(
        &filter,
        serde_json::json!({"jsonrpc": "2.0", "id": "abc-123", "method": "ping"}),
    )
    .await;
    assert_eq!(json["id"], "abc-123");
}

// ── Tools tests ─────────────────────────────────────────────────────

#[tokio::test]
async fn tools_list_returns_tools() {
    let filter = make_filter();
    let json = jsonrpc_request(
        &filter,
        serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}),
    )
    .await;
    let tools = json["result"]["tools"].as_array().expect("tools array");
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0]["name"], "test__echo");
    assert_eq!(tools[0]["description"], "Echo tool");
}

#[tokio::test]
async fn tools_call_succeeds() {
    let filter = make_filter();
    let json = jsonrpc_request(
        &filter,
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {"name": "test__echo", "arguments": {"message": "hi"}}
        }),
    )
    .await;
    assert_eq!(json["result"]["content"][0]["text"], "echoed");
}

#[tokio::test]
async fn tools_call_not_found() {
    let filter = make_filter();
    let json = jsonrpc_request(
        &filter,
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {"name": "nonexistent/tool"}
        }),
    )
    .await;
    assert_eq!(json["error"]["code"], error_codes::METHOD_NOT_FOUND);
}

#[tokio::test]
async fn tools_call_missing_params() {
    let filter = make_filter();
    let json = jsonrpc_request(
        &filter,
        serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call"}),
    )
    .await;
    assert_eq!(json["error"]["code"], error_codes::INVALID_PARAMS);
}

// ── Resources tests ─────────────────────────────────────────────────

#[tokio::test]
async fn resources_list_returns_resources() {
    let filter = make_filter();
    let json = jsonrpc_request(
        &filter,
        serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "resources/list"}),
    )
    .await;
    let resources = json["result"]["resources"]
        .as_array()
        .expect("resources array");
    assert_eq!(resources.len(), 1);
    assert_eq!(resources[0]["uri"], "test+file:///readme.md");
    assert_eq!(resources[0]["name"], "readme");
    assert_eq!(resources[0]["mimeType"], "text/markdown");
}

#[tokio::test]
async fn resources_read_returns_contents() {
    let filter = make_filter();
    let json = jsonrpc_request(
        &filter,
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "resources/read",
            "params": {"uri": "test+file:///readme.md"}
        }),
    )
    .await;
    let contents = json["result"]["contents"]
        .as_array()
        .expect("contents array");
    assert_eq!(contents.len(), 1);
    assert_eq!(contents[0]["text"], "# Hello");
    assert_eq!(contents[0]["mimeType"], "text/markdown");
}

#[tokio::test]
async fn resources_read_not_found() {
    let filter = make_filter();
    let json = jsonrpc_request(
        &filter,
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "resources/read",
            "params": {"uri": "unknown+file:///missing.txt"}
        }),
    )
    .await;
    assert_eq!(json["error"]["code"], error_codes::METHOD_NOT_FOUND);
}

#[tokio::test]
async fn resources_read_missing_params() {
    let filter = make_filter();
    let json = jsonrpc_request(
        &filter,
        serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "resources/read"}),
    )
    .await;
    assert_eq!(json["error"]["code"], error_codes::INVALID_PARAMS);
}

#[tokio::test]
async fn resources_templates_list_returns_templates() {
    let filter = make_filter();
    let json = jsonrpc_request(
        &filter,
        serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "resources/templates/list"}),
    )
    .await;
    let templates = json["result"]["resourceTemplates"]
        .as_array()
        .expect("templates array");
    assert_eq!(templates.len(), 1);
    assert_eq!(templates[0]["uriTemplate"], "test+file:///{path}");
    assert_eq!(templates[0]["name"], "files");
}

// ── Prompts tests ───────────────────────────────────────────────────

#[tokio::test]
async fn prompts_list_returns_prompts() {
    let filter = make_filter();
    let json = jsonrpc_request(
        &filter,
        serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "prompts/list"}),
    )
    .await;
    let prompts = json["result"]["prompts"].as_array().expect("prompts array");
    assert_eq!(prompts.len(), 1);
    assert_eq!(prompts[0]["name"], "test/summarize");
    assert_eq!(prompts[0]["description"], "Summarize text");
    let args = prompts[0]["arguments"].as_array().expect("arguments array");
    assert_eq!(args.len(), 1);
    assert_eq!(args[0]["name"], "text");
    assert_eq!(args[0]["required"], true);
}

#[tokio::test]
async fn prompts_get_succeeds() {
    let filter = make_filter();
    let json = jsonrpc_request(
        &filter,
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "prompts/get",
            "params": {"name": "test/summarize"}
        }),
    )
    .await;
    assert_eq!(json["result"]["description"], "Summarize text");
    let messages = json["result"]["messages"]
        .as_array()
        .expect("messages array");
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0]["role"], "user");
    assert_eq!(messages[0]["content"]["text"], "Please summarize this");
}

#[tokio::test]
async fn prompts_get_with_arguments() {
    let filter = make_filter();
    let json = jsonrpc_request(
        &filter,
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "prompts/get",
            "params": {
                "name": "test/summarize",
                "arguments": {"text": "some long text"}
            }
        }),
    )
    .await;
    // Our mock ignores arguments but should still succeed
    assert!(json["result"]["messages"].is_array());
}

#[tokio::test]
async fn prompts_get_not_found() {
    let filter = make_filter();
    let json = jsonrpc_request(
        &filter,
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "prompts/get",
            "params": {"name": "nonexistent/prompt"}
        }),
    )
    .await;
    assert_eq!(json["error"]["code"], error_codes::METHOD_NOT_FOUND);
}

#[tokio::test]
async fn prompts_get_missing_params() {
    let filter = make_filter();
    let json = jsonrpc_request(
        &filter,
        serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "prompts/get"}),
    )
    .await;
    assert_eq!(json["error"]["code"], error_codes::INVALID_PARAMS);
}

// ── Subscription tests ──────────────────────────────────────────────

#[tokio::test]
async fn resources_subscribe_succeeds() {
    let filter = make_filter();
    let json = jsonrpc_request(
        &filter,
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "resources/subscribe",
            "params": {"uri": "test+file:///readme.md"}
        }),
    )
    .await;
    assert!(json["result"].is_object());
    assert!(json["error"].is_null());
}

#[tokio::test]
async fn resources_subscribe_unsupported_uri() {
    let filter = make_filter();
    let json = jsonrpc_request(
        &filter,
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "resources/subscribe",
            "params": {"uri": "unknown://foo"}
        }),
    )
    .await;
    assert_eq!(json["error"]["code"], error_codes::INVALID_PARAMS);
}

#[tokio::test]
async fn resources_subscribe_missing_params() {
    let filter = make_filter();
    let json = jsonrpc_request(
        &filter,
        serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "resources/subscribe"}),
    )
    .await;
    assert_eq!(json["error"]["code"], error_codes::INVALID_PARAMS);
}

#[tokio::test]
async fn resources_unsubscribe_succeeds() {
    let filter = make_filter();
    let json = jsonrpc_request(
        &filter,
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "resources/unsubscribe",
            "params": {"uri": "test+file:///readme.md"}
        }),
    )
    .await;
    assert!(json["result"].is_object());
    assert!(json["error"].is_null());
}

// ── Logging tests ───────────────────────────────────────────────────

#[tokio::test]
async fn logging_set_level_succeeds() {
    let filter = make_filter();
    let json = jsonrpc_request(
        &filter,
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "logging/setLevel",
            "params": {"level": "warning"}
        }),
    )
    .await;
    assert!(json["result"].is_object());
    assert!(json["error"].is_null());
}

#[tokio::test]
async fn logging_set_level_missing_params() {
    let filter = make_filter();
    let json = jsonrpc_request(
        &filter,
        serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "logging/setLevel"}),
    )
    .await;
    assert_eq!(json["error"]["code"], error_codes::INVALID_PARAMS);
}

// ── Completion tests ────────────────────────────────────────────────

#[tokio::test]
async fn completion_complete_prompt_ref() {
    let filter = make_filter();
    let json = jsonrpc_request(
        &filter,
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "completion/complete",
            "params": {
                "ref": {"type": "ref/prompt", "name": "test/summarize"},
                "argument": {"name": "text", "value": "hel"}
            }
        }),
    )
    .await;
    assert!(json["error"].is_null());
    let values = json["result"]["completion"]["values"]
        .as_array()
        .expect("values array");
    assert_eq!(values[0], "suggestion1");
}

#[tokio::test]
async fn completion_complete_resource_ref() {
    let filter = make_filter();
    let json = jsonrpc_request(
        &filter,
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "completion/complete",
            "params": {
                "ref": {"type": "ref/resource", "uri": "test+file:///{path}"},
                "argument": {"name": "path", "value": "src/"}
            }
        }),
    )
    .await;
    assert!(json["error"].is_null());
    assert!(json["result"]["completion"]["values"].is_array());
}

#[tokio::test]
async fn completion_complete_missing_params() {
    let filter = make_filter();
    let json = jsonrpc_request(
        &filter,
        serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "completion/complete"}),
    )
    .await;
    assert_eq!(json["error"]["code"], error_codes::INVALID_PARAMS);
}

// ── Capabilities and version tests ──────────────────────────────────

#[tokio::test]
async fn initialize_protocol_version_is_2025_11_25() {
    let filter = make_filter();
    let json = jsonrpc_request(
        &filter,
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-11-25",
                "capabilities": {},
                "clientInfo": {"name": "test"}
            }
        }),
    )
    .await;
    assert_eq!(json["result"]["protocolVersion"], "2025-11-25");
}

#[tokio::test]
async fn initialize_advertises_logging_but_not_unimplemented() {
    let filter = make_filter();
    let json = jsonrpc_request(
        &filter,
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-11-25",
                "capabilities": {},
                "clientInfo": {"name": "test"}
            }
        }),
    )
    .await;
    let caps = &json["result"]["capabilities"];
    assert!(caps["logging"].is_object());
    // Completions and resource subscriptions are not advertised until implemented.
    assert!(caps["completions"].is_null());
    assert!(caps["resources"]["subscribe"].is_null());
}

// ── Notification tests ──────────────────────────────────────────────

#[tokio::test]
async fn notification_cancelled_returns_accepted() {
    let filter = make_filter();
    let resp = warp::test::request()
        .method("POST")
        .path("/mcp")
        .json(&serde_json::json!({
            "jsonrpc": "2.0",
            "method": "notifications/cancelled",
            "params": {"requestId": 42, "reason": "user cancelled"}
        }))
        .reply(&filter)
        .await;
    assert_eq!(resp.status(), 202);
}

#[tokio::test]
async fn notification_progress_returns_accepted() {
    let filter = make_filter();
    let resp = warp::test::request()
        .method("POST")
        .path("/mcp")
        .json(&serde_json::json!({
            "jsonrpc": "2.0",
            "method": "notifications/progress",
            "params": {"progressToken": "tok-1", "progress": 0.5}
        }))
        .reply(&filter)
        .await;
    assert_eq!(resp.status(), 202);
}

#[tokio::test]
async fn notification_roots_list_changed_returns_accepted() {
    let filter = make_filter();
    let resp = warp::test::request()
        .method("POST")
        .path("/mcp")
        .json(&serde_json::json!({
            "jsonrpc": "2.0",
            "method": "notifications/roots/list_changed"
        }))
        .reply(&filter)
        .await;
    assert_eq!(resp.status(), 202);
}
