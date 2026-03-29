//! MCP client — upstream connection management, registry, route construction,
//! and sampling request handler.

use std::sync::Arc;

use warp::Filter;

type RouteFilter = warp::filters::BoxedFilter<(Box<dyn warp::Reply>,)>;

/// Warp route filters produced by MCP client initialization.
pub struct McpRoutes {
    /// Admin tool management endpoints (gated by management auth).
    pub admin_tool_routes: RouteFilter,
    /// MCP JSON-RPC server endpoint (`POST /mcp`, `GET /mcp/sse`).
    pub server: RouteFilter,
    /// `GET /v1/tools` composite tool listing (MCP + skills).
    pub tool_list: RouteFilter,
    /// Per-server bridge endpoints (`POST /mcp/:server`, `GET /mcp/:server/sse`).
    pub bridge_routes: RouteFilter,
    /// Background task guards — dropped when routes are dropped.
    _guards: Vec<Box<dyn std::any::Any + Send>>,
}

impl McpRoutes {
    /// Noop routes for when the `mcp` feature is disabled.
    #[cfg(not(feature = "mcp"))]
    pub fn noop() -> Self {
        let noop = warp::path!("mcp" / ..)
            .and_then(|| async { Err::<String, _>(warp::reject::not_found()) })
            .map(|r: String| Box::new(r) as Box<dyn warp::Reply>)
            .boxed();
        Self {
            admin_tool_routes: noop.clone(),
            server: noop.clone(),
            tool_list: noop.clone(),
            bridge_routes: noop,
            _guards: Vec::new(),
        }
    }
}

// ── Feature-gated builder ────────────────────────────────────────

#[cfg(feature = "mcp")]
use std::collections::HashMap;
#[cfg(feature = "mcp")]
use std::pin::Pin;

#[cfg(feature = "mcp")]
use bitrouter_api::router::{admin_tools, mcp as mcp_admin, tools};
#[cfg(feature = "mcp")]
use bitrouter_config::{ApiProtocol, ProviderConfig};
#[cfg(feature = "mcp")]
use bitrouter_core::api::mcp::gateway::{
    McpClientRequestHandler, McpPromptServer, McpResourceServer, McpToolServer,
};
#[cfg(feature = "mcp")]
use bitrouter_core::api::mcp::types::{
    CreateMessageParams, CreateMessageResult, ElicitationCreateParams, ElicitationCreateResult,
    JsonRpcError, McpRole, SamplingContent, SamplingContentOrArray,
};
#[cfg(feature = "mcp")]
use bitrouter_core::models::language::call_options::LanguageModelCallOptions;
#[cfg(feature = "mcp")]
use bitrouter_core::models::language::content::LanguageModelContent;
#[cfg(feature = "mcp")]
use bitrouter_core::models::language::data_content::LanguageModelDataContent;
#[cfg(feature = "mcp")]
use bitrouter_core::models::language::finish_reason::LanguageModelFinishReason;
#[cfg(feature = "mcp")]
use bitrouter_core::models::language::language_model::LanguageModel;
#[cfg(feature = "mcp")]
use bitrouter_core::models::language::prompt::{
    LanguageModelAssistantContent, LanguageModelMessage, LanguageModelToolResult,
    LanguageModelToolResultOutput, LanguageModelUserContent,
};
#[cfg(feature = "mcp")]
use bitrouter_core::models::language::tool::LanguageModelTool;
#[cfg(feature = "mcp")]
use bitrouter_core::models::language::tool_choice::LanguageModelToolChoice;
#[cfg(feature = "mcp")]
use bitrouter_core::models::shared::types::JsonSchema;
#[cfg(feature = "mcp")]
use bitrouter_core::observe::{CallerContext, ToolObserveCallback};
#[cfg(feature = "mcp")]
use bitrouter_core::routers::admin::{ParamRestrictions, ToolFilter};
#[cfg(feature = "mcp")]
use bitrouter_core::routers::dynamic_tool::DynamicToolRegistry;
#[cfg(feature = "mcp")]
use bitrouter_core::routers::model_router::LanguageModelRouter;
#[cfg(feature = "mcp")]
use bitrouter_core::routers::routing_table::{RouteEntry, RoutingTable, RoutingTarget};
#[cfg(feature = "mcp")]
use bitrouter_providers::agentskills::registry::FilesystemSkillRegistry;
#[cfg(feature = "mcp")]
use bitrouter_providers::mcp::client::bridge::SingleServerBridge;
#[cfg(feature = "mcp")]
use bitrouter_providers::mcp::client::registry::ConfigMcpRegistry;
#[cfg(feature = "mcp")]
use bitrouter_providers::mcp::client::upstream::UpstreamConnection;

