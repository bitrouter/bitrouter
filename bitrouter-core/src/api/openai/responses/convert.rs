//! Conversion between OpenAI Responses format and core LanguageModel types.

use std::collections::HashMap;

use crate::models::{
    language::{
        call_options::LanguageModelCallOptions,
        content::LanguageModelContent,
        data_content::LanguageModelDataContent,
        generate_result::LanguageModelGenerateResult,
        prompt::{
            LanguageModelAssistantContent, LanguageModelMessage, LanguageModelToolResult,
            LanguageModelToolResultOutput, LanguageModelUserContent,
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
    let mut prompt = Vec::new();
    // `instructions` is the canonical developer prompt on Responses; lift it
    // into a System message so the routed model sees it regardless of the
    // upstream protocol.
    // https://platform.openai.com/docs/api-reference/responses/create#responses-create-instructions
    if let Some(text) = request
        .instructions
        .as_ref()
        .filter(|t| !t.trim().is_empty())
    {
        prompt.push(LanguageModelMessage::System {
            content: text.clone(),
            provider_options: None,
        });
    }
    match request.input {
        ResponsesInput::Text(text) => prompt.push(LanguageModelMessage::User {
            content: vec![LanguageModelUserContent::Text {
                text,
                provider_options: None,
            }],
            provider_options: None,
        }),
        ResponsesInput::Items(items) => prompt.extend(convert_input_items(items)),
    }

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
        object: Some("response".to_owned()),
        created_at: now_unix(),
        model: model_id.to_owned(),
        output,
        usage: Some(ResponsesUsage {
            input_tokens: Some(input_tokens),
            output_tokens: Some(output_tokens),
            total_tokens: Some(input_tokens + output_tokens),
            input_tokens_details: None,
            output_tokens_details: None,
        }),
        status: Some("completed".to_owned()),
        incomplete_details: None,
        error: None,
    }
}

// ── Streaming ───────────────────────────────────────────────────────────────

/// Stateful converter that emits a canonical OpenAI Responses streaming
/// lifecycle: `response.created` → `response.in_progress` → per-item
/// `output_item.added` (+ `content_part.added` for messages) → delta events
/// → `*.done` events → `response.completed`. Every emitted event carries a
/// monotonically-increasing `sequence_number`.
///
/// Strict clients such as Codex CLI close the stream with
/// "stream closed before response.completed" if any of this is missing or
/// out of order, then retry immediately.
/// <https://platform.openai.com/docs/api-reference/responses-streaming>
pub struct StreamConverter {
    model_id: String,
    response_id: String,
    created_at: i64,
    sequence_number: u64,
    /// `response.created` / `response.in_progress` are emitted lazily on the
    /// first delivered stream part so the response object can carry the
    /// upstream response id once `ResponseMetadata` arrives.
    created_emitted: bool,
    /// True once `response.completed` (or `failed`/`incomplete`) has been
    /// written. Further calls are no-ops to keep the SSE stream well-formed.
    completed_emitted: bool,
    /// Active output item index — incremented every time a new item opens.
    next_output_index: u32,
    /// Open message item carrying streaming text.
    text_item: Option<MessageItemState>,
    /// Open reasoning item carrying streaming chain-of-thought.
    reasoning_item: Option<ReasoningItemState>,
    /// Open function-call items keyed by upstream tool id, in arrival order.
    tool_items: HashMap<String, ToolItemState>,
    tool_order: Vec<String>,
    /// Completed output items, in emission order — populated as items close
    /// and replayed on `response.completed` so the final envelope mirrors
    /// the non-streaming response.
    completed_output: Vec<ResponsesOutputItem>,
}

struct MessageItemState {
    item_id: String,
    output_index: u32,
    /// Concatenated text deltas; finalized into `output_text.done.text`.
    accumulated_text: String,
}

struct ReasoningItemState {
    item_id: String,
    output_index: u32,
    accumulated_text: String,
}

struct ToolItemState {
    item_id: String,
    output_index: u32,
    call_id: String,
    tool_name: String,
    accumulated_args: String,
}

impl StreamConverter {
    /// Creates a converter bound to a target model id. The response id is
    /// generated locally and surfaced in the `response.created` envelope so
    /// clients can correlate the stream with downstream telemetry.
    pub fn new(model_id: impl Into<String>) -> Self {
        Self {
            model_id: model_id.into(),
            response_id: format!("resp-{}", generate_id()),
            created_at: now_unix(),
            sequence_number: 0,
            created_emitted: false,
            completed_emitted: false,
            next_output_index: 0,
            text_item: None,
            reasoning_item: None,
            tool_items: HashMap::new(),
            tool_order: Vec::new(),
            completed_output: Vec::new(),
        }
    }

    /// Converts a [`LanguageModelStreamPart`] into one or more canonical
    /// Responses SSE events. Returns an empty vector for parts that have no
    /// direct mapping (e.g. `StreamStart`).
    pub fn convert(&mut self, part: &LanguageModelStreamPart) -> Vec<ResponsesStreamEvent> {
        if self.completed_emitted {
            return Vec::new();
        }

        // Adopt the upstream response id *before* emitting `response.created`
        // so the envelope carries the canonical id from the very first event.
        // Multi-turn chaining via `previous_response_id` depends on this.
        if !self.created_emitted
            && let LanguageModelStreamPart::ResponseMetadata {
                id: Some(upstream), ..
            } = part
        {
            self.response_id = upstream.clone();
        }

        let mut events = self.emit_created_if_needed();

        match part {
            LanguageModelStreamPart::ResponseMetadata { id, .. } => {
                // Late metadata (after `response.created` already emitted)
                // still updates the in-progress id for downstream telemetry.
                if let Some(upstream) = id.as_deref() {
                    self.response_id = upstream.to_owned();
                }
            }
            LanguageModelStreamPart::TextStart { .. } => {
                events.extend(self.close_reasoning_item());
                self.open_text_item(&mut events);
            }
            LanguageModelStreamPart::TextDelta { delta, .. } => {
                events.extend(self.close_reasoning_item());
                self.open_text_item(&mut events);
                let (item_id, output_index) = {
                    let item = self
                        .text_item
                        .as_mut()
                        .expect("text item just opened above");
                    item.accumulated_text.push_str(delta);
                    (item.item_id.clone(), item.output_index)
                };
                events.push(self.make_event(
                    "response.output_text.delta",
                    ResponsesStreamEvent {
                        event_type: String::new(),
                        sequence_number: 0,
                        response: None,
                        item_id: Some(item_id),
                        output_index: Some(output_index),
                        content_index: Some(0),
                        delta: Some(delta.clone()),
                        text: None,
                        call_id: None,
                        name: None,
                        arguments: None,
                        item: None,
                        part: None,
                    },
                ));
            }
            LanguageModelStreamPart::TextEnd { .. } => {
                events.extend(self.close_text_item());
            }
            LanguageModelStreamPart::ReasoningStart { .. } => {
                events.extend(self.close_text_item());
                self.open_reasoning_item(&mut events);
            }
            LanguageModelStreamPart::ReasoningDelta { delta, .. } => {
                events.extend(self.close_text_item());
                self.open_reasoning_item(&mut events);
                let (item_id, output_index) = {
                    let item = self
                        .reasoning_item
                        .as_mut()
                        .expect("reasoning item just opened above");
                    item.accumulated_text.push_str(delta);
                    (item.item_id.clone(), item.output_index)
                };
                events.push(self.make_event(
                    "response.reasoning_text.delta",
                    ResponsesStreamEvent {
                        event_type: String::new(),
                        sequence_number: 0,
                        response: None,
                        item_id: Some(item_id),
                        output_index: Some(output_index),
                        content_index: Some(0),
                        delta: Some(delta.clone()),
                        text: None,
                        call_id: None,
                        name: None,
                        arguments: None,
                        item: None,
                        part: None,
                    },
                ));
            }
            LanguageModelStreamPart::ReasoningEnd { .. } => {
                events.extend(self.close_reasoning_item());
            }
            LanguageModelStreamPart::ToolInputStart { id, tool_name, .. } => {
                events.extend(self.close_text_item());
                events.extend(self.close_reasoning_item());
                self.open_tool_item(id, tool_name, &mut events);
            }
            LanguageModelStreamPart::ToolInputDelta { id, delta, .. } => {
                events.extend(self.close_text_item());
                events.extend(self.close_reasoning_item());
                if !self.tool_items.contains_key(id) {
                    self.open_tool_item(id, "tool", &mut events);
                }
                let (item_id, output_index, call_id) = {
                    let item = self
                        .tool_items
                        .get_mut(id)
                        .expect("tool item just opened above");
                    item.accumulated_args.push_str(delta);
                    (
                        item.item_id.clone(),
                        item.output_index,
                        item.call_id.clone(),
                    )
                };
                events.push(self.make_event(
                    "response.function_call_arguments.delta",
                    ResponsesStreamEvent {
                        event_type: String::new(),
                        sequence_number: 0,
                        response: None,
                        item_id: Some(item_id),
                        output_index: Some(output_index),
                        content_index: None,
                        delta: Some(delta.clone()),
                        text: None,
                        call_id: Some(call_id),
                        name: None,
                        arguments: None,
                        item: None,
                        part: None,
                    },
                ));
            }
            LanguageModelStreamPart::ToolInputEnd { id, .. } => {
                events.extend(self.close_tool_item(id));
            }
            LanguageModelStreamPart::ToolCall {
                tool_call_id,
                tool_name,
                tool_input,
                ..
            } => {
                // Single-shot tool call: open, emit full arguments, close.
                events.extend(self.close_text_item());
                events.extend(self.close_reasoning_item());
                self.open_tool_item(tool_call_id, tool_name, &mut events);
                let (item_id, output_index, call_id) = {
                    let item = self
                        .tool_items
                        .get_mut(tool_call_id)
                        .expect("tool item just opened above");
                    item.accumulated_args.push_str(tool_input);
                    (
                        item.item_id.clone(),
                        item.output_index,
                        item.call_id.clone(),
                    )
                };
                events.push(self.make_event(
                    "response.function_call_arguments.delta",
                    ResponsesStreamEvent {
                        event_type: String::new(),
                        sequence_number: 0,
                        response: None,
                        item_id: Some(item_id),
                        output_index: Some(output_index),
                        content_index: None,
                        delta: Some(tool_input.clone()),
                        text: None,
                        call_id: Some(call_id),
                        name: None,
                        arguments: None,
                        item: None,
                        part: None,
                    },
                ));
                events.extend(self.close_tool_item(tool_call_id));
            }
            LanguageModelStreamPart::Finish { usage, .. } => {
                events.extend(self.close_text_item());
                events.extend(self.close_reasoning_item());
                let pending: Vec<String> = self.tool_order.clone();
                for id in pending {
                    events.extend(self.close_tool_item(&id));
                }
                let response = self.build_completed_response(Some(usage));
                events.push(self.make_event(
                    "response.completed",
                    ResponsesStreamEvent {
                        event_type: String::new(),
                        sequence_number: 0,
                        response: Some(response),
                        item_id: None,
                        output_index: None,
                        content_index: None,
                        delta: None,
                        text: None,
                        call_id: None,
                        name: None,
                        arguments: None,
                        item: None,
                        part: None,
                    },
                ));
                self.completed_emitted = true;
            }
            // Stream lifecycle markers without a direct Responses event.
            LanguageModelStreamPart::StreamStart { .. }
            | LanguageModelStreamPart::Raw { .. }
            | LanguageModelStreamPart::Error { .. }
            | LanguageModelStreamPart::File { .. }
            | LanguageModelStreamPart::ToolApprovalRequest { .. }
            | LanguageModelStreamPart::UrlSource { .. }
            | LanguageModelStreamPart::DocumentSource { .. }
            | LanguageModelStreamPart::ToolResult { .. } => {}
        }

        events
    }

    /// Closes any still-open items and emits a synthetic `response.completed`
    /// if the upstream stream ended without a `Finish` part. Returns the
    /// resulting events so the handler can flush them before tearing down
    /// the SSE channel.
    pub fn finish(&mut self) -> Vec<ResponsesStreamEvent> {
        if self.completed_emitted {
            return Vec::new();
        }
        let mut events = self.emit_created_if_needed();
        events.extend(self.close_text_item());
        events.extend(self.close_reasoning_item());
        let pending: Vec<String> = self.tool_order.clone();
        for id in pending {
            events.extend(self.close_tool_item(&id));
        }
        let response = self.build_completed_response(None);
        events.push(self.make_event(
            "response.completed",
            ResponsesStreamEvent {
                event_type: String::new(),
                sequence_number: 0,
                response: Some(response),
                item_id: None,
                output_index: None,
                content_index: None,
                delta: None,
                text: None,
                call_id: None,
                name: None,
                arguments: None,
                item: None,
                part: None,
            },
        ));
        self.completed_emitted = true;
        events
    }

    fn emit_created_if_needed(&mut self) -> Vec<ResponsesStreamEvent> {
        if self.created_emitted {
            return Vec::new();
        }
        self.created_emitted = true;
        let in_progress_response = self.build_in_progress_response();
        vec![
            self.make_event(
                "response.created",
                ResponsesStreamEvent {
                    event_type: String::new(),
                    sequence_number: 0,
                    response: Some(in_progress_response.clone()),
                    item_id: None,
                    output_index: None,
                    content_index: None,
                    delta: None,
                    text: None,
                    call_id: None,
                    name: None,
                    arguments: None,
                    item: None,
                    part: None,
                },
            ),
            self.make_event(
                "response.in_progress",
                ResponsesStreamEvent {
                    event_type: String::new(),
                    sequence_number: 0,
                    response: Some(in_progress_response),
                    item_id: None,
                    output_index: None,
                    content_index: None,
                    delta: None,
                    text: None,
                    call_id: None,
                    name: None,
                    arguments: None,
                    item: None,
                    part: None,
                },
            ),
        ]
    }

    fn open_text_item(&mut self, events: &mut Vec<ResponsesStreamEvent>) {
        if self.text_item.is_some() {
            return;
        }
        let output_index = self.next_output_index;
        self.next_output_index += 1;
        let item_id = format!("msg-{}", generate_id());
        let state = MessageItemState {
            item_id: item_id.clone(),
            output_index,
            accumulated_text: String::new(),
        };
        let in_progress_item = serde_json::json!({
            "type": "message",
            "id": item_id,
            "role": "assistant",
            "content": [],
            "status": "in_progress",
        });
        events.push(self.make_event(
            "response.output_item.added",
            ResponsesStreamEvent {
                event_type: String::new(),
                sequence_number: 0,
                response: None,
                item_id: None,
                output_index: Some(output_index),
                content_index: None,
                delta: None,
                text: None,
                call_id: None,
                name: None,
                arguments: None,
                item: Some(in_progress_item),
                part: None,
            },
        ));
        let part = serde_json::json!({
            "type": "output_text",
            "text": "",
            "annotations": [],
        });
        events.push(self.make_event(
            "response.content_part.added",
            ResponsesStreamEvent {
                event_type: String::new(),
                sequence_number: 0,
                response: None,
                item_id: Some(item_id),
                output_index: Some(output_index),
                content_index: Some(0),
                delta: None,
                text: None,
                call_id: None,
                name: None,
                arguments: None,
                item: None,
                part: Some(part),
            },
        ));
        self.text_item = Some(state);
    }

    fn close_text_item(&mut self) -> Vec<ResponsesStreamEvent> {
        let Some(state) = self.text_item.take() else {
            return Vec::new();
        };
        let final_text = state.accumulated_text.clone();
        let part = serde_json::json!({
            "type": "output_text",
            "text": final_text,
            "annotations": [],
        });
        let final_item = serde_json::json!({
            "type": "message",
            "id": state.item_id,
            "role": "assistant",
            "content": [part.clone()],
            "status": "completed",
        });
        self.completed_output.push(ResponsesOutputItem::Message {
            id: Some(state.item_id.clone()),
            role: Some("assistant".to_owned()),
            content: vec![ResponsesOutputContent::OutputText {
                text: final_text.clone(),
            }],
            status: Some("completed".to_owned()),
        });
        let mut events = Vec::with_capacity(4);
        events.push(self.make_event(
            "response.output_text.done",
            ResponsesStreamEvent {
                event_type: String::new(),
                sequence_number: 0,
                response: None,
                item_id: Some(state.item_id.clone()),
                output_index: Some(state.output_index),
                content_index: Some(0),
                delta: None,
                text: Some(final_text),
                call_id: None,
                name: None,
                arguments: None,
                item: None,
                part: None,
            },
        ));
        events.push(self.make_event(
            "response.content_part.done",
            ResponsesStreamEvent {
                event_type: String::new(),
                sequence_number: 0,
                response: None,
                item_id: Some(state.item_id.clone()),
                output_index: Some(state.output_index),
                content_index: Some(0),
                delta: None,
                text: None,
                call_id: None,
                name: None,
                arguments: None,
                item: None,
                part: Some(part),
            },
        ));
        events.push(self.make_event(
            "response.output_item.done",
            ResponsesStreamEvent {
                event_type: String::new(),
                sequence_number: 0,
                response: None,
                item_id: None,
                output_index: Some(state.output_index),
                content_index: None,
                delta: None,
                text: None,
                call_id: None,
                name: None,
                arguments: None,
                item: Some(final_item),
                part: None,
            },
        ));
        events
    }

    fn open_reasoning_item(&mut self, events: &mut Vec<ResponsesStreamEvent>) {
        if self.reasoning_item.is_some() {
            return;
        }
        let output_index = self.next_output_index;
        self.next_output_index += 1;
        let item_id = format!("rs-{}", generate_id());
        let state = ReasoningItemState {
            item_id: item_id.clone(),
            output_index,
            accumulated_text: String::new(),
        };
        let in_progress_item = serde_json::json!({
            "type": "reasoning",
            "id": item_id,
            "summary": [],
            "content": [],
            "status": "in_progress",
        });
        events.push(self.make_event(
            "response.output_item.added",
            ResponsesStreamEvent {
                event_type: String::new(),
                sequence_number: 0,
                response: None,
                item_id: None,
                output_index: Some(output_index),
                content_index: None,
                delta: None,
                text: None,
                call_id: None,
                name: None,
                arguments: None,
                item: Some(in_progress_item),
                part: None,
            },
        ));
        let part = serde_json::json!({
            "type": "reasoning_text",
            "text": "",
        });
        events.push(self.make_event(
            "response.content_part.added",
            ResponsesStreamEvent {
                event_type: String::new(),
                sequence_number: 0,
                response: None,
                item_id: Some(item_id),
                output_index: Some(output_index),
                content_index: Some(0),
                delta: None,
                text: None,
                call_id: None,
                name: None,
                arguments: None,
                item: None,
                part: Some(part),
            },
        ));
        self.reasoning_item = Some(state);
    }

    fn close_reasoning_item(&mut self) -> Vec<ResponsesStreamEvent> {
        let Some(state) = self.reasoning_item.take() else {
            return Vec::new();
        };
        let final_text = state.accumulated_text.clone();
        let part = serde_json::json!({
            "type": "reasoning_text",
            "text": final_text,
        });
        let final_item = serde_json::json!({
            "type": "reasoning",
            "id": state.item_id,
            "summary": [],
            "content": [part.clone()],
            "status": "completed",
        });
        // `vec!` ordering is left-to-right, so `make_event` is invoked once
        // per slot and `sequence_number` is assigned in emission order.
        vec![
            self.make_event(
                "response.reasoning_text.done",
                ResponsesStreamEvent {
                    event_type: String::new(),
                    sequence_number: 0,
                    response: None,
                    item_id: Some(state.item_id.clone()),
                    output_index: Some(state.output_index),
                    content_index: Some(0),
                    delta: None,
                    text: Some(final_text),
                    call_id: None,
                    name: None,
                    arguments: None,
                    item: None,
                    part: None,
                },
            ),
            self.make_event(
                "response.content_part.done",
                ResponsesStreamEvent {
                    event_type: String::new(),
                    sequence_number: 0,
                    response: None,
                    item_id: Some(state.item_id.clone()),
                    output_index: Some(state.output_index),
                    content_index: Some(0),
                    delta: None,
                    text: None,
                    call_id: None,
                    name: None,
                    arguments: None,
                    item: None,
                    part: Some(part),
                },
            ),
            self.make_event(
                "response.output_item.done",
                ResponsesStreamEvent {
                    event_type: String::new(),
                    sequence_number: 0,
                    response: None,
                    item_id: None,
                    output_index: Some(state.output_index),
                    content_index: None,
                    delta: None,
                    text: None,
                    call_id: None,
                    name: None,
                    arguments: None,
                    item: Some(final_item),
                    part: None,
                },
            ),
        ]
    }

    fn open_tool_item(
        &mut self,
        upstream_id: &str,
        tool_name: &str,
        events: &mut Vec<ResponsesStreamEvent>,
    ) {
        if self.tool_items.contains_key(upstream_id) {
            return;
        }
        let output_index = self.next_output_index;
        self.next_output_index += 1;
        let item_id = format!("fc-{}", generate_id());
        let state = ToolItemState {
            item_id: item_id.clone(),
            output_index,
            call_id: upstream_id.to_owned(),
            tool_name: tool_name.to_owned(),
            accumulated_args: String::new(),
        };
        let in_progress_item = serde_json::json!({
            "type": "function_call",
            "id": item_id,
            "call_id": upstream_id,
            "name": tool_name,
            "arguments": "",
            "status": "in_progress",
        });
        events.push(self.make_event(
            "response.output_item.added",
            ResponsesStreamEvent {
                event_type: String::new(),
                sequence_number: 0,
                response: None,
                item_id: None,
                output_index: Some(output_index),
                content_index: None,
                delta: None,
                text: None,
                call_id: None,
                name: None,
                arguments: None,
                item: Some(in_progress_item),
                part: None,
            },
        ));
        self.tool_items.insert(upstream_id.to_owned(), state);
        self.tool_order.push(upstream_id.to_owned());
    }

    fn close_tool_item(&mut self, upstream_id: &str) -> Vec<ResponsesStreamEvent> {
        let Some(state) = self.tool_items.remove(upstream_id) else {
            return Vec::new();
        };
        self.tool_order.retain(|id| id != upstream_id);
        let final_args = state.accumulated_args.clone();
        let final_item = serde_json::json!({
            "type": "function_call",
            "id": state.item_id,
            "call_id": state.call_id,
            "name": state.tool_name,
            "arguments": final_args,
            "status": "completed",
        });
        self.completed_output
            .push(ResponsesOutputItem::FunctionCall {
                id: Some(state.item_id.clone()),
                call_id: state.call_id.clone(),
                name: state.tool_name.clone(),
                arguments: final_args.clone(),
                status: Some("completed".to_owned()),
            });
        vec![
            self.make_event(
                "response.function_call_arguments.done",
                ResponsesStreamEvent {
                    event_type: String::new(),
                    sequence_number: 0,
                    response: None,
                    item_id: Some(state.item_id.clone()),
                    output_index: Some(state.output_index),
                    content_index: None,
                    delta: None,
                    text: None,
                    call_id: Some(state.call_id),
                    name: None,
                    arguments: Some(final_args),
                    item: None,
                    part: None,
                },
            ),
            self.make_event(
                "response.output_item.done",
                ResponsesStreamEvent {
                    event_type: String::new(),
                    sequence_number: 0,
                    response: None,
                    item_id: None,
                    output_index: Some(state.output_index),
                    content_index: None,
                    delta: None,
                    text: None,
                    call_id: None,
                    name: None,
                    arguments: None,
                    item: Some(final_item),
                    part: None,
                },
            ),
        ]
    }

    fn build_in_progress_response(&self) -> ResponsesResponse {
        ResponsesResponse {
            id: self.response_id.clone(),
            object: Some("response".to_owned()),
            created_at: self.created_at,
            model: self.model_id.clone(),
            output: Vec::new(),
            usage: None,
            status: Some("in_progress".to_owned()),
            incomplete_details: None,
            error: None,
        }
    }

    fn build_completed_response(
        &self,
        usage: Option<&crate::models::language::usage::LanguageModelUsage>,
    ) -> ResponsesResponse {
        // Codex CLI's `ResponseCompletedUsage` types input/output/total as
        // non-optional `i64` (see codex-rs/codex-api/src/sse/responses.rs),
        // so any `null` inside the usage object fails the response.completed
        // parse and the stream hangs. Default unknown counts to 0 and always
        // emit the three core fields together; if no counts are known at
        // all, omit `usage` entirely so codex falls back to `usage: None`.
        let usage = usage.and_then(|u| {
            let input = u.input_tokens.total;
            let output = u.output_tokens.total;
            if input.is_none() && output.is_none() {
                return None;
            }
            let input = input.unwrap_or(0);
            let output = output.unwrap_or(0);
            Some(ResponsesUsage {
                input_tokens: Some(input),
                output_tokens: Some(output),
                total_tokens: Some(input.saturating_add(output)),
                input_tokens_details: None,
                output_tokens_details: None,
            })
        });
        ResponsesResponse {
            id: self.response_id.clone(),
            object: Some("response".to_owned()),
            created_at: self.created_at,
            model: self.model_id.clone(),
            output: self.completed_output.clone(),
            usage,
            status: Some("completed".to_owned()),
            incomplete_details: None,
            error: None,
        }
    }

    fn make_event(
        &mut self,
        event_type: &str,
        mut event: ResponsesStreamEvent,
    ) -> ResponsesStreamEvent {
        event.event_type = event_type.to_owned();
        event.sequence_number = self.sequence_number;
        self.sequence_number += 1;
        event
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
                "assistant" => {
                    // Codex CLI sends prior assistant turns with role=assistant
                    // and `output_text` content. Routing them through `User`
                    // produces consecutive user messages with no assistant
                    // turns between them; upstreams like Z.ai/GLM reject the
                    // chat-completions payload as malformed
                    // ("The prompt parameter was not received normally").
                    // Codex commonly emits assistant messages with empty text
                    // because the visible content lives in a sibling
                    // `reasoning` item (which bitrouter can't replay to
                    // chat-completions upstreams); drop those empty shells
                    // rather than emit a vacuous Assistant message.
                    let parts = input_content_to_assistant_parts(msg.content);
                    if !parts.is_empty() {
                        messages.push(LanguageModelMessage::Assistant {
                            content: parts,
                            provider_options: None,
                        });
                    }
                }
                _ => {
                    let parts = input_content_to_parts(msg.content);
                    if !parts.is_empty() {
                        messages.push(LanguageModelMessage::User {
                            content: parts,
                            provider_options: None,
                        });
                    }
                }
            },
            ResponsesInputItem::FunctionCallOutput(fco) => {
                messages.push(LanguageModelMessage::Tool {
                    content: vec![LanguageModelToolResult::ToolResult {
                        tool_call_id: fco.call_id,
                        tool_name: String::new(),
                        output: LanguageModelToolResultOutput::Text {
                            value: stringify_function_call_output(&fco.output),
                            provider_options: None,
                        },
                        provider_options: None,
                    }],
                    provider_options: None,
                });
            }
            ResponsesInputItem::FunctionCall(fc) => {
                let input = serde_json::from_str(&fc.arguments).unwrap_or(serde_json::Value::Null);
                messages.push(LanguageModelMessage::Assistant {
                    content: vec![LanguageModelAssistantContent::ToolCall {
                        tool_call_id: fc.call_id,
                        tool_name: fc.name,
                        input,
                        provider_executed: None,
                        provider_options: None,
                    }],
                    provider_options: None,
                });
            }
            ResponsesInputItem::Unknown(value) => {
                // Codex CLI emits `reasoning` items separately from the
                // assistant message: extract the text and surface it as
                // Reasoning content on an Assistant message. Providers that
                // support extended thinking (Anthropic) will round-trip it;
                // chat-completions providers strip it per their own
                // convert_prompt (see openai/chat/api.rs).
                if let Some(reasoning_text) = extract_reasoning_text(&value)
                    && !reasoning_text.is_empty()
                {
                    messages.push(LanguageModelMessage::Assistant {
                        content: vec![LanguageModelAssistantContent::Reasoning {
                            text: reasoning_text,
                            provider_options: None,
                        }],
                        provider_options: None,
                    });
                }
                // Other unknown item types (web_search_call, local_shell_call,
                // image_generation_call, custom_tool_call, compaction, ...)
                // are silently dropped — bitrouter can't replay them to a
                // non-Responses upstream.
            }
        }
    }
    messages
}

