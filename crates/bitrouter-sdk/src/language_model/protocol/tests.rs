//! Phase-2 protocol-conversion tests: the 4×4 inbound/outbound matrix plus the
//! v0 bug-regression suite.

use crate::language_model::protocol::*;
use crate::language_model::types::*;

// ===== fixtures =====

/// Helper trait combining inbound + outbound; the 4 built-in adapter structs
/// implement both so the matrix tests can use a single handle for each
/// protocol.
trait BothAdapter: InboundAdapter + OutboundAdapter {}
impl<T: InboundAdapter + OutboundAdapter> BothAdapter for T {}

/// Test-only lookup that returns one of the four built-in adapters as a
/// `BothAdapter` so the matrix tests can call inbound + outbound methods on
/// the same value.
fn adapter_for(protocol: ApiProtocol) -> Box<dyn BothAdapter> {
    match protocol {
        ApiProtocol::ChatCompletions => Box::new(chat_completions::ChatCompletionsAdapter),
        ApiProtocol::Messages => Box::new(messages::MessagesAdapter),
        ApiProtocol::Responses => Box::new(responses::ResponsesAdapter),
        ApiProtocol::GenerateContent => Box::new(generate_content::GenerateContentAdapter),
        ApiProtocol::Custom(_) => unreachable!("test helper only handles built-in protocols"),
    }
}

fn all_protocols() -> [ApiProtocol; 4] {
    [
        ApiProtocol::Messages,
        ApiProtocol::ChatCompletions,
        ApiProtocol::Responses,
        ApiProtocol::GenerateContent,
    ]
}

/// A canonical prompt exercising system + a user message + a tool definition.
fn sample_prompt() -> Prompt {
    Prompt {
        model: "test-model".to_string(),
        system: Some("be brief".to_string()),
        messages: vec![Message::text(Role::User, "what is 2+2?")],
        tools: vec![Tool::Function {
            name: "calculator".to_string(),
            description: Some("does math".to_string()),
            parameters: serde_json::json!({ "type": "object" }),
            strict: None,
        }],
        params: GenerationParams {
            temperature: Some(0.5),
            max_tokens: Some(256),
            ..Default::default()
        },
        response_format: None,
        stream: false,
    }
}

/// A canonical result with reasoning + text + a tool call, in that order — the
/// order must survive every conversion (v0 #416, #454-1).
fn sample_result() -> GenerateResult {
    GenerateResult {
        content: vec![
            Content::Reasoning {
                text: "thinking...".to_string(),
            },
            Content::Text {
                text: "the answer is 4".to_string(),
            },
            Content::ToolCall {
                id: "call_1".to_string(),
                name: "calculator".to_string(),
                arguments: "{\"op\":\"add\"}".to_string(),
                provider_executed: false,
            },
        ],
        usage: Some(Usage {
            prompt_tokens: 12,
            completion_tokens: 8,
            reasoning_tokens: 3,
            ..Default::default()
        }),
        finish_reason: Some(FinishReason::Stop),
        response_id: None,
        stop_details: None,
    }
}

