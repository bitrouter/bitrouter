//! Phase-2 protocol-conversion tests: the 4×4 inbound/outbound matrix plus the
//! v0 bug-regression suite (008 §3.2 / §3.3).

use crate::language_model::protocol::*;
use crate::language_model::types::*;

// ===== fixtures =====

fn all_protocols() -> [ApiProtocol; 4] {
    [
        ApiProtocol::Anthropic,
        ApiProtocol::Openai,
        ApiProtocol::Responses,
        ApiProtocol::Google,
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
        }),
        finish_reason: Some(FinishReason::Stop),
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
            let inbound = adapter_for(inbound_proto);
            let outbound = adapter_for(outbound_proto);
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
            },
        },
        StreamPart::Finish {
            reason: FinishReason::Stop,
        },
    ];

    for outbound_proto in all_protocols() {
        let outbound = adapter_for(outbound_proto);
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
            decoded
                .iter()
                .any(|p| matches!(p, StreamPart::Finish { .. })),
            "{outbound_proto:?}: finish survived"
        );

        // and the decoded stream re-encodes in every inbound protocol
        for inbound_proto in all_protocols() {
            let inbound = adapter_for(inbound_proto);
            let mut enc = inbound.stream_encoder("resp_s", "test-model");
            for part in &decoded {
                enc.encode(part).unwrap_or_else(|e| {
                    panic!("{inbound_proto:?} re-encode of {outbound_proto:?} stream: {e}")
                });
            }
        }
    }
}

// ===== per-adapter unit tests (005 §9.3) =====

#[test]
fn openai_chat_request_roundtrip() {
    let adapter = adapter_for(ApiProtocol::Openai);
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
fn anthropic_response_roundtrip() {
    let adapter = adapter_for(ApiProtocol::Anthropic);
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
fn google_request_roundtrip() {
    let adapter = adapter_for(ApiProtocol::Google);
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

// ===== v0 bug regression suite (008 §3.2) =====

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
    let adapter = adapter_for(ApiProtocol::Openai);
    // `messages` should be an array; a string is a type error.
    let bad = serde_json::json!({ "model": "m", "messages": "oops" });
    let err = adapter.parse_request(bad).unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("ChatRequest"), "names the target type: {msg}");
    assert!(msg.contains("line"), "carries serde location: {msg}");
    assert!(msg.contains("preview"), "carries a body preview: {msg}");
    assert_eq!(err.status(), 400);
}

/// #416 — a mixed text + tool_use Anthropic message must not be rejected; the
/// blocks keep their order in the canonical representation.
#[test]
fn regression_416_mixed_text_and_tool_call_preserved() {
    let adapter = adapter_for(ApiProtocol::Anthropic);
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
fn regression_227_anthropic_system_accepts_string_or_array() {
    let adapter = adapter_for(ApiProtocol::Anthropic);

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
    let adapter = adapter_for(ApiProtocol::Anthropic);
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
        let adapter = adapter_for(proto);
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
    let adapter = adapter_for(ApiProtocol::Openai);
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
    };
    // OpenAI Chat: `usage` key is absent when there is no usage.
    let chat = adapter_for(ApiProtocol::Openai)
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
    };
    let chat_empty = adapter_for(ApiProtocol::Openai)
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

    // `response.incomplete` is a clean terminal event mapped to Finish(Length)
    let incomplete = SseEvent {
        event: Some("response.incomplete".to_string()),
        data: serde_json::json!({
            "type": "response.incomplete",
            "response": { "status": "incomplete" }
        })
        .to_string(),
    };
    let parts = decoder
        .decode(&incomplete)
        .expect("incomplete is not an error");
    assert!(
        parts.iter().any(|p| matches!(
            p,
            StreamPart::Finish {
                reason: FinishReason::Length
            }
        )),
        "response.incomplete → Finish(Length)"
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
fn regression_422_anthropic_ping_events_ignored() {
    let adapter = adapter_for(ApiProtocol::Anthropic);
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
fn regression_429_responses_routing_and_anthropic_frames() {
    // a gpt-5 prompt rendered for the Responses protocol
    let responses = adapter_for(ApiProtocol::Responses);
    let mut prompt = sample_prompt();
    prompt.model = "gpt-5.1".to_string();
    let req = responses.render_request(&prompt).unwrap();
    assert_eq!(req["model"], "gpt-5.1");
    assert!(req["input"].is_array(), "Responses uses an `input` array");

    // Anthropic outbound stream frames are named SSE events
    let anthropic = adapter_for(ApiProtocol::Anthropic);
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
        }),
        finish_reason: Some(FinishReason::Stop),
    };
    let chat = adapter_for(ApiProtocol::Openai)
        .render_response(&result, &sample_prompt(), "c1")
        .unwrap();
    assert_eq!(chat["usage"]["prompt_tokens"], 0);
    assert!(chat["usage"]["prompt_tokens"].is_number());
    assert!(!chat["usage"]["total_tokens"].is_null());
}
