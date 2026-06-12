//! [`McpRouterToolset`] — a [`RouterToolset`] backed by the [`crate::mcp`]
//! routing module. BitRouter acts as an MCP
//! *client*: it discovers an upstream MCP server's tools (`tools/list`),
//! advertises them to the model, and executes the model's calls (`tools/call`)
//! inside the LLM loop.
//!
//! Generic over `Arc<dyn crate::mcp::Executor>`, so it works with the bundled
//! `RmcpExecutor` (behind the `mcp` feature) or any custom executor — it needs
//! no feature gate of its own.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use super::toolset::RouterToolset;
use crate::caller::CallerContext;
use crate::error::Result;
use crate::language_model::types::{
    ProviderMetadata, Tool, ToolResultContentPart, ToolResultOutput,
};
use crate::mcp::{Executor as McpExecutor, McpRequest, RoutingTable as McpRoutingTable};

/// Convert an MCP `tools/list` result into canonical IR function tools. Each
/// MCP tool `{name, description?, inputSchema}` becomes a [`Tool::Function`];
/// `prefix` (if set) is prepended to the name so router tool names cannot
/// collide with the caller's own.
fn tools_from_list(value: &serde_json::Value, prefix: Option<&str>) -> Vec<Tool> {
    let Some(entries) = value.get("tools").and_then(|t| t.as_array()) else {
        return Vec::new();
    };
    let mut tools = Vec::with_capacity(entries.len());
    for entry in entries {
        let Some(name) = entry.get("name").and_then(|n| n.as_str()) else {
            continue;
        };
        let full_name = match prefix {
            Some(p) => format!("{p}{name}"),
            None => name.to_string(),
        };
        let description = entry
            .get("description")
            .and_then(|d| d.as_str())
            .map(str::to_string);
        let parameters = entry
            .get("inputSchema")
            .cloned()
            .unwrap_or_else(|| serde_json::json!({ "type": "object" }));
        tools.push(Tool::Function {
            name: full_name,
            description,
            parameters,
            strict: None,
            provider_metadata: ProviderMetadata::new(),
        });
    }
    tools
}

/// Convert an MCP `tools/call` result into a canonical IR tool-result output.
/// All-text content maps to [`ToolResultOutput::Text`] (single) or
/// [`ToolResultOutput::Content`] (multiple); `isError` maps to the error
/// variants. Non-text or unrecognised shapes are preserved verbatim as
/// [`ToolResultOutput::Json`] (lossless).
fn output_from_call(value: &serde_json::Value) -> ToolResultOutput {
    let is_error = value
        .get("isError")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    if let Some(content) = value.get("content").and_then(|c| c.as_array()) {
        let all_text = content
            .iter()
            .all(|i| i.get("type").and_then(|t| t.as_str()) == Some("text"));
        if all_text {
            let parts: Vec<ToolResultContentPart> = content
                .iter()
                .filter_map(|i| i.get("text").and_then(|t| t.as_str()))
                .map(|s| ToolResultContentPart::Text {
                    text: s.to_string(),
                })
                .collect();
            let joined = parts
                .iter()
                .filter_map(|p| match p {
                    ToolResultContentPart::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            return if is_error {
                ToolResultOutput::ErrorText { value: joined }
            } else if parts.len() <= 1 {
                ToolResultOutput::Text { value: joined }
            } else {
                ToolResultOutput::Content { value: parts }
            };
        }
    }
    if is_error {
        ToolResultOutput::ErrorJson {
            value: value.clone(),
        }
    } else {
        ToolResultOutput::Json {
            value: value.clone(),
        }
    }
}

/// A [`RouterToolset`] over one upstream MCP server.
pub struct McpRouterToolset {
    executor: Arc<dyn McpExecutor>,
    routing: Arc<dyn McpRoutingTable>,
    server_name: String,
    prefix: Option<String>,
    /// Advertised (prefixed) tool names, cached on `list_tools` so `owns` can
    /// route an intercepted call back to this set.
    advertised: Mutex<BTreeSet<String>>,
}

impl McpRouterToolset {
    /// Build a toolset for the named MCP server.
    pub fn new(
        executor: Arc<dyn McpExecutor>,
        routing: Arc<dyn McpRoutingTable>,
        server_name: impl Into<String>,
        prefix: Option<String>,
    ) -> Self {
        Self {
            executor,
            routing,
            server_name: server_name.into(),
            prefix,
            advertised: Mutex::new(BTreeSet::new()),
        }
    }

    /// Strip the configured prefix off a router-facing tool name to recover the
    /// MCP server's own name.
    fn unprefixed<'a>(&self, name: &'a str) -> &'a str {
        match &self.prefix {
            Some(p) => name.strip_prefix(p.as_str()).unwrap_or(name),
            None => name,
        }
    }
}

#[async_trait]
impl RouterToolset for McpRouterToolset {
    async fn list_tools(&self, caller: &CallerContext) -> Result<Vec<Tool>> {
        let request = McpRequest::direct(
            self.server_name.clone(),
            "tools/list",
            serde_json::json!({}),
            caller.clone(),
        );
        let target = self.routing.resolve(&request.selector, caller).await?;
        let response = self.executor.execute(&target, &request).await?;
        let tools = tools_from_list(&response.result, self.prefix.as_deref());
        if let Ok(mut advertised) = self.advertised.lock() {
            advertised.clear();
            for tool in &tools {
                advertised.insert(tool.name().to_string());
            }
        }
        Ok(tools)
    }