fn text_of(content: &[Content]) -> String {
    content
        .iter()
        .filter_map(|c| match c {
            Content::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

// ===== 4×4 conversion matrix =====

/// The full inbound→outbound matrix: exercise all six conversion functions for
/// every (inbound, outbound) pair and assert the request body and the response
/// text survive the round trip.
#[test]
fn conversion_matrix_4x4_non_streaming() {
    for inbound_proto in all_protocols() {
        for outbound_proto in all_protocols() {
            let inbound = adapter_for(inbound_proto.clone());
            let outbound = adapter_for(outbound_proto.clone());
            let canonical = sample_prompt();

            // client → router (inbound parse of an inbound-rendered request)
            let client_req = inbound
                .render_request(&canonical)
                .unwrap_or_else(|e| panic!("{inbound_proto:?} render_request: {e}"));
            let prompt = inbound
                .parse_request(client_req)
                .unwrap_or_else(|e| panic!("{inbound_proto:?} parse_request: {e}"));
            assert_eq!(
                prompt.messages.len(),
                1,
                "{inbound_proto:?}→{outbound_proto:?}: message survived inbound round trip"
            );
            assert_eq!(prompt.messages[0].role, Role::User);

            // router → provider (outbound render)
            let provider_req = outbound.render_request(&prompt).unwrap_or_else(|e| {
                panic!("{inbound_proto:?}→{outbound_proto:?} outbound render_request: {e}")
            });
            assert!(provider_req.is_object());

            // provider → router (outbound parse of an outbound-rendered response)
            let provider_resp = outbound
                .render_response(&sample_result(), &prompt, "resp_1")
                .unwrap_or_else(|e| panic!("{outbound_proto:?} render_response: {e}"));
            let result = outbound.parse_response(provider_resp).unwrap_or_else(|e| {
                panic!("{inbound_proto:?}→{outbound_proto:?} parse_response: {e}")
            });
            assert_eq!(
                text_of(&result.content),
                "the answer is 4",
                "{inbound_proto:?}→{outbound_proto:?}: response text survived"
            );

            // router → client (inbound render)
            let client_resp = inbound
                .render_response(&result, &prompt, "resp_1")
                .unwrap_or_else(|e| panic!("{inbound_proto:?} render_response: {e}"));
            assert!(
                client_resp.is_object(),
                "{inbound_proto:?}→{outbound_proto:?}: client response is a JSON object"
            );
        }
    }
}

/// The streaming 4×4 matrix: for every (inbound, outbound) pair, encode a
/// canonical part stream in the outbound protocol, decode it back, and assert
/// the text/reasoning/tool-call parts survive — then re-encode in the inbound
/// protocol.
#[test]
fn conversion_matrix_4x4_streaming() {
    let canonical_parts = vec![
        StreamPart::ReasoningDelta {
            text: "hmm ".to_string(),
        },
        StreamPart::TextDelta {
            text: "the ".to_string(),
        },
        StreamPart::TextDelta {
            text: "answer".to_string(),
        },
        StreamPart::ToolCallDelta {
            id: "call_9".to_string(),
            name: Some("calc".to_string()),
            arguments: "{\"x\":1}".to_string(),
        },
        StreamPart::Usage {
            usage: Usage {
                prompt_tokens: 5,
                completion_tokens: 3,
                reasoning_tokens: 1,
                ..Default::default()
            },
        },
        StreamPart::Finish {
            reason: FinishReason::Stop,
        },
    ];

    for outbound_proto in all_protocols() {
        let outbound = adapter_for(outbound_proto.clone());
        // encode canonical → outbound SSE frames
        let mut encoder = outbound.stream_encoder("resp_s", "test-model");
        let mut frames = Vec::new();
        for part in &canonical_parts {
            frames.extend(encoder.encode(part).unwrap());
        }
        frames.extend(encoder.finish().unwrap());

        // decode outbound SSE frames → canonical parts
        let mut decoder = outbound.stream_decoder();
        let mut decoded = Vec::new();
        for frame in &frames {
            if let SseFrame::Event { event, data } = frame {
                let sse = SseEvent {
                    event: event.clone(),
                    data: data.clone(),
                };
                decoded.extend(decoder.decode(&sse).unwrap());
            }
        }
        decoded.extend(decoder.finish().unwrap());

        let text: String = decoded
            .iter()
            .filter_map(|p| match p {
                StreamPart::TextDelta { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            text, "the answer",
            "{outbound_proto:?}: streaming text survived encode→decode"
        );
        assert!(
            decoded
                .iter()
                .any(|p| matches!(p, StreamPart::ReasoningDelta { .. })),
            "{outbound_proto:?}: reasoning delta survived"
        );
        assert!(
            decoded
                .iter()
                .any(|p| matches!(p, StreamPart::ToolCallDelta { .. })),
            "{outbound_proto:?}: tool-call delta survived"
        );
        assert!(
            decoded.iter().any(|p| p.is_terminal()),
            "{outbound_proto:?}: terminal part (Finish / ResponseCompleted) survived"
        );

        // and the decoded stream re-encodes in every inbound protocol
        for inbound_proto in all_protocols() {
            let inbound = adapter_for(inbound_proto.clone());
            let mut enc = inbound.stream_encoder("resp_s", "test-model");
            for part in &decoded {
                enc.encode(part).unwrap_or_else(|e| {
                    panic!("{inbound_proto:?} re-encode of {outbound_proto:?} stream: {e}")
                });
            }
        }
    }
}

// ===== per-adapter unit tests =====

/// Each outbound adapter must extract the provider-native response id
/// from a non-streaming body into `GenerateResult.response_id` so the
/// observe plugin can stamp it onto the OTel `gen_ai.response.id`
/// attribute (current GenAI semconv:
/// <https://opentelemetry.io/docs/specs/semconv/gen-ai/gen-ai-spans/>).
#[test]
fn outbound_adapters_extract_response_id() {
    // Chat Completions: top-level `id` (`chatcmpl-...`).
    let openai_chat = adapter_for(ApiProtocol::ChatCompletions);
    let body = serde_json::json!({
        "id": "chatcmpl-abc123",
        "object": "chat.completion",
        "model": "gpt-test",
        "choices": [{"index": 0, "message": {"role": "assistant", "content": "hi"}, "finish_reason": "stop"}],
    });
    assert_eq!(
        openai_chat
            .parse_response(body)
            .unwrap()
            .response_id
            .as_deref(),
        Some("chatcmpl-abc123"),
        "Chat Completions must extract top-level `id`"
    );

    // Messages: top-level `id` (`msg_...`).
    let anthropic = adapter_for(ApiProtocol::Messages);
    let body = serde_json::json!({
        "id": "msg_01ABC",
        "type": "message",
        "role": "assistant",
        "content": [{"type": "text", "text": "hi"}],
        "stop_reason": "end_turn",
    });
    assert_eq!(
        anthropic
            .parse_response(body)
            .unwrap()
            .response_id
            .as_deref(),
        Some("msg_01ABC"),
        "Anthropic must extract top-level `id`"
    );

    // Generate Content: top-level `responseId`.
    let google = adapter_for(ApiProtocol::GenerateContent);
    let body = serde_json::json!({
        "responseId": "google-resp-xyz",
        "candidates": [{"content": {"parts": [{"text": "hi"}]}, "finishReason": "STOP"}],
    });
    assert_eq!(
        google.parse_response(body).unwrap().response_id.as_deref(),
        Some("google-resp-xyz"),
        "Google must extract `responseId`"
    );

    // Responses: top-level `id` (`resp_...`).
    let responses = adapter_for(ApiProtocol::Responses);
    let body = serde_json::json!({
        "id": "resp_abc789",
        "object": "response",
        "status": "completed",
        "output": [{"type": "message", "content": [{"text": "hi"}]}],
    });
    assert_eq!(
        responses
            .parse_response(body)
            .unwrap()
            .response_id
            .as_deref(),
        Some("resp_abc789"),
        "Responses must extract top-level `id`"
    );

    // Absent id: graceful None.
    let openai_chat = adapter_for(ApiProtocol::ChatCompletions);
    let body = serde_json::json!({
        "choices": [{"index": 0, "message": {"role": "assistant", "content": "hi"}, "finish_reason": "stop"}],
    });
    assert_eq!(
        openai_chat.parse_response(body).unwrap().response_id,
        None,
        "missing provider id must surface as None, not panic"
    );
}

#[test]
fn chat_completions_request_roundtrip() {
    let adapter = adapter_for(ApiProtocol::ChatCompletions);
    let prompt = sample_prompt();
    let json = adapter.render_request(&prompt).unwrap();
    assert_eq!(json["model"], "test-model");
    assert_eq!(json["messages"][0]["role"], "system");
    assert_eq!(json["temperature"], 0.5);
    let parsed = adapter.parse_request(json).unwrap();
    assert_eq!(parsed.system.as_deref(), Some("be brief"));
    assert_eq!(parsed.tools.len(), 1);
}

#[test]
fn chat_completions_passes_through_uncommon_params() {
    // tool_choice, stop, seed, response_format, n, presence/frequency_penalty,
    // logit_bias, logprobs, top_logprobs, user, parallel_tool_calls,
    // stream_options — every field without a typed slot survives parse → render.
    let adapter = adapter_for(ApiProtocol::ChatCompletions);
    let body = serde_json::json!({
        "model": "gpt-5",
        "messages": [{"role": "user", "content": "hi"}],
        "tool_choice": {"type": "function", "function": {"name": "search"}},
        "stop": ["END", "STOP"],
        "seed": 7,
        "response_format": {"type": "json_object"},
        "n": 2,
        "presence_penalty": 0.5,
        "frequency_penalty": -0.25,
        "parallel_tool_calls": false,
        "logit_bias": {"1234": -100},
        "user": "alice"
    });
    let prompt = adapter.parse_request(body.clone()).unwrap();
    let rendered = adapter.render_request(&prompt).unwrap();
    for key in [
        "tool_choice",
        "stop",
        "seed",
        "response_format",
        "n",
        "presence_penalty",
        "frequency_penalty",
        "parallel_tool_calls",
        "logit_bias",
        "user",
    ] {
        assert_eq!(
            rendered[key], body[key],
            "Chat Completions `{key}` must survive parse/render"
        );
    }
}

#[test]
fn messages_passes_through_uncommon_params() {
    let adapter = adapter_for(ApiProtocol::Messages);
    let body = serde_json::json!({
        "model": "claude-sonnet-4-6",
        "messages": [{"role": "user", "content": "hi"}],
        "max_tokens": 1024,
        "tool_choice": {"type": "auto"},
        "stop_sequences": ["END"],
        "top_k": 40,
        "metadata": {"user_id": "alice"},
        "thinking": {"type": "enabled", "budget_tokens": 1000}
    });
    let prompt = adapter.parse_request(body.clone()).unwrap();
    let rendered = adapter.render_request(&prompt).unwrap();
    for key in [
        "tool_choice",
        "stop_sequences",
        "top_k",
        "metadata",
        "thinking",
    ] {
        assert_eq!(
            rendered[key], body[key],
            "Anthropic `{key}` must survive parse/render"
        );
    }
}

#[test]
fn generate_content_passes_through_uncommon_generation_config() {
    let adapter = adapter_for(ApiProtocol::GenerateContent);
    let body = serde_json::json!({
        "model": "gemini-2.0-flash",
        "contents": [{"role": "user", "parts": [{"text": "hi"}]}],
        "generationConfig": {
            "temperature": 0.5,
            "stopSequences": ["END"],
            "topK": 40,
            "seed": 7,
            "responseMimeType": "application/json",
            "responseSchema": {"type": "object"},
            "presencePenalty": 0.1,
            "frequencyPenalty": -0.1
        }
    });
    let prompt = adapter.parse_request(body.clone()).unwrap();
    let rendered = adapter.render_request(&prompt).unwrap();
    let gc = &rendered["generationConfig"];
    for key in [
        "stopSequences",
        "topK",
        "seed",
        "responseMimeType",
        "responseSchema",
        "presencePenalty",
        "frequencyPenalty",
    ] {
        assert_eq!(
            gc[key], body["generationConfig"][key],
            "Google generationConfig.{key} must survive parse/render"
        );
    }
}

// ===== structured outputs (response_format) =====

/// A canonical prompt carrying a JSON-Schema response_format constraint.
fn sample_prompt_with_schema() -> Prompt {
    Prompt {
        response_format: Some(ResponseFormat::JsonSchema {
            name: Some("weather".to_string()),
            strict: Some(true),
            schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "location": {"type": "string"},
                    "temperature": {"type": "number"}
                },
                "required": ["location", "temperature"],
                "additionalProperties": false
            }),
        }),
        ..sample_prompt()
    }
}

#[test]
fn chat_completions_inbound_promotes_json_schema_response_format() {
    let adapter = adapter_for(ApiProtocol::ChatCompletions);
    let body = serde_json::json!({
        "model": "gpt-4o",
        "messages": [{"role": "user", "content": "weather?"}],
        "response_format": {
            "type": "json_schema",
            "json_schema": {
                "name": "weather",
                "strict": true,
                "schema": {"type": "object", "properties": {"x": {"type": "string"}}}
            }
        }
    });
    let prompt = adapter.parse_request(body).unwrap();
    match prompt.response_format {
        Some(ResponseFormat::JsonSchema {
            name,
            strict,
            schema,
        }) => {
            assert_eq!(name.as_deref(), Some("weather"));
            assert_eq!(strict, Some(true));
            assert_eq!(schema["properties"]["x"]["type"], "string");
        }
        other => panic!("expected JsonSchema, got {other:?}"),
    }
    // Promoted out of extras to avoid double-rendering.
    assert!(!prompt.params.extra.contains_key("response_format"));
}

#[test]
fn chat_completions_inbound_leaves_json_object_in_extras() {
    // The legacy `{type: "json_object"}` JSON mode has no schema to translate,
    // so it must keep passing through opaquely.
    let adapter = adapter_for(ApiProtocol::ChatCompletions);
    let body = serde_json::json!({
        "model": "gpt-4o",
        "messages": [{"role": "user", "content": "hi"}],
        "response_format": {"type": "json_object"}
    });
    let prompt = adapter.parse_request(body.clone()).unwrap();
    assert!(prompt.response_format.is_none());
    let rendered = adapter.render_request(&prompt).unwrap();
    assert_eq!(rendered["response_format"], body["response_format"]);
}

#[test]
fn chat_completions_outbound_renders_json_schema() {
    let adapter = adapter_for(ApiProtocol::ChatCompletions);
    let rendered = adapter
        .render_request(&sample_prompt_with_schema())
        .unwrap();
    assert_eq!(rendered["response_format"]["type"], "json_schema");
    assert_eq!(
        rendered["response_format"]["json_schema"]["name"],
        "weather"
    );
    assert_eq!(rendered["response_format"]["json_schema"]["strict"], true);
    assert_eq!(
        rendered["response_format"]["json_schema"]["schema"]["properties"]["location"]["type"],
        "string"
    );
}

#[test]
fn chat_completions_outbound_supplies_default_name() {
    // Chat Completions requires `name`; renderer fills it when absent (e.g. when
    // the inbound was Anthropic/Google which carry no name).
    let adapter = adapter_for(ApiProtocol::ChatCompletions);
    let prompt = Prompt {
        response_format: Some(ResponseFormat::JsonSchema {
            name: None,
            strict: None,
            schema: serde_json::json!({"type": "object"}),
        }),
        ..sample_prompt()
    };
    let rendered = adapter.render_request(&prompt).unwrap();
    assert_eq!(
        rendered["response_format"]["json_schema"]["name"],
        "response"
    );
    // strict is omitted (None) rather than serialised as null.
    assert!(
        rendered["response_format"]["json_schema"]
            .get("strict")
            .is_none()
    );
}

#[test]
fn messages_inbound_promotes_output_config_format() {
    let adapter = adapter_for(ApiProtocol::Messages);
    let body = serde_json::json!({
        "model": "claude-opus-4-7",
        "max_tokens": 1024,
        "messages": [{"role": "user", "content": "weather?"}],
        "output_config": {
            "format": {
                "type": "json_schema",
                "schema": {"type": "object", "properties": {"x": {"type": "string"}}}
            }
        }
    });
    let prompt = adapter.parse_request(body).unwrap();
    match prompt.response_format {
        Some(ResponseFormat::JsonSchema { schema, .. }) => {
            assert_eq!(schema["properties"]["x"]["type"], "string");
        }
        other => panic!("expected JsonSchema, got {other:?}"),
    }
    assert!(!prompt.params.extra.contains_key("output_config"));
}

#[test]
fn messages_inbound_accepts_legacy_output_format_alias() {
    // The deprecated flat `output_format` shape (pre-GA, still emitted by
    // some clients — vercel/ai#12298) must still parse cleanly.
    let adapter = adapter_for(ApiProtocol::Messages);
    let body = serde_json::json!({
        "model": "claude-opus-4-7",
        "max_tokens": 1024,
        "messages": [{"role": "user", "content": "weather?"}],
        "output_format": {
            "type": "json_schema",
            "schema": {"type": "object"}
        }
    });
    let prompt = adapter.parse_request(body).unwrap();
    assert!(matches!(
        prompt.response_format,
        Some(ResponseFormat::JsonSchema { .. })
    ));
    assert!(!prompt.params.extra.contains_key("output_format"));
}

#[test]
fn messages_inbound_legacy_alias_does_not_disturb_output_config_siblings() {
    // If the legacy `output_format` alias is what matched, an unrelated
    // `output_config` blob the client supplied must be left fully intact in
    // extras so its siblings (`unknown_key` here) survive the round trip.
    let adapter = adapter_for(ApiProtocol::Messages);
    let body = serde_json::json!({
        "model": "claude-opus-4-7",
        "max_tokens": 1024,
        "messages": [{"role": "user", "content": "weather?"}],
        "output_config": {"unknown_key": "x"},
        "output_format": {"type": "json_schema", "schema": {"type": "object"}}
    });
    let prompt = adapter.parse_request(body).unwrap();
    assert!(matches!(
        prompt.response_format,
        Some(ResponseFormat::JsonSchema { .. })
    ));
    assert_eq!(prompt.params.extra["output_config"]["unknown_key"], "x");
    assert!(!prompt.params.extra.contains_key("output_format"));
}

#[test]
fn messages_outbound_renders_output_config_format() {
    let adapter = adapter_for(ApiProtocol::Messages);
    let rendered = adapter
        .render_request(&sample_prompt_with_schema())
        .unwrap();
    assert_eq!(rendered["output_config"]["format"]["type"], "json_schema");
    assert_eq!(
        rendered["output_config"]["format"]["schema"]["properties"]["location"]["type"],
        "string"
    );
    // Messages carries no `name` / `strict` — confirm they're dropped, not
    // forwarded as unknown fields.
    assert!(rendered["output_config"]["format"].get("name").is_none());
    assert!(rendered["output_config"]["format"].get("strict").is_none());
}

#[test]
fn messages_inbound_promotes_output_config_effort() {
    // Anthropic's GA reasoning `effort` knob (`output_config.effort`) is lifted
    // into the canonical `reasoning_effort` and stripped from the pass-through
    // extras so the outbound adapter renders it exactly once.
    let adapter = adapter_for(ApiProtocol::Messages);
    let body = serde_json::json!({
        "model": "claude-opus-4-8",
        "max_tokens": 1024,
        "messages": [{"role": "user", "content": "hi"}],
        "output_config": {"effort": "high"}
    });
    let prompt = adapter.parse_request(body).unwrap();
    assert_eq!(prompt.params.reasoning_effort.as_deref(), Some("high"));
    assert!(!prompt.params.extra.contains_key("output_config"));
}

#[test]
fn messages_inbound_promotes_output_config_format_and_effort() {
    // `format` + `effort` under one `output_config`: both are promoted to their
    // canonical slots and the now-empty `output_config` is dropped from extras.
    let adapter = adapter_for(ApiProtocol::Messages);
    let body = serde_json::json!({
        "model": "claude-opus-4-8",
        "max_tokens": 1024,
        "messages": [{"role": "user", "content": "hi"}],
        "output_config": {
            "effort": "max",
            "format": {"type": "json_schema", "schema": {"type": "object"}}
        }
    });
    let prompt = adapter.parse_request(body).unwrap();
    assert_eq!(prompt.params.reasoning_effort.as_deref(), Some("max"));
    assert!(matches!(
        prompt.response_format,
        Some(ResponseFormat::JsonSchema { .. })
    ));
    assert!(!prompt.params.extra.contains_key("output_config"));
}

#[test]
fn messages_inbound_effort_preserves_unknown_output_config_siblings() {
    // Lifting `effort` must leave unrelated `output_config` siblings intact.
    let adapter = adapter_for(ApiProtocol::Messages);
    let body = serde_json::json!({
        "model": "claude-opus-4-8",
        "max_tokens": 1024,
        "messages": [{"role": "user", "content": "hi"}],
        "output_config": {"effort": "low", "unknown_key": "x"}
    });
    let prompt = adapter.parse_request(body).unwrap();
    assert_eq!(prompt.params.reasoning_effort.as_deref(), Some("low"));
    assert_eq!(prompt.params.extra["output_config"]["unknown_key"], "x");
    assert!(prompt.params.extra["output_config"].get("effort").is_none());
}

#[test]
fn messages_outbound_renders_output_config_effort() {
    let adapter = adapter_for(ApiProtocol::Messages);
    let prompt = Prompt {
        model: "claude-opus-4-8".to_string(),
        system: None,
        messages: vec![Message::text(Role::User, "hi")],
        tools: vec![],
        params: GenerationParams {
            reasoning_effort: Some("high".to_string()),
            ..Default::default()
        },
        response_format: None,
        stream: false,
    };
    let rendered = adapter.render_request(&prompt).unwrap();
    assert_eq!(rendered["output_config"]["effort"], "high");
}

#[test]
fn messages_outbound_merges_format_and_effort_into_output_config() {
    // `response_format` + `reasoning_effort` must coexist under one
    // `output_config` object rather than one clobbering the other.
    let adapter = adapter_for(ApiProtocol::Messages);
    let prompt = Prompt {
        model: "claude-opus-4-8".to_string(),
        system: None,
        messages: vec![Message::text(Role::User, "hi")],
        tools: vec![],
        params: GenerationParams {
            reasoning_effort: Some("xhigh".to_string()),
            ..Default::default()
        },
        response_format: Some(ResponseFormat::JsonSchema {
            name: None,
            strict: None,
            schema: serde_json::json!({"type": "object"}),
        }),
        stream: false,
    };
    let rendered = adapter.render_request(&prompt).unwrap();
    assert_eq!(rendered["output_config"]["effort"], "xhigh");
    assert_eq!(rendered["output_config"]["format"]["type"], "json_schema");
}

#[test]
fn messages_outbound_preserves_unknown_output_config_sibling_with_effort() {
    // The inbound adapter leaves unknown output_config siblings in extra after
    // lifting effort; the outbound render must merge them back rather than drop
    // them when it rebuilds output_config.
    let adapter = adapter_for(ApiProtocol::Messages);
    let body = serde_json::json!({
        "model": "claude-opus-4-8",
        "max_tokens": 1024,
        "messages": [{"role": "user", "content": "hi"}],
        "output_config": {"effort": "high", "unknown_key": "x"}
    });
    let prompt = adapter.parse_request(body).unwrap();
    let rendered = adapter.render_request(&prompt).unwrap();
    assert_eq!(rendered["output_config"]["effort"], "high");
    assert_eq!(rendered["output_config"]["unknown_key"], "x");
}

#[test]
fn effort_routes_messages_to_chat_completions() {
    // Cross-protocol (reverse direction): a Messages client's
    // output_config.effort reaches an OpenAI-style upstream as reasoning_effort.
    let messages = adapter_for(ApiProtocol::Messages);
    let cc = adapter_for(ApiProtocol::ChatCompletions);
    let body = serde_json::json!({
        "model": "claude-opus-4-8",
        "max_tokens": 1024,
        "messages": [{"role": "user", "content": "hi"}],
        "output_config": {"effort": "high"}
    });
    let prompt = messages.parse_request(body).unwrap();
    let rendered = cc.render_request(&prompt).unwrap();
    assert_eq!(rendered["reasoning_effort"], "high");
}

#[test]
fn effort_routes_chat_completions_to_messages() {
    // Cross-protocol: a Chat Completions client's `reasoning_effort` reaches an
    // Anthropic Messages upstream as `output_config.effort`.
    let cc = adapter_for(ApiProtocol::ChatCompletions);
    let messages = adapter_for(ApiProtocol::Messages);
    let body = serde_json::json!({
        "model": "claude-opus-4-8",
        "messages": [{"role": "user", "content": "hi"}],
        "reasoning_effort": "high"
    });
    let prompt = cc.parse_request(body).unwrap();
    let rendered = messages.render_request(&prompt).unwrap();
    assert_eq!(rendered["output_config"]["effort"], "high");
}

#[test]
fn effort_routes_messages_to_responses() {
    // Cross-protocol: a Messages client's `output_config.effort` reaches a
    // Responses upstream as `reasoning.effort`.
    let messages = adapter_for(ApiProtocol::Messages);
    let responses = adapter_for(ApiProtocol::Responses);
    let body = serde_json::json!({
        "model": "claude-opus-4-8",
        "max_tokens": 1024,
        "messages": [{"role": "user", "content": "hi"}],
        "output_config": {"effort": "max"}
    });
    let prompt = messages.parse_request(body).unwrap();
    let rendered = responses.render_request(&prompt).unwrap();
    assert_eq!(rendered["reasoning"]["effort"], "max");
}

#[test]
fn messages_inbound_accepts_mid_conversation_system() {
    // Opus 4.8 mid-conversation system messages: a `role:"system"` entry at a
    // non-first position parses into a canonical System-role message in place.
    let adapter = adapter_for(ApiProtocol::Messages);
    let body = serde_json::json!({
        "model": "claude-opus-4-8",
        "max_tokens": 1024,
        "messages": [
            {"role": "user", "content": "hi"},
            {"role": "system", "content": "Terse mode: keep replies under 40 words."},
            {"role": "assistant", "content": "ok"}
        ]
    });
    let prompt = adapter.parse_request(body).unwrap();
    assert_eq!(prompt.messages.len(), 3);
    assert_eq!(prompt.messages[1].role, Role::System);
    assert_eq!(
        text_of(&prompt.messages[1].content),
        "Terse mode: keep replies under 40 words."
    );
}

#[test]
fn messages_outbound_renders_mid_conversation_system() {
    // A canonical System-role message renders as a `role:"system"` entry so the
    // request is serialized faithfully; the upstream model decides support. The
    // top-level system instruction still rides the out-of-band `system` field.
    let adapter = adapter_for(ApiProtocol::Messages);
    let prompt = Prompt {
        model: "claude-opus-4-8".to_string(),
        system: Some("top-level system".to_string()),
        messages: vec![
            Message::text(Role::User, "hi"),
            Message::text(Role::System, "switch to terse mode"),
        ],
        tools: vec![],
        params: GenerationParams::default(),
        response_format: None,
        stream: false,
    };
    let rendered = adapter.render_request(&prompt).unwrap();
    assert_eq!(rendered["system"], "top-level system");
    assert_eq!(rendered["messages"][1]["role"], "system");
    assert_eq!(
        rendered["messages"][1]["content"][0]["text"],
        "switch to terse mode"
    );
}

#[test]
fn messages_mid_conversation_system_round_trips() {
    // Messages -> canonical -> Messages preserves both the top-level system and
    // an interleaved mid-conversation system message, in order.
    let adapter = adapter_for(ApiProtocol::Messages);
    let body = serde_json::json!({
        "model": "claude-opus-4-8",
        "max_tokens": 1024,
        "system": "you are helpful",
        "messages": [
            {"role": "user", "content": "hi"},
            {"role": "system", "content": "be terse"},
            {"role": "user", "content": "go"}
        ]
    });
    let prompt = adapter.parse_request(body).unwrap();
    let rendered = adapter.render_request(&prompt).unwrap();
    assert_eq!(rendered["system"], "you are helpful");
    let msgs = rendered["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 3);
    assert_eq!(msgs[1]["role"], "system");
    assert_eq!(msgs[1]["content"][0]["text"], "be terse");
}

#[test]
fn mid_conversation_system_routes_messages_to_chat_completions() {
    // Cross-protocol: a Messages client's mid-conversation system message reaches
    // an OpenAI-style upstream as an in-place `role:"system"` message.
    let messages = adapter_for(ApiProtocol::Messages);
    let cc = adapter_for(ApiProtocol::ChatCompletions);
    let body = serde_json::json!({
        "model": "claude-opus-4-8",
        "max_tokens": 1024,
        "messages": [
            {"role": "user", "content": "hi"},
            {"role": "system", "content": "be terse"}
        ]
    });
    let prompt = messages.parse_request(body).unwrap();
    let rendered = cc.render_request(&prompt).unwrap();
    let msgs = rendered["messages"].as_array().unwrap();
    assert!(
        msgs.iter()
            .any(|m| m["role"] == "system" && m.to_string().contains("be terse")),
        "interleaved system survives cross-protocol routing: {msgs:?}"
    );
}

#[test]
fn messages_inbound_parses_refusal_stop_details() {
    // Opus 4.8 refusals carry a stop_details object {type, category,
    // explanation}; surface category + explanation on the canonical result.
    let adapter = adapter_for(ApiProtocol::Messages);
    let body = serde_json::json!({
        "id": "msg_1",
        "type": "message",
        "role": "assistant",
        "model": "claude-opus-4-8",
        "content": [],
        "stop_reason": "refusal",
        "stop_details": {"type": "refusal", "category": "cyber", "explanation": "declined"},
        "usage": {"input_tokens": 5, "output_tokens": 0}
    });
    let result = adapter.parse_response(body).unwrap();
    assert_eq!(result.finish_reason, Some(FinishReason::ContentFilter));
    let details = result.stop_details.expect("stop_details present");
    assert_eq!(details.category.as_deref(), Some("cyber"));
    assert_eq!(details.explanation.as_deref(), Some("declined"));
}

#[test]
fn messages_parse_response_omits_stop_details_when_absent() {
    let adapter = adapter_for(ApiProtocol::Messages);
    let body = serde_json::json!({
        "id": "msg_1",
        "type": "message",
        "role": "assistant",
        "model": "claude-opus-4-8",
        "content": [{"type": "text", "text": "hi"}],
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 5, "output_tokens": 1}
    });
    let result = adapter.parse_response(body).unwrap();
    assert!(result.stop_details.is_none());
}

#[test]
fn messages_outbound_renders_refusal_stop_details() {
    let adapter = adapter_for(ApiProtocol::Messages);
    let result = GenerateResult {
        content: vec![],
        usage: None,
        finish_reason: Some(FinishReason::ContentFilter),
        response_id: None,
        stop_details: Some(StopDetails {
            category: Some("bio".to_string()),
            explanation: None,
        }),
    };
    let rendered = adapter
        .render_response(&result, &sample_prompt(), "msg_1")
        .unwrap();
    assert_eq!(rendered["stop_reason"], "refusal");
    assert_eq!(rendered["stop_details"]["type"], "refusal");
    assert_eq!(rendered["stop_details"]["category"], "bio");
    // explanation was None -> omitted, not serialised as null.
    assert!(rendered["stop_details"].get("explanation").is_none());
}

#[test]
fn refusal_stop_details_round_trips_messages() {
    // Messages -> canonical -> Messages preserves the refusal category and
    // explanation in Anthropic's wire shape.
    let adapter = adapter_for(ApiProtocol::Messages);
    let body = serde_json::json!({
        "id": "msg_1",
        "type": "message",
        "role": "assistant",
        "model": "claude-opus-4-8",
        "content": [],
        "stop_reason": "refusal",
        "stop_details": {"type": "refusal", "category": "cyber", "explanation": "no"},
        "usage": {"input_tokens": 3, "output_tokens": 0}
    });
    let result = adapter.parse_response(body).unwrap();
    let rendered = adapter
        .render_response(&result, &sample_prompt(), "msg_1")
        .unwrap();
    assert_eq!(rendered["stop_details"]["category"], "cyber");
    assert_eq!(rendered["stop_details"]["explanation"], "no");
}

#[test]
fn generate_content_inbound_promotes_response_schema() {
    let adapter = adapter_for(ApiProtocol::GenerateContent);
    let body = serde_json::json!({
        "model": "gemini-2.5-pro",
        "contents": [{"role": "user", "parts": [{"text": "weather?"}]}],
        "generationConfig": {
            "responseMimeType": "application/json",
            "responseSchema": {"type": "object", "properties": {"x": {"type": "string"}}}
        }
    });
    let prompt = adapter.parse_request(body).unwrap();
    match prompt.response_format {
        Some(ResponseFormat::JsonSchema { schema, .. }) => {
            assert_eq!(schema["properties"]["x"]["type"], "string");
        }
        other => panic!("expected JsonSchema, got {other:?}"),
    }
    assert!(!prompt.params.extra.contains_key("responseSchema"));
    assert!(!prompt.params.extra.contains_key("responseMimeType"));
}

#[test]
fn generate_content_inbound_leaves_enum_mime_in_extras() {
    // `text/x.enum` has no JSON schema; must stay in extras for opaque
    // Generate Content-native pass-through.
    let adapter = adapter_for(ApiProtocol::GenerateContent);
    let body = serde_json::json!({
        "model": "gemini-2.5-pro",
        "contents": [{"role": "user", "parts": [{"text": "x"}]}],
        "generationConfig": {
            "responseMimeType": "text/x.enum"
        }
    });
    let prompt = adapter.parse_request(body.clone()).unwrap();
    assert!(prompt.response_format.is_none());
    let rendered = adapter.render_request(&prompt).unwrap();
    assert_eq!(
        rendered["generationConfig"]["responseMimeType"],
        "text/x.enum"
    );
}

#[test]
fn generate_content_outbound_renders_response_schema() {
    let adapter = adapter_for(ApiProtocol::GenerateContent);
    let rendered = adapter
        .render_request(&sample_prompt_with_schema())
        .unwrap();
    let gc = &rendered["generationConfig"];
    assert_eq!(gc["responseMimeType"], "application/json");
    assert_eq!(
        gc["responseSchema"]["properties"]["location"]["type"],
        "string"
    );
}

#[test]
fn responses_inbound_promotes_text_format() {
    let adapter = adapter_for(ApiProtocol::Responses);
    let body = serde_json::json!({
        "model": "gpt-4o",
        "input": "weather?",
        "text": {
            "format": {
                "type": "json_schema",
                "name": "weather",
                "strict": true,
                "schema": {"type": "object"}
            },
            "verbosity": "low"
        }
    });
    let prompt = adapter.parse_request(body).unwrap();
    assert!(matches!(
        prompt.response_format,
        Some(ResponseFormat::JsonSchema { .. })
    ));
    // Sibling keys under `text` survive opaquely (the `format` child was
    // promoted, the rest of `text` stays in extras).
    assert_eq!(prompt.params.extra["text"]["verbosity"], "low");
    assert!(prompt.params.extra["text"].get("format").is_none());
}

#[test]
fn responses_inbound_leaves_json_object_in_extras() {
    // `text.format: {type: "json_object"}` (and `{type: "text"}`) carries no
    // schema to translate, so it is not promoted to the canonical slot — it
    // passes through opaquely and round-trips unchanged. Mirrors the Chat
    // Completions json_object case.
    let adapter = adapter_for(ApiProtocol::Responses);
    let body = serde_json::json!({
        "model": "gpt-4o",
        "input": "weather?",
        "text": {"format": {"type": "json_object"}}
    });
    let prompt = adapter.parse_request(body.clone()).unwrap();
    assert!(prompt.response_format.is_none());
    let rendered = adapter.render_request(&prompt).unwrap();
    assert_eq!(rendered["text"], body["text"]);
}

#[test]
fn responses_outbound_renders_text_format() {
    let adapter = adapter_for(ApiProtocol::Responses);
    let rendered = adapter
        .render_request(&sample_prompt_with_schema())
        .unwrap();
    assert_eq!(rendered["text"]["format"]["type"], "json_schema");
    assert_eq!(rendered["text"]["format"]["name"], "weather");
    assert_eq!(rendered["text"]["format"]["strict"], true);
}

#[test]
fn responses_outbound_merges_text_siblings() {
    // When an inbound supplied a `text` object with `verbosity` (or any
    // other future sibling), the outbound render must preserve it alongside
    // the synthesised `format`.
    let adapter = adapter_for(ApiProtocol::Responses);
    let mut prompt = sample_prompt_with_schema();
    prompt
        .params
        .extra
        .insert("text".into(), serde_json::json!({ "verbosity": "low" }));
    let rendered = adapter.render_request(&prompt).unwrap();
    assert_eq!(rendered["text"]["verbosity"], "low");
    assert_eq!(rendered["text"]["format"]["type"], "json_schema");
}

#[test]
fn response_format_survives_cross_protocol_routing() {
    // Set the canonical field once and assert every outbound adapter emits
    // the right native shape — this is the cross-protocol guarantee.
    let prompt = sample_prompt_with_schema();
    let schema = match prompt.response_format.as_ref().unwrap() {
        ResponseFormat::JsonSchema { schema, .. } => schema.clone(),
    };

    // Chat Completions
    let chat = adapter_for(ApiProtocol::ChatCompletions)
        .render_request(&prompt)
        .unwrap();
    assert_eq!(chat["response_format"]["json_schema"]["schema"], schema);

    // Messages
    let ant = adapter_for(ApiProtocol::Messages)
        .render_request(&prompt)
        .unwrap();
    assert_eq!(ant["output_config"]["format"]["schema"], schema);

    // Generate Content
    let g = adapter_for(ApiProtocol::GenerateContent)
        .render_request(&prompt)
        .unwrap();
    assert_eq!(g["generationConfig"]["responseSchema"], schema);
    assert_eq!(
        g["generationConfig"]["responseMimeType"],
        "application/json"
    );

    // Responses
    let r = adapter_for(ApiProtocol::Responses)
        .render_request(&prompt)
        .unwrap();
    assert_eq!(r["text"]["format"]["schema"], schema);
}

#[test]
fn builtin_adapters_advertise_response_format_support() {
    for proto in all_protocols() {
        let a = adapter_for(proto.clone());
        assert!(
            a.supports_response_format(),
            "{proto:?} should advertise response_format support"
        );
    }
}

#[test]
fn messages_no_beta_header_is_emitted() {
    // The deprecated `anthropic-beta: structured-outputs-2025-11-13` header
    // is no longer required by the Anthropic GA endpoint and is actively
    // rejected by Vertex AI (vercel/ai#10981). The Anthropic transport must
    // not introduce it as a side effect of structured outputs.
    use crate::language_model::protocol::Transport;
    use crate::language_model::types::RoutingTarget;
    let transport = crate::language_model::protocol::messages::MessagesTransport;
    let client = reqwest::Client::new();
    let req = client
        .post("http://example.invalid/v1/messages")
        .build()
        .unwrap();
    let target = RoutingTarget {
        provider_name: "anthropic".into(),
        service_id: "claude-opus-4-7".into(),
        api_base: "http://example.invalid".into(),
        api_key: "k".into(),
        api_protocol: ApiProtocol::Messages,
        account_label: None,
        api_key_override: None,
        api_base_override: None,
        auth_scheme: Default::default(),
    };
    let req = futures::executor::block_on(transport.authorise(req, &target)).unwrap();
    assert!(
        req.headers().get("anthropic-beta").is_none(),
        "anthropic-beta header must not be set by the transport (deprecated and Vertex-incompatible)"
    );
}

#[test]
fn messages_auth_scheme_selects_one_credential_header() {
    // The Messages transport honours `RoutingTarget::auth_scheme`: `x-api-key`
    // by default, `Authorization: Bearer` when asked — and never both, since
    // the Anthropic API rejects a request carrying both credential headers.
    use crate::language_model::protocol::Transport;
    use crate::language_model::types::{AuthScheme, RoutingTarget};
    let transport = crate::language_model::protocol::messages::MessagesTransport;
    let client = reqwest::Client::new();
    let base = RoutingTarget {
        provider_name: "gw".into(),
        service_id: "claude".into(),
        api_base: "http://example.invalid".into(),
        api_key: "secret".into(),
        api_protocol: ApiProtocol::Messages,
        account_label: None,
        api_key_override: None,
        api_base_override: None,
        auth_scheme: AuthScheme::XApiKey,
    };

    // Default (x-api-key) scheme → `x-api-key` only.
    let req = client
        .post("http://example.invalid/v1/messages")
        .build()
        .unwrap();
    let req = futures::executor::block_on(transport.authorise(req, &base)).unwrap();
    assert_eq!(
        req.headers().get("x-api-key").unwrap().to_str().unwrap(),
        "secret"
    );
    assert!(req.headers().get(reqwest::header::AUTHORIZATION).is_none());

    // Bearer scheme → `Authorization: Bearer` only.
    let bearer = RoutingTarget {
        auth_scheme: AuthScheme::Bearer,
        ..base.clone()
    };
    let req = client
        .post("http://example.invalid/v1/messages")
        .build()
        .unwrap();
    let req = futures::executor::block_on(transport.authorise(req, &bearer)).unwrap();
    assert_eq!(
        req.headers()
            .get(reqwest::header::AUTHORIZATION)
            .unwrap()
            .to_str()
            .unwrap(),
        "Bearer secret"
    );
    assert!(req.headers().get("x-api-key").is_none());
}

#[test]
fn messages_cache_tokens_round_trip() {
    // Messages prompt caching exposes `cache_read_input_tokens` and
    // `cache_creation_input_tokens` in `usage`. Parser captures them, encoder
    // emits them on the non-streaming response, and on `message_delta` they
    // accompany the streaming finalisation.
    //
    // SDK contract: `cache_read_tokens` / `cache_write_tokens` are **subsets
    // of** `prompt_tokens` (matches Chat Completions / Generate Content). Messages' wire format
    // is the opposite — `input_tokens` is the uncached portion, reported
    // alongside the cache buckets — so the parser folds the cache totals
    // back into `prompt_tokens` and the renderer unfolds them when writing
    // the wire payload.
    let adapter = adapter_for(ApiProtocol::Messages);
    let body = serde_json::json!({
        "id": "msg_1",
        "type": "message",
        "role": "assistant",
        "model": "claude-3-7-sonnet",
        "content": [{"type": "text", "text": "ok"}],
        "stop_reason": "end_turn",
        "usage": {
            "input_tokens": 100,
            "output_tokens": 50,
            "cache_read_input_tokens": 80,
            "cache_creation_input_tokens": 20
        }
    });
    let result = adapter.parse_response(body).unwrap();
    let usage = result.usage.unwrap();
    assert_eq!(usage.cache_read_tokens, 80);
    assert_eq!(usage.cache_write_tokens, 20);
    // Canonical IR: prompt_tokens is the inclusive total (100 + 80 + 20).
    assert_eq!(usage.prompt_tokens, 200);
    let rendered = adapter
        .render_response(&result, &sample_prompt(), "msg_1")
        .unwrap();
    assert_eq!(rendered["usage"]["cache_read_input_tokens"], 80);
    assert_eq!(rendered["usage"]["cache_creation_input_tokens"], 20);
    // Wire format: input_tokens excludes the cache buckets (audit1 §13).
    // Round-trips to the same 100 the upstream sent.
    assert_eq!(rendered["usage"]["input_tokens"], 100);
}

#[test]
fn messages_cache_tokens_match_subset_contract() {
    // Regression: the canonical `Usage` doc-comment states that
    // `cache_read_tokens` and `cache_write_tokens` are **subsets of**
    // `prompt_tokens`. Anthropic's wire payload uses the opposite
    // convention (uncached `input_tokens` + sibling cache fields), so
    // the parser must fold the cache buckets into `prompt_tokens` or
    // downstream billing layers that compute `no_cache = prompt_tokens
    // - cache_read - cache_write` saturate to 0 on cache-heavy
    // requests and silently undercharge the uncached portion.
    let adapter = adapter_for(ApiProtocol::Messages);
    let body = serde_json::json!({
        "id": "msg_1",
        "type": "message",
        "role": "assistant",
        "model": "claude-sonnet-4",
        "content": [{"type": "text", "text": "ok"}],
        "stop_reason": "end_turn",
        "usage": {
            // A realistic cache-hit-heavy request: 5K uncached prompt,
            // 30K cache reads, 0 cache writes. Without folding into
            // `prompt_tokens`, the billing-layer subtraction (5000 -
            // 30000 - 0) saturates to 0 and the 5000 uncached tokens
            // get billed at $0.
            "input_tokens": 5_000,
            "output_tokens": 200,
            "cache_read_input_tokens": 30_000,
        }
    });
    let usage = adapter.parse_response(body).unwrap().usage.unwrap();
    assert_eq!(usage.cache_read_tokens, 30_000);
    assert_eq!(usage.cache_write_tokens, 0);
    assert_eq!(usage.prompt_tokens, 35_000);
    // Holds the doc-comment invariant byte-for-byte.
    assert!(
        usage.prompt_tokens >= usage.cache_read_tokens + usage.cache_write_tokens,
        "Usage::prompt_tokens must be inclusive of cache buckets: \
         got prompt={} cache_read={} cache_write={}",
        usage.prompt_tokens,
        usage.cache_read_tokens,
        usage.cache_write_tokens,
    );
}

#[test]
fn messages_usage_with_no_cache_fields_keeps_prompt_tokens_unchanged() {
    // Belt-and-braces: when an upstream omits the cache fields entirely
    // (the common case for non-Anthropic Anthropic-API-compatible
    // upstreams, or Anthropic requests without prompt caching), the
    // canonical `prompt_tokens` must equal Anthropic's wire-level
    // `input_tokens` — the cache-fold is a no-op when both cache
    // totals are 0.
    let adapter = adapter_for(ApiProtocol::Messages);
    let body = serde_json::json!({
        "id": "msg_1",
        "type": "message",
        "role": "assistant",
        "model": "claude-sonnet-4",
        "content": [{"type": "text", "text": "ok"}],
        "stop_reason": "end_turn",
        "usage": {
            "input_tokens": 1_234,
            "output_tokens": 56,
        }
    });
    let usage = adapter.parse_response(body).unwrap().usage.unwrap();
    assert_eq!(usage.prompt_tokens, 1_234);
    assert_eq!(usage.cache_read_tokens, 0);
    assert_eq!(usage.cache_write_tokens, 0);
}

#[test]
fn messages_stream_preserves_cache_inclusive_prompt_tokens() {
    // The stream decoder receives `message_start` (which carries the
    // full cache breakdown) and `message_delta` (which typically only
    // refreshes `output_tokens`). The terminal Usage frame must reflect
    // the inclusive prompt_tokens contract — the test pins this so a
    // future refactor of the delta path can't quietly drop the
    // cache-fold and undercharge again.
    let decoder = adapter_for(ApiProtocol::Messages);
    let mut stream_decoder = decoder.stream_decoder();

    let start = SseEvent {
        event: Some("message_start".into()),
        data: serde_json::json!({
            "type": "message_start",
            "message": {
                "id": "msg_1",
                "type": "message",
                "role": "assistant",
                "model": "claude-sonnet-4",
                "content": [],
                "stop_reason": null,
                "usage": {
                    "input_tokens": 5_000,
                    "output_tokens": 0,
                    "cache_read_input_tokens": 30_000,
                    "cache_creation_input_tokens": 20,
                }
            }
        })
        .to_string(),
    };
    let _ = stream_decoder.decode(&start).unwrap();

    // A message_delta that only carries `output_tokens` (the common
    // shape per Anthropic streaming docs) must NOT zero out the cache
    // buckets nor revert prompt_tokens to the wire-level exclusive value.
    let delta = SseEvent {
        event: Some("message_delta".into()),
        data: serde_json::json!({
            "type": "message_delta",
            "delta": { "stop_reason": "end_turn" },
            "usage": { "output_tokens": 200 }
        })
        .to_string(),
    };
    let parts = stream_decoder.decode(&delta).unwrap();
    let usage = parts
        .iter()
        .find_map(|p| match p {
            StreamPart::Usage { usage } => Some(*usage),
            _ => None,
        })
        .expect("terminal Usage frame missing");
    assert_eq!(usage.cache_read_tokens, 30_000);
    assert_eq!(usage.cache_write_tokens, 20);
    assert_eq!(usage.completion_tokens, 200);
    assert_eq!(usage.prompt_tokens, 5_000 + 30_000 + 20);
}

#[test]
fn chat_completions_cache_tokens_round_trip() {
    // Chat Completions surfaces cached prompt tokens via
    // `prompt_tokens_details.cached_tokens`. Parse → canonical → render.
    let adapter = adapter_for(ApiProtocol::ChatCompletions);
    let body = serde_json::json!({
        "id": "c1",
        "choices": [{"message": {"role": "assistant", "content": "ok"}, "finish_reason": "stop"}],
        "usage": {
            "prompt_tokens": 100,
            "completion_tokens": 50,
            "prompt_tokens_details": {"cached_tokens": 70}
        }
    });
    let result = adapter.parse_response(body).unwrap();
    let usage = result.usage.unwrap();
    assert_eq!(usage.cache_read_tokens, 70);
    let rendered = adapter
        .render_response(&result, &sample_prompt(), "c1")
        .unwrap();
    assert_eq!(
        rendered["usage"]["prompt_tokens_details"]["cached_tokens"],
        70
    );
}

#[test]
fn chat_completions_parse_captures_refusal_and_reasoning_aliases() {
    // `message.refusal` (when non-empty) is the OpenAI refusal text; carry it
    // as a Content::Text and set FinishReason::ContentFilter regardless of
    // what `finish_reason` says (OpenAI sometimes also says "content_filter"
    // but not always). `message.reasoning` / `message.thinking` are
    // OpenAI-compatible vendor aliases for `reasoning_content`.
    let adapter = adapter_for(ApiProtocol::ChatCompletions);

    // refusal
    let body = serde_json::json!({
        "choices": [{
            "message": {"role": "assistant", "refusal": "I cannot help."},
            "finish_reason": "stop"
        }]
    });
    let result = adapter.parse_response(body).unwrap();
    assert_eq!(result.finish_reason, Some(FinishReason::ContentFilter));
    assert!(result.content.iter().any(|c| match c {
        Content::Text { text } => text == "I cannot help.",
        _ => false,
    }));

    // `reasoning` alias
    let body = serde_json::json!({
        "choices": [{
            "message": {"role": "assistant", "reasoning": "step by step", "content": "ok"},
            "finish_reason": "stop"
        }]
    });
    let result = adapter.parse_response(body).unwrap();
    assert!(
        matches!(result.content.first(), Some(Content::Reasoning { text }) if text == "step by step")
    );

    // `thinking` alias (Aliyun-style)
    let body = serde_json::json!({
        "choices": [{
            "message": {"role": "assistant", "thinking": "internal monologue", "content": "out"},
            "finish_reason": "stop"
        }]
    });
    let result = adapter.parse_response(body).unwrap();
    assert!(
        matches!(result.content.first(), Some(Content::Reasoning { text }) if text == "internal monologue")
    );
}

#[test]
fn messages_stream_encoder_closes_block_on_kind_transition() {
    // v0 #429 regression: when the canonical part stream transitions
    // text → reasoning → text → tool, the Anthropic encoder MUST emit a
    // `content_block_stop` before opening the new block kind. Strict
    // clients (Claude Code) reject a text_delta inside an open `thinking`
    // block. Ref: docs.anthropic.com/en/api/messages-streaming.
    let adapter = adapter_for(ApiProtocol::Messages);
    let mut encoder = adapter.stream_encoder("msg_x", "claude-3-7-sonnet");
    let parts = [
        StreamPart::ReasoningDelta {
            text: "think ".into(),
        },
        StreamPart::TextDelta {
            text: "answer ".into(),
        },
        StreamPart::ToolCallDelta {
            id: "t1".into(),
            name: Some("calc".into()),
            arguments: "{}".into(),
        },
    ];
    let mut events: Vec<String> = Vec::new();
    for part in &parts {
        for frame in encoder.encode(part).unwrap() {
            if let SseFrame::Event { event, data } = frame {
                events.push(format!("{} {data}", event.unwrap_or_default()));
            }
        }
    }
    // Find the sequence: thinking block → close → text block → close → tool block.
    let joined = events.join("\n");
    let thinking_open = joined
        .find("\"content_block\":{\"type\":\"thinking\"")
        .or_else(|| joined.find("\"type\":\"thinking\""));
    let first_stop = joined.find("content_block_stop");
    let text_open = joined.find("\"type\":\"text\"");
    assert!(thinking_open.is_some(), "thinking block opened: {joined}");
    assert!(first_stop.is_some(), "block stop emitted: {joined}");
    assert!(text_open.is_some(), "text block opened: {joined}");
    assert!(
        thinking_open < first_stop && first_stop < text_open,
        "block_stop must fall *between* thinking_start and text_start; got:\n{joined}"
    );
}

#[test]
fn messages_stream_error_maps_to_proper_http_status() {
    // Messages mid-stream `error` events carry `error.type` — a 4xx must
    // be threaded to `Upstream.status` so the fallback policy can decide
    // "don't retry" instead of always treating these as 5xx. Ref:
    // docs.anthropic.com/en/api/errors.
    let adapter = adapter_for(ApiProtocol::Messages);
    let mut decoder = adapter.stream_decoder();
    let err = decoder
        .decode(&SseEvent {
            event: Some("error".to_string()),
            data: serde_json::json!({
                "type": "error",
                "error": { "type": "rate_limit_error", "message": "too many" }
            })
            .to_string(),
        })
        .unwrap_err();
    match err {
        crate::error::BitrouterError::Upstream { status, .. } => {
            assert_eq!(status, 429, "rate_limit_error → 429");
        }
        other => panic!("expected Upstream, got {other:?}"),
    }
}

#[test]
fn responses_stream_error_maps_to_proper_http_status() {
    // Responses `response.failed` likewise — `error.type` decides
    // `Upstream.status` so the fallback policy can tell "client did
    // something wrong" (4xx, don't retry) from "upstream broke" (5xx).
    let adapter = adapter_for(ApiProtocol::Responses);
    let mut decoder = adapter.stream_decoder();
    let err = decoder
        .decode(&SseEvent {
            event: Some("response.failed".to_string()),
            data: serde_json::json!({
                "type": "response.failed",
                "response": { "error": { "type": "invalid_request_error", "message": "nope" } }
            })
            .to_string(),
        })
        .unwrap_err();
    match err {
        crate::error::BitrouterError::Upstream { status, .. } => {
            assert_eq!(status, 400, "invalid_request_error → 400");
        }
        other => panic!("expected Upstream, got {other:?}"),
    }
}

#[test]
fn streaming_decoders_emit_response_started_once() {
    // The 3 streaming protocols whose canonical IR previously dropped the
    // upstream response id now surface it as a one-shot `ResponseStarted`,
    // so observability can stamp `gen_ai.response.id` on the trace. OpenAI
    // Responses is unaffected (it carries the id on `ResponseCompleted`).

    // Chat Completions: top-level `id` repeats on every chunk → emit once.
    let adapter = adapter_for(ApiProtocol::ChatCompletions);
    let mut dec = adapter.stream_decoder();
    let first = dec
        .decode(&SseEvent {
            event: None,
            data: serde_json::json!({
                "id": "chatcmpl-stream1",
                "object": "chat.completion.chunk",
                "choices": [{"index": 0, "delta": {"role": "assistant", "content": "hi"}, "finish_reason": null}],
            })
            .to_string(),
        })
        .unwrap();
    assert!(
        first
            .iter()
            .any(|p| matches!(p, StreamPart::ResponseStarted { id } if id == "chatcmpl-stream1")),
        "Chat Completions first chunk emits ResponseStarted; got {first:?}"
    );
    let second = dec
        .decode(&SseEvent {
            event: None,
            data: serde_json::json!({
                "id": "chatcmpl-stream1",
                "choices": [{"index": 0, "delta": {"content": " there"}, "finish_reason": null}],
            })
            .to_string(),
        })
        .unwrap();
    assert!(
        !second
            .iter()
            .any(|p| matches!(p, StreamPart::ResponseStarted { .. })),
        "ResponseStarted is emitted only once per stream; got {second:?}"
    );

    // Messages: `message_start` carries `message.id` (fires once).
    let adapter = adapter_for(ApiProtocol::Messages);
    let mut dec = adapter.stream_decoder();
    let parts = dec
        .decode(&SseEvent {
            event: Some("message_start".to_string()),
            data: serde_json::json!({
                "type": "message_start",
                "message": {"id": "msg_stream1", "usage": {"input_tokens": 3, "output_tokens": 0}}
            })
            .to_string(),
        })
        .unwrap();
    assert!(
        parts
            .iter()
            .any(|p| matches!(p, StreamPart::ResponseStarted { id } if id == "msg_stream1")),
        "Anthropic message_start emits ResponseStarted; got {parts:?}"
    );

    // Generate Content: top-level `responseId` repeats on every chunk → emit once.
    let adapter = adapter_for(ApiProtocol::GenerateContent);
    let mut dec = adapter.stream_decoder();
    let parts = dec
        .decode(&SseEvent {
            event: None,
            data: serde_json::json!({
                "responseId": "google-stream1",
                "candidates": [{"content": {"role": "model", "parts": [{"text": "hi"}]}}]
            })
            .to_string(),
        })
        .unwrap();
    assert!(
        parts
            .iter()
            .any(|p| matches!(p, StreamPart::ResponseStarted { id } if id == "google-stream1")),
        "Google first chunk emits ResponseStarted; got {parts:?}"
    );
}

#[test]
fn chat_encoder_role_survives_leading_response_started() {
    // Regression: a leading `ResponseStarted` (now emitted first by the
    // Chat Completions / Generate Content decoders) must NOT consume the one-shot
    // `role: assistant` marker. The role has to ride the first real
    // content chunk; otherwise a Chat Completions client never sees it.
    let adapter = adapter_for(ApiProtocol::ChatCompletions);
    let mut enc = adapter.stream_encoder("chatcmpl-x", "gpt-5");

    // ResponseStarted arrives first — must emit no frames.
    let started = enc
        .encode(&StreamPart::ResponseStarted {
            id: "chatcmpl-upstream".to_string(),
        })
        .unwrap();
    assert!(
        started.is_empty(),
        "ResponseStarted must not emit a client frame; got {started:?}"
    );

    // The first content chunk must still carry `role: assistant`.
    let frames = enc
        .encode(&StreamPart::TextDelta {
            text: "hi".to_string(),
        })
        .unwrap();
    let SseFrame::Event { data, .. } = frames.first().expect("a content frame") else {
        panic!("expected an SSE event frame");
    };
    let chunk: serde_json::Value = serde_json::from_str(data).unwrap();
    assert_eq!(
        chunk["choices"][0]["delta"]["role"], "assistant",
        "role must ride the first content chunk even after a leading ResponseStarted; got {chunk}"
    );
    assert_eq!(chunk["choices"][0]["delta"]["content"], "hi");
}

#[test]
fn responses_omits_usage_when_none() {
    // v0 #6ae55b2 — when upstream reported no token counts, the wire shape
    // omits the `usage` key entirely. Mirrors the streaming `emit_terminal`.
    let adapter = adapter_for(ApiProtocol::Responses);
    let result = GenerateResult {
        content: vec![Content::Text {
            text: "ok".to_string(),
        }],
        usage: None,
        finish_reason: Some(FinishReason::Stop),
        response_id: None,
        stop_details: None,
    };
    let rendered = adapter
        .render_response(&result, &sample_prompt(), "resp_n")
        .unwrap();
    assert!(
        rendered.get("usage").is_none(),
        "usage must be absent when upstream had no counts, got: {rendered}"
    );
}

#[test]
fn chat_completions_streaming_forces_include_usage() {
    // Chat Completions omits the trailing usage chunk unless the caller asks for it.
    // Settlement requires that chunk, so the outbound request injects
    // `stream_options.include_usage = true` whenever stream=true.
    let adapter = adapter_for(ApiProtocol::ChatCompletions);
    let body = serde_json::json!({
        "model": "gpt-4",
        "messages": [{"role": "user", "content": "hi"}],
        "stream": true
    });
    let prompt = adapter.parse_request(body).unwrap();
    let rendered = adapter.render_request(&prompt).unwrap();
    assert_eq!(rendered["stream_options"]["include_usage"], true);

    // Non-streaming requests don't get the field — there's no streaming
    // chunk to receive, and providers reject the field on non-streaming
    // calls.
    let body = serde_json::json!({
        "model": "gpt-4",
        "messages": [{"role": "user", "content": "hi"}]
    });
    let prompt = adapter.parse_request(body).unwrap();
    let rendered = adapter.render_request(&prompt).unwrap();
    assert!(rendered.get("stream_options").is_none());

    // Caller-supplied stream_options keys survive the merge.
    let body = serde_json::json!({
        "model": "gpt-4",
        "messages": [{"role": "user", "content": "hi"}],
        "stream": true,
        "stream_options": {"include_obfuscation": false}
    });
    let prompt = adapter.parse_request(body).unwrap();
    let rendered = adapter.render_request(&prompt).unwrap();
    assert_eq!(rendered["stream_options"]["include_usage"], true);
    assert_eq!(rendered["stream_options"]["include_obfuscation"], false);
}

#[test]
fn generate_content_passes_through_top_level_extras() {
    // toolConfig / safetySettings / cachedContent live at the request root,
    // not under generationConfig. They must survive the round-trip.
    // Refs: <https://ai.google.dev/api/generate-content>.
    let adapter = adapter_for(ApiProtocol::GenerateContent);
    let body = serde_json::json!({
        "model": "gemini-2.0-flash",
        "contents": [{"role": "user", "parts": [{"text": "hi"}]}],
        "toolConfig": {
            "functionCallingConfig": {"mode": "ANY", "allowedFunctionNames": ["lookup"]}
        },
        "safetySettings": [
            {"category": "HARM_CATEGORY_HARASSMENT", "threshold": "BLOCK_NONE"}
        ],
        "cachedContent": "cachedContents/abc123"
    });
    let prompt = adapter.parse_request(body.clone()).unwrap();
    let rendered = adapter.render_request(&prompt).unwrap();
    for key in ["toolConfig", "safetySettings", "cachedContent"] {
        assert_eq!(
            rendered[key], body[key],
            "Google top-level `{key}` must survive parse/render"
        );
    }
}

#[test]
fn generate_content_request_stream_flag_is_propagated() {
    // The server injects `stream: true` from a `:streamGenerateContent` path
    // verb. Before #stream-flag-fix the adapter dropped this field on the
    // floor and forced stream=false, sending streaming clients to the
    // non-streaming branch.
    let adapter = adapter_for(ApiProtocol::GenerateContent);
    let body = serde_json::json!({
        "model": "gemini-2.0-flash",
        "contents": [{"role": "user", "parts": [{"text": "hi"}]}],
        "stream": true
    });
    let prompt = adapter.parse_request(body).unwrap();
    assert!(
        prompt.stream,
        "streamGenerateContent must set prompt.stream"
    );

    // And the wire shape does NOT leak `stream` back upstream (Google's
    // generate-content body has no `stream` field of its own).
    let rendered = adapter.render_request(&prompt).unwrap();
    assert!(
        rendered.get("stream").is_none(),
        "Google outbound must not include a stream field"
    );
}

#[test]
fn responses_passes_through_uncommon_params() {
    let adapter = adapter_for(ApiProtocol::Responses);
    let body = serde_json::json!({
        "model": "gpt-5",
        "input": [{"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi"}]}],
        "tool_choice": "auto",
        "parallel_tool_calls": false,
        "max_tool_calls": 3,
        "include": ["reasoning.encrypted_content"],
        "metadata": {"trace_id": "t1"},
        "previous_response_id": "rsp_prev",
        "store": false,
        "stream_options": {"include_obfuscation": false}
    });
    let prompt = adapter.parse_request(body.clone()).unwrap();
    let rendered = adapter.render_request(&prompt).unwrap();
    for key in [
        "tool_choice",
        "parallel_tool_calls",
        "max_tool_calls",
        "include",
        "metadata",
        "previous_response_id",
        "store",
        "stream_options",
    ] {
        assert_eq!(
            rendered[key], body[key],
            "Responses `{key}` must survive parse/render"
        );
    }
}

