//! MCP client request handler — handles server→client requests
//! (sampling, elicitation) by routing to bitrouter's LLM providers.

use std::pin::Pin;
use std::sync::Arc;

use bitrouter_core::api::mcp::gateway::McpClientRequestHandler;
use bitrouter_core::api::mcp::types::{
    CreateMessageParams, CreateMessageResult, ElicitationCreateParams, ElicitationCreateResult,
    JsonRpcError, McpRole, SamplingContent, SamplingContentOrArray,
};
use bitrouter_core::models::language::call_options::LanguageModelCallOptions;
use bitrouter_core::models::language::content::LanguageModelContent;
use bitrouter_core::models::language::data_content::LanguageModelDataContent;
use bitrouter_core::models::language::finish_reason::LanguageModelFinishReason;
use bitrouter_core::models::language::language_model::LanguageModel;
use bitrouter_core::models::language::prompt::{
    LanguageModelAssistantContent, LanguageModelMessage, LanguageModelToolResult,
    LanguageModelToolResultOutput, LanguageModelUserContent,
};
use bitrouter_core::models::language::tool::LanguageModelTool;
use bitrouter_core::models::language::tool_choice::LanguageModelToolChoice;
use bitrouter_core::models::shared::types::JsonSchema;
use bitrouter_core::routers::model_router::LanguageModelRouter;
use bitrouter_core::routers::routing_table::{RouteEntry, RoutingTable, RoutingTarget};

/// Handles MCP server→client requests using bitrouter's LLM routing.
pub struct McpSamplingHandler<R, M> {
    routing_table: Arc<R>,
    model_router: Arc<M>,
}

impl<R, M> McpSamplingHandler<R, M>
where
    R: RoutingTable + Send + Sync + 'static,
    M: LanguageModelRouter + Send + Sync + 'static,
{
    pub fn new(routing_table: Arc<R>, model_router: Arc<M>) -> Self {
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
fn json_value_to_schema(value: serde_json::Value) -> JsonSchema {
    match value {
        serde_json::Value::Object(map) => JsonSchema::from(map),
        serde_json::Value::Bool(b) => JsonSchema::from(b),
        _ => JsonSchema::from(true),
    }
}

/// Try substring matching a hint against available routes.
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
