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

/// A minimal valid request body for `protocol`, carrying a single user turn —
/// enough for `parse_request` to succeed so a test can layer one extra field on.
fn minimal_request(protocol: ApiProtocol) -> serde_json::Value {
    match protocol {
        ApiProtocol::Messages => serde_json::json!({
            "model": "m", "max_tokens": 1024,
            "messages": [{ "role": "user", "content": "hi" }],
        }),
        ApiProtocol::ChatCompletions => serde_json::json!({
            "model": "m", "messages": [{ "role": "user", "content": "hi" }],
        }),
        ApiProtocol::Responses => serde_json::json!({ "model": "m", "input": "hi" }),
        ApiProtocol::GenerateContent => serde_json::json!({
            "model": "m", "contents": [{ "role": "user", "parts": [{ "text": "hi" }] }],
        }),
        ApiProtocol::Custom(_) => unreachable!("test helper only handles built-in protocols"),
    }
}

/// A canonical prompt exercising system + a user message + a tool definition.
fn sample_prompt() -> Prompt {
    Prompt {
        model: "test-model".to_string(),
        system: Some("be brief".to_string()),
        system_provider_metadata: Default::default(),
        messages: vec![Message::text(Role::User, "what is 2+2?")],
        tools: vec![Tool::Function {
            name: "calculator".to_string(),
            description: Some("does math".to_string()),
            parameters: serde_json::json!({ "type": "object" }),
            strict: None,
            provider_metadata: Default::default(),
        }],
        params: GenerationParams {
            temperature: Some(0.5),
            max_tokens: Some(256),
            ..Default::default()
        },
        response_format: None,
        tool_choice: None,
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
                provider_metadata: Default::default(),
            },
            Content::Text {
                text: "the answer is 4".to_string(),
                provider_metadata: Default::default(),
            },
            Content::ToolCall {
                id: "call_1".to_string(),
                name: "calculator".to_string(),
                arguments: "{\"op\":\"add\"}".to_string(),
                provider_executed: false,
                dynamic: false,
                provider_metadata: Default::default(),
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
        provider_metadata: Default::default(),
    }
}