#[cfg(feature = "mcp")]
use crate::runtime::auth::{self, JwtAuthContext};

// ── McpClient builder ────────────────────────────────────────────

/// Builder for MCP upstream connections, registries, and route filters.
#[cfg(feature = "mcp")]
pub struct McpClient<T, R> {
    providers: Vec<(String, ProviderConfig)>,
    has_skill_providers: bool,
    table: Arc<T>,
    router: Arc<R>,
    auth_ctx: Option<Arc<JwtAuthContext>>,
    observer: Option<Arc<dyn ToolObserveCallback>>,
    account_filter: Option<warp::filters::BoxedFilter<(CallerContext,)>>,
    skill_registry: Option<Arc<FilesystemSkillRegistry>>,
}

#[cfg(feature = "mcp")]
impl<T, R> McpClient<T, R>
where
    T: RoutingTable + Send + Sync + 'static,
    R: LanguageModelRouter + Send + Sync + 'static,
{
    pub fn new(
        providers_by_protocol: &HashMap<ApiProtocol, Vec<(String, ProviderConfig)>>,
        table: Arc<T>,
        router: Arc<R>,
    ) -> Self {
        let providers = providers_by_protocol
            .get(&ApiProtocol::Mcp)
            .cloned()
            .unwrap_or_default();
        let has_skill_providers = providers_by_protocol.contains_key(&ApiProtocol::Skill);

        Self {
            providers,
            has_skill_providers,
            table,
            router,
            auth_ctx: None,
            observer: None,
            account_filter: None,
            skill_registry: None,
        }
    }

    pub fn with_auth(mut self, auth_ctx: Arc<JwtAuthContext>) -> Self {
        self.auth_ctx = Some(auth_ctx);
        self
    }

    pub fn with_observe(mut self, observer: Arc<dyn ToolObserveCallback>) -> Self {
        self.observer = Some(observer);
        self
    }

    pub fn with_account_filter(
        mut self,
        filter: impl Filter<Extract = (CallerContext,), Error = warp::Rejection>
        + Clone
        + Send
        + Sync
        + 'static,
    ) -> Self {
        self.account_filter = Some(filter.boxed());
        self
    }

    pub fn with_skill_registry(mut self, registry: Arc<FilesystemSkillRegistry>) -> Self {
        self.skill_registry = Some(registry);
        self
    }

    pub async fn build(self) -> McpRoutes {
        // Convert providers to ToolServerConfigs.
        let mcp_configs: Vec<bitrouter_core::routers::upstream::ToolServerConfig> = self
            .providers
            .iter()
            .filter_map(|(name, p)| {
                bitrouter_config::compat::provider_to_tool_server_config(name, p)
                    .map_err(|e| {
                        tracing::warn!(provider = %name, error = %e, "skipping MCP provider");
                    })
                    .ok()
            })
            .collect();

        // Extract initial filters and restrictions from configs.
        let initial_filters: HashMap<String, ToolFilter> = mcp_configs
            .iter()
            .filter_map(|cfg| cfg.tool_filter.clone().map(|f| (cfg.name.clone(), f)))
            .collect();
        let initial_restrictions: HashMap<String, ParamRestrictions> = mcp_configs
            .iter()
            .filter(|cfg| !cfg.param_restrictions.rules.is_empty())
            .map(|cfg| (cfg.name.clone(), cfg.param_restrictions.clone()))
            .collect();

        // Build the sampling handler so upstream MCP servers can request
        // LLM generation via sampling/createMessage.
        let sampling_handler: Option<Arc<dyn McpClientRequestHandler>> =
            Some(Arc::new(McpSamplingHandler::new(self.table, self.router)));

        // Build all upstream connections.
        let mut connections: HashMap<String, Arc<UpstreamConnection>> =
            HashMap::with_capacity(mcp_configs.len());
        for config in &mcp_configs {
            let name = config.name.clone();
            match UpstreamConnection::connect(config.clone(), sampling_handler.clone()).await {
                Ok(conn) => {
                    connections.insert(name, Arc::new(conn));
                }
                Err(e) => {
                    tracing::warn!(
                        upstream = %name,
                        error = %e,
                        "failed to connect to MCP upstream"
                    );
                }
            }
        }

        let mut guards: Vec<Box<dyn std::any::Any + Send>> = Vec::new();

        let (inner, registry) = if !connections.is_empty() {
            let reg = ConfigMcpRegistry::from_connections(connections.clone());
            tracing::info!("MCP registry started with {} upstreams", connections.len());
            let inner = Arc::new(reg);
            let guard = inner.spawn_refresh_listeners().await;
            guards.push(Box::new(guard));
            let wrapped = Arc::new(DynamicToolRegistry::new(
                Arc::clone(&inner),
                initial_filters,
                initial_restrictions,
            ));
            (Some(inner), Some(wrapped))
        } else {
            (None, None)
        };

        // Build bridge endpoints for servers with `bridge: true`.
        let mut bridge_map: HashMap<String, Arc<SingleServerBridge>> = HashMap::new();
        if let Some(ref reg) = inner {
            for config in mcp_configs.iter().filter(|c| c.bridge) {
                if let Some(conn) = connections.get(&config.name) {
                    let (bridge, guard) = SingleServerBridge::new(
                        Arc::clone(conn),
                        McpToolServer::subscribe_tool_changes(reg.as_ref()),
                        McpResourceServer::subscribe_resource_changes(reg.as_ref()),
                        McpPromptServer::subscribe_prompt_changes(reg.as_ref()),
                    );
                    tracing::info!(server = %config.name, "MCP bridge enabled");
                    bridge_map.insert(config.name.clone(), bridge);
                    guards.push(Box::new(guard));
                }
            }
        }
        let bridge_routes = mcp_admin::mcp_bridge_filter(Arc::new(bridge_map))
            .map(|r| Box::new(r) as Box<dyn warp::Reply>)
            .boxed();

        // Build admin tool routes (gated by management auth when configured).
        let admin_tool_routes = if let Some(ref auth_ctx) = self.auth_ctx {
            auth::auth_gate(auth::management_auth(auth_ctx.clone()))
                .and(admin_tools::admin_tools_filter(registry.clone()))
                .map(|r| Box::new(r) as Box<dyn warp::Reply>)
                .boxed()
        } else {
            admin_tools::admin_tools_filter(registry.clone())
                .map(|r| Box::new(r) as Box<dyn warp::Reply>)
                .boxed()
        };

        // Build MCP server filter with tool-call observation.
        let server = if let (Some(observer), Some(account_filter)) =
            (self.observer, self.account_filter)
        {
            mcp_admin::mcp_server_filter_with_observe(registry.clone(), observer, account_filter)
                .map(|r| Box::new(r) as Box<dyn warp::Reply>)
                .boxed()
        } else {
            mcp_admin::mcp_server_filter(registry.clone())
                .map(|r| Box::new(r) as Box<dyn warp::Reply>)
                .boxed()
        };

        // Compose MCP tools + agentskills into a single ToolRegistry
        // for the unified GET /v1/tools endpoint.
        let tool_list = if let Some(ref mcp_reg) = registry {
            if let Some(ref skill_reg) = self.skill_registry {
                let composite = Arc::new(
                    bitrouter_core::routers::registry::CompositeToolRegistry::new(
                        mcp_reg.clone(),
                        skill_reg.clone(),
                    ),
                );
                tools::tools_filter(Some(composite))
                    .map(|r| Box::new(r) as Box<dyn warp::Reply>)
                    .boxed()
            } else {
                tools::tools_filter(Some(mcp_reg.clone()))
                    .map(|r| Box::new(r) as Box<dyn warp::Reply>)
                    .boxed()
            }
        } else if self.has_skill_providers {
            tools::tools_filter(self.skill_registry)
                .map(|r| Box::new(r) as Box<dyn warp::Reply>)
                .boxed()
        } else {
            tools::tools_filter(registry)
                .map(|r| Box::new(r) as Box<dyn warp::Reply>)
                .boxed()
        };

        McpRoutes {
            admin_tool_routes,
            server,
            tool_list,
            bridge_routes,
            _guards: guards,
        }
    }
}