    async fn call_tool(
        &self,
        name: &str,
        arguments: &str,
        caller: &CallerContext,
    ) -> Result<ToolResultOutput> {
        let parsed: serde_json::Value =
            serde_json::from_str(arguments).unwrap_or_else(|_| serde_json::json!({}));
        let params = serde_json::json!({
            "name": self.unprefixed(name),
            "arguments": parsed,
        });
        let request = McpRequest::direct(
            self.server_name.clone(),
            "tools/call",
            params,
            caller.clone(),
        );
        let target = self.routing.resolve(&request.selector, caller).await?;
        let response = self.executor.execute(&target, &request).await?;
        Ok(output_from_call(&response.result))
    }

    fn owns(&self, name: &str) -> bool {
        if self
            .prefix
            .as_ref()
            .is_some_and(|p| name.starts_with(p.as_str()))
        {
            return true;
        }
        self.advertised
            .lock()
            .is_ok_and(|advertised| advertised.contains(name))
    }

    fn server_name(&self) -> Option<&str> {
        Some(&self.server_name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::transport::McpTransport;
    use crate::mcp::{McpResponse, McpStreamPart, McpTarget, ServerSelector};
    use futures::stream::BoxStream;

    fn list_json() -> serde_json::Value {
        serde_json::json!({
            "tools": [
                { "name": "search", "description": "web search",
                  "inputSchema": { "type": "object", "properties": { "q": { "type": "string" } } } },
                { "name": "echo" }
            ]
        })
    }

    #[test]
    fn tools_from_list_applies_prefix_and_defaults() {
        let tools = tools_from_list(&list_json(), Some("mcp__demo__"));
        assert_eq!(tools.len(), 2);
        let Tool::Function {
            name,
            description,
            parameters,
            ..
        } = &tools[0]
        else {
            panic!("expected function tool");
        };
        assert_eq!(name, "mcp__demo__search");
        assert_eq!(description.as_deref(), Some("web search"));
        assert_eq!(parameters["type"], "object");
        // Second tool had no description / inputSchema → defaults.
        let Tool::Function {
            name, description, ..
        } = &tools[1]
        else {
            panic!("expected function tool");
        };
        assert_eq!(name, "mcp__demo__echo");
        assert!(description.is_none());
    }

    #[test]
    fn output_from_call_maps_text_and_errors() {
        let ok = serde_json::json!({ "content": [ { "type": "text", "text": "42" } ] });
        assert_eq!(
            output_from_call(&ok),
            ToolResultOutput::Text {
                value: "42".to_string()
            }
        );
        let err = serde_json::json!({ "isError": true, "content": [ { "type": "text", "text": "boom" } ] });
        assert_eq!(
            output_from_call(&err),
            ToolResultOutput::ErrorText {
                value: "boom".to_string()
            }
        );
        let img = serde_json::json!({ "content": [ { "type": "image", "data": "..." } ] });
        assert!(matches!(
            output_from_call(&img),
            ToolResultOutput::Json { .. }
        ));
    }

    struct MockMcp {
        list: serde_json::Value,
        call: serde_json::Value,
    }

    #[async_trait]
    impl McpExecutor for MockMcp {
        async fn execute(&self, _target: &McpTarget, request: &McpRequest) -> Result<McpResponse> {
            let result = if request.method == "tools/list" {
                self.list.clone()
            } else {
                self.call.clone()
            };
            Ok(McpResponse {
                request_id: request.request_id.clone(),
                result,
            })
        }
        async fn execute_streaming(
            &self,
            _target: &McpTarget,
            _request: &McpRequest,
        ) -> Result<BoxStream<'static, Result<McpStreamPart>>> {
            Err(crate::error::BitrouterError::internal("not used in tests"))
        }
    }

    struct MockRouting;

    #[async_trait]
    impl McpRoutingTable for MockRouting {
        async fn resolve(
            &self,
            _selector: &ServerSelector,
            _caller: &CallerContext,
        ) -> Result<McpTarget> {
            Ok(McpTarget::Direct {
                server_name: "demo".to_string(),
                transport: McpTransport::Http {
                    url: "http://localhost".to_string(),
                    headers: Default::default(),
                },
            })
        }
    }

    #[tokio::test]
    async fn mcp_toolset_lists_executes_and_owns() {
        let mock = Arc::new(MockMcp {
            list: list_json(),
            call: serde_json::json!({ "content": [ { "type": "text", "text": "done" } ] }),
        });
        let toolset = McpRouterToolset::new(
            mock,
            Arc::new(MockRouting),
            "demo",
            Some("mcp__demo__".to_string()),
        );
        let caller = CallerContext::local();

        let tools = toolset.list_tools(&caller).await.unwrap();
        assert_eq!(tools.len(), 2);
        assert!(toolset.owns("mcp__demo__search"));
        assert!(!toolset.owns("some_client_tool"));

        let out = toolset
            .call_tool("mcp__demo__search", "{\"q\":\"rust\"}", &caller)
            .await
            .unwrap();
        assert_eq!(
            out,
            ToolResultOutput::Text {
                value: "done".to_string()
            }
        );
    }
}