fn text_of(content: &[Content]) -> String {
    content
        .iter()
        .filter_map(|c| match c {
            Content::Text { text, .. } => Some(text.as_str()),
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
            description: None,
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
                "description": "Structured weather report",
                "strict": true,
                "schema": {"type": "object", "properties": {"x": {"type": "string"}}}
            }
        }
    });
    let prompt = adapter.parse_request(body).unwrap();
    match prompt.response_format {
        Some(ResponseFormat::JsonSchema {
            name,
            description,
            strict,
            schema,
        }) => {
            assert_eq!(name.as_deref(), Some("weather"));
            // The OpenAI `json_schema.description` is promoted, not dropped.
            assert_eq!(description.as_deref(), Some("Structured weather report"));
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
            description: None,
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
    // description is likewise omitted, not emitted as null, when unset.
    assert!(
        rendered["response_format"]["json_schema"]
            .get("description")
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
        system_provider_metadata: Default::default(),
        messages: vec![Message::text(Role::User, "hi")],
        tools: vec![],
        params: GenerationParams {
            reasoning_effort: Some("high".to_string()),
            ..Default::default()
        },
        response_format: None,
        tool_choice: None,
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
        system_provider_metadata: Default::default(),
        messages: vec![Message::text(Role::User, "hi")],
        tools: vec![],
        params: GenerationParams {
            reasoning_effort: Some("xhigh".to_string()),
            ..Default::default()
        },
        response_format: Some(ResponseFormat::JsonSchema {
            name: None,
            description: None,
            strict: None,
            schema: serde_json::json!({"type": "object"}),
        }),
        tool_choice: None,
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
        system_provider_metadata: Default::default(),
        messages: vec![
            Message::text(Role::User, "hi"),
            Message::text(Role::System, "switch to terse mode"),
        ],
        tools: vec![],
        params: GenerationParams::default(),
        response_format: None,
        tool_choice: None,
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
        provider_metadata: Default::default(),
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
fn messages_stream_web_search_count_from_delta() {
    // `message_delta.usage` carries cumulative final counts, so
    // `web_search_count` must be ASSIGNED from the delta value, not
    // accumulated. This test also guards against double-counting: the
    // `message_start` carries no `server_tool_use`, so if the decoder
    // were to add rather than assign, `start(0) + delta(5) == 5` and
    // `start(0) + delta(5) + delta(5) == 10` would both pass — but since
    // only one `message_delta` is emitted per stream, the regression is
    // caught by the equality check below.
    let decoder = adapter_for(ApiProtocol::Messages);
    let mut stream_decoder = decoder.stream_decoder();

    let start = SseEvent {
        event: Some("message_start".into()),
        data: serde_json::json!({
            "type": "message_start",
            "message": {
                "id": "msg_2",
                "type": "message",
                "role": "assistant",
                "model": "claude-sonnet-4",
                "content": [],
                "stop_reason": null,
                "usage": { "input_tokens": 10, "output_tokens": 0 }
            }
        })
        .to_string(),
    };
    let _ = stream_decoder.decode(&start).unwrap();

    let delta = SseEvent {
        event: Some("message_delta".into()),
        data: serde_json::json!({
            "type": "message_delta",
            "delta": { "stop_reason": "end_turn" },
            "usage": {
                "output_tokens": 15,
                "server_tool_use": { "web_search_requests": 5 }
            }
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
    assert_eq!(usage.web_search_count, 5);
    assert_eq!(usage.completion_tokens, 15);
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
        Content::Text { text, .. } => text == "I cannot help.",
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
        matches!(result.content.first(), Some(Content::Reasoning { text, .. }) if text == "step by step")
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
        matches!(result.content.first(), Some(Content::Reasoning { text, .. }) if text == "internal monologue")
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

// ===== block-lifecycle markers (StreamPart::TextStart/TextEnd/ReasoningStart/
// ReasoningEnd): the merged-block fix. =====

/// Encode a canonical part stream through `protocol` and return every emitted
/// SSE event as `(event_name, data_json)`. Shared by the block-marker tests.
fn encode_stream_events(
    protocol: ApiProtocol,
    parts: &[StreamPart],
) -> Vec<(String, serde_json::Value)> {
    let adapter = adapter_for(protocol);
    let mut encoder = adapter.stream_encoder("resp_m", "test-model");
    let mut out = Vec::new();
    for part in parts {
        for frame in encoder.encode(part).unwrap() {
            if let SseFrame::Event { event, data } = frame {
                let json: serde_json::Value = serde_json::from_str(&data).unwrap();
                out.push((event.unwrap_or_default(), json));
            }
        }
    }
    for frame in encoder.finish().unwrap() {
        if let SseFrame::Event { event, data } = frame {
            let json: serde_json::Value =
                serde_json::from_str(&data).unwrap_or(serde_json::json!({}));
            out.push((event.unwrap_or_default(), json));
        }
    }
    out
}

/// Decode a sequence of SSE `(event_name, data_json)` through `protocol` into
/// the canonical part stream. Shared by the round-trip tests.
fn decode_stream(protocol: ApiProtocol, events: &[(&str, serde_json::Value)]) -> Vec<StreamPart> {
    let adapter = adapter_for(protocol);
    let mut decoder = adapter.stream_decoder();
    let mut parts = Vec::new();
    for (event, data) in events {
        parts.extend(
            decoder
                .decode(&SseEvent {
                    event: Some((*event).to_string()),
                    data: data.to_string(),
                })
                .unwrap(),
        );
    }
    parts.extend(decoder.finish().unwrap());
    parts
}

/// A canonical stream of two *distinct* text blocks, each bracketed by its own
/// start/end marker (what a block-framed upstream decoder now produces). The
/// merged-block bug was that, without the markers, this collapsed to a flat
/// `TextDelta,TextDelta` run that re-encoded into ONE block.
fn two_text_blocks() -> Vec<StreamPart> {
    vec![
        StreamPart::TextStart { id: "0".into() },
        StreamPart::TextDelta {
            text: "first".into(),
        },
        StreamPart::TextEnd { id: "0".into() },
        StreamPart::TextStart { id: "1".into() },
        StreamPart::TextDelta {
            text: "second".into(),
        },
        StreamPart::TextEnd { id: "1".into() },
        StreamPart::Finish {
            reason: FinishReason::Stop,
        },
    ]
}

#[test]
fn anthropic_two_text_blocks_reencode_as_distinct_blocks_not_merged() {
    // The merged-block fix on the Anthropic wire: two bracketed text blocks must
    // re-encode as TWO `content_block_start`(type=text) + TWO
    // `content_block_stop`, never a single merged block. Without the lifecycle
    // markers both deltas land in one block.
    let events = encode_stream_events(ApiProtocol::Messages, &two_text_blocks());
    let text_starts = events
        .iter()
        .filter(|(name, data)| {
            name == "content_block_start"
                && data.pointer("/content_block/type").and_then(|t| t.as_str()) == Some("text")
        })
        .count();
    let stops = events
        .iter()
        .filter(|(name, _)| name == "content_block_stop")
        .count();
    assert_eq!(
        text_starts, 2,
        "two distinct text blocks must open two text `content_block_start`s, not merge: {events:?}"
    );
    assert_eq!(stops, 2, "each opened block must close: {events:?}");
    // The two blocks must carry distinct indices (not the same index reused).
    let indices: Vec<u64> = events
        .iter()
        .filter(|(name, data)| {
            name == "content_block_start"
                && data.pointer("/content_block/type").and_then(|t| t.as_str()) == Some("text")
        })
        .filter_map(|(_, data)| data.get("index").and_then(|i| i.as_u64()))
        .collect();
    assert_eq!(indices.len(), 2);
    assert_ne!(indices[0], indices[1], "blocks must use distinct indices");
}

#[test]
fn responses_two_text_blocks_reencode_as_distinct_items_not_merged() {
    // The merged-block fix on the Responses wire: two bracketed text blocks must
    // re-encode as TWO `output_item.added`(type=message) + TWO
    // `output_item.done`, each in its own `output_index` — never one merged
    // message item (which would render the two upstream blocks as one).
    let events = encode_stream_events(ApiProtocol::Responses, &two_text_blocks());
    let message_added = events
        .iter()
        .filter(|(name, data)| {
            name == "response.output_item.added"
                && data.pointer("/item/type").and_then(|t| t.as_str()) == Some("message")
        })
        .count();
    let message_done = events
        .iter()
        .filter(|(name, data)| {
            name == "response.output_item.done"
                && data.pointer("/item/type").and_then(|t| t.as_str()) == Some("message")
        })
        .count();
    assert_eq!(
        message_added, 2,
        "two distinct text blocks must open two message items, not merge: {events:?}"
    );
    assert_eq!(message_done, 2, "each message item must close: {events:?}");
    // Distinct `output_index` per item.
    let indices: Vec<u64> = events
        .iter()
        .filter(|(name, data)| {
            name == "response.output_item.added"
                && data.pointer("/item/type").and_then(|t| t.as_str()) == Some("message")
        })
        .filter_map(|(_, data)| data.get("output_index").and_then(|i| i.as_u64()))
        .collect();
    assert_eq!(indices.len(), 2);
    assert_ne!(
        indices[0], indices[1],
        "message items must use distinct output indices"
    );
    // The terminal `response.completed.output` must mirror both items.
    let completed_output_len = events
        .iter()
        .rev()
        .find(|(name, _)| name == "response.completed")
        .and_then(|(_, data)| data.pointer("/response/output"))
        .and_then(|o| o.as_array())
        .map(|a| a.len())
        .unwrap_or(0);
    assert_eq!(
        completed_output_len, 2,
        "terminal envelope must replay both message items: {events:?}"
    );
}

#[test]
fn text_reasoning_text_reencodes_with_three_distinct_blocks() {
    // text → reasoning → text, each bracketed, must re-encode as three distinct
    // blocks with the right kinds on both block-framed wires — the canonical
    // ordering (#416/#454-1) plus the merged-block fix together.
    let parts = vec![
        StreamPart::TextStart { id: "a".into() },
        StreamPart::TextDelta {
            text: "intro".into(),
        },
        StreamPart::TextEnd { id: "a".into() },
        StreamPart::ReasoningStart { id: "b".into() },
        StreamPart::ReasoningDelta {
            text: "ponder".into(),
        },
        StreamPart::ReasoningEnd { id: "b".into() },
        StreamPart::TextStart { id: "c".into() },
        StreamPart::TextDelta {
            text: "outro".into(),
        },
        StreamPart::TextEnd { id: "c".into() },
        StreamPart::Finish {
            reason: FinishReason::Stop,
        },
    ];

    // Anthropic: 2 text blocks + 1 thinking block, 3 stops.
    let anthropic = encode_stream_events(ApiProtocol::Messages, &parts);
    let count_kind = |kind: &str| {
        anthropic
            .iter()
            .filter(|(name, data)| {
                name == "content_block_start"
                    && data.pointer("/content_block/type").and_then(|t| t.as_str()) == Some(kind)
            })
            .count()
    };
    assert_eq!(count_kind("text"), 2, "two text blocks: {anthropic:?}");
    assert_eq!(
        count_kind("thinking"),
        1,
        "one thinking block: {anthropic:?}"
    );
    assert_eq!(
        anthropic
            .iter()
            .filter(|(name, _)| name == "content_block_stop")
            .count(),
        3,
        "three block stops: {anthropic:?}"
    );

    // Responses: 2 message items + 1 reasoning item.
    let responses = encode_stream_events(ApiProtocol::Responses, &parts);
    let count_item = |kind: &str| {
        responses
            .iter()
            .filter(|(name, data)| {
                name == "response.output_item.added"
                    && data.pointer("/item/type").and_then(|t| t.as_str()) == Some(kind)
            })
            .count()
    };
    assert_eq!(count_item("message"), 2, "two message items: {responses:?}");
    assert_eq!(
        count_item("reasoning"),
        1,
        "one reasoning item: {responses:?}"
    );
}

#[test]
fn coarse_wires_drop_block_markers_and_emit_only_deltas() {
    // Chat Completions and Generate Content frame no content blocks, so the
    // block-lifecycle markers re-encode to NOTHING — a bare start/end emits zero
    // SSE frames — while the text deltas pass through unaffected. This proves the
    // coarse-vs-framed split: a block-framed upstream re-encoded to a coarse
    // client simply drops the frames.
    for protocol in [ApiProtocol::ChatCompletions, ApiProtocol::GenerateContent] {
        let adapter = adapter_for(protocol.clone());
        let mut encoder = adapter.stream_encoder("resp_c", "test-model");

        // Each marker alone yields no frames.
        for marker in [
            StreamPart::TextStart { id: "0".into() },
            StreamPart::TextEnd { id: "0".into() },
            StreamPart::ReasoningStart { id: "1".into() },
            StreamPart::ReasoningEnd { id: "1".into() },
        ] {
            assert!(
                encoder.encode(&marker).unwrap().is_empty(),
                "{protocol:?}: marker {marker:?} must emit no frames"
            );
        }

        // The full two-block stream decodes back to exactly the two deltas,
        // concatenated — the markers left no trace.
        let events = encode_stream_events(protocol.clone(), &two_text_blocks());
        let mut decoder = adapter.stream_decoder();
        let mut decoded = Vec::new();
        for (event, data) in &events {
            decoded.extend(
                decoder
                    .decode(&SseEvent {
                        event: Some(event.clone()),
                        data: data.to_string(),
                    })
                    .unwrap(),
            );
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
            text, "firstsecond",
            "{protocol:?}: coarse text deltas survive unaffected"
        );
        assert!(
            !decoded.iter().any(|p| matches!(
                p,
                StreamPart::TextStart { .. }
                    | StreamPart::TextEnd { .. }
                    | StreamPart::ReasoningStart { .. }
                    | StreamPart::ReasoningEnd { .. }
            )),
            "{protocol:?}: coarse wire must not produce block markers: {decoded:?}"
        );
    }
}

#[test]
fn block_markers_survive_anthropic_decode_then_reencode_roundtrip() {
    // Full same-protocol round trip on Anthropic: real two-block upstream SSE
    // decodes to a marker-bracketed canonical stream, which re-encodes back into
    // two distinct content blocks. This is the end-to-end merged-block fix.
    let upstream = [
        (
            "message_start",
            serde_json::json!({ "type": "message_start", "message": { "id": "msg_1", "usage": { "input_tokens": 3 } } }),
        ),
        (
            "content_block_start",
            serde_json::json!({ "type": "content_block_start", "index": 0, "content_block": { "type": "text", "text": "" } }),
        ),
        (
            "content_block_delta",
            serde_json::json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "text_delta", "text": "first" } }),
        ),
        (
            "content_block_stop",
            serde_json::json!({ "type": "content_block_stop", "index": 0 }),
        ),
        (
            "content_block_start",
            serde_json::json!({ "type": "content_block_start", "index": 1, "content_block": { "type": "text", "text": "" } }),
        ),
        (
            "content_block_delta",
            serde_json::json!({ "type": "content_block_delta", "index": 1, "delta": { "type": "text_delta", "text": "second" } }),
        ),
        (
            "content_block_stop",
            serde_json::json!({ "type": "content_block_stop", "index": 1 }),
        ),
        (
            "message_delta",
            serde_json::json!({ "type": "message_delta", "delta": { "stop_reason": "end_turn" }, "usage": { "output_tokens": 2 } }),
        ),
        (
            "message_stop",
            serde_json::json!({ "type": "message_stop" }),
        ),
    ];
    let decoded = decode_stream(ApiProtocol::Messages, &upstream);
    // The decoder must surface the two block boundaries as start/end markers.
    assert_eq!(
        decoded
            .iter()
            .filter(|p| matches!(p, StreamPart::TextStart { .. }))
            .count(),
        2,
        "decode must emit two TextStart markers: {decoded:?}"
    );
    assert_eq!(
        decoded
            .iter()
            .filter(|p| matches!(p, StreamPart::TextEnd { .. }))
            .count(),
        2,
        "decode must emit two TextEnd markers: {decoded:?}"
    );

    // Re-encode and assert the two distinct blocks survive.
    let reencoded = encode_stream_events(ApiProtocol::Messages, &decoded);
    let text_starts = reencoded
        .iter()
        .filter(|(name, data)| {
            name == "content_block_start"
                && data.pointer("/content_block/type").and_then(|t| t.as_str()) == Some("text")
        })
        .count();
    assert_eq!(
        text_starts, 2,
        "round trip must preserve two distinct text blocks: {reencoded:?}"
    );
}

#[test]
fn block_markers_survive_responses_decode_then_reencode_roundtrip() {
    // Full same-protocol round trip on Responses: two real `message` items
    // decode to marker-bracketed canonical parts and re-encode into two distinct
    // message items.
    let upstream = [
        (
            "response.created",
            serde_json::json!({ "type": "response.created", "response": { "id": "resp_1" } }),
        ),
        (
            "response.output_item.added",
            serde_json::json!({ "type": "response.output_item.added", "output_index": 0, "item": { "type": "message", "id": "msg_a", "role": "assistant" } }),
        ),
        (
            "response.output_text.delta",
            serde_json::json!({ "type": "response.output_text.delta", "item_id": "msg_a", "output_index": 0, "delta": "first" }),
        ),
        (
            "response.output_item.done",
            serde_json::json!({ "type": "response.output_item.done", "output_index": 0, "item": { "type": "message", "id": "msg_a", "role": "assistant" } }),
        ),
        (
            "response.output_item.added",
            serde_json::json!({ "type": "response.output_item.added", "output_index": 1, "item": { "type": "message", "id": "msg_b", "role": "assistant" } }),
        ),
        (
            "response.output_text.delta",
            serde_json::json!({ "type": "response.output_text.delta", "item_id": "msg_b", "output_index": 1, "delta": "second" }),
        ),
        (
            "response.output_item.done",
            serde_json::json!({ "type": "response.output_item.done", "output_index": 1, "item": { "type": "message", "id": "msg_b", "role": "assistant" } }),
        ),
        (
            "response.completed",
            serde_json::json!({ "type": "response.completed", "response": { "id": "resp_1", "status": "completed" } }),
        ),
    ];
    let decoded = decode_stream(ApiProtocol::Responses, &upstream);
    assert_eq!(
        decoded
            .iter()
            .filter(|p| matches!(p, StreamPart::TextStart { .. }))
            .count(),
        2,
        "decode must emit two TextStart markers: {decoded:?}"
    );
    assert_eq!(
        decoded
            .iter()
            .filter(|p| matches!(p, StreamPart::TextEnd { .. }))
            .count(),
        2,
        "decode must emit two TextEnd markers: {decoded:?}"
    );

    let reencoded = encode_stream_events(ApiProtocol::Responses, &decoded);
    let message_added = reencoded
        .iter()
        .filter(|(name, data)| {
            name == "response.output_item.added"
                && data.pointer("/item/type").and_then(|t| t.as_str()) == Some("message")
        })
        .count();
    assert_eq!(
        message_added, 2,
        "round trip must preserve two distinct message items: {reencoded:?}"
    );
}

#[test]
fn tool_call_streaming_still_frames_distinctly_with_markers_present() {
    // Regression guard: the markers must not disturb tool-call framing. A text
    // block followed by two tool calls must still produce, on Responses, one
    // message item + two distinct `function_call` items (each tool call in its
    // own output slot) — tool blocks carry no start/end marker by design.
    let parts = vec![
        StreamPart::TextStart { id: "0".into() },
        StreamPart::TextDelta {
            text: "calling".into(),
        },
        StreamPart::TextEnd { id: "0".into() },
        StreamPart::ToolCallDelta {
            id: "call_a".into(),
            name: Some("first".into()),
            arguments: "{}".into(),
        },
        StreamPart::ToolCallDelta {
            id: "call_b".into(),
            name: Some("second".into()),
            arguments: "{}".into(),
        },
        StreamPart::Finish {
            reason: FinishReason::Stop,
        },
    ];
    let events = encode_stream_events(ApiProtocol::Responses, &parts);
    let function_added = events
        .iter()
        .filter(|(name, data)| {
            name == "response.output_item.added"
                && data.pointer("/item/type").and_then(|t| t.as_str()) == Some("function_call")
        })
        .count();
    assert_eq!(
        function_added, 2,
        "two tool calls must open two function_call items: {events:?}"
    );
    // And exactly one message item from the single text block.
    let message_added = events
        .iter()
        .filter(|(name, data)| {
            name == "response.output_item.added"
                && data.pointer("/item/type").and_then(|t| t.as_str()) == Some("message")
        })
        .count();
    assert_eq!(
        message_added, 1,
        "one text block → one message item: {events:?}"
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
            provider_metadata: Default::default(),
        }],
        usage: None,
        finish_reason: Some(FinishReason::Stop),
        response_id: None,
        stop_details: None,
        provider_metadata: Default::default(),
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
            provider_metadata: Default::default(),
        }],
        usage: None,
        finish_reason: None,
        response_id: None,
        stop_details: None,
        provider_metadata: Default::default(),
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
        provider_metadata: Default::default(),
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
            provider_metadata: Default::default(),
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
        provider_metadata: Default::default(),
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
    // Compare *semantically* by parsing both sides to `serde_json::Value`
    // rather than as strings. JSON object key order is not part of the schema
    // contract, and it is sensitive to whether `serde_json`'s `preserve_order`
    // feature is unified into this build — any workspace dependency can toggle
    // that (e.g. `bitrouter-attestation`'s `dcap-qvl` does, which reorders the
    // properties schemars emits). `Value` equality is order-insensitive for
    // object keys while staying order-sensitive for arrays (`required`,
    // `oneOf`, …), which *are* meaningful. Parsing also subsumes the old
    // CRLF/whitespace normalisation. Stored snapshots stay human-readable.
    let expected_json: serde_json::Value = serde_json::from_str(&expected).unwrap_or_else(|e| {
        panic!(
            "snapshot {} is not valid JSON ({e}); re-run with BITROUTER_BLESS=1 to recreate.",
            path.display()
        )
    });
    let actual_json: serde_json::Value = serde_json::from_str(&actual).unwrap();
    assert_eq!(
        expected_json, actual_json,
        "schema snapshot for `{name}` drifted; re-run with BITROUTER_BLESS=1 to update.\n\
         actual schema:\n{actual}"
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
        system_provider_metadata: Default::default(),
        messages: vec![Message {
            role: Role::User,
            content: vec![Content::File {
                media_type: "image/png".to_string(),
                data: DataContent::Base64 {
                    data: IMG_B64.to_string(),
                },
                filename: None,
                provider_metadata: Default::default(),
            }],
        }],
        tools: vec![],
        params: GenerationParams {
            max_tokens: Some(256),
            ..Default::default()
        },
        response_format: None,
        tool_choice: None,
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
            provider_metadata: Default::default(),
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
            provider_metadata: Default::default(),
        }],
        usage: None,
        finish_reason: Some(FinishReason::Stop),
        response_id: None,
        stop_details: None,
        provider_metadata: Default::default(),
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
        system_provider_metadata: Default::default(),
        messages: vec![Message {
            role: Role::Tool,
            content: vec![Content::ToolResult {
                call_id: call_id.to_string(),
                tool_name: tool_name.map(str::to_string),
                output,
                dynamic: false,
                provider_metadata: Default::default(),
            }],
        }],
        tools: vec![],
        params: GenerationParams {
            max_tokens: Some(256),
            ..Default::default()
        },
        response_format: None,
        tool_choice: None,
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
                ..
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
    // <https://github.com/vercel/ai/blob/8e650ab809ac47de5d16f26bf544a9a73b0d39a3/packages/openai/src/responses/convert-to-openai-responses-input.ts>
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

/// #547 — an Anthropic Messages client (Claude Code) sends `tool_choice` in
/// Anthropic's object shape. Routed to an OpenAI Chat Completions upstream it
/// MUST become OpenAI's native shape (the bare string `"auto"`, not the object
/// `{"type":"auto"}`, which OpenAI rejects), not pass through verbatim. Before
/// the fix `tool_choice` rode `extra` opaquely and broke tool use on
/// non-Anthropic models while plain-text generation still worked.
#[test]
fn regression_547_messages_tool_choice_translates_to_chat() {
    let inbound = adapter_for(ApiProtocol::Messages);
    let outbound = adapter_for(ApiProtocol::ChatCompletions);

    // auto / any / none map to the bare strings; a forced tool maps to the
    // nested `{type:function, function:{name}}` object.
    let cases = [
        (
            serde_json::json!({ "type": "auto" }),
            serde_json::json!("auto"),
        ),
        (
            serde_json::json!({ "type": "any" }),
            serde_json::json!("required"),
        ),
        (
            serde_json::json!({ "type": "none" }),
            serde_json::json!("none"),
        ),
        (
            serde_json::json!({ "type": "tool", "name": "Edit" }),
            serde_json::json!({ "type": "function", "function": { "name": "Edit" } }),
        ),
    ];

    for (anthropic, expected_openai) in cases {
        let request = serde_json::json!({
            "model": "deepseek/deepseek-v4-pro",
            "max_tokens": 1024,
            "messages": [{ "role": "user", "content": "edit the file" }],
            "tools": [{ "name": "Edit", "input_schema": { "type": "object" } }],
            "tool_choice": anthropic,
        });
        let prompt = inbound
            .parse_request(request)
            .expect("parse messages request");
        let rendered = outbound
            .render_request(&prompt)
            .expect("render chat request");
        assert_eq!(
            rendered["tool_choice"], expected_openai,
            "Anthropic {anthropic} must render as OpenAI {expected_openai}"
        );
        // The translated value must not leak the inbound object shape.
        assert_ne!(rendered["tool_choice"], anthropic);
    }
}

/// `tool_choice` is canonical, so it round-trips across every inbound→outbound
/// pair in the 4×4 matrix. Drives each native shape and asserts the canonical
/// promotion + re-rendering for a forced-tool choice (the variant that differs
/// most between protocols).
#[test]
fn tool_choice_round_trips_across_protocol_matrix() {
    // Native `tool_choice` (or, for Google, `toolConfig`) that forces tool "X".
    fn force_tool_field(p: ApiProtocol) -> (&'static str, serde_json::Value) {
        match p {
            ApiProtocol::Messages => (
                "tool_choice",
                serde_json::json!({ "type": "tool", "name": "X" }),
            ),
            ApiProtocol::ChatCompletions => (
                "tool_choice",
                serde_json::json!({ "type": "function", "function": { "name": "X" } }),
            ),
            ApiProtocol::Responses => (
                "tool_choice",
                serde_json::json!({ "type": "function", "name": "X" }),
            ),
            ApiProtocol::GenerateContent => (
                "toolConfig",
                serde_json::json!({
                    "functionCallingConfig": { "mode": "ANY", "allowedFunctionNames": ["X"] }
                }),
            ),
            ApiProtocol::Custom(_) => unreachable!(),
        }
    }

    // Did the outbound request force exactly tool "X"?
    fn forces_tool_x(p: ApiProtocol, req: &serde_json::Value) -> bool {
        match p {
            ApiProtocol::Messages => {
                req["tool_choice"] == serde_json::json!({ "type": "tool", "name": "X" })
            }
            ApiProtocol::ChatCompletions => {
                req["tool_choice"]["type"] == "function"
                    && req["tool_choice"]["function"]["name"] == "X"
            }
            ApiProtocol::Responses => {
                req["tool_choice"]["type"] == "function" && req["tool_choice"]["name"] == "X"
            }
            ApiProtocol::GenerateContent => {
                let fcc = &req["toolConfig"]["functionCallingConfig"];
                fcc["mode"] == "ANY" && fcc["allowedFunctionNames"][0] == "X"
            }
            ApiProtocol::Custom(_) => unreachable!(),
        }
    }

    for from in all_protocols() {
        for to in all_protocols() {
            let inbound = adapter_for(from.clone());
            let outbound = adapter_for(to.clone());

            let mut body = minimal_request(from.clone());
            let (field, value) = force_tool_field(from.clone());
            body[field] = value;

            let prompt = inbound
                .parse_request(body)
                .unwrap_or_else(|e| panic!("{from:?} parse_request: {e}"));
            assert_eq!(
                prompt.tool_choice,
                Some(ToolChoice::Tool { name: "X".into() }),
                "{from:?} must promote a forced-tool choice into the canonical slot"
            );

            let rendered = outbound
                .render_request(&prompt)
                .unwrap_or_else(|e| panic!("{to:?} render_request: {e}"));
            assert!(
                forces_tool_x(to.clone(), &rendered),
                "{from:?} → {to:?} must force tool X; got {rendered}"
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
    // <https://github.com/vercel/ai/blob/8e650ab809ac47de5d16f26bf544a9a73b0d39a3/packages/openai/src/responses/convert-to-openai-responses-input.ts>
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
    // <https://github.com/vercel/ai/blob/8e650ab809ac47de5d16f26bf544a9a73b0d39a3/packages/openai/src/responses/convert-to-openai-responses-input.ts>
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
    // <https://github.com/vercel/ai/blob/8e650ab809ac47de5d16f26bf544a9a73b0d39a3/packages/openai/src/responses/convert-to-openai-responses-input.ts>
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

    let denied = serde_json::to_value(ToolResultOutput::ExecutionDenied {
        reason: Some("nope".to_string()),
    })
    .unwrap();
    assert_eq!(denied["type"], "execution_denied");
    assert_eq!(denied["reason"], "nope");
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
        dynamic: false,
        provider_metadata: Default::default(),
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
        dynamic: false,
        provider_metadata: Default::default(),
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
                dynamic: false,
                provider_metadata: Default::default(),
            },
            Content::ToolCall {
                id: "srvtoolu_1".to_string(),
                name: "web_search".to_string(),
                arguments: "{\"query\":\"x\"}".to_string(),
                provider_executed: true,
                dynamic: false,
                provider_metadata: Default::default(),
            },
        ],
        usage: None,
        finish_reason: Some(FinishReason::Stop),
        response_id: None,
        stop_details: None,
        provider_metadata: Default::default(),
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

// ===== A2: Anthropic MCP (`mcp_tool_use` / `mcp_tool_result`) =====

/// An Anthropic `mcp_tool_use` block parses to a `dynamic`, provider-executed
/// `ToolCall` whose `server_name` is preserved in `provider_metadata`.
#[test]
fn messages_parses_mcp_tool_use_as_dynamic_with_server_name() {
    let adapter = adapter_for(ApiProtocol::Messages);
    let body = serde_json::json!({
        "id": "msg_1",
        "content": [
            {
                "type": "mcp_tool_use",
                "id": "mcptoolu_1",
                "name": "search_docs",
                "input": { "query": "q" },
                "server_name": "docs-server"
            }
        ],
        "stop_reason": "end_turn"
    });
    let result = adapter.parse_response(body).unwrap();
    let call = result
        .content
        .iter()
        .find_map(|c| match c {
            Content::ToolCall {
                id,
                name,
                dynamic,
                provider_executed,
                provider_metadata,
                ..
            } => Some((id, name, *dynamic, *provider_executed, provider_metadata)),
            _ => None,
        })
        .expect("an mcp_tool_use call");
    assert_eq!(call.0, "mcptoolu_1");
    assert_eq!(call.1, "search_docs");
    assert!(call.2, "mcp_tool_use is dynamic");
    assert!(call.3, "mcp_tool_use is provider-executed");
    // The server identity rides the anthropic namespace.
    let anthropic = call.4.get("anthropic").and_then(|v| v.as_object()).unwrap();
    assert_eq!(anthropic["type"], "mcp-tool-use");
    assert_eq!(anthropic["serverName"], "docs-server");
}

/// A full Anthropic MCP round-trip: an `mcp_tool_use` call followed by its inline
/// `mcp_tool_result` parses to a `dynamic` `ToolCall` + `dynamic` `ToolResult`
/// and renders back to the SAME two blocks, preserving `server_name`, the tool
/// args, and the inline result content (the A2 fidelity goal).
#[test]
fn messages_mcp_tool_use_and_result_round_trip() {
    let adapter = adapter_for(ApiProtocol::Messages);
    let body = serde_json::json!({
        "id": "msg_1",
        "content": [
            {
                "type": "mcp_tool_use",
                "id": "mcptoolu_1",
                "name": "search_docs",
                "input": { "query": "rust" },
                "server_name": "docs-server"
            },
            {
                "type": "mcp_tool_result",
                "tool_use_id": "mcptoolu_1",
                "is_error": false,
                "content": [ { "type": "text", "text": "found 3 docs" } ]
            }
        ],
        "stop_reason": "end_turn"
    });
    let parsed = adapter.parse_response(body).unwrap();
    // Canonical IR: a dynamic call + a dynamic result.
    assert!(matches!(
        parsed.content.first(),
        Some(Content::ToolCall { dynamic: true, .. })
    ));
    assert!(matches!(
        parsed.content.get(1),
        Some(Content::ToolResult { dynamic: true, .. })
    ));
    let rendered = adapter
        .render_response(&parsed, &sample_prompt(), "msg_1")
        .unwrap();
    let blocks = rendered["content"].as_array().unwrap();
    // The MCP call block round-trips with its server name and args.
    assert_eq!(blocks[0]["type"], "mcp_tool_use");
    assert_eq!(blocks[0]["id"], "mcptoolu_1");
    assert_eq!(blocks[0]["name"], "search_docs");
    assert_eq!(blocks[0]["server_name"], "docs-server");
    assert_eq!(blocks[0]["input"]["query"], "rust");
    // The inline result block round-trips with its content and pairing id.
    assert_eq!(blocks[1]["type"], "mcp_tool_result");
    assert_eq!(blocks[1]["tool_use_id"], "mcptoolu_1");
    assert_eq!(blocks[1]["is_error"], false);
    assert_eq!(
        blocks[1]["content"],
        serde_json::json!([{ "type": "text", "text": "found 3 docs" }])
    );
}

/// An `mcp_tool_result` carrying `is_error: true` round-trips as an error result.
#[test]
fn messages_mcp_tool_result_error_round_trips() {
    let adapter = adapter_for(ApiProtocol::Messages);
    let body = serde_json::json!({
        "id": "msg_1",
        "content": [
            {
                "type": "mcp_tool_use",
                "id": "mcptoolu_1",
                "name": "do_thing",
                "input": {},
                "server_name": "srv"
            },
            {
                "type": "mcp_tool_result",
                "tool_use_id": "mcptoolu_1",
                "is_error": true,
                "content": "boom"
            }
        ],
        "stop_reason": "end_turn"
    });
    let parsed = adapter.parse_response(body).unwrap();
    // The result is an error variant in the canonical IR.
    let is_error = parsed.content.iter().find_map(|c| match c {
        Content::ToolResult { output, .. } => Some(output.is_error()),
        _ => None,
    });
    assert_eq!(is_error, Some(true));
    let rendered = adapter
        .render_response(&parsed, &sample_prompt(), "msg_1")
        .unwrap();
    assert_eq!(rendered["content"][1]["type"], "mcp_tool_result");
    assert_eq!(rendered["content"][1]["is_error"], true);
    assert_eq!(rendered["content"][1]["content"], "boom");
}

/// An Anthropic **request** echoing an assistant turn with `mcp_tool_use` +
/// `mcp_tool_result` blocks round-trips: the inline MCP result stays in the
/// assistant turn (it is NOT split into a request-side `tool_result`) and
/// re-renders as an `mcp_tool_result` block.
#[test]
fn messages_request_mcp_blocks_round_trip_in_assistant_turn() {
    let adapter = adapter_for(ApiProtocol::Messages);
    let body = serde_json::json!({
        "model": "claude",
        "max_tokens": 100,
        "messages": [
            { "role": "user", "content": "search the docs" },
            {
                "role": "assistant",
                "content": [
                    {
                        "type": "mcp_tool_use",
                        "id": "mcptoolu_1",
                        "name": "search_docs",
                        "input": { "query": "rust" },
                        "server_name": "docs-server"
                    },
                    {
                        "type": "mcp_tool_result",
                        "tool_use_id": "mcptoolu_1",
                        "is_error": false,
                        "content": [ { "type": "text", "text": "ok" } ]
                    }
                ]
            }
        ]
    });
    let prompt = adapter.parse_request(body).unwrap();
    // The MCP result is NOT split into a separate Tool-role message — both blocks
    // ride the single assistant turn.
    let assistant = prompt
        .messages
        .iter()
        .find(|m| m.role == Role::Assistant)
        .expect("an assistant message");
    assert!(
        prompt.messages.iter().all(|m| m.role != Role::Tool),
        "a dynamic MCP result must not become a Tool-role message"
    );
    assert!(matches!(
        assistant.content.first(),
        Some(Content::ToolCall { dynamic: true, .. })
    ));
    assert!(matches!(
        assistant.content.get(1),
        Some(Content::ToolResult { dynamic: true, .. })
    ));
    // Re-render onto the Anthropic request wire: the assistant content reproduces
    // both MCP blocks.
    let rendered = adapter.render_request(&prompt).unwrap();
    let assistant_msg = rendered["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["role"] == "assistant")
        .expect("an assistant request message");
    let blocks = assistant_msg["content"].as_array().unwrap();
    assert_eq!(blocks[0]["type"], "mcp_tool_use");
    assert_eq!(blocks[0]["server_name"], "docs-server");
    assert_eq!(blocks[1]["type"], "mcp_tool_result");
    assert_eq!(blocks[1]["tool_use_id"], "mcptoolu_1");
    assert_eq!(
        blocks[1]["content"],
        serde_json::json!([{ "type": "text", "text": "ok" }])
    );
}

// ===== A1: Responses MCP (`mcp_call`) + `local_shell_call` =====

/// An OpenAI Responses `mcp_call` (with an inline `output`) parses to a
/// `dynamic` provider-executed `ToolCall` + a paired `dynamic` `ToolResult`, and
/// renders back to a SINGLE `mcp_call` item that preserves the inline result —
/// the A1 fidelity goal (the inline result was previously dropped).
#[test]
fn responses_mcp_call_round_trips_with_inline_result() {
    let adapter = adapter_for(ApiProtocol::Responses);
    let body = serde_json::json!({
        "id": "resp_1",
        "status": "completed",
        "output": [
            {
                "type": "mcp_call",
                "id": "mcp_1",
                "server_label": "my-mcp",
                "name": "lookup",
                "arguments": "{\"q\":\"x\"}",
                "output": "the answer"
            }
        ]
    });
    let parsed = adapter.parse_response(body).unwrap();
    // Lowered to a dynamic provider-executed call + its dynamic inline result.
    assert!(matches!(
        parsed.content.first(),
        Some(Content::ToolCall {
            dynamic: true,
            provider_executed: true,
            ..
        })
    ));
    assert!(matches!(
        parsed.content.get(1),
        Some(Content::ToolResult { dynamic: true, .. })
    ));
    let rendered = adapter
        .render_response(&parsed, &sample_prompt(), "resp_1")
        .unwrap();
    // Exactly ONE mcp_call item is re-emitted (the call + result recombined).
    let mcp_items: Vec<_> = rendered["output"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|i| i["type"] == "mcp_call")
        .collect();
    assert_eq!(
        mcp_items.len(),
        1,
        "call + result recombine into one mcp_call"
    );
    let item = mcp_items[0];
    assert_eq!(item["id"], "mcp_1");
    assert_eq!(item["server_label"], "my-mcp");
    assert_eq!(item["name"], "lookup");
    assert_eq!(item["arguments"], "{\"q\":\"x\"}");
    assert_eq!(item["output"], "the answer", "inline result preserved");
    // No stray function_call / function_call_output leaked from the split pair.
    assert!(
        rendered["output"]
            .as_array()
            .unwrap()
            .iter()
            .all(|i| i["type"] != "function_call" && i["type"] != "function_call_output"),
        "the dynamic call/result must not leak as plain function items"
    );
}

/// An `mcp_call` carrying an inline `error` (instead of `output`) round-trips the
/// error faithfully on the single recombined `mcp_call` item.
#[test]
fn responses_mcp_call_round_trips_inline_error() {
    let adapter = adapter_for(ApiProtocol::Responses);
    let body = serde_json::json!({
        "id": "resp_1",
        "status": "completed",
        "output": [
            {
                "type": "mcp_call",
                "id": "mcp_1",
                "server_label": "my-mcp",
                "name": "lookup",
                "arguments": "{}",
                "error": "upstream exploded"
            }
        ]
    });
    let parsed = adapter.parse_response(body).unwrap();
    let rendered = adapter
        .render_response(&parsed, &sample_prompt(), "resp_1")
        .unwrap();
    let item = rendered["output"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["type"] == "mcp_call")
        .unwrap();
    assert_eq!(item["error"], "upstream exploded");
    assert!(
        item.get("output").is_none(),
        "no output key when only error"
    );
}

/// A `dynamic` provider-executed MCP `ToolCall` that reaches the Responses render
/// WITHOUT its paired inline `ToolResult` (e.g. an upstream truncated the response
/// after the call but before its inline result, then routed to a Responses client)
/// must degrade to a VALID `function_call` item — never to an `mcp.<name>_call` /
/// `<name>_call` item, which is not a Responses output item type.
#[test]
fn responses_unpaired_dynamic_mcp_call_degrades_to_function_call() {
    let adapter = adapter_for(ApiProtocol::Responses);
    let result = GenerateResult {
        content: vec![Content::ToolCall {
            // The `mcp.` prefix is what `parse_mcp_call` stamps on an MCP tool
            // name; if this leaked into the `<name>_call` branch it would emit
            // the invalid `mcp.search_docs_call`.
            id: "mcp_1".to_string(),
            name: "mcp.search_docs".to_string(),
            arguments: "{\"q\":\"x\"}".to_string(),
            provider_executed: true,
            dynamic: true,
            provider_metadata: Default::default(),
        }],
        usage: None,
        finish_reason: Some(FinishReason::Length),
        response_id: None,
        stop_details: None,
        provider_metadata: Default::default(),
    };
    let rendered = adapter
        .render_response(&result, &sample_prompt(), "resp_1")
        .unwrap();
    let output = rendered["output"].as_array().unwrap();
    // Exactly one item, and it is a valid `function_call` — not `<name>_call`.
    assert_eq!(output.len(), 1, "the lone unpaired call yields one item");
    let item = &output[0];
    assert_eq!(
        item["type"], "function_call",
        "an unpaired dynamic MCP call must degrade to a valid function_call"
    );
    assert_eq!(item["call_id"], "mcp_1");
    assert_eq!(item["name"], "mcp.search_docs");
    assert_eq!(item["arguments"], "{\"q\":\"x\"}");
    // The bug being closed: no invalid `<name>_call` / `mcp_call` item is emitted.
    assert!(
        output.iter().all(|i| i["type"] != "search_docs_call"
            && i["type"] != "mcp.search_docs_call"
            && i["type"] != "mcp_call"),
        "no invalid `<name>_call` (or `mcp_call`) item for an unpaired dynamic MCP call"
    );
}

/// Multiple `mcp_call`s interleaved with a regular `function_call` in one Responses
/// response: each MCP call recombines with ITS OWN inline result by `call_id` (no
/// cross-contamination, none dropped or duplicated), and the regular `function_call`
/// renders independently alongside (the response wire carries no
/// `function_call_output` — that is an input item, exercised by the request-side
/// test below).
#[test]
fn responses_interleaved_mcp_calls_recombine_per_call_id() {
    let adapter = adapter_for(ApiProtocol::Responses);
    let body = serde_json::json!({
        "id": "resp_1",
        "status": "completed",
        "output": [
            {
                "type": "mcp_call",
                "id": "mcp_a",
                "server_label": "srv-a",
                "name": "lookup",
                "arguments": "{\"q\":\"a\"}",
                "output": "answer-a"
            },
            // A regular function call interleaved between the two MCP calls.
            {
                "type": "function_call",
                "id": "fc_item_1",
                "call_id": "fc_1",
                "name": "calculator",
                "arguments": "{\"op\":\"add\"}"
            },
            {
                "type": "mcp_call",
                "id": "mcp_b",
                "server_label": "srv-b",
                "name": "search",
                "arguments": "{\"q\":\"b\"}",
                "output": "answer-b"
            }
        ]
    });
    let parsed = adapter.parse_response(body).unwrap();
    let rendered = adapter
        .render_response(&parsed, &sample_prompt(), "resp_1")
        .unwrap();
    let output = rendered["output"].as_array().unwrap();

    // Exactly two mcp_call items, each carrying its OWN result — no cross-talk.
    let mcp_items: Vec<_> = output.iter().filter(|i| i["type"] == "mcp_call").collect();
    assert_eq!(mcp_items.len(), 2, "two mcp_calls recombine into two items");
    let item_a = mcp_items
        .iter()
        .find(|i| i["id"] == "mcp_a")
        .expect("the first mcp_call");
    assert_eq!(item_a["server_label"], "srv-a");
    assert_eq!(item_a["name"], "lookup");
    assert_eq!(item_a["arguments"], "{\"q\":\"a\"}");
    assert_eq!(item_a["output"], "answer-a", "call a keeps its own result");
    let item_b = mcp_items
        .iter()
        .find(|i| i["id"] == "mcp_b")
        .expect("the second mcp_call");
    assert_eq!(item_b["server_label"], "srv-b");
    assert_eq!(item_b["name"], "search");
    assert_eq!(item_b["arguments"], "{\"q\":\"b\"}");
    assert_eq!(item_b["output"], "answer-b", "call b keeps its own result");

    // The interleaved regular function call renders independently as its own
    // `function_call` item (not recombined into, or contaminated by, either MCP
    // call).
    let fc = output
        .iter()
        .find(|i| i["type"] == "function_call")
        .expect("the regular function_call");
    assert_eq!(fc["call_id"], "fc_1");
    assert_eq!(fc["name"], "calculator");

    // Nothing dropped or duplicated: exactly 2 mcp_call + 1 function_call, and no
    // dynamic MCP pair leaked as a plain function item.
    assert_eq!(
        output.len(),
        3,
        "two mcp_calls + one independent function_call, none dropped/duplicated"
    );
}

/// On the REQUEST (input) wire, a regular `function_call` + its
/// `function_call_output` interleaved with a dynamic MCP call/result pair render
/// independently: the regular pair survives as `function_call` + `function_call_output`,
/// while the provider-executed MCP pair is dropped (not replayed on the input wire,
/// matching the AI SDK) — no cross-contamination either way.
#[test]
fn responses_request_function_call_pair_independent_of_dynamic_mcp() {
    let adapter = adapter_for(ApiProtocol::Responses);
    let prompt = Prompt {
        model: "test-model".to_string(),
        system: None,
        system_provider_metadata: Default::default(),
        messages: vec![Message {
            role: Role::Assistant,
            content: vec![
                // A regular client function call + its output.
                Content::ToolCall {
                    id: "fc_1".to_string(),
                    name: "calculator".to_string(),
                    arguments: "{\"op\":\"add\"}".to_string(),
                    provider_executed: false,
                    dynamic: false,
                    provider_metadata: Default::default(),
                },
                Content::ToolResult {
                    call_id: "fc_1".to_string(),
                    tool_name: Some("calculator".to_string()),
                    output: ToolResultOutput::Json {
                        value: serde_json::json!({ "sum": 42 }),
                    },
                    dynamic: false,
                    provider_metadata: Default::default(),
                },
                // A dynamic provider-executed MCP call + its inline result,
                // interleaved. Both must drop on the input wire.
                Content::ToolCall {
                    id: "mcp_1".to_string(),
                    name: "mcp.lookup".to_string(),
                    arguments: "{\"q\":\"x\"}".to_string(),
                    provider_executed: true,
                    dynamic: true,
                    provider_metadata: Default::default(),
                },
                Content::ToolResult {
                    call_id: "mcp_1".to_string(),
                    tool_name: Some("mcp.lookup".to_string()),
                    output: ToolResultOutput::Json {
                        value: serde_json::json!({
                            "type": "call", "name": "lookup", "output": "answer"
                        }),
                    },
                    dynamic: true,
                    provider_metadata: Default::default(),
                },
            ],
        }],
        tools: vec![],
        params: GenerationParams::default(),
        response_format: None,
        tool_choice: None,
        stream: false,
    };
    let rendered = adapter.render_request(&prompt).unwrap();
    let input = rendered["input"].as_array().unwrap();

    // The regular function_call + function_call_output both render, keyed to fc_1.
    let fc = input
        .iter()
        .find(|i| i["type"] == "function_call")
        .expect("the regular function_call");
    assert_eq!(fc["call_id"], "fc_1");
    assert_eq!(fc["name"], "calculator");
    let fc_out = input
        .iter()
        .find(|i| i["type"] == "function_call_output")
        .expect("the regular function_call_output");
    assert_eq!(fc_out["call_id"], "fc_1");

    // The dynamic MCP pair is dropped on the input wire (not replayed), and it did
    // not contaminate the regular pair: exactly one function_call + one
    // function_call_output, no mcp_call, no second function_call for `mcp_1`.
    assert!(
        input.iter().all(|i| i["type"] != "mcp_call"),
        "a provider-executed MCP call is not replayed as an input item"
    );
    assert_eq!(
        input
            .iter()
            .filter(|i| i["type"] == "function_call")
            .count(),
        1,
        "only the regular client call renders; the MCP call does not leak as one"
    );
    assert_eq!(
        input
            .iter()
            .filter(|i| i["type"] == "function_call_output")
            .count(),
        1,
        "only the regular client result renders; the MCP result does not leak as one"
    );
}

/// A Responses `local_shell_call` parses to a client `ToolCall` named
/// `local_shell` whose `action` is preserved, and renders back to a
/// `local_shell_call` item (same-protocol round-trip of the call).
#[test]
fn responses_local_shell_call_round_trips() {
    let adapter = adapter_for(ApiProtocol::Responses);
    let body = serde_json::json!({
        "id": "resp_1",
        "status": "completed",
        "output": [
            {
                "type": "local_shell_call",
                "id": "ls_item_1",
                "call_id": "ls_call_1",
                "action": {
                    "type": "exec",
                    "command": ["echo", "hi"],
                    "timeout_ms": 1000
                }
            }
        ]
    });
    let parsed = adapter.parse_response(body).unwrap();
    let call = parsed
        .content
        .iter()
        .find_map(|c| match c {
            Content::ToolCall {
                id,
                name,
                arguments,
                provider_executed,
                dynamic,
                ..
            } => Some((id, name, arguments, *provider_executed, *dynamic)),
            _ => None,
        })
        .expect("a local_shell call");
    assert_eq!(call.0, "ls_call_1");
    assert_eq!(call.1, "local_shell");
    assert!(
        !call.3,
        "local_shell is a client call, not provider-executed"
    );
    assert!(!call.4, "local_shell is not a dynamic MCP call");
    // The action payload survives in the call input.
    let input: serde_json::Value = serde_json::from_str(call.2).unwrap();
    assert_eq!(
        input["action"]["command"],
        serde_json::json!(["echo", "hi"])
    );
    assert_eq!(input["action"]["timeout_ms"], 1000);
    let rendered = adapter
        .render_response(&parsed, &sample_prompt(), "resp_1")
        .unwrap();
    let item = rendered["output"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["type"] == "local_shell_call")
        .expect("a local_shell_call output item");
    assert_eq!(item["call_id"], "ls_call_1");
    assert_eq!(
        item["id"], "ls_item_1",
        "the item id is restored from metadata"
    );
    assert_eq!(item["action"]["command"], serde_json::json!(["echo", "hi"]));
    assert_eq!(item["action"]["timeout_ms"], 1000);
}

// ===== cross-protocol degrade =====

/// A `dynamic` MCP `ToolCall` (parsed from an Anthropic `mcp_tool_use`) routed to
/// a non-MCP wire (Chat Completions) degrades faithfully to a regular tool call:
/// the call survives with its name + args, but the MCP server identity — which
/// has no slot on that wire — is dropped.
#[test]
fn dynamic_mcp_call_degrades_to_plain_tool_call_cross_protocol() {
    let messages = adapter_for(ApiProtocol::Messages);
    let chat = adapter_for(ApiProtocol::ChatCompletions);
    // Parse an Anthropic MCP call into the canonical IR.
    let body = serde_json::json!({
        "id": "msg_1",
        "content": [
            {
                "type": "mcp_tool_use",
                "id": "mcptoolu_1",
                "name": "search_docs",
                "input": { "query": "q" },
                "server_name": "docs-server"
            }
        ],
        "stop_reason": "end_turn"
    });
    let result = messages.parse_response(body).unwrap();
    // Render that result onto the Chat Completions wire.
    let rendered = chat
        .render_response(&result, &sample_prompt(), "chatcmpl_1")
        .unwrap();
    let tool_calls = rendered["choices"][0]["message"]["tool_calls"]
        .as_array()
        .expect("a plain tool_calls array");
    assert_eq!(tool_calls.len(), 1);
    // The call degrades to a regular function tool call: name + args survive…
    assert_eq!(tool_calls[0]["function"]["name"], "search_docs");
    assert_eq!(tool_calls[0]["id"], "mcptoolu_1");
    // …and there is no MCP server identity anywhere on this wire's tool call.
    let serialized = serde_json::to_string(&tool_calls[0]).unwrap();
    assert!(
        !serialized.contains("docs-server") && !serialized.contains("server_name"),
        "MCP server identity must not leak onto a non-MCP wire: {serialized}"
    );
}

/// The new `dynamic` flag is omitted from JSON when false (V3
/// `skip_serializing_if`) and present when true, on both `ToolCall` and
/// `ToolResult`.
#[test]
fn dynamic_flag_serde_defaults() {
    // false → omitted on a ToolCall.
    let call_false = serde_json::to_value(Content::ToolCall {
        id: "c1".to_string(),
        name: "t".to_string(),
        arguments: "{}".to_string(),
        provider_executed: false,
        dynamic: false,
        provider_metadata: Default::default(),
    })
    .unwrap();
    assert!(
        call_false.get("dynamic").is_none(),
        "false dynamic must be omitted: {call_false}"
    );
    // true → present on a ToolCall.
    let call_true = serde_json::to_value(Content::ToolCall {
        id: "c1".to_string(),
        name: "t".to_string(),
        arguments: "{}".to_string(),
        provider_executed: true,
        dynamic: true,
        provider_metadata: Default::default(),
    })
    .unwrap();
    assert_eq!(call_true["dynamic"], true);
    // Defaulting back: a payload without `dynamic` deserializes to false.
    let back: Content = serde_json::from_value(serde_json::json!({
        "type": "tool_call",
        "id": "c1",
        "name": "t",
        "arguments": "{}"
    }))
    .unwrap();
    assert!(matches!(back, Content::ToolCall { dynamic: false, .. }));
    // false → omitted on a ToolResult too.
    let result_false = serde_json::to_value(Content::ToolResult {
        call_id: "c1".to_string(),
        tool_name: None,
        output: ToolResultOutput::Text {
            value: "x".to_string(),
        },
        dynamic: false,
        provider_metadata: Default::default(),
    })
    .unwrap();
    assert!(
        result_false.get("dynamic").is_none(),
        "false dynamic must be omitted on a result: {result_false}"
    );
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
        system_provider_metadata: Default::default(),
        messages: vec![Message {
            role: Role::Assistant,
            content: vec![
                Content::ToolCall {
                    id: "call_1".to_string(),
                    name: "get_weather".to_string(),
                    arguments: "{}".to_string(),
                    provider_executed: false,
                    dynamic: false,
                    provider_metadata: Default::default(),
                },
                Content::ToolCall {
                    id: "ws_1".to_string(),
                    name: "web_search".to_string(),
                    arguments: "{}".to_string(),
                    provider_executed: true,
                    dynamic: false,
                    provider_metadata: Default::default(),
                },
            ],
        }],
        tools: Vec::new(),
        params: GenerationParams::default(),
        response_format: None,
        tool_choice: None,
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
            dynamic: false,
            provider_metadata: Default::default(),
        }],
        usage: None,
        finish_reason: Some(FinishReason::Stop),
        response_id: Some("resp_1".to_string()),
        stop_details: None,
        provider_metadata: Default::default(),
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
/// item keyed by `id`. (`local_shell_call` / `mcp_call` now parse too — exercised
/// by their dedicated round-trip tests — so this test no longer asserts they are
/// dropped.)
#[test]
fn responses_parses_and_reproduces_image_and_computer_calls() {
    let adapter = adapter_for(ApiProtocol::Responses);
    let body = serde_json::json!({
        "id": "resp_1",
        "status": "completed",
        "output": [
            { "type": "image_generation_call", "id": "ig_1", "result": "BASE64..." },
            { "type": "computer_call", "id": "cu_1", "status": "completed" }
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
    assert_eq!(
        calls,
        vec![("image_generation", true, "{}"), ("computer", true, "{}"),],
        "image_generation_call/computer_call parse as provider-executed server tools"
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
        dynamic: false,
        provider_metadata: Default::default(),
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
        dynamic: false,
        provider_metadata: Default::default(),
    })
    .unwrap();
    assert_eq!(value_true["provider_executed"], true);
}

// ===== Part B: typed tool choice (V3 toolChoice) =====

/// Anthropic nests `disable_parallel_tool_use` inside `tool_choice`; every other
/// protocol carries it as a top-level `parallel_tool_calls`. Canonicalizing
/// `tool_choice` must not drop it — it translates both ways and survives an
/// Anthropic round-trip.
#[test]
fn tool_choice_preserves_parallel_tool_use_control() {
    let messages = adapter_for(ApiProtocol::Messages);
    let chat = adapter_for(ApiProtocol::ChatCompletions);

    // Anthropic → OpenAI: nested flag becomes the top-level boolean.
    let anthropic_req = serde_json::json!({
        "model": "m",
        "max_tokens": 1024,
        "messages": [{ "role": "user", "content": "hi" }],
        "tool_choice": { "type": "auto", "disable_parallel_tool_use": true },
    });
    let prompt = messages.parse_request(anthropic_req).unwrap();
    let chat_out = chat.render_request(&prompt).unwrap();
    assert_eq!(chat_out["tool_choice"], "auto");
    assert_eq!(
        chat_out["parallel_tool_calls"], false,
        "disable_parallel_tool_use must survive as parallel_tool_calls"
    );

    // OpenAI → Anthropic: top-level boolean becomes the nested flag.
    let chat_req = serde_json::json!({
        "model": "m",
        "messages": [{ "role": "user", "content": "hi" }],
        "parallel_tool_calls": false,
    });
    let prompt = chat.parse_request(chat_req).unwrap();
    let anthropic_out = messages.render_request(&prompt).unwrap();
    assert_eq!(
        anthropic_out["tool_choice"],
        serde_json::json!({ "type": "auto", "disable_parallel_tool_use": true }),
        "parallel_tool_calls must reach Anthropic nested under tool_choice"
    );
    // ...and not leak as a top-level field Anthropic doesn't define.
    assert!(anthropic_out.get("parallel_tool_calls").is_none());

    // Anthropic → Anthropic round-trip keeps the nested flag intact.
    let round_trip = messages
        .render_request(&messages.parse_request(anthropic_out).unwrap())
        .unwrap();
    assert_eq!(
        round_trip["tool_choice"]["disable_parallel_tool_use"], true,
        "Anthropic round-trip must not drop disable_parallel_tool_use"
    );
}

/// A `tool_choice` shape an adapter can't map to the canonical slot (e.g. a
/// Responses hosted-tool / `allowed_tools` selector) is left in `extra` and
/// passes through verbatim on a same-protocol round-trip — never silently
/// dropped or mangled.
#[test]
fn unmapped_tool_choice_passes_through_unchanged() {
    let responses = adapter_for(ApiProtocol::Responses);
    let exotic = serde_json::json!({ "type": "allowed_tools", "mode": "auto", "tools": [] });
    let req = serde_json::json!({
        "model": "m",
        "input": "hi",
        "tool_choice": exotic,
    });
    let prompt = responses.parse_request(req).unwrap();
    assert_eq!(
        prompt.tool_choice, None,
        "an unmapped shape must not be force-fit into the canonical slot"
    );
    let rendered = responses.render_request(&prompt).unwrap();
    assert_eq!(
        rendered["tool_choice"], exotic,
        "an unmapped tool_choice must pass through verbatim"
    );
}

/// Generate Content's `allowedFunctionNames` is a restricting *set*; the
/// canonical slot only models the single-tool case. A multi-name `ANY` (or an
/// `AUTO`/`NONE` carrying names) must therefore be left in `extra` and pass
/// through verbatim — never narrowed to a bare `Required`/`Auto` that would
/// silently widen the constraint.
#[test]
fn gc_tool_choice_with_restricting_set_passes_through() {
    let gc = adapter_for(ApiProtocol::GenerateContent);
    let cases = [
        serde_json::json!({ "mode": "ANY", "allowedFunctionNames": ["A", "B"] }),
        serde_json::json!({ "mode": "AUTO", "allowedFunctionNames": ["A"] }),
    ];
    for fcc in cases {
        let mut body = minimal_request(ApiProtocol::GenerateContent);
        body["toolConfig"] = serde_json::json!({ "functionCallingConfig": fcc });
        let prompt = gc.parse_request(body.clone()).unwrap();
        assert_eq!(
            prompt.tool_choice, None,
            "a restricting-set config must not be force-fit into the canonical slot: {fcc}"
        );
        let rendered = gc.render_request(&prompt).unwrap();
        assert_eq!(
            rendered["toolConfig"], body["toolConfig"],
            "a restricting-set toolConfig must pass through verbatim: {fcc}"
        );
    }
}

// ===== typed sampling params (top_k / seed / stop / presence_penalty /
// frequency_penalty) cross-protocol translation =====

/// Each typed sampling slot the protocol carries survives a render→parse
/// round-trip on its own wire — the same-protocol fidelity contract. Per the
/// official wire shapes: Chat Completions carries `seed` / `stop` /
/// `presence_penalty` / `frequency_penalty` but no top-k; Anthropic carries
/// `top_k` / `stop_sequences`; Gemini carries all five; Responses carries none.
#[test]
fn sampling_params_same_protocol_round_trip() {
    // Chat Completions: seed, stop, presence_penalty, frequency_penalty.
    let chat = adapter_for(ApiProtocol::ChatCompletions);
    let params = chat
        .parse_request(serde_json::json!({
            "model": "gpt-5",
            "messages": [{"role": "user", "content": "hi"}],
            "seed": 7,
            "stop": ["END", "STOP"],
            "presence_penalty": 0.5,
            "frequency_penalty": -0.25
        }))
        .unwrap();
    let back = chat
        .parse_request(chat.render_request(&params).unwrap())
        .unwrap()
        .params;
    assert_eq!(back.seed, Some(7));
    assert_eq!(back.stop, vec!["END".to_string(), "STOP".to_string()]);
    assert_eq!(back.presence_penalty, Some(0.5));
    assert_eq!(back.frequency_penalty, Some(-0.25));
    assert_eq!(back.top_k, None, "Chat Completions has no top-k");

    // A scalar `stop` string normalises to a one-element list.
    let scalar = chat
        .parse_request(serde_json::json!({
            "model": "gpt-5",
            "messages": [{"role": "user", "content": "hi"}],
            "stop": "END"
        }))
        .unwrap();
    assert_eq!(scalar.params.stop, vec!["END".to_string()]);

    // Anthropic: top_k, stop_sequences.
    let anthropic = adapter_for(ApiProtocol::Messages);
    let back = anthropic
        .parse_request(
            anthropic
                .render_request(
                    &anthropic
                        .parse_request(serde_json::json!({
                            "model": "claude-opus-4-8",
                            "max_tokens": 16,
                            "messages": [{"role": "user", "content": "hi"}],
                            "top_k": 40,
                            "stop_sequences": ["END"]
                        }))
                        .unwrap(),
                )
                .unwrap(),
        )
        .unwrap()
        .params;
    assert_eq!(back.top_k, Some(40));
    assert_eq!(back.stop, vec!["END".to_string()]);

    // Gemini: all five, nested under generationConfig.
    let gemini = adapter_for(ApiProtocol::GenerateContent);
    let back = gemini
        .parse_request(
            gemini
                .render_request(
                    &gemini
                        .parse_request(serde_json::json!({
                            "model": "gemini-2.0-flash",
                            "contents": [{"role": "user", "parts": [{"text": "hi"}]}],
                            "generationConfig": {
                                "topK": 40,
                                "seed": 7,
                                "stopSequences": ["END", "STOP"],
                                "presencePenalty": 0.1,
                                "frequencyPenalty": -0.1
                            }
                        }))
                        .unwrap(),
                )
                .unwrap(),
        )
        .unwrap()
        .params;
    assert_eq!(back.top_k, Some(40));
    assert_eq!(back.seed, Some(7));
    assert_eq!(back.stop, vec!["END".to_string(), "STOP".to_string()]);
    assert_eq!(back.presence_penalty, Some(0.1));
    assert_eq!(back.frequency_penalty, Some(-0.1));
}

/// Cross-protocol translation, Chat → others: `seed` + `stop` +
/// `presence_penalty` + `frequency_penalty` authored on a Chat Completions body
/// must reach Gemini as `generationConfig.{seed, stopSequences, presencePenalty,
/// frequencyPenalty}` (the WHOLE point — these used to no-op as top-level keys
/// against Gemini's nested-config wire) and `stop` must reach Anthropic as
/// `stop_sequences`.
#[test]
fn sampling_params_translate_chat_to_gemini_and_anthropic() {
    let chat = adapter_for(ApiProtocol::ChatCompletions);
    let prompt = chat
        .parse_request(serde_json::json!({
            "model": "gpt-5",
            "messages": [{"role": "user", "content": "hi"}],
            "seed": 7,
            "stop": ["END", "STOP"],
            "presence_penalty": 0.5,
            "frequency_penalty": -0.25
        }))
        .unwrap();

    // Gemini: nested in generationConfig under the Google wire names.
    let gc = &adapter_for(ApiProtocol::GenerateContent)
        .render_request(&prompt)
        .unwrap()["generationConfig"];
    assert_eq!(gc["seed"], 7);
    assert_eq!(gc["stopSequences"], serde_json::json!(["END", "STOP"]));
    assert_eq!(gc["presencePenalty"], 0.5);
    assert_eq!(gc["frequencyPenalty"], -0.25);

    // Anthropic: `stop` becomes `stop_sequences`; seed / penalties have no
    // Anthropic wire field and must not appear.
    let anthropic = adapter_for(ApiProtocol::Messages)
        .render_request(&prompt)
        .unwrap();
    assert_eq!(
        anthropic["stop_sequences"],
        serde_json::json!(["END", "STOP"])
    );
    assert!(anthropic.get("seed").is_none());
    assert!(anthropic.get("presence_penalty").is_none());
    assert!(anthropic.get("frequency_penalty").is_none());

    // Responses carries none of these — they must not leak onto its wire.
    let responses = adapter_for(ApiProtocol::Responses)
        .render_request(&prompt)
        .unwrap();
    for key in ["seed", "stop", "presence_penalty", "frequency_penalty"] {
        assert!(
            responses.get(key).is_none(),
            "Responses must not render `{key}`"
        );
    }
}

/// Cross-protocol translation, Gemini/Anthropic → others: a Gemini `topK` +
/// `stopSequences` must reach Anthropic as `top_k` + `stop_sequences` and Chat
/// Completions as `stop` (Chat has no top-k, so `topK` is dropped there, not
/// leaked).
#[test]
fn sampling_params_translate_gemini_and_anthropic_to_others() {
    // Author on Gemini, render to Anthropic + Chat.
    let gemini = adapter_for(ApiProtocol::GenerateContent);
    let prompt = gemini
        .parse_request(serde_json::json!({
            "model": "gemini-2.0-flash",
            "contents": [{"role": "user", "parts": [{"text": "hi"}]}],
            "generationConfig": {
                "topK": 40,
                "stopSequences": ["END"]
            }
        }))
        .unwrap();
    assert_eq!(prompt.params.top_k, Some(40));

    // Anthropic: top_k + stop_sequences.
    let anthropic = adapter_for(ApiProtocol::Messages)
        .render_request(&prompt)
        .unwrap();
    assert_eq!(anthropic["top_k"], 40);
    assert_eq!(anthropic["stop_sequences"], serde_json::json!(["END"]));

    // Chat Completions: stop survives; top_k is dropped (no wire field), never
    // splatted verbatim.
    let chat = adapter_for(ApiProtocol::ChatCompletions)
        .render_request(&prompt)
        .unwrap();
    assert_eq!(chat["stop"], serde_json::json!(["END"]));
    assert!(
        chat.get("top_k").is_none() && chat.get("topK").is_none(),
        "Chat Completions must not carry top-k in any form: {chat}"
    );

    // Author an Anthropic `top_k` and confirm it reaches Gemini as `topK`.
    let anthropic_in = adapter_for(ApiProtocol::Messages)
        .parse_request(serde_json::json!({
            "model": "claude-opus-4-8",
            "max_tokens": 16,
            "messages": [{"role": "user", "content": "hi"}],
            "top_k": 64
        }))
        .unwrap();
    let gc = &adapter_for(ApiProtocol::GenerateContent)
        .render_request(&anthropic_in)
        .unwrap()["generationConfig"];
    assert_eq!(gc["topK"], 64);
}

/// Cross-protocol no-leak, Gemini/Anthropic → Responses: the Responses API has no
/// wire field for `top_k` / `seed` / `stop` / `presence_penalty` /
/// `frequency_penalty`, so a source request carrying any of them must render to
/// Responses with NONE of those values present — in neither the native (snake)
/// nor the source-wire (camel) spelling, and not leaked through the `extra`
/// splat. The params Responses *does* support (`temperature`, `top_p`,
/// `max_output_tokens`) still survive. Locks in the "Responses carries none"
/// contract the [`GenerationParams`] field docs assert.
#[test]
fn sampling_params_do_not_leak_to_responses() {
    let responses = adapter_for(ApiProtocol::Responses);

    // Gemini source: all five generationConfig knobs, plus supported params.
    let gemini = adapter_for(ApiProtocol::GenerateContent)
        .parse_request(serde_json::json!({
            "model": "gemini-2.0-flash",
            "contents": [{"role": "user", "parts": [{"text": "hi"}]}],
            "generationConfig": {
                "temperature": 0.5,
                "topP": 0.9,
                "maxOutputTokens": 32,
                "topK": 40,
                "seed": 7,
                "stopSequences": ["END"],
                "presencePenalty": 0.1,
                "frequencyPenalty": -0.1
            }
        }))
        .unwrap();
    let from_gemini = responses.render_request(&gemini).unwrap();
    // None of the unsupported five appears in any spelling.
    for key in [
        "top_k",
        "topK",
        "seed",
        "stop",
        "stop_sequences",
        "stopSequences",
        "presence_penalty",
        "presencePenalty",
        "frequency_penalty",
        "frequencyPenalty",
    ] {
        assert!(
            from_gemini.get(key).is_none(),
            "Responses must not carry `{key}` from a Gemini source: {from_gemini}"
        );
    }
    // The supported params survive the route.
    assert_eq!(from_gemini["temperature"], 0.5);
    assert_eq!(from_gemini["top_p"], 0.9);
    assert_eq!(from_gemini["max_output_tokens"], 32);

    // Anthropic source: `top_k` must not reach Responses in any form.
    let anthropic = adapter_for(ApiProtocol::Messages)
        .parse_request(serde_json::json!({
            "model": "claude-opus-4-8",
            "max_tokens": 16,
            "messages": [{"role": "user", "content": "hi"}],
            "top_k": 64
        }))
        .unwrap();
    assert_eq!(anthropic.params.top_k, Some(64));
    let from_anthropic = responses.render_request(&anthropic).unwrap();
    assert!(
        from_anthropic.get("top_k").is_none() && from_anthropic.get("topK").is_none(),
        "Responses must not carry top-k from an Anthropic source: {from_anthropic}"
    );
}

/// A parsed sampling param must NOT also remain in the raw `extra` passthrough —
/// otherwise it would be double-written (once from the typed slot, once verbatim
/// from `extra`) and forwarded with its source-wire name into a cross-protocol
/// target that ignores it.
#[test]
fn parsed_sampling_params_are_not_duplicated_in_extra() {
    // Chat Completions: seed / stop / penalties leave `extra`.
    let chat = adapter_for(ApiProtocol::ChatCompletions)
        .parse_request(serde_json::json!({
            "model": "gpt-5",
            "messages": [{"role": "user", "content": "hi"}],
            "seed": 7,
            "stop": ["END"],
            "presence_penalty": 0.5,
            "frequency_penalty": -0.25
        }))
        .unwrap();
    for key in ["seed", "stop", "presence_penalty", "frequency_penalty"] {
        assert!(
            !chat.params.extra.contains_key(key),
            "Chat Completions `{key}` must be promoted out of extra, not duplicated"
        );
    }

    // Anthropic: top_k / stop_sequences leave `extra`.
    let anthropic = adapter_for(ApiProtocol::Messages)
        .parse_request(serde_json::json!({
            "model": "claude-opus-4-8",
            "max_tokens": 16,
            "messages": [{"role": "user", "content": "hi"}],
            "top_k": 40,
            "stop_sequences": ["END"]
        }))
        .unwrap();
    for key in ["top_k", "stop_sequences"] {
        assert!(
            !anthropic.params.extra.contains_key(key),
            "Anthropic `{key}` must be promoted out of extra, not duplicated"
        );
    }

    // Gemini: all five leave the generationConfig-level `extra`. (The Gemini
    // adapter only stashes top-level Google fields under the sentinel key, so
    // generationConfig knobs that were promoted simply never land in `extra`.)
    let gemini = adapter_for(ApiProtocol::GenerateContent)
        .parse_request(serde_json::json!({
            "model": "gemini-2.0-flash",
            "contents": [{"role": "user", "parts": [{"text": "hi"}]}],
            "generationConfig": {
                "topK": 40,
                "seed": 7,
                "stopSequences": ["END"],
                "presencePenalty": 0.1,
                "frequencyPenalty": -0.1
            }
        }))
        .unwrap();
    for key in [
        "topK",
        "seed",
        "stopSequences",
        "presencePenalty",
        "frequencyPenalty",
    ] {
        assert!(
            !gemini.params.extra.contains_key(key),
            "Gemini generationConfig `{key}` must be promoted out of extra"
        );
    }
    // And the render carries each exactly once (no duplicate verbatim splat):
    // re-rendering and re-parsing recovers the same typed values.
    let rendered = adapter_for(ApiProtocol::GenerateContent)
        .render_request(&gemini)
        .unwrap();
    let gc = &rendered["generationConfig"];
    assert_eq!(gc["topK"], 40);
    assert_eq!(gc["seed"], 7);
    assert_eq!(gc["stopSequences"], serde_json::json!(["END"]));
}

// ===== Part C: typed tools (V3 function `strict` + provider-defined tools) =====

/// A prompt carrying exactly the given tool list (and nothing else interesting).
fn prompt_with_tools(tools: Vec<Tool>) -> Prompt {
    Prompt {
        model: "m".to_string(),
        system: None,
        system_provider_metadata: Default::default(),
        messages: vec![Message::text(Role::User, "hi")],
        tools,
        params: GenerationParams::default(),
        response_format: None,
        tool_choice: None,
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
                provider_metadata: Default::default(),
            }],
        );
        assert_eq!(
            tools,
            vec![Tool::Function {
                name: "get_weather".to_string(),
                description: Some("look up weather".to_string()),
                parameters: serde_json::json!({ "type": "object" }),
                strict: Some(true),
                provider_metadata: Default::default(),
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
        provider_metadata: Default::default(),
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
        provider_metadata: Default::default(),
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
                provider_metadata: Default::default(),
            }],
        );
        assert_eq!(
            tools,
            vec![Tool::Function {
                name: "get_weather".to_string(),
                description: Some("desc".to_string()),
                parameters: serde_json::json!({ "type": "object" }),
                strict: None,
                provider_metadata: Default::default(),
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
                provider_metadata: Default::default(),
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
            provider_metadata: Default::default(),
        },
        Tool::ProviderDefined {
            id: "openai.code_interpreter".to_string(),
            name: "code_interpreter".to_string(),
            args: serde_json::json!({ "container": { "type": "auto" } }),
            provider_metadata: Default::default(),
        },
        Tool::ProviderDefined {
            id: "openai.file_search".to_string(),
            name: "file_search".to_string(),
            args: serde_json::json!({ "vector_store_ids": ["vs_1"], "max_num_results": 5 }),
            provider_metadata: Default::default(),
        },
        Tool::ProviderDefined {
            id: "openai.image_generation".to_string(),
            name: "image_generation".to_string(),
            args: serde_json::json!({ "quality": "high" }),
            provider_metadata: Default::default(),
        },
        Tool::ProviderDefined {
            id: "openai.computer_use_preview".to_string(),
            name: "computer_use_preview".to_string(),
            args: serde_json::json!({ "display_width": 1024, "display_height": 768, "environment": "browser" }),
            provider_metadata: Default::default(),
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
            provider_metadata: Default::default(),
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
            provider_metadata: Default::default(),
        },
        Tool::ProviderDefined {
            id: "anthropic.code_execution_20250522".to_string(),
            name: "code_execution".to_string(),
            args: serde_json::json!({}),
            provider_metadata: Default::default(),
        },
        Tool::ProviderDefined {
            id: "anthropic.computer_20250124".to_string(),
            name: "computer".to_string(),
            args: serde_json::json!({ "display_width_px": 1024, "display_height_px": 768 }),
            provider_metadata: Default::default(),
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
            provider_metadata: Default::default(),
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
            provider_metadata: Default::default(),
        },
        Tool::ProviderDefined {
            id: "google.codeExecution".to_string(),
            name: "codeExecution".to_string(),
            args: serde_json::json!({}),
            provider_metadata: Default::default(),
        },
        Tool::ProviderDefined {
            id: "google.googleSearchRetrieval".to_string(),
            name: "googleSearchRetrieval".to_string(),
            args: serde_json::json!({ "dynamicRetrievalConfig": { "mode": "MODE_DYNAMIC" } }),
            provider_metadata: Default::default(),
        },
        Tool::ProviderDefined {
            id: "google.urlContext".to_string(),
            name: "urlContext".to_string(),
            args: serde_json::json!({}),
            provider_metadata: Default::default(),
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
            provider_metadata: Default::default(),
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
            provider_metadata: Default::default(),
        },
        Tool::ProviderDefined {
            id: "openai.web_search_preview".to_string(),
            name: "web_search_preview".to_string(),
            args: serde_json::json!({ "search_context_size": "high" }),
            provider_metadata: Default::default(),
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
        provider_metadata: Default::default(),
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
        Tool::ProviderDefined { id, name, args, .. } => {
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
        provider_metadata: Default::default(),
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
/// tool has no valid representation, so it is DROPPED rather than splatted as a
/// `{type:<tool>}` entry — which a strict upstream rejects, failing the whole
/// request. A function tool alongside it still renders.
#[test]
fn provider_defined_tool_dropped_from_chat_completions() {
    let server = Tool::ProviderDefined {
        id: "openai.web_search_preview".to_string(),
        name: "web_search_preview".to_string(),
        args: serde_json::json!({ "search_context_size": "low" }),
        provider_metadata: Default::default(),
    };
    let func = Tool::Function {
        name: "get_weather".to_string(),
        description: None,
        parameters: serde_json::json!({ "type": "object" }),
        strict: None,
        provider_metadata: Default::default(),
    };
    let adapter = adapter_for(ApiProtocol::ChatCompletions);

    // A server tool alone leaves no tools at all — omit the key, never `tools: []`.
    let only_server = adapter
        .render_request(&prompt_with_tools(vec![server.clone()]))
        .unwrap();
    assert!(
        only_server.get("tools").is_none(),
        "server-only tools must be omitted, not sent as an invalid entry: {only_server}"
    );

    // Mixed with a function tool: only the function tool survives.
    let mixed = adapter
        .render_request(&prompt_with_tools(vec![server, func]))
        .unwrap();
    let tools = mixed["tools"].as_array().expect("function tool present");
    assert_eq!(tools.len(), 1, "only the function tool survives: {tools:?}");
    assert_eq!(tools[0]["type"], "function");
    assert_eq!(tools[0]["function"]["name"], "get_weather");
}

/// Cross-protocol regression (Codex): a Responses request carrying OpenAI
/// server tools (`web_search`) and a Codex `namespace` group alongside real
/// function tools, routed to a Chat Completions upstream, renders ONLY the
/// function tools — every entry is `{type:"function"}`. Pre-fix the server /
/// namespace tools were splatted verbatim and DeepSeek rejected the whole
/// request (`upstream_error`), which Codex surfaced as a stream disconnect.
#[test]
fn responses_server_tools_dropped_when_routed_to_chat_completions() {
    let responses_req = serde_json::json!({
        "model": "m",
        "input": [{ "role": "user", "content": "hi" }],
        "tools": [
            { "type": "function", "name": "exec_command", "parameters": { "type": "object" } },
            { "type": "web_search", "external_web_access": true },
            { "type": "namespace", "name": "multi_agent_v1", "tools": [], "description": "x" }
        ]
    });
    let prompt = adapter_for(ApiProtocol::Responses)
        .parse_request(responses_req)
        .unwrap();
    let chat = adapter_for(ApiProtocol::ChatCompletions)
        .render_request(&prompt)
        .unwrap();
    let tools = chat["tools"].as_array().expect("function tool present");
    assert_eq!(tools.len(), 1, "only the function tool survives: {tools:?}");
    assert!(
        tools.iter().all(|t| t["type"] == "function"),
        "every Chat Completions tool must be a function: {tools:?}"
    );
    assert_eq!(tools[0]["function"]["name"], "exec_command");
}

/// Cross-protocol regression (Claude Code): a Messages request carrying an
/// Anthropic server tool (`web_search_20250305`) alongside a custom function
/// tool, routed to a Chat Completions upstream, renders only the function tool.
#[test]
fn messages_server_tools_dropped_when_routed_to_chat_completions() {
    let messages_req = serde_json::json!({
        "model": "m",
        "max_tokens": 100,
        "messages": [{ "role": "user", "content": "hi" }],
        "tools": [
            { "name": "get_weather", "input_schema": { "type": "object" } },
            { "type": "web_search_20250305", "name": "web_search", "max_uses": 3 }
        ]
    });
    let prompt = adapter_for(ApiProtocol::Messages)
        .parse_request(messages_req)
        .unwrap();
    let chat = adapter_for(ApiProtocol::ChatCompletions)
        .render_request(&prompt)
        .unwrap();
    let tools = chat["tools"].as_array().expect("function tool present");
    assert_eq!(tools.len(), 1, "only the function tool survives: {tools:?}");
    assert!(
        tools.iter().all(|t| t["type"] == "function"),
        "every Chat Completions tool must be a function: {tools:?}"
    );
    assert_eq!(tools[0]["function"]["name"], "get_weather");
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
        provider_metadata: Default::default(),
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
        provider_metadata: Default::default(),
    })
    .unwrap();
    assert_eq!(provider["type"], "provider_defined");
    assert_eq!(provider["id"], "openai.web_search_preview");
}

// ===== Source (web-search citations) — V3 LanguageModelV3Source parity =====

/// Collect every `Content::Source` from a result's content, in order.
fn sources_of(content: &[Content]) -> Vec<&Source> {
    content
        .iter()
        .filter_map(|c| match c {
            Content::Source { source, .. } => Some(source),
            _ => None,
        })
        .collect()
}

/// The canonical `Source` IR uses an internal `source_type` tag and round-trips
/// through serde for both the URL and document variants.
#[test]
fn source_serde_round_trips_both_variants() {
    let url = Content::Source {
        source: Source::Url {
            id: "https://example.invalid/a#0".to_string(),
            url: "https://example.invalid/a".to_string(),
            title: Some("A".to_string()),
        },
        provider_metadata: Default::default(),
    };
    let v = serde_json::to_value(&url).unwrap();
    assert_eq!(v["type"], "source");
    assert_eq!(v["source"]["source_type"], "url");
    assert_eq!(v["source"]["url"], "https://example.invalid/a");
    assert_eq!(url, serde_json::from_value(v).unwrap());

    let doc = Content::Source {
        source: Source::Document {
            id: "report.pdf#0".to_string(),
            media_type: "application/pdf".to_string(),
            title: "report.pdf".to_string(),
            filename: Some("report.pdf".to_string()),
        },
        provider_metadata: Default::default(),
    };
    let v = serde_json::to_value(&doc).unwrap();
    assert_eq!(v["source"]["source_type"], "document");
    assert_eq!(v["source"]["media_type"], "application/pdf");
    assert_eq!(doc, serde_json::from_value(v).unwrap());

    // An absent URL title is omitted entirely (no JSON null) per the IR rule.
    let no_title = serde_json::to_value(&Content::Source {
        source: Source::Url {
            id: "x".to_string(),
            url: "https://example.invalid/x".to_string(),
            title: None,
        },
        provider_metadata: Default::default(),
    })
    .unwrap();
    assert!(no_title["source"].get("title").is_none());
}

/// Chat Completions: a `message.annotations[]` `url_citation` is lifted into a
/// `Content::Source` on parse and re-attached at the same location on render —
/// a same-protocol citation round-trip.
#[test]
fn chat_completions_url_citation_round_trip() {
    let adapter = chat_completions::ChatCompletionsAdapter;
    let provider_resp = serde_json::json!({
        "id": "chatcmpl-1",
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": "see source",
                "annotations": [{
                    "type": "url_citation",
                    "url_citation": {
                        "url": "https://example.invalid/doc",
                        "title": "Doc",
                        "start_index": 0,
                        "end_index": 10
                    }
                }]
            },
            "finish_reason": "stop"
        }]
    });
    let result = adapter.parse_response(provider_resp).unwrap();
    let sources = sources_of(&result.content);
    assert_eq!(sources.len(), 1, "one url citation parsed into a Source");
    match sources[0] {
        Source::Url { url, title, id } => {
            assert_eq!(url, "https://example.invalid/doc");
            assert_eq!(title.as_deref(), Some("Doc"));
            // The wire carried no id; one is synthesized from url + index.
            assert_eq!(id, "https://example.invalid/doc#0");
        }
        other => panic!("expected url source, got {other:?}"),
    }

    // Render back: the citation reappears under `message.annotations[]`.
    let prompt = sample_prompt();
    let rendered = adapter
        .render_response(&result, &prompt, "chatcmpl-1")
        .unwrap();
    let ann = &rendered["choices"][0]["message"]["annotations"][0];
    assert_eq!(ann["type"], "url_citation");
    assert_eq!(ann["url_citation"]["url"], "https://example.invalid/doc");
    assert_eq!(ann["url_citation"]["title"], "Doc");

    // And a re-parse of the rendered body recovers the same Source.
    let reparsed = adapter.parse_response(rendered).unwrap();
    assert_eq!(sources_of(&reparsed.content), sources_of(&result.content));
}

/// Gemini: `groundingMetadata.groundingChunks[].web` is lifted into a
/// `Content::Source` on parse and re-attached on render.
#[test]
fn generate_content_grounding_round_trip() {
    let adapter = generate_content::GenerateContentAdapter;
    let provider_resp = serde_json::json!({
        "candidates": [{
            "content": { "role": "model", "parts": [{ "text": "grounded answer" }] },
            "groundingMetadata": {
                "groundingChunks": [
                    { "web": { "uri": "https://example.invalid/g1", "title": "G1" } },
                    { "web": { "uri": "https://example.invalid/g2" } }
                ]
            },
            "finishReason": "STOP"
        }]
    });
    let result = adapter.parse_response(provider_resp).unwrap();
    let sources = sources_of(&result.content);
    assert_eq!(sources.len(), 2, "two grounding chunks parsed into Sources");
    match sources[1] {
        // A chunk without a title yields a titleless Source (no fabricated title).
        Source::Url { url, title, .. } => {
            assert_eq!(url, "https://example.invalid/g2");
            assert!(title.is_none());
        }
        other => panic!("expected url source, got {other:?}"),
    }

    let prompt = sample_prompt();
    let rendered = adapter.render_response(&result, &prompt, "resp_1").unwrap();
    let chunks = &rendered["candidates"][0]["groundingMetadata"]["groundingChunks"];
    assert_eq!(chunks[0]["web"]["uri"], "https://example.invalid/g1");
    assert_eq!(chunks[0]["web"]["title"], "G1");
    assert_eq!(chunks[1]["web"]["uri"], "https://example.invalid/g2");
    // The titleless chunk renders with no `title` key (no null).
    assert!(chunks[1]["web"].get("title").is_none());

    let reparsed = adapter.parse_response(rendered).unwrap();
    assert_eq!(sources_of(&reparsed.content), sources_of(&result.content));
}

/// Gemini SHOULD-FIX: grounding chunk kinds beyond `web` are mapped, not
/// silently dropped. A `retrievedContext` chunk with an http(s) uri becomes a
/// [`Source::Url`]; an `image` chunk maps via its `sourceUri`; a `maps` chunk
/// with a `uri` becomes a URL; and a `gs://` `retrievedContext` becomes a
/// [`Source::Document`] with the media type inferred from the path.
#[test]
fn generate_content_grounding_maps_all_chunk_kinds() {
    let adapter = generate_content::GenerateContentAdapter;
    let provider_resp = serde_json::json!({
        "candidates": [{
            "content": { "role": "model", "parts": [{ "text": "grounded" }] },
            "groundingMetadata": {
                "groundingChunks": [
                    { "retrievedContext": { "uri": "https://example.invalid/rag", "title": "RAG" } },
                    { "image": { "sourceUri": "https://example.invalid/page", "imageUri": "https://example.invalid/i.png", "title": "Img" } },
                    { "maps": { "uri": "https://maps.example.invalid/p", "title": "Place" } },
                    { "retrievedContext": { "uri": "gs://bucket/report.pdf", "title": "Report" } }
                ]
            },
            "finishReason": "STOP"
        }]
    });
    let result = adapter.parse_response(provider_resp).unwrap();
    let sources = sources_of(&result.content);
    assert_eq!(sources.len(), 4, "every grounding chunk kind is mapped");

    // retrievedContext (http) -> Url
    match sources[0] {
        Source::Url { url, title, .. } => {
            assert_eq!(url, "https://example.invalid/rag");
            assert_eq!(title.as_deref(), Some("RAG"));
        }
        other => panic!("expected url source for retrievedContext(http), got {other:?}"),
    }
    // image -> Url keyed by sourceUri (not imageUri)
    match sources[1] {
        Source::Url { url, .. } => assert_eq!(url, "https://example.invalid/page"),
        other => panic!("expected url source for image chunk, got {other:?}"),
    }
    // maps -> Url
    match sources[2] {
        Source::Url { url, .. } => assert_eq!(url, "https://maps.example.invalid/p"),
        other => panic!("expected url source for maps chunk, got {other:?}"),
    }
    // retrievedContext (gs://) -> Document with inferred media type + filename
    match sources[3] {
        Source::Document {
            media_type,
            title,
            filename,
            ..
        } => {
            assert_eq!(media_type, "application/pdf");
            assert_eq!(title, "Report");
            assert_eq!(filename.as_deref(), Some("report.pdf"));
        }
        other => panic!("expected document source for retrievedContext(gs://), got {other:?}"),
    }
}

/// Anthropic: both citation shapes — a text block's `citations[]`
/// (`web_search_result_location`) and a `web_search_tool_result` block's
/// `content[]` (`web_search_result`) — are lifted into `Content::Source` parts,
/// and render re-attaches them as a `web_search_tool_result` block.
#[test]
fn messages_citations_round_trip_both_shapes() {
    let adapter = messages::MessagesAdapter;
    let provider_resp = serde_json::json!({
        "id": "msg_1",
        "type": "message",
        "role": "assistant",
        "content": [
            {
                "type": "text",
                "text": "cited",
                "citations": [{
                    "type": "web_search_result_location",
                    "url": "https://example.invalid/inline",
                    "title": "Inline",
                    "cited_text": "snippet",
                    "encrypted_index": "abc"
                }]
            },
            {
                "type": "web_search_tool_result",
                "tool_use_id": "srvtoolu_x",
                "content": [
                    { "type": "web_search_result", "url": "https://example.invalid/r1", "title": "R1" },
                    { "type": "web_search_result", "url": "https://example.invalid/r2", "title": "R2" }
                ]
            }
        ],
        "stop_reason": "end_turn",
        "usage": { "input_tokens": 1, "output_tokens": 1 }
    });
    let result = adapter.parse_response(provider_resp).unwrap();
    let sources = sources_of(&result.content);
    assert_eq!(sources.len(), 3, "inline citation + 2 search results");
    // Ids are unique across blocks (running index): inline is #0, results #1/#2.
    let ids: Vec<&str> = sources
        .iter()
        .map(|s| match s {
            Source::Url { id, .. } => id.as_str(),
            Source::Document { id, .. } => id.as_str(),
        })
        .collect();
    assert_eq!(
        ids,
        vec![
            "https://example.invalid/inline#0",
            "https://example.invalid/r1#1",
            "https://example.invalid/r2#2"
        ]
    );

    // Render: a single `web_search_tool_result` block carries every URL source.
    let prompt = sample_prompt();
    let rendered = adapter.render_response(&result, &prompt, "msg_1").unwrap();
    let blocks = rendered["content"].as_array().unwrap();
    let wstr = blocks
        .iter()
        .find(|b| b["type"] == "web_search_tool_result")
        .expect("a web_search_tool_result block was rendered");
    let entries = wstr["content"].as_array().unwrap();
    assert_eq!(entries.len(), 3);
    assert_eq!(entries[0]["type"], "web_search_result");
    assert_eq!(entries[0]["url"], "https://example.invalid/inline");

    // Re-parse recovers all three sources (now all from the result block).
    let reparsed = adapter.parse_response(rendered).unwrap();
    assert_eq!(sources_of(&reparsed.content).len(), 3);
}

/// Anthropic MUST-FIX: a response carrying BOTH a `server_tool_use` and its
/// `web_search_tool_result` renders a VALID paired wire — the originating call's
/// real id is reused as the result block's `tool_use_id` (same-protocol reuse,
/// no second `server_tool_use` is synthesized). A client echoing this assistant
/// turn into a follow-up must not see an orphan call/result.
#[test]
fn messages_web_search_pair_reuses_real_id() {
    let adapter = messages::MessagesAdapter;
    // Upstream: a `server_tool_use` call followed by its result block.
    let provider_resp = serde_json::json!({
        "id": "msg_1",
        "type": "message",
        "role": "assistant",
        "content": [
            { "type": "server_tool_use", "id": "srvtoolu_real", "name": "web_search", "input": { "query": "q" } },
            {
                "type": "web_search_tool_result",
                "tool_use_id": "srvtoolu_real",
                "content": [
                    { "type": "web_search_result", "url": "https://example.invalid/r1", "title": "R1" }
                ]
            }
        ],
        "stop_reason": "end_turn",
        "usage": { "input_tokens": 1, "output_tokens": 1 }
    });
    let parsed = adapter.parse_response(provider_resp).unwrap();
    let rendered = adapter
        .render_response(&parsed, &sample_prompt(), "msg_1")
        .unwrap();
    let blocks = rendered["content"].as_array().unwrap();
    // Exactly one `server_tool_use` (the real one) — no synthesized duplicate.
    let server_calls: Vec<&serde_json::Value> = blocks
        .iter()
        .filter(|b| b["type"] == "server_tool_use")
        .collect();
    assert_eq!(
        server_calls.len(),
        1,
        "no duplicate server_tool_use synthesized"
    );
    let call_id = server_calls[0]["id"].as_str().unwrap();
    let result = blocks
        .iter()
        .find(|b| b["type"] == "web_search_tool_result")
        .expect("a web_search_tool_result block was rendered");
    // The pair correlates: the result's tool_use_id == the call's real id.
    assert_eq!(call_id, "srvtoolu_real");
    assert_eq!(
        result["tool_use_id"].as_str().unwrap(),
        call_id,
        "result block reuses the originating server_tool_use id"
    );
}

/// Anthropic MUST-FIX (mixed wire): when an inline-cited text block's `Source`
/// precedes the `server_tool_use` in canonical order, the render must still
/// reuse the REAL call id (not synthesize a second pair that would orphan the
/// real `server_tool_use`). Exactly one `server_tool_use` is emitted and its id
/// equals the single result block's `tool_use_id`.
#[test]
fn messages_web_search_pair_no_orphan_with_inline_citation() {
    let adapter = messages::MessagesAdapter;
    let provider_resp = serde_json::json!({
        "id": "msg_1",
        "type": "message",
        "role": "assistant",
        "content": [
            {
                "type": "text",
                "text": "cited",
                "citations": [{
                    "type": "web_search_result_location",
                    "url": "https://example.invalid/inline",
                    "title": "Inline",
                    "cited_text": "snippet",
                    "encrypted_index": "abc"
                }]
            },
            { "type": "server_tool_use", "id": "srvtoolu_real", "name": "web_search", "input": { "query": "q" } },
            {
                "type": "web_search_tool_result",
                "tool_use_id": "srvtoolu_real",
                "content": [
                    { "type": "web_search_result", "url": "https://example.invalid/r1", "title": "R1" }
                ]
            }
        ],
        "stop_reason": "end_turn",
        "usage": { "input_tokens": 1, "output_tokens": 1 }
    });
    let parsed = adapter.parse_response(provider_resp).unwrap();
    let rendered = adapter
        .render_response(&parsed, &sample_prompt(), "msg_1")
        .unwrap();
    let blocks = rendered["content"].as_array().unwrap();
    // Exactly one server_tool_use (the real one): no synthesized duplicate, so
    // no orphan call.
    let server_calls: Vec<&serde_json::Value> = blocks
        .iter()
        .filter(|b| b["type"] == "server_tool_use")
        .collect();
    assert_eq!(
        server_calls.len(),
        1,
        "no orphaned/duplicate server_tool_use"
    );
    assert_eq!(server_calls[0]["id"], "srvtoolu_real");
    // Exactly one result block, pairing with the real call id; it carries every
    // URL source (the inline one + the result hit).
    let results: Vec<&serde_json::Value> = blocks
        .iter()
        .filter(|b| b["type"] == "web_search_tool_result")
        .collect();
    assert_eq!(
        results.len(),
        1,
        "all sources collapse into one result block"
    );
    assert_eq!(results[0]["tool_use_id"], "srvtoolu_real");
    assert_eq!(results[0]["content"].as_array().unwrap().len(), 2);
}

/// Anthropic MUST-FIX (cross-protocol): a Gemini grounding response rendered to
/// an Anthropic client has no originating `server_tool_use`, so the render
/// SYNTHESIZES a matching call block immediately before the
/// `web_search_tool_result`. Both blocks must be present and share one id, so
/// the emitted wire is a valid pair.
#[test]
fn messages_web_search_pair_synthesized_cross_protocol() {
    let gemini = generate_content::GenerateContentAdapter;
    let messages = messages::MessagesAdapter;
    let gemini_resp = serde_json::json!({
        "candidates": [{
            "content": { "role": "model", "parts": [{ "text": "grounded" }] },
            "groundingMetadata": {
                "groundingChunks": [
                    { "web": { "uri": "https://example.invalid/g", "title": "G" } }
                ]
            },
            "finishReason": "STOP"
        }]
    });
    // Gemini upstream -> canonical (a bare `Source`, no provider-executed call).
    let canonical = gemini.parse_response(gemini_resp).unwrap();
    assert_eq!(sources_of(&canonical.content).len(), 1);
    assert!(
        !canonical
            .content
            .iter()
            .any(|c| matches!(c, Content::ToolCall { .. })),
        "Gemini grounding carries no tool call"
    );

    // canonical -> Anthropic client response.
    let rendered = messages
        .render_response(&canonical, &sample_prompt(), "msg_1")
        .unwrap();
    let blocks = rendered["content"].as_array().unwrap();
    let server_call = blocks
        .iter()
        .find(|b| b["type"] == "server_tool_use")
        .expect("a server_tool_use block was synthesized");
    let result = blocks
        .iter()
        .find(|b| b["type"] == "web_search_tool_result")
        .expect("a web_search_tool_result block was rendered");
    assert_eq!(server_call["name"], "web_search");
    // The synthesized pair shares one id -> valid wire.
    assert_eq!(
        server_call["id"].as_str().unwrap(),
        result["tool_use_id"].as_str().unwrap(),
        "synthesized pair shares one tool_use_id"
    );
    // The synthesized call precedes its result block on the wire.
    let call_pos = blocks.iter().position(|b| b["type"] == "server_tool_use");
    let result_pos = blocks
        .iter()
        .position(|b| b["type"] == "web_search_tool_result");
    assert!(call_pos < result_pos, "the call precedes its result block");

    // The rendered wire re-parses cleanly: the synthesized call becomes a
    // provider-executed tool call, the result block its source.
    let reparsed = messages.parse_response(rendered).unwrap();
    assert_eq!(sources_of(&reparsed.content).len(), 1);
}

/// Responses: an `output_text` part's `annotations[]` — a `url_citation` and a
/// `file_citation` — are lifted into `Content::Source` parts (URL + document)
/// and re-attached on render.
#[test]
fn responses_annotations_round_trip_url_and_document() {
    let adapter = responses::ResponsesAdapter;
    let provider_resp = serde_json::json!({
        "id": "resp_1",
        "object": "response",
        "status": "completed",
        "output": [{
            "type": "message",
            "role": "assistant",
            "content": [{
                "type": "output_text",
                "text": "answer",
                "annotations": [
                    { "type": "url_citation", "url": "https://example.invalid/u", "title": "U" },
                    { "type": "file_citation", "filename": "report.txt", "file_id": "file_1", "index": 3 }
                ]
            }]
        }]
    });
    let result = adapter.parse_response(provider_resp).unwrap();
    let sources = sources_of(&result.content);
    assert_eq!(sources.len(), 2);
    assert!(matches!(sources[0], Source::Url { .. }));
    match sources[1] {
        Source::Document {
            media_type,
            title,
            filename,
            ..
        } => {
            assert_eq!(media_type, "text/plain");
            assert_eq!(title, "report.txt");
            assert_eq!(filename.as_deref(), Some("report.txt"));
        }
        other => panic!("expected document source, got {other:?}"),
    }

    let prompt = sample_prompt();
    let rendered = adapter.render_response(&result, &prompt, "resp_1").unwrap();
    let anns = rendered["output"][0]["content"][0]["annotations"]
        .as_array()
        .unwrap();
    assert_eq!(anns.len(), 2);
    assert_eq!(anns[0]["type"], "url_citation");
    assert_eq!(anns[0]["url"], "https://example.invalid/u");
    assert_eq!(anns[1]["type"], "file_citation");
    assert_eq!(anns[1]["filename"], "report.txt");

    // The URL source re-parses identically; the document source survives as a
    // document citation (its provider file_id is a documented drop).
    let reparsed = adapter.parse_response(rendered).unwrap();
    assert_eq!(sources_of(&reparsed.content).len(), 2);
}

/// Cross-protocol: a Gemini grounding response is parsed to canonical Sources,
/// then rendered onto the Chat Completions wire as `message.annotations[]` —
/// url + title + a (synthesized) id all cross faithfully.
#[test]
fn cross_protocol_gemini_grounding_to_chat_annotations() {
    let gemini = generate_content::GenerateContentAdapter;
    let chat = chat_completions::ChatCompletionsAdapter;

    let gemini_resp = serde_json::json!({
        "candidates": [{
            "content": { "role": "model", "parts": [{ "text": "x" }] },
            "groundingMetadata": {
                "groundingChunks": [
                    { "web": { "uri": "https://example.invalid/x", "title": "X" } }
                ]
            },
            "finishReason": "STOP"
        }]
    });
    // Gemini upstream -> canonical
    let canonical = gemini.parse_response(gemini_resp).unwrap();
    assert_eq!(sources_of(&canonical.content).len(), 1);

    // canonical -> Chat client response: the citation lands on `annotations[]`.
    let prompt = sample_prompt();
    let chat_resp = chat
        .render_response(&canonical, &prompt, "chatcmpl-x")
        .unwrap();
    let ann = &chat_resp["choices"][0]["message"]["annotations"][0];
    assert_eq!(ann["type"], "url_citation");
    assert_eq!(ann["url_citation"]["url"], "https://example.invalid/x");
    assert_eq!(ann["url_citation"]["title"], "X");

    // And a Chat client parsing that response recovers the same canonical Source.
    let back = chat.parse_response(chat_resp).unwrap();
    assert_eq!(sources_of(&back.content), sources_of(&canonical.content));
}

/// Streaming, Gemini decode: grounding metadata in a stream chunk surfaces as a
/// `StreamPart::Source`, deduped across the repeated accumulating chunks.
#[test]
fn generate_content_streams_source_deduped() {
    let adapter = generate_content::GenerateContentAdapter;
    let mut decoder = adapter.stream_decoder();
    let chunk = |with_finish: bool| {
        let mut c = serde_json::json!({
            "candidates": [{
                "content": { "role": "model", "parts": [{ "text": "t" }] },
                "groundingMetadata": {
                    "groundingChunks": [
                        { "web": { "uri": "https://example.invalid/s", "title": "S" } }
                    ]
                }
            }]
        });
        if with_finish {
            c["candidates"][0]["finishReason"] = "STOP".into();
        }
        SseEvent {
            event: None,
            data: c.to_string(),
        }
    };
    let mut parts = decoder.decode(&chunk(false)).unwrap();
    // The same grounding chunk repeats on the next frame but must not re-emit.
    parts.extend(decoder.decode(&chunk(true)).unwrap());

    let source_parts: Vec<&Source> = parts
        .iter()
        .filter_map(|p| match p {
            StreamPart::Source { source } => Some(source),
            _ => None,
        })
        .collect();
    assert_eq!(
        source_parts.len(),
        1,
        "grounding source emitted exactly once"
    );
    match source_parts[0] {
        Source::Url { url, .. } => assert_eq!(url, "https://example.invalid/s"),
        other => panic!("expected url source, got {other:?}"),
    }
}

/// Streaming, Chat decode: `delta.annotations[]` surfaces as a
/// `StreamPart::Source`, and the Chat encoder re-attaches it on the wire — a
/// live decode + live re-encode for the same protocol.
#[test]
fn chat_completions_streams_and_reencodes_source() {
    let adapter = chat_completions::ChatCompletionsAdapter;
    let mut decoder = adapter.stream_decoder();
    let event = SseEvent {
        event: None,
        data: serde_json::json!({
            "id": "chatcmpl-1",
            "choices": [{
                "index": 0,
                "delta": {
                    "annotations": [{
                        "type": "url_citation",
                        "url_citation": { "url": "https://example.invalid/s", "title": "S" }
                    }]
                }
            }]
        })
        .to_string(),
    };
    let parts = decoder.decode(&event).unwrap();
    let src = parts
        .iter()
        .find_map(|p| match p {
            StreamPart::Source { source } => Some(source.clone()),
            _ => None,
        })
        .expect("decoded a StreamPart::Source from delta.annotations");

    // Re-encode the decoded source: it reappears as a `delta.annotations` chunk.
    let mut encoder = adapter.stream_encoder("chatcmpl-1", "m");
    let frames = encoder.encode(&StreamPart::Source { source: src }).unwrap();
    let frame = frames
        .first()
        .expect("encoder emitted a frame for the source");
    let SseFrame::Event { data, .. } = frame else {
        panic!("expected an SSE event frame");
    };
    let chunk: serde_json::Value = serde_json::from_str(data).unwrap();
    let ann = &chunk["choices"][0]["delta"]["annotations"][0];
    assert_eq!(ann["type"], "url_citation");
    assert_eq!(ann["url_citation"]["url"], "https://example.invalid/s");
}

/// Streaming, Anthropic: a `web_search_tool_result` content block decodes to a
/// `StreamPart::Source`, which the Messages encoder re-emits as the same block —
/// a live streamed-citation round-trip on the Anthropic wire.
#[test]
fn messages_streams_and_reencodes_source() {
    let adapter = messages::MessagesAdapter;
    let mut decoder = adapter.stream_decoder();
    let event = SseEvent {
        event: Some("content_block_start".to_string()),
        data: serde_json::json!({
            "type": "content_block_start",
            "index": 2,
            "content_block": {
                "type": "web_search_tool_result",
                "tool_use_id": "srvtoolu_x",
                "content": [
                    { "type": "web_search_result", "url": "https://example.invalid/s", "title": "S" }
                ]
            }
        })
        .to_string(),
    };
    let parts = decoder.decode(&event).unwrap();
    let src = parts
        .iter()
        .find_map(|p| match p {
            StreamPart::Source { source } => Some(source.clone()),
            _ => None,
        })
        .expect("decoded a StreamPart::Source from a streamed web_search_tool_result");
    match &src {
        Source::Url { url, title, .. } => {
            assert_eq!(url, "https://example.invalid/s");
            assert_eq!(title.as_deref(), Some("S"));
        }
        other => panic!("expected url source, got {other:?}"),
    }

    // Re-encode: the Messages encoder buffers streamed citations and flushes
    // them as one collapsed `server_tool_use` ↔ `web_search_tool_result` pair at
    // terminal. The `Source` part alone emits no result block (it is buffered);
    // the terminal `Finish` triggers the flush.
    let mut encoder = adapter.stream_encoder("msg_1", "m");
    let buffered = encoder.encode(&StreamPart::Source { source: src }).unwrap();
    assert!(
        !buffered.iter().any(|f| match f {
            SseFrame::Event { data, .. } => {
                let v: serde_json::Value = serde_json::from_str(data).unwrap();
                v["content_block"]["type"] == "web_search_tool_result"
            }
            _ => false,
        }),
        "the source is buffered, not emitted as its own block yet"
    );
    let frames = encoder
        .encode(&StreamPart::Finish {
            reason: FinishReason::Stop,
        })
        .unwrap();
    // Collect the synthesized pair's blocks and assert the wire is a VALID pair:
    // a `server_tool_use` and a `web_search_tool_result` sharing one id.
    let server_tool_id = frames.iter().find_map(|f| match f {
        SseFrame::Event { data, .. } => {
            let v: serde_json::Value = serde_json::from_str(data).unwrap();
            (v["content_block"]["type"] == "server_tool_use")
                .then(|| v["content_block"]["id"].as_str().unwrap().to_string())
        }
        _ => None,
    });
    let result_tool_use_id = frames.iter().find_map(|f| match f {
        SseFrame::Event { data, .. } => {
            let v: serde_json::Value = serde_json::from_str(data).unwrap();
            (v["content_block"]["type"] == "web_search_tool_result"
                && v["content_block"]["content"][0]["url"] == "https://example.invalid/s")
                .then(|| {
                    v["content_block"]["tool_use_id"]
                        .as_str()
                        .unwrap()
                        .to_string()
                })
        }
        _ => None,
    });
    let server_tool_id =
        server_tool_id.expect("encoder emitted a synthesized server_tool_use block");
    let result_tool_use_id =
        result_tool_use_id.expect("encoder emitted the paired web_search_tool_result block");
    assert_eq!(
        server_tool_id, result_tool_use_id,
        "the streamed pair shares one tool_use_id (valid wire)"
    );
}

/// Streaming, Responses SHOULD-FIX: a `response.output_text.annotation.added`
/// event decodes to a `StreamPart::Source` (no longer dropped by the catch-all),
/// and the Responses encoder re-emits it as the same annotation event — a live
/// streamed-citation round-trip on the Responses wire.
#[test]
fn responses_streams_and_reencodes_annotation() {
    let adapter = responses::ResponsesAdapter;
    let mut decoder = adapter.stream_decoder();
    let event = SseEvent {
        event: Some("response.output_text.annotation.added".to_string()),
        data: serde_json::json!({
            "type": "response.output_text.annotation.added",
            "item_id": "msg_1",
            "output_index": 0,
            "content_index": 0,
            "annotation_index": 0,
            "annotation": {
                "type": "url_citation",
                "url": "https://example.invalid/s",
                "title": "S",
                "start_index": 0,
                "end_index": 5
            }
        })
        .to_string(),
    };
    let parts = decoder.decode(&event).unwrap();
    let src = parts
        .iter()
        .find_map(|p| match p {
            StreamPart::Source { source } => Some(source.clone()),
            _ => None,
        })
        .expect("decoded a StreamPart::Source from response.output_text.annotation.added");
    match &src {
        Source::Url { url, title, .. } => {
            assert_eq!(url, "https://example.invalid/s");
            assert_eq!(title.as_deref(), Some("S"));
        }
        other => panic!("expected url source, got {other:?}"),
    }

    // Re-encode the decoded source: it reappears as a
    // `response.output_text.annotation.added` event with the same citation.
    let mut encoder = adapter.stream_encoder("resp_1", "m");
    let frames = encoder.encode(&StreamPart::Source { source: src }).unwrap();
    let ann = frames
        .iter()
        .find_map(|f| match f {
            SseFrame::Event { data, .. } => {
                let v: serde_json::Value = serde_json::from_str(data).unwrap();
                (v["type"] == "response.output_text.annotation.added")
                    .then_some(v["annotation"].clone())
            }
            _ => None,
        })
        .expect("encoder re-emitted a response.output_text.annotation.added event");
    assert_eq!(ann["type"], "url_citation");
    assert_eq!(ann["url"], "https://example.invalid/s");
    assert_eq!(ann["title"], "S");
}

// ===== per-part provider_metadata (V3 providerMetadata / providerOptions) =====

/// Read the `anthropic.cacheControl` object out of a part's provider metadata.
fn anthropic_cache_control(meta: &ProviderMetadata) -> Option<&serde_json::Value> {
    meta.get("anthropic")?.get("cacheControl")
}

/// An ephemeral cache-control breakpoint, as Anthropic spells it on the wire.
fn ephemeral() -> serde_json::Value {
    serde_json::json!({ "type": "ephemeral" })
}

/// Anthropic `cache_control` on a request text block survives a full
/// same-protocol round-trip (parse → render), landing back on the rendered block
/// as a `cache_control` field. This is prompt caching — the breakpoint must be
/// reproduced exactly or the cache boundary moves.
#[test]
fn messages_cache_control_on_text_block_round_trips() {
    let adapter = messages::MessagesAdapter;
    let body = serde_json::json!({
        "model": "claude-3-5-sonnet",
        "max_tokens": 64,
        "messages": [{
            "role": "user",
            "content": [{
                "type": "text",
                "text": "cache me",
                "cache_control": { "type": "ephemeral" },
            }],
        }],
    });
    let prompt = adapter.parse_request(body).unwrap();
    // It landed in the canonical slot under the anthropic namespace.
    let user = prompt
        .messages
        .iter()
        .find(|m| m.role == Role::User)
        .unwrap();
    match &user.content[0] {
        Content::Text {
            provider_metadata, ..
        } => assert_eq!(
            anthropic_cache_control(provider_metadata),
            Some(&ephemeral())
        ),
        other => panic!("expected text content, got {other:?}"),
    }
    // And it renders back onto the Anthropic text block.
    let rendered = adapter.render_request(&prompt).unwrap();
    let block = &rendered["messages"][0]["content"][0];
    assert_eq!(block["type"], "text");
    assert_eq!(block["cache_control"], ephemeral());
}

/// Anthropic tool-level `cache_control` (the "cache the whole tools array"
/// pattern) round-trips: lifted off the tool on parse, rendered back on the tool.
#[test]
fn messages_cache_control_on_tool_round_trips() {
    let adapter = messages::MessagesAdapter;
    let body = serde_json::json!({
        "model": "claude-3-5-sonnet",
        "max_tokens": 64,
        "messages": [{ "role": "user", "content": "hi" }],
        "tools": [{
            "name": "get_weather",
            "description": "weather",
            "input_schema": { "type": "object" },
            "cache_control": { "type": "ephemeral" },
        }],
    });
    let prompt = adapter.parse_request(body).unwrap();
    match &prompt.tools[0] {
        Tool::Function {
            provider_metadata, ..
        } => assert_eq!(
            anthropic_cache_control(provider_metadata),
            Some(&ephemeral())
        ),
        other => panic!("expected function tool, got {other:?}"),
    }
    let rendered = adapter.render_request(&prompt).unwrap();
    assert_eq!(rendered["tools"][0]["cache_control"], ephemeral());
    // The breakpoint did NOT leak into the tool's `input_schema` / args.
    assert!(
        rendered["tools"][0]["input_schema"]
            .get("cache_control")
            .is_none()
    );
}

/// Anthropic `cache_control` on a `tool_result` block round-trips: a long tool
/// output can mark a cache boundary, and the breakpoint must survive.
#[test]
fn messages_cache_control_on_tool_result_round_trips() {
    let adapter = messages::MessagesAdapter;
    let body = serde_json::json!({
        "model": "claude-3-5-sonnet",
        "max_tokens": 64,
        "messages": [{
            "role": "user",
            "content": [{
                "type": "tool_result",
                "tool_use_id": "toolu_1",
                "content": "the result",
                "cache_control": { "type": "ephemeral" },
            }],
        }],
    });
    let prompt = adapter.parse_request(body).unwrap();
    let tool_msg = prompt
        .messages
        .iter()
        .find(|m| m.role == Role::Tool)
        .unwrap();
    match &tool_msg.content[0] {
        Content::ToolResult {
            provider_metadata, ..
        } => assert_eq!(
            anthropic_cache_control(provider_metadata),
            Some(&ephemeral())
        ),
        other => panic!("expected tool result, got {other:?}"),
    }
    let rendered = adapter.render_request(&prompt).unwrap();
    // Tool results render inside a user-role message's content blocks.
    let block = &rendered["messages"][0]["content"][0];
    assert_eq!(block["type"], "tool_result");
    assert_eq!(block["cache_control"], ephemeral());
}

/// Cross-protocol preservation: an Anthropic `cache_control` hint parsed from a
/// Messages request is preserved (namespaced) when the same canonical prompt is
/// rendered to a *Chat Completions* upstream. Chat Completions has no
/// `cache_control` on its wire, so it does not express the hint — but it also
/// must not lose it: the canonical slot still carries it for any later hop back
/// to Anthropic, and the OpenAI namespace is untouched.
#[test]
fn anthropic_cache_control_preserved_across_protocols() {
    let inbound = messages::MessagesAdapter;
    let outbound = chat_completions::ChatCompletionsAdapter;
    let body = serde_json::json!({
        "model": "claude-3-5-sonnet",
        "max_tokens": 64,
        "messages": [{
            "role": "user",
            "content": [{
                "type": "text",
                "text": "cache me",
                "cache_control": { "type": "ephemeral" },
            }],
        }],
    });
    let prompt = inbound.parse_request(body).unwrap();
    // The canonical prompt still carries the anthropic-namespaced hint.
    let user = prompt
        .messages
        .iter()
        .find(|m| m.role == Role::User)
        .unwrap();
    match &user.content[0] {
        Content::Text {
            provider_metadata, ..
        } => assert_eq!(
            anthropic_cache_control(provider_metadata),
            Some(&ephemeral())
        ),
        other => panic!("expected text content, got {other:?}"),
    }
    // Rendering to Chat Completions does not surface (or corrupt) the hint: no
    // `cache_control` appears anywhere in the Chat request body.
    let rendered = outbound.render_request(&prompt).unwrap();
    assert!(
        !rendered.to_string().contains("cache_control"),
        "Chat Completions must not emit Anthropic cache_control on its wire"
    );
    // The canonical IR is unchanged — the hint is still there for a later
    // Anthropic hop (faithful namespaced preservation).
    match &prompt
        .messages
        .iter()
        .find(|m| m.role == Role::User)
        .unwrap()
        .content[0]
    {
        Content::Text {
            provider_metadata, ..
        } => assert_eq!(
            anthropic_cache_control(provider_metadata),
            Some(&ephemeral())
        ),
        other => panic!("expected text content, got {other:?}"),
    }
}

/// An Anthropic thinking block's `signature` round-trips through Messages so a
/// multi-turn thinking conversation can replay the signed reasoning block. The
/// signature has no canonical field; it rides `provider_metadata["anthropic"]`.
#[test]
fn messages_reasoning_signature_round_trips() {
    let adapter = messages::MessagesAdapter;
    let response = serde_json::json!({
        "id": "msg_1",
        "type": "message",
        "role": "assistant",
        "content": [{
            "type": "thinking",
            "thinking": "let me think",
            "signature": "SIG-abc-123",
        }],
        "stop_reason": "end_turn",
        "usage": { "input_tokens": 1, "output_tokens": 1 },
    });
    let result = adapter.parse_response(response).unwrap();
    match &result.content[0] {
        Content::Reasoning {
            text,
            provider_metadata,
        } => {
            assert_eq!(text, "let me think");
            assert_eq!(
                provider_metadata
                    .get("anthropic")
                    .and_then(|a| a.get("signature")),
                Some(&serde_json::Value::String("SIG-abc-123".into()))
            );
        }
        other => panic!("expected reasoning, got {other:?}"),
    }
    // Re-render the reasoning as a request block: the signature reappears.
    let block = render_content_block_via_request(&adapter, &result.content[0]);
    assert_eq!(block["type"], "thinking");
    assert_eq!(block["thinking"], "let me think");
    assert_eq!(block["signature"], "SIG-abc-123");
}

/// A `redacted_thinking` block round-trips byte-for-byte: the encrypted `data`
/// and the `redacted_thinking` block type are both restored on render.
#[test]
fn messages_redacted_thinking_round_trips() {
    let adapter = messages::MessagesAdapter;
    let response = serde_json::json!({
        "id": "msg_1",
        "type": "message",
        "role": "assistant",
        "content": [{
            "type": "redacted_thinking",
            "data": "ENCRYPTED-PAYLOAD",
        }],
        "stop_reason": "end_turn",
        "usage": { "input_tokens": 1, "output_tokens": 1 },
    });
    let result = adapter.parse_response(response).unwrap();
    let block = render_content_block_via_request(&adapter, &result.content[0]);
    assert_eq!(block["type"], "redacted_thinking");
    assert_eq!(block["data"], "ENCRYPTED-PAYLOAD");
    // It is NOT re-emitted as a plaintext `thinking` block.
    assert!(block.get("thinking").is_none());
}

/// Render a single canonical assistant content block through the Messages
/// request renderer (assistant message) and return the one rendered block.
fn render_content_block_via_request(
    adapter: &messages::MessagesAdapter,
    content: &Content,
) -> serde_json::Value {
    let prompt = Prompt {
        model: "claude-3-5-sonnet".to_string(),
        system: None,
        system_provider_metadata: Default::default(),
        messages: vec![Message {
            role: Role::Assistant,
            content: vec![content.clone()],
        }],
        tools: vec![],
        params: GenerationParams {
            max_tokens: Some(64),
            ..Default::default()
        },
        response_format: None,
        tool_choice: None,
        stream: false,
    };
    let rendered = adapter.render_request(&prompt).unwrap();
    rendered["messages"][0]["content"][0].clone()
}

/// OpenAI `system_fingerprint` survives a same-protocol round-trip: it is lifted
/// from the response into the result's `provider_metadata["openai"]` and
/// rendered back onto the Chat Completions response object.
#[test]
fn chat_system_fingerprint_round_trips_on_result() {
    let adapter = chat_completions::ChatCompletionsAdapter;
    let response = serde_json::json!({
        "id": "chatcmpl-1",
        "object": "chat.completion",
        "model": "gpt-4o",
        "system_fingerprint": "fp_44709d6fcb",
        "choices": [{
            "index": 0,
            "message": { "role": "assistant", "content": "hi" },
            "finish_reason": "stop",
        }],
    });
    let result = adapter.parse_response(response).unwrap();
    assert_eq!(
        result
            .provider_metadata
            .get("openai")
            .and_then(|o| o.get("systemFingerprint")),
        Some(&serde_json::Value::String("fp_44709d6fcb".into()))
    );
    let prompt = Prompt {
        model: "gpt-4o".to_string(),
        system: None,
        system_provider_metadata: Default::default(),
        messages: vec![],
        tools: vec![],
        params: GenerationParams::default(),
        response_format: None,
        tool_choice: None,
        stream: false,
    };
    let rendered = adapter
        .render_response(&result, &prompt, "chatcmpl-1")
        .unwrap();
    assert_eq!(rendered["system_fingerprint"], "fp_44709d6fcb");
}

/// Gemini `modelVersion` (no canonical field) round-trips through Generate
/// Content at result level under `provider_metadata["google"]`.
#[test]
fn generate_content_model_version_round_trips_on_result() {
    let adapter = generate_content::GenerateContentAdapter;
    let response = serde_json::json!({
        "candidates": [{
            "content": { "role": "model", "parts": [{ "text": "hi" }] },
            "finishReason": "STOP",
            "index": 0,
        }],
        "modelVersion": "gemini-2.0-flash-001",
        "usageMetadata": { "promptTokenCount": 1, "candidatesTokenCount": 1, "totalTokenCount": 2 },
    });
    let result = adapter.parse_response(response).unwrap();
    assert_eq!(
        result
            .provider_metadata
            .get("google")
            .and_then(|g| g.get("modelVersion")),
        Some(&serde_json::Value::String("gemini-2.0-flash-001".into()))
    );
    let prompt = Prompt {
        model: "gemini-2.0-flash".to_string(),
        system: None,
        system_provider_metadata: Default::default(),
        messages: vec![],
        tools: vec![],
        params: GenerationParams::default(),
        response_format: None,
        tool_choice: None,
        stream: false,
    };
    let rendered = adapter.render_response(&result, &prompt, "resp_1").unwrap();
    assert_eq!(rendered["modelVersion"], "gemini-2.0-flash-001");
}

/// Gemini `thoughtSignature` on a thinking part round-trips so a multi-turn
/// thinking conversation can replay the signed reasoning.
#[test]
fn generate_content_thought_signature_round_trips() {
    let adapter = generate_content::GenerateContentAdapter;
    let response = serde_json::json!({
        "candidates": [{
            "content": { "role": "model", "parts": [
                { "text": "reasoning", "thought": true, "thoughtSignature": "TS-xyz" },
            ] },
            "finishReason": "STOP",
            "index": 0,
        }],
        "usageMetadata": { "promptTokenCount": 1, "candidatesTokenCount": 1, "totalTokenCount": 2 },
    });
    let result = adapter.parse_response(response).unwrap();
    match &result.content[0] {
        Content::Reasoning {
            provider_metadata, ..
        } => assert_eq!(
            provider_metadata
                .get("google")
                .and_then(|g| g.get("thoughtSignature")),
            Some(&serde_json::Value::String("TS-xyz".into()))
        ),
        other => panic!("expected reasoning, got {other:?}"),
    }
    // Re-render to a request: the signature reappears on the thought part.
    let prompt = Prompt {
        model: "gemini-2.0-flash".to_string(),
        system: None,
        system_provider_metadata: Default::default(),
        messages: vec![Message {
            role: Role::Assistant,
            content: vec![result.content[0].clone()],
        }],
        tools: vec![],
        params: GenerationParams::default(),
        response_format: None,
        tool_choice: None,
        stream: false,
    };
    let rendered = adapter.render_request(&prompt).unwrap();
    let part = &rendered["contents"][0]["parts"][0];
    assert_eq!(part["thought"], true);
    assert_eq!(part["thoughtSignature"], "TS-xyz");
}

/// The `File.extra` → `provider_metadata` migration is real: an OpenAI image
/// `detail` hint (the canonical example of the former ad-hoc `extra`) is parsed
/// under `provider_metadata["openai"]["detail"]` and rendered back onto the
/// `image_url`. There is no `extra` mechanism anymore — this is the single slot.
#[test]
fn chat_image_detail_round_trips_via_provider_metadata() {
    let adapter = chat_completions::ChatCompletionsAdapter;
    let body = serde_json::json!({
        "model": "gpt-4o",
        "messages": [{
            "role": "user",
            "content": [{
                "type": "image_url",
                "image_url": { "url": "https://example.invalid/a.png", "detail": "high" },
            }],
        }],
    });
    let prompt = adapter.parse_request(body).unwrap();
    match &prompt.messages[0].content[0] {
        Content::File {
            provider_metadata, ..
        } => assert_eq!(
            provider_metadata
                .get("openai")
                .and_then(|o| o.get("detail")),
            Some(&serde_json::Value::String("high".into()))
        ),
        other => panic!("expected file content, got {other:?}"),
    }
    let rendered = adapter.render_request(&prompt).unwrap();
    assert_eq!(
        rendered["messages"][0]["content"][0]["image_url"]["detail"],
        "high"
    );
}

/// The OpenAI image `detail` hint is preserved (namespaced) across protocols:
/// routed to an Anthropic upstream, which has no `detail` field, it is not
/// expressed but also not lost from the canonical IR.
#[test]
fn openai_image_detail_preserved_across_protocols() {
    let inbound = chat_completions::ChatCompletionsAdapter;
    let outbound = messages::MessagesAdapter;
    let body = serde_json::json!({
        "model": "gpt-4o",
        "messages": [{
            "role": "user",
            "content": [{
                "type": "image_url",
                "image_url": { "url": "https://example.invalid/a.png", "detail": "low" },
            }],
        }],
    });
    let prompt = inbound.parse_request(body).unwrap();
    // Render to Anthropic — the image block carries no `detail`.
    let rendered = outbound.render_request(&prompt).unwrap();
    let block = &rendered["messages"][0]["content"][0];
    assert_eq!(block["type"], "image");
    assert!(block.get("detail").is_none());
    // The canonical IR still carries it under the openai namespace.
    match &prompt.messages[0].content[0] {
        Content::File {
            provider_metadata, ..
        } => assert_eq!(
            provider_metadata
                .get("openai")
                .and_then(|o| o.get("detail")),
            Some(&serde_json::Value::String("low".into()))
        ),
        other => panic!("expected file content, got {other:?}"),
    }
}

/// The Anthropic Source ↔ ToolCall correlation is *exact*: the originating
/// `server_tool_use` id paired with a `web_search_tool_result` block is captured
/// into the Source's `provider_metadata` on parse, and — when the originating
/// call did not survive into the rendered content — restored as the synthesized
/// pair's `tool_use_id`, rather than the by-position placeholder.
#[test]
fn messages_source_tool_use_id_correlation_is_exact() {
    let adapter = messages::MessagesAdapter;
    let response = serde_json::json!({
        "id": "msg_1",
        "type": "message",
        "role": "assistant",
        "content": [
            { "type": "server_tool_use", "id": "srvtoolu_REAL", "name": "web_search", "input": {} },
            {
                "type": "web_search_tool_result",
                "tool_use_id": "srvtoolu_REAL",
                "content": [{ "type": "web_search_result", "url": "https://example.invalid/x", "title": "X" }],
            },
        ],
        "stop_reason": "end_turn",
        "usage": { "input_tokens": 1, "output_tokens": 1 },
    });
    let result = adapter.parse_response(response).unwrap();
    // The Source captured the exact originating id.
    let source = result
        .content
        .iter()
        .find_map(|c| match c {
            Content::Source {
                provider_metadata, ..
            } => Some(provider_metadata.clone()),
            _ => None,
        })
        .expect("a Source was lifted from the web_search_tool_result");
    assert_eq!(
        source.get("anthropic").and_then(|a| a.get("toolUseId")),
        Some(&serde_json::Value::String("srvtoolu_REAL".into()))
    );

    // Drop the originating call from the content (simulating a hop that did not
    // round-trip the provider-executed call) and confirm the render falls back
    // to the EXACT preserved id, not the `srvtoolu_citations` placeholder.
    let mut result_without_call = result.clone();
    result_without_call
        .content
        .retain(|c| !matches!(c, Content::ToolCall { .. }));
    let prompt = Prompt {
        model: "claude-3-5-sonnet".to_string(),
        system: None,
        system_provider_metadata: Default::default(),
        messages: vec![],
        tools: vec![],
        params: GenerationParams::default(),
        response_format: None,
        tool_choice: None,
        stream: false,
    };
    let rendered = adapter
        .render_response(&result_without_call, &prompt, "msg_1")
        .unwrap();
    let blocks = rendered["content"].as_array().unwrap();
    let pair_id = blocks
        .iter()
        .find(|b| b["type"] == "web_search_tool_result")
        .and_then(|b| b["tool_use_id"].as_str())
        .unwrap();
    assert_eq!(
        pair_id, "srvtoolu_REAL",
        "the synthesized pair must reuse the exact originating tool_use_id"
    );
    // The synthesized server_tool_use shares that exact id, so the pair is valid.
    let call_id = blocks
        .iter()
        .find(|b| b["type"] == "server_tool_use")
        .and_then(|b| b["id"].as_str())
        .unwrap();
    assert_eq!(call_id, "srvtoolu_REAL");
}

/// When the originating provider-executed call *does* survive into the content,
/// the render reuses its real id and emits no duplicate `server_tool_use` — the
/// exact-id metadata does not change that established same-protocol behavior.
#[test]
fn messages_source_with_surviving_call_reuses_call_id() {
    let adapter = messages::MessagesAdapter;
    let response = serde_json::json!({
        "id": "msg_1",
        "type": "message",
        "role": "assistant",
        "content": [
            { "type": "server_tool_use", "id": "srvtoolu_REAL", "name": "web_search", "input": {} },
            {
                "type": "web_search_tool_result",
                "tool_use_id": "srvtoolu_REAL",
                "content": [{ "type": "web_search_result", "url": "https://example.invalid/x", "title": "X" }],
            },
        ],
        "stop_reason": "end_turn",
        "usage": { "input_tokens": 1, "output_tokens": 1 },
    });
    let result = adapter.parse_response(response).unwrap();
    let prompt = Prompt {
        model: "claude-3-5-sonnet".to_string(),
        system: None,
        system_provider_metadata: Default::default(),
        messages: vec![],
        tools: vec![],
        params: GenerationParams::default(),
        response_format: None,
        tool_choice: None,
        stream: false,
    };
    let rendered = adapter.render_response(&result, &prompt, "msg_1").unwrap();
    let blocks = rendered["content"].as_array().unwrap();
    // Exactly one server_tool_use (the originating call), keyed by the real id.
    let calls: Vec<_> = blocks
        .iter()
        .filter(|b| b["type"] == "server_tool_use")
        .collect();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0]["id"], "srvtoolu_REAL");
    let result_block = blocks
        .iter()
        .find(|b| b["type"] == "web_search_tool_result")
        .unwrap();
    assert_eq!(result_block["tool_use_id"], "srvtoolu_REAL");
}

/// Anthropic `cache_control` on a request **image** block round-trips: lifted
/// into the canonical slot on parse, rendered back onto the image block. Caching
/// is not text-only — image/document/tool_use blocks are cacheable too.
#[test]
fn messages_cache_control_on_image_block_round_trips() {
    let adapter = messages::MessagesAdapter;
    let body = serde_json::json!({
        "model": "claude-3-5-sonnet",
        "max_tokens": 64,
        "messages": [{
            "role": "user",
            "content": [{
                "type": "image",
                "source": { "type": "base64", "media_type": "image/png", "data": IMG_B64 },
                "cache_control": { "type": "ephemeral" },
            }],
        }],
    });
    let prompt = adapter.parse_request(body).unwrap();
    match &prompt.messages[0].content[0] {
        Content::File {
            provider_metadata, ..
        } => assert_eq!(
            anthropic_cache_control(provider_metadata),
            Some(&ephemeral())
        ),
        other => panic!("expected file content, got {other:?}"),
    }
    let rendered = adapter.render_request(&prompt).unwrap();
    let block = &rendered["messages"][0]["content"][0];
    assert_eq!(block["type"], "image");
    assert_eq!(block["cache_control"], ephemeral());
}

/// Anthropic `cache_control` on a request **document** block round-trips — a
/// long PDF prefix is a common cache breakpoint.
#[test]
fn messages_cache_control_on_document_block_round_trips() {
    let adapter = messages::MessagesAdapter;
    let body = serde_json::json!({
        "model": "claude-3-5-sonnet",
        "max_tokens": 64,
        "messages": [{
            "role": "user",
            "content": [{
                "type": "document",
                "source": { "type": "base64", "media_type": "application/pdf", "data": IMG_B64 },
                "cache_control": { "type": "ephemeral" },
            }],
        }],
    });
    let prompt = adapter.parse_request(body).unwrap();
    match &prompt.messages[0].content[0] {
        Content::File {
            media_type,
            provider_metadata,
            ..
        } => {
            assert_eq!(media_type, "application/pdf");
            assert_eq!(
                anthropic_cache_control(provider_metadata),
                Some(&ephemeral())
            );
        }
        other => panic!("expected file content, got {other:?}"),
    }
    let rendered = adapter.render_request(&prompt).unwrap();
    let block = &rendered["messages"][0]["content"][0];
    assert_eq!(block["type"], "document");
    assert_eq!(block["cache_control"], ephemeral());
}

/// Anthropic `cache_control` on an assistant **tool_use** block round-trips:
/// caching applies to `tool_use` blocks just like text/image/document.
#[test]
fn messages_cache_control_on_tool_use_block_round_trips() {
    let adapter = messages::MessagesAdapter;
    // A tool_use block arrives on an assistant turn; parse it through a request.
    let body = serde_json::json!({
        "model": "claude-3-5-sonnet",
        "max_tokens": 64,
        "messages": [{
            "role": "assistant",
            "content": [{
                "type": "tool_use",
                "id": "toolu_1",
                "name": "get_weather",
                "input": { "city": "SF" },
                "cache_control": { "type": "ephemeral" },
            }],
        }],
    });
    let prompt = adapter.parse_request(body).unwrap();
    let assistant = prompt
        .messages
        .iter()
        .find(|m| m.role == Role::Assistant)
        .unwrap();
    match &assistant.content[0] {
        Content::ToolCall {
            provider_metadata, ..
        } => assert_eq!(
            anthropic_cache_control(provider_metadata),
            Some(&ephemeral())
        ),
        other => panic!("expected tool call, got {other:?}"),
    }
    let rendered = adapter.render_request(&prompt).unwrap();
    let block = &rendered["messages"][0]["content"][0];
    assert_eq!(block["type"], "tool_use");
    assert_eq!(block["cache_control"], ephemeral());
}

/// `cache_control` is **not** emitted on a `thinking` block: Anthropic rejects
/// it there (thinking blocks are cached implicitly in a prior assistant turn).
/// Even when the canonical reasoning part carries an `anthropic.cacheControl`
/// hint, the render must omit it from the `thinking` block.
#[test]
fn messages_cache_control_not_emitted_on_thinking_block() {
    let adapter = messages::MessagesAdapter;
    let mut meta = ProviderMetadata::new();
    set_provider_metadata(&mut meta, "anthropic", "signature", "SIG-1".into());
    set_provider_metadata(&mut meta, "anthropic", "cacheControl", ephemeral());
    let reasoning = Content::Reasoning {
        text: "thinking...".to_string(),
        provider_metadata: meta,
    };
    let block = render_content_block_via_request(&adapter, &reasoning);
    assert_eq!(block["type"], "thinking");
    // The signature still rides (continuity), but cache_control must be absent.
    assert_eq!(block["signature"], "SIG-1");
    assert!(
        block.get("cache_control").is_none(),
        "Anthropic rejects cache_control on a thinking block; it must not be emitted"
    );
}

/// System-prompt `cache_control` (the highest-value, most common Anthropic cache
/// point) round-trips: a `cache_control` on the `system` block array is lifted
/// into `system_provider_metadata` on parse and re-rendered as a cached
/// `[{type:"text", text, cache_control}]` system block.
#[test]
fn messages_system_cache_control_round_trips() {
    let adapter = messages::MessagesAdapter;
    let body = serde_json::json!({
        "model": "claude-3-5-sonnet",
        "max_tokens": 64,
        "system": [{
            "type": "text",
            "text": "You are a long, cacheable system prompt.",
            "cache_control": { "type": "ephemeral" },
        }],
        "messages": [{ "role": "user", "content": "hi" }],
    });
    let prompt = adapter.parse_request(body).unwrap();
    // The system text is collapsed, and the cache breakpoint is captured.
    assert_eq!(
        prompt.system.as_deref(),
        Some("You are a long, cacheable system prompt.")
    );
    assert_eq!(
        anthropic_cache_control(&prompt.system_provider_metadata),
        Some(&ephemeral())
    );
    // It re-renders as a cached array-form system block (not a bare string).
    let rendered = adapter.render_request(&prompt).unwrap();
    let system = &rendered["system"];
    assert!(
        system.is_array(),
        "system with a cache breakpoint renders as an array"
    );
    assert_eq!(system[0]["type"], "text");
    assert_eq!(
        system[0]["text"],
        "You are a long, cacheable system prompt."
    );
    assert_eq!(system[0]["cache_control"], ephemeral());
}

/// A plain-string system prompt (no breakpoint) still renders as a bare string —
/// the array form is reserved for the cached case.
#[test]
fn messages_system_without_cache_control_renders_as_string() {
    let adapter = messages::MessagesAdapter;
    let body = serde_json::json!({
        "model": "claude-3-5-sonnet",
        "max_tokens": 64,
        "system": "plain system",
        "messages": [{ "role": "user", "content": "hi" }],
    });
    let prompt = adapter.parse_request(body).unwrap();
    assert!(prompt.system_provider_metadata.is_empty());
    let rendered = adapter.render_request(&prompt).unwrap();
    assert_eq!(rendered["system"], "plain system");
}

/// The OpenAI image `detail` hint round-trips through **Responses** symmetrically
/// with Chat Completions: parsed off an `input_image` part under
/// `provider_metadata["openai"]["detail"]` and rendered back onto `input_image`.
#[test]
fn responses_image_detail_round_trips_via_provider_metadata() {
    let adapter = responses::ResponsesAdapter;
    let body = serde_json::json!({
        "model": "gpt-5",
        "input": [{
            "type": "message",
            "role": "user",
            "content": [{
                "type": "input_image",
                "image_url": format!("data:image/png;base64,{IMG_B64}"),
                "detail": "high",
            }],
        }],
    });
    let prompt = adapter.parse_request(body).unwrap();
    match &prompt.messages[0].content[0] {
        Content::File {
            provider_metadata, ..
        } => assert_eq!(
            provider_metadata
                .get("openai")
                .and_then(|o| o.get("detail")),
            Some(&serde_json::Value::String("high".into()))
        ),
        other => panic!("expected file content, got {other:?}"),
    }
    let rendered = adapter.render_request(&prompt).unwrap();
    // The rendered input carries the image part with its `detail` restored.
    let image = rendered["input"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|item| item["content"].as_array().cloned().unwrap_or_default())
        .find(|p| p["type"] == "input_image")
        .expect("an input_image part was rendered");
    assert_eq!(image["detail"], "high");
}

/// The OpenAI `detail` hint survives a Chat Completions → Responses hop: both
/// speak the `openai` namespace, so the hint is expressed natively on the
/// Responses `input_image` (unlike the cross-provider Anthropic case where it is
/// preserved-but-not-expressed).
#[test]
fn openai_image_detail_crosses_chat_to_responses() {
    let inbound = chat_completions::ChatCompletionsAdapter;
    let outbound = responses::ResponsesAdapter;
    let body = serde_json::json!({
        "model": "gpt-4o",
        "messages": [{
            "role": "user",
            "content": [{
                "type": "image_url",
                "image_url": { "url": "https://example.invalid/a.png", "detail": "low" },
            }],
        }],
    });
    let prompt = inbound.parse_request(body).unwrap();
    let rendered = outbound.render_request(&prompt).unwrap();
    let image = rendered["input"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|item| item["content"].as_array().cloned().unwrap_or_default())
        .find(|p| p["type"] == "input_image")
        .expect("an input_image part was rendered");
    assert_eq!(
        image["detail"], "low",
        "the OpenAI detail hint is expressed natively on a same-namespace Responses hop"
    );
}

/// Gemini `thoughtSignature` round-trips on a **functionCall** part (not just the
/// thinking-part path): a tool call that continues a reasoning chain carries the
/// signature, and it must reappear on the rendered `functionCall` part so a
/// follow-up turn can replay the chain.
#[test]
fn generate_content_thought_signature_round_trips_on_function_call() {
    let adapter = generate_content::GenerateContentAdapter;
    let response = serde_json::json!({
        "candidates": [{
            "content": { "role": "model", "parts": [
                {
                    "functionCall": { "name": "get_weather", "args": { "city": "SF" } },
                    "thoughtSignature": "TS-fc-1",
                },
            ] },
            "finishReason": "STOP",
            "index": 0,
        }],
        "usageMetadata": { "promptTokenCount": 1, "candidatesTokenCount": 1, "totalTokenCount": 2 },
    });
    let result = adapter.parse_response(response).unwrap();
    match &result.content[0] {
        Content::ToolCall {
            name,
            provider_metadata,
            ..
        } => {
            assert_eq!(name, "get_weather");
            assert_eq!(
                provider_metadata
                    .get("google")
                    .and_then(|g| g.get("thoughtSignature")),
                Some(&serde_json::Value::String("TS-fc-1".into()))
            );
        }
        other => panic!("expected tool call, got {other:?}"),
    }
    // Re-render to a request: the signature reappears on the functionCall part.
    let prompt = Prompt {
        model: "gemini-2.0-flash".to_string(),
        system: None,
        system_provider_metadata: Default::default(),
        messages: vec![Message {
            role: Role::Assistant,
            content: vec![result.content[0].clone()],
        }],
        tools: vec![],
        params: GenerationParams::default(),
        response_format: None,
        tool_choice: None,
        stream: false,
    };
    let rendered = adapter.render_request(&prompt).unwrap();
    let part = &rendered["contents"][0]["parts"][0];
    assert!(part.get("functionCall").is_some());
    assert_eq!(part["thoughtSignature"], "TS-fc-1");
}

// ===== tool-approval flow (human-in-the-loop) + execution-denied =====
//
// Only OpenAI Responses carries the approval handshake on the wire
// (`mcp_approval_request` output item / `mcp_approval_response` input item); the
// other three protocols have no approval item and drop the parts on render. The
// `execution-denied` tool-result output is the denial leg of that flow.

/// A Responses `mcp_approval_request` output item parses into a
/// `Content::ToolApprovalRequest` whose `approval_id` is the item id, with the
/// MCP server identity lifted into `provider_metadata["openai"]`, and renders
/// back to the identical output item (same-protocol round trip).
#[test]
fn responses_mcp_approval_request_round_trips() {
    let adapter = adapter_for(ApiProtocol::Responses);
    let body = serde_json::json!({
        "id": "resp_1",
        "status": "completed",
        "output": [
            {
                "id": "mcpr_abc",
                "type": "mcp_approval_request",
                "server_label": "dmcp",
                "name": "roll",
                "arguments": "{\"diceRollExpression\":\"2d4 + 1\"}",
            }
        ]
    });
    let result = adapter.parse_response(body).unwrap();
    // Parsed into a ToolApprovalRequest carrying the server identity.
    let (approval_id, tool_call_id, meta) = result
        .content
        .iter()
        .find_map(|c| match c {
            Content::ToolApprovalRequest {
                approval_id,
                tool_call_id,
                provider_metadata,
            } => Some((
                approval_id.as_str(),
                tool_call_id.as_str(),
                provider_metadata,
            )),
            _ => None,
        })
        .expect("an approval request part");
    assert_eq!(approval_id, "mcpr_abc");
    // The wire carries no separate tool-call id; it is synthesized deterministically.
    assert_eq!(tool_call_id, "approval:mcpr_abc");
    let openai = meta.get("openai").and_then(|o| o.as_object()).unwrap();
    assert_eq!(openai["serverLabel"], "dmcp");
    assert_eq!(openai["name"], "roll");
    assert_eq!(openai["arguments"], "{\"diceRollExpression\":\"2d4 + 1\"}");

    // Render back: the exact `mcp_approval_request` output item reappears.
    let rendered = adapter
        .render_response(&result, &sample_prompt(), "resp_1")
        .unwrap();
    let item = rendered["output"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["type"] == "mcp_approval_request")
        .expect("mcp_approval_request re-emitted");
    assert_eq!(item["id"], "mcpr_abc");
    assert_eq!(item["server_label"], "dmcp");
    assert_eq!(item["name"], "roll");
    assert_eq!(item["arguments"], "{\"diceRollExpression\":\"2d4 + 1\"}");
    // Single-id source: no distinct `itemId` was stored, so the render must NOT
    // add a redundant `approval_request_id` (the `id` alone conveys it).
    assert!(
        item.get("approval_request_id").is_none(),
        "single-id approval item must not gain a redundant approval_request_id: {item}"
    );
    let openai = meta.get("openai").and_then(|o| o.as_object()).unwrap();
    assert!(
        !openai.contains_key("itemId"),
        "coinciding id must not be stored as itemId: {openai:?}"
    );
}

/// A Responses `mcp_approval_request` with an `approval_request_id` *distinct*
/// from its item `id` uses the former as the approval/correlation id, preserves
/// the latter under `provider_metadata["openai"]["itemId"]`, and re-emits BOTH on
/// render so the two-id form round-trips losslessly (mirroring the AI SDK
/// reference's `approval_request_id ?? id` choice while not dropping the raw id).
#[test]
fn responses_mcp_approval_request_prefers_approval_request_id() {
    let adapter = adapter_for(ApiProtocol::Responses);
    let body = serde_json::json!({
        "id": "resp_1",
        "status": "completed",
        "output": [
            {
                "id": "item_xyz",
                "approval_request_id": "mcpr_real",
                "type": "mcp_approval_request",
                "name": "roll",
                "server_label": "dmcp",
            }
        ]
    });
    let result = adapter.parse_response(body).unwrap();
    let (approval_id, meta) = result
        .content
        .iter()
        .find_map(|c| match c {
            Content::ToolApprovalRequest {
                approval_id,
                provider_metadata,
                ..
            } => Some((approval_id.as_str(), provider_metadata)),
            _ => None,
        })
        .unwrap();
    // `approval_request_id` wins over the item `id` as the correlation key.
    assert_eq!(approval_id, "mcpr_real");
    // The distinct raw item id is preserved, not dropped.
    let openai = meta.get("openai").and_then(|o| o.as_object()).unwrap();
    assert_eq!(openai["itemId"], "item_xyz");

    // Render back: both ids reappear on the item (`id` = raw item id,
    // `approval_request_id` = correlation key), so the original round-trips.
    let rendered = adapter
        .render_response(&result, &sample_prompt(), "resp_1")
        .unwrap();
    let item = rendered["output"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["type"] == "mcp_approval_request")
        .expect("mcp_approval_request re-emitted");
    assert_eq!(item["id"], "item_xyz");
    assert_eq!(item["approval_request_id"], "mcpr_real");
    assert_eq!(item["server_label"], "dmcp");
    assert_eq!(item["name"], "roll");
}

/// An approved `mcp_approval_response` input item round-trips through Responses as
/// a `Content::ToolApprovalResponse` (and emits NO paired execution-denied).
#[test]
fn responses_mcp_approval_response_approved_round_trips() {
    let adapter = adapter_for(ApiProtocol::Responses);
    let body = serde_json::json!({
        "model": "gpt-5",
        "input": [
            {
                "type": "mcp_approval_response",
                "approval_request_id": "mcpr_abc",
                "approve": true,
            }
        ]
    });
    let prompt = adapter.parse_request(body).unwrap();
    let parts: Vec<&Content> = prompt.messages.iter().flat_map(|m| &m.content).collect();
    // Exactly the approval-response part; no execution-denied result on approval.
    assert_eq!(
        parts.len(),
        1,
        "approved response yields only the approval part"
    );
    match parts[0] {
        Content::ToolApprovalResponse {
            approval_id,
            approved,
            reason,
            ..
        } => {
            assert_eq!(approval_id, "mcpr_abc");
            assert!(*approved);
            assert!(reason.is_none(), "the wire carries no reason");
        }
        other => panic!("expected approval response, got {other:?}"),
    }

    // Render back to the identical input item.
    let rendered = adapter.render_request(&prompt).unwrap();
    let item = rendered["input"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["type"] == "mcp_approval_response")
        .expect("mcp_approval_response re-emitted");
    assert_eq!(item["approval_request_id"], "mcpr_abc");
    assert_eq!(item["approve"], true);
}

/// A denied `mcp_approval_response` parses into the approval-response part PLUS a
/// paired `ExecutionDenied` tool result (carrying the approval id), and renders
/// back to a single `mcp_approval_response` — the denial is NOT duplicated as a
/// `function_call_output`, matching the AI SDK's execution-denied skip rule.
#[test]
fn responses_mcp_approval_response_denied_pairs_execution_denied() {
    let adapter = adapter_for(ApiProtocol::Responses);
    let body = serde_json::json!({
        "model": "gpt-5",
        "input": [
            {
                "type": "mcp_approval_response",
                "approval_request_id": "mcpr_abc",
                "approve": false,
            }
        ]
    });
    let prompt = adapter.parse_request(body).unwrap();
    let parts: Vec<&Content> = prompt.messages.iter().flat_map(|m| &m.content).collect();
    assert_eq!(
        parts.len(),
        2,
        "denial yields the approval part + a denied result"
    );
    // The approval-response part records the denial.
    assert!(matches!(
        parts[0],
        Content::ToolApprovalResponse {
            approved: false,
            ..
        }
    ));
    // The paired tool result is an ExecutionDenied carrying the approval id.
    match parts[1] {
        Content::ToolResult {
            call_id,
            output,
            provider_metadata,
            ..
        } => {
            assert_eq!(call_id, "mcpr_abc");
            assert!(matches!(
                output,
                ToolResultOutput::ExecutionDenied { reason: None }
            ));
            assert_eq!(
                provider_metadata["openai"]["approvalId"],
                serde_json::json!("mcpr_abc"),
                "the denial carries its approval id so render can skip it",
            );
        }
        other => panic!("expected a tool result, got {other:?}"),
    }

    // Render: only the `mcp_approval_response` is emitted; the approval-paired
    // execution-denied is skipped (no duplicate `function_call_output`).
    let rendered = adapter.render_request(&prompt).unwrap();
    let input = rendered["input"].as_array().unwrap();
    let approval_items = input
        .iter()
        .filter(|i| i["type"] == "mcp_approval_response")
        .count();
    let denial_outputs = input
        .iter()
        .filter(|i| i["type"] == "function_call_output")
        .count();
    assert_eq!(
        approval_items, 1,
        "the denial is conveyed by mcp_approval_response"
    );
    assert_eq!(
        denial_outputs, 0,
        "an approval-paired execution-denied is not also emitted as a string: {rendered}"
    );
}

/// A full approve handshake: the assistant's `mcp_approval_request` is answered
/// by an approved `mcp_approval_response`, and the tool's actual output rides as
/// an ordinary `function_call_output`. Both legs survive a same-protocol route.
#[test]
fn responses_approval_then_tool_runs_full_handshake() {
    let adapter = adapter_for(ApiProtocol::Responses);
    let body = serde_json::json!({
        "model": "gpt-5",
        "input": [
            {
                "type": "mcp_approval_response",
                "approval_request_id": "mcpr_abc",
                "approve": true,
            },
            {
                "type": "function_call_output",
                "call_id": "mcpr_abc",
                "output": "4",
            }
        ]
    });
    let prompt = adapter.parse_request(body).unwrap();
    let rendered = adapter.render_request(&prompt).unwrap();
    let input = rendered["input"].as_array().unwrap();
    // The approval grant is preserved.
    assert!(input.iter().any(|i| i["type"] == "mcp_approval_response"
        && i["approve"] == true
        && i["approval_request_id"] == "mcpr_abc"));
    // The tool's real result is preserved as a function_call_output.
    let out = input
        .iter()
        .find(|i| i["type"] == "function_call_output")
        .expect("the approved tool's output survives");
    assert_eq!(out["call_id"], "mcpr_abc");
    assert_eq!(out["output"], "4");
}

/// An *unpaired* `ExecutionDenied` (no approval id in metadata) degrades to a
/// `function_call_output` whose `output` is the denial string on Responses, and
/// re-parses as `Text` — exactly as documented (the structured denial has no
/// distinct tag on the wire without an approval).
#[test]
fn responses_unpaired_execution_denied_degrades_to_string() {
    let adapter = adapter_for(ApiProtocol::Responses);
    let prompt = tool_result_prompt(
        "call_1",
        None,
        ToolResultOutput::ExecutionDenied {
            reason: Some("user refused".to_string()),
        },
    );
    let rendered = adapter.render_request(&prompt).unwrap();
    let out = rendered["input"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["type"] == "function_call_output")
        .expect("an unpaired denial renders as function_call_output");
    assert_eq!(out["output"], "user refused");
    // Re-parses as Text (the wire has no execution-denied tag without an approval).
    let back = round_trip_tool_output(
        ApiProtocol::Responses,
        ToolResultOutput::ExecutionDenied {
            reason: Some("user refused".to_string()),
        },
    );
    assert_eq!(
        back,
        ToolResultOutput::Text {
            value: "user refused".to_string()
        }
    );
}

/// An unpaired `ExecutionDenied` with no reason renders the AI SDK's default
/// denial sentinel.
#[test]
fn responses_execution_denied_without_reason_uses_default_sentinel() {
    let adapter = adapter_for(ApiProtocol::Responses);
    let prompt = tool_result_prompt(
        "call_1",
        None,
        ToolResultOutput::ExecutionDenied { reason: None },
    );
    let rendered = adapter.render_request(&prompt).unwrap();
    let out = rendered["input"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["type"] == "function_call_output")
        .unwrap();
    assert_eq!(out["output"], "Tool call execution denied.");
}

/// On the three non-Responses wires there is no approval handshake, so an
/// `ExecutionDenied` tool result degrades to the denial string (or the default
/// sentinel) via `to_provider_string`, never silently dropped.
#[test]
fn execution_denied_degrades_to_string_on_non_responses_wires() {
    for protocol in [
        ApiProtocol::ChatCompletions,
        ApiProtocol::Messages,
        ApiProtocol::GenerateContent,
    ] {
        let adapter = adapter_for(protocol.clone());
        let prompt = tool_result_prompt(
            "call_1",
            Some("roll"),
            ToolResultOutput::ExecutionDenied {
                reason: Some("denied by policy".to_string()),
            },
        );
        let rendered = adapter
            .render_request(&prompt)
            .unwrap_or_else(|e| panic!("{protocol:?} render_request: {e}"));
        // The denial reason text must appear somewhere in the rendered request.
        let blob = serde_json::to_string(&rendered).unwrap();
        assert!(
            blob.contains("denied by policy"),
            "{protocol:?} dropped the execution-denied reason: {rendered}"
        );
    }
}

/// The three non-Responses adapters drop a `ToolApprovalResponse` request part
/// (no wire item for it), mirroring the AI SDK converters' `continue`. The
/// request must still render successfully (no panic, no error).
#[test]
fn approval_response_part_is_dropped_on_non_responses_wires() {
    for protocol in [
        ApiProtocol::ChatCompletions,
        ApiProtocol::Messages,
        ApiProtocol::GenerateContent,
    ] {
        let adapter = adapter_for(protocol.clone());
        let prompt = Prompt {
            model: "m".to_string(),
            system: None,
            system_provider_metadata: Default::default(),
            messages: vec![Message {
                role: Role::Tool,
                content: vec![Content::ToolApprovalResponse {
                    approval_id: "mcpr_abc".to_string(),
                    approved: true,
                    reason: None,
                    provider_metadata: Default::default(),
                }],
            }],
            tools: vec![],
            params: GenerationParams::default(),
            response_format: None,
            tool_choice: None,
            stream: false,
        };
        // Renders without error; the approval part has no wire item and is dropped.
        adapter
            .render_request(&prompt)
            .unwrap_or_else(|e| panic!("{protocol:?} render_request: {e}"));
    }
}

/// The new content variants and the `ExecutionDenied` output round-trip through
/// serde unchanged, with the expected tagged-union shapes.
#[test]
fn approval_types_serde_round_trip() {
    // ToolApprovalRequest
    let req = Content::ToolApprovalRequest {
        approval_id: "mcpr_abc".to_string(),
        tool_call_id: "approval:mcpr_abc".to_string(),
        provider_metadata: Default::default(),
    };
    let v = serde_json::to_value(&req).unwrap();
    assert_eq!(v["type"], "tool_approval_request");
    assert_eq!(v["approval_id"], "mcpr_abc");
    assert_eq!(v["tool_call_id"], "approval:mcpr_abc");
    assert_eq!(serde_json::from_value::<Content>(v).unwrap(), req);

    // ToolApprovalResponse, with an optional reason set.
    let resp = Content::ToolApprovalResponse {
        approval_id: "mcpr_abc".to_string(),
        approved: false,
        reason: Some("nope".to_string()),
        provider_metadata: Default::default(),
    };
    let v = serde_json::to_value(&resp).unwrap();
    assert_eq!(v["type"], "tool_approval_response");
    assert_eq!(v["approved"], false);
    assert_eq!(v["reason"], "nope");
    assert_eq!(serde_json::from_value::<Content>(v).unwrap(), resp);

    // ExecutionDenied output, reason omitted when None.
    let denied = ToolResultOutput::ExecutionDenied { reason: None };
    let v = serde_json::to_value(&denied).unwrap();
    assert_eq!(v["type"], "execution_denied");
    assert!(v.get("reason").is_none(), "None reason omits the key: {v}");
    assert_eq!(
        serde_json::from_value::<ToolResultOutput>(v).unwrap(),
        denied
    );
}

// ===== raw finish-reason preservation (V3 `finishReason.raw`) =====
//
// The canonical `FinishReason` enum maps several native reasons onto one
// variant; for the lossy cases an adapter stashes the raw provider string under
// `provider_metadata["<provider>"]["rawFinishReason"]` on parse and reads it
// back on render so a same-protocol round-trip is byte-faithful. These tests
// pin both halves (parse stashes, render restores) plus the negative case
// (lossless reasons stash nothing).

/// The `rawFinishReason` value stashed under `provider_id`, if any.
fn raw_finish(result: &GenerateResult, provider_id: &str) -> Option<String> {
    result
        .provider_metadata
        .get(provider_id)?
        .as_object()?
        .get("rawFinishReason")?
        .as_str()
        .map(str::to_string)
}

#[test]
fn messages_stop_sequence_raw_round_trips() {
    let adapter = adapter_for(ApiProtocol::Messages);
    // `stop_sequence` maps to the unified `Stop`, which would otherwise render
    // back as `end_turn` and lose the distinction.
    let body = serde_json::json!({
        "id": "msg_1",
        "type": "message",
        "role": "assistant",
        "model": "claude-opus-4-7",
        "content": [{"type": "text", "text": "hi"}],
        "stop_reason": "stop_sequence",
        "usage": {"input_tokens": 1, "output_tokens": 1},
    });
    let result = adapter.parse_response(body).unwrap();
    assert_eq!(result.finish_reason, Some(FinishReason::Stop));
    assert_eq!(
        raw_finish(&result, PROVIDER_ID_ANTHROPIC).as_deref(),
        Some("stop_sequence"),
        "lossy stop_reason is stashed for faithful re-render"
    );
    // Render restores the exact native value rather than the enum's `end_turn`.
    let rendered = adapter
        .render_response(&result, &sample_prompt(), "msg_1")
        .unwrap();
    assert_eq!(rendered["stop_reason"], "stop_sequence");
}

#[test]
fn messages_end_turn_raw_is_not_stashed() {
    let adapter = adapter_for(ApiProtocol::Messages);
    // `end_turn` → `Stop` → renders `end_turn`: lossless, so nothing is stashed.
    let body = serde_json::json!({
        "id": "msg_1",
        "type": "message",
        "role": "assistant",
        "model": "claude-opus-4-7",
        "content": [{"type": "text", "text": "hi"}],
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 1, "output_tokens": 1},
    });
    let result = adapter.parse_response(body).unwrap();
    assert_eq!(result.finish_reason, Some(FinishReason::Stop));
    assert!(
        raw_finish(&result, PROVIDER_ID_ANTHROPIC).is_none(),
        "a lossless reason stashes no raw value"
    );
    let rendered = adapter
        .render_response(&result, &sample_prompt(), "msg_1")
        .unwrap();
    assert_eq!(rendered["stop_reason"], "end_turn");
}

