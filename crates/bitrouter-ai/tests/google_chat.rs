//! Google Chat continuity must survive gateway encoding and stream collection.

use bitrouter_ai::client::{HttpTimeouts, ModelClient};
use bitrouter_ai::error::ModelError;
use bitrouter_ai::protocol::chat_completions::ChatCompletionsAdapter;
use bitrouter_ai::protocol::{InboundAdapter, OutboundAdapter, SseEvent};
use bitrouter_ai::stream::SseFrame;
use bitrouter_ai::stream::collect::collect_generate;
use bitrouter_ai::target::ModelTarget;
use bitrouter_ai::types::{ApiProtocol, Message, Role, ToolResultOutput};
use bitrouter_ai::types::{Content, Prompt};
use futures::stream;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{header, method, path},
};

type TestResult = Result<(), Box<dyn std::error::Error + Send + Sync>>;

fn prompt() -> bitrouter_ai::error::Result<Prompt> {
    ChatCompletionsAdapter.parse_request(json!({
        "model":"gemini-fixture",
        "messages":[{"role":"user","content":"check two cities"}]
    }))
}

fn signature(call: &Value) -> &Value {
    &call["extra_content"]["google"]["thought_signature"]
}

fn target(base: &str) -> ModelTarget {
    let mut target = ModelTarget {
        provider_name: "google".into(),
        service_id: "gemini-fixture".into(),
        api_protocol: ApiProtocol::ChatCompletions,
        api_base: base.into(),
        api_key: "fixture-static-key".into(),
        credential_priority: Default::default(),
        account_label: Some("selected".into()),
        auth_scheme: Default::default(),
        compatibility: Default::default(),
    };
    target.compatibility.chat_completions.google_extensions = true;
    target
}

fn signed_response() -> Value {
    json!({"choices":[{"message":{"role":"assistant","tool_calls":[
        {"id":"one","type":"function","function":{"name":"weather","arguments":"{\"city\":\"Paris\"}"},"extra_content":{"google":{"thought_signature":"first-opaque-signature"}}}
    ]},"finish_reason":"tool_calls"}]})
}

fn followup(result: bitrouter_ai::types::GenerateResult) -> bitrouter_ai::error::Result<Prompt> {
    let mut source = prompt()?;
    source.messages.push(Message {
        role: Role::Assistant,
        content: result.content,
    });
    source.messages.push(Message {
        role: Role::Tool,
        content: vec![Content::ToolResult {
            call_id: "one".into(),
            tool_name: Some("weather".into()),
            output: ToolResultOutput::Text {
                value: "sunny".into(),
            },
            dynamic: false,
            provider_metadata: Default::default(),
        }],
    });
    Ok(source)
}

#[tokio::test]
async fn actual_selected_call_binds_history_and_refuses_changed_target_before_io() -> TestResult {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(header("authorization", "Bearer fixture-static-key"))
        .respond_with(ResponseTemplate::new(200).set_body_json(signed_response()))
        .mount(&server)
        .await;
    let target = target(&server.uri());
    let client = ModelClient::new(HttpTimeouts::default())?;
    let result = client
        .generate(&target, &prompt()?, &CancellationToken::new())
        .await?;
    let gateway = ChatCompletionsAdapter.render_response(&result, &prompt()?, "gateway")?;
    let calls = &gateway["choices"][0]["message"]["tool_calls"];
    assert!(calls[0]["extra_content"]["bitrouter"]["google_replay_proof"].is_string());
    let source=ChatCompletionsAdapter.parse_request(json!({"model":"gemini-fixture","messages":[
        {"role":"assistant","tool_calls":calls}, {"role":"tool","tool_call_id":"one","content":"sunny"}
    ]}))?;
    let body = client.render_request(&target, &source, false)?;
    assert_eq!(
        signature(&body["messages"][0]["tool_calls"][0]),
        "first-opaque-signature"
    );
    assert!(
        body["messages"][0]["tool_calls"][0]["extra_content"]
            .get("bitrouter")
            .is_none()
    );
    for mut changed in [
        target.clone(),
        target.clone(),
        target.clone(),
        target.clone(),
        target.clone(),
    ]
    .into_iter()
    .enumerate()
    {
        match changed.0 {
            0 => changed.1.api_key = "different-account-key".into(),
            1 => changed.1.service_id = "different-model".into(),
            2 => changed.1.api_base = format!("{}/another-endpoint", server.uri()),
            3 => changed.1.account_label = Some("other".into()),
            _ => changed.1.compatibility.chat_completions.google_extensions = false,
        }
        let error = client
            .generate(&changed.1, &source, &CancellationToken::new())
            .await
            .err()
            .ok_or("changed replay target was admitted")?;
        assert!(matches!(error, ModelError::Incompatible { .. }));
        assert!(!format!("{error:?}").contains("opaque-signature"));
    }
    let requests = server
        .received_requests()
        .await
        .ok_or("request inventory unavailable")?;
    assert_eq!(requests.len(), 1);
    let raw = ChatCompletionsAdapter.parse_response(signed_response())?;
    assert!(
        client
            .render_request(&target, &followup(raw)?, false)
            .is_err()
    );
    let mut altered = source.clone();
    if let Content::ToolCall { arguments, .. } = &mut altered.messages[0].content[0] {
        *arguments = "changed arguments".into();
    }
    assert!(client.render_request(&target, &altered, false).is_err());
    Ok(())
}

