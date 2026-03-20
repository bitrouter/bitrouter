//! Conversion between OpenAI Chat Completions format and core LanguageModel types.

use std::collections::HashMap;

use bitrouter_core::models::{
    language::{
        call_options::LanguageModelCallOptions,
        content::LanguageModelContent,
        finish_reason::LanguageModelFinishReason,
        generate_result::LanguageModelGenerateResult,
        prompt::{
            LanguageModelAssistantContent, LanguageModelMessage, LanguageModelToolResult,
            LanguageModelToolResultOutput, LanguageModelUserContent,
        },
        stream_part::LanguageModelStreamPart,
        tool::LanguageModelTool,
        tool_choice::LanguageModelToolChoice,
    },
    shared::types::JsonSchema,
};

use super::types::*;
use crate::util::{generate_id, now_unix};

/// Extracts the model name from a chat completion request body.
pub fn extract_model_name(request: &ChatCompletionRequest) -> &str {
    &request.model
}

/// Converts a [`ChatCompletionRequest`] into [`LanguageModelCallOptions`].
pub fn to_call_options(request: ChatCompletionRequest) -> LanguageModelCallOptions {
    let prompt = request.messages.into_iter().map(convert_message).collect();

    let tools = request.tools.map(|tools| {
        tools
            .into_iter()
            .map(|t| {
                let schema_value = t.function.parameters.unwrap_or(serde_json::json!({}));
                let input_schema: JsonSchema =
                    serde_json::from_value(schema_value).unwrap_or_default();
                LanguageModelTool::Function {
                    name: t.function.name,
                    description: t.function.description,
                    input_schema,
                    input_examples: vec![],
                    strict: t.function.strict,
                    provider_options: None,
                }
            })
            .collect()
    });

    let tool_choice = request.tool_choice.and_then(convert_tool_choice);

    LanguageModelCallOptions {
        prompt,
        stream: request.stream,
        max_output_tokens: request.max_completion_tokens.or(request.max_tokens),
        temperature: request.temperature,
        top_p: request.top_p,
        top_k: None,
        stop_sequences: request.stop,
        presence_penalty: request.presence_penalty,
        frequency_penalty: request.frequency_penalty,
        response_format: None,
        seed: request.seed,
        tools,
        tool_choice,
        include_raw_chunks: None,
        abort_signal: None,
        headers: None,
        provider_options: None,
    }
}

/// Converts a [`LanguageModelGenerateResult`] into a [`ChatCompletionResponse`].
pub fn from_generate_result(
    model_id: &str,
    result: LanguageModelGenerateResult,
) -> ChatCompletionResponse {
    let (content, tool_calls) = extract_content_and_tool_calls(&result.content);
    let finish_reason = map_finish_reason(&result.finish_reason);
    let prompt_tokens = result.usage.input_tokens.total.unwrap_or(0);
    let completion_tokens = result.usage.output_tokens.total.unwrap_or(0);

    ChatCompletionResponse {
        id: format!("chatcmpl-{}", generate_id()),
        object: "chat.completion".to_owned(),
        created: now_unix(),
        model: model_id.to_owned(),
        choices: vec![ChatCompletionChoice {
            index: 0,
            message: ChatCompletionChoiceMessage {
                role: "assistant".to_owned(),
                content,
                tool_calls,
            },
            finish_reason: Some(finish_reason),
        }],
        usage: Some(ChatCompletionUsage {
            prompt_tokens,
            completion_tokens,
            total_tokens: prompt_tokens + completion_tokens,
        }),
    }
}

// ── Streaming ───────────────────────────────────────────────────────────────

/// Stateful converter that tracks tool-call indices across streaming events.
pub struct StreamConverter {
    model_id: String,
    stream_id: String,
    tool_id_to_index: HashMap<String, u32>,
    next_tool_index: u32,
}

impl StreamConverter {
    pub fn new(model_id: String, stream_id: String) -> Self {
        Self {
            model_id,
            stream_id,
            tool_id_to_index: HashMap::new(),
            next_tool_index: 0,
        }
    }