#[test]
fn generate_content_recitation_raw_round_trips() {
    let adapter = adapter_for(ApiProtocol::GenerateContent);
    // `RECITATION` collapses to `ContentFilter`, which renders back as `SAFETY`;
    // the precise sub-reason must survive via the stash.
    let body = serde_json::json!({
        "candidates": [{
            "content": {"role": "model", "parts": [{"text": "hi"}]},
            "finishReason": "RECITATION",
            "index": 0,
        }],
        "usageMetadata": {"promptTokenCount": 1, "candidatesTokenCount": 1, "totalTokenCount": 2},
    });
    let result = adapter.parse_response(body).unwrap();
    assert_eq!(result.finish_reason, Some(FinishReason::ContentFilter));
    assert_eq!(
        raw_finish(&result, PROVIDER_ID_GOOGLE).as_deref(),
        Some("RECITATION")
    );
    let rendered = adapter
        .render_response(&result, &sample_prompt(), "r1")
        .unwrap();
    assert_eq!(rendered["candidates"][0]["finishReason"], "RECITATION");
}

#[test]
fn generate_content_stop_raw_is_not_stashed() {
    let adapter = adapter_for(ApiProtocol::GenerateContent);
    let body = serde_json::json!({
        "candidates": [{
            "content": {"role": "model", "parts": [{"text": "hi"}]},
            "finishReason": "STOP",
            "index": 0,
        }],
        "usageMetadata": {"promptTokenCount": 1, "candidatesTokenCount": 1, "totalTokenCount": 2},
    });
    let result = adapter.parse_response(body).unwrap();
    assert_eq!(result.finish_reason, Some(FinishReason::Stop));
    assert!(raw_finish(&result, PROVIDER_ID_GOOGLE).is_none());
    let rendered = adapter
        .render_response(&result, &sample_prompt(), "r1")
        .unwrap();
    assert_eq!(rendered["candidates"][0]["finishReason"], "STOP");
}