/// Extracts the concatenated text from a `reasoning` input item's `content`
/// array. Codex CLI's shape per [`codex_protocol::models::ResponseItem`]:
/// `{type: "reasoning", id, summary, content: [{type:"reasoning_text", text}], encrypted_content}`.
/// Returns `None` for any other item type so callers can ignore non-reasoning
/// unknowns.
/// <https://platform.openai.com/docs/api-reference/responses/create#input>
fn extract_reasoning_text(value: &serde_json::Value) -> Option<String> {
    let obj = value.as_object()?;
    if obj.get("type").and_then(|t| t.as_str()) != Some("reasoning") {
        return None;
    }
    let content = obj.get("content").and_then(|c| c.as_array())?;
    let mut pieces = Vec::new();
    for item in content {
        if let Some(text) = item.get("text").and_then(|t| t.as_str())
            && !text.is_empty()
        {
            pieces.push(text.to_owned());
        }
    }
    if pieces.is_empty() {
        None
    } else {
        Some(pieces.join(""))
    }
}

fn input_content_to_assistant_parts(
    content: Option<ResponsesInputContent>,
) -> Vec<LanguageModelAssistantContent> {
    match content {
        Some(ResponsesInputContent::Text(s)) if !s.is_empty() => {
            vec![LanguageModelAssistantContent::Text {
                text: s,
                provider_options: None,
            }]
        }
        Some(ResponsesInputContent::Parts(parts)) => parts
            .into_iter()
            .filter_map(|p| match p {
                // Both `input_text` and `output_text` carry user-visible
                // assistant text on the wire; collapse to Text content.
                ResponsesInputContentPart::InputText { text }
                | ResponsesInputContentPart::OutputText { text } => {
                    if text.is_empty() {
                        None
                    } else {
                        Some(LanguageModelAssistantContent::Text {
                            text,
                            provider_options: None,
                        })
                    }
                }
                ResponsesInputContentPart::InputImage { .. } => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// Codex CLI's `function_call_output.output` may be a plain string or a
/// structured object with `content`/`content_items` (multimodal tool outputs).
/// We collapse the structured form to plain text so the routed model — which
/// often only accepts a string tool result — receives a usable value.
/// <https://platform.openai.com/docs/api-reference/responses/create#input>
fn stringify_function_call_output(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Null => String::new(),
        serde_json::Value::Object(map) => {
            if let Some(serde_json::Value::String(s)) = map.get("content") {
                return s.clone();
            }
            if let Some(serde_json::Value::Array(items)) = map.get("content_items") {
                let pieces: Vec<String> = items
                    .iter()
                    .filter_map(|item| {
                        item.get("text")
                            .and_then(|t| t.as_str())
                            .map(|s| s.to_owned())
                    })
                    .collect();
                if !pieces.is_empty() {
                    return pieces.join("\n");
                }
            }
            serde_json::to_string(value).unwrap_or_default()
        }
        other => serde_json::to_string(other).unwrap_or_default(),
    }
}

fn input_content_to_string(content: Option<ResponsesInputContent>) -> String {
    match content {
        Some(ResponsesInputContent::Text(s)) => s,
        Some(ResponsesInputContent::Parts(parts)) => parts
            .into_iter()
            .filter_map(|p| match p {
                ResponsesInputContentPart::InputText { text }
                | ResponsesInputContentPart::OutputText { text } => Some(text),
                ResponsesInputContentPart::InputImage { .. } => None,
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
                ResponsesInputContentPart::InputText { text }
                | ResponsesInputContentPart::OutputText { text } => {
                    LanguageModelUserContent::Text {
                        text,
                        provider_options: None,
                    }
                }
                ResponsesInputContentPart::InputImage { image_url } => {
                    LanguageModelUserContent::File {
                        filename: None,
                        data: LanguageModelDataContent::Url(image_url),
                        media_type: "image/png".to_owned(),
                        provider_options: None,
                    }
                }
            })
            .collect(),
        None => vec![],
    }
}

fn extract_output_items(blocks: &[LanguageModelContent]) -> Vec<ResponsesOutputItem> {
    let mut out: Vec<ResponsesOutputItem> = Vec::new();
    let mut pending_text: Vec<ResponsesOutputContent> = Vec::new();

    let flush_text = |pending: &mut Vec<ResponsesOutputContent>,
                      out: &mut Vec<ResponsesOutputItem>| {
        if !pending.is_empty() {
            out.push(ResponsesOutputItem::Message {
                id: Some(format!("msg-{}", generate_id())),
                role: Some("assistant".to_owned()),
                content: std::mem::take(pending),
                status: Some("completed".to_owned()),
            });
        }
    };

    for block in blocks {
        match block {
            LanguageModelContent::Text { text, .. } => {
                pending_text.push(ResponsesOutputContent::OutputText { text: text.clone() });
            }
            LanguageModelContent::ToolCall {
                tool_call_id,
                tool_name,
                tool_input,
                ..
            } => {
                flush_text(&mut pending_text, &mut out);
                out.push(ResponsesOutputItem::FunctionCall {
                    id: Some(format!("fc-{}", generate_id())),
                    call_id: tool_call_id.clone(),
                    name: tool_name.clone(),
                    arguments: tool_input.clone(),
                    status: Some("completed".to_owned()),
                });
            }
            _ => {}
        }
    }
    flush_text(&mut pending_text, &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::language::{
        content::LanguageModelContent,
        finish_reason::LanguageModelFinishReason,
        generate_result::LanguageModelGenerateResult,
        prompt::{LanguageModelMessage, LanguageModelUserContent},
        stream_part::LanguageModelStreamPart,
        tool::LanguageModelTool,
        usage::{LanguageModelInputTokens, LanguageModelOutputTokens, LanguageModelUsage},
    };

    use super::super::types::{
        ResponsesInputMessage, ResponsesOutputContent, ResponsesOutputItem, ResponsesRequest,
        ResponsesResponse, ResponsesStreamEvent, ResponsesUsage,
    };

    // ── Request Deserialization ─────────────────────────────────────────

    #[test]
    fn deserialize_simple_text_input() {
        let json = r#"{
            "model": "gpt-4o",
            "input": "Hello, world!"
        }"#;
        let req: ResponsesRequest = serde_json::from_str(json).expect("parse failed");
        assert_eq!(req.model, "gpt-4o");
        match &req.input {
            ResponsesInput::Text(text) => assert_eq!(text, "Hello, world!"),
            other => panic!("expected Text input, got {other:?}"),
        }
        assert!(req.tools.is_none());
        assert!(req.temperature.is_none());
    }

    #[test]
    fn deserialize_message_items_input() {
        let json = r#"{
            "model": "gpt-4o",
            "input": [
                {"role": "system", "content": "Be helpful."},
                {"role": "user", "content": "Hi there"}
            ]
        }"#;
        let req: ResponsesRequest = serde_json::from_str(json).expect("parse failed");
        match &req.input {
            ResponsesInput::Items(items) => {
                assert_eq!(items.len(), 2);
                match &items[0] {
                    ResponsesInputItem::Message(msg) => assert_eq!(msg.role, "system"),
                    other => panic!("expected Message, got {other:?}"),
                }
                match &items[1] {
                    ResponsesInputItem::Message(msg) => assert_eq!(msg.role, "user"),
                    other => panic!("expected Message, got {other:?}"),
                }
            }
            other => panic!("expected Items input, got {other:?}"),
        }
    }

    #[test]
    fn deserialize_function_call_output_input() {
        let json = r#"{
            "model": "gpt-4o",
            "input": [
                {"call_id": "call_abc", "output": "42"}
            ]
        }"#;
        let req: ResponsesRequest = serde_json::from_str(json).expect("parse failed");
        match &req.input {
            ResponsesInput::Items(items) => {
                assert_eq!(items.len(), 1);
                match &items[0] {
                    ResponsesInputItem::FunctionCallOutput(fco) => {
                        assert_eq!(fco.call_id, "call_abc");
                        assert_eq!(fco.output, "42");
                    }
                    other => panic!("expected FunctionCallOutput, got {other:?}"),
                }
            }
            other => panic!("expected Items input, got {other:?}"),
        }
    }

    #[test]
    fn deserialize_with_tools() {
        let json = r#"{
            "model": "gpt-4o",
            "input": "What is the weather?",
            "tools": [
                {
                    "type": "function",
                    "name": "get_weather",
                    "description": "Get current weather",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "location": {"type": "string"}
                        }
                    },
                    "strict": true
                }
            ]
        }"#;
        let req: ResponsesRequest = serde_json::from_str(json).expect("parse failed");
        let tools = req.tools.as_ref().expect("tools missing");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].r#type, "function");
        assert_eq!(tools[0].name.as_deref(), Some("get_weather"));
        assert_eq!(tools[0].description.as_deref(), Some("Get current weather"));
        assert!(tools[0].parameters.is_some());
        assert_eq!(tools[0].strict, Some(true));
    }

    #[test]
    fn deserialize_with_parameters() {
        let json = r#"{
            "model": "gpt-4o",
            "input": "Hi",
            "temperature": 0.7,
            "top_p": 0.9,
            "max_output_tokens": 256,
            "stream": true
        }"#;
        let req: ResponsesRequest = serde_json::from_str(json).expect("parse failed");
        assert!((req.temperature.expect("temp") - 0.7).abs() < f32::EPSILON);
        assert!((req.top_p.expect("top_p") - 0.9).abs() < f32::EPSILON);
        assert_eq!(req.max_output_tokens, Some(256));
        assert_eq!(req.stream, Some(true));
    }

    #[test]
    fn deserialize_multipart_content() {
        let json = r#"{
            "model": "gpt-4o",
            "input": [
                {
                    "role": "user",
                    "content": [
                        {"type": "input_text", "text": "Hello "},
                        {"type": "input_text", "text": "world"}
                    ]
                }
            ]
        }"#;
        let req: ResponsesRequest = serde_json::from_str(json).expect("parse failed");
        match &req.input {
            ResponsesInput::Items(items) => {
                assert_eq!(items.len(), 1);
                match &items[0] {
                    ResponsesInputItem::Message(msg) => match msg.content.as_ref() {
                        Some(ResponsesInputContent::Parts(parts)) => {
                            assert_eq!(parts.len(), 2);
                        }
                        other => panic!("expected Parts content, got {other:?}"),
                    },
                    other => panic!("expected Message, got {other:?}"),
                }
            }
            other => panic!("expected Items input, got {other:?}"),
        }
    }

    #[test]
    fn deserialize_assistant_output_text_input_content() {
        let json = r#"{
            "model": "gpt-5.5",
            "input": [
                {
                    "type": "message",
                    "role": "assistant",
                    "content": [
                        {"type": "output_text", "text": "previous assistant reply"}
                    ]
                }
            ]
        }"#;
        let req: ResponsesRequest = serde_json::from_str(json).expect("parse failed");
        match &req.input {
            ResponsesInput::Items(items) => {
                assert_eq!(items.len(), 1);
                match &items[0] {
                    ResponsesInputItem::Message(msg) => {
                        assert_eq!(msg.role, "assistant");
                        match msg.content.as_ref() {
                            Some(ResponsesInputContent::Parts(parts)) => {
                                assert_eq!(parts.len(), 1);
                                assert!(matches!(
                                    &parts[0],
                                    ResponsesInputContentPart::OutputText { text }
                                        if text == "previous assistant reply"
                                ));
                            }
                            other => panic!("expected Parts content, got {other:?}"),
                        }
                    }
                    other => panic!("expected Message, got {other:?}"),
                }
            }
            other => panic!("expected Items input, got {other:?}"),
        }
    }

    #[test]
    fn serialize_assistant_output_text_input_content() {
        let req = ResponsesRequest {
            model: "gpt-5.5".to_owned(),
            input: ResponsesInput::Items(vec![ResponsesInputItem::Message(
                ResponsesInputMessage {
                    item_type: "message".to_owned(),
                    role: "assistant".to_owned(),
                    content: Some(ResponsesInputContent::Parts(vec![
                        ResponsesInputContentPart::OutputText {
                            text: "previous assistant reply".to_owned(),
                        },
                    ])),
                },
            )]),
            instructions: None,
            temperature: None,
            top_p: None,
            max_output_tokens: None,
            stream: None,
            tools: None,
            tool_choice: None,
            parallel_tool_calls: None,
            text: None,
        };

        let val = serde_json::to_value(&req).expect("serialize failed");
        assert_eq!(val["model"], "gpt-5.5");
        assert_eq!(val["input"][0]["type"], "message");
        assert_eq!(val["input"][0]["role"], "assistant");
        assert_eq!(val["input"][0]["content"][0]["type"], "output_text");
        assert_eq!(
            val["input"][0]["content"][0]["text"],
            "previous assistant reply"
        );
    }

    // ── Response Serialization ──────────────────────────────────────────

    #[test]
    fn serialize_text_response() {
        let resp = ResponsesResponse {
            id: "resp-test-1".to_owned(),
            object: Some("response".to_owned()),
            created_at: 1700000000,
            model: "gpt-4o".to_owned(),
            output: vec![ResponsesOutputItem::Message {
                id: Some("msg-1".to_owned()),
                role: Some("assistant".to_owned()),
                content: vec![ResponsesOutputContent::OutputText {
                    text: "Hello!".to_owned(),
                }],
                status: Some("completed".to_owned()),
            }],
            usage: Some(ResponsesUsage {
                input_tokens: Some(10),
                output_tokens: Some(5),
                total_tokens: Some(15),
                input_tokens_details: None,
                output_tokens_details: None,
            }),
            status: Some("completed".to_owned()),
            incomplete_details: None,
            error: None,
        };
        let val = serde_json::to_value(&resp).expect("serialize failed");
        assert_eq!(val["id"], "resp-test-1");
        assert_eq!(val["object"], "response");
        assert_eq!(val["model"], "gpt-4o");
        assert_eq!(val["status"], "completed");
        assert_eq!(val["output"][0]["type"], "message");
        assert_eq!(val["output"][0]["role"], "assistant");
        assert_eq!(val["output"][0]["content"][0]["text"], "Hello!");
        assert_eq!(val["usage"]["input_tokens"], 10);
        assert_eq!(val["usage"]["output_tokens"], 5);
        assert_eq!(val["usage"]["total_tokens"], 15);
    }

    #[test]
    fn serialize_function_call_response() {
        let resp = ResponsesResponse {
            id: "resp-test-2".to_owned(),
            object: Some("response".to_owned()),
            created_at: 1700000000,
            model: "gpt-4o".to_owned(),
            output: vec![ResponsesOutputItem::FunctionCall {
                id: Some("fc-1".to_owned()),
                call_id: "call_xyz".to_owned(),
                name: "get_weather".to_owned(),
                arguments: r#"{"location":"NYC"}"#.to_owned(),
                status: Some("completed".to_owned()),
            }],
            usage: None,
            status: Some("completed".to_owned()),
            incomplete_details: None,
            error: None,
        };
        let val = serde_json::to_value(&resp).expect("serialize failed");
        assert_eq!(val["output"][0]["type"], "function_call");
        assert_eq!(val["output"][0]["call_id"], "call_xyz");
        assert_eq!(val["output"][0]["name"], "get_weather");
        assert_eq!(val["output"][0]["arguments"], r#"{"location":"NYC"}"#);
        assert!(val.get("usage").is_none());
    }

    #[test]
    fn serialize_streaming_events() {
        let text_delta = ResponsesStreamEvent {
            event_type: "response.output_text.delta".to_owned(),
            sequence_number: 5,
            response: None,
            item_id: Some("msg-1".to_owned()),
            output_index: Some(0),
            content_index: Some(0),
            delta: Some("Hello".to_owned()),
            text: None,
            call_id: None,
            name: None,
            arguments: None,
            item: None,
            part: None,
        };
        let val = serde_json::to_value(&text_delta).expect("serialize failed");
        assert_eq!(val["type"], "response.output_text.delta");
        assert_eq!(val["sequence_number"], 5);
        assert_eq!(val["delta"], "Hello");
        assert_eq!(val["output_index"], 0);
        assert_eq!(val["content_index"], 0);

        let fn_delta = ResponsesStreamEvent {
            event_type: "response.function_call_arguments.delta".to_owned(),
            sequence_number: 6,
            response: None,
            item_id: Some("fc-1".to_owned()),
            output_index: Some(1),
            content_index: None,
            delta: Some(r#"{"loc"#.to_owned()),
            text: None,
            call_id: Some("call_1".to_owned()),
            name: None,
            arguments: None,
            item: None,
            part: None,
        };
        let val = serde_json::to_value(&fn_delta).expect("serialize failed");
        assert_eq!(val["type"], "response.function_call_arguments.delta");
        assert_eq!(val["delta"], r#"{"loc"#);
        assert_eq!(val["call_id"], "call_1");

        let completed = ResponsesStreamEvent {
            event_type: "response.completed".to_owned(),
            sequence_number: 7,
            response: None,
            item_id: None,
            output_index: None,
            content_index: None,
            delta: None,
            text: None,
            call_id: None,
            name: None,
            arguments: None,
            item: None,
            part: None,
        };
        let val = serde_json::to_value(&completed).expect("serialize failed");
        assert_eq!(val["type"], "response.completed");
        assert!(val.get("delta").is_none());
        assert!(val.get("output_index").is_none());
    }

    // ── Conversion: to_call_options ─────────────────────────────────────

    #[test]
    fn to_call_options_text_input() {
        let json = r#"{
            "model": "gpt-4o",
            "input": "Hello"
        }"#;
        let req: ResponsesRequest = serde_json::from_str(json).expect("parse failed");
        let opts = to_call_options(req);
        assert_eq!(opts.prompt.len(), 1);
        match &opts.prompt[0] {
            LanguageModelMessage::User { content, .. } => {
                assert_eq!(content.len(), 1);
                match &content[0] {
                    LanguageModelUserContent::Text { text, .. } => assert_eq!(text, "Hello"),
                    other => panic!("expected Text, got {other:?}"),
                }
            }
            other => panic!("expected User message, got {other:?}"),
        }
        assert!(opts.tools.is_none());
        assert!(opts.temperature.is_none());
    }

    #[test]
    fn to_call_options_message_items() {
        let json = r#"{
            "model": "gpt-4o",
            "input": [
                {"role": "system", "content": "Be concise."},
                {"role": "user", "content": "Hi"}
            ]
        }"#;
        let req: ResponsesRequest = serde_json::from_str(json).expect("parse failed");
        let opts = to_call_options(req);
        assert_eq!(opts.prompt.len(), 2);
        match &opts.prompt[0] {
            LanguageModelMessage::System { content, .. } => {
                assert_eq!(content, "Be concise.");
            }
            other => panic!("expected System message, got {other:?}"),
        }
        match &opts.prompt[1] {
            LanguageModelMessage::User { content, .. } => {
                assert_eq!(content.len(), 1);
                match &content[0] {
                    LanguageModelUserContent::Text { text, .. } => assert_eq!(text, "Hi"),
                    other => panic!("expected Text, got {other:?}"),
                }
            }
            other => panic!("expected User message, got {other:?}"),
        }
    }

    #[test]
    fn to_call_options_developer_role_becomes_system() {
        let json = r#"{
            "model": "gpt-4o",
            "input": [
                {"role": "developer", "content": "Instructions here"}
            ]
        }"#;
        let req: ResponsesRequest = serde_json::from_str(json).expect("parse failed");
        let opts = to_call_options(req);
        assert_eq!(opts.prompt.len(), 1);
        match &opts.prompt[0] {
            LanguageModelMessage::System { content, .. } => {
                assert_eq!(content, "Instructions here");
            }
            other => panic!("expected System message, got {other:?}"),
        }
    }

    #[test]
    fn to_call_options_function_call_output() {
        let json = r#"{
            "model": "gpt-4o",
            "input": [
                {"call_id": "call_123", "output": "result_value"}
            ]
        }"#;
        let req: ResponsesRequest = serde_json::from_str(json).expect("parse failed");
        let opts = to_call_options(req);
        assert_eq!(opts.prompt.len(), 1);
        match &opts.prompt[0] {
            LanguageModelMessage::Tool { content, .. } => {
                assert_eq!(content.len(), 1);
                match &content[0] {
                    LanguageModelToolResult::ToolResult {
                        tool_call_id,
                        output,
                        ..
                    } => {
                        assert_eq!(tool_call_id, "call_123");
                        match output {
                            LanguageModelToolResultOutput::Text { value, .. } => {
                                assert_eq!(value, "result_value");
                            }
                            other => panic!("expected Text output, got {other:?}"),
                        }
                    }
                    other => panic!("expected ToolResult, got {other:?}"),
                }
            }
            other => panic!("expected Tool message, got {other:?}"),
        }
    }

    #[test]
    fn to_call_options_with_function_tools() {
        let json = r#"{
            "model": "gpt-4o",
            "input": "Hi",
            "tools": [
                {
                    "type": "function",
                    "name": "search",
                    "description": "Search the web",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "query": {"type": "string"}
                        }
                    },
                    "strict": true
                }
            ]
        }"#;
        let req: ResponsesRequest = serde_json::from_str(json).expect("parse failed");
        let opts = to_call_options(req);
        let tools = opts.tools.expect("tools missing");
        assert_eq!(tools.len(), 1);
        match &tools[0] {
            LanguageModelTool::Function {
                name,
                description,
                strict,
                ..
            } => {
                assert_eq!(name, "search");
                assert_eq!(description.as_deref(), Some("Search the web"));
                assert_eq!(*strict, Some(true));
            }
            other => panic!("expected Function tool, got {other:?}"),
        }
    }

    #[test]
    fn to_call_options_non_function_tools_filtered() {
        let json = r#"{
            "model": "gpt-4o",
            "input": "Hi",
            "tools": [
                {"type": "code_interpreter"},
                {"type": "function", "name": "search", "parameters": {}}
            ]
        }"#;
        let req: ResponsesRequest = serde_json::from_str(json).expect("parse failed");
        let opts = to_call_options(req);
        let tools = opts.tools.expect("tools missing");
        assert_eq!(tools.len(), 1);
    }

    #[test]
    fn to_call_options_maps_parameters() {
        let json = r#"{
            "model": "gpt-4o",
            "input": "Hi",
            "temperature": 0.5,
            "top_p": 0.8,
            "max_output_tokens": 100,
            "stream": true
        }"#;
        let req: ResponsesRequest = serde_json::from_str(json).expect("parse failed");
        let opts = to_call_options(req);
        assert!((opts.temperature.expect("temp") - 0.5).abs() < f32::EPSILON);
        assert!((opts.top_p.expect("top_p") - 0.8).abs() < f32::EPSILON);
        assert_eq!(opts.max_output_tokens, Some(100));
        assert_eq!(opts.stream, Some(true));
    }

    #[test]
    fn to_call_options_multipart_content() {
        let json = r#"{
            "model": "gpt-4o",
            "input": [
                {
                    "role": "user",
                    "content": [
                        {"type": "input_text", "text": "Hello "},
                        {"type": "input_text", "text": "world"}
                    ]
                }
            ]
        }"#;
        let req: ResponsesRequest = serde_json::from_str(json).expect("parse failed");
        let opts = to_call_options(req);
        assert_eq!(opts.prompt.len(), 1);
        match &opts.prompt[0] {
            LanguageModelMessage::User { content, .. } => {
                assert_eq!(content.len(), 2);
                match &content[0] {
                    LanguageModelUserContent::Text { text, .. } => assert_eq!(text, "Hello "),
                    other => panic!("expected Text, got {other:?}"),
                }
                match &content[1] {
                    LanguageModelUserContent::Text { text, .. } => assert_eq!(text, "world"),
                    other => panic!("expected Text, got {other:?}"),
                }
            }
            other => panic!("expected User message, got {other:?}"),
        }
    }

    // ── Conversion: from_generate_result ────────────────────────────────

    fn make_usage(input: u32, output: u32) -> LanguageModelUsage {
        LanguageModelUsage {
            input_tokens: LanguageModelInputTokens {
                total: Some(input),
                no_cache: None,
                cache_read: None,
                cache_write: None,
            },
            output_tokens: LanguageModelOutputTokens {
                total: Some(output),
                text: None,
                reasoning: None,
            },
            raw: None,
        }
    }

    #[test]
    fn from_generate_result_text() {
        let result = LanguageModelGenerateResult {
            content: vec![LanguageModelContent::Text {
                text: "Hello world".to_owned(),
                provider_metadata: None,
            }],
            finish_reason: LanguageModelFinishReason::Stop,
            usage: make_usage(10, 5),
            provider_metadata: None,
            request: None,
            response_metadata: None,
            warnings: None,
        };
        let resp = from_generate_result("gpt-4o", result);
        assert_eq!(resp.object.as_deref(), Some("response"));
        assert_eq!(resp.model, "gpt-4o");
        assert_eq!(resp.status.as_deref(), Some("completed"));
        assert!(resp.id.starts_with("resp-"));
        assert_eq!(resp.output.len(), 1);
        match &resp.output[0] {
            ResponsesOutputItem::Message {
                role,
                content,
                status,
                ..
            } => {
                assert_eq!(role.as_deref(), Some("assistant"));
                assert_eq!(status.as_deref(), Some("completed"));
                assert_eq!(content.len(), 1);
                match &content[0] {
                    ResponsesOutputContent::OutputText { text } => {
                        assert_eq!(text, "Hello world");
                    }
                    other => panic!("expected OutputText, got {other:?}"),
                }
            }
            other => panic!("expected Message output, got {other:?}"),
        }
        let usage = resp.usage.expect("usage missing");
        assert_eq!(usage.input_tokens, Some(10));
        assert_eq!(usage.output_tokens, Some(5));
        assert_eq!(usage.total_tokens, Some(15));
    }

    #[test]
    fn from_generate_result_tool_call() {
        let result = LanguageModelGenerateResult {
            content: vec![LanguageModelContent::ToolCall {
                tool_call_id: "call_xyz".to_owned(),
                tool_name: "get_weather".to_owned(),
                tool_input: r#"{"location":"NYC"}"#.to_owned(),
                provider_executed: None,
                dynamic: None,
                provider_metadata: None,
            }],
            finish_reason: LanguageModelFinishReason::FunctionCall,
            usage: make_usage(15, 20),
            provider_metadata: None,
            request: None,
            response_metadata: None,
            warnings: None,
        };
        let resp = from_generate_result("gpt-4o", result);
        assert_eq!(resp.output.len(), 1);
        match &resp.output[0] {
            ResponsesOutputItem::FunctionCall {
                call_id,
                name,
                arguments,
                status,
                ..
            } => {
                assert_eq!(call_id, "call_xyz");
                assert_eq!(name, "get_weather");
                assert_eq!(arguments, r#"{"location":"NYC"}"#);
                assert_eq!(status.as_deref(), Some("completed"));
            }
            other => panic!("expected FunctionCall output, got {other:?}"),
        }
        let usage = resp.usage.expect("usage missing");
        assert_eq!(usage.input_tokens, Some(15));
        assert_eq!(usage.output_tokens, Some(20));
        assert_eq!(usage.total_tokens, Some(35));
    }

    // ── Conversion: StreamConverter ─────────────────────────────────────

    #[test]
    fn stream_converter_text_delta_emits_full_lifecycle_prefix() {
        // The first delta of a stream must be preceded by the canonical
        // lifecycle preamble — `response.created`, `response.in_progress`,
        // `response.output_item.added` (message), `response.content_part.added`
        // (output_text) — before the actual `response.output_text.delta`.
        // Codex CLI errors on streams that skip any of these and retries.
        // https://platform.openai.com/docs/api-reference/responses-streaming
        let mut conv = StreamConverter::new("test-model");
        let part = LanguageModelStreamPart::TextDelta {
            id: "t1".to_owned(),
            delta: "Hello".to_owned(),
            provider_metadata: None,
        };
        let events = conv.convert(&part);
        assert_eq!(events.len(), 5);
        assert_eq!(events[0].event_type, "response.created");
        assert!(events[0].response.is_some());
        assert_eq!(events[1].event_type, "response.in_progress");
        assert_eq!(events[2].event_type, "response.output_item.added");
        let item = events[2].item.as_ref().expect("item present");
        assert_eq!(item["type"], "message");
        assert_eq!(item["status"], "in_progress");
        assert_eq!(events[3].event_type, "response.content_part.added");
        let part = events[3].part.as_ref().expect("part present");
        assert_eq!(part["type"], "output_text");
        assert_eq!(events[4].event_type, "response.output_text.delta");
        assert_eq!(events[4].delta.as_deref(), Some("Hello"));
        assert_eq!(events[4].output_index, Some(0));
        assert_eq!(events[4].content_index, Some(0));

        // Every event must carry a monotonic sequence_number starting from 0.
        for (i, e) in events.iter().enumerate() {
            assert_eq!(e.sequence_number, i as u64);
        }
    }

    #[test]
    fn stream_converter_reasoning_delta_emits_reasoning_lifecycle() {
        // Regression test for issue #448: ReasoningDelta must surface as
        // `response.reasoning_text.delta` wrapped in a reasoning output
        // item, with the full lifecycle preamble so Responses-format
        // clients render thinking in real time.
        let mut conv = StreamConverter::new("test-model");
        let events = conv.convert(&LanguageModelStreamPart::ReasoningDelta {
            id: "r1".to_owned(),
            delta: "Hmm".to_owned(),
            provider_metadata: None,
        });
        assert_eq!(events[0].event_type, "response.created");
        assert_eq!(events[1].event_type, "response.in_progress");
        assert_eq!(events[2].event_type, "response.output_item.added");
        assert_eq!(events[2].item.as_ref().expect("item")["type"], "reasoning");
        assert_eq!(events[3].event_type, "response.content_part.added");
        assert_eq!(
            events[3].part.as_ref().expect("part")["type"],
            "reasoning_text"
        );
        assert_eq!(events[4].event_type, "response.reasoning_text.delta");
        assert_eq!(events[4].delta.as_deref(), Some("Hmm"));
    }

    #[test]
    fn stream_converter_reasoning_end_closes_open_item() {
        // ReasoningEnd received before any reasoning content opened the
        // item: emit only the lifecycle preamble (no spurious .done).
        let mut conv = StreamConverter::new("test-model");
        let events = conv.convert(&LanguageModelStreamPart::ReasoningEnd {
            id: "r1".to_owned(),
            provider_metadata: None,
        });
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].event_type, "response.created");
        assert_eq!(events[1].event_type, "response.in_progress");
    }

    #[test]
    fn stream_converter_reasoning_start_opens_item() {
        // ReasoningStart eagerly opens the reasoning output item so
        // subsequent deltas can attach without further preamble.
        let mut conv = StreamConverter::new("test-model");
        let events = conv.convert(&LanguageModelStreamPart::ReasoningStart {
            id: "r1".to_owned(),
            provider_metadata: None,
        });
        assert_eq!(events.len(), 4);
        assert_eq!(events[0].event_type, "response.created");
        assert_eq!(events[1].event_type, "response.in_progress");
        assert_eq!(events[2].event_type, "response.output_item.added");
        assert_eq!(events[3].event_type, "response.content_part.added");
    }

    #[test]
    fn stream_converter_tool_call_emits_lifecycle_and_function_call_events() {
        let mut conv = StreamConverter::new("test-model");
        let part = LanguageModelStreamPart::ToolCall {
            tool_call_id: "call_full".to_owned(),
            tool_name: "calculator".to_owned(),
            tool_input: r#"{"expr":"2+2"}"#.to_owned(),
            provider_executed: None,
            dynamic: None,
            provider_metadata: None,
        };
        let events = conv.convert(&part);
        // created, in_progress, output_item.added, function_call_arguments.delta,
        // function_call_arguments.done, output_item.done
        assert_eq!(events.len(), 6);
        assert_eq!(events[0].event_type, "response.created");
        assert_eq!(events[1].event_type, "response.in_progress");
        assert_eq!(events[2].event_type, "response.output_item.added");
        let item = events[2].item.as_ref().expect("item missing");
        assert_eq!(item["type"], "function_call");
        assert_eq!(item["status"], "in_progress");
        assert_eq!(
            events[3].event_type,
            "response.function_call_arguments.delta"
        );
        assert_eq!(events[3].delta.as_deref(), Some(r#"{"expr":"2+2"}"#));
        assert_eq!(events[3].call_id.as_deref(), Some("call_full"));
        assert_eq!(
            events[4].event_type,
            "response.function_call_arguments.done"
        );
        assert_eq!(events[4].arguments.as_deref(), Some(r#"{"expr":"2+2"}"#));
        assert_eq!(events[5].event_type, "response.output_item.done");
        let done_item = events[5].item.as_ref().expect("item missing");
        assert_eq!(done_item["status"], "completed");
        assert_eq!(done_item["name"], "calculator");
    }

    #[test]
    fn stream_converter_tool_input_start_delta() {
        let mut conv = StreamConverter::new("test-model");

        // Start event: created + in_progress + output_item.added
        let start = LanguageModelStreamPart::ToolInputStart {
            id: "call_a".to_owned(),
            tool_name: "search".to_owned(),
            provider_executed: None,
            dynamic: None,
            title: None,
            provider_metadata: None,
        };
        let events = conv.convert(&start);
        assert_eq!(events.len(), 3);
        assert_eq!(events[2].event_type, "response.output_item.added");
        let item = events[2].item.as_ref().expect("item missing");
        assert_eq!(item["type"], "function_call");
        assert_eq!(item["name"], "search");
        assert_eq!(item["status"], "in_progress");

        // Delta event: function_call_arguments.delta only
        let delta = LanguageModelStreamPart::ToolInputDelta {
            id: "call_a".to_owned(),
            delta: r#"{"query":"rust"}"#.to_owned(),
            provider_metadata: None,
        };
        let events = conv.convert(&delta);
        assert_eq!(events.len(), 1);
        assert_eq!(
            events[0].event_type,
            "response.function_call_arguments.delta"
        );
        assert_eq!(events[0].output_index, Some(0));
        assert_eq!(events[0].delta.as_deref(), Some(r#"{"query":"rust"}"#));
        assert_eq!(events[0].call_id.as_deref(), Some("call_a"));
    }

    #[test]
    fn stream_converter_tool_input_end_emits_done_when_open() {
        let mut conv = StreamConverter::new("test-model");
        // Open first, then close.
        conv.convert(&LanguageModelStreamPart::ToolInputStart {
            id: "call_a".to_owned(),
            tool_name: "search".to_owned(),
            provider_executed: None,
            dynamic: None,
            title: None,
            provider_metadata: None,
        });
        let events = conv.convert(&LanguageModelStreamPart::ToolInputEnd {
            id: "call_a".to_owned(),
            provider_metadata: None,
        });
        assert_eq!(events.len(), 2);
        assert_eq!(
            events[0].event_type,
            "response.function_call_arguments.done"
        );
        assert_eq!(events[1].event_type, "response.output_item.done");
    }

    #[test]
    fn stream_converter_finish_emits_completed_with_usage_and_output() {
        let mut conv = StreamConverter::new("test-model");
        // Emit some text first so the final response has output items.
        conv.convert(&LanguageModelStreamPart::TextDelta {
            id: "t1".to_owned(),
            delta: "Hello".to_owned(),
            provider_metadata: None,
        });
        let events = conv.convert(&LanguageModelStreamPart::Finish {
            usage: make_usage(25, 30),
            finish_reason: LanguageModelFinishReason::Stop,
            provider_metadata: None,
        });
        // output_text.done, content_part.done, output_item.done, response.completed
        assert_eq!(events.len(), 4);
        assert_eq!(events[3].event_type, "response.completed");
        let resp = events[3].response.as_ref().expect("response present");
        let usage = resp.usage.as_ref().expect("usage present");
        assert_eq!(usage.input_tokens, Some(25));
        assert_eq!(usage.output_tokens, Some(30));
        assert_eq!(usage.total_tokens, Some(55));
        assert_eq!(resp.status.as_deref(), Some("completed"));
        assert_eq!(resp.output.len(), 1);
    }

    #[test]
    fn stream_converter_completed_payload_parses_as_codex_response_completed() {
        // Lock the contract with codex-rs/codex-api/src/sse/responses.rs::
        // ResponseCompletedUsage where input_tokens / output_tokens /
        // total_tokens are typed `i64` (not Option). A `null` value inside
        // the usage object fails the parse, the stream errors out, the
        // oneshot LastResponse channel never fires, and the Codex TUI hangs.
        #[derive(serde::Deserialize)]
        #[allow(dead_code)]
        struct CodexResponseCompleted {
            id: String,
            #[serde(default)]
            usage: Option<CodexUsage>,
            #[serde(default)]
            end_turn: Option<bool>,
        }
        #[derive(serde::Deserialize)]
        #[allow(dead_code)]
        struct CodexUsage {
            input_tokens: i64,
            output_tokens: i64,
            total_tokens: i64,
        }

        let mut conv = StreamConverter::new("test-model");
        conv.convert(&LanguageModelStreamPart::TextDelta {
            id: "t1".to_owned(),
            delta: "Hi".to_owned(),
            provider_metadata: None,
        });
        let events = conv.convert(&LanguageModelStreamPart::Finish {
            usage: make_usage(11, 22),
            finish_reason: LanguageModelFinishReason::Stop,
            provider_metadata: None,
        });
        let completed = events
            .iter()
            .find(|e| e.event_type == "response.completed")
            .and_then(|e| e.response.as_ref())
            .expect("response.completed present");
        let json = serde_json::to_value(completed).expect("serialize");
        // The whole JSON shape must round-trip into Codex's struct.
        let parsed: CodexResponseCompleted =
            serde_json::from_value(json.clone()).expect("Codex parses response.completed");
        assert_eq!(parsed.id, completed.id);
        let usage = parsed.usage.expect("usage present");
        assert_eq!(usage.input_tokens, 11);
        assert_eq!(usage.output_tokens, 22);
        assert_eq!(usage.total_tokens, 33);
        // And there must be no JSON null inside usage that would break the parse.
        let usage_obj = json["usage"].as_object().expect("usage object");
        for key in ["input_tokens", "output_tokens", "total_tokens"] {
            assert!(
                !usage_obj[key].is_null(),
                "usage.{key} must not be null in response.completed"
            );
        }
    }

    #[test]
    fn stream_converter_completed_omits_usage_when_upstream_has_none() {
        // If the upstream provides no token counts at all, the usage object
        // is omitted entirely rather than emitting nulls. Codex's
        // `usage: Option<ResponseCompletedUsage>` deserializes None cleanly.
        use crate::models::language::usage::{
            LanguageModelInputTokens, LanguageModelOutputTokens, LanguageModelUsage,
        };
        let empty_usage = LanguageModelUsage {
            input_tokens: LanguageModelInputTokens {
                total: None,
                no_cache: None,
                cache_read: None,
                cache_write: None,
            },
            output_tokens: LanguageModelOutputTokens {
                total: None,
                text: None,
                reasoning: None,
            },
            raw: None,
        };
        let mut conv = StreamConverter::new("test-model");
        conv.convert(&LanguageModelStreamPart::TextDelta {
            id: "t1".to_owned(),
            delta: "Hi".to_owned(),
            provider_metadata: None,
        });
        let events = conv.convert(&LanguageModelStreamPart::Finish {
            usage: empty_usage,
            finish_reason: LanguageModelFinishReason::Stop,
            provider_metadata: None,
        });
        let completed = events
            .iter()
            .find(|e| e.event_type == "response.completed")
            .and_then(|e| e.response.as_ref())
            .expect("response.completed present");
        assert!(
            completed.usage.is_none(),
            "usage must be omitted when no token counts"
        );
        let json = serde_json::to_value(completed).expect("serialize");
        assert!(
            json.get("usage").is_none(),
            "usage key must be absent from serialized JSON, got {json}"
        );
    }

    #[test]
    fn stream_converter_text_start_opens_message_item() {
        let mut conv = StreamConverter::new("test-model");
        let part = LanguageModelStreamPart::TextStart {
            id: "t1".to_owned(),
            provider_metadata: None,
        };
        let events = conv.convert(&part);
        assert_eq!(events.len(), 4);
        assert_eq!(events[0].event_type, "response.created");
        assert_eq!(events[1].event_type, "response.in_progress");
        assert_eq!(events[2].event_type, "response.output_item.added");
        assert_eq!(events[3].event_type, "response.content_part.added");
    }

    #[test]
    fn stream_converter_finish_method_synthesizes_completed() {
        // If the upstream closes without a Finish part, the handler is
        // expected to call `finish()` to terminate the SSE stream with a
        // valid `response.completed` envelope. Otherwise Codex CLI raises
        // "stream closed before response.completed" and retries.
        let mut conv = StreamConverter::new("test-model");
        conv.convert(&LanguageModelStreamPart::TextDelta {
            id: "t1".to_owned(),
            delta: "Hi".to_owned(),
            provider_metadata: None,
        });
        let events = conv.finish();
        assert!(events.iter().any(|e| e.event_type == "response.completed"));
    }

    #[test]
    fn stream_converter_no_done_terminator_emitted() {
        // The Responses streaming protocol terminates with `response.completed`,
        // not with a `[DONE]` sentinel. The converter must never emit one.
        let mut conv = StreamConverter::new("test-model");
        let mut all_events = Vec::new();
        all_events.extend(conv.convert(&LanguageModelStreamPart::TextDelta {
            id: "t1".to_owned(),
            delta: "x".to_owned(),
            provider_metadata: None,
        }));
        all_events.extend(conv.convert(&LanguageModelStreamPart::Finish {
            usage: make_usage(1, 1),
            finish_reason: LanguageModelFinishReason::Stop,
            provider_metadata: None,
        }));
        assert!(all_events.iter().all(|e| !e.event_type.contains("[DONE]")
            && e.event_type != "done"
            && e.delta.as_deref() != Some("[DONE]")));
    }

    #[test]
    fn stream_converter_sequence_numbers_monotonic_across_calls() {
        let mut conv = StreamConverter::new("test-model");
        let mut all = Vec::new();
        all.extend(conv.convert(&LanguageModelStreamPart::TextDelta {
            id: "t1".to_owned(),
            delta: "a".to_owned(),
            provider_metadata: None,
        }));
        all.extend(conv.convert(&LanguageModelStreamPart::TextDelta {
            id: "t1".to_owned(),
            delta: "b".to_owned(),
            provider_metadata: None,
        }));
        all.extend(conv.convert(&LanguageModelStreamPart::Finish {
            usage: make_usage(1, 1),
            finish_reason: LanguageModelFinishReason::Stop,
            provider_metadata: None,
        }));
        for (i, e) in all.iter().enumerate() {
            assert_eq!(e.sequence_number, i as u64, "event {i}: {:?}", e.event_type);
        }
    }

    #[test]
    fn stream_converter_response_id_adopts_upstream_metadata() {
        // When the upstream provides a response id via ResponseMetadata,
        // the converter must adopt it *before* emitting `response.created`
        // so the envelope carries the canonical id for downstream chaining
        // via `previous_response_id`.
        let mut conv = StreamConverter::new("test-model");
        let preamble = conv.convert(&LanguageModelStreamPart::ResponseMetadata {
            id: Some("resp-from-upstream".to_owned()),
            timestamp: None,
            model_id: None,
        });
        let created = preamble
            .iter()
            .find(|e| e.event_type == "response.created")
            .and_then(|e| e.response.as_ref())
            .expect("response.created present with response envelope");
        assert_eq!(created.id, "resp-from-upstream");
    }

    #[test]
    fn stream_converter_multiple_tools_sequential_indices() {
        let mut conv = StreamConverter::new("test-model");

        let start1 = LanguageModelStreamPart::ToolInputStart {
            id: "call_a".to_owned(),
            tool_name: "tool_a".to_owned(),
            provider_executed: None,
            dynamic: None,
            title: None,
            provider_metadata: None,
        };
        let events1 = conv.convert(&start1);
        let added1 = events1
            .iter()
            .find(|e| e.event_type == "response.output_item.added")
            .expect("output_item.added present");
        assert_eq!(added1.output_index, Some(0));

        let start2 = LanguageModelStreamPart::ToolInputStart {
            id: "call_b".to_owned(),
            tool_name: "tool_b".to_owned(),
            provider_executed: None,
            dynamic: None,
            title: None,
            provider_metadata: None,
        };
        let events2 = conv.convert(&start2);
        let added2 = events2
            .iter()
            .find(|e| e.event_type == "response.output_item.added")
            .expect("output_item.added present");
        assert_eq!(added2.output_index, Some(1));
    }

    // ── extract_model_name ──────────────────────────────────────────────

    #[test]
    fn extract_model_name_returns_model() {
        let json = r#"{
            "model": "gpt-4o-mini",
            "input": "test"
        }"#;
        let req: ResponsesRequest = serde_json::from_str(json).expect("parse failed");
        assert_eq!(extract_model_name(&req), "gpt-4o-mini");
    }

    // ── Codex CLI compatibility ─────────────────────────────────────────

    #[test]
    fn deserialize_codex_multi_turn_request_with_reasoning_items() {
        // Regression for the 400 reported when Codex CLI sends a multi-turn
        // request: `input` contains `reasoning`, `web_search_call`, and
        // other item types from previous turns. Our untagged enum must
        // accept (and silently drop) anything it doesn't model, instead of
        // failing the whole request.
        // https://platform.openai.com/docs/api-reference/responses/create#input
        let json = r#"{
            "model": "opencode-go:glm-5.1",
            "instructions": "You are a coding agent running in the Codex CLI.",
            "input": [
                {
                    "type": "message",
                    "role": "developer",
                    "content": [{"type": "input_text", "text": "<permissions instructions>"}]
                },
                {
                    "type": "message",
                    "role": "user",
                    "content": [{"type": "input_text", "text": "hi"}]
                },
                {
                    "type": "message",
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": "Hello!"}]
                },
                {
                    "type": "reasoning",
                    "id": "rs_abc",
                    "summary": [],
                    "content": [{"type": "reasoning_text", "text": "I should greet."}],
                    "encrypted_content": null
                },
                {
                    "type": "web_search_call",
                    "id": "ws_1",
                    "status": "completed",
                    "action": {"type": "search", "query": "weather"}
                },
                {
                    "type": "local_shell_call",
                    "call_id": "ls_1",
                    "status": "completed",
                    "action": {"type": "exec", "command": ["ls"]}
                }
            ],
            "tool_choice": "auto",
            "parallel_tool_calls": false,
            "stream": true,
            "store": false,
            "include": ["reasoning.encrypted_content"],
            "prompt_cache_key": "thread-1"
        }"#;
        let req: ResponsesRequest = serde_json::from_str(json).expect("Codex payload must parse");
        assert_eq!(
            req.instructions.as_deref(),
            Some("You are a coding agent running in the Codex CLI.")
        );
        match &req.input {
            ResponsesInput::Items(items) => {
                assert_eq!(items.len(), 6);
                // Reasoning, web_search_call, local_shell_call fall through
                // to the catch-all variant.
                assert!(matches!(items[3], ResponsesInputItem::Unknown(_)));
                assert!(matches!(items[4], ResponsesInputItem::Unknown(_)));
                assert!(matches!(items[5], ResponsesInputItem::Unknown(_)));
            }
            other => panic!("expected Items, got {other:?}"),
        }
    }

    #[test]
    fn to_call_options_lifts_instructions_to_system_message() {
        // Codex sends the entire system prompt via the top-level
        // `instructions` field, not as a message. `to_call_options` must
        // synthesize a System message so the routed model sees it.
        let json = r#"{
            "model": "opencode-go:glm-5.1",
            "instructions": "Be precise.",
            "input": [{"type": "message", "role": "user", "content": [{"type": "input_text", "text": "Hi"}]}]
        }"#;
        let req: ResponsesRequest = serde_json::from_str(json).expect("parse");
        let opts = to_call_options(req);
        assert!(matches!(
            opts.prompt.first(),
            Some(LanguageModelMessage::System { content, .. }) if content == "Be precise."
        ));
        assert!(matches!(
            opts.prompt.get(1),
            Some(LanguageModelMessage::User { .. })
        ));
    }

    #[test]
    fn to_call_options_lifts_reasoning_items_to_assistant_reasoning() {
        // Reasoning input items become Assistant messages with Reasoning
        // content so providers that round-trip thinking (Anthropic) see them.
        // Chat-completions providers strip Reasoning per their own
        // convert_prompt and skip the empty assistant turn that remains.
        let json = r#"{
            "model": "opencode-go:glm-5.1",
            "input": [
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "Hi"}]},
                {"type": "reasoning", "id": "rs_1", "summary": [], "content": [{"type":"reasoning_text","text":"thinking..."}]}
            ]
        }"#;
        let req: ResponsesRequest = serde_json::from_str(json).expect("parse");
        let opts = to_call_options(req);
        assert_eq!(opts.prompt.len(), 2);
        assert!(matches!(opts.prompt[0], LanguageModelMessage::User { .. }));
        match &opts.prompt[1] {
            LanguageModelMessage::Assistant { content, .. } => {
                assert_eq!(content.len(), 1);
                assert!(matches!(
                    &content[0],
                    LanguageModelAssistantContent::Reasoning { text, .. } if text == "thinking..."
                ));
            }
            other => panic!("expected Assistant Reasoning, got {other:?}"),
        }
    }

    #[test]
    fn to_call_options_drops_other_unknown_items() {
        // Non-reasoning Unknown items (web_search_call, etc.) still drop.
        let json = r#"{
            "model": "x",
            "input": [
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "Hi"}]},
                {"type": "web_search_call", "id": "ws_1", "status": "completed"}
            ]
        }"#;
        let req: ResponsesRequest = serde_json::from_str(json).expect("parse");
        let opts = to_call_options(req);
        assert_eq!(opts.prompt.len(), 1);
        assert!(matches!(opts.prompt[0], LanguageModelMessage::User { .. }));
    }

    #[test]
    fn to_call_options_maps_assistant_role_to_assistant_message() {
        // Codex CLI sends prior turns with role=assistant; they must NOT be
        // demoted to User. Z.ai/GLM rejects payloads with no assistant turns
        // ("The prompt parameter was not received normally").
        let json = r#"{
            "model": "opencode-go:glm-5.1",
            "input": [
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "Hi"}]},
                {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "Hello"}]},
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "Continue"}]}
            ]
        }"#;
        let req: ResponsesRequest = serde_json::from_str(json).expect("parse");
        let opts = to_call_options(req);
        assert_eq!(opts.prompt.len(), 3);
        assert!(matches!(opts.prompt[0], LanguageModelMessage::User { .. }));
        match &opts.prompt[1] {
            LanguageModelMessage::Assistant { content, .. } => {
                assert!(matches!(
                    &content[0],
                    LanguageModelAssistantContent::Text { text, .. } if text == "Hello"
                ));
            }
            other => panic!("expected Assistant, got {other:?}"),
        }
        assert!(matches!(opts.prompt[2], LanguageModelMessage::User { .. }));
    }

    #[test]
    fn to_call_options_drops_empty_assistant_messages_from_codex() {
        // Codex CLI commonly emits assistant messages with empty
        // `output_text` because the visible content lives in a sibling
        // reasoning item. Dropping these prevents an invalid chat-completions
        // payload where many consecutive empty assistant turns confuse the
        // upstream's prompt-shape validator.
        let json = r#"{
            "model": "opencode-go:glm-5.1",
            "input": [
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "Hi"}]},
                {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": ""}]},
                {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": ""}]}
            ]
        }"#;
        let req: ResponsesRequest = serde_json::from_str(json).expect("parse");
        let opts = to_call_options(req);
        assert_eq!(opts.prompt.len(), 1);
        assert!(matches!(opts.prompt[0], LanguageModelMessage::User { .. }));
    }

    #[test]
    fn function_call_output_accepts_string_or_content_object() {
        // The Responses API allows `output` to be either a plain string or
        // a structured `{content: "..."}` / `{content_items: [...]}` object
        // (Codex emits the structured form for multimodal tool results).
        let plain = r#"{
            "model": "x",
            "input": [
                {"type": "function_call_output", "call_id": "c1", "output": "result text"}
            ]
        }"#;
        let req: ResponsesRequest = serde_json::from_str(plain).expect("plain output parses");
        let opts = to_call_options(req);
        match &opts.prompt[0] {
            LanguageModelMessage::Tool { content, .. } => match &content[0] {
                LanguageModelToolResult::ToolResult { output, .. } => match output {
                    LanguageModelToolResultOutput::Text { value, .. } => {
                        assert_eq!(value, "result text")
                    }
                    other => panic!("expected Text output, got {other:?}"),
                },
                other => panic!("expected ToolResult, got {other:?}"),
            },
            other => panic!("expected Tool message, got {other:?}"),
        }

        let structured = r#"{
            "model": "x",
            "input": [
                {
                    "type": "function_call_output",
                    "call_id": "c1",
                    "output": {"content_items": [{"type": "input_text", "text": "first"}, {"type": "input_text", "text": "second"}]}
                }
            ]
        }"#;
        let req: ResponsesRequest =
            serde_json::from_str(structured).expect("structured output parses");
        let opts = to_call_options(req);
        match &opts.prompt[0] {
            LanguageModelMessage::Tool { content, .. } => match &content[0] {
                LanguageModelToolResult::ToolResult { output, .. } => match output {
                    LanguageModelToolResultOutput::Text { value, .. } => {
                        assert_eq!(value, "first\nsecond")
                    }
                    other => panic!("expected Text output, got {other:?}"),
                },
                other => panic!("expected ToolResult, got {other:?}"),
            },
            other => panic!("expected Tool message, got {other:?}"),
        }
    }
}
