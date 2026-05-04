//! Conversion between Anthropic Messages format and core LanguageModel types.

use std::collections::{HashMap, HashSet};

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
    AnthropicContentBlock, AnthropicMessageContent, AnthropicToolChoice, MessagesMessageDelta,
    MessagesRequest, MessagesResponse, MessagesStreamDelta, MessagesStreamEvent,
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
    // Accept both plain string and array-of-content-blocks formats.
    if let Some(system) = request.system {
        prompt.push(LanguageModelMessage::System {
            content: system.into_text(),
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

    let tool_choice = request.tool_choice.as_ref().and_then(convert_tool_choice);

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
        usage: Some(MessagesUsage {
            input_tokens: Some(input_tokens),
            output_tokens: Some(output_tokens),
            cache_creation_input_tokens: None,
            cache_read_input_tokens: None,
        }),
    }
}

// ── Streaming ───────────────────────────────────────────────────────────────

/// Stateful converter that tracks content-block indices across streaming events.
pub struct StreamConverter {
    model_id: String,
    message_id: String,
    message_started: bool,
    message_stopped: bool,
    active_text_block_index: Option<u32>,
    tool_id_to_index: HashMap<String, u32>,
    started_tool_ids: HashSet<String>,
    closed_tool_ids: HashSet<String>,
    next_block_index: u32,
}

impl StreamConverter {
    pub fn new(model_id: String) -> Self {
        Self {
            model_id,
            message_id: format!("msg-{}", generate_id()),
            message_started: false,
            message_stopped: false,
            active_text_block_index: None,
            tool_id_to_index: HashMap::new(),
            started_tool_ids: HashSet::new(),
            closed_tool_ids: HashSet::new(),
            next_block_index: 0,
        }
    }

    /// Converts a [`LanguageModelStreamPart`] into Anthropic SSE events.
    pub fn convert(&mut self, part: &LanguageModelStreamPart) -> Vec<MessagesStreamEvent> {
        match part {
            LanguageModelStreamPart::StreamStart { .. }
            | LanguageModelStreamPart::ResponseMetadata { .. } => self.start_message_events(),
            LanguageModelStreamPart::TextStart { .. } => self.start_text_events(),
            LanguageModelStreamPart::TextDelta { delta, .. } => {
                let mut events = self.start_text_events();
                let index = self.active_text_block_index.unwrap_or(0);
                events.push(MessagesStreamEvent::ContentBlockDelta {
                    index,
                    delta: MessagesStreamDelta::TextDelta {
                        text: delta.clone(),
                    },
                });
                events
            }
            LanguageModelStreamPart::TextEnd { .. } => self.stop_text_events(),
            LanguageModelStreamPart::Error { error } => {
                let mut events = self.start_message_events();
                self.message_stopped = true;
                events.push(MessagesStreamEvent::Error {
                    error: crate::api::anthropic::messages::types::MessagesStreamError {
                        error_type: "provider_error".to_owned(),
                        message: error.to_string(),
                    },
                });
                events
            }
            LanguageModelStreamPart::ToolInputStart { id, tool_name, .. } => {
                self.start_tool_events(id, tool_name)
            }
            LanguageModelStreamPart::ToolInputDelta { id, delta, .. } => {
                if self.closed_tool_ids.contains(id) {
                    return Vec::new();
                }

                let mut events = self.start_tool_events(id, "tool");
                let Some(index) = self.tool_id_to_index.get(id).copied() else {
                    return events;
                };
                events.push(MessagesStreamEvent::ContentBlockDelta {
                    index,
                    delta: MessagesStreamDelta::InputJsonDelta {
                        partial_json: delta.clone(),
                    },
                });
                events
            }
            LanguageModelStreamPart::ToolInputEnd { id, .. } => {
                if self.closed_tool_ids.contains(id) {
                    return Vec::new();
                }

                let mut events = self.start_tool_events(id, "tool");
                if let Some(index) = self.tool_id_to_index.get(id).copied() {
                    events.push(MessagesStreamEvent::ContentBlockStop { index });
                    self.closed_tool_ids.insert(id.clone());
                }
                events
            }
            LanguageModelStreamPart::ToolCall {
                tool_call_id,
                tool_name,
                tool_input,
                ..
            } => {
                let mut events = self.stop_text_events();
                let index = self.next_block_index;
                self.next_block_index += 1;
                events.extend([
                    MessagesStreamEvent::ContentBlockStart {
                        index,
                        content_block: AnthropicContentBlock::ToolUse {
                            id: tool_call_id.clone(),
                            name: tool_name.clone(),
                            input: serde_json::json!({}),
                        },
                    },
                    MessagesStreamEvent::ContentBlockDelta {
                        index,
                        delta: MessagesStreamDelta::InputJsonDelta {
                            partial_json: tool_input.clone(),
                        },
                    },
                    MessagesStreamEvent::ContentBlockStop { index },
                ]);
                events
            }
            LanguageModelStreamPart::Finish {
                finish_reason,
                usage,
                ..
            } => {
                let mut events = self.stop_text_events();
                events.push(MessagesStreamEvent::MessageDelta {
                    delta: MessagesMessageDelta {
                        delta_type: "message_delta".to_owned(),
                        stop_reason: Some(map_finish_reason(finish_reason)),
                        stop_sequence: None,
                    },
                    usage: Some(MessagesUsage {
                        input_tokens: Some(usage.input_tokens.total.unwrap_or(0)),
                        output_tokens: Some(usage.output_tokens.total.unwrap_or(0)),
                        cache_creation_input_tokens: usage.input_tokens.cache_write,
                        cache_read_input_tokens: usage.input_tokens.cache_read,
                    }),
                    message: Some(MessagesStreamMessage {
                        id: self.message_id.clone(),
                        model: self.model_id.clone(),
                        role: "assistant".to_owned(),
                    }),
                });
                events.push(MessagesStreamEvent::MessageStop);
                self.message_stopped = true;
                events
            }
            _ => vec![],
        }
    }

    /// Emits terminal Anthropic stream events when an upstream closes cleanly
    /// without a final finish part.
    pub fn finish(&mut self) -> Vec<MessagesStreamEvent> {
        if self.message_stopped {
            return Vec::new();
        }

        let mut events = self.stop_text_events();
        events.push(MessagesStreamEvent::MessageDelta {
            delta: MessagesMessageDelta {
                delta_type: "message_delta".to_owned(),
                stop_reason: Some("end_turn".to_owned()),
                stop_sequence: None,
            },
            usage: Some(empty_stream_usage()),
            message: Some(MessagesStreamMessage {
                id: self.message_id.clone(),
                model: self.model_id.clone(),
                role: "assistant".to_owned(),
            }),
        });
        events.push(MessagesStreamEvent::MessageStop);
        self.message_stopped = true;
        events
    }

    fn start_message_events(&mut self) -> Vec<MessagesStreamEvent> {
        if self.message_started {
            return Vec::new();
        }

        self.message_started = true;
        vec![MessagesStreamEvent::MessageStart {
            message: MessagesResponse {
                id: self.message_id.clone(),
                response_type: "message".to_owned(),
                role: "assistant".to_owned(),
                content: Vec::new(),
                model: self.model_id.clone(),
                stop_reason: None,
                stop_sequence: None,
                usage: Some(empty_stream_usage()),
            },
        }]
    }

    fn start_text_events(&mut self) -> Vec<MessagesStreamEvent> {
        let mut events = self.start_message_events();
        if self.active_text_block_index.is_none() {
            let index = self.next_block_index;
            self.next_block_index += 1;
            self.active_text_block_index = Some(index);
            events.push(MessagesStreamEvent::ContentBlockStart {
                index,
                content_block: AnthropicContentBlock::Text {
                    text: String::new(),
                },
            });
        }
        events
    }

    fn stop_text_events(&mut self) -> Vec<MessagesStreamEvent> {
        let mut events = self.start_message_events();
        if let Some(index) = self.active_text_block_index.take() {
            events.push(MessagesStreamEvent::ContentBlockStop { index });
        }
        events
    }

    fn start_tool_events(&mut self, id: &str, tool_name: &str) -> Vec<MessagesStreamEvent> {
        if self.closed_tool_ids.contains(id) {
            return Vec::new();
        }

        let mut events = self.stop_text_events();
        if self.started_tool_ids.contains(id) {
            return events;
        }

        let index = self.next_block_index;
        self.tool_id_to_index.insert(id.to_owned(), index);
        self.started_tool_ids.insert(id.to_owned());
        self.next_block_index += 1;
        events.push(MessagesStreamEvent::ContentBlockStart {
            index,
            content_block: AnthropicContentBlock::ToolUse {
                id: id.to_owned(),
                name: tool_name.to_owned(),
                input: serde_json::json!({}),
            },
        });
        events
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
                // Thinking blocks are echoed back to BitRouter on multi-turn
                // requests but not currently propagated to providers.
                AnthropicContentBlock::ToolResult { .. }
                | AnthropicContentBlock::Image { .. }
                | AnthropicContentBlock::Thinking { .. }
                | AnthropicContentBlock::RedactedThinking { .. } => None,
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
                        ..
                    } => {
                        tool_results.push(LanguageModelToolResult::ToolResult {
                            tool_call_id: tool_use_id,
                            tool_name: String::new(),
                            output: LanguageModelToolResultOutput::Text {
                                value: content.map(|c| c.into_text()).unwrap_or_default(),
                                provider_options: None,
                            },
                            provider_options: None,
                        });
                    }
                    AnthropicContentBlock::ToolUse { .. }
                    | AnthropicContentBlock::Image { .. }
                    | AnthropicContentBlock::Thinking { .. }
                    | AnthropicContentBlock::RedactedThinking { .. } => {}
                }
            }
            (user_parts, tool_results)
        }
        None => (vec![], vec![]),
    }
}