#[test]
fn chat_completions_function_call_raw_round_trips() {
    let adapter = adapter_for(ApiProtocol::ChatCompletions);
    // The legacy `function_call` reason maps to `ToolCalls`, which renders back
    // as `tool_calls`; the original string must survive via the stash.
    let body = serde_json::json!({
        "id": "chatcmpl-1",
        "object": "chat.completion",
        "model": "gpt-4o",
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": "hi"},
            "finish_reason": "function_call",
        }],
        "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2},
    });
    let result = adapter.parse_response(body).unwrap();
    assert_eq!(result.finish_reason, Some(FinishReason::ToolCalls));
    assert_eq!(
        raw_finish(&result, PROVIDER_ID_OPENAI).as_deref(),
        Some("function_call")
    );
    let rendered = adapter
        .render_response(&result, &sample_prompt(), "chatcmpl-1")
        .unwrap();
    assert_eq!(rendered["choices"][0]["finish_reason"], "function_call");
}

#[test]
fn chat_completions_stop_raw_is_not_stashed() {
    let adapter = adapter_for(ApiProtocol::ChatCompletions);
    let body = serde_json::json!({
        "id": "chatcmpl-1",
        "object": "chat.completion",
        "model": "gpt-4o",
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": "hi"},
            "finish_reason": "stop",
        }],
        "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2},
    });
    let result = adapter.parse_response(body).unwrap();
    assert_eq!(result.finish_reason, Some(FinishReason::Stop));
    assert!(raw_finish(&result, PROVIDER_ID_OPENAI).is_none());
    let rendered = adapter
        .render_response(&result, &sample_prompt(), "chatcmpl-1")
        .unwrap();
    assert_eq!(rendered["choices"][0]["finish_reason"], "stop");
}