#[tokio::test]
async fn streamed_proof_uses_complete_arguments_and_round_trips_into_next_request() -> TestResult {
    let target = target("https://google-fixture.test");
    let mut decoder = ChatCompletionsAdapter.stream_decoder();
    let mut parts = Vec::new();
    for delta in [
        json!({"tool_calls":[{"index":0,"id":"one","function":{"name":"weather","arguments":"{\"city\":"},"extra_content":{"google":{"thought_signature":"first-opaque-signature"}}}]}),
        json!({"tool_calls":[{"index":0,"function":{"arguments":"\"Paris\"}"}}]}),
    ] {
        parts.extend(decoder.decode(&SseEvent {
            event: None,
            data: json!({"choices":[{"delta":delta}]}).to_string(),
        })?);
    }
    parts.extend(decoder.decode(&SseEvent {
        event: None,
        data: json!({"choices":[{"delta":{},"finish_reason":"tool_calls"}]}).to_string(),
    })?);
    parts.extend(decoder.decode(&SseEvent {
        event: None,
        data: "[DONE]".into(),
    })?);
    let bound = bitrouter_ai::providers::google_chat::bind_stream(
        stream::iter(parts.into_iter().map(Ok::<_, ModelError>)),
        target.clone(),
    );
    futures::pin_mut!(bound);
    let result = collect_generate(bound).await?;
    let direct = bitrouter_ai::providers::google_chat::bind_result(
        ChatCompletionsAdapter.parse_response(signed_response())?,
        &target,
    )?;
    assert_eq!(result.content, direct.content);
    ModelClient::new(HttpTimeouts::default())?.render_request(
        &target,
        &followup(result)?,
        false,
    )?;
    Ok(())
}

#[test]
fn google_controls_cannot_leak_to_an_unrelated_target_or_silently_drop_cache_references()
-> TestResult {
    let target = target("https://google-fixture.test");
    let client = ModelClient::new(HttpTimeouts::default())?;
    let mut source = prompt()?;
    source.params.extra.insert(
        "extra_body".into(),
        json!({"google":{"thinking_config":{"include_thoughts":true}}}),
    );
    let body = client.render_request(&target, &source, false)?;
    assert_eq!(
        body["extra_body"]["google"]["thinking_config"]["include_thoughts"],
        true
    );
    let mut unrelated = target.clone();
    unrelated.compatibility.chat_completions.google_extensions = false;
    assert!(client.render_request(&unrelated, &source, false).is_err());
    source.params.extra.insert(
        "extra_body".into(),
        json!({"google":{"cached_content":"cachedContents/private"}}),
    );
    assert!(client.render_request(&target, &source, false).is_err());
    for config in [
        json!({"thinking_level":"low"}),
        json!({"thinking_budget":1024}),
    ] {
        source.params.extra.insert(
            "extra_body".into(),
            json!({"google":{"thinking_config":config}}),
        );
        assert!(client.render_request(&target, &source, false).is_err());
    }
    let request = reqwest::Client::new()
        .post("https://google-fixture.test/chat/completions")
        .bearer_auth("wrong-key")
        .json(&json!({"model":target.service_id}))
        .build()?;
    assert!(
        bitrouter_ai::providers::google_chat::validate_authenticated_request(&request, &target)
            .is_err()
    );
    Ok(())
}

