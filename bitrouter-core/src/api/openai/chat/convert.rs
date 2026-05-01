//! Conversion between OpenAI Chat Completions format and core LanguageModel types.

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
    ChatCompletionChoice, ChatCompletionChoiceMessage, ChatCompletionChunk,
    ChatCompletionChunkChoice, ChatCompletionChunkDelta, ChatCompletionRequest,
    ChatCompletionResponse, ChatCompletionUsage, ChatContentPart, ChatMessage, ChatMessageContent,
    ChatResponseToolCall, ChatResponseToolCallDelta, ChatResponseToolCallDeltaFunction,
    ChatResponseToolCallFunction, ChatToolChoice,
};
use crate::api::util::{generate_id, now_unix};

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

    let tool_choice = request.tool_choice.as_ref().and_then(convert_tool_choice);

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
        object: Some("chat.completion".to_owned()),
        created: now_unix(),
        model: model_id.to_owned(),
        system_fingerprint: None,
        choices: vec![ChatCompletionChoice {
            index: 0,
            message: ChatCompletionChoiceMessage {
                role: "assistant".to_owned(),
                content,
                refusal: None,
                tool_calls,
            },
            finish_reason: Some(finish_reason),
        }],
        usage: Some(ChatCompletionUsage {
            prompt_tokens: Some(prompt_tokens),
            completion_tokens: Some(completion_tokens),
            total_tokens: Some(prompt_tokens + completion_tokens),
            prompt_tokens_details: None,
            completion_tokens_details: None,
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
                        prompt_tokens: Some(prompt_tokens),
                        completion_tokens: Some(completion_tokens),
                        total_tokens: Some(prompt_tokens + completion_tokens),
                        prompt_tokens_details: None,
                        completion_tokens_details: None,
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
            object: Some("chat.completion.chunk".to_owned()),
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

fn convert_tool_choice(value: &ChatToolChoice) -> Option<LanguageModelToolChoice> {
    match value {
        ChatToolChoice::Mode(s) => match s.as_str() {
            "auto" => Some(LanguageModelToolChoice::Auto),
            "none" => Some(LanguageModelToolChoice::None),
            "required" => Some(LanguageModelToolChoice::Required),
            _ => None,
        },
        ChatToolChoice::Named { function, .. } => Some(LanguageModelToolChoice::Tool {
            tool_name: function.name.clone(),
        }),
    }
}

fn extract_content_and_tool_calls(
    blocks: &[LanguageModelContent],
) -> (Option<String>, Option<Vec<ChatResponseToolCall>>) {
    let mut text_parts: Vec<String> = Vec::new();
    let mut tool_calls: Vec<ChatResponseToolCall> = Vec::new();

    for block in blocks {
        match block {
            LanguageModelContent::Text { text, .. } => text_parts.push(text.clone()),
            LanguageModelContent::ToolCall {
                tool_call_id,
                tool_name,
                tool_input,
                ..
            } => tool_calls.push(ChatResponseToolCall {
                id: tool_call_id.clone(),
                r#type: "function".to_owned(),
                function: ChatResponseToolCallFunction {
                    name: tool_name.clone(),
                    arguments: tool_input.clone(),
                },
            }),
            _ => {}
        }
    }

    let content = if text_parts.is_empty() {
        None
    } else {
        Some(text_parts.join(""))
    };
    let tool_calls = if tool_calls.is_empty() {
        None
    } else {
        Some(tool_calls)
    };
    (content, tool_calls)
}

fn content_to_string(content: Option<ChatMessageContent>) -> String {
    match content {
        Some(ChatMessageContent::Text(s)) => s,
        Some(ChatMessageContent::Parts(parts)) => parts
            .into_iter()
            .filter_map(|p| match p {
                ChatContentPart::Text { text } => Some(text),
                ChatContentPart::ImageUrl { .. } => None,
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
            .filter_map(|p| match p {
                ChatContentPart::Text { text } => Some(LanguageModelUserContent::Text {
                    text,
                    provider_options: None,
                }),
                ChatContentPart::ImageUrl { .. } => None,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::language::{
        content::LanguageModelContent,
        finish_reason::LanguageModelFinishReason,
        generate_result::LanguageModelGenerateResult,
        stream_part::LanguageModelStreamPart,
        usage::{LanguageModelInputTokens, LanguageModelOutputTokens, LanguageModelUsage},
    };

    // ── Request Deserialization ─────────────────────────────────────────

    #[test]
    fn deserialize_simple_text_message_request() {
        let json = r#"{
            "model": "gpt-4",
            "messages": [{"role": "user", "content": "Hello"}]
        }"#;
        let req: ChatCompletionRequest = serde_json::from_str(json).expect("parse failed");
        assert_eq!(req.model, "gpt-4");
        assert_eq!(req.messages.len(), 1);
        assert_eq!(req.messages[0].role, "user");
        match &req.messages[0].content {
            Some(ChatMessageContent::Text(t)) => assert_eq!(t, "Hello"),
            other => panic!("expected Text content, got {other:?}"),
        }
        assert!(req.temperature.is_none());
        assert!(req.tools.is_none());
    }

    #[test]
    fn deserialize_system_and_user_message_request() {
        let json = r#"{
            "model": "gpt-4o",
            "messages": [
                {"role": "system", "content": "You are a helpful assistant."},
                {"role": "user", "content": "What is Rust?"}
            ]
        }"#;
        let req: ChatCompletionRequest = serde_json::from_str(json).expect("parse failed");
        assert_eq!(req.messages.len(), 2);
        assert_eq!(req.messages[0].role, "system");
        assert_eq!(req.messages[1].role, "user");
    }

    #[test]
    fn deserialize_multi_turn_conversation() {
        let json = r#"{
            "model": "gpt-4",
            "messages": [
                {"role": "system", "content": "You are helpful."},
                {"role": "user", "content": "Hi"},
                {"role": "assistant", "content": "Hello! How can I help?"},
                {"role": "user", "content": "Tell me about Rust."}
            ]
        }"#;
        let req: ChatCompletionRequest = serde_json::from_str(json).expect("parse failed");
        assert_eq!(req.messages.len(), 4);
        assert_eq!(req.messages[0].role, "system");
        assert_eq!(req.messages[1].role, "user");
        assert_eq!(req.messages[2].role, "assistant");
        assert_eq!(req.messages[3].role, "user");
    }

    #[test]
    fn deserialize_tool_use_request() {
        let json = r#"{
            "model": "gpt-4",
            "messages": [{"role": "user", "content": "What is the weather?"}],
            "tools": [
                {
                    "type": "function",
                    "function": {
                        "name": "get_weather",
                        "description": "Get the current weather",
                        "parameters": {
                            "type": "object",
                            "properties": {
                                "location": {"type": "string"}
                            },
                            "required": ["location"]
                        }
                    }
                }
            ],
            "tool_choice": "auto"
        }"#;
        let req: ChatCompletionRequest = serde_json::from_str(json).expect("parse failed");
        let tools = req.tools.as_ref().expect("tools missing");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].r#type, "function");
        assert_eq!(tools[0].function.name, "get_weather");
        assert_eq!(
            tools[0].function.description.as_deref(),
            Some("Get the current weather")
        );
        assert!(tools[0].function.parameters.is_some());
        match &req.tool_choice {
            Some(ChatToolChoice::Mode(s)) => assert_eq!(s, "auto"),
            other => panic!("expected Mode(\"auto\"), got {other:?}"),
        }
    }

    #[test]
    fn deserialize_tool_call_in_messages() {
        let json = r#"{
            "model": "gpt-4",
            "messages": [
                {"role": "user", "content": "What is the weather in SF?"},
                {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [
                        {
                            "id": "call_abc123",
                            "type": "function",
                            "function": {
                                "name": "get_weather",
                                "arguments": "{\"location\":\"San Francisco\"}"
                            }
                        }
                    ]
                },
                {
                    "role": "tool",
                    "tool_call_id": "call_abc123",
                    "name": "get_weather",
                    "content": "72°F and sunny"
                }
            ]
        }"#;
        let req: ChatCompletionRequest = serde_json::from_str(json).expect("parse failed");
        assert_eq!(req.messages.len(), 3);

        let assistant_msg = &req.messages[1];
        assert_eq!(assistant_msg.role, "assistant");
        let tc = assistant_msg
            .tool_calls
            .as_ref()
            .expect("tool_calls missing");
        assert_eq!(tc.len(), 1);
        assert_eq!(tc[0].id, "call_abc123");
        assert_eq!(tc[0].function.name, "get_weather");
        assert_eq!(tc[0].function.arguments, r#"{"location":"San Francisco"}"#);

        let tool_msg = &req.messages[2];
        assert_eq!(tool_msg.role, "tool");
        assert_eq!(tool_msg.tool_call_id.as_deref(), Some("call_abc123"));
        assert_eq!(tool_msg.name.as_deref(), Some("get_weather"));
    }

    #[test]
    fn deserialize_multi_part_content() {
        let json = r#"{
            "model": "gpt-4",
            "messages": [
                {
                    "role": "user",
                    "content": [
                        {"type": "text", "text": "Hello "},
                        {"type": "text", "text": "world!"}
                    ]
                }
            ]
        }"#;
        let req: ChatCompletionRequest = serde_json::from_str(json).expect("parse failed");
        match &req.messages[0].content {
            Some(ChatMessageContent::Parts(parts)) => {
                assert_eq!(parts.len(), 2);
                match &parts[0] {
                    ChatContentPart::Text { text } => assert_eq!(text, "Hello "),
                    other => panic!("expected Text, got {other:?}"),
                }
                match &parts[1] {
                    ChatContentPart::Text { text } => assert_eq!(text, "world!"),
                    other => panic!("expected Text, got {other:?}"),
                }
            }
            other => panic!("expected Parts content, got {other:?}"),
        }
    }

    #[test]
    fn deserialize_all_parameters_request() {
        let json = r#"{
            "model": "gpt-4",
            "messages": [{"role": "user", "content": "Hi"}],
            "temperature": 0.7,
            "top_p": 0.9,
            "max_tokens": 100,
            "max_completion_tokens": 200,
            "stop": ["\n", "END"],
            "presence_penalty": 0.5,
            "frequency_penalty": 0.3,
            "seed": 42,
            "stream": true,
            "parallel_tool_calls": false
        }"#;
        let req: ChatCompletionRequest = serde_json::from_str(json).expect("parse failed");
        assert!((req.temperature.expect("temp") - 0.7).abs() < f32::EPSILON);
        assert!((req.top_p.expect("top_p") - 0.9).abs() < f32::EPSILON);
        assert_eq!(req.max_tokens, Some(100));
        assert_eq!(req.max_completion_tokens, Some(200));
        assert_eq!(
            req.stop.as_deref(),
            Some(&["\n".to_owned(), "END".to_owned()][..])
        );
        assert!((req.presence_penalty.expect("pp") - 0.5).abs() < f32::EPSILON);
        assert!((req.frequency_penalty.expect("fp") - 0.3).abs() < f32::EPSILON);
        assert_eq!(req.seed, Some(42));
        assert_eq!(req.stream, Some(true));
        assert_eq!(req.parallel_tool_calls, Some(false));
    }

    // ── Response Serialization ──────────────────────────────────────────

    #[test]
    fn serialize_text_response() {
        let resp = ChatCompletionResponse {
            id: "chatcmpl-123".to_owned(),
            object: Some("chat.completion".to_owned()),
            created: 1700000000,
            model: "gpt-4".to_owned(),
            system_fingerprint: None,
            choices: vec![ChatCompletionChoice {
                index: 0,
                message: ChatCompletionChoiceMessage {
                    role: "assistant".to_owned(),
                    content: Some("Hello!".to_owned()),
                    refusal: None,
                    tool_calls: None,
                },
                finish_reason: Some("stop".to_owned()),
            }],
            usage: Some(ChatCompletionUsage {
                prompt_tokens: Some(10),
                completion_tokens: Some(5),
                total_tokens: Some(15),
                prompt_tokens_details: None,
                completion_tokens_details: None,
            }),
        };
        let val = serde_json::to_value(&resp).expect("serialize failed");
        assert_eq!(val["id"], "chatcmpl-123");
        assert_eq!(val["object"], "chat.completion");
        assert_eq!(val["choices"][0]["message"]["content"], "Hello!");
        assert_eq!(val["choices"][0]["finish_reason"], "stop");
        assert_eq!(val["usage"]["prompt_tokens"], 10);
        assert_eq!(val["usage"]["total_tokens"], 15);
        assert!(val["choices"][0]["message"].get("tool_calls").is_none());
    }

    #[test]
    fn serialize_tool_call_response() {
        let resp = ChatCompletionResponse {
            id: "chatcmpl-456".to_owned(),
            object: Some("chat.completion".to_owned()),
            created: 1700000000,
            model: "gpt-4".to_owned(),
            system_fingerprint: None,
            choices: vec![ChatCompletionChoice {
                index: 0,
                message: ChatCompletionChoiceMessage {
                    role: "assistant".to_owned(),
                    content: None,
                    refusal: None,
                    tool_calls: Some(vec![ChatResponseToolCall {
                        id: "call_abc".to_owned(),
                        r#type: "function".to_owned(),
                        function: ChatResponseToolCallFunction {
                            name: "get_weather".to_owned(),
                            arguments: r#"{"location":"NYC"}"#.to_owned(),
                        },
                    }]),
                },
                finish_reason: Some("tool_calls".to_owned()),
            }],
            usage: None,
        };
        let val = serde_json::to_value(&resp).expect("serialize failed");
        assert!(val["choices"][0]["message"]["content"].is_null());
        let tc = &val["choices"][0]["message"]["tool_calls"][0];
        assert_eq!(tc["id"], "call_abc");
        assert_eq!(tc["type"], "function");
        assert_eq!(tc["function"]["name"], "get_weather");
        assert_eq!(tc["function"]["arguments"], r#"{"location":"NYC"}"#);
    }

    #[test]
    fn serialize_streaming_chunk_text_delta() {
        let chunk = ChatCompletionChunk {
            id: "chatcmpl-stream-1".to_owned(),
            object: Some("chat.completion.chunk".to_owned()),
            created: 1700000000,
            model: "gpt-4".to_owned(),
            choices: vec![ChatCompletionChunkChoice {
                index: 0,
                delta: ChatCompletionChunkDelta {
                    role: Some("assistant".to_owned()),
                    content: Some("Hello".to_owned()),
                    tool_calls: None,
                },
                finish_reason: None,
            }],
            usage: None,
        };
        let val = serde_json::to_value(&chunk).expect("serialize failed");
        assert_eq!(val["object"], "chat.completion.chunk");
        assert_eq!(val["choices"][0]["delta"]["content"], "Hello");
        assert_eq!(val["choices"][0]["delta"]["role"], "assistant");
        assert!(val["choices"][0]["finish_reason"].is_null());
        assert!(val.get("usage").is_none());
    }

    #[test]
    fn serialize_streaming_chunk_tool_call() {
        use super::super::types::{ChatResponseToolCallDelta, ChatResponseToolCallDeltaFunction};
        let chunk = ChatCompletionChunk {
            id: "chatcmpl-stream-2".to_owned(),
            object: Some("chat.completion.chunk".to_owned()),
            created: 1700000000,
            model: "gpt-4".to_owned(),
            choices: vec![ChatCompletionChunkChoice {
                index: 0,
                delta: ChatCompletionChunkDelta {
                    role: None,
                    content: None,
                    tool_calls: Some(vec![ChatResponseToolCallDelta {
                        index: 0,
                        id: Some("call_abc".to_owned()),
                        r#type: Some("function".to_owned()),
                        function: Some(ChatResponseToolCallDeltaFunction {
                            name: Some("get_weather".to_owned()),
                            arguments: Some(r#"{"loc"#.to_owned()),
                        }),
                    }]),
                },
                finish_reason: None,
            }],
            usage: None,
        };
        let val = serde_json::to_value(&chunk).expect("serialize failed");
        let tc = &val["choices"][0]["delta"]["tool_calls"][0];
        assert_eq!(tc["index"], 0);
        assert_eq!(tc["id"], "call_abc");
        assert_eq!(tc["function"]["name"], "get_weather");
        assert_eq!(tc["function"]["arguments"], r#"{"loc"#);
    }

    #[test]
    fn serialize_streaming_chunk_finish() {
        let chunk = ChatCompletionChunk {
            id: "chatcmpl-stream-3".to_owned(),
            object: Some("chat.completion.chunk".to_owned()),
            created: 1700000000,
            model: "gpt-4".to_owned(),
            choices: vec![ChatCompletionChunkChoice {
                index: 0,
                delta: ChatCompletionChunkDelta {
                    role: None,
                    content: None,
                    tool_calls: None,
                },
                finish_reason: Some("stop".to_owned()),
            }],
            usage: Some(ChatCompletionUsage {
                prompt_tokens: Some(20),
                completion_tokens: Some(30),
                total_tokens: Some(50),
                prompt_tokens_details: None,
                completion_tokens_details: None,
            }),
        };
        let val = serde_json::to_value(&chunk).expect("serialize failed");
        assert_eq!(val["choices"][0]["finish_reason"], "stop");
        assert_eq!(val["usage"]["prompt_tokens"], 20);
        assert_eq!(val["usage"]["completion_tokens"], 30);
        assert_eq!(val["usage"]["total_tokens"], 50);
    }

    // ── Conversion: to_call_options ─────────────────────────────────────

    #[test]
    fn to_call_options_simple_request() {
        let json = r#"{
            "model": "gpt-4",
            "messages": [{"role": "user", "content": "Hello"}]
        }"#;
        let req: ChatCompletionRequest = serde_json::from_str(json).expect("parse failed");
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
        assert!(opts.tool_choice.is_none());
        assert!(opts.temperature.is_none());
    }

    #[test]
    fn to_call_options_with_tools() {
        let json = r#"{
            "model": "gpt-4",
            "messages": [{"role": "user", "content": "Hi"}],
            "tools": [
                {
                    "type": "function",
                    "function": {
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
                }
            ]
        }"#;
        let req: ChatCompletionRequest = serde_json::from_str(json).expect("parse failed");
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
    fn to_call_options_with_tool_choice_auto() {
        let json = r#"{
            "model": "gpt-4",
            "messages": [{"role": "user", "content": "Hi"}],
            "tool_choice": "auto"
        }"#;
        let req: ChatCompletionRequest = serde_json::from_str(json).expect("parse failed");
        let opts = to_call_options(req);
        match opts.tool_choice {
            Some(LanguageModelToolChoice::Auto) => {}
            other => panic!("expected Auto, got {other:?}"),
        }
    }

    #[test]
    fn to_call_options_with_tool_choice_none() {
        let json = r#"{
            "model": "gpt-4",
            "messages": [{"role": "user", "content": "Hi"}],
            "tool_choice": "none"
        }"#;
        let req: ChatCompletionRequest = serde_json::from_str(json).expect("parse failed");
        let opts = to_call_options(req);
        match opts.tool_choice {
            Some(LanguageModelToolChoice::None) => {}
            other => panic!("expected None choice, got {other:?}"),
        }
    }

    #[test]
    fn to_call_options_with_tool_choice_required() {
        let json = r#"{
            "model": "gpt-4",
            "messages": [{"role": "user", "content": "Hi"}],
            "tool_choice": "required"
        }"#;
        let req: ChatCompletionRequest = serde_json::from_str(json).expect("parse failed");
        let opts = to_call_options(req);
        match opts.tool_choice {
            Some(LanguageModelToolChoice::Required) => {}
            other => panic!("expected Required, got {other:?}"),
        }
    }

    #[test]
    fn to_call_options_with_tool_choice_specific_tool() {
        let json = r#"{
            "model": "gpt-4",
            "messages": [{"role": "user", "content": "Hi"}],
            "tool_choice": {"type": "function", "function": {"name": "get_weather"}}
        }"#;
        let req: ChatCompletionRequest = serde_json::from_str(json).expect("parse failed");
        let opts = to_call_options(req);
        match &opts.tool_choice {
            Some(LanguageModelToolChoice::Tool { tool_name }) => {
                assert_eq!(tool_name, "get_weather");
            }
            other => panic!("expected Tool choice, got {other:?}"),
        }
    }

    #[test]
    fn to_call_options_maps_all_parameters() {
        let json = r#"{
            "model": "gpt-4",
            "messages": [{"role": "user", "content": "Hi"}],
            "temperature": 0.5,
            "top_p": 0.8,
            "max_tokens": 100,
            "max_completion_tokens": 200,
            "stop": ["END"],
            "presence_penalty": 0.2,
            "frequency_penalty": 0.1,
            "seed": 99,
            "stream": true
        }"#;
        let req: ChatCompletionRequest = serde_json::from_str(json).expect("parse failed");
        let opts = to_call_options(req);
        assert!((opts.temperature.expect("temp") - 0.5).abs() < f32::EPSILON);
        assert!((opts.top_p.expect("top_p") - 0.8).abs() < f32::EPSILON);
        // max_completion_tokens takes priority over max_tokens
        assert_eq!(opts.max_output_tokens, Some(200));
        assert_eq!(
            opts.stop_sequences.as_deref(),
            Some(&["END".to_owned()][..])
        );
        assert!((opts.presence_penalty.expect("pp") - 0.2).abs() < f32::EPSILON);
        assert!((opts.frequency_penalty.expect("fp") - 0.1).abs() < f32::EPSILON);
        assert_eq!(opts.seed, Some(99));
        assert_eq!(opts.stream, Some(true));
    }

    #[test]
    fn to_call_options_max_tokens_fallback() {
        let json = r#"{
            "model": "gpt-4",
            "messages": [{"role": "user", "content": "Hi"}],
            "max_tokens": 150
        }"#;
        let req: ChatCompletionRequest = serde_json::from_str(json).expect("parse failed");
        let opts = to_call_options(req);
        assert_eq!(opts.max_output_tokens, Some(150));
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
        let resp = from_generate_result("gpt-4", result);
        assert_eq!(resp.object.as_deref(), Some("chat.completion"));
        assert_eq!(resp.model, "gpt-4");
        assert_eq!(resp.choices.len(), 1);
        assert_eq!(
            resp.choices[0].message.content.as_deref(),
            Some("Hello world")
        );
        assert!(resp.choices[0].message.tool_calls.is_none());
        assert_eq!(resp.choices[0].finish_reason.as_deref(), Some("stop"));
        let usage = resp.usage.expect("usage missing");
        assert_eq!(usage.prompt_tokens, Some(10));
        assert_eq!(usage.completion_tokens, Some(5));
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
        assert!(resp.choices[0].message.content.is_none());
        assert_eq!(resp.choices[0].finish_reason.as_deref(), Some("tool_calls"));
        let tc = resp.choices[0]
            .message
            .tool_calls
            .as_ref()
            .expect("tool_calls missing");
        assert_eq!(tc.len(), 1);
        assert_eq!(tc[0].id, "call_xyz");
        assert_eq!(tc[0].r#type, "function");
        assert_eq!(tc[0].function.name, "get_weather");
        assert_eq!(tc[0].function.arguments, r#"{"location":"NYC"}"#);
    }

    // Regression test for issue #416: a generate result containing both
    // assistant text and tool_call blocks must serialize as a single
    // assistant choice with both `content` and `tool_calls` populated.
    #[test]
    fn from_generate_result_text_and_tool_calls() {
        let result = LanguageModelGenerateResult {
            content: vec![
                LanguageModelContent::Text {
                    text: "Let me check.".to_owned(),
                    provider_metadata: None,
                },
                LanguageModelContent::ToolCall {
                    tool_call_id: "call_a".to_owned(),
                    tool_name: "get_weather".to_owned(),
                    tool_input: r#"{"location":"NYC"}"#.to_owned(),
                    provider_executed: None,
                    dynamic: None,
                    provider_metadata: None,
                },
            ],
            finish_reason: LanguageModelFinishReason::FunctionCall,
            usage: make_usage(10, 5),
            provider_metadata: None,
            request: None,
            response_metadata: None,
            warnings: None,
        };
        let resp = from_generate_result("gpt-4o", result);
        assert_eq!(
            resp.choices[0].message.content.as_deref(),
            Some("Let me check.")
        );
        let tc = resp.choices[0]
            .message
            .tool_calls
            .as_ref()
            .expect("tool_calls missing");
        assert_eq!(tc.len(), 1);
        assert_eq!(tc[0].id, "call_a");
        assert_eq!(tc[0].function.name, "get_weather");
        assert_eq!(resp.choices[0].finish_reason.as_deref(), Some("tool_calls"));
    }

    // ── Conversion: StreamConverter ─────────────────────────────────────

    #[test]
    fn stream_converter_text_delta() {
        let mut conv = StreamConverter::new("gpt-4".to_owned(), "stream-1".to_owned());
        let part = LanguageModelStreamPart::TextDelta {
            id: "t1".to_owned(),
            delta: "Hello".to_owned(),
            provider_metadata: None,
        };
        let chunk = conv.convert(&part).expect("should produce chunk");
        assert_eq!(chunk.object.as_deref(), Some("chat.completion.chunk"));
        assert_eq!(chunk.model, "gpt-4");
        assert_eq!(chunk.id, "stream-1");
        assert_eq!(chunk.choices[0].delta.content.as_deref(), Some("Hello"));
        assert!(chunk.choices[0].delta.tool_calls.is_none());
        assert!(chunk.choices[0].finish_reason.is_none());
    }

    #[test]
    fn stream_converter_tool_input_start() {
        let mut conv = StreamConverter::new("gpt-4".to_owned(), "stream-2".to_owned());
        let part = LanguageModelStreamPart::ToolInputStart {
            id: "call_1".to_owned(),
            tool_name: "search".to_owned(),
            provider_executed: None,
            dynamic: None,
            title: None,
            provider_metadata: None,
        };
        let chunk = conv.convert(&part).expect("should produce chunk");
        let tc = chunk.choices[0]
            .delta
            .tool_calls
            .as_ref()
            .expect("tool_calls missing");
        assert_eq!(tc.len(), 1);
        assert_eq!(tc[0].index, 0);
        assert_eq!(tc[0].id.as_deref(), Some("call_1"));
        assert_eq!(tc[0].r#type.as_deref(), Some("function"));
        assert_eq!(
            tc[0].function.as_ref().and_then(|f| f.name.as_deref()),
            Some("search")
        );
    }

    #[test]
    fn stream_converter_tool_input_delta() {
        let mut conv = StreamConverter::new("gpt-4".to_owned(), "stream-3".to_owned());
        // Start the tool first to register the index
        let start = LanguageModelStreamPart::ToolInputStart {
            id: "call_2".to_owned(),
            tool_name: "search".to_owned(),
            provider_executed: None,
            dynamic: None,
            title: None,
            provider_metadata: None,
        };
        conv.convert(&start);

        let part = LanguageModelStreamPart::ToolInputDelta {
            id: "call_2".to_owned(),
            delta: r#"{"query":"rust"}"#.to_owned(),
            provider_metadata: None,
        };
        let chunk = conv.convert(&part).expect("should produce chunk");
        let tc = chunk.choices[0]
            .delta
            .tool_calls
            .as_ref()
            .expect("tool_calls missing");
        assert_eq!(tc[0].index, 0);
        assert!(tc[0].id.is_none());
        assert!(tc[0].r#type.is_none());
        assert_eq!(
            tc[0].function.as_ref().and_then(|f| f.arguments.as_deref()),
            Some(r#"{"query":"rust"}"#)
        );
    }

    #[test]
    fn stream_converter_tool_call() {
        let mut conv = StreamConverter::new("gpt-4".to_owned(), "stream-4".to_owned());
        let part = LanguageModelStreamPart::ToolCall {
            tool_call_id: "call_full".to_owned(),
            tool_name: "calculator".to_owned(),
            tool_input: r#"{"expr":"2+2"}"#.to_owned(),
            provider_executed: None,
            dynamic: None,
            provider_metadata: None,
        };
        let chunk = conv.convert(&part).expect("should produce chunk");
        let tc = chunk.choices[0]
            .delta
            .tool_calls
            .as_ref()
            .expect("tool_calls missing");
        assert_eq!(tc[0].index, 0);
        assert_eq!(tc[0].id.as_deref(), Some("call_full"));
        assert_eq!(tc[0].r#type.as_deref(), Some("function"));
        let func = tc[0].function.as_ref().expect("function missing");
        assert_eq!(func.name.as_deref(), Some("calculator"));
        assert_eq!(func.arguments.as_deref(), Some(r#"{"expr":"2+2"}"#));
    }

    #[test]
    fn stream_converter_finish() {
        let mut conv = StreamConverter::new("gpt-4".to_owned(), "stream-5".to_owned());
        let part = LanguageModelStreamPart::Finish {
            usage: make_usage(25, 30),
            finish_reason: LanguageModelFinishReason::Stop,
            provider_metadata: None,
        };
        let chunk = conv.convert(&part).expect("should produce chunk");
        assert_eq!(chunk.choices[0].finish_reason.as_deref(), Some("stop"));
        assert!(chunk.choices[0].delta.content.is_none());
        assert!(chunk.choices[0].delta.tool_calls.is_none());
        let usage = chunk.usage.expect("usage missing");
        assert_eq!(usage.prompt_tokens, Some(25));
        assert_eq!(usage.completion_tokens, Some(30));
        assert_eq!(usage.total_tokens, Some(55));
    }

    #[test]
    fn stream_converter_finish_function_call_reason() {
        let mut conv = StreamConverter::new("gpt-4".to_owned(), "stream-6".to_owned());
        let part = LanguageModelStreamPart::Finish {
            usage: make_usage(10, 10),
            finish_reason: LanguageModelFinishReason::FunctionCall,
            provider_metadata: None,
        };
        let chunk = conv.convert(&part).expect("should produce chunk");
        assert_eq!(
            chunk.choices[0].finish_reason.as_deref(),
            Some("tool_calls")
        );
    }

    #[test]
    fn stream_converter_ignores_unknown_parts() {
        let mut conv = StreamConverter::new("gpt-4".to_owned(), "stream-7".to_owned());
        let part = LanguageModelStreamPart::TextStart {
            id: "t1".to_owned(),
            provider_metadata: None,
        };
        assert!(conv.convert(&part).is_none());
    }

    #[test]
    fn stream_converter_multiple_tools_get_sequential_indices() {
        let mut conv = StreamConverter::new("gpt-4".to_owned(), "stream-8".to_owned());

        let start1 = LanguageModelStreamPart::ToolInputStart {
            id: "call_a".to_owned(),
            tool_name: "tool_a".to_owned(),
            provider_executed: None,
            dynamic: None,
            title: None,
            provider_metadata: None,
        };
        let chunk1 = conv.convert(&start1).expect("chunk");
        assert_eq!(
            chunk1.choices[0].delta.tool_calls.as_ref().expect("tc")[0].index,
            0
        );

        let start2 = LanguageModelStreamPart::ToolInputStart {
            id: "call_b".to_owned(),
            tool_name: "tool_b".to_owned(),
            provider_executed: None,
            dynamic: None,
            title: None,
            provider_metadata: None,
        };
        let chunk2 = conv.convert(&start2).expect("chunk");
        assert_eq!(
            chunk2.choices[0].delta.tool_calls.as_ref().expect("tc")[0].index,
            1
        );
    }

    // ── Message Conversion ──────────────────────────────────────────────

    #[test]
    fn to_call_options_converts_system_message() {
        let json = r#"{
            "model": "gpt-4",
            "messages": [
                {"role": "system", "content": "Be helpful."},
                {"role": "user", "content": "Hi"}
            ]
        }"#;
        let req: ChatCompletionRequest = serde_json::from_str(json).expect("parse failed");
        let opts = to_call_options(req);
        match &opts.prompt[0] {
            LanguageModelMessage::System { content, .. } => {
                assert_eq!(content, "Be helpful.");
            }
            other => panic!("expected System message, got {other:?}"),
        }
    }

    #[test]
    fn to_call_options_converts_assistant_with_tool_calls() {
        let json = r#"{
            "model": "gpt-4",
            "messages": [
                {
                    "role": "assistant",
                    "content": "Let me check.",
                    "tool_calls": [
                        {
                            "id": "call_1",
                            "type": "function",
                            "function": {
                                "name": "lookup",
                                "arguments": "{\"q\":\"test\"}"
                            }
                        }
                    ]
                }
            ]
        }"#;
        let req: ChatCompletionRequest = serde_json::from_str(json).expect("parse failed");
        let opts = to_call_options(req);
        match &opts.prompt[0] {
            LanguageModelMessage::Assistant { content, .. } => {
                assert_eq!(content.len(), 2);
                match &content[0] {
                    LanguageModelAssistantContent::Text { text, .. } => {
                        assert_eq!(text, "Let me check.");
                    }
                    other => panic!("expected Text, got {other:?}"),
                }
                match &content[1] {
                    LanguageModelAssistantContent::ToolCall {
                        tool_call_id,
                        tool_name,
                        input,
                        ..
                    } => {
                        assert_eq!(tool_call_id, "call_1");
                        assert_eq!(tool_name, "lookup");
                        assert_eq!(input, &serde_json::json!({"q": "test"}));
                    }
                    other => panic!("expected ToolCall, got {other:?}"),
                }
            }
            other => panic!("expected Assistant message, got {other:?}"),
        }
    }

    #[test]
    fn to_call_options_converts_tool_message() {
        let json = r#"{
            "model": "gpt-4",
            "messages": [
                {
                    "role": "tool",
                    "tool_call_id": "call_1",
                    "name": "lookup",
                    "content": "result data"
                }
            ]
        }"#;
        let req: ChatCompletionRequest = serde_json::from_str(json).expect("parse failed");
        let opts = to_call_options(req);
        match &opts.prompt[0] {
            LanguageModelMessage::Tool { content, .. } => {
                assert_eq!(content.len(), 1);
                match &content[0] {
                    LanguageModelToolResult::ToolResult {
                        tool_call_id,
                        tool_name,
                        output,
                        ..
                    } => {
                        assert_eq!(tool_call_id, "call_1");
                        assert_eq!(tool_name, "lookup");
                        match output {
                            LanguageModelToolResultOutput::Text { value, .. } => {
                                assert_eq!(value, "result data");
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
    fn to_call_options_converts_multipart_user_content() {
        let json = r#"{
            "model": "gpt-4",
            "messages": [
                {
                    "role": "user",
                    "content": [
                        {"type": "text", "text": "Part A"},
                        {"type": "text", "text": "Part B"}
                    ]
                }
            ]
        }"#;
        let req: ChatCompletionRequest = serde_json::from_str(json).expect("parse failed");
        let opts = to_call_options(req);
        match &opts.prompt[0] {
            LanguageModelMessage::User { content, .. } => {
                assert_eq!(content.len(), 2);
                match &content[0] {
                    LanguageModelUserContent::Text { text, .. } => assert_eq!(text, "Part A"),
                    other => panic!("expected Text, got {other:?}"),
                }
                match &content[1] {
                    LanguageModelUserContent::Text { text, .. } => assert_eq!(text, "Part B"),
                    other => panic!("expected Text, got {other:?}"),
                }
            }
            other => panic!("expected User message, got {other:?}"),
        }
    }

    #[test]
    fn extract_model_name_returns_model() {
        let json = r#"{
            "model": "gpt-4-turbo",
            "messages": [{"role": "user", "content": "Hi"}]
        }"#;
        let req: ChatCompletionRequest = serde_json::from_str(json).expect("parse failed");
        assert_eq!(extract_model_name(&req), "gpt-4-turbo");
    }

    // ── Finish Reason Mapping ───────────────────────────────────────────

    #[test]
    fn map_finish_reason_all_variants() {
        assert_eq!(map_finish_reason(&LanguageModelFinishReason::Stop), "stop");
        assert_eq!(
            map_finish_reason(&LanguageModelFinishReason::Length),
            "length"
        );
        assert_eq!(
            map_finish_reason(&LanguageModelFinishReason::FunctionCall),
            "tool_calls"
        );
        assert_eq!(
            map_finish_reason(&LanguageModelFinishReason::ContentFilter),
            "content_filter"
        );
        assert_eq!(map_finish_reason(&LanguageModelFinishReason::Error), "stop");
        assert_eq!(
            map_finish_reason(&LanguageModelFinishReason::Other("unknown".to_owned())),
            "stop"
        );
    }
}