// ===== response_format `description` (V3 `responseFormat.description`) =====

#[test]
fn responses_inbound_promotes_json_schema_description() {
    let adapter = adapter_for(ApiProtocol::Responses);
    let body = serde_json::json!({
        "model": "gpt-4o",
        "input": "weather?",
        "text": {
            "format": {
                "type": "json_schema",
                "name": "weather",
                "description": "Structured weather report",
                "schema": {"type": "object"},
            }
        }
    });
    let prompt = adapter.parse_request(body).unwrap();
    match prompt.response_format {
        Some(ResponseFormat::JsonSchema {
            name, description, ..
        }) => {
            assert_eq!(name.as_deref(), Some("weather"));
            assert_eq!(description.as_deref(), Some("Structured weather report"));
        }
        other => panic!("expected JsonSchema, got {other:?}"),
    }
}

#[test]
fn responses_outbound_renders_json_schema_description() {
    let adapter = adapter_for(ApiProtocol::Responses);
    let prompt = Prompt {
        response_format: Some(ResponseFormat::JsonSchema {
            name: Some("weather".to_string()),
            description: Some("Structured weather report".to_string()),
            strict: None,
            schema: serde_json::json!({"type": "object"}),
        }),
        ..sample_prompt()
    };
    let rendered = adapter.render_request(&prompt).unwrap();
    assert_eq!(
        rendered["text"]["format"]["description"],
        "Structured weather report"
    );
}