// ── Sampling handler ─────────────────────────────────────────────

/// Handles MCP server→client requests using bitrouter's LLM routing.
#[cfg(feature = "mcp")]
struct McpSamplingHandler<R, M> {
    routing_table: Arc<R>,
    model_router: Arc<M>,
}

#[cfg(feature = "mcp")]
impl<R, M> McpSamplingHandler<R, M>
where
    R: RoutingTable + Send + Sync + 'static,
    M: LanguageModelRouter + Send + Sync + 'static,
{
    fn new(routing_table: Arc<R>, model_router: Arc<M>) -> Self {
        Self {
            routing_table,
            model_router,
        }
    }

    /// Try to resolve model hints to a routing target.
    ///
    /// Iterates through hints and tries substring matching against available
    /// routes. Returns an error if no hint matches.
    async fn resolve_model(
        &self,
        params: &CreateMessageParams,
    ) -> Result<RoutingTarget, JsonRpcError> {
        let routes = self.routing_table.list_routes();

        if let Some(ref prefs) = params.model_preferences
            && let Some(ref hints) = prefs.hints
        {
            for hint in hints {
                if let Some(target) = match_hint(&hint.name, &routes) {
                    return Ok(target);
                }
            }
        }

        // No hints matched — try routing the first hint as a direct model name.
        if let Some(ref prefs) = params.model_preferences
            && let Some(ref hints) = prefs.hints
            && let Some(first) = hints.first()
            && let Ok(target) = self.routing_table.route(&first.name).await
        {
            return Ok(target);
        }

        Err(JsonRpcError {
            code: -1,
            message: "no model hints matched any configured route".to_owned(),
            data: None,
        })
    }
}

