//! Conversion between Anthropic Messages format and core LanguageModel types.

use std::collections::HashMap;

use crate::models::{
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

use super::types::{
    AnthropicContentBlock, AnthropicMessageContent, MessagesRequest, MessagesResponse,
    MessagesResponseContent, MessagesStreamContentBlock, MessagesStreamDelta, MessagesStreamEvent,
    MessagesStreamMessage, MessagesUsage,
};
use crate::api::util::generate_id;

/// Extracts the model name from a messages request body.
pub fn extract_model_name(request: &MessagesRequest) -> &str {
    &request.model
}

/// Converts a [`MessagesRequest`] into [`LanguageModelCallOptions`].
pub fn to_call_options(request: MessagesRequest) -> LanguageModelCallOptions {
    let mut prompt: Vec<LanguageModelMessage> = Vec::new();

    // Anthropic system message is a top-level field, not part of messages.
    if let Some(system) = request.system {
        prompt.push(LanguageModelMessage::System {
            content: system,
            provider_options: None,
        });
    }

    for msg in request.messages {
        match msg.role.as_str() {
            "assistant" => {
                let content = convert_assistant_content(msg.content);
                prompt.push(LanguageModelMessage::Assistant {
                    content,
                    provider_options: None,
                });
            }
            _ => {
                let (user_parts, tool_results) = split_user_content(msg.content);
                if !tool_results.is_empty() {
                    prompt.push(LanguageModelMessage::Tool {
                        content: tool_results,
                        provider_options: None,
                    });
                }
                if !user_parts.is_empty() {
                    prompt.push(LanguageModelMessage::User {
                        content: user_parts,
                        provider_options: None,
                    });
                }
            }
        }
    }

    let tools = request.tools.map(|tools| {
        tools
            .into_iter()
            .map(|t| {
                let input_schema: JsonSchema =
                    serde_json::from_value(t.input_schema).unwrap_or_default();
                LanguageModelTool::Function {
                    name: t.name,
                    description: t.description,
                    input_schema,
                    input_examples: vec![],
                    strict: None,
                    provider_options: None,
                }
            })
            .collect()
    });

    let tool_choice = request.tool_choice.and_then(convert_tool_choice);

    LanguageModelCallOptions {
        prompt,
        stream: request.stream,
        max_output_tokens: Some(request.max_tokens),
        temperature: request.temperature,
        top_p: request.top_p,
        top_k: request.top_k,
        stop_sequences: request.stop_sequences,
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
    }
}

/// Converts a [`LanguageModelGenerateResult`] into a [`MessagesResponse`].
pub fn from_generate_result(
    model_id: &str,
    result: LanguageModelGenerateResult,
) -> MessagesResponse {
    let content = extract_response_content(&result.content);
    let stop_reason = map_finish_reason(&result.finish_reason);
    let input_tokens = result.usage.input_tokens.total.unwrap_or(0);
    let output_tokens = result.usage.output_tokens.total.unwrap_or(0);

    MessagesResponse {
        id: format!("msg-{}", generate_id()),
        response_type: "message".to_owned(),
        role: "assistant".to_owned(),
        content,
        model: model_id.to_owned(),
        stop_reason: Some(stop_reason),
        stop_sequence: None,
        usage: MessagesUsage {
            input_tokens,
            output_tokens,
        },
    }
}

// ── Streaming ───────────────────────────────────────────────────────────────

/// Stateful converter that tracks content-block indices across streaming events.
pub struct StreamConverter {
    model_id: String,
    tool_id_to_index: HashMap<String, u32>,
    next_block_index: u32,
}

impl StreamConverter {
    pub fn new(model_id: String) -> Self {
        Self {
            model_id,
            tool_id_to_index: HashMap::new(),
            next_block_index: 0,
        }
    }

    /// Converts a [`LanguageModelStreamPart`] into Anthropic SSE events.
    pub fn convert(&mut self, part: &LanguageModelStreamPart) -> Vec<MessagesStreamEvent> {
        match part {
            LanguageModelStreamPart::TextDelta { delta, .. } => {
                vec![MessagesStreamEvent {
                    event_type: "content_block_delta".to_owned(),
                    index: Some(0),
                    delta: Some(MessagesStreamDelta {
                        delta_type: "text_delta".to_owned(),
                        text: Some(delta.clone()),
                        stop_reason: None,
                        partial_json: None,
                    }),
                    message: None,
                    content_block: None,
                }]
            }
            LanguageModelStreamPart::ToolInputStart { id, tool_name, .. } => {
                let index = self.next_block_index;
                self.tool_id_to_index.insert(id.clone(), index);
                self.next_block_index += 1;
                vec![MessagesStreamEvent {
                    event_type: "content_block_start".to_owned(),
                    index: Some(index),
                    delta: None,
                    message: None,
                    content_block: Some(MessagesStreamContentBlock {
                        block_type: "tool_use".to_owned(),
                        id: Some(id.clone()),
                        name: Some(tool_name.clone()),
                        input: Some(serde_json::json!({})),
                        text: None,
                    }),
                }]
            }
            LanguageModelStreamPart::ToolInputDelta { id, delta, .. } => {
                let index = self
                    .tool_id_to_index
                    .get(id)
                    .copied()
                    .unwrap_or(self.next_block_index);
                vec![MessagesStreamEvent {
                    event_type: "content_block_delta".to_owned(),
                    index: Some(index),
                    delta: Some(MessagesStreamDelta {
                        delta_type: "input_json_delta".to_owned(),
                        text: None,
                        stop_reason: None,
                        partial_json: Some(delta.clone()),
                    }),
                    message: None,
                    content_block: None,
                }]
            }
            LanguageModelStreamPart::ToolInputEnd { id, .. } => {
                let index = self
                    .tool_id_to_index
                    .get(id)
                    .copied()
                    .unwrap_or(self.next_block_index);
                vec![MessagesStreamEvent {
                    event_type: "content_block_stop".to_owned(),
                    index: Some(index),
                    delta: None,
                    message: None,
                    content_block: None,
                }]
            }
            LanguageModelStreamPart::ToolCall {
                tool_call_id,
                tool_name,
                tool_input,
                ..
            } => {
                let index = self.next_block_index;
                self.next_block_index += 1;
                vec![
                    MessagesStreamEvent {
                        event_type: "content_block_start".to_owned(),
                        index: Some(index),
                        delta: None,
                        message: None,
                        content_block: Some(MessagesStreamContentBlock {
                            block_type: "tool_use".to_owned(),
                            id: Some(tool_call_id.clone()),
                            name: Some(tool_name.clone()),
                            input: Some(serde_json::json!({})),
                            text: None,
                        }),
                    },
                    MessagesStreamEvent {
                        event_type: "content_block_delta".to_owned(),
                        index: Some(index),
                        delta: Some(MessagesStreamDelta {
                            delta_type: "input_json_delta".to_owned(),
                            text: None,
                            stop_reason: None,
                            partial_json: Some(tool_input.clone()),
                        }),
                        message: None,
                        content_block: None,
                    },
                    MessagesStreamEvent {
                        event_type: "content_block_stop".to_owned(),
                        index: Some(index),
                        delta: None,
                        message: None,
                        content_block: None,
                    },
                ]
            }
            LanguageModelStreamPart::Finish { finish_reason, .. } => {
                vec![MessagesStreamEvent {
                    event_type: "message_delta".to_owned(),
                    index: None,
                    delta: Some(MessagesStreamDelta {
                        delta_type: "message_delta".to_owned(),
                        text: None,
                        stop_reason: Some(map_finish_reason(finish_reason)),
                        partial_json: None,
                    }),
                    message: Some(MessagesStreamMessage {
                        id: format!("msg-{}", generate_id()),
                        model: self.model_id.clone(),
                        role: "assistant".to_owned(),
                    }),
                    content_block: None,
                }]
            }
            _ => vec![],
        }
    }
}

// ── Helpers ─────────────────────────────────────────────────────────────────

fn convert_assistant_content(
    content: Option<AnthropicMessageContent>,
) -> Vec<LanguageModelAssistantContent> {
    match content {
        Some(AnthropicMessageContent::Text(s)) => {
            vec![LanguageModelAssistantContent::Text {
                text: s,
                provider_options: None,
            }]
        }
        Some(AnthropicMessageContent::Blocks(blocks)) => blocks
            .into_iter()
            .filter_map(|b| match b {
                AnthropicContentBlock::Text { text } => Some(LanguageModelAssistantContent::Text {
                    text,
                    provider_options: None,
                }),
                AnthropicContentBlock::ToolUse { id, name, input } => {
                    Some(LanguageModelAssistantContent::ToolCall {
                        tool_call_id: id,
                        tool_name: name,
                        input,
                        provider_executed: None,
                        provider_options: None,
                    })
                }
                AnthropicContentBlock::ToolResult { .. } => None,
            })
            .collect(),
        None => vec![],
    }
}

fn split_user_content(
    content: Option<AnthropicMessageContent>,
) -> (Vec<LanguageModelUserContent>, Vec<LanguageModelToolResult>) {
    match content {
        Some(AnthropicMessageContent::Text(s)) => (
            vec![LanguageModelUserContent::Text {
                text: s,
                provider_options: None,
            }],
            vec![],
        ),
        Some(AnthropicMessageContent::Blocks(blocks)) => {
            let mut user_parts = Vec::new();
            let mut tool_results = Vec::new();
            for block in blocks {
                match block {
                    AnthropicContentBlock::Text { text } => {
                        user_parts.push(LanguageModelUserContent::Text {
                            text,
                            provider_options: None,
                        });
                    }
                    AnthropicContentBlock::ToolResult {
                        tool_use_id,
                        content,
                    } => {
                        tool_results.push(LanguageModelToolResult::ToolResult {
                            tool_call_id: tool_use_id,
                            tool_name: String::new(),
                            output: LanguageModelToolResultOutput::Text {
                                value: content.unwrap_or_default(),
                                provider_options: None,
                            },
                            provider_options: None,
                        });
                    }
                    AnthropicContentBlock::ToolUse { .. } => {}
                }
            }
            (user_parts, tool_results)
        }
        None => (vec![], vec![]),
    }
}

fn convert_tool_choice(value: serde_json::Value) -> Option<LanguageModelToolChoice> {
    let obj = value.as_object()?;
    let tc_type = obj.get("type")?.as_str()?;
    match tc_type {
        "auto" => Some(LanguageModelToolChoice::Auto),
        "any" => Some(LanguageModelToolChoice::Required),
        "tool" => {
            let name = obj.get("name")?.as_str()?.to_owned();
            Some(LanguageModelToolChoice::Tool { tool_name: name })
        }
        _ => None,
    }
}

fn extract_response_content(content: &LanguageModelContent) -> Vec<MessagesResponseContent> {
    match content {
        LanguageModelContent::Text { text, .. } => {
            vec![MessagesResponseContent::Text { text: text.clone() }]
        }
        LanguageModelContent::ToolCall {
            tool_call_id,
            tool_name,
            tool_input,
            ..
        } => {
            let input: serde_json::Value = serde_json::from_str(tool_input).unwrap_or_default();
            vec![MessagesResponseContent::ToolUse {
                id: tool_call_id.clone(),
                name: tool_name.clone(),
                input,
            }]
        }
        _ => vec![],
    }
}

fn map_finish_reason(reason: &LanguageModelFinishReason) -> String {
    match reason {
        LanguageModelFinishReason::Stop => "end_turn".to_owned(),
        LanguageModelFinishReason::Length => "max_tokens".to_owned(),
        LanguageModelFinishReason::FunctionCall => "tool_use".to_owned(),
        LanguageModelFinishReason::ContentFilter => "end_turn".to_owned(),
        LanguageModelFinishReason::Error => "end_turn".to_owned(),
        LanguageModelFinishReason::Other(_) => "end_turn".to_owned(),
    }
}