#[test]
fn chat_completions_outbound_renders_json_schema_description() {
    let adapter = adapter_for(ApiProtocol::ChatCompletions);
    let prompt = Prompt {
        response_format: Some(ResponseFormat::JsonSchema {
            name: Some("weather".to_string()),
            description: Some("Structured weather report".to_string()),
            strict: None,
            schema: serde_json::json!({"type": "object"}),
        }),
        ..sample_prompt()
    };
    let rendered = adapter.render_request(&prompt).unwrap();
    assert_eq!(
        rendered["response_format"]["json_schema"]["description"],
        "Structured weather report"
    );
}

#[test]
fn json_schema_description_round_trips_openai_family() {
    // A description survives a same-protocol render→parse hop on both OpenAI
    // wires (Chat Completions and Responses), and is dropped on the
    // Anthropic/Google wires that carry no schema description.
    for proto in [ApiProtocol::ChatCompletions, ApiProtocol::Responses] {
        let adapter = adapter_for(proto.clone());
        let prompt = Prompt {
            response_format: Some(ResponseFormat::JsonSchema {
                name: Some("weather".to_string()),
                description: Some("Structured weather report".to_string()),
                strict: None,
                schema: serde_json::json!({"type": "object"}),
            }),
            ..sample_prompt()
        };
        let rendered = adapter.render_request(&prompt).unwrap();
        let back = adapter.parse_request(rendered).unwrap();
        match back.response_format {
            Some(ResponseFormat::JsonSchema { description, .. }) => {
                assert_eq!(
                    description.as_deref(),
                    Some("Structured weather report"),
                    "{proto:?} should round-trip the description"
                );
            }
            other => panic!("{proto:?}: expected JsonSchema, got {other:?}"),
        }
    }
}

