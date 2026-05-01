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
        }
    }
    messages
}

fn input_content_to_string(content: Option<ResponsesInputContent>) -> String {
    match content {
        Some(ResponsesInputContent::Text(s)) => s,
        Some(ResponsesInputContent::Parts(parts)) => parts
            .into_iter()
            .filter_map(|p| match p {
                ResponsesInputContentPart::InputText { text } => Some(text),
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
                ResponsesInputContentPart::InputText { text } => LanguageModelUserContent::Text {
                    text,
                    provider_options: None,
                },
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
        ResponsesOutputContent, ResponsesOutputItem, ResponsesRequest, ResponsesResponse,
        ResponsesStreamEvent, ResponsesUsage,
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
            item_id: None,
            output_index: Some(0),
            content_index: Some(0),
            delta: Some("Hello".to_owned()),
            call_id: None,
            name: None,
            arguments: None,
            item: None,
        };
        let val = serde_json::to_value(&text_delta).expect("serialize failed");
        assert_eq!(val["type"], "response.output_text.delta");
        assert_eq!(val["delta"], "Hello");
        assert_eq!(val["output_index"], 0);
        assert_eq!(val["content_index"], 0);

        let fn_delta = ResponsesStreamEvent {
            event_type: "response.function_call_arguments.delta".to_owned(),
            item_id: None,
            output_index: Some(1),
            content_index: None,
            delta: Some(r#"{"loc"#.to_owned()),
            call_id: Some("call_1".to_owned()),
            name: None,
            arguments: None,
            item: None,
        };
        let val = serde_json::to_value(&fn_delta).expect("serialize failed");
        assert_eq!(val["type"], "response.function_call_arguments.delta");
        assert_eq!(val["delta"], r#"{"loc"#);
        assert_eq!(val["call_id"], "call_1");

        let completed = ResponsesStreamEvent {
            event_type: "response.completed".to_owned(),
            item_id: None,
            output_index: None,
            content_index: None,
            delta: None,
            call_id: None,
            name: None,
            arguments: None,
            item: None,
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
    fn stream_converter_text_delta() {
        let mut conv = StreamConverter::new();
        let part = LanguageModelStreamPart::TextDelta {
            id: "t1".to_owned(),
            delta: "Hello".to_owned(),
            provider_metadata: None,
        };
        let events = conv.convert(&part);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_type, "response.output_text.delta");
        assert_eq!(events[0].delta.as_deref(), Some("Hello"));
        assert_eq!(events[0].output_index, Some(0));
        assert_eq!(events[0].content_index, Some(0));
    }

    #[test]
    fn stream_converter_tool_call() {
        let mut conv = StreamConverter::new();
        let part = LanguageModelStreamPart::ToolCall {
            tool_call_id: "call_full".to_owned(),
            tool_name: "calculator".to_owned(),
            tool_input: r#"{"expr":"2+2"}"#.to_owned(),
            provider_executed: None,
            dynamic: None,
            provider_metadata: None,
        };
        let events = conv.convert(&part);
        assert_eq!(events.len(), 4);

        // First: output_item.added
        assert_eq!(events[0].event_type, "response.output_item.added");
        assert_eq!(events[0].output_index, Some(0));
        let item = events[0].item.as_ref().expect("item missing");
        assert_eq!(item["type"], "function_call");
        assert_eq!(item["status"], "in_progress");

        // Second: function_call_arguments.delta
        assert_eq!(
            events[1].event_type,
            "response.function_call_arguments.delta"
        );
        assert_eq!(events[1].delta.as_deref(), Some(r#"{"expr":"2+2"}"#));
        assert_eq!(events[1].call_id.as_deref(), Some("call_full"));

        // Third: function_call_arguments.done
        assert_eq!(
            events[2].event_type,
            "response.function_call_arguments.done"
        );
        assert_eq!(events[2].arguments.as_deref(), Some(r#"{"expr":"2+2"}"#));

        // Fourth: output_item.done
        assert_eq!(events[3].event_type, "response.output_item.done");
        let done_item = events[3].item.as_ref().expect("item missing");
        assert_eq!(done_item["status"], "completed");
        assert_eq!(done_item["name"], "calculator");
    }

    #[test]
    fn stream_converter_tool_input_start_delta() {
        let mut conv = StreamConverter::new();

        // Start event
        let start = LanguageModelStreamPart::ToolInputStart {
            id: "call_a".to_owned(),
            tool_name: "search".to_owned(),
            provider_executed: None,
            dynamic: None,
            title: None,
            provider_metadata: None,
        };
        let events = conv.convert(&start);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_type, "response.output_item.added");
        assert_eq!(events[0].output_index, Some(0));
        assert_eq!(events[0].name.as_deref(), Some("search"));
        let item = events[0].item.as_ref().expect("item missing");
        assert_eq!(item["type"], "function_call");
        assert_eq!(item["name"], "search");
        assert_eq!(item["status"], "in_progress");

        // Delta event uses the tracked index
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
    fn stream_converter_tool_input_end_is_empty() {
        let mut conv = StreamConverter::new();
        let part = LanguageModelStreamPart::ToolInputEnd {
            id: "call_a".to_owned(),
            provider_metadata: None,
        };
        let events = conv.convert(&part);
        assert!(events.is_empty());
    }

    #[test]
    fn stream_converter_finish() {
        let mut conv = StreamConverter::new();
        let part = LanguageModelStreamPart::Finish {
            usage: make_usage(25, 30),
            finish_reason: LanguageModelFinishReason::Stop,
            provider_metadata: None,
        };
        let events = conv.convert(&part);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_type, "response.completed");
        assert!(events[0].delta.is_none());
        assert!(events[0].output_index.is_none());
    }

    #[test]
    fn stream_converter_ignores_unknown_parts() {
        let mut conv = StreamConverter::new();
        let part = LanguageModelStreamPart::TextStart {
            id: "t1".to_owned(),
            provider_metadata: None,
        };
        let events = conv.convert(&part);
        assert!(events.is_empty());
    }

    #[test]
    fn stream_converter_multiple_tools_sequential_indices() {
        let mut conv = StreamConverter::new();

        let start1 = LanguageModelStreamPart::ToolInputStart {
            id: "call_a".to_owned(),
            tool_name: "tool_a".to_owned(),
            provider_executed: None,
            dynamic: None,
            title: None,
            provider_metadata: None,
        };
        let events1 = conv.convert(&start1);
        assert_eq!(events1[0].output_index, Some(0));

        let start2 = LanguageModelStreamPart::ToolInputStart {
            id: "call_b".to_owned(),
            tool_name: "tool_b".to_owned(),
            provider_executed: None,
            dynamic: None,
            title: None,
            provider_metadata: None,
        };
        let events2 = conv.convert(&start2);
        assert_eq!(events2[0].output_index, Some(1));
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
}
