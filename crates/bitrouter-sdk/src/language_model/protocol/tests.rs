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
        tools: vec![Tool {
            name: "calculator".to_string(),
            description: Some("does math".to_string()),
            parameters: serde_json::json!({ "type": "object" }),
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
    // tool_result with array content is read as text
    let tr = prompt
        .messages
        .iter()
        .flat_map(|m| &m.content)
        .find_map(|c| match c {
            Content::ToolResult { content, .. } => Some(content.as_str()),
            _ => None,
        });
    assert_eq!(
        tr,
        Some("42"),
        "array tool_result content flattened to text"
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