// ===== regression: empty-string continuation tool-name fragments tool calls =====
//
// Some OpenAI-compatible upstreams (e.g. Kimi / DeepSeek serving stacks that
// emit `functions.<name>:<idx>` tool ids) re-send `"function":{"name":""}` on
// every argument-continuation chunk. Treating that empty name as a *new* tool
// call fragmented one call into one broken `function_call` item per delta
// (item 0: real name + empty args; the rest: empty name + a partial-args
// fragment), so a Responses-API client rejected every item ("unsupported call"
// / "EOF while parsing function arguments") and looped forever.

/// Encode a canonical stream into Responses SSE and return the `function_call`
/// items present in the terminal `response.completed.output` array.
fn responses_encode_tool_items(parts: &[StreamPart]) -> Vec<serde_json::Value> {
    let adapter = adapter_for(ApiProtocol::Responses);
    let mut encoder = adapter.stream_encoder("resp_repro", "kimi");
    let mut frames = Vec::new();
    for p in parts {
        frames.extend(encoder.encode(p).unwrap());
    }
    let completed = frames
        .iter()
        .find_map(|f| match f {
            SseFrame::Event { event, data } if event.as_deref() == Some("response.completed") => {
                Some(serde_json::from_str::<serde_json::Value>(data).unwrap())
            }
            _ => None,
        })
        .expect("response.completed present");
    completed["response"]["output"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|i| i["type"] == "function_call")
        .cloned()
        .collect()
}