#[test]
fn messages_response_roundtrip() {
    let adapter = adapter_for(ApiProtocol::Messages);
    let prompt = sample_prompt();
    let json = adapter
        .render_response(&sample_result(), &prompt, "msg_1")
        .unwrap();
    assert_eq!(json["type"], "message");
    assert_eq!(json["role"], "assistant");
    // content order: reasoning(thinking), text, tool_use
    assert_eq!(json["content"][0]["type"], "thinking");
    assert_eq!(json["content"][1]["type"], "text");
    assert_eq!(json["content"][2]["type"], "tool_use");
    let parsed = adapter.parse_response(json).unwrap();
    assert_eq!(parsed.content.len(), 3);
}

#[test]
fn generate_content_request_roundtrip() {
    let adapter = adapter_for(ApiProtocol::GenerateContent);
    let prompt = sample_prompt();
    let json = adapter.render_request(&prompt).unwrap();
    assert_eq!(json["systemInstruction"]["parts"][0]["text"], "be brief");
    assert_eq!(json["contents"][0]["role"], "user");
    let parsed = adapter.parse_request(json).unwrap();
    assert_eq!(parsed.system.as_deref(), Some("be brief"));
}

#[test]
fn responses_request_roundtrip() {
    let adapter = adapter_for(ApiProtocol::Responses);
    let prompt = sample_prompt();
    let json = adapter.render_request(&prompt).unwrap();
    assert_eq!(json["model"], "test-model");
    assert_eq!(json["instructions"], "be brief");
    let parsed = adapter.parse_request(json).unwrap();
    assert_eq!(parsed.messages.len(), 1);
}

