//! Conversion between OpenAI Responses format and core LanguageModel types.

use std::collections::HashMap;

use crate::models::{
    language::{
        call_options::LanguageModelCallOptions,
        content::LanguageModelContent,
        generate_result::LanguageModelGenerateResult,
        prompt::{
            LanguageModelMessage, LanguageModelToolResult, LanguageModelToolResultOutput,
            LanguageModelUserContent,
        },
        stream_part::LanguageModelStreamPart,
        tool::LanguageModelTool,
    },
    shared::types::JsonSchema,
};

use super::types::{
    ResponsesInput, ResponsesInputContent, ResponsesInputContentPart, ResponsesInputItem,
    ResponsesOutputContent, ResponsesOutputItem, ResponsesRequest, ResponsesResponse,
    ResponsesStreamEvent, ResponsesUsage,
};
use crate::api::util::{generate_id, now_unix};

/// Extracts the model name from a responses request body.
pub fn extract_model_name(request: &ResponsesRequest) -> &str {
    &request.model
}

/// Converts a [`ResponsesRequest`] into [`LanguageModelCallOptions`].
pub fn to_call_options(request: ResponsesRequest) -> LanguageModelCallOptions {
    let prompt = match request.input {
        ResponsesInput::Text(text) => vec![LanguageModelMessage::User {
            content: vec![LanguageModelUserContent::Text {
                text,
                provider_options: None,
            }],
            provider_options: None,
        }],
        ResponsesInput::Items(items) => convert_input_items(items),
    };

    let tools = request.tools.map(|tools| {
        tools
            .into_iter()
            .filter(|t| t.r#type == "function")
            .filter_map(|t| {
                let name = t.name?;
                let schema_value = t.parameters.unwrap_or(serde_json::json!({}));
                let input_schema: JsonSchema =
                    serde_json::from_value(schema_value).unwrap_or_default();
                Some(LanguageModelTool::Function {
                    name,
                    description: t.description,
                    input_schema,
                    input_examples: vec![],
                    strict: t.strict,
                    provider_options: None,
                })
            })
            .collect()
    });

    LanguageModelCallOptions {
        prompt,
        stream: request.stream,
        max_output_tokens: request.max_output_tokens,
        temperature: request.temperature,
        top_p: request.top_p,
        top_k: None,
        stop_sequences: None,
        presence_penalty: None,
        frequency_penalty: None,
        response_format: None,
        seed: None,
        tools,
        tool_choice: None,
        include_raw_chunks: None,
        abort_signal: None,
        headers: None,
        provider_options: None,
    }
}

/// Converts a [`LanguageModelGenerateResult`] into a [`ResponsesResponse`].
pub fn from_generate_result(
    model_id: &str,
    result: LanguageModelGenerateResult,
) -> ResponsesResponse {
    let output = extract_output_items(&result.content);
    let input_tokens = result.usage.input_tokens.total.unwrap_or(0);
    let output_tokens = result.usage.output_tokens.total.unwrap_or(0);

    ResponsesResponse {
        id: format!("resp-{}", generate_id()),
        object: "response".to_owned(),
        created_at: now_unix(),
        model: model_id.to_owned(),
        output,
        usage: Some(ResponsesUsage {
            input_tokens,
            output_tokens,
            total_tokens: input_tokens + output_tokens,
        }),
        status: "completed".to_owned(),
    }
}

// ── Streaming ───────────────────────────────────────────────────────────────

/// Stateful converter that tracks output item indices across streaming events.
pub struct StreamConverter {
    tool_id_to_index: HashMap<String, u32>,
    next_output_index: u32,
}

impl Default for StreamConverter {
    fn default() -> Self {
        Self::new()
    }
}

impl StreamConverter {
    pub fn new() -> Self {
        Self {
            tool_id_to_index: HashMap::new(),
            next_output_index: 0,
        }
    }

    /// Converts a [`LanguageModelStreamPart`] into Responses SSE events.
    pub fn convert(&mut self, part: &LanguageModelStreamPart) -> Vec<ResponsesStreamEvent> {
        match part {
            LanguageModelStreamPart::TextDelta { delta, .. } => {
                vec![ResponsesStreamEvent {
                    event_type: "response.output_text.delta".to_owned(),
                    item_id: None,
                    output_index: Some(0),
                    content_index: Some(0),
                    delta: Some(delta.clone()),
                    call_id: None,
                    name: None,
                    arguments: None,
                    item: None,
                }]
            }
            LanguageModelStreamPart::ToolCall {
                tool_call_id,
                tool_name,
                tool_input,
                ..
            } => {
                let index = self.next_output_index;
                self.next_output_index += 1;
                let item_id = format!("fc-{}", generate_id());
                let item = serde_json::json!({
                    "type": "function_call",
                    "id": item_id,
                    "call_id": tool_call_id,
                    "name": tool_name,
                    "arguments": tool_input,
                    "status": "completed"
                });
                vec![
                    ResponsesStreamEvent {
                        event_type: "response.output_item.added".to_owned(),
                        item_id: None,
                        output_index: Some(index),
                        content_index: None,
                        delta: None,
                        call_id: None,
                        name: None,
                        arguments: None,
                        item: Some(serde_json::json!({
                            "type": "function_call",
                            "id": item_id,
                            "call_id": tool_call_id,
                            "name": tool_name,
                            "arguments": "",
                            "status": "in_progress"
                        })),
                    },
                    ResponsesStreamEvent {
                        event_type: "response.function_call_arguments.delta".to_owned(),
                        item_id: None,
                        output_index: Some(index),
                        content_index: None,
                        delta: Some(tool_input.clone()),
                        call_id: Some(tool_call_id.clone()),
                        name: None,
                        arguments: None,
                        item: None,
                    },
                    ResponsesStreamEvent {
                        event_type: "response.function_call_arguments.done".to_owned(),
                        item_id: None,
                        output_index: Some(index),
                        content_index: None,
                        delta: None,
                        call_id: Some(tool_call_id.clone()),
                        name: None,
                        arguments: Some(tool_input.clone()),
                        item: None,
                    },
                    ResponsesStreamEvent {
                        event_type: "response.output_item.done".to_owned(),
                        item_id: None,
                        output_index: Some(index),
                        content_index: None,
                        delta: None,
                        call_id: None,
                        name: None,
                        arguments: None,
                        item: Some(item),
                    },
                ]
            }
            LanguageModelStreamPart::ToolInputStart { id, tool_name, .. } => {
                let index = self.next_output_index;
                self.tool_id_to_index.insert(id.clone(), index);
                self.next_output_index += 1;
                let item_id = format!("fc-{}", generate_id());
                vec![ResponsesStreamEvent {
                    event_type: "response.output_item.added".to_owned(),
                    item_id: None,
                    output_index: Some(index),
                    content_index: None,
                    delta: None,
                    call_id: None,
                    name: Some(tool_name.clone()),
                    arguments: None,
                    item: Some(serde_json::json!({
                        "type": "function_call",
                        "id": item_id,
                        "call_id": id,
                        "name": tool_name,
                        "arguments": "",
                        "status": "in_progress"
                    })),
                }]
            }
            LanguageModelStreamPart::ToolInputDelta { id, delta, .. } => {
                let index = self
                    .tool_id_to_index
                    .get(id)
                    .copied()
                    .unwrap_or(self.next_output_index);
                vec![ResponsesStreamEvent {
                    event_type: "response.function_call_arguments.delta".to_owned(),
                    item_id: None,
                    output_index: Some(index),
                    content_index: None,
                    delta: Some(delta.clone()),
                    call_id: Some(id.clone()),
                    name: None,
                    arguments: None,
                    item: None,
                }]
            }
            LanguageModelStreamPart::ToolInputEnd { .. } => {
                // No specific event needed; the arguments.done will come from
                // the caller if needed, or the finish event signals completion.
                vec![]
            }
            LanguageModelStreamPart::Finish { .. } => {
                vec![ResponsesStreamEvent {
                    event_type: "response.completed".to_owned(),
                    item_id: None,
                    output_index: None,
                    content_index: None,
                    delta: None,
                    call_id: None,
                    name: None,
                    arguments: None,
                    item: None,
                }]
            }
            _ => vec![],
        }
    }
}

// ── Helpers ─────────────────────────────────────────────────────────────────

fn convert_input_items(items: Vec<ResponsesInputItem>) -> Vec<LanguageModelMessage> {
    let mut messages = Vec::new();
    for item in items {
        match item {
            ResponsesInputItem::Message(msg) => match msg.role.as_str() {
                "system" | "developer" => {
                    messages.push(LanguageModelMessage::System {
                        content: input_content_to_string(msg.content),
                        provider_options: None,
                    });
                }
                _ => {
                    messages.push(LanguageModelMessage::User {
                        content: input_content_to_parts(msg.content),
                        provider_options: None,
                    });
                }
            },
            ResponsesInputItem::FunctionCallOutput(fco) => {
                messages.push(LanguageModelMessage::Tool {
                    content: vec![LanguageModelToolResult::ToolResult {
                        tool_call_id: fco.call_id,
                        tool_name: String::new(),
                        output: LanguageModelToolResultOutput::Text {
                            value: fco.output,
                            provider_options: None,
                        },
                        provider_options: None,
                    }],
                    provider_options: None,
                });
            }
        }
    }
    messages
}

fn input_content_to_string(content: Option<ResponsesInputContent>) -> String {
    match content {
        Some(ResponsesInputContent::Text(s)) => s,
        Some(ResponsesInputContent::Parts(parts)) => parts
            .into_iter()
            .map(|p| match p {
                ResponsesInputContentPart::InputText { text } => text,
            })
            .collect::<Vec<_>>()
            .join(""),
        None => String::new(),
    }
}

fn input_content_to_parts(content: Option<ResponsesInputContent>) -> Vec<LanguageModelUserContent> {
    match content {
        Some(ResponsesInputContent::Text(s)) => vec![LanguageModelUserContent::Text {
            text: s,
            provider_options: None,
        }],
        Some(ResponsesInputContent::Parts(parts)) => parts
            .into_iter()
            .map(|p| match p {
                ResponsesInputContentPart::InputText { text } => LanguageModelUserContent::Text {
                    text,
                    provider_options: None,
                },
            })
            .collect(),
        None => vec![],
    }
}

fn extract_output_items(content: &LanguageModelContent) -> Vec<ResponsesOutputItem> {
    match content {
        LanguageModelContent::Text { text, .. } => {
            vec![ResponsesOutputItem::Message {
                id: format!("msg-{}", generate_id()),
                role: "assistant".to_owned(),
                content: vec![ResponsesOutputContent {
                    content_type: "output_text".to_owned(),
                    text: text.clone(),
                }],
                status: "completed".to_owned(),
            }]
        }
        LanguageModelContent::ToolCall {
            tool_call_id,
            tool_name,
            tool_input,
            ..
        } => {
            vec![ResponsesOutputItem::FunctionCall {
                id: format!("fc-{}", generate_id()),
                call_id: tool_call_id.clone(),
                name: tool_name.clone(),
                arguments: tool_input.clone(),
                status: "completed".to_owned(),
            }]
        }
        _ => vec![],
    }
}
