//! `BitrouterMcp` — the rmcp origin server handler. One `#[tool_router]`
//! definition serves both stdio and streamable HTTP.

use std::sync::Arc;

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, Content, ServerCapabilities, ServerInfo};
use rmcp::{ErrorData as McpError, ServerHandler, tool, tool_handler, tool_router};

use crate::backend::{Backend, CompleteRequest};

#[derive(Clone)]
pub struct BitrouterMcp {
    backend: Arc<dyn Backend>,
    tool_router: ToolRouter<BitrouterMcp>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct CompleteArgs {
    /// Routable model name (from `list_models`).
    pub model: String,
    /// Chat messages, OpenAI shape: `[{"role":"user","content":"…"}]`.
    pub messages: Vec<serde_json::Value>,
    pub max_tokens: Option<u32>,
    pub temperature: Option<f32>,
    pub system: Option<String>,
}

#[tool_router]
impl BitrouterMcp {
    pub fn new(backend: Arc<dyn Backend>) -> Self {
        Self {
            backend,
            tool_router: Self::tool_router(),
        }
    }

    #[tool(description = "Route a completion through BitRouter and return the full result.")]
    async fn complete(
        &self,
        Parameters(args): Parameters<CompleteArgs>,
    ) -> Result<CallToolResult, McpError> {
        let req = CompleteRequest {
            model: args.model,
            messages: args.messages,
            max_tokens: args.max_tokens,
            temperature: args.temperature,
            system: args.system,
        };
        match self.backend.complete(req).await {
            Ok(r) => Ok(CallToolResult::success(vec![Content::text(
                serde_json::to_string(&r).unwrap_or_default(),
            )])),
            Err(e) => Ok(CallToolResult::error(vec![Content::text(e.to_string())])),
        }
    }

    #[tool(description = "List models routable through BitRouter.")]
    async fn list_models(&self) -> Result<CallToolResult, McpError> {
        match self.backend.list_models().await {
            Ok(m) => Ok(CallToolResult::success(vec![Content::text(
                serde_json::to_string(&m).unwrap_or_default(),
            )])),
            Err(e) => Ok(CallToolResult::error(vec![Content::text(e.to_string())])),
        }
    }

    #[tool(
        description = "Report BitRouter status (local: liveness/models/providers; cloud: credit balance)."
    )]
    async fn status(&self) -> Result<CallToolResult, McpError> {
        match self.backend.status().await {
            Ok(s) => Ok(CallToolResult::success(vec![Content::text(
                serde_json::to_string(&s).unwrap_or_default(),
            )])),
            Err(e) => Ok(CallToolResult::error(vec![Content::text(e.to_string())])),
        }
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for BitrouterMcp {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build()).with_instructions(
            "BitRouter origin MCP server. Use `list_models` to discover routable \
                 models, `complete` to run a completion, `status` for health/credits."
                .to_string(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{BackendError, CompleteResponse, ModelInfo, StatusInfo, Usage};

    struct StubBackend;
    #[async_trait::async_trait]
    impl Backend for StubBackend {
        async fn complete(&self, _: CompleteRequest) -> Result<CompleteResponse, BackendError> {
            Ok(CompleteResponse {
                content: "ok".into(),
                model: "m".into(),
                usage: Usage {
                    input_tokens: 1,
                    output_tokens: 1,
                },
                finish_reason: "stop".into(),
            })
        }
        async fn list_models(&self) -> Result<Vec<ModelInfo>, BackendError> {
            Ok(vec![])
        }
        async fn status(&self) -> Result<StatusInfo, BackendError> {
            Ok(StatusInfo::Cloud {
                available_micro_usd: 1,
                balance_micro_usd: 1,
                pending_micro_usd: 0,
            })
        }
    }

    #[test]
    fn handler_constructs_with_three_tools() {
        let h = BitrouterMcp::new(Arc::new(StubBackend));
        assert_eq!(h.tool_router.list_all().len(), 3);
    }
}