// ===== v0 bug regression suite =====

/// #276 — ANSI escape codes in the model name. After sanitising, an escape
/// sequence is stripped so the router sees a clean (here: unknown) model name
/// rather than producing a 500.
#[test]
fn regression_276_ansi_escape_in_model_name() {
    let dirty = "gpt-5\u{1b}[1m";
    let clean = sanitize_model_name(dirty);
    assert_eq!(clean, "gpt-5", "ANSI escape stripped from model name");
    assert!(!clean.contains('\u{1b}'));
    // a tab / newline injected mid-name is also stripped
    assert_eq!(sanitize_model_name("  gpt-4o\t\n "), "gpt-4o");
}

/// #367 → #391 — deserialisation errors must be diagnosable: they carry the
/// target type name, the serde line/column, and a body preview.
#[test]
fn regression_367_deser_errors_are_descriptive() {
    let adapter = adapter_for(ApiProtocol::ChatCompletions);
    // `messages` should be an array; a string is a type error.
    let bad = serde_json::json!({ "model": "m", "messages": "oops" });
    let err = adapter.parse_request(bad).unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("ChatRequest"), "names the target type: {msg}");
    assert!(msg.contains("line"), "carries serde location: {msg}");
    assert!(msg.contains("preview"), "carries a body preview: {msg}");
    assert_eq!(err.status(), 400);
}

/// Preview truncation must respect UTF-8 char boundaries — slicing at byte
/// 240 of a body whose 240th byte sits inside a multi-byte sequence used to
/// panic on the request path.
#[test]
fn deser_error_preview_handles_multi_byte_utf8() {
    let adapter = adapter_for(ApiProtocol::ChatCompletions);
    // ~120 "é" (2 bytes) gives a body well over 240 bytes whose boundary
    // would fall inside a multi-byte rune if naïvely byte-sliced.
    let pad: String = "é".repeat(200);
    let bad = serde_json::json!({ "model": "m", "messages": pad });
    let err = adapter.parse_request(bad).unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("ChatRequest"), "still diagnosable: {msg}");
    assert!(msg.contains("preview"), "carries a body preview: {msg}");
}

/// #416 — a mixed text + tool_use Anthropic message must not be rejected; the
/// blocks keep their order in the canonical representation.
#[test]
fn regression_416_mixed_text_and_tool_call_preserved() {
    let adapter = adapter_for(ApiProtocol::Messages);
    let body = serde_json::json!({
        "model": "claude",
        "messages": [{
            "role": "assistant",
            "content": [
                { "type": "text", "text": "let me compute" },
                { "type": "tool_use", "id": "t1", "name": "calc", "input": { "x": 1 } },
            ],
        }],
    });
    let prompt = adapter.parse_request(body).expect("must not 502/reject");
    let content = &prompt.messages[0].content;
    assert_eq!(content.len(), 2);
    assert!(matches!(content[0], Content::Text { .. }), "text first");
    assert!(
        matches!(content[1], Content::ToolCall { .. }),
        "tool call second — order preserved"
    );
}

/// #227 → #228 — Anthropic `system` accepts both a string and a content-block
/// array.
#[test]
fn regression_227_messages_system_accepts_string_or_array() {
    let adapter = adapter_for(ApiProtocol::Messages);

    let as_string = serde_json::json!({
        "model": "claude",
        "system": "you are helpful",
        "messages": [{ "role": "user", "content": "hi" }],
    });
    let p1 = adapter.parse_request(as_string).unwrap();
    assert_eq!(p1.system.as_deref(), Some("you are helpful"));

    let as_array = serde_json::json!({
        "model": "claude",
        "system": [
            { "type": "text", "text": "line one" },
            { "type": "text", "text": "line two" },
        ],
        "messages": [{ "role": "user", "content": "hi" }],
    });
    let p2 = adapter.parse_request(as_array).unwrap();
    assert_eq!(p2.system.as_deref(), Some("line one\nline two"));
}

/// #364 — `tool_result.content` accepts a string or an array; `thinking`
/// blocks round-trip.
#[test]
fn regression_364_tool_result_array_and_thinking() {
    let adapter = adapter_for(ApiProtocol::Messages);
    let body = serde_json::json!({
        "model": "claude",
        "messages": [
            {
                "role": "assistant",
                "content": [{ "type": "thinking", "thinking": "pondering" }],
            },
            {
                "role": "user",
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": "t1",
                    "content": [{ "type": "text", "text": "42" }],
                }],
            },
        ],
    });
    let prompt = adapter.parse_request(body).unwrap();
    // thinking block becomes canonical Reasoning
    assert!(
        prompt
            .messages
            .iter()
            .flat_map(|m| &m.content)
            .any(|c| matches!(c, Content::Reasoning { .. })),
        "thinking block preserved as Reasoning"
    );
    // a text-only tool_result block array collapses to a Text output
    let tr = prompt
        .messages
        .iter()
        .flat_map(|m| &m.content)
        .find_map(|c| match c {
            Content::ToolResult { output, .. } => Some(output.clone()),
            _ => None,
        });
    assert_eq!(
        tr,
        Some(ToolResultOutput::Text {
            value: "42".to_string()
        }),
        "text-only array tool_result content flattens to a Text output"
    );
}

/// #454-1 — reasoning content is not dropped across any of the four protocols.
#[test]
fn regression_454_1_reasoning_survives_all_protocols() {
    for proto in all_protocols() {
        let adapter = adapter_for(proto.clone());
        let result = sample_result(); // has a Reasoning block
        let rendered = adapter
            .render_response(&result, &sample_prompt(), "r1")
            .unwrap();
        let parsed = adapter.parse_response(rendered).unwrap();
        assert!(
            parsed
                .content
                .iter()
                .any(|c| matches!(c, Content::Reasoning { .. })),
            "{proto:?}: reasoning content survived render→parse"
        );
    }
}

/// #454-4 — role mapping is total: an unknown role is an error, never a silent
/// downgrade to `user`.
#[test]
fn regression_454_4_unknown_role_is_an_error() {
    let adapter = adapter_for(ApiProtocol::ChatCompletions);
    let body = serde_json::json!({
        "model": "m",
        "messages": [{ "role": "wizard", "content": "abracadabra" }],
    });
    let err = adapter.parse_request(body).unwrap_err();
    assert_eq!(err.status(), 400);
    assert!(err.to_string().contains("wizard"), "names the bad role");
}

/// #454-5 — wire types omit absent values entirely; they never serialise a
/// JSON `null`. A result with no usage must not carry a `null` usage key.
#[test]
fn regression_454_5_no_null_on_the_wire() {
    let result = GenerateResult {
        content: vec![Content::Text {
            text: "hi".to_string(),
        }],
        usage: None,
        finish_reason: None,
        response_id: None,
        stop_details: None,
    };
    // Chat Completions: `usage` key is absent when there is no usage.
    let chat = adapter_for(ApiProtocol::ChatCompletions)
        .render_response(&result, &sample_prompt(), "c1")
        .unwrap();
    assert!(
        chat.get("usage").is_none(),
        "absent usage omits the key, not null: {chat}"
    );
    // `content` is always a string (here "hi"), never null
    assert_eq!(chat["choices"][0]["message"]["content"], "hi");
    assert!(!chat["choices"][0]["message"]["content"].is_null());

    // an empty assistant reply still renders content as "" — never null
    let empty = GenerateResult {
        content: vec![],
        usage: None,
        finish_reason: None,
        response_id: None,
        stop_details: None,
    };
    let chat_empty = adapter_for(ApiProtocol::ChatCompletions)
        .render_response(&empty, &sample_prompt(), "c2")
        .unwrap();
    assert_eq!(chat_empty["choices"][0]["message"]["content"], "");
    assert!(!chat_empty["choices"][0]["message"]["content"].is_null());
}

/// #454-3 — the Responses `input` accepts a plain string or a heterogeneous
/// item array (Codex multi-turn) without a hard 400.
#[test]
fn regression_454_3_responses_input_accepts_string_and_item_array() {
    let adapter = adapter_for(ApiProtocol::Responses);

    let as_string = serde_json::json!({ "model": "gpt-5", "input": "hello there" });
    let p1 = adapter.parse_request(as_string).unwrap();
    assert_eq!(p1.messages.len(), 1);
    assert_eq!(text_of(&p1.messages[0].content), "hello there");

    // a mixed item array: a message, a function_call, a function_call_output,
    // and an unknown item type — none of which may cause a 400.
    let as_items = serde_json::json!({
        "model": "gpt-5",
        "input": [
            { "type": "message", "role": "user", "content": "do a thing" },
            { "type": "function_call", "call_id": "c1", "name": "do", "arguments": "{}" },
            { "type": "function_call_output", "call_id": "c1", "output": "done" },
            { "type": "some_future_item", "data": "ignored" },
        ],
    });
    let p2 = adapter
        .parse_request(as_items)
        .expect("Codex-style mixed input must not 400");
    assert!(
        p2.messages.len() >= 3,
        "known items parsed, unknown skipped"
    );
}

/// #432 — `response.incomplete` and unknown forward-compatible events are not
/// mis-flagged as errors by the Responses stream decoder.
#[test]
fn regression_432_responses_incomplete_and_unknown_events_not_errors() {
    let adapter = adapter_for(ApiProtocol::Responses);
    let mut decoder = adapter.stream_decoder();

    // an unknown event type is ignored, not an error
    let unknown = SseEvent {
        event: Some("response.some_new_event".to_string()),
        data: serde_json::json!({ "type": "response.some_new_event" }).to_string(),
    };
    assert!(
        decoder.decode(&unknown).is_ok(),
        "unknown event is not an error"
    );

    // `response.incomplete` is a clean terminal event — mapped to a
    // `ResponseCompleted` part with status "incomplete", never an
    // error.
    let incomplete = SseEvent {
        event: Some("response.incomplete".to_string()),
        data: serde_json::json!({
            "type": "response.incomplete",
            "response": { "id": "resp_inc", "status": "incomplete" }
        })
        .to_string(),
    };
    let parts = decoder
        .decode(&incomplete)
        .expect("incomplete is not an error");
    assert!(
        parts.iter().any(|p| matches!(
            p,
            StreamPart::ResponseCompleted { status, .. } if status == "incomplete"
        )),
        "response.incomplete → ResponseCompleted{{ status: incomplete }}"
    );
}

/// #454-2 — the Responses streaming envelope is complete: every event carries a
/// `sequence_number`, `response.completed` carries the full `response` object,
/// and there is no `[DONE]` sentinel.
#[test]
fn regression_454_2_responses_stream_envelope() {
    let adapter = adapter_for(ApiProtocol::Responses);
    let mut encoder = adapter.stream_encoder("resp_x", "gpt-5");
    let mut frames = Vec::new();
    frames.extend(
        encoder
            .encode(&StreamPart::TextDelta {
                text: "hi".to_string(),
            })
            .unwrap(),
    );
    frames.extend(
        encoder
            .encode(&StreamPart::Finish {
                reason: FinishReason::Stop,
            })
            .unwrap(),
    );
    frames.extend(encoder.finish().unwrap());

    // every event frame carries a sequence_number
    for frame in &frames {
        if let SseFrame::Event { data, .. } = frame {
            let json: serde_json::Value = serde_json::from_str(data).unwrap();
            assert!(
                json.get("sequence_number").is_some(),
                "every Responses event has sequence_number: {data}"
            );
        }
    }
    // a response.completed event exists and carries the response object
    let completed = frames.iter().find_map(|f| match f {
        SseFrame::Event { event, data } if event.as_deref() == Some("response.completed") => {
            Some(data.clone())
        }
        _ => None,
    });
    let completed = completed.expect("response.completed event present");
    let json: serde_json::Value = serde_json::from_str(&completed).unwrap();
    assert!(
        json["response"].is_object(),
        "completed carries response obj"
    );

    // no `[DONE]` sentinel anywhere
    assert!(
        !frames.iter().any(|f| matches!(
            f,
            SseFrame::Event { data, .. } if data.trim() == "[DONE]"
        )),
        "Responses must not emit [DONE]"
    );
}

/// Codex-hang regression: the Responses stream must emit
/// `response.in_progress` after `response.created`, and the terminal
/// `response.completed` must carry the full `output` array (every
/// closed reasoning / message / tool item). Codex CLI reconstructs the
/// assistant turn from `response.completed.response.output` — an empty
/// array renders as a blank turn even though the deltas streamed fine.
#[test]
fn responses_stream_completed_carries_output_array() {
    let adapter = adapter_for(ApiProtocol::Responses);
    let mut encoder = adapter.stream_encoder("resp_h", "gpt-5");
    let mut frames = Vec::new();
    frames.extend(
        encoder
            .encode(&StreamPart::ReasoningDelta {
                text: "think".to_string(),
            })
            .unwrap(),
    );
    frames.extend(
        encoder
            .encode(&StreamPart::TextDelta {
                text: "answer".to_string(),
            })
            .unwrap(),
    );
    frames.extend(
        encoder
            .encode(&StreamPart::Finish {
                reason: FinishReason::Stop,
            })
            .unwrap(),
    );

    let event_names: Vec<&str> = frames
        .iter()
        .filter_map(|f| match f {
            SseFrame::Event { event, .. } => event.as_deref(),
            _ => None,
        })
        .collect();
    // created → in_progress preamble.
    assert_eq!(event_names.first(), Some(&"response.created"));
    assert_eq!(event_names.get(1), Some(&"response.in_progress"));
    // reasoning + message items each fully bracketed.
    assert!(event_names.contains(&"response.output_item.added"));
    assert!(event_names.contains(&"response.content_part.added"));
    assert!(event_names.contains(&"response.reasoning_text.delta"));
    assert!(event_names.contains(&"response.output_text.delta"));
    assert!(event_names.contains(&"response.output_item.done"));

    // response.completed carries both items in `output`.
    let completed = frames
        .iter()
        .find_map(|f| match f {
            SseFrame::Event { event, data } if event.as_deref() == Some("response.completed") => {
                Some(serde_json::from_str::<serde_json::Value>(data).unwrap())
            }
            _ => None,
        })
        .expect("response.completed present");
    let output = completed["response"]["output"]
        .as_array()
        .expect("output is an array");
    assert_eq!(output.len(), 2, "reasoning + message items: {output:?}");
    assert_eq!(output[0]["type"], "reasoning");
    assert_eq!(output[1]["type"], "message");
    assert_eq!(output[1]["content"][0]["text"], "answer");
}

/// Codex tool-use regression: a `ToolCallDelta` stream produces a fully
/// bracketed `function_call` item — `output_item.added` →
/// `function_call_arguments.delta` → `function_call_arguments.done` →
/// `output_item.done` — and the item lands in `response.completed`'s
/// `output` array with its accumulated arguments.
#[test]
fn responses_stream_tool_call_lifecycle() {
    let adapter = adapter_for(ApiProtocol::Responses);
    let mut encoder = adapter.stream_encoder("resp_t", "gpt-5");
    let mut frames = Vec::new();
    frames.extend(
        encoder
            .encode(&StreamPart::ToolCallDelta {
                id: "call_1".to_string(),
                name: Some("shell".to_string()),
                arguments: "{\"cmd\":".to_string(),
            })
            .unwrap(),
    );
    frames.extend(
        encoder
            .encode(&StreamPart::ToolCallDelta {
                id: "call_1".to_string(),
                name: None,
                arguments: "\"ls\"}".to_string(),
            })
            .unwrap(),
    );
    frames.extend(
        encoder
            .encode(&StreamPart::Finish {
                reason: FinishReason::ToolCalls,
            })
            .unwrap(),
    );

    let event_names: Vec<&str> = frames
        .iter()
        .filter_map(|f| match f {
            SseFrame::Event { event, .. } => event.as_deref(),
            _ => None,
        })
        .collect();
    assert!(event_names.contains(&"response.function_call_arguments.delta"));
    assert!(event_names.contains(&"response.function_call_arguments.done"));

    let completed = frames
        .iter()
        .find_map(|f| match f {
            SseFrame::Event { event, data } if event.as_deref() == Some("response.completed") => {
                Some(serde_json::from_str::<serde_json::Value>(data).unwrap())
            }
            _ => None,
        })
        .expect("response.completed present");
    let output = completed["response"]["output"]
        .as_array()
        .expect("output is an array");
    assert_eq!(output.len(), 1, "one function_call item: {output:?}");
    assert_eq!(output[0]["type"], "function_call");
    assert_eq!(output[0]["call_id"], "call_1");
    assert_eq!(output[0]["name"], "shell");
    assert_eq!(output[0]["arguments"], "{\"cmd\":\"ls\"}");
}

///.3 — `response.completed` decodes to the dedicated `ResponseCompleted`
/// part, preserving the response id + status + usage that a bare `Finish` would
/// have lost; and that part re-encodes to a `response.completed` event carrying
/// the same id.
#[test]
fn responses_completed_preserves_id_status_and_usage() {
    let adapter = adapter_for(ApiProtocol::Responses);

    // decode: response.completed → ResponseCompleted
    let mut decoder = adapter.stream_decoder();
    let event = SseEvent {
        event: Some("response.completed".to_string()),
        data: serde_json::json!({
            "type": "response.completed",
            "response": {
                "id": "resp_xyz",
                "status": "completed",
                "usage": { "input_tokens": 12, "output_tokens": 8 }
            }
        })
        .to_string(),
    };
    let parts = decoder.decode(&event).unwrap();
    match parts.first() {
        Some(StreamPart::ResponseCompleted { id, status, usage }) => {
            assert_eq!(id, "resp_xyz");
            assert_eq!(status, "completed");
            assert_eq!(usage.unwrap().prompt_tokens, 12);
            assert_eq!(usage.unwrap().completion_tokens, 8);
        }
        other => panic!("expected ResponseCompleted, got {other:?}"),
    }

    // re-encode: ResponseCompleted → a response.completed event with the id
    let mut encoder = adapter.stream_encoder("fallback_id", "gpt-5");
    let frames = encoder
        .encode(&StreamPart::ResponseCompleted {
            id: "resp_xyz".to_string(),
            status: "completed".to_string(),
            usage: Some(Usage {
                prompt_tokens: 12,
                completion_tokens: 8,
                reasoning_tokens: 0,
                ..Default::default()
            }),
        })
        .unwrap();
    let completed = frames.iter().find_map(|f| match f {
        SseFrame::Event { event, data } if event.as_deref() == Some("response.completed") => {
            Some(serde_json::from_str::<serde_json::Value>(data).unwrap())
        }
        _ => None,
    });
    let completed = completed.expect("response.completed event emitted");
    // the carried response id wins over the encoder's fallback request id
    assert_eq!(completed["response"]["id"], "resp_xyz");
    assert_eq!(completed["response"]["status"], "completed");
    assert_eq!(completed["response"]["usage"]["input_tokens"], 12);
}

/// Upstreams that omit the SSE `event:` line (OpenRouter, vanilla OpenAI
/// fronted via some gateways) cause the SSE parser to default the event
/// name to `"message"`. The Responses decoder must trust the body `type`
/// field in that case instead of treating `"message"` as the event name —
/// otherwise every delta/lifecycle frame is silently dropped as "unknown".
#[test]
fn responses_stream_decoder_prefers_body_type_over_default_message_event() {
    let adapter = adapter_for(ApiProtocol::Responses);
    let mut decoder = adapter.stream_decoder();

    let event = SseEvent {
        // SSE default event name when the upstream omits `event:`.
        event: Some("message".to_string()),
        data: serde_json::json!({
            "type": "response.output_text.delta",
            "delta": "pong",
        })
        .to_string(),
    };
    let parts = decoder
        .decode(&event)
        .expect("decoder must not error on default-named events");
    assert!(
        matches!(
            parts.first(),
            Some(StreamPart::TextDelta { text }) if text == "pong"
        ),
        "body `type` must win over the SSE default `message` event name; got {parts:?}"
    );
}