#[cfg(feature = "mcp")]
impl<R, M> McpClientRequestHandler for McpSamplingHandler<R, M>
where
    R: RoutingTable + Send + Sync + 'static,
    M: LanguageModelRouter + Send + Sync + 'static,
{
    fn handle_sampling(
        &self,
        server_name: &str,
        params: CreateMessageParams,
    ) -> Pin<
        Box<
            dyn std::future::Future<Output = Result<CreateMessageResult, JsonRpcError>> + Send + '_,
        >,
    > {
        let server_name = server_name.to_owned();
        Box::pin(async move {
            tracing::info!(
                upstream = %server_name,
                "handling sampling/createMessage request"
            );

            // 1. Resolve model from hints.
            let target = self.resolve_model(&params).await?;
            let model_id = target.model_id.clone();

            // 2. Instantiate the model.
            let model = self
                .model_router
                .route_model(target)
                .await
                .map_err(|e| JsonRpcError {
                    code: -32603,
                    message: format!("failed to route model: {e}"),
                    data: None,
                })?;

            // 3. Convert MCP messages to bitrouter prompt.
            let mut prompt = Vec::new();

            // Prepend system prompt if provided.
            if let Some(ref system) = params.system_prompt {
                prompt.push(LanguageModelMessage::System {
                    content: system.clone(),
                    provider_options: None,
                });
            }

            for msg in &params.messages {
                prompt.push(convert_sampling_message(msg)?);
            }

            // 4. Convert tools.
            let tools = params.tools.as_ref().map(|defs| {
                defs.iter()
                    .map(|t| LanguageModelTool::Function {
                        name: t.name.clone(),
                        description: t.description.clone(),
                        input_schema: json_value_to_schema(t.input_schema.clone()),
                        input_examples: Vec::new(),
                        strict: None,
                        provider_options: None,
                    })
                    .collect()
            });

            // 5. Convert tool choice.
            let tool_choice = params
                .tool_choice
                .as_ref()
                .map(|tc| match tc.mode.as_str() {
                    "required" => LanguageModelToolChoice::Required,
                    "none" => LanguageModelToolChoice::None,
                    _ => LanguageModelToolChoice::Auto,
                });

            // 6. Build call options.
            let options = LanguageModelCallOptions {
                prompt,
                stream: Some(false),
                max_output_tokens: Some(params.max_tokens),
                temperature: None,
                top_p: None,
                top_k: None,
                stop_sequences: None,
                presence_penalty: None,
                frequency_penalty: None,
                response_format: None,
                seed: None,
                tools,
                tool_choice,
                include_raw_chunks: None,
                abort_signal: None,
                headers: None,
                provider_options: None,
            };

            // 7. Call the LLM.
            let result = model.generate(options).await.map_err(|e| JsonRpcError {
                code: -32603,
                message: format!("LLM generation failed: {e}"),
                data: None,
            })?;

            // 8. Convert result back to MCP format.
            let (content, stop_reason) =
                convert_generate_result(result.content, result.finish_reason);

            Ok(CreateMessageResult {
                role: McpRole::Assistant,
                content,
                model: model_id,
                stop_reason: Some(stop_reason),
            })
        })
    }

    fn handle_elicitation(
        &self,
        server_name: &str,
        _params: ElicitationCreateParams,
    ) -> Pin<
        Box<
            dyn std::future::Future<Output = Result<ElicitationCreateResult, JsonRpcError>>
                + Send
                + '_,
        >,
    > {
        let server_name = server_name.to_owned();
        Box::pin(async move {
            tracing::debug!(
                upstream = %server_name,
                "declining elicitation/create request"
            );
            Ok(ElicitationCreateResult {
                action: "decline".to_owned(),
                content: None,
            })
        })
    }
}