    /// Converts a [`LanguageModelStreamPart`] into a [`ChatCompletionChunk`].
    pub fn convert(&mut self, part: &LanguageModelStreamPart) -> Option<ChatCompletionChunk> {
        match part {
            LanguageModelStreamPart::TextDelta { delta, .. } => Some(self.make_chunk(
                ChatCompletionChunkDelta {
                    role: None,
                    content: Some(delta.clone()),
                    tool_calls: None,
                },
                None,
                None,
            )),
            LanguageModelStreamPart::ToolInputStart { id, tool_name, .. } => {
                let index = self.next_tool_index;
                self.tool_id_to_index.insert(id.clone(), index);
                self.next_tool_index += 1;
                Some(self.make_chunk(
                    ChatCompletionChunkDelta {
                        role: None,
                        content: None,
                        tool_calls: Some(vec![ChatResponseToolCallDelta {
                            index,
                            id: Some(id.clone()),
                            r#type: Some("function".to_owned()),
                            function: Some(ChatResponseToolCallDeltaFunction {
                                name: Some(tool_name.clone()),
                                arguments: None,
                            }),
                        }]),
                    },
                    None,
                    None,
                ))
            }
            LanguageModelStreamPart::ToolInputDelta { id, delta, .. } => {
                let index = self
                    .tool_id_to_index
                    .get(id)
                    .copied()
                    .unwrap_or(self.next_tool_index);
                Some(self.make_chunk(
                    ChatCompletionChunkDelta {
                        role: None,
                        content: None,
                        tool_calls: Some(vec![ChatResponseToolCallDelta {
                            index,
                            id: None,
                            r#type: None,
                            function: Some(ChatResponseToolCallDeltaFunction {
                                name: None,
                                arguments: Some(delta.clone()),
                            }),
                        }]),
                    },
                    None,
                    None,
                ))
            }
            LanguageModelStreamPart::ToolCall {
                tool_call_id,
                tool_name,
                tool_input,
                ..
            } => {
                let index = self.next_tool_index;
                self.next_tool_index += 1;
                Some(self.make_chunk(
                    ChatCompletionChunkDelta {
                        role: None,
                        content: None,
                        tool_calls: Some(vec![ChatResponseToolCallDelta {
                            index,
                            id: Some(tool_call_id.clone()),
                            r#type: Some("function".to_owned()),
                            function: Some(ChatResponseToolCallDeltaFunction {
                                name: Some(tool_name.clone()),
                                arguments: Some(tool_input.clone()),
                            }),
                        }]),
                    },
                    None,
                    None,
                ))
            }
            LanguageModelStreamPart::Finish {
                finish_reason,
                usage,
                ..
            } => {
                let prompt_tokens = usage.input_tokens.total.unwrap_or(0);
                let completion_tokens = usage.output_tokens.total.unwrap_or(0);
                Some(self.make_chunk(
                    ChatCompletionChunkDelta {
                        role: None,
                        content: None,
                        tool_calls: None,
                    },
                    Some(map_finish_reason(finish_reason)),
                    Some(ChatCompletionUsage {
                        prompt_tokens,
                        completion_tokens,
                        total_tokens: prompt_tokens + completion_tokens,
                    }),
                ))
            }
            _ => None,
        }
    }

    fn make_chunk(
        &self,
        delta: ChatCompletionChunkDelta,
        finish_reason: Option<String>,
        usage: Option<ChatCompletionUsage>,
    ) -> ChatCompletionChunk {
        ChatCompletionChunk {
            id: self.stream_id.clone(),
            object: "chat.completion.chunk".to_owned(),
            created: now_unix(),
            model: self.model_id.clone(),
            choices: vec![ChatCompletionChunkChoice {
                index: 0,
                delta,
                finish_reason,
            }],
            usage,
        }
    }
}

// ── Helpers ─────────────────────────────────────────────────────────────────