/// #434 — Responses function-call stream items map `item_id` back to the
/// canonical `call_id`, and the `.done` event does not duplicate the arguments.
#[test]
fn regression_434_responses_tool_stream_id_mapping() {
    let adapter = adapter_for(ApiProtocol::Responses);
    let mut decoder = adapter.stream_decoder();

    // output_item.added introduces the function call (item id ≠ call id)
    let added = SseEvent {
        event: Some("response.output_item.added".to_string()),
        data: serde_json::json!({
            "type": "response.output_item.added",
            "item": { "type": "function_call", "id": "item_42", "call_id": "call_abc", "name": "calc" },
        })
        .to_string(),
    };
    let p = decoder.decode(&added).unwrap();
    assert!(matches!(
        p.first(),
        Some(StreamPart::ToolCallDelta { id, .. }) if id == "call_abc"
    ));

    // arguments.delta references the item id; the decoder maps it to call_abc
    let delta = SseEvent {
        event: Some("response.function_call_arguments.delta".to_string()),
        data: serde_json::json!({
            "type": "response.function_call_arguments.delta",
            "item_id": "item_42",
            "delta": "{\"x\":1}",
        })
        .to_string(),
    };
    let p = decoder.decode(&delta).unwrap();
    match p.first() {
        Some(StreamPart::ToolCallDelta { id, arguments, .. }) => {
            assert_eq!(id, "call_abc", "item_id mapped back to call_id");
            assert_eq!(arguments, "{\"x\":1}");
        }
        other => panic!("expected ToolCallDelta, got {other:?}"),
    }

    // the `.done` event must NOT re-emit the arguments (would duplicate)
    let done = SseEvent {
        event: Some("response.function_call_arguments.done".to_string()),
        data: serde_json::json!({
            "type": "response.function_call_arguments.done",
            "item_id": "item_42",
            "arguments": "{\"x\":1}",
        })
        .to_string(),
    };
    let p = decoder.decode(&done).unwrap();
    assert!(p.is_empty(), ".done must not duplicate the arguments delta");
}

/// #422 — Anthropic inbound `ping` events are ignored, never treated as errors
/// or content. (The outbound keepalive itself is covered in `stream` tests.)
#[test]
fn regression_422_messages_ping_events_ignored() {
    let adapter = adapter_for(ApiProtocol::Messages);
    let mut decoder = adapter.stream_decoder();
    let ping = SseEvent {
        event: Some("ping".to_string()),
        data: serde_json::json!({ "type": "ping" }).to_string(),
    };
    let parts = decoder.decode(&ping).expect("ping is not an error");
    assert!(parts.is_empty(), "ping yields no canonical parts");
}

/// #429 — gpt-5.x style models routed through the Responses protocol round-trip
/// correctly; the Anthropic outbound stream frames are well-formed events.
#[test]
fn regression_429_responses_routing_and_messages_frames() {
    // a gpt-5 prompt rendered for the Responses protocol
    let responses = adapter_for(ApiProtocol::Responses);
    let mut prompt = sample_prompt();
    prompt.model = "gpt-5.1".to_string();
    let req = responses.render_request(&prompt).unwrap();
    assert_eq!(req["model"], "gpt-5.1");
    assert!(req["input"].is_array(), "Responses uses an `input` array");

    // Messages outbound stream frames are named SSE events
    let anthropic = adapter_for(ApiProtocol::Messages);
    let mut enc = anthropic.stream_encoder("m1", "claude");
    let frames = enc
        .encode(&StreamPart::TextDelta {
            text: "x".to_string(),
        })
        .unwrap();
    assert!(
        frames
            .iter()
            .all(|f| matches!(f, SseFrame::Event { event: Some(_), .. })),
        "Anthropic frames are named events"
    );
}

/// #454 family — usage with explicit zero is `0` on the wire, not `null`; a
/// completed-with-usage stream carries a numeric usage chunk.
#[test]
fn regression_454_5_usage_zero_is_numeric_not_null() {
    let result = GenerateResult {
        content: vec![Content::Text {
            text: "hi".to_string(),
        }],
        usage: Some(Usage {
            prompt_tokens: 0,
            completion_tokens: 0,
            reasoning_tokens: 0,
            ..Default::default()
        }),
        finish_reason: Some(FinishReason::Stop),
        response_id: None,
        stop_details: None,
    };
    let chat = adapter_for(ApiProtocol::ChatCompletions)
        .render_response(&result, &sample_prompt(), "c1")
        .unwrap();
    assert_eq!(chat["usage"]["prompt_tokens"], 0);
    assert!(chat["usage"]["prompt_tokens"].is_number());
    assert!(!chat["usage"]["total_tokens"].is_null());
}

// ===== JsonSchema snapshot tests =====
//
// These tests guard the OpenAPI contract derived from the wire-shape types.
// `bitrouter-cloud` consumes these schemas (via `aide`) to publish the API
// reference, so any unintended drift in the documented shape is a contract
// change. The expected schema is stored under `snapshots/`; to update after a
// deliberate change, re-run the test with `BITROUTER_BLESS=1` set and commit
// the regenerated file.

/// Generate the JsonSchema for `T`, pretty-print it, and compare against the
/// fixture at `snapshots/<name>.json`. `BITROUTER_BLESS=1` rewrites the fixture.
fn assert_schema_snapshot<T: schemars::JsonSchema>(name: &str) {
    let schema = schemars::schema_for!(T);
    let actual = serde_json::to_string_pretty(&schema).unwrap();
    let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("src/language_model/protocol/snapshots")
        .join(format!("{name}.json"));

    if std::env::var_os("BITROUTER_BLESS").is_some() {
        std::fs::write(&path, format!("{actual}\n")).unwrap();
        return;
    }

    let expected = std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "snapshot {} not readable ({e}); re-run with BITROUTER_BLESS=1 to create.\n\
             actual schema:\n{actual}",
            path.display()
        )
    });
    // Normalise CRLF → LF before comparing. `.gitattributes` pins the
    // snapshot files to LF, but a contributor without an autocrlf-aware
    // setup (or a checkout made before that pin landed) can still end up
    // with CRLF on disk on Windows. The freshly-generated `actual` always
    // uses LF, so without this normalise step the test fails for a reason
    // that has nothing to do with the schema.
    let expected = expected.replace("\r\n", "\n");
    assert_eq!(
        expected.trim(),
        actual.trim(),
        "schema snapshot for `{name}` drifted; re-run with BITROUTER_BLESS=1 to update"
    );
}

#[test]
fn messages_request_schema_is_stable() {
    assert_schema_snapshot::<messages::MessagesRequest>("messages_request");
}

#[test]
fn chat_completions_request_schema_is_stable() {
    assert_schema_snapshot::<chat_completions::ChatRequest>("chat_completions_request");
}

#[test]
fn generate_content_request_schema_is_stable() {
    assert_schema_snapshot::<generate_content::GenerateContentRequest>("generate_content_request");
}

#[test]
fn responses_request_schema_is_stable() {
    assert_schema_snapshot::<responses::ResponsesRequest>("responses_request");
}

/// `#[schemars(skip)]` on the `extra` `HashMap` must hide it from the published
/// contract — the schema for the request should never expose
/// `additionalProperties` of arbitrary JSON values. The exact wording belongs
/// to the snapshots above; this asserts the negative behavior outright so a
/// regression is obvious from the failure message.
#[test]
fn extra_passthrough_field_is_not_in_schema() {
    let s = serde_json::to_value(schemars::schema_for!(messages::MessagesRequest)).unwrap();
    assert!(
        s.get("properties").and_then(|p| p.get("extra")).is_none(),
        "MessagesRequest schema must not expose `extra` (pass-through field)",
    );
    let s = serde_json::to_value(schemars::schema_for!(chat_completions::ChatRequest)).unwrap();
    assert!(
        s.get("properties").and_then(|p| p.get("extra")).is_none(),
        "Chat CompletionsRequest schema must not expose `extra` (pass-through field)",
    );
    // Generate Content has two `extra` fields — top-level and on `generationConfig`.
    // Walk both points to make sure neither leaks.
    let s = serde_json::to_value(schemars::schema_for!(
        generate_content::GenerateContentRequest
    ))
    .unwrap();
    assert!(
        s.get("properties").and_then(|p| p.get("extra")).is_none(),
        "Google GenerateContentRequest schema must not expose top-level `extra`",
    );
    let gen_cfg = s
        .get("$defs")
        .and_then(|d| d.get("GenerateContentGenerationConfig"))
        .expect("schema must include GenerateContentGenerationConfig in $defs");
    assert!(
        gen_cfg
            .get("properties")
            .and_then(|p| p.get("extra"))
            .is_none(),
        "Google GenerateContentGenerationConfig schema must not expose `extra`",
    );
    let s = serde_json::to_value(schemars::schema_for!(responses::ResponsesRequest)).unwrap();
    assert!(
        s.get("properties").and_then(|p| p.get("extra")).is_none(),
        "ResponsesRequest schema must not expose `extra` (pass-through field)",
    );
}

// ===== multimodal (file) content =====

const IMG_B64: &str = "iVBORw0KGgoAAAANSUhEUg==";

/// A canonical prompt whose single user message is one image file part.
fn image_file_prompt() -> Prompt {
    Prompt {
        model: "test-model".to_string(),
        system: None,
        messages: vec![Message {
            role: Role::User,
            content: vec![Content::File {
                media_type: "image/png".to_string(),
                data: DataContent::Base64 {
                    data: IMG_B64.to_string(),
                },
                filename: None,
                extra: Default::default(),
            }],
        }],
        tools: vec![],
        params: GenerationParams {
            max_tokens: Some(256),
            ..Default::default()
        },
        response_format: None,
        stream: false,
    }
}

/// The first file part `(media_type, data)` in a parsed prompt.
fn first_file(prompt: &Prompt) -> (&str, &DataContent) {
    prompt
        .messages
        .iter()
        .flat_map(|m| &m.content)
        .find_map(|c| match c {
            Content::File {
                media_type, data, ..
            } => Some((media_type.as_str(), data)),
            _ => None,
        })
        .expect("prompt should carry a file part")
}

#[test]
fn image_parses_to_file_in_every_protocol() {
    let base64 = DataContent::Base64 {
        data: IMG_B64.to_string(),
    };
    let cases = [
        (
            ApiProtocol::ChatCompletions,
            serde_json::json!({
                "model": "m",
                "messages": [{ "role": "user", "content": [
                    { "type": "image_url", "image_url": { "url": format!("data:image/png;base64,{IMG_B64}") } }
                ]}]
            }),
        ),
        (
            ApiProtocol::Messages,
            serde_json::json!({
                "model": "m", "max_tokens": 16,
                "messages": [{ "role": "user", "content": [
                    { "type": "image", "source": { "type": "base64", "media_type": "image/png", "data": IMG_B64 } }
                ]}]
            }),
        ),
        (
            ApiProtocol::Responses,
            serde_json::json!({
                "model": "m",
                "input": [{ "type": "message", "role": "user", "content": [
                    { "type": "input_image", "image_url": format!("data:image/png;base64,{IMG_B64}") }
                ]}]
            }),
        ),
        (
            ApiProtocol::GenerateContent,
            serde_json::json!({
                "contents": [{ "role": "user", "parts": [
                    { "inlineData": { "mimeType": "image/png", "data": IMG_B64 } }
                ]}]
            }),
        ),
    ];
    for (protocol, body) in cases {
        let prompt = adapter_for(protocol.clone())
            .parse_request(body)
            .unwrap_or_else(|e| panic!("{protocol:?} parse failed: {e:?}"));
        let (media_type, data) = first_file(&prompt);
        assert_eq!(media_type, "image/png", "{protocol:?} media type");
        assert_eq!(data, &base64, "{protocol:?} image data");
        assert!(
            prompt
                .required_capabilities()
                .contains(&Capability::ImageInput),
            "{protocol:?} must require image_input"
        );
    }
}

#[test]
fn image_file_survives_cross_protocol_round_trip() {
    // The 4x4 guarantee: a canonical image renders to each protocol's native
    // shape and parses back to the same canonical file part.
    let base64 = DataContent::Base64 {
        data: IMG_B64.to_string(),
    };
    for protocol in all_protocols() {
        let adapter = adapter_for(protocol.clone());
        let rendered = adapter.render_request(&image_file_prompt()).unwrap();
        let reparsed = adapter.parse_request(rendered).unwrap_or_else(|e| {
            panic!("{protocol:?} could not re-parse its rendered image: {e:?}")
        });
        let (media_type, data) = first_file(&reparsed);
        assert_eq!(media_type, "image/png", "{protocol:?} lost media type");
        assert_eq!(data, &base64, "{protocol:?} lost image data");
    }
}

#[test]
fn image_file_renders_to_chat_image_url() {
    let req = adapter_for(ApiProtocol::ChatCompletions)
        .render_request(&image_file_prompt())
        .unwrap();
    let part = &req["messages"][0]["content"][0];
    assert_eq!(part["type"], "image_url");
    assert_eq!(
        part["image_url"]["url"],
        format!("data:image/png;base64,{IMG_B64}")
    );
}

#[test]
fn image_file_renders_to_messages_image_block() {
    let req = adapter_for(ApiProtocol::Messages)
        .render_request(&image_file_prompt())
        .unwrap();
    let block = &req["messages"][0]["content"][0];
    assert_eq!(block["type"], "image");
    assert_eq!(block["source"]["type"], "base64");
    assert_eq!(block["source"]["media_type"], "image/png");
    assert_eq!(block["source"]["data"], IMG_B64);
}

#[test]
fn image_file_renders_to_generate_content_inline_data() {
    let req = adapter_for(ApiProtocol::GenerateContent)
        .render_request(&image_file_prompt())
        .unwrap();
    let part = &req["contents"][0]["parts"][0];
    assert_eq!(part["inlineData"]["mimeType"], "image/png");
    assert_eq!(part["inlineData"]["data"], IMG_B64);
}

#[test]
fn audio_round_trips_through_chat_completions() {
    let body = serde_json::json!({
        "model": "m",
        "messages": [{ "role": "user", "content": [
            { "type": "input_audio", "input_audio": { "data": IMG_B64, "format": "mp3" } }
        ]}]
    });
    let adapter = adapter_for(ApiProtocol::ChatCompletions);
    let prompt = adapter.parse_request(body).unwrap();
    let (media_type, _) = first_file(&prompt);
    assert_eq!(media_type, "audio/mp3");
    assert!(
        prompt
            .required_capabilities()
            .contains(&Capability::AudioInput)
    );
    let req = adapter.render_request(&prompt).unwrap();
    let part = &req["messages"][0]["content"][0];
    assert_eq!(part["type"], "input_audio");
    assert_eq!(part["input_audio"]["format"], "mp3");
    assert_eq!(part["input_audio"]["data"], IMG_B64);
}

#[test]
fn pdf_file_renders_to_messages_document_block() {
    let mut prompt = image_file_prompt();
    prompt.messages = vec![Message {
        role: Role::User,
        content: vec![Content::File {
            media_type: "application/pdf".to_string(),
            data: DataContent::Base64 {
                data: IMG_B64.to_string(),
            },
            filename: Some("doc.pdf".to_string()),
            extra: Default::default(),
        }],
    }];
    assert!(
        prompt
            .required_capabilities()
            .contains(&Capability::FileInput)
    );
    let req = adapter_for(ApiProtocol::Messages)
        .render_request(&prompt)
        .unwrap();
    let block = &req["messages"][0]["content"][0];
    assert_eq!(block["type"], "document");
    assert_eq!(block["source"]["media_type"], "application/pdf");
    assert_eq!(block["source"]["data"], IMG_B64);
}

#[test]
fn image_url_parses_to_url_data_content() {
    let body = serde_json::json!({
        "model": "m",
        "messages": [{ "role": "user", "content": [
            { "type": "image_url", "image_url": { "url": "https://example.invalid/a.png" } }
        ]}]
    });
    let prompt = adapter_for(ApiProtocol::ChatCompletions)
        .parse_request(body)
        .unwrap();
    let (_, data) = first_file(&prompt);
    assert_eq!(
        data,
        &DataContent::Url {
            url: "https://example.invalid/a.png".to_string(),
        }
    );
}

#[test]
fn response_modalities_round_trip_through_chat_completions() {
    let body = serde_json::json!({
        "model": "m",
        "messages": [{ "role": "user", "content": "draw a cat" }],
        "modalities": ["text", "image"]
    });
    let adapter = adapter_for(ApiProtocol::ChatCompletions);
    let prompt = adapter.parse_request(body).unwrap();
    assert_eq!(
        prompt.params.response_modalities,
        vec![Modality::Text, Modality::Image]
    );
    assert!(
        prompt
            .required_capabilities()
            .contains(&Capability::ImageOutput)
    );
    let req = adapter.render_request(&prompt).unwrap();
    assert_eq!(req["modalities"], serde_json::json!(["text", "image"]));
}

#[test]
fn response_modalities_round_trip_through_generate_content() {
    let body = serde_json::json!({
        "contents": [{ "role": "user", "parts": [{ "text": "draw a cat" }] }],
        "generationConfig": { "responseModalities": ["TEXT", "IMAGE"] }
    });
    let adapter = adapter_for(ApiProtocol::GenerateContent);
    let prompt = adapter.parse_request(body).unwrap();
    assert_eq!(
        prompt.params.response_modalities,
        vec![Modality::Text, Modality::Image]
    );
    assert!(
        prompt
            .required_capabilities()
            .contains(&Capability::ImageOutput)
    );
    let req = adapter.render_request(&prompt).unwrap();
    assert_eq!(
        req["generationConfig"]["responseModalities"],
        serde_json::json!(["TEXT", "IMAGE"])
    );
}

#[test]
fn generated_image_round_trips_through_generate_content_stream() {
    // A generated image survives the canonical stream `File` part through the
    // Gemini encoder (inlineData chunk) and back through its decoder.
    let outbound = adapter_for(ApiProtocol::GenerateContent);
    let part = StreamPart::File {
        media_type: "image/png".to_string(),
        data: DataContent::Base64 {
            data: IMG_B64.to_string(),
        },
    };
    let mut encoder = outbound.stream_encoder("resp_s", "test-model");
    let mut frames = encoder.encode(&part).unwrap();
    frames.extend(encoder.finish().unwrap());

    let mut decoder = outbound.stream_decoder();
    let mut decoded = Vec::new();
    for frame in &frames {
        if let SseFrame::Event { event, data } = frame {
            decoded.extend(
                decoder
                    .decode(&SseEvent {
                        event: event.clone(),
                        data: data.clone(),
                    })
                    .unwrap(),
            );
        }
    }
    decoded.extend(decoder.finish().unwrap());

    let expected = DataContent::Base64 {
        data: IMG_B64.to_string(),
    };
    let file = decoded.iter().find_map(|p| match p {
        StreamPart::File { media_type, data } => Some((media_type.as_str(), data)),
        _ => None,
    });
    assert_eq!(file, Some(("image/png", &expected)));
}

#[test]
fn generated_image_round_trips_through_generate_content_response() {
    let adapter = adapter_for(ApiProtocol::GenerateContent);
    let body = serde_json::json!({
        "candidates": [{
            "content": { "role": "model", "parts": [
                { "inlineData": { "mimeType": "image/png", "data": IMG_B64 } }
            ]},
            "finishReason": "STOP"
        }]
    });
    let result = adapter.parse_response(body).unwrap();
    let expected = DataContent::Base64 {
        data: IMG_B64.to_string(),
    };
    let file = result.content.iter().find_map(|c| match c {
        Content::File {
            media_type, data, ..
        } => Some((media_type.as_str(), data)),
        _ => None,
    });
    assert_eq!(file, Some(("image/png", &expected)));

    let rendered = adapter
        .render_response(&result, &sample_prompt(), "id")
        .unwrap();
    let s = serde_json::to_string(&rendered).unwrap();
    assert!(
        s.contains("inlineData") && s.contains(IMG_B64),
        "rendered response should carry the generated image: {s}"
    );
}

#[test]
fn generated_image_renders_into_chat_response_best_effort() {
    let adapter = adapter_for(ApiProtocol::ChatCompletions);
    let result = GenerateResult {
        content: vec![Content::File {
            media_type: "image/png".to_string(),
            data: DataContent::Base64 {
                data: IMG_B64.to_string(),
            },
            filename: None,
            extra: Default::default(),
        }],
        usage: None,
        finish_reason: Some(FinishReason::Stop),
        response_id: None,
        stop_details: None,
    };
    let rendered = adapter
        .render_response(&result, &sample_prompt(), "id")
        .unwrap();
    let part = &rendered["choices"][0]["message"]["content"][0];
    assert_eq!(part["type"], "image_url");
    assert_eq!(
        part["image_url"]["url"],
        format!("data:image/png;base64,{IMG_B64}")
    );
}

#[test]
fn generated_file_url_round_trips_through_generate_content() {
    // The URL (`fileData` / `fileUri`) output path mirrors the base64 one.
    let adapter = adapter_for(ApiProtocol::GenerateContent);
    let body = serde_json::json!({
        "candidates": [{
            "content": { "role": "model", "parts": [
                { "fileData": { "mimeType": "image/png", "fileUri": "https://example.invalid/g.png" } }
            ]},
            "finishReason": "STOP"
        }]
    });
    let result = adapter.parse_response(body).unwrap();
    let expected = DataContent::Url {
        url: "https://example.invalid/g.png".to_string(),
    };
    let file = result.content.iter().find_map(|c| match c {
        Content::File {
            media_type, data, ..
        } => Some((media_type.as_str(), data)),
        _ => None,
    });
    assert_eq!(file, Some(("image/png", &expected)));

    let rendered = adapter
        .render_response(&result, &sample_prompt(), "id")
        .unwrap();
    let s = serde_json::to_string(&rendered).unwrap();
    assert!(
        s.contains("fileData") && s.contains("https://example.invalid/g.png"),
        "rendered response should carry the file URL: {s}"
    );
}