// ── Conversion helpers ────────────────────────────────────────────

/// Convert a `serde_json::Value` to a `JsonSchema`.
#[cfg(feature = "mcp")]
fn json_value_to_schema(value: serde_json::Value) -> JsonSchema {
    match value {
        serde_json::Value::Object(map) => JsonSchema::from(map),
        serde_json::Value::Bool(b) => JsonSchema::from(b),
        _ => JsonSchema::from(true),
    }
}

/// Try substring matching a hint against available routes.
#[cfg(feature = "mcp")]
fn match_hint(hint: &str, routes: &[RouteEntry]) -> Option<RoutingTarget> {
    // Try matching against model names first.
    for route in routes {
        if route.model.contains(hint) || hint.contains(&route.model) {
            return Some(RoutingTarget {
                provider_name: route.provider.clone(),
                model_id: route.model.clone(),
            });
        }
    }
    None
}

/// Convert an MCP SamplingMessage to a bitrouter LanguageModelMessage.
#[cfg(feature = "mcp")]
fn convert_sampling_message(
    msg: &bitrouter_core::api::mcp::types::SamplingMessage,
) -> Result<LanguageModelMessage, JsonRpcError> {
    let contents = match &msg.content {
        SamplingContentOrArray::Single(c) => vec![c.clone()],
        SamplingContentOrArray::Array(arr) => arr.clone(),
    };

    match msg.role {
        McpRole::User => {
            // Check if all contents are tool results.
            let all_tool_results = contents
                .iter()
                .all(|c| matches!(c, SamplingContent::ToolResult { .. }));

            if all_tool_results && !contents.is_empty() {
                let tool_results = contents
                    .into_iter()
                    .filter_map(|c| {
                        if let SamplingContent::ToolResult {
                            tool_use_id,
                            content,
                        } = c
                        {
                            let output = content
                                .map(|blocks| {
                                    let texts: Vec<String> = blocks
                                        .into_iter()
                                        .filter_map(|b| {
                                            if let SamplingContent::Text { text } = b {
                                                Some(text)
                                            } else {
                                                None
                                            }
                                        })
                                        .collect();
                                    texts.join("\n")
                                })
                                .unwrap_or_default();

                            Some(LanguageModelToolResult::ToolResult {
                                tool_call_id: tool_use_id,
                                tool_name: String::new(),
                                output: LanguageModelToolResultOutput::Text {
                                    value: output,
                                    provider_options: None,
                                },
                                provider_options: None,
                            })
                        } else {
                            None
                        }
                    })
                    .collect();

                return Ok(LanguageModelMessage::Tool {
                    content: tool_results,
                    provider_options: None,
                });
            }

            let user_content = contents
                .into_iter()
                .map(convert_to_user_content)
                .collect::<Result<Vec<_>, _>>()?;

            Ok(LanguageModelMessage::User {
                content: user_content,
                provider_options: None,
            })
        }
        McpRole::Assistant => {
            let assistant_content = contents
                .into_iter()
                .map(convert_to_assistant_content)
                .collect::<Result<Vec<_>, _>>()?;

            Ok(LanguageModelMessage::Assistant {
                content: assistant_content,
                provider_options: None,
            })
        }
    }
}