/// Decoder normalizes an empty continuation `name` to `None` so the canonical
/// IR never carries `Some("")` as a (non-)name announcement.
#[test]
fn chat_decode_empty_continuation_name_normalized_to_none() {
    let parts = decode_stream(
        ApiProtocol::ChatCompletions,
        &[
            (
                "",
                serde_json::json!({"choices":[{"delta":{"tool_calls":[
                    {"index":0,"id":"functions.exec_command:0","type":"function",
                     "function":{"name":"exec_command","arguments":""}}]}}]}),
            ),
            (
                "",
                serde_json::json!({"choices":[{"delta":{"tool_calls":[
                    {"index":0,"id":"functions.exec_command:0","type":"function",
                     "function":{"name":"","arguments":"{\"cmd\":\""}}]}}]}),
            ),
            (
                "",
                serde_json::json!({"choices":[{"delta":{"tool_calls":[
                    {"index":0,"id":"functions.exec_command:0","type":"function",
                     "function":{"name":"","arguments":"ls\"}"}}]}}]}),
            ),
            (
                "",
                serde_json::json!({"choices":[{"delta":{},"finish_reason":"tool_calls"}]}),
            ),
        ],
    );
    let names: Vec<Option<String>> = parts
        .iter()
        .filter_map(|p| match p {
            StreamPart::ToolCallDelta { name, .. } => Some(name.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(names[0].as_deref(), Some("exec_command"));
    assert_eq!(names[1], None, "empty continuation name normalized to None");
    assert_eq!(names[2], None, "empty continuation name normalized to None");
}

/// End-to-end (chat upstream → Responses client): an empty-name continuation
/// stream re-encodes to exactly ONE function_call item with the full arguments.
#[test]
fn responses_encode_empty_continuation_name_single_tool_call() {
    let parts = decode_stream(
        ApiProtocol::ChatCompletions,
        &[
            (
                "",
                serde_json::json!({"choices":[{"delta":{"tool_calls":[
                    {"index":0,"id":"functions.exec_command:0","type":"function",
                     "function":{"name":"exec_command","arguments":""}}]}}]}),
            ),
            (
                "",
                serde_json::json!({"choices":[{"delta":{"tool_calls":[
                    {"index":0,"id":"functions.exec_command:0","type":"function",
                     "function":{"name":"","arguments":"{\"cmd\":\""}}]}}]}),
            ),
            (
                "",
                serde_json::json!({"choices":[{"delta":{"tool_calls":[
                    {"index":0,"id":"functions.exec_command:0","type":"function",
                     "function":{"name":"","arguments":"ls\"}"}}]}}]}),
            ),
            (
                "",
                serde_json::json!({"choices":[{"delta":{},"finish_reason":"tool_calls"}]}),
            ),
        ],
    );
    let items = responses_encode_tool_items(&parts);
    assert_eq!(
        items.len(),
        1,
        "one tool call, not one item per delta: {items:?}"
    );
    assert_eq!(items[0]["name"], "exec_command");
    assert_eq!(items[0]["call_id"], "functions.exec_command:0");
    assert_eq!(items[0]["arguments"], "{\"cmd\":\"ls\"}");
}

/// Encoder defense-in-depth: even if a `Some("")` continuation name reaches the
/// Responses encoder directly (any protocol decoder / future upstream), it is
/// treated as a continuation, not a new call.
#[test]
fn responses_encoder_treats_empty_name_delta_as_continuation() {
    let id = "functions.exec_command:0".to_string();
    let parts = vec![
        StreamPart::ToolCallDelta {
            id: id.clone(),
            name: Some("exec_command".into()),
            arguments: "".into(),
        },
        StreamPart::ToolCallDelta {
            id: id.clone(),
            name: Some("".into()),
            arguments: "{\"cmd\":\"".into(),
        },
        StreamPart::ToolCallDelta {
            id: id.clone(),
            name: Some("".into()),
            arguments: "ls\"}".into(),
        },
        StreamPart::Finish {
            reason: FinishReason::ToolCalls,
        },
    ];
    let items = responses_encode_tool_items(&parts);
    assert_eq!(
        items.len(),
        1,
        "empty-name deltas must not open new items: {items:?}"
    );
    assert_eq!(items[0]["name"], "exec_command");
    assert_eq!(items[0]["arguments"], "{\"cmd\":\"ls\"}");
}

/// The empty-name continuation chunks used by the Codex repro, reused to prove
/// the Anthropic (Messages) and Gemini encoder paths behave the same way.
fn empty_continuation_name_chat_events() -> Vec<(&'static str, serde_json::Value)> {
    vec![
        (
            "",
            serde_json::json!({"choices":[{"delta":{"tool_calls":[
                {"index":0,"id":"functions.exec_command:0","type":"function",
                 "function":{"name":"exec_command","arguments":""}}]}}]}),
        ),
        (
            "",
            serde_json::json!({"choices":[{"delta":{"tool_calls":[
                {"index":0,"id":"functions.exec_command:0","type":"function",
                 "function":{"name":"","arguments":"{\"cmd\":\""}}]}}]}),
        ),
        (
            "",
            serde_json::json!({"choices":[{"delta":{"tool_calls":[
                {"index":0,"id":"functions.exec_command:0","type":"function",
                 "function":{"name":"","arguments":"ls\"}"}}]}}]}),
        ),
        (
            "",
            serde_json::json!({"choices":[{"delta":{},"finish_reason":"tool_calls"}]}),
        ),
    ]
}

/// Encode a canonical stream as Anthropic Messages SSE and return one
/// `(name, accumulated_input_json)` per `tool_use` block (keyed by block index).
fn messages_encode_tool_blocks(parts: &[StreamPart]) -> Vec<(String, String)> {
    let adapter = adapter_for(ApiProtocol::Messages);
    let mut encoder = adapter.stream_encoder("msg_repro", "claude");
    let mut frames = Vec::new();
    for p in parts {
        frames.extend(encoder.encode(p).unwrap());
    }
    use std::collections::BTreeMap;
    let mut blocks: BTreeMap<i64, (String, String)> = BTreeMap::new();
    for f in &frames {
        if let SseFrame::Event { event, data } = f {
            let v: serde_json::Value = serde_json::from_str(data).unwrap();
            match event.as_deref() {
                Some("content_block_start") if v["content_block"]["type"] == "tool_use" => {
                    let idx = v["index"].as_i64().unwrap_or(-1);
                    let name = v["content_block"]["name"]
                        .as_str()
                        .unwrap_or("")
                        .to_string();
                    blocks.entry(idx).or_insert((name, String::new()));
                }
                Some("content_block_delta") if v["delta"]["type"] == "input_json_delta" => {
                    let idx = v["index"].as_i64().unwrap_or(-1);
                    if let Some(b) = blocks.get_mut(&idx) {
                        b.1.push_str(v["delta"]["partial_json"].as_str().unwrap_or(""));
                    }
                }
                _ => {}
            }
        }
    }
    blocks.into_values().collect()
}

/// Anthropic path (e.g. headless Claude Code): an empty-name continuation
/// stream re-encodes to exactly ONE `tool_use` block with the full input,
/// rather than one empty-named block per delta.
#[test]
fn messages_encode_empty_continuation_name_single_tool_use() {
    let parts = decode_stream(
        ApiProtocol::ChatCompletions,
        &empty_continuation_name_chat_events(),
    );
    let blocks = messages_encode_tool_blocks(&parts);
    assert_eq!(
        blocks.len(),
        1,
        "one tool_use block, not one per delta: {blocks:?}"
    );
    assert_eq!(blocks[0].0, "exec_command");
    assert_eq!(blocks[0].1, "{\"cmd\":\"ls\"}");
}

/// Messages encoder defense-in-depth: a `Some("")` continuation reaching the
/// encoder directly is a continuation, not a new tool_use block.
#[test]
fn messages_encoder_treats_empty_name_delta_as_continuation() {
    let id = "functions.exec_command:0".to_string();
    let parts = vec![
        StreamPart::ToolCallDelta {
            id: id.clone(),
            name: Some("exec_command".into()),
            arguments: "".into(),
        },
        StreamPart::ToolCallDelta {
            id: id.clone(),
            name: Some("".into()),
            arguments: "{\"cmd\":\"".into(),
        },
        StreamPart::ToolCallDelta {
            id: id.clone(),
            name: Some("".into()),
            arguments: "ls\"}".into(),
        },
        StreamPart::Finish {
            reason: FinishReason::ToolCalls,
        },
    ];
    let blocks = messages_encode_tool_blocks(&parts);
    assert_eq!(
        blocks.len(),
        1,
        "empty-name deltas must not open new blocks: {blocks:?}"
    );
    assert_eq!(blocks[0].0, "exec_command");
    assert_eq!(blocks[0].1, "{\"cmd\":\"ls\"}");
}

/// Encode a canonical stream as Generate Content SSE (including the terminal
/// `finish()` flush) and return one `(name, args_json_string)` per emitted
/// `functionCall` part.
fn gemini_encode_function_calls(parts: &[StreamPart]) -> Vec<(String, String)> {
    let adapter = adapter_for(ApiProtocol::GenerateContent);
    let mut encoder = adapter.stream_encoder("g_repro", "gemini");
    let mut frames = Vec::new();
    for p in parts {
        frames.extend(encoder.encode(p).unwrap());
    }
    frames.extend(encoder.finish().unwrap());
    let mut calls = Vec::new();
    for f in &frames {
        if let SseFrame::Event { data, .. } = f {
            let v: serde_json::Value = serde_json::from_str(data).unwrap();
            if let Some(arr) = v["candidates"][0]["content"]["parts"].as_array() {
                for part in arr {
                    if let Some(fc) = part.get("functionCall") {
                        calls.push((
                            fc["name"].as_str().unwrap_or("").to_string(),
                            fc["args"].to_string(),
                        ));
                    }
                }
            }
        }
    }
    calls
}

/// Gemini path: a fragmented tool call (name on first chunk, args streamed
/// across deltas) re-encodes to ONE `functionCall` with the FULL args — the
/// encoder accumulates instead of emitting one `functionCall{args:{}}` per
/// delta.
#[test]
fn gemini_encode_fragmented_args_single_function_call() {
    let id = "call_1".to_string();
    let parts = vec![
        StreamPart::ToolCallDelta {
            id: id.clone(),
            name: Some("exec_command".into()),
            arguments: "".into(),
        },
        StreamPart::ToolCallDelta {
            id: id.clone(),
            name: None,
            arguments: "{\"cmd\":".into(),
        },
        StreamPart::ToolCallDelta {
            id: id.clone(),
            name: None,
            arguments: "\"ls\"}".into(),
        },
        StreamPart::Finish {
            reason: FinishReason::ToolCalls,
        },
    ];
    let calls = gemini_encode_function_calls(&parts);
    assert_eq!(
        calls.len(),
        1,
        "one functionCall, not one per delta: {calls:?}"
    );
    assert_eq!(calls[0].0, "exec_command");
    assert_eq!(calls[0].1, "{\"cmd\":\"ls\"}");
}

/// Gemini path: empty-name continuation chunks (upstreams that re-send
/// `name:""` every chunk) also collapse to ONE complete `functionCall`.
#[test]
fn gemini_encode_empty_continuation_name_single_function_call() {
    let parts = decode_stream(
        ApiProtocol::ChatCompletions,
        &empty_continuation_name_chat_events(),
    );
    let calls = gemini_encode_function_calls(&parts);
    assert_eq!(
        calls.len(),
        1,
        "one functionCall, not fragmented: {calls:?}"
    );
    assert_eq!(calls[0].0, "exec_command");
    assert_eq!(calls[0].1, "{\"cmd\":\"ls\"}");
}

/// Gemini path: two distinct (parallel) tool calls still emit two complete
/// `functionCall` parts — accumulation must not merge separate calls.
#[test]
fn gemini_encode_two_tool_calls_emit_two_function_calls() {
    let parts = vec![
        StreamPart::ToolCallDelta {
            id: "a".into(),
            name: Some("first".into()),
            arguments: "{\"x\":1}".into(),
        },
        StreamPart::ToolCallDelta {
            id: "b".into(),
            name: Some("second".into()),
            arguments: "{\"y\":2}".into(),
        },
        StreamPart::Finish {
            reason: FinishReason::ToolCalls,
        },
    ];
    let calls = gemini_encode_function_calls(&parts);
    assert_eq!(calls.len(), 2, "two distinct calls: {calls:?}");
    assert_eq!(calls[0].0, "first");
    assert_eq!(calls[0].1, "{\"x\":1}");
    assert_eq!(calls[1].0, "second");
    assert_eq!(calls[1].1, "{\"y\":2}");
}

// ===== server-side tool loop: ServerToolCall / ServerToolResult rendering =====

fn server_tool_call_parts() -> [StreamPart; 2] {
    [
        StreamPart::ServerToolCall {
            id: "c1".into(),
            name: "mcp__docs__search".into(),
            arguments: "{\"q\":\"rust\"}".into(),
            server_name: Some("docs".into()),
            dynamic: true,
        },
        StreamPart::ServerToolResult {
            call_id: "c1".into(),
            tool_name: Some("mcp__docs__search".into()),
            output: ToolResultOutput::Text {
                value: "found it".into(),
            },
            dynamic: true,
        },
    ]
}

#[test]
fn messages_encodes_server_tool_call_and_result_blocks() {
    let events = encode_stream_events(ApiProtocol::Messages, &server_tool_call_parts());
    // An `mcp_tool_use` content block carrying the server_name.
    assert!(
        events.iter().any(|(ev, d)| ev == "content_block_start"
            && d.pointer("/content_block/type").and_then(|v| v.as_str()) == Some("mcp_tool_use")
            && d.pointer("/content_block/server_name")
                .and_then(|v| v.as_str())
                == Some("docs")),
        "expected an mcp_tool_use block with server_name: {events:?}"
    );
    // The arguments stream as an input_json_delta.
    assert!(
        events.iter().any(|(ev, d)| ev == "content_block_delta"
            && d.pointer("/delta/type").and_then(|v| v.as_str()) == Some("input_json_delta")),
        "expected an input_json_delta: {events:?}"
    );
    // An `mcp_tool_result` block referencing the call, carrying the output.
    assert!(
        events.iter().any(|(ev, d)| ev == "content_block_start"
            && d.pointer("/content_block/type").and_then(|v| v.as_str())
                == Some("mcp_tool_result")
            && d.pointer("/content_block/tool_use_id")
                .and_then(|v| v.as_str())
                == Some("c1")),
        "expected an mcp_tool_result block: {events:?}"
    );
}

#[test]
fn responses_encodes_mcp_call_output_item() {
    let events = encode_stream_events(ApiProtocol::Responses, &server_tool_call_parts());
    let item = events
        .iter()
        .find(|(ev, d)| {
            ev == "response.output_item.done"
                && d.pointer("/item/type").and_then(|v| v.as_str()) == Some("mcp_call")
        })
        .map(|(_, d)| d)
        .expect("expected an mcp_call output item");
    assert_eq!(
        item.pointer("/item/name").and_then(|v| v.as_str()),
        Some("mcp__docs__search")
    );
    assert_eq!(
        item.pointer("/item/output").and_then(|v| v.as_str()),
        Some("found it")
    );
    assert_eq!(
        item.pointer("/item/server_label").and_then(|v| v.as_str()),
        Some("docs")
    );
}

/// Regression #560: some OpenAI-compatible upstreams (e.g. DeepSeek / Kimi)
/// stream a tool call whose FIRST `tool_calls` delta carries only `id` +
/// partial `arguments`, delivering `function.name` in a LATER delta. The Chat
/// Completions stream encoder must hold the tool call's opening chunk back until
/// the name is known, so the first `tool_calls` chunk a client sees always
/// carries a string `function.name`. Otherwise strict clients (the Vercel AI
/// SDK `@ai-sdk/openai-compatible` / opencode) abort the stream with
/// `AI_InvalidResponseDataError: Expected 'function.name' to be a string`.
/// <https://github.com/anomalyco/opencode/issues/24137>
#[test]
fn chat_encode_late_tool_name_first_chunk_carries_name() {
    let parts = decode_stream(
        ApiProtocol::ChatCompletions,
        &[
            // First delta: id + empty arguments, NO function.name yet.
            (
                "",
                serde_json::json!({"choices":[{"delta":{"tool_calls":[
                    {"index":0,"id":"call_123","type":"function",
                     "function":{"arguments":""}}]}}]}),
            ),
            // The name arrives only on the second delta, alongside partial args.
            (
                "",
                serde_json::json!({"choices":[{"delta":{"tool_calls":[
                    {"index":0,"id":"call_123","type":"function",
                     "function":{"name":"bash","arguments":"{\"cmd\":\""}}]}}]}),
            ),
            // Continuation re-sends an empty name (normalized to None upstream).
            (
                "",
                serde_json::json!({"choices":[{"delta":{"tool_calls":[
                    {"index":0,"id":"call_123","type":"function",
                     "function":{"name":"","arguments":"ls\"}"}}]}}]}),
            ),
            (
                "",
                serde_json::json!({"choices":[{"delta":{},"finish_reason":"tool_calls"}]}),
            ),
        ],
    );
    let events = encode_stream_events(ApiProtocol::ChatCompletions, &parts);

    // Every emitted `tool_calls[]` delta, in wire order.
    let tool_deltas: Vec<serde_json::Value> = events
        .iter()
        .filter_map(|(_, chunk)| chunk["choices"][0]["delta"].get("tool_calls").cloned())
        .flat_map(|tc| tc.as_array().cloned().unwrap_or_default())
        .collect();

    // The FIRST tool_calls chunk the client sees must name the function.
    let first = tool_deltas
        .first()
        .expect("at least one tool_calls delta is emitted");
    assert_eq!(
        first["function"]["name"], "bash",
        "first streamed tool_calls chunk must carry a string function.name; got {first}"
    );
    assert_eq!(first["id"], "call_123", "tool-call id is preserved");

    // All argument fragments survive the buffering, in order.
    let args: String = tool_deltas
        .iter()
        .filter_map(|d| d["function"]["arguments"].as_str())
        .collect();
    assert_eq!(
        args, "{\"cmd\":\"ls\"}",
        "argument fragments survive the hold-until-name buffering: {tool_deltas:?}"
    );
}

#[test]
fn coarse_wires_drop_server_tool_activity() {
    // Chat Completions and Generate Content have no server-tool / MCP stream
    // form; the activity is dropped (the final answer still streams).
    for proto in [ApiProtocol::ChatCompletions, ApiProtocol::GenerateContent] {
        let events = encode_stream_events(proto.clone(), &server_tool_call_parts());
        assert!(
            !events.iter().any(|(_, d)| {
                let s = d.to_string();
                s.contains("mcp_tool_use")
                    || s.contains("mcp_call")
                    || s.contains("server_tool_use")
            }),
            "coarse wire {proto:?} must drop server-tool activity: {events:?}"
        );
    }
}