fn convert_message(msg: ChatMessage) -> LanguageModelMessage {
    match msg.role.as_str() {
        "system" => LanguageModelMessage::System {
            content: content_to_string(msg.content),
            provider_options: None,
        },
        "assistant" => {
            let mut content: Vec<LanguageModelAssistantContent> = Vec::new();
            let text = content_to_string(msg.content);
            if !text.is_empty() {
                content.push(LanguageModelAssistantContent::Text {
                    text,
                    provider_options: None,
                });
            }
            if let Some(tool_calls) = msg.tool_calls {
                for tc in tool_calls {
                    let input: serde_json::Value =
                        serde_json::from_str(&tc.function.arguments).unwrap_or_default();
                    content.push(LanguageModelAssistantContent::ToolCall {
                        tool_call_id: tc.id,
                        tool_name: tc.function.name,
                        input,
                        provider_executed: None,
                        provider_options: None,
                    });
                }
            }
            LanguageModelMessage::Assistant {
                content,
                provider_options: None,
            }
        }
        "tool" => {
            let tool_call_id = msg.tool_call_id.unwrap_or_default();
            let tool_name = msg.name.unwrap_or_default();
            let output_text = content_to_string(msg.content);
            LanguageModelMessage::Tool {
                content: vec![LanguageModelToolResult::ToolResult {
                    tool_call_id,
                    tool_name,
                    output: LanguageModelToolResultOutput::Text {
                        value: output_text,
                        provider_options: None,
                    },
                    provider_options: None,
                }],
                provider_options: None,
            }
        }
        _ => LanguageModelMessage::User {
            content: content_to_parts(msg.content),
            provider_options: None,
        },
    }
}

fn convert_tool_choice(value: serde_json::Value) -> Option<LanguageModelToolChoice> {
    match &value {
        serde_json::Value::String(s) => match s.as_str() {
            "auto" => Some(LanguageModelToolChoice::Auto),
            "none" => Some(LanguageModelToolChoice::None),
            "required" => Some(LanguageModelToolChoice::Required),
            _ => None,
        },
        serde_json::Value::Object(obj) => {
            let name = obj
                .get("function")
                .and_then(|f| f.get("name"))
                .and_then(|n| n.as_str())
                .map(|s| s.to_owned());
            name.map(|tool_name| LanguageModelToolChoice::Tool { tool_name })
        }
        _ => None,
    }
}

fn extract_content_and_tool_calls(
    content: &LanguageModelContent,
) -> (Option<String>, Option<Vec<ChatResponseToolCall>>) {
    match content {
        LanguageModelContent::Text { text, .. } => (Some(text.clone()), None),
        LanguageModelContent::ToolCall {
            tool_call_id,
            tool_name,
            tool_input,
            ..
        } => (
            None,
            Some(vec![ChatResponseToolCall {
                id: tool_call_id.clone(),
                r#type: "function".to_owned(),
                function: ChatResponseToolCallFunction {
                    name: tool_name.clone(),
                    arguments: tool_input.clone(),
                },
            }]),
        ),
        _ => (None, None),
    }
}

fn content_to_string(content: Option<ChatMessageContent>) -> String {
    match content {
        Some(ChatMessageContent::Text(s)) => s,
        Some(ChatMessageContent::Parts(parts)) => parts
            .into_iter()
            .map(|p| match p {
                ChatContentPart::Text { text } => text,
            })
            .collect::<Vec<_>>()
            .join(""),
        None => String::new(),
    }
}

fn content_to_parts(content: Option<ChatMessageContent>) -> Vec<LanguageModelUserContent> {
    match content {
        Some(ChatMessageContent::Text(s)) => vec![LanguageModelUserContent::Text {
            text: s,
            provider_options: None,
        }],
        Some(ChatMessageContent::Parts(parts)) => parts
            .into_iter()
            .map(|p| match p {
                ChatContentPart::Text { text } => LanguageModelUserContent::Text {
                    text,
                    provider_options: None,
                },
            })
            .collect(),
        None => vec![],
    }
}

fn map_finish_reason(reason: &LanguageModelFinishReason) -> String {
    match reason {
        LanguageModelFinishReason::Stop => "stop".to_owned(),
        LanguageModelFinishReason::Length => "length".to_owned(),
        LanguageModelFinishReason::FunctionCall => "tool_calls".to_owned(),
        LanguageModelFinishReason::ContentFilter => "content_filter".to_owned(),
        LanguageModelFinishReason::Error => "stop".to_owned(),
        LanguageModelFinishReason::Other(_) => "stop".to_owned(),
    }
}