fn convert_tool_choice(value: &AnthropicToolChoice) -> Option<LanguageModelToolChoice> {
    match value {
        AnthropicToolChoice::Auto => Some(LanguageModelToolChoice::Auto),
        AnthropicToolChoice::Any => Some(LanguageModelToolChoice::Required),
        AnthropicToolChoice::Tool { name } => Some(LanguageModelToolChoice::Tool {
            tool_name: name.clone(),
        }),
    }
}

fn extract_response_content(blocks: &[LanguageModelContent]) -> Vec<AnthropicContentBlock> {
    let mut out: Vec<AnthropicContentBlock> = Vec::with_capacity(blocks.len());
    for block in blocks {
        match block {
            LanguageModelContent::Text { text, .. } => {
                out.push(AnthropicContentBlock::Text { text: text.clone() });
            }
            LanguageModelContent::ToolCall {
                tool_call_id,
                tool_name,
                tool_input,
                ..
            } => {
                let input: serde_json::Value = serde_json::from_str(tool_input).unwrap_or_default();
                out.push(AnthropicContentBlock::ToolUse {
                    id: tool_call_id.clone(),
                    name: tool_name.clone(),
                    input,
                });
            }
            LanguageModelContent::Reasoning { text, .. } => {
                out.push(AnthropicContentBlock::Thinking {
                    thinking: text.clone(),
                    signature: None,
                });
            }
            _ => {}
        }
    }
    out
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

fn empty_stream_usage() -> MessagesUsage {
    MessagesUsage {
        input_tokens: Some(0),
        output_tokens: Some(0),
        cache_creation_input_tokens: None,
        cache_read_input_tokens: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_converter_starts_tool_block_for_delta_without_start() {
        let mut converter = StreamConverter::new("claude-compatible".to_owned());

        let events = converter.convert(&LanguageModelStreamPart::ToolInputDelta {
            id: "call_1".to_owned(),
            delta: r#"{"task":"plan"}"#.to_owned(),
            provider_metadata: None,
        });

        assert!(matches!(
            events.as_slice(),
            [
                MessagesStreamEvent::MessageStart { .. },
                MessagesStreamEvent::ContentBlockStart { index: 0, content_block },
                MessagesStreamEvent::ContentBlockDelta { index: 0, delta },
            ] if matches!(
                content_block,
                AnthropicContentBlock::ToolUse { id, name, .. } if id == "call_1" && name == "tool"
            ) && matches!(
                delta,
                MessagesStreamDelta::InputJsonDelta { partial_json } if partial_json == r#"{"task":"plan"}"#
            )
        ));
    }

    #[test]
    fn stream_converter_ends_tool_block_for_end_without_start() {
        let mut converter = StreamConverter::new("claude-compatible".to_owned());

        let events = converter.convert(&LanguageModelStreamPart::ToolInputEnd {
            id: "call_1".to_owned(),
            provider_metadata: None,
        });

        assert!(matches!(
            events.as_slice(),
            [
                MessagesStreamEvent::MessageStart { .. },
                MessagesStreamEvent::ContentBlockStart { index: 0, content_block },
                MessagesStreamEvent::ContentBlockStop { index: 0 },
            ] if matches!(
                content_block,
                AnthropicContentBlock::ToolUse { id, name, .. } if id == "call_1" && name == "tool"
            )
        ));

        let duplicate_events = converter.convert(&LanguageModelStreamPart::ToolInputEnd {
            id: "call_1".to_owned(),
            provider_metadata: None,
        });
        assert!(duplicate_events.is_empty());
    }

    #[test]
    fn stream_converter_message_start_includes_usage() {
        let mut converter = StreamConverter::new("claude-compatible".to_owned());

        let events = converter.convert(&LanguageModelStreamPart::TextStart {
            id: "text".to_owned(),
            provider_metadata: None,
        });

        assert!(matches!(
            events.first(),
            Some(MessagesStreamEvent::MessageStart { message })
                if message.usage.as_ref().is_some_and(|usage|
                    usage.input_tokens == Some(0) && usage.output_tokens == Some(0)
                )
        ));
    }

    #[test]
    fn stream_converter_finish_includes_token_counts() {
        let mut converter = StreamConverter::new("claude-compatible".to_owned());

        let events = converter.convert(&LanguageModelStreamPart::Finish {
            usage: crate::models::language::usage::LanguageModelUsage {
                input_tokens: crate::models::language::usage::LanguageModelInputTokens {
                    total: None,
                    no_cache: None,
                    cache_read: None,
                    cache_write: None,
                },
                output_tokens: crate::models::language::usage::LanguageModelOutputTokens {
                    total: None,
                    text: None,
                    reasoning: None,
                },
                raw: None,
            },
            finish_reason: LanguageModelFinishReason::Stop,
            provider_metadata: None,
        });

        assert!(events.iter().any(|event| matches!(
            event,
            MessagesStreamEvent::MessageDelta {
                usage: Some(usage),
                ..
            } if usage.input_tokens == Some(0) && usage.output_tokens == Some(0)
        )));
    }
}