// ===== structured tool results (LanguageModelV3 ToolResultOutput parity) =====

/// A canonical prompt whose single Tool-role message carries one tool result
/// with the given call id, optional tool name, and typed output.
fn tool_result_prompt(call_id: &str, tool_name: Option<&str>, output: ToolResultOutput) -> Prompt {
    Prompt {
        model: "test-model".to_string(),
        system: None,
        messages: vec![Message {
            role: Role::Tool,
            content: vec![Content::ToolResult {
                call_id: call_id.to_string(),
                tool_name: tool_name.map(str::to_string),
                output,
            }],
        }],
        tools: vec![],
        params: GenerationParams {
            max_tokens: Some(256),
            ..Default::default()
        },
        response_format: None,
        stream: false,
    }
}

/// The first tool result `(call_id, tool_name, output)` in a parsed prompt.
fn first_tool_result(prompt: &Prompt) -> (&str, Option<&str>, &ToolResultOutput) {
    prompt
        .messages
        .iter()
        .flat_map(|m| &m.content)
        .find_map(|c| match c {
            Content::ToolResult {
                call_id,
                tool_name,
                output,
            } => Some((call_id.as_str(), tool_name.as_deref(), output)),
            _ => None,
        })
        .expect("prompt should carry a tool result")
}

/// Render `output` through `protocol` as a tool-result request, re-parse it, and
/// return the canonical output that survived the round trip.
fn round_trip_tool_output(protocol: ApiProtocol, output: ToolResultOutput) -> ToolResultOutput {
    let adapter = adapter_for(protocol.clone());
    let rendered = adapter
        .render_request(&tool_result_prompt("call_1", None, output))
        .unwrap_or_else(|e| panic!("{protocol:?} render_request: {e}"));
    let reparsed = adapter
        .parse_request(rendered)
        .unwrap_or_else(|e| panic!("{protocol:?} parse_request: {e}"));
    first_tool_result(&reparsed).2.clone()
}

#[test]
fn tool_result_text_round_trips_through_string_capable_protocols() {
    // Chat Completions, Messages, and Responses all carry a string tool-result
    // body, so a Text output survives a round trip unchanged. Generate Content
    // is excluded: its `functionResponse.response` is always a JSON object, so
    // Text necessarily degrades there (covered by its own test below).
    for protocol in [
        ApiProtocol::ChatCompletions,
        ApiProtocol::Messages,
        ApiProtocol::Responses,
    ] {
        let output = ToolResultOutput::Text {
            value: "the result is 42".to_string(),
        };
        assert_eq!(
            round_trip_tool_output(protocol.clone(), output.clone()),
            output,
            "{protocol:?} lost a Text tool result"
        );
    }
}

#[test]
fn tool_result_text_degrades_to_json_result_on_gemini() {
    // Gemini's `functionResponse.response` must be a JSON object, so a Text
    // output is rendered losslessly under a `result` key and re-parses as a
    // Json output carrying that key — a faithful, non-dropping degrade.
    // <https://ai.google.dev/api/caching#FunctionResponse>
    let output = ToolResultOutput::Text {
        value: "the result is 42".to_string(),
    };
    let back = round_trip_tool_output(ApiProtocol::GenerateContent, output);
    assert_eq!(
        back,
        ToolResultOutput::Json {
            value: serde_json::json!({ "result": "the result is 42" }),
        },
        "Gemini Text tool result degrades to a Json {{result}} object"
    );
}

#[test]
fn tool_result_json_round_trips_through_generate_content() {
    // Generate Content (`functionResponse.response`) is the only request wire
    // whose tool-result body is a structured JSON *object* slot, so a Json object
    // output survives intact there. (Responses' `output` is a string slot — see
    // `tool_result_json_degrades_to_text_on_string_wires`.)
    let output = ToolResultOutput::Json {
        value: serde_json::json!({ "celsius": 21, "unit": "C" }),
    };
    assert_eq!(
        round_trip_tool_output(ApiProtocol::GenerateContent, output.clone()),
        output,
        "Generate Content lost a Json tool result"
    );
}

#[test]
fn tool_result_json_degrades_to_text_on_string_wires() {
    // Neither OpenAI Chat Completions (`tool` message `content`) nor OpenAI
    // Responses (`function_call_output.output`) has a slot for a bare JSON value —
    // both are string (or part-array) slots. A Json output is therefore
    // stringified and re-parses as Text (a lossless degrade). The Responses case
    // is the audit-proven fix: `output` is `JSON.stringify`d, never a raw object.
    // <https://platform.openai.com/docs/api-reference/chat/create#chat-create-messages>
    // <https://github.com/vercel/ai/blob/main/packages/openai/src/responses/convert-to-openai-responses-input.ts>
    let output = ToolResultOutput::Json {
        value: serde_json::json!({ "celsius": 21, "unit": "C" }),
    };
    for protocol in [ApiProtocol::ChatCompletions, ApiProtocol::Responses] {
        let back = round_trip_tool_output(protocol.clone(), output.clone());
        assert_eq!(
            back,
            ToolResultOutput::Text {
                value: output.to_provider_string(),
            },
            "{protocol:?} Json tool result must degrade to a stringified Text output"
        );
    }
}

#[test]
fn tool_result_json_degrades_to_text_on_anthropic() {
    // Anthropic's tool_result body is a string (or block array), with no slot
    // for a bare JSON value — so a Json output degrades losslessly to its
    // stringified form rather than being dropped.
    let output = ToolResultOutput::Json {
        value: serde_json::json!({ "k": 1 }),
    };
    let back = round_trip_tool_output(ApiProtocol::Messages, output.clone());
    assert_eq!(
        back,
        ToolResultOutput::Text {
            value: output.to_provider_string(),
        },
        "Anthropic Json tool result degrades to a stringified Text output"
    );
}

#[test]
fn tool_result_error_text_round_trips_through_anthropic() {
    // Anthropic is the only request protocol with a native error flag
    // (`tool_result.is_error`); ErrorText must survive a round trip through it.
    let output = ToolResultOutput::ErrorText {
        value: "tool exploded".to_string(),
    };
    assert_eq!(
        round_trip_tool_output(ApiProtocol::Messages, output.clone()),
        output,
        "Anthropic lost an ErrorText tool result"
    );
}

#[test]
fn tool_result_error_json_round_trips_through_anthropic() {
    // Anthropic's error body is a string, so ErrorJson round-trips as ErrorText
    // (the flag survives, the structure flattens — a lossless degrade).
    let output = ToolResultOutput::ErrorJson {
        value: serde_json::json!({ "code": "E_BOOM", "retryable": false }),
    };
    let back = round_trip_tool_output(ApiProtocol::Messages, output.clone());
    assert_eq!(
        back,
        ToolResultOutput::ErrorText {
            value: output.to_provider_string(),
        },
        "Anthropic ErrorJson keeps the error flag, flattening to ErrorText"
    );
}

#[test]
fn tool_result_is_error_renders_anthropic_flag() {
    // The rendered Anthropic wire must carry `is_error: true` for an error
    // output and omit it otherwise.
    let err = adapter_for(ApiProtocol::Messages)
        .render_request(&tool_result_prompt(
            "t1",
            None,
            ToolResultOutput::ErrorText {
                value: "bad".to_string(),
            },
        ))
        .unwrap();
    let err_block = &err["messages"][0]["content"][0];
    assert_eq!(err_block["type"], "tool_result");
    assert_eq!(err_block["is_error"], serde_json::Value::Bool(true));
    assert_eq!(err_block["content"], "bad");

    let ok = adapter_for(ApiProtocol::Messages)
        .render_request(&tool_result_prompt(
            "t1",
            None,
            ToolResultOutput::Text {
                value: "good".to_string(),
            },
        ))
        .unwrap();
    assert!(
        ok["messages"][0]["content"][0].get("is_error").is_none(),
        "a non-error tool result must omit the is_error flag"
    );
}

#[test]
fn tool_result_content_round_trips_through_anthropic() {
    // A multimodal (content-variant) tool result: text + an image part survive a
    // round trip through Anthropic's tool_result block array.
    let output = ToolResultOutput::Content {
        value: vec![
            ToolResultContentPart::Text {
                text: "see image".to_string(),
            },
            ToolResultContentPart::Media {
                media_type: "image/png".to_string(),
                data: DataContent::Base64 {
                    data: IMG_B64.to_string(),
                },
            },
        ],
    };
    assert_eq!(
        round_trip_tool_output(ApiProtocol::Messages, output.clone()),
        output,
        "Anthropic lost a multimodal Content tool result"
    );
}

#[test]
fn tool_result_content_renders_anthropic_image_block() {
    // The rendered Anthropic wire carries the multimodal result as a block
    // array with a `text` block and an `image` block.
    let output = ToolResultOutput::Content {
        value: vec![
            ToolResultContentPart::Text {
                text: "look".to_string(),
            },
            ToolResultContentPart::Media {
                media_type: "image/png".to_string(),
                data: DataContent::Base64 {
                    data: IMG_B64.to_string(),
                },
            },
        ],
    };
    let req = adapter_for(ApiProtocol::Messages)
        .render_request(&tool_result_prompt("t1", None, output))
        .unwrap();
    let blocks = &req["messages"][0]["content"][0]["content"];
    assert_eq!(blocks[0]["type"], "text");
    assert_eq!(blocks[0]["text"], "look");
    assert_eq!(blocks[1]["type"], "image");
    assert_eq!(blocks[1]["source"]["type"], "base64");
    assert_eq!(blocks[1]["source"]["media_type"], "image/png");
    assert_eq!(blocks[1]["source"]["data"], IMG_B64);
}

#[test]
fn tool_result_content_skips_non_image_media_on_anthropic() {
    // Anthropic `tool_result` content accepts only `text` and `image` blocks.
    // A non-image media part (e.g. a PDF) must be skipped, NOT emitted as an
    // `image` block with a non-image media_type.
    // <https://docs.anthropic.com/en/docs/build-with-claude/tool-use#tool-result>
    let output = ToolResultOutput::Content {
        value: vec![
            ToolResultContentPart::Text {
                text: "report".to_string(),
            },
            ToolResultContentPart::Media {
                media_type: "application/pdf".to_string(),
                data: DataContent::Base64 {
                    data: IMG_B64.to_string(),
                },
            },
            ToolResultContentPart::Media {
                media_type: "image/png".to_string(),
                data: DataContent::Base64 {
                    data: IMG_B64.to_string(),
                },
            },
        ],
    };
    let req = adapter_for(ApiProtocol::Messages)
        .render_request(&tool_result_prompt("t1", None, output))
        .unwrap();
    let blocks = req["messages"][0]["content"][0]["content"]
        .as_array()
        .expect("tool_result content is a block array");
    // Only the text block and the single image block survive; the PDF is dropped.
    assert_eq!(
        blocks.len(),
        2,
        "non-image media must be skipped: {blocks:?}"
    );
    assert_eq!(blocks[0]["type"], "text");
    assert_eq!(blocks[1]["type"], "image");
    assert_eq!(blocks[1]["source"]["media_type"], "image/png");
    // No block may carry a non-image media type under an `image` type tag.
    for b in blocks {
        if b["type"] == "image" {
            let mt = b["source"]["media_type"].as_str().unwrap_or("");
            assert!(
                mt.is_empty() || mt.starts_with("image/"),
                "an image block must not carry a non-image media_type: {mt}"
            );
        }
    }
}

#[test]
fn tool_result_content_round_trips_through_chat_completions() {
    // OpenAI Chat Completions carries a multimodal tool result as a content-part
    // array (text + image_url), so a Content output survives there too.
    let output = ToolResultOutput::Content {
        value: vec![
            ToolResultContentPart::Text {
                text: "pic".to_string(),
            },
            ToolResultContentPart::Media {
                media_type: "image/png".to_string(),
                data: DataContent::Base64 {
                    data: IMG_B64.to_string(),
                },
            },
        ],
    };
    assert_eq!(
        round_trip_tool_output(ApiProtocol::ChatCompletions, output.clone()),
        output,
        "Chat Completions lost a multimodal Content tool result"
    );
}

#[test]
fn tool_result_json_renders_responses_output_as_string() {
    // Regression: the Responses `function_call_output.output` field is a
    // `string | content-part-array`, not a bare JSON-value slot. A Json output
    // must be emitted as a *stringified* JSON value (the reference does
    // `JSON.stringify(output.value)`), never as a raw object — otherwise the
    // OpenAI wire rejects it.
    // <https://github.com/vercel/ai/blob/main/packages/openai/src/responses/convert-to-openai-responses-input.ts>
    let value = serde_json::json!({ "celsius": 21, "unit": "C" });
    for output in [
        ToolResultOutput::Json {
            value: value.clone(),
        },
        ToolResultOutput::ErrorJson {
            value: value.clone(),
        },
    ] {
        let req = adapter_for(ApiProtocol::Responses)
            .render_request(&tool_result_prompt("c1", None, output.clone()))
            .unwrap();
        // Find the function_call_output item in the rendered input array.
        let wire_output = req["input"]
            .as_array()
            .expect("input is an array")
            .iter()
            .find(|i| i["type"] == "function_call_output")
            .expect("a function_call_output item")["output"]
            .clone();
        assert!(
            wire_output.is_string(),
            "Responses `output` must be a JSON string, not a bare {:?}: {wire_output:?}",
            output
        );
        // And the string must be the JSON serialization of the value.
        assert_eq!(
            wire_output.as_str().unwrap(),
            value.to_string(),
            "Responses `output` string must be the stringified JSON value"
        );
    }
}

#[test]
fn tool_result_content_round_trips_through_responses() {
    // A multimodal Content tool result (text + inline image + inline non-image
    // file) survives a round trip through the Responses
    // `function_call_output.output` part array. The wire has a real multimodal
    // slot here (`input_text` / `input_image` / `input_file`), so media is
    // preserved rather than flattened to text. Inline `data:` payloads carry the
    // media type so it round-trips exactly (a plain `input_file` URL, by
    // contrast, has no media-type hint — the same subtype loss the Anthropic
    // URL-source path documents).
    let output = ToolResultOutput::Content {
        value: vec![
            ToolResultContentPart::Text {
                text: "see attachments".to_string(),
            },
            ToolResultContentPart::Media {
                media_type: "image/png".to_string(),
                data: DataContent::Base64 {
                    data: IMG_B64.to_string(),
                },
            },
            ToolResultContentPart::Media {
                media_type: "application/pdf".to_string(),
                data: DataContent::Base64 {
                    data: IMG_B64.to_string(),
                },
            },
        ],
    };
    assert_eq!(
        round_trip_tool_output(ApiProtocol::Responses, output.clone()),
        output,
        "Responses lost a multimodal Content tool result"
    );
}

#[test]
fn tool_result_content_responses_file_url_round_trips_as_url() {
    // A non-image file delivered by URL renders as `input_file.file_data` =
    // the URL, and re-parses as a URL-form Media part. The media subtype is not
    // carried on the wire (a bare URL has no type hint), so it degrades to the
    // generic `application/octet-stream` — the type still round-trips, only the
    // subtype is lost, which is the faithful behavior for an untyped URL slot.
    let output = ToolResultOutput::Content {
        value: vec![ToolResultContentPart::Media {
            media_type: "application/pdf".to_string(),
            data: DataContent::Url {
                url: "https://example.invalid/report.pdf".to_string(),
            },
        }],
    };
    let back = round_trip_tool_output(ApiProtocol::Responses, output);
    assert_eq!(
        back,
        ToolResultOutput::Content {
            value: vec![ToolResultContentPart::Media {
                media_type: "application/octet-stream".to_string(),
                data: DataContent::Url {
                    url: "https://example.invalid/report.pdf".to_string(),
                },
            }],
        },
        "a URL-form file part round-trips as a URL, degrading only its subtype"
    );
}

#[test]
fn tool_result_content_renders_responses_part_array() {
    // The rendered Responses wire carries the multimodal result as a part array:
    // text → input_text, image/* → input_image (image_url), other media →
    // input_file (file_data). It must NOT collapse to a string.
    // <https://github.com/vercel/ai/blob/main/packages/openai/src/responses/convert-to-openai-responses-input.ts>
    let output = ToolResultOutput::Content {
        value: vec![
            ToolResultContentPart::Text {
                text: "look".to_string(),
            },
            ToolResultContentPart::Media {
                media_type: "image/png".to_string(),
                data: DataContent::Base64 {
                    data: IMG_B64.to_string(),
                },
            },
            ToolResultContentPart::Media {
                media_type: "application/pdf".to_string(),
                data: DataContent::Url {
                    url: "https://example.invalid/a.pdf".to_string(),
                },
            },
        ],
    };
    let req = adapter_for(ApiProtocol::Responses)
        .render_request(&tool_result_prompt("c1", None, output))
        .unwrap();
    let wire_output = req["input"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["type"] == "function_call_output")
        .unwrap()["output"]
        .clone();
    let parts = wire_output
        .as_array()
        .expect("Responses Content output must be a part array, not a string");
    assert_eq!(parts[0]["type"], "input_text");
    assert_eq!(parts[0]["text"], "look");
    assert_eq!(parts[1]["type"], "input_image");
    assert_eq!(
        parts[1]["image_url"],
        format!("data:image/png;base64,{IMG_B64}")
    );
    assert_eq!(parts[2]["type"], "input_file");
    assert_eq!(parts[2]["file_data"], "https://example.invalid/a.pdf");
}

#[test]
fn tool_result_content_file_id_round_trips_through_responses() {
    // A provider file reference (V3 `file-id` / `image-file-id`) rides the
    // Responses wire as an `input_image` / `input_file` part whose payload is
    // `file_id`. It round-trips as a `FileId` content part. This is the only wire
    // that carries the construct, so the variant is constructed (parse) and
    // consumed (render) here in non-test code.
    // <https://github.com/vercel/ai/blob/main/packages/openai/src/responses/convert-to-openai-responses-input.ts>
    let output = ToolResultOutput::Content {
        value: vec![
            ToolResultContentPart::FileId {
                media_type: Some("image/*".to_string()),
                id: "file-img-123".to_string(),
            },
            ToolResultContentPart::FileId {
                media_type: None,
                id: "file-doc-456".to_string(),
            },
        ],
    };
    // Assert the wire shape first.
    let req = adapter_for(ApiProtocol::Responses)
        .render_request(&tool_result_prompt("c1", None, output.clone()))
        .unwrap();
    let parts = req["input"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["type"] == "function_call_output")
        .unwrap()["output"]
        .as_array()
        .expect("FileId parts render as a part array")
        .clone();
    assert_eq!(parts[0]["type"], "input_image");
    assert_eq!(parts[0]["file_id"], "file-img-123");
    assert_eq!(parts[1]["type"], "input_file");
    assert_eq!(parts[1]["file_id"], "file-doc-456");

    // Then the full round trip.
    assert_eq!(
        round_trip_tool_output(ApiProtocol::Responses, output.clone()),
        output,
        "Responses lost a FileId tool-result content part"
    );
}

#[test]
fn tool_name_survives_gemini_function_response_round_trip() {
    // Gemini keys tool results by function name; `tool_name` must survive a
    // render→parse round trip through `functionResponse`.
    let output = ToolResultOutput::Json {
        value: serde_json::json!({ "ok": true }),
    };
    let adapter = adapter_for(ApiProtocol::GenerateContent);
    let rendered = adapter
        .render_request(&tool_result_prompt(
            "call_1",
            Some("get_weather"),
            output.clone(),
        ))
        .unwrap();
    // The rendered wire carries the tool name under `functionResponse.name`.
    let fr = &rendered["contents"][0]["parts"][0]["functionResponse"];
    assert_eq!(fr["name"], "get_weather");
    assert_eq!(fr["response"]["ok"], true);

    let reparsed = adapter.parse_request(rendered).unwrap();
    let (_, tool_name, parsed_output) = first_tool_result(&reparsed);
    assert_eq!(
        tool_name,
        Some("get_weather"),
        "Gemini functionResponse must preserve the tool name"
    );
    assert_eq!(parsed_output, &output, "Gemini lost the Json tool output");
}

#[test]
fn gemini_function_response_carries_call_id_when_distinct() {
    // When the call id differs from the tool name, the Gemini wire carries it
    // under `functionResponse.id` and a round trip recovers both.
    let adapter = adapter_for(ApiProtocol::GenerateContent);
    let rendered = adapter
        .render_request(&tool_result_prompt(
            "call_42",
            Some("get_weather"),
            ToolResultOutput::Json {
                value: serde_json::json!({ "ok": true }),
            },
        ))
        .unwrap();
    let fr = &rendered["contents"][0]["parts"][0]["functionResponse"];
    assert_eq!(fr["name"], "get_weather");
    assert_eq!(fr["id"], "call_42");

    let reparsed = adapter.parse_request(rendered).unwrap();
    let (call_id, tool_name, _) = first_tool_result(&reparsed);
    assert_eq!(call_id, "call_42", "Gemini lost the distinct call id");
    assert_eq!(tool_name, Some("get_weather"));
}

#[test]
fn tool_result_output_serde_uses_snake_case_tags() {
    // The IR serde representation is the cross-protocol contract: snake_case
    // type tags on both the output union and its content parts.
    let text = serde_json::to_value(ToolResultOutput::Text {
        value: "x".to_string(),
    })
    .unwrap();
    assert_eq!(text["type"], "text");
    assert_eq!(text["value"], "x");

    let err = serde_json::to_value(ToolResultOutput::ErrorText {
        value: "boom".to_string(),
    })
    .unwrap();
    assert_eq!(err["type"], "error_text");

    let err_json = serde_json::to_value(ToolResultOutput::ErrorJson {
        value: serde_json::json!({ "a": 1 }),
    })
    .unwrap();
    assert_eq!(err_json["type"], "error_json");

    let content = serde_json::to_value(ToolResultOutput::Content {
        value: vec![ToolResultContentPart::Media {
            media_type: "image/png".to_string(),
            data: DataContent::Url {
                url: "https://example.invalid/a.png".to_string(),
            },
        }],
    })
    .unwrap();
    assert_eq!(content["type"], "content");
    assert_eq!(content["value"][0]["type"], "media");
    assert_eq!(content["value"][0]["media_type"], "image/png");
    assert_eq!(content["value"][0]["data"]["kind"], "url");
}