#[test]
fn ingress_and_response_preserve_each_calls_google_signature() -> TestResult {
    let calls = json!([
        {"id":"one","type":"function","function":{"name":"weather","arguments":"{\"city\":\"Paris\"}"},"extra_content":{"google":{"thought_signature":"first-opaque-signature"}}},
        {"id":"two","type":"function","function":{"name":"weather","arguments":"{\"city\":\"London\"}"}}
    ]);
    let source = ChatCompletionsAdapter.parse_request(json!({
        "model":"gemini-fixture",
        "messages":[{"role":"assistant","tool_calls":calls}]
    }))?;
    assert!(matches!(
        &source.messages[0].content[0],
        Content::ToolCall { provider_metadata, .. }
            if provider_metadata["google"]["thoughtSignature"] == "first-opaque-signature"
    ));
    let result = ChatCompletionsAdapter.parse_response(json!({
        "choices":[{"message":{"role":"assistant","tool_calls":calls},"finish_reason":"tool_calls"}]
    }))?;
    let rendered = ChatCompletionsAdapter.render_response(&result, &prompt()?, "gateway")?;
    assert_eq!(rendered["choices"][0]["message"]["tool_calls"], calls);
    assert!(!format!("{result:?}").contains("first-opaque-signature"));
    Ok(())
}

#[tokio::test]
async fn late_and_signature_only_deltas_survive_collection_and_downstream_sse() -> TestResult {
    let deltas = vec![
        json!({"tool_calls":[{"index":0,"id":"one","function":{"name":"weather","arguments":"{\"city\":"}}]}),
        json!({"tool_calls":[{"index":1,"id":"two","function":{"arguments":"{\"city\":\"London\"}"},"extra_content":{"google":{"thought_signature":"second-opaque-signature"}}}]}),
        json!({"tool_calls":[{"index":0,"function":{"arguments":"\"Paris\"}"}}]}),
        json!({"tool_calls":[{"index":1,"function":{"name":"weather","arguments":""}}]}),
        json!({"tool_calls":[{"index":0,"extra_content":{"google":{"thought_signature":"first-opaque-signature"}}}]}),
    ];
    let mut decoder = ChatCompletionsAdapter.stream_decoder();
    let mut parts = Vec::new();
    for delta in deltas {
        parts.extend(decoder.decode(&SseEvent {
            event: None,
            data: json!({"choices":[{"delta":delta}]}).to_string(),
        })?);
    }
    parts.extend(decoder.decode(&SseEvent {
        event: None,
        data: json!({"choices":[{"delta":{},"finish_reason":"tool_calls"}]}).to_string(),
    })?);
    parts.extend(decoder.decode(&SseEvent {
        event: None,
        data: "[DONE]".into(),
    })?);
    let result = collect_generate(stream::iter(
        parts.clone().into_iter().map(Ok::<_, ModelError>),
    ))
    .await?;
    let body = ChatCompletionsAdapter.render_response(&result, &prompt()?, "gateway")?;
    let calls = &body["choices"][0]["message"]["tool_calls"];
    assert_eq!(calls[0]["id"], "one");
    assert_eq!(calls[1]["id"], "two");
    assert_eq!(signature(&calls[0]), "first-opaque-signature");
    assert_eq!(signature(&calls[1]), "second-opaque-signature");
    assert_eq!(calls[0]["function"]["arguments"], "{\"city\":\"Paris\"}");
    assert_eq!(calls[1]["function"]["arguments"], "{\"city\":\"London\"}");

    let mut encoder = ChatCompletionsAdapter.stream_encoder("gateway", "gemini-fixture");
    let mut downstream = ChatCompletionsAdapter.stream_decoder();
    let mut replayed = Vec::new();
    for part in &parts {
        for frame in encoder.encode(part)? {
            if let SseFrame::Event { event, data } = frame {
                replayed.extend(downstream.decode(&SseEvent { event, data })?);
            }
        }
    }
    replayed.extend(downstream.finish()?);
    let replayed =
        collect_generate(stream::iter(replayed.into_iter().map(Ok::<_, ModelError>))).await?;
    assert_eq!(replayed.content, result.content);
    assert!(!format!("{parts:?}").contains("opaque-signature"));
    Ok(())
}

#[test]
fn malformed_or_unknown_google_continuity_is_not_silently_erased() -> TestResult {
    for google in [
        json!({"thought_signature":7}),
        json!({"thought_signature":""}),
        json!({"future_continuity":"opaque"}),
    ] {
        let call = json!({"id":"one","function":{"name":"weather","arguments":"{}"},"extra_content":{"google":google}});
        assert!(
            ChatCompletionsAdapter
                .parse_request(json!({
                    "model":"gemini-fixture","messages":[{"role":"assistant","tool_calls":[call]}]
                }))
                .is_err()
        );
        assert!(
            ChatCompletionsAdapter
                .parse_response(json!({
                    "choices":[{"message":{"tool_calls":[call]}}]
                }))
                .is_err()
        );
        let mut decoder = ChatCompletionsAdapter.stream_decoder();
        assert!(decoder.decode(&SseEvent {
            event: None,
            data: json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"one","extra_content":{"google":google}}]}}]}).to_string(),
        }).is_err());
    }
    Ok(())
}