#[cfg(feature = "mcp")]
fn convert_to_user_content(
    content: SamplingContent,
) -> Result<LanguageModelUserContent, JsonRpcError> {
    match content {
        SamplingContent::Text { text } => Ok(LanguageModelUserContent::Text {
            text,
            provider_options: None,
        }),
        SamplingContent::Image { data, mime_type } => Ok(LanguageModelUserContent::File {
            filename: None,
            data: LanguageModelDataContent::String(data),
            media_type: mime_type,
            provider_options: None,
        }),
        SamplingContent::Audio { data, mime_type } => Ok(LanguageModelUserContent::File {
            filename: None,
            data: LanguageModelDataContent::String(data),
            media_type: mime_type,
            provider_options: None,
        }),
        other => Err(JsonRpcError {
            code: -32602,
            message: format!("unexpected content type in user message: {other:?}"),
            data: None,
        }),
    }
}

#[cfg(feature = "mcp")]
fn convert_to_assistant_content(
    content: SamplingContent,
) -> Result<LanguageModelAssistantContent, JsonRpcError> {
    match content {
        SamplingContent::Text { text } => Ok(LanguageModelAssistantContent::Text {
            text,
            provider_options: None,
        }),
        SamplingContent::ToolUse { id, name, input } => {
            Ok(LanguageModelAssistantContent::ToolCall {
                tool_call_id: id,
                tool_name: name,
                input: serde_json::from_value(input).unwrap_or_default(),
                provider_executed: None,
                provider_options: None,
            })
        }
        other => Err(JsonRpcError {
            code: -32602,
            message: format!("unexpected content type in assistant message: {other:?}"),
            data: None,
        }),
    }
}

/// Convert a bitrouter LLM result back to MCP format.
#[cfg(feature = "mcp")]
fn convert_generate_result(
    content: LanguageModelContent,
    finish_reason: LanguageModelFinishReason,
) -> (SamplingContent, String) {
    let stop_reason = match finish_reason {
        LanguageModelFinishReason::Stop => "endTurn",
        LanguageModelFinishReason::Length => "maxTokens",
        LanguageModelFinishReason::FunctionCall => "toolUse",
        LanguageModelFinishReason::ContentFilter => "endTurn",
        LanguageModelFinishReason::Error => "endTurn",
        LanguageModelFinishReason::Other(_) => "endTurn",
    };

    let sampling_content = match content {
        LanguageModelContent::Text { text, .. } => SamplingContent::Text { text },
        LanguageModelContent::ToolCall {
            tool_call_id,
            tool_name,
            tool_input,
            ..
        } => SamplingContent::ToolUse {
            id: tool_call_id,
            name: tool_name,
            input: serde_json::from_str(&tool_input)
                .unwrap_or(serde_json::Value::Object(serde_json::Map::new())),
        },
        _ => SamplingContent::Text {
            text: String::new(),
        },
    };

    (sampling_content, stop_reason.to_owned())
}