#[test]
fn tool_result_content_serde_round_trips() {
    // The whole Content block round-trips through serde unchanged, including the
    // optional tool_name field.
    let original = Content::ToolResult {
        call_id: "c1".to_string(),
        tool_name: Some("calc".to_string()),
        output: ToolResultOutput::Content {
            value: vec![
                ToolResultContentPart::Text {
                    text: "hi".to_string(),
                },
                ToolResultContentPart::Media {
                    media_type: "image/png".to_string(),
                    data: DataContent::Base64 {
                        data: IMG_B64.to_string(),
                    },
                },
            ],
        },
    };
    let value = serde_json::to_value(&original).unwrap();
    assert_eq!(value["type"], "tool_result");
    assert_eq!(value["tool_name"], "calc");
    let back: Content = serde_json::from_value(value).unwrap();
    assert_eq!(back, original);
}

#[test]
fn tool_result_without_tool_name_omits_the_field() {
    // `tool_name` is `skip_serializing_if = "Option::is_none"`, so a result
    // without a name must not emit the key.
    let value = serde_json::to_value(Content::ToolResult {
        call_id: "c1".to_string(),
        tool_name: None,
        output: ToolResultOutput::Text {
            value: "x".to_string(),
        },
    })
    .unwrap();
    assert!(
        value.get("tool_name").is_none(),
        "absent tool_name must omit the key, not emit null: {value}"
    );
}

// ===== Part A: provider-executed tool calls (V3 providerExecuted) =====

/// Anthropic `server_tool_use` response blocks parse as provider-executed tool
/// calls; ordinary `tool_use` blocks do not.
#[test]
fn messages_parse_response_marks_server_tool_use_provider_executed() {
    let adapter = adapter_for(ApiProtocol::Messages);
    let body = serde_json::json!({
        "id": "msg_1",
        "content": [
            { "type": "tool_use", "id": "toolu_1", "name": "get_weather", "input": {"city": "SF"} },
            { "type": "server_tool_use", "id": "srvtoolu_1", "name": "web_search", "input": {"query": "claude shannon"} }
        ],
        "stop_reason": "end_turn"
    });
    let result = adapter.parse_response(body).unwrap();
    let calls: Vec<_> = result
        .content
        .iter()
        .filter_map(|c| match c {
            Content::ToolCall {
                name,
                provider_executed,
                ..
            } => Some((name.as_str(), *provider_executed)),
            _ => None,
        })
        .collect();
    assert_eq!(
        calls,
        vec![("get_weather", false), ("web_search", true)],
        "client tool_use is not provider-executed; server_tool_use is"
    );
}

/// A provider-executed canonical call renders back as an Anthropic
/// `server_tool_use` block; a client call renders as `tool_use`.
#[test]
fn messages_renders_provider_executed_as_server_tool_use() {
    let adapter = adapter_for(ApiProtocol::Messages);
    let result = GenerateResult {
        content: vec![
            Content::ToolCall {
                id: "toolu_1".to_string(),
                name: "get_weather".to_string(),
                arguments: "{}".to_string(),
                provider_executed: false,
            },
            Content::ToolCall {
                id: "srvtoolu_1".to_string(),
                name: "web_search".to_string(),
                arguments: "{\"query\":\"x\"}".to_string(),
                provider_executed: true,
            },
        ],
        usage: None,
        finish_reason: Some(FinishReason::Stop),
        response_id: None,
        stop_details: None,
    };
    let prompt = sample_prompt();
    let rendered = adapter.render_response(&result, &prompt, "msg_1").unwrap();
    let blocks = rendered["content"].as_array().unwrap();
    assert_eq!(blocks[0]["type"], "tool_use");
    assert_eq!(blocks[1]["type"], "server_tool_use");
    assert_eq!(blocks[1]["name"], "web_search");
}

/// A full Anthropic server-tool round-trip preserves the `server_tool_use`
/// block type via the `provider_executed` flag.
#[test]
fn messages_server_tool_use_round_trips() {
    let adapter = adapter_for(ApiProtocol::Messages);
    let body = serde_json::json!({
        "id": "msg_1",
        "content": [
            { "type": "server_tool_use", "id": "srvtoolu_1", "name": "web_search", "input": {"query": "q"} }
        ],
        "stop_reason": "end_turn"
    });
    let parsed = adapter.parse_response(body).unwrap();
    let rendered = adapter
        .render_response(&parsed, &sample_prompt(), "msg_1")
        .unwrap();
    assert_eq!(rendered["content"][0]["type"], "server_tool_use");
    assert_eq!(rendered["content"][0]["id"], "srvtoolu_1");
}

/// OpenAI Responses built-in tool output items parse as provider-executed tool
/// calls; a `function_call` item does not.
#[test]
fn responses_parse_response_marks_server_tools_provider_executed() {
    let adapter = adapter_for(ApiProtocol::Responses);
    let body = serde_json::json!({
        "id": "resp_1",
        "status": "completed",
        "output": [
            { "type": "function_call", "call_id": "call_1", "name": "get_weather", "arguments": "{}" },
            { "type": "web_search_call", "id": "ws_1" },
            { "type": "file_search_call", "id": "fs_1" },
            { "type": "code_interpreter_call", "id": "ci_1", "code": "print(1)", "container_id": "cntr_1" }
        ]
    });
    let result = adapter.parse_response(body).unwrap();
    let calls: Vec<_> = result
        .content
        .iter()
        .filter_map(|c| match c {
            Content::ToolCall {
                name,
                provider_executed,
                arguments,
                ..
            } => Some((name.as_str(), *provider_executed, arguments.as_str())),
            _ => None,
        })
        .collect();
    assert_eq!(calls[0], ("get_weather", false, "{}"));
    assert_eq!(calls[1], ("web_search", true, "{}"));
    assert_eq!(calls[2], ("file_search", true, "{}"));
    assert_eq!(calls[3].0, "code_interpreter");
    assert!(calls[3].1, "code_interpreter_call is provider-executed");
    // its code / container survive as the call input
    let ci_input: serde_json::Value = serde_json::from_str(calls[3].2).unwrap();
    assert_eq!(ci_input["code"], "print(1)");
    assert_eq!(ci_input["containerId"], "cntr_1");
}

/// On the Responses *request* (input) side a provider-executed call is NOT
/// re-emitted as a client `function_call` item — the provider already ran it.
#[test]
fn responses_render_request_drops_provider_executed_calls() {
    let adapter = adapter_for(ApiProtocol::Responses);
    let prompt = Prompt {
        model: "gpt-5".to_string(),
        system: None,
        messages: vec![Message {
            role: Role::Assistant,
            content: vec![
                Content::ToolCall {
                    id: "call_1".to_string(),
                    name: "get_weather".to_string(),
                    arguments: "{}".to_string(),
                    provider_executed: false,
                },
                Content::ToolCall {
                    id: "ws_1".to_string(),
                    name: "web_search".to_string(),
                    arguments: "{}".to_string(),
                    provider_executed: true,
                },
            ],
        }],
        tools: Vec::new(),
        params: GenerationParams::default(),
        response_format: None,
        stream: false,
    };
    let rendered = adapter.render_request(&prompt).unwrap();
    let input = rendered["input"].as_array().unwrap();
    let function_calls: Vec<_> = input
        .iter()
        .filter(|i| i["type"] == "function_call")
        .collect();
    assert_eq!(
        function_calls.len(),
        1,
        "only the client function_call is re-sent as input: {rendered}"
    );
    assert_eq!(function_calls[0]["call_id"], "call_1");
}

/// On the Responses *response* (output) side a provider-executed call is
/// reproduced as its native server-tool output item, keyed by `id`.
#[test]
fn responses_render_response_reproduces_server_tool_item() {
    let adapter = adapter_for(ApiProtocol::Responses);
    let result = GenerateResult {
        content: vec![Content::ToolCall {
            id: "ws_1".to_string(),
            name: "web_search".to_string(),
            arguments: "{}".to_string(),
            provider_executed: true,
        }],
        usage: None,
        finish_reason: Some(FinishReason::Stop),
        response_id: Some("resp_1".to_string()),
        stop_details: None,
    };
    let rendered = adapter
        .render_response(&result, &sample_prompt(), "resp_1")
        .unwrap();
    let output = rendered["output"].as_array().unwrap();
    let item = output
        .iter()
        .find(|i| i["type"] == "web_search_call")
        .expect("server-tool output item reproduced");
    assert_eq!(item["id"], "ws_1");
    assert!(
        output.iter().all(|i| i["type"] != "function_call"),
        "a provider-executed call must not render as function_call"
    );
}

/// `image_generation_call` and `computer_call` output items are parsed as
/// provider-executed tool calls (no echoed input, matching the AI SDK), and they
/// round-trip: `render_response` reproduces each as its native `<name>_call`
/// item keyed by `id`. `local_shell_call` / `mcp_call` are deferred and not
/// parsed (so they must NOT appear as tool calls), per the documented skip.
#[test]
fn responses_parses_and_reproduces_image_and_computer_calls() {
    let adapter = adapter_for(ApiProtocol::Responses);
    let body = serde_json::json!({
        "id": "resp_1",
        "status": "completed",
        "output": [
            { "type": "image_generation_call", "id": "ig_1", "result": "BASE64..." },
            { "type": "computer_call", "id": "cu_1", "status": "completed" },
            // deferred — must not surface as tool calls
            { "type": "local_shell_call", "call_id": "ls_1", "action": {"command": ["ls"]} },
            { "type": "mcp_call", "id": "mc_1", "name": "fetch", "arguments": "{}" }
        ]
    });
    let result = adapter.parse_response(body).unwrap();
    let calls: Vec<_> = result
        .content
        .iter()
        .filter_map(|c| match c {
            Content::ToolCall {
                name,
                provider_executed,
                arguments,
                ..
            } => Some((name.as_str(), *provider_executed, arguments.as_str())),
            _ => None,
        })
        .collect();
    // Only the two (a)-mapped server tools are parsed; the deferred items drop.
    assert_eq!(
        calls,
        vec![("image_generation", true, "{}"), ("computer", true, "{}"),],
        "image_generation_call/computer_call parse; local_shell_call/mcp_call defer"
    );

    // Live, not dead: each parsed call is consumed by the reproduction site,
    // re-emitting its native `<name>_call` output item keyed by `id`.
    let rendered = adapter
        .render_response(&result, &sample_prompt(), "resp_1")
        .unwrap();
    let output = rendered["output"].as_array().unwrap();
    let ig = output
        .iter()
        .find(|i| i["type"] == "image_generation_call")
        .expect("image_generation_call reproduced");
    assert_eq!(ig["id"], "ig_1");
    let cu = output
        .iter()
        .find(|i| i["type"] == "computer_call")
        .expect("computer_call reproduced");
    assert_eq!(cu["id"], "cu_1");
    assert!(
        output.iter().all(|i| i["type"] != "function_call"),
        "provider-executed calls must not render as function_call: {rendered}"
    );
}

/// `provider_executed` defaults to false and is omitted from the serialized
/// canonical form when false (no JSON `null`, no `false` noise).
#[test]
fn tool_call_provider_executed_omitted_when_false() {
    let value = serde_json::to_value(Content::ToolCall {
        id: "c1".to_string(),
        name: "t".to_string(),
        arguments: "{}".to_string(),
        provider_executed: false,
    })
    .unwrap();
    assert!(
        value.get("provider_executed").is_none(),
        "false provider_executed must be omitted: {value}"
    );
    let value_true = serde_json::to_value(Content::ToolCall {
        id: "c1".to_string(),
        name: "t".to_string(),
        arguments: "{}".to_string(),
        provider_executed: true,
    })
    .unwrap();
    assert_eq!(value_true["provider_executed"], true);
}

// ===== Part B: typed tool choice (V3 toolChoice) =====

/// Read whatever a protocol renders for `tool_choice` from an outbound request
/// body, normalising Gemini's top-level `toolConfig` and the simple-string
/// forms into a comparable canonical [`ToolChoice`] via the inbound parse path.
fn rendered_tool_choice(protocol: ApiProtocol, prompt: &Prompt) -> Option<ToolChoice> {
    let adapter = adapter_for(protocol);
    let rendered = adapter.render_request(prompt).unwrap();
    // Round-trip back through the same protocol's parser, which is the public
    // contract: render then parse must recover the typed choice.
    adapter.parse_request(rendered).unwrap().params.tool_choice
}

/// A prompt carrying a specific tool choice.
fn prompt_with_tool_choice(choice: ToolChoice) -> Prompt {
    Prompt {
        model: "m".to_string(),
        system: None,
        messages: vec![Message::text(Role::User, "hi")],
        tools: vec![Tool::Function {
            name: "search".to_string(),
            description: None,
            parameters: serde_json::json!({"type": "object"}),
            strict: None,
        }],
        params: GenerationParams {
            tool_choice: Some(choice),
            ..Default::default()
        },
        response_format: None,
        stream: false,
    }
}

/// Each canonical [`ToolChoice`] survives a render→parse round-trip on every
/// protocol — the same-protocol fidelity contract.
#[test]
fn tool_choice_same_protocol_round_trips_on_all_protocols() {
    let choices = [
        ToolChoice::Auto,
        ToolChoice::None,
        ToolChoice::Required,
        ToolChoice::Tool {
            name: "search".to_string(),
        },
    ];
    for choice in choices {
        for proto in all_protocols() {
            let prompt = prompt_with_tool_choice(choice.clone());
            assert_eq!(
                rendered_tool_choice(proto.clone(), &prompt),
                Some(choice.clone()),
                "{proto:?} must round-trip tool_choice {choice:?}"
            );
        }
    }
}

/// Cross-protocol translation: a `Required` choice authored in one protocol
/// reaches every other protocol as that protocol's "must call a tool" form
/// (Chat `required` ↔ Anthropic `any` ↔ Gemini `ANY` ↔ Responses `required`).
#[test]
fn tool_choice_required_translates_across_protocols() {
    // Author it as a raw Chat Completions body, parse to canonical, then render
    // to every protocol and assert the native shape.
    let chat = adapter_for(ApiProtocol::ChatCompletions);
    let body = serde_json::json!({
        "model": "gpt-5",
        "messages": [{"role": "user", "content": "hi"}],
        "tool_choice": "required"
    });
    let prompt = chat.parse_request(body).unwrap();
    assert_eq!(prompt.params.tool_choice, Some(ToolChoice::Required));

    // Anthropic -> {type:"any"}
    let anthropic = adapter_for(ApiProtocol::Messages)
        .render_request(&prompt)
        .unwrap();
    assert_eq!(anthropic["tool_choice"], serde_json::json!({"type": "any"}));
    // Responses -> "required"
    let responses = adapter_for(ApiProtocol::Responses)
        .render_request(&prompt)
        .unwrap();
    assert_eq!(responses["tool_choice"], "required");
    // Gemini -> toolConfig.functionCallingConfig.mode = ANY
    let gemini = adapter_for(ApiProtocol::GenerateContent)
        .render_request(&prompt)
        .unwrap();
    assert_eq!(gemini["toolConfig"]["functionCallingConfig"]["mode"], "ANY");
}

/// Cross-protocol translation of a forced specific tool (V3 `tool`) — by name —
/// reaches all four protocols in their native shape.
#[test]
fn tool_choice_specific_tool_translates_across_protocols() {
    let prompt = prompt_with_tool_choice(ToolChoice::Tool {
        name: "search".to_string(),
    });
    // Chat Completions: {type:"function", function:{name}}
    let chat = adapter_for(ApiProtocol::ChatCompletions)
        .render_request(&prompt)
        .unwrap();
    assert_eq!(
        chat["tool_choice"],
        serde_json::json!({"type": "function", "function": {"name": "search"}})
    );
    // Responses: {type:"function", name} (flat)
    let responses = adapter_for(ApiProtocol::Responses)
        .render_request(&prompt)
        .unwrap();
    assert_eq!(
        responses["tool_choice"],
        serde_json::json!({"type": "function", "name": "search"})
    );
    // Anthropic: {type:"tool", name}
    let anthropic = adapter_for(ApiProtocol::Messages)
        .render_request(&prompt)
        .unwrap();
    assert_eq!(
        anthropic["tool_choice"],
        serde_json::json!({"type": "tool", "name": "search"})
    );
    // Gemini: mode ANY + allowedFunctionNames:[name]
    let gemini = adapter_for(ApiProtocol::GenerateContent)
        .render_request(&prompt)
        .unwrap();
    assert_eq!(
        gemini["toolConfig"]["functionCallingConfig"],
        serde_json::json!({"mode": "ANY", "allowedFunctionNames": ["search"]})
    );
}

/// `None` is faithfully expressed on every protocol (Chat `"none"`, Anthropic
/// `{type:"none"}` — which Anthropic natively supports — Responses `"none"`,
/// Gemini `mode:"NONE"`).
#[test]
fn tool_choice_none_translates_across_protocols() {
    let prompt = prompt_with_tool_choice(ToolChoice::None);
    assert_eq!(
        adapter_for(ApiProtocol::ChatCompletions)
            .render_request(&prompt)
            .unwrap()["tool_choice"],
        "none"
    );
    assert_eq!(
        adapter_for(ApiProtocol::Messages)
            .render_request(&prompt)
            .unwrap()["tool_choice"],
        serde_json::json!({"type": "none"})
    );
    assert_eq!(
        adapter_for(ApiProtocol::Responses)
            .render_request(&prompt)
            .unwrap()["tool_choice"],
        "none"
    );
    assert_eq!(
        adapter_for(ApiProtocol::GenerateContent)
            .render_request(&prompt)
            .unwrap()["toolConfig"]["functionCallingConfig"]["mode"],
        "NONE"
    );
}

/// A parsed `tool_choice` must NOT also remain in the raw `extra` passthrough —
/// otherwise it would be double-written (and forwarded verbatim cross-protocol).
#[test]
fn parsed_tool_choice_is_not_duplicated_in_extra() {
    // Chat Completions
    let chat = adapter_for(ApiProtocol::ChatCompletions);
    let prompt = chat
        .parse_request(serde_json::json!({
            "model": "gpt-5",
            "messages": [{"role": "user", "content": "hi"}],
            "tool_choice": "required"
        }))
        .unwrap();
    assert!(prompt.params.tool_choice.is_some());
    assert!(
        !prompt.params.extra.contains_key("tool_choice"),
        "Chat Completions tool_choice must be removed from extra"
    );

    // Anthropic
    let anthropic = adapter_for(ApiProtocol::Messages);
    let prompt = anthropic
        .parse_request(serde_json::json!({
            "model": "claude-opus-4-8",
            "max_tokens": 16,
            "messages": [{"role": "user", "content": "hi"}],
            "tool_choice": {"type": "any"}
        }))
        .unwrap();
    assert!(prompt.params.tool_choice.is_some());
    assert!(
        !prompt.params.extra.contains_key("tool_choice"),
        "Anthropic tool_choice must be removed from extra"
    );

    // Responses
    let responses = adapter_for(ApiProtocol::Responses);
    let prompt = responses
        .parse_request(serde_json::json!({
            "model": "gpt-5",
            "input": "hi",
            "tool_choice": "auto"
        }))
        .unwrap();
    assert!(prompt.params.tool_choice.is_some());
    assert!(
        !prompt.params.extra.contains_key("tool_choice"),
        "Responses tool_choice must be removed from extra"
    );

    // Gemini: toolConfig is lifted out of the top-level sentinel entirely.
    let gemini = adapter_for(ApiProtocol::GenerateContent);
    let prompt = gemini
        .parse_request(serde_json::json!({
            "model": "gemini-2.0-flash",
            "contents": [{"role": "user", "parts": [{"text": "hi"}]}],
            "toolConfig": {"functionCallingConfig": {"mode": "ANY"}}
        }))
        .unwrap();
    assert_eq!(prompt.params.tool_choice, Some(ToolChoice::Required));
    let top_level = prompt
        .params
        .extra
        .get("__google_top_level__")
        .and_then(|v| v.as_object());
    assert!(
        top_level
            .map(|t| !t.contains_key("toolConfig"))
            .unwrap_or(true),
        "Gemini toolConfig must be lifted out of the top-level extras: {:?}",
        prompt.params.extra
    );
}

/// An exotic `tool_choice` shape that does not map onto any V3 variant is
/// preserved verbatim via [`ToolChoice::Other`] and round-trips losslessly on
/// the same protocol.
#[test]
fn exotic_tool_choice_falls_back_to_other_and_round_trips() {
    // Responses `allowed_tools` constraint — no V3 equivalent.
    let exotic = serde_json::json!({
        "type": "allowed_tools",
        "mode": "auto",
        "tools": [{"type": "function", "name": "search"}]
    });
    let responses = adapter_for(ApiProtocol::Responses);
    let prompt = responses
        .parse_request(serde_json::json!({
            "model": "gpt-5",
            "input": "hi",
            "tool_choice": exotic.clone()
        }))
        .unwrap();
    assert_eq!(
        prompt.params.tool_choice,
        Some(ToolChoice::Other {
            value: exotic.clone()
        })
    );
    let rendered = responses.render_request(&prompt).unwrap();
    assert_eq!(
        rendered["tool_choice"], exotic,
        "Other tool_choice must render back byte-for-byte"
    );

    // Gemini `ANY` + multiple allowedFunctionNames — also no V3 equivalent.
    let gemini = adapter_for(ApiProtocol::GenerateContent);
    let tool_config = serde_json::json!({
        "functionCallingConfig": {"mode": "ANY", "allowedFunctionNames": ["a", "b"]}
    });
    let prompt = gemini
        .parse_request(serde_json::json!({
            "model": "gemini-2.0-flash",
            "contents": [{"role": "user", "parts": [{"text": "hi"}]}],
            "toolConfig": tool_config.clone()
        }))
        .unwrap();
    assert!(matches!(
        prompt.params.tool_choice,
        Some(ToolChoice::Other { .. })
    ));
    let rendered = gemini.render_request(&prompt).unwrap();
    assert_eq!(
        rendered["toolConfig"], tool_config,
        "Gemini exotic toolConfig must render back verbatim"
    );
}