#[tokio::test]
async fn late_invalid_continuity_retains_available_provider_usage() -> TestResult {
    let mut body = signed_response();
    body["choices"][0]["message"]["tool_calls"][0]["id"] = json!("");
    body["usage"] = json!({"prompt_tokens":12,"completion_tokens":7,"completion_tokens_details":{"reasoning_tokens":3},"prompt_tokens_details":{"cached_tokens":2}});
    let selected = target("https://google-fixture.test");
    let parsed = ChatCompletionsAdapter.parse_response(body.clone())?;
    let error = bitrouter_ai::providers::google_chat::bind_result(parsed, &selected)
        .err()
        .ok_or("invalid signed call was accepted")?;
    let ModelError::InvalidResponse {
        usage: Some(usage), ..
    } = error
    else {
        return Err("late error lost usage".into());
    };
    assert_eq!(usage.completion_tokens, 7);
    assert_eq!(usage.reasoning_tokens, 3);
    assert_eq!(
        usage
            .normalized_buckets()
            .map_err(|_| "usage normalization failed")?
            .output_tokens,
        4
    );

    body["choices"][0]["message"]["tool_calls"][0]["id"] = json!("one");
    body["choices"][0]["message"]["tool_calls"][0]["function"]["arguments"] = json!(false);
    let malformed = ChatCompletionsAdapter.parse_response(body.clone())?;
    assert!(matches!(
        bitrouter_ai::providers::google_chat::bind_result(malformed, &selected),
        Err(ModelError::InvalidResponse { usage: Some(_), .. })
    ));
    body["choices"][0]["message"]["tool_calls"][0]["extra_content"]["google"]["thought_signature"] =
        json!(false);
    let error = ModelClient::parse_response(
        &ChatCompletionsAdapter,
        &ApiProtocol::ChatCompletions,
        &body.to_string(),
    )
    .err()
    .ok_or("malformed signature accepted")?;
    assert!(matches!(
        error,
        ModelError::InvalidResponse { usage: Some(_), .. }
    ));
    Ok(())
}

#[test]
fn google_target_refuses_unproven_constraints_without_rewriting_source() -> TestResult {
    let selected = target("https://google-fixture.test");
    let client = ModelClient::new(HttpTimeouts::default())?;
    for parameters in [
        json!({"type":"object","properties":{"x":{"type":"integer","exclusiveMinimum":0}}}),
        json!({"type":"object","additionalProperties":false}),
        json!({"type":"object","properties":{"x":{"type":"string","pattern":"^a"}}}),
    ] {
        let source = ChatCompletionsAdapter.parse_request(json!({"model":"fixture","messages":[{"role":"user","content":"keep"}],"tools":[{"type":"function","function":{"name":"f","parameters":parameters}}]}))?;
        let original = source.clone();
        assert!(matches!(
            client.render_request(&selected, &source, false),
            Err(ModelError::Incompatible { .. })
        ));
        assert_eq!(source, original);
        let mut unrelated = selected.clone();
        unrelated.compatibility.chat_completions.google_extensions = false;
        assert_eq!(
            client.render_request(&unrelated, &source, false)?["tools"][0]["function"]["parameters"],
            parameters
        );
    }
    let source = ChatCompletionsAdapter.parse_request(json!({"model":"fixture","messages":[{"role":"assistant","content":"x","extra_content":{"google":{"thought_signature":"opaque"}}}]}));
    assert!(source.is_err());
    Ok(())
}

#[test]
fn foreign_gateway_encoders_refuse_to_erase_google_continuity() -> TestResult {
    use bitrouter_ai::protocol::{messages::MessagesAdapter, responses::ResponsesAdapter};
    let result = ChatCompletionsAdapter.parse_response(signed_response())?;
    let source = prompt()?;
    for adapter in [
        &MessagesAdapter as &dyn InboundAdapter,
        &ResponsesAdapter as &dyn InboundAdapter,
    ] {
        assert!(
            adapter
                .render_response(&result, &source, "gateway")
                .is_err()
        );
        let mut encoder = adapter.stream_encoder("gateway", &source.model);
        let Content::ToolCall {
            id,
            name,
            arguments,
            provider_metadata,
            ..
        } = &result.content[0]
        else {
            return Err("missing call".into());
        };
        assert!(
            encoder
                .encode(&bitrouter_ai::types::StreamPart::ToolCallDelta {
                    id: id.clone(),
                    name: Some(name.clone()),
                    arguments: arguments.clone(),
                    provider_metadata: provider_metadata.clone()
                })
                .is_err()
        );
    }
    Ok(())
}