/// Regression: a Gemini `toolConfig` whose `functionCallingConfig` cannot be
/// reduced to a typed [`ToolChoice`] — because it omits `mode`, or carries an
/// unmodelled sibling key — must NOT vanish. Before the parser was made
/// infallible it returned `None` for these shapes while the caller had already
/// removed `toolConfig` from the top-level extras, so the choice was dropped and
/// broke even a same-protocol Gemini→Gemini round-trip. It must now degrade to
/// `ToolChoice::Other` and re-emit verbatim, with no duplicate left in extras.
#[test]
fn gemini_unreducible_tool_config_survives_round_trip() {
    let gemini = adapter_for(ApiProtocol::GenerateContent);
    // Case 1: `functionCallingConfig` with `allowedFunctionNames` but NO `mode`.
    // Case 2: a different shape — a `functionCallingConfig` carrying an unknown
    // sibling key alongside a recognised `mode`.
    let configs = [
        serde_json::json!({
            "functionCallingConfig": {"allowedFunctionNames": ["lookup"]}
        }),
        serde_json::json!({
            "functionCallingConfig": {"mode": "VALIDATED", "responseSchema": {"x": 1}}
        }),
    ];
    for tool_config in configs {
        let prompt = gemini
            .parse_request(serde_json::json!({
                "model": "gemini-2.0-flash",
                "contents": [{"role": "user", "parts": [{"text": "hi"}]}],
                "toolConfig": tool_config.clone()
            }))
            .unwrap();
        // The unreducible config is preserved verbatim in the typed slot, not
        // dropped.
        assert_eq!(
            prompt.params.tool_choice,
            Some(ToolChoice::Other {
                value: tool_config.clone()
            }),
            "unreducible toolConfig must be preserved as Other: {tool_config}"
        );
        // It must NOT also linger in the top-level extras sentinel (otherwise it
        // would be double-written on render).
        let lingering = prompt
            .params
            .extra
            .get("__google_top_level__")
            .and_then(|v| v.as_object())
            .map(|t| t.contains_key("toolConfig"))
            .unwrap_or(false);
        assert!(
            !lingering,
            "toolConfig must be lifted out of extras, not duplicated: {:?}",
            prompt.params.extra
        );
        // Gemini→Gemini render re-emits the original toolConfig byte-for-byte,
        // exactly once at the request root.
        let rendered = gemini.render_request(&prompt).unwrap();
        assert_eq!(
            rendered["toolConfig"], tool_config,
            "unreducible toolConfig must round-trip verbatim"
        );
        // And a second render→parse→render hop is still stable (no drift).
        let reparsed = gemini.parse_request(rendered).unwrap();
        let rerendered = gemini.render_request(&reparsed).unwrap();
        assert_eq!(
            rerendered["toolConfig"], tool_config,
            "unreducible toolConfig must remain stable across a second hop"
        );
    }
}

/// Cross-protocol translation of `Auto`: authored as a Chat Completions
/// `"auto"`, it must reach Anthropic as `{type:"auto"}` and Gemini as
/// `mode:"AUTO"` (and stay `"auto"` on Responses).
#[test]
fn tool_choice_auto_translates_across_protocols() {
    let chat = adapter_for(ApiProtocol::ChatCompletions);
    let prompt = chat
        .parse_request(serde_json::json!({
            "model": "gpt-5",
            "messages": [{"role": "user", "content": "hi"}],
            "tool_choice": "auto"
        }))
        .unwrap();
    assert_eq!(prompt.params.tool_choice, Some(ToolChoice::Auto));

    // Anthropic -> {type:"auto"}
    assert_eq!(
        adapter_for(ApiProtocol::Messages)
            .render_request(&prompt)
            .unwrap()["tool_choice"],
        serde_json::json!({"type": "auto"})
    );
    // Gemini -> toolConfig.functionCallingConfig.mode = AUTO
    assert_eq!(
        adapter_for(ApiProtocol::GenerateContent)
            .render_request(&prompt)
            .unwrap()["toolConfig"]["functionCallingConfig"]["mode"],
        "AUTO"
    );
    // Responses -> "auto"
    assert_eq!(
        adapter_for(ApiProtocol::Responses)
            .render_request(&prompt)
            .unwrap()["tool_choice"],
        "auto"
    );
}

/// The canonical `tool_choice` field is omitted from the serialized
/// `GenerationParams` when absent (no JSON `null`).
#[test]
fn generation_params_omits_absent_tool_choice() {
    let value = serde_json::to_value(GenerationParams::default()).unwrap();
    assert!(
        value.get("tool_choice").is_none(),
        "absent tool_choice must be omitted: {value}"
    );
}

// ===== Part C: typed tools (V3 function `strict` + provider-defined tools) =====

/// A prompt carrying exactly the given tool list (and nothing else interesting).
fn prompt_with_tools(tools: Vec<Tool>) -> Prompt {
    Prompt {
        model: "m".to_string(),
        system: None,
        messages: vec![Message::text(Role::User, "hi")],
        tools,
        params: GenerationParams::default(),
        response_format: None,
        stream: false,
    }
}

/// Render a tool list through `protocol` and parse it back through the same
/// protocol — the same-protocol fidelity contract. Returns the recovered tools.
fn round_trip_tools(protocol: &ApiProtocol, tools: Vec<Tool>) -> Vec<Tool> {
    let adapter = adapter_for(protocol.clone());
    let prompt = prompt_with_tools(tools);
    let rendered = adapter.render_request(&prompt).unwrap();
    adapter.parse_request(rendered).unwrap().tools
}

/// The raw `tools` JSON a protocol renders for a tool list (for native-shape
/// assertions).
fn rendered_tools_json(protocol: &ApiProtocol, tools: Vec<Tool>) -> serde_json::Value {
    adapter_for(protocol.clone())
        .render_request(&prompt_with_tools(tools))
        .unwrap()["tools"]
        .clone()
}

// --- function-tool `strict` ---

/// A function tool's `strict` flag survives a same-protocol round-trip on the
/// two protocols whose wire has a `strict` slot (Chat Completions, Responses).
#[test]
fn function_tool_strict_round_trips_on_openai_protocols() {
    for protocol in &[ApiProtocol::ChatCompletions, ApiProtocol::Responses] {
        let tools = round_trip_tools(
            protocol,
            vec![Tool::Function {
                name: "get_weather".to_string(),
                description: Some("look up weather".to_string()),
                parameters: serde_json::json!({ "type": "object" }),
                strict: Some(true),
            }],
        );
        assert_eq!(
            tools,
            vec![Tool::Function {
                name: "get_weather".to_string(),
                description: Some("look up weather".to_string()),
                parameters: serde_json::json!({ "type": "object" }),
                strict: Some(true),
            }],
            "{protocol:?} must preserve function-tool strict"
        );
    }
}

/// Chat Completions renders `strict` under `function`; Responses renders it flat.
#[test]
fn function_tool_strict_renders_in_native_position() {
    let tool = Tool::Function {
        name: "f".to_string(),
        description: None,
        parameters: serde_json::json!({ "type": "object" }),
        strict: Some(true),
    };
    let chat = rendered_tools_json(&ApiProtocol::ChatCompletions, vec![tool.clone()]);
    assert_eq!(chat[0]["type"], "function");
    assert_eq!(chat[0]["function"]["strict"], true);

    let responses = rendered_tools_json(&ApiProtocol::Responses, vec![tool]);
    assert_eq!(responses[0]["type"], "function");
    assert_eq!(responses[0]["strict"], true);
}

/// `strict` is omitted (not emitted as `false`/`null`) when unset.
#[test]
fn function_tool_strict_omitted_when_absent() {
    let tool = Tool::Function {
        name: "f".to_string(),
        description: None,
        parameters: serde_json::json!({ "type": "object" }),
        strict: None,
    };
    let chat = rendered_tools_json(&ApiProtocol::ChatCompletions, vec![tool.clone()]);
    assert!(
        chat[0]["function"].get("strict").is_none(),
        "absent strict must be omitted: {chat}"
    );
    let responses = rendered_tools_json(&ApiProtocol::Responses, vec![tool]);
    assert!(
        responses[0].get("strict").is_none(),
        "absent strict must be omitted: {responses}"
    );
}

/// Anthropic and Gemini have no `strict` slot — the flag is documented as
/// dropped. A function tool still round-trips otherwise; `strict` comes back
/// `None`.
#[test]
fn function_tool_strict_dropped_on_anthropic_and_gemini() {
    for protocol in &[ApiProtocol::Messages, ApiProtocol::GenerateContent] {
        let tools = round_trip_tools(
            protocol,
            vec![Tool::Function {
                name: "get_weather".to_string(),
                description: Some("desc".to_string()),
                parameters: serde_json::json!({ "type": "object" }),
                strict: Some(true),
            }],
        );
        assert_eq!(
            tools,
            vec![Tool::Function {
                name: "get_weather".to_string(),
                description: Some("desc".to_string()),
                parameters: serde_json::json!({ "type": "object" }),
                strict: None,
            }],
            "{protocol:?} has no strict slot; it must drop to None but keep the tool"
        );
        // And it is never emitted on the wire.
        let rendered = rendered_tools_json(
            protocol,
            vec![Tool::Function {
                name: "f".to_string(),
                description: None,
                parameters: serde_json::json!({}),
                strict: Some(true),
            }],
        );
        assert!(
            !rendered.to_string().contains("strict"),
            "{protocol:?} must not emit strict: {rendered}"
        );
    }
}

// --- provider-defined tools: same-protocol round-trips ---

/// Every Responses server tool round-trips losslessly (id `openai.<type>` +
/// verbatim args), covering the full documented set.
#[test]
fn responses_provider_defined_tools_round_trip() {
    let cases = vec![
        Tool::ProviderDefined {
            id: "openai.web_search_preview".to_string(),
            name: "web_search_preview".to_string(),
            args: serde_json::json!({ "search_context_size": "high" }),
        },
        Tool::ProviderDefined {
            id: "openai.code_interpreter".to_string(),
            name: "code_interpreter".to_string(),
            args: serde_json::json!({ "container": { "type": "auto" } }),
        },
        Tool::ProviderDefined {
            id: "openai.file_search".to_string(),
            name: "file_search".to_string(),
            args: serde_json::json!({ "vector_store_ids": ["vs_1"], "max_num_results": 5 }),
        },
        Tool::ProviderDefined {
            id: "openai.image_generation".to_string(),
            name: "image_generation".to_string(),
            args: serde_json::json!({ "quality": "high" }),
        },
        Tool::ProviderDefined {
            id: "openai.computer_use_preview".to_string(),
            name: "computer_use_preview".to_string(),
            args: serde_json::json!({ "display_width": 1024, "display_height": 768, "environment": "browser" }),
        },
    ];
    for tool in cases {
        let round_tripped = round_trip_tools(&ApiProtocol::Responses, vec![tool.clone()]);
        assert_eq!(round_tripped, vec![tool.clone()], "round-trip for {tool:?}");
    }
}

/// Responses renders a server tool flat as `{type:<tool>, …args}`.
#[test]
fn responses_provider_defined_renders_flat_native_shape() {
    let rendered = rendered_tools_json(
        &ApiProtocol::Responses,
        vec![Tool::ProviderDefined {
            id: "openai.web_search_preview".to_string(),
            name: "web_search_preview".to_string(),
            args: serde_json::json!({ "search_context_size": "low" }),
        }],
    );
    assert_eq!(rendered[0]["type"], "web_search_preview");
    assert_eq!(rendered[0]["search_context_size"], "low");
    // No function-only keys leak onto a server tool.
    assert!(rendered[0].get("parameters").is_none());
}

/// Every Anthropic server tool round-trips losslessly (an `anthropic.<version>`
/// id with a stable `name` and verbatim args), covering web search, code
/// execution, and computer use.
#[test]
fn messages_provider_defined_tools_round_trip() {
    let cases = vec![
        Tool::ProviderDefined {
            id: "anthropic.web_search_20250305".to_string(),
            name: "web_search".to_string(),
            args: serde_json::json!({ "max_uses": 5 }),
        },
        Tool::ProviderDefined {
            id: "anthropic.code_execution_20250522".to_string(),
            name: "code_execution".to_string(),
            args: serde_json::json!({}),
        },
        Tool::ProviderDefined {
            id: "anthropic.computer_20250124".to_string(),
            name: "computer".to_string(),
            args: serde_json::json!({ "display_width_px": 1024, "display_height_px": 768 }),
        },
    ];
    for tool in cases {
        let round_tripped = round_trip_tools(&ApiProtocol::Messages, vec![tool.clone()]);
        assert_eq!(round_tripped, vec![tool.clone()], "round-trip for {tool:?}");
    }
}

/// Anthropic renders a server tool as `{type:<version>, name, …args}`.
#[test]
fn messages_provider_defined_renders_versioned_native_shape() {
    let rendered = rendered_tools_json(
        &ApiProtocol::Messages,
        vec![Tool::ProviderDefined {
            id: "anthropic.web_search_20250305".to_string(),
            name: "web_search".to_string(),
            args: serde_json::json!({ "max_uses": 3 }),
        }],
    );
    assert_eq!(rendered[0]["type"], "web_search_20250305");
    assert_eq!(rendered[0]["name"], "web_search");
    assert_eq!(rendered[0]["max_uses"], 3);
}

/// Every Gemini built-in tool round-trips losslessly (id `google.<key>` +
/// verbatim args), covering search, code execution, and URL context.
#[test]
fn generate_content_provider_defined_tools_round_trip() {
    let cases = vec![
        Tool::ProviderDefined {
            id: "google.googleSearch".to_string(),
            name: "googleSearch".to_string(),
            args: serde_json::json!({}),
        },
        Tool::ProviderDefined {
            id: "google.codeExecution".to_string(),
            name: "codeExecution".to_string(),
            args: serde_json::json!({}),
        },
        Tool::ProviderDefined {
            id: "google.googleSearchRetrieval".to_string(),
            name: "googleSearchRetrieval".to_string(),
            args: serde_json::json!({ "dynamicRetrievalConfig": { "mode": "MODE_DYNAMIC" } }),
        },
        Tool::ProviderDefined {
            id: "google.urlContext".to_string(),
            name: "urlContext".to_string(),
            args: serde_json::json!({}),
        },
    ];
    for tool in cases {
        let round_tripped = round_trip_tools(&ApiProtocol::GenerateContent, vec![tool.clone()]);
        assert_eq!(round_tripped, vec![tool.clone()], "round-trip for {tool:?}");
    }
}

/// Gemini renders a built-in tool as a single-key `{<toolKey>: args}` object.
#[test]
fn generate_content_provider_defined_renders_single_key_object() {
    let rendered = rendered_tools_json(
        &ApiProtocol::GenerateContent,
        vec![Tool::ProviderDefined {
            id: "google.googleSearch".to_string(),
            name: "googleSearch".to_string(),
            args: serde_json::json!({}),
        }],
    );
    // The built-in tool is its own tool-array element with the camelCase key.
    let entry = rendered
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e.get("googleSearch").is_some())
        .expect("googleSearch entry present");
    assert_eq!(entry["googleSearch"], serde_json::json!({}));
}

/// A single Gemini tool object carrying both `functionDeclarations` and a
/// built-in `googleSearch` key expands into both a function tool and a
/// provider-defined tool, and rebuilds the same wire on render.
#[test]
fn generate_content_mixed_function_and_builtin_tools_round_trip() {
    let adapter = adapter_for(ApiProtocol::GenerateContent);
    let body = serde_json::json!({
        "contents": [{ "role": "user", "parts": [{ "text": "hi" }] }],
        "tools": [{
            "functionDeclarations": [{
                "name": "get_weather",
                "description": "w",
                "parameters": { "type": "object" }
            }],
            "googleSearch": {}
        }]
    });
    let parsed = adapter.parse_request(body).unwrap();
    assert_eq!(parsed.tools.len(), 2, "one function + one builtin");
    assert!(matches!(parsed.tools[0], Tool::Function { .. }));
    assert!(matches!(parsed.tools[1], Tool::ProviderDefined { .. }));

    // Re-render and re-parse: both tools survive (the function-declarations
    // object plus the googleSearch element).
    let rendered = adapter.render_request(&parsed).unwrap();
    let reparsed = adapter.parse_request(rendered).unwrap();
    assert_eq!(reparsed.tools, parsed.tools);
}

/// A Responses array mixing a function tool and a server tool keeps both, in
/// order, with the function/provider split intact.
#[test]
fn responses_mixed_function_and_server_tools_round_trip() {
    let tools = vec![
        Tool::Function {
            name: "get_weather".to_string(),
            description: None,
            parameters: serde_json::json!({ "type": "object" }),
            strict: Some(true),
        },
        Tool::ProviderDefined {
            id: "openai.web_search_preview".to_string(),
            name: "web_search_preview".to_string(),
            args: serde_json::json!({ "search_context_size": "high" }),
        },
    ];
    let round_tripped = round_trip_tools(&ApiProtocol::Responses, tools.clone());
    assert_eq!(round_tripped, tools);
}

// --- provider-defined tools: cross-protocol faithful passthrough ---

/// Documented cross-protocol behavior: a provider-defined tool routed to a
/// *different* provider's wire is **preserved verbatim** in its source-native
/// shape (so the upstream decides), never silently dropped and never lossily
/// "translated". Concretely, an Anthropic `web_search_20250305` rendered onto a
/// Responses (OpenAI) request still arrives as `{type:"web_search_20250305",
/// name:"web_search", …}` — and a Responses parser, treating any non-`function`
/// type as a server tool, recovers it with its original `anthropic.*` id intact.
#[test]
fn provider_defined_tool_cross_protocol_is_preserved_verbatim() {
    let anthropic_tool = Tool::ProviderDefined {
        id: "anthropic.web_search_20250305".to_string(),
        name: "web_search".to_string(),
        args: serde_json::json!({ "max_uses": 5 }),
    };

    // Render the Anthropic-native tool onto a Responses (OpenAI) request.
    let rendered = rendered_tools_json(&ApiProtocol::Responses, vec![anthropic_tool.clone()]);
    // Verbatim source-native shape: the versioned Anthropic `type` + `name` +
    // args are all present, unchanged — not mapped to `web_search_preview`.
    assert_eq!(rendered[0]["type"], "web_search_20250305");
    assert_eq!(rendered[0]["name"], "web_search");
    assert_eq!(rendered[0]["max_uses"], 5);

    // The Responses inbound parser recovers it as a provider-defined tool. The
    // `type` it sees is the Anthropic version, namespaced `openai.*` because the
    // wire it arrived on is OpenAI's — bitrouter cannot know the tool's true
    // origin from the wire alone, but it is faithfully forwarded, never dropped.
    let adapter = adapter_for(ApiProtocol::Responses);
    let reparsed = adapter
        .parse_request(
            adapter
                .render_request(&prompt_with_tools(vec![anthropic_tool]))
                .unwrap(),
        )
        .unwrap();
    assert_eq!(reparsed.tools.len(), 1, "tool is forwarded, not dropped");
    match &reparsed.tools[0] {
        Tool::ProviderDefined { id, name, args } => {
            assert_eq!(id, "openai.web_search_20250305");
            assert_eq!(name, "web_search");
            assert_eq!(args["max_uses"], 5);
        }
        other => panic!("expected a provider-defined tool, got {other:?}"),
    }
}

/// The same faithful-passthrough holds onto Anthropic's wire: an OpenAI
/// `web_search_preview` routed to a Messages request is preserved verbatim in
/// its **source-native (OpenAI)** shape — `{type:"web_search_preview", …args}`,
/// flat and with no `name` key, exactly as OpenAI serializes it — rather than
/// being dropped or lossily reshaped into Anthropic's `{type, name, …}` form.
/// bitrouter never invents a `name` the source did not carry; the upstream
/// decides what to do with the unfamiliar tool.
#[test]
fn provider_defined_tool_cross_protocol_onto_anthropic_is_preserved() {
    let openai_tool = Tool::ProviderDefined {
        id: "openai.web_search_preview".to_string(),
        name: "web_search_preview".to_string(),
        args: serde_json::json!({ "search_context_size": "high" }),
    };
    let rendered = rendered_tools_json(&ApiProtocol::Messages, vec![openai_tool]);
    assert_eq!(rendered[0]["type"], "web_search_preview");
    assert_eq!(rendered[0]["search_context_size"], "high");
    // Source-native (OpenAI) shape is flat: no Anthropic-style `name` is invented.
    assert!(
        rendered[0].get("name").is_none(),
        "must not fabricate an Anthropic `name` key: {rendered}"
    );
}

/// And onto Chat Completions (function-only on the wire): a provider-defined
/// tool is still preserved verbatim into the `tools` array rather than dropped.
#[test]
fn provider_defined_tool_preserved_into_chat_completions() {
    let tool = Tool::ProviderDefined {
        id: "openai.web_search_preview".to_string(),
        name: "web_search_preview".to_string(),
        args: serde_json::json!({ "search_context_size": "low" }),
    };
    let rendered = rendered_tools_json(&ApiProtocol::ChatCompletions, vec![tool]);
    assert_eq!(rendered[0]["type"], "web_search_preview");
    assert_eq!(rendered[0]["search_context_size"], "low");
}

/// The canonical `Tool` enum uses an internal `type` tag (`function` /
/// `provider_defined`) — a compact, self-describing IR serialization.
#[test]
fn tool_serde_uses_type_tag() {
    let function = serde_json::to_value(Tool::Function {
        name: "f".to_string(),
        description: None,
        parameters: serde_json::json!({}),
        strict: None,
    })
    .unwrap();
    assert_eq!(function["type"], "function");
    // Absent optionals are omitted (no JSON null).
    assert!(function.get("description").is_none());
    assert!(function.get("strict").is_none());

    let provider = serde_json::to_value(Tool::ProviderDefined {
        id: "openai.web_search_preview".to_string(),
        name: "web_search_preview".to_string(),
        args: serde_json::json!({}),
    })
    .unwrap();
    assert_eq!(provider["type"], "provider_defined");
    assert_eq!(provider["id"], "openai.web_search_preview");
}
