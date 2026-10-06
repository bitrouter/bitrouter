//! Responses completed reasoning survives JSON/SSE output without replay authority.
use bitrouter_ai::error::{ModelError, Result};
use bitrouter_ai::protocol::{
    InboundAdapter, OutboundAdapter, SseEvent, chat_completions::ChatCompletionsAdapter,
    responses::ResponsesAdapter,
};
use bitrouter_ai::stream::collect::collect_generate;
use bitrouter_ai::types::{Content, Prompt, StreamPart};
use futures::stream;
use serde_json::{Value, json};

type TestResult = std::result::Result<(), Box<dyn std::error::Error + Send + Sync>>;

fn prompt() -> Result<Prompt> {
    ChatCompletionsAdapter
        .parse_request(json!({"model":"fixture","messages":[{"role":"user","content":"hello"}]}))
}
fn reasoning(id: &str, text: &str, encrypted: Value) -> Value {
    json!({"type":"reasoning","id":id,"status":"completed","summary":if text.is_empty(){json!([])}else{json!([{"type":"summary_text","text":text}])},"encrypted_content":encrypted})
}
fn decode(events: Vec<Value>) -> Result<Vec<StreamPart>> {
    let mut decoder = ResponsesAdapter.stream_decoder();
    let mut parts = Vec::new();
    for event in events {
        parts.extend(decoder.decode(&SseEvent {
            event: None,
            data: event.to_string(),
        })?);
    }
    parts.extend(decoder.finish()?);
    Ok(parts)
}
fn frame_values(
    frames: Vec<bitrouter_ai::stream::SseFrame>,
) -> std::result::Result<Vec<Value>, serde_json::Error> {
    frames
        .into_iter()
        .filter_map(|frame| match frame {
            bitrouter_ai::stream::SseFrame::Event { data, .. } => Some(serde_json::from_str(&data)),
            _ => None,
        })
        .collect()
}

#[test]
fn json_preserves_each_reasoning_item_in_output_order_including_empty_summary() -> TestResult {
    let first = reasoning("rs-one", "", json!("opaque-one-secret"));
    let second = json!({"type":"reasoning","id":"rs-two","status":"incomplete","summary":[{"type":"summary_text","text":"part one"},{"type":"summary_text","text":"part two"}],"content":[{"type":"reasoning_text","text":"native text"}],"encrypted_content":"opaque-two-secret","extension":{"future":"preserved"}});
    let body = json!({"id":"resp-one","status":"completed","output":[first,{"type":"message","role":"assistant","content":[{"type":"output_text","text":"between"}]},second,{"type":"function_call","id":"fc-one","call_id":"call-one","name":"tool","arguments":"{}"}]});
    let parsed = ResponsesAdapter.parse_response(body.clone())?;
    assert_eq!(parsed.content.len(), 4);
    let rendered = ResponsesAdapter.render_response(&parsed, &prompt()?, "gateway-id")?;
    assert_eq!(rendered["output"][0], body["output"][0]);
    assert_eq!(rendered["output"][1]["type"], "message");
    assert_eq!(rendered["output"][2], body["output"][2]);
    assert_eq!(rendered["output"][3]["call_id"], "call-one");
    assert!(!format!("{parsed:?}").contains("secret"));
    Ok(())
}

#[tokio::test]
async fn sse_uses_completed_item_and_collector_matches_native_json_payload() -> TestResult {
    let native = reasoning("rs-one", "visible summary", json!("complete-opaque-secret"));
    let parts = decode(vec![
        json!({"type":"response.created","response":{"id":"resp-one"}}),
        json!({"type":"response.output_item.added","output_index":0,"item":{"type":"reasoning","id":"rs-one","summary":[],"encrypted_content":"partial-opaque-secret","status":"in_progress"}}),
        json!({"type":"response.reasoning_summary_text.delta","item_id":"rs-one","delta":"visible "}),
        json!({"type":"response.reasoning_summary_text.delta","item_id":"rs-one","delta":"summary"}),
        json!({"type":"response.output_item.done","output_index":0,"item":native}),
        json!({"type":"response.completed","response":{"id":"resp-one","status":"completed","output":[native]}}),
    ])?;
    let end = parts
        .iter()
        .find(|part| matches!(part, StreamPart::ReasoningEnd { .. }))
        .ok_or("missing reasoning end")?;
    assert_eq!(
        serde_json::to_value(end)?["native"],
        json!({"protocol":"responses","item":native})
    );
    assert!(!format!("{parts:?}").contains("opaque-secret"));
    let collected = collect_generate(stream::iter(
        parts.clone().into_iter().map(Ok::<_, ModelError>),
    ))
    .await?;
    let direct = ResponsesAdapter
        .parse_response(json!({"id":"resp-one","status":"completed","output":[native]}))?;
    assert_eq!(collected.content, direct.content);
    let mut encoder = ResponsesAdapter.stream_encoder("gateway-id", "fixture");
    let mut frames = Vec::new();
    for part in &parts {
        frames.extend(encoder.encode(part)?);
    }
    let values = frame_values(frames)?;
    let done = values
        .iter()
        .find(|value| value["type"] == "response.output_item.done")
        .ok_or("missing item close")?;
    assert_eq!(done["item"], native);
    let completed = values
        .iter()
        .find(|value| value["type"] == "response.completed")
        .ok_or("missing response close")?;
    assert_eq!(completed["response"]["output"][0], native);
    assert!(
        !values
            .iter()
            .any(|value| value.to_string().contains("partial-opaque-secret"))
    );
    Ok(())
}

#[tokio::test]
async fn terminal_only_summary_and_encrypted_only_blocks_are_not_lost() -> TestResult {
    for text in ["", "summary returned only at item completion"] {
        let native = reasoning("rs-one", text, json!("opaque-secret"));
        let parts = decode(vec![
            json!({"type":"response.output_item.added","output_index":0,"item":{"id":"rs-one","type":"reasoning","summary":[]}}),
            json!({"type":"response.output_item.done","output_index":0,"item":native}),
            json!({"type":"response.completed","response":{"id":"resp-one","status":"completed","output":[native]}}),
        ])?;
        let result =
            collect_generate(stream::iter(parts.into_iter().map(Ok::<_, ModelError>))).await?;
        assert!(
            matches!(result.content.as_slice(),[Content::Reasoning {text:actual,..}] if actual == text)
        );
        assert_eq!(
            ResponsesAdapter.render_response(&result, &prompt()?, "gateway-id")?["output"][0],
            native
        );
    }
    Ok(())
}

#[test]
fn output_preservation_does_not_replay_opaque_reasoning_or_turn_it_into_anthropic_signature()
-> TestResult {
    let native = reasoning("rs-one", "summary", json!("opaque-secret"));
    let result = ResponsesAdapter.parse_response(json!({"output":[native]}))?;
    let mut source = prompt()?;
    source.messages[0].content = result.content.clone();
    let original = source.clone();
    for adapter in [
        &ResponsesAdapter as &dyn OutboundAdapter,
        &ChatCompletionsAdapter as &dyn OutboundAdapter,
        &bitrouter_ai::protocol::messages::MessagesAdapter as &dyn OutboundAdapter,
        &bitrouter_ai::protocol::generate_content::GenerateContentAdapter as &dyn OutboundAdapter,
    ] {
        assert!(matches!(
            adapter.render_request(&source),
            Err(ModelError::Incompatible { .. })
        ));
    }
    assert_eq!(source, original);
    let end = serde_json::to_value(decode(vec![
        json!({"type":"response.output_item.added","item":{"type":"reasoning","id":"rs-one","summary":[]}}),
        json!({"type":"response.output_item.done","item":native}),
            json!({"type":"response.completed","response":{"id":"resp-one","status":"completed","output":[native]}}),
    ])?.into_iter().find(|part|matches!(part,StreamPart::ReasoningEnd {..})).ok_or("missing end")?)?;
    assert!(end["signature"].is_null());
    assert!(end["native"].is_object());
    Ok(())
}

#[tokio::test]
async fn model_client_json_and_codex_sse_keep_reasoning_without_dispatching_unadmitted_history()
-> TestResult {
    use bitrouter_ai::client::{HttpTimeouts, ModelClient};
    use bitrouter_ai::target::ModelTarget;
    use bitrouter_ai::types::ApiProtocol;
    use tokio_util::sync::CancellationToken;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};
    for provider in ["fixture", "openai-codex"] {
        let server = MockServer::start().await;
        let native = reasoning("rs-one", "summary", json!("opaque-secret"));
        let response = json!({"id":"resp-one","status":"completed","output":[native],"usage":{"input_tokens":2,"output_tokens":3,"total_tokens":5}});
        let template = if provider == "openai-codex" {
            let body=[json!({"type":"response.output_item.added","output_index":0,"item":{"type":"reasoning","id":"rs-one","summary":[]}}),json!({"type":"response.output_item.done","output_index":0,"item":native}),json!({"type":"response.completed","response":response})].iter().map(|event|format!("data: {event}\n\n")).collect::<String>();
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(body)
        } else {
            ResponseTemplate::new(200).set_body_json(response)
        };
        Mock::given(method("POST"))
            .and(path("/responses"))
            .respond_with(template)
            .expect(1)
            .mount(&server)
            .await;
        let selected = ModelTarget {
            provider_name: provider.into(),
            service_id: "fixture".into(),
            api_protocol: ApiProtocol::Responses,
            api_base: server.uri(),
            api_key: "explicit-key".into(),
            credential_priority: Default::default(),
            account_label: None,
            auth_scheme: Default::default(),
            compatibility: Default::default(),
        };
        let source = prompt()?;
        let original = source.clone();
        let client = ModelClient::new(HttpTimeouts::default())?;
        let result = client
            .generate(&selected, &source, &CancellationToken::new())
            .await?;
        assert_eq!(
            ResponsesAdapter.render_response(&result, &source, "gateway-id")?["output"][0],
            native
        );
        assert_eq!(result.usage.ok_or("missing usage")?.total(), 5);
        assert_eq!(source, original);
        let mut history = source.clone();
        history.messages[0].content = result.content;
        assert!(matches!(
            client
                .generate(&selected, &history, &CancellationToken::new())
                .await,
            Err(ModelError::Incompatible { .. })
        ));
        assert!(matches!(
            client
                .stream(&selected, &history, &CancellationToken::new())
                .await,
            Err(ModelError::Incompatible { .. })
        ));
    }
    Ok(())
}

#[tokio::test]
async fn native_snapshots_cannot_undo_changed_visible_text_or_cross_into_other_output_wires()
-> TestResult {
    let native = reasoning("rs-one", "original text", json!("opaque-secret"));
    let mut parts = decode(vec![
        json!({"type":"response.output_item.added","output_index":0,"item":{"type":"reasoning","id":"rs-one","summary":[]}}),
        json!({"type":"response.output_item.done","output_index":0,"item":native}),
        json!({"type":"response.completed","response":{"id":"resp-one","status":"completed","output":[native]}}),
    ])?;
    for part in &mut parts {
        if let StreamPart::ReasoningDelta { text, .. } = part {
            *text = "changed text".into();
        }
    }
    let error = collect_generate(stream::iter(
        parts.clone().into_iter().map(Ok::<_, ModelError>),
    ))
    .await
    .err()
    .ok_or("snapshot restored changed stream text")?;
    assert!(matches!(error, ModelError::InvalidResponse { .. }));
    assert!(!format!("{error:?}").contains("opaque-secret"));
    let mut encoder = ResponsesAdapter.stream_encoder("gateway-id", "fixture");
    let mut failed = false;
    for part in &parts {
        if encoder.encode(part).is_err() {
            failed = true;
            break;
        }
    }
    assert!(failed);
    let mut result = ResponsesAdapter.parse_response(json!({"output":[native]}))?;
    if let Content::Reasoning { text, .. } = &mut result.content[0] {
        *text = "changed text".into();
    }
    assert!(matches!(
        ResponsesAdapter.render_response(&result, &prompt()?, "gateway-id"),
        Err(ModelError::InvalidResponse { .. })
    ));
    let result = ResponsesAdapter.parse_response(json!({"output":[native]}))?;
    for adapter in [
        &ChatCompletionsAdapter as &dyn InboundAdapter,
        &bitrouter_ai::protocol::messages::MessagesAdapter as &dyn InboundAdapter,
        &bitrouter_ai::protocol::generate_content::GenerateContentAdapter as &dyn InboundAdapter,
    ] {
        assert!(matches!(
            adapter.render_response(&result, &prompt()?, "gateway-id"),
            Err(ModelError::Incompatible { .. })
        ));
        let end = parts
            .iter()
            .find(|part| matches!(part, StreamPart::ReasoningEnd { .. }))
            .ok_or("missing native end")?;
        assert!(
            adapter
                .stream_encoder("gateway-id", "fixture")
                .encode(end)
                .is_err()
        );
    }
    Ok(())
}

#[test]
fn rejects_contradictory_or_malformed_native_snapshots_without_echoing_payload() -> TestResult {
    for native in [
        reasoning("rs-one", "other text", json!("opaque-secret")),
        reasoning("rs-one", "visible", json!({"secret":"opaque-secret"})),
    ] {
        let mut decoder = ResponsesAdapter.stream_decoder();
        decoder.decode(&SseEvent {event:None,data:json!({"type":"response.output_item.added","item":{"type":"reasoning","id":"rs-one","summary":[]}}).to_string()})?;
        decoder.decode(&SseEvent {event:None,data:json!({"type":"response.reasoning_summary_text.delta","item_id":"rs-one","delta":"visible"}).to_string()})?;
        let error = decoder
            .decode(&SseEvent {
                event: None,
                data: json!({"type":"response.output_item.done","item":native}).to_string(),
            })
            .err()
            .ok_or("invalid native snapshot accepted")?;
        assert!(matches!(error, ModelError::InvalidResponse { .. }));
        assert!(!format!("{error:?}").contains("opaque-secret"));
    }
    Ok(())
}

#[test]
fn legacy_reasoning_payloads_still_deserialize_without_native_fields() -> TestResult {
    let block: Content = serde_json::from_value(json!({"type":"reasoning","text":"legacy"}))?;
    assert!(serde_json::to_value(&block)?.get("native").is_none());
    let start: StreamPart = serde_json::from_value(json!({"kind":"reasoning_start","id":"r"}))?;
    let delta: StreamPart =
        serde_json::from_value(json!({"kind":"reasoning_delta","text":"legacy"}))?;
    let end: StreamPart = serde_json::from_value(
        json!({"kind":"reasoning_end","id":"r","signature":"legacy-signature"}),
    )?;
    assert!(
        serde_json::to_value(start)?
            .get("source_protocol")
            .is_none()
    );
    assert!(serde_json::to_value(delta)?.get("source_kind").is_none());
    assert!(serde_json::to_value(end)?.get("native").is_none());
    Ok(())
}

#[tokio::test]
async fn summary_and_reasoning_text_lanes_keep_missing_suffixes_and_native_close_identity()
-> TestResult {
    let native = json!({"type":"reasoning","id":"rs-one","status":"completed","summary":[{"type":"summary_text","text":"summary"}],"content":[{"type":"reasoning_text","text":"native text"}],"encrypted_content":"opaque-secret"});
    let parts = decode(vec![
        json!({"type":"response.output_item.added","output_index":0,"item":{"type":"reasoning","id":"rs-one","summary":[]}}),
        json!({"type":"response.reasoning_summary_text.delta","item_id":"rs-one","delta":"sum"}),
        json!({"type":"response.output_item.done","output_index":0,"item":native}),
        json!({"type":"response.completed","response":{"id":"resp-one","status":"completed","output":[native]}}),
    ])?;
    let result = collect_generate(stream::iter(
        parts.clone().into_iter().map(Ok::<_, ModelError>),
    ))
    .await?;
    assert!(
        matches!(result.content.as_slice(),[Content::Reasoning {text,..}] if text=="summarynative text")
    );
    let mut encoder = ResponsesAdapter.stream_encoder("gateway-id", "fixture");
    let mut frames = Vec::new();
    for part in &parts {
        frames.extend(encoder.encode(part)?);
    }
    let values = frame_values(frames)?;
    let summary = values
        .iter()
        .filter(|value| value["type"] == "response.reasoning_summary_text.delta")
        .filter_map(|value| value["delta"].as_str())
        .collect::<String>();
    let text = values
        .iter()
        .filter(|value| value["type"] == "response.reasoning_text.delta")
        .filter_map(|value| value["delta"].as_str())
        .collect::<String>();
    assert_eq!(summary, "summary");
    assert_eq!(text, "native text");
    for value in values.iter().filter(|value| {
        value["type"] == "response.output_item.added"
            || value["type"] == "response.output_item.done"
    }) {
        assert_eq!(value["item"]["id"], "rs-one");
    }
    assert_eq!(
        values
            .iter()
            .find(|value| value["type"] == "response.output_item.done")
            .ok_or("missing close")?["item"],
        native
    );
    Ok(())
}

#[test]
fn ingress_preserves_encrypted_only_reasoning_and_native_array_order() -> TestResult {
    let native = reasoning("rs-ingress-secret", "", json!("opaque-ingress-secret"));
    let request = json!({"model":"fixture","input":[{"role":"user","content":"before"},native,{"role":"user","content":"after"}]});
    let parsed = ResponsesAdapter.parse_request(request)?;
    assert_eq!(parsed.messages.len(), 3);
    let Content::Reasoning {
        native: Some(retained),
        text,
        ..
    } = &parsed.messages[1].content[0]
    else {
        return Err("ingress dropped native reasoning".into());
    };
    assert!(text.is_empty());
    assert_eq!(serde_json::to_value(retained)?["item"], native);
    assert!(!format!("{parsed:?}").contains("ingress-secret"));
    Ok(())
}

#[test]
fn admission_reports_source_locations_without_opaque_material() -> TestResult {
    use bitrouter_ai::conversion::{
        ConversionDisposition, ConversionEffect, ConversionLocation, ConversionReason,
        request_admission,
    };
    use bitrouter_ai::types::ApiProtocol;
    let native = reasoning("rs-report-secret", "visible", json!("opaque-report-secret"));
    let source = ResponsesAdapter.parse_request(json!({"model":"fixture","input":[native.clone(),{"role":"user","content":"between"},native]}))?;
    let original = source.clone();
    for protocol in [
        ApiProtocol::Responses,
        ApiProtocol::ChatCompletions,
        ApiProtocol::Messages,
        ApiProtocol::GenerateContent,
        ApiProtocol::Custom("custom-secret".into()),
    ] {
        let report = request_admission(&source, &protocol);
        assert_eq!(report.issues.len(), 2);
        assert_eq!(
            report.issues[0].location,
            ConversionLocation::MessageContent {
                message: 0,
                block: 0
            }
        );
        assert_eq!(
            report.issues[1].location,
            ConversionLocation::MessageContent {
                message: 2,
                block: 0
            }
        );
        if protocol == ApiProtocol::Responses {
            assert_eq!(
                report.issues[0].reason,
                ConversionReason::NativeReasoningAuthorityUnproven
            );
            assert_eq!(report.issues[0].effect, ConversionEffect::ReplayAuthority);
            assert_eq!(
                report.issues[0].disposition,
                ConversionDisposition::RejectReplay
            );
        } else if matches!(protocol, ApiProtocol::Custom(_)) {
            assert_eq!(
                report.issues[0].reason,
                ConversionReason::NativeReasoningCompatibilityUnknown
            );
            assert_eq!(report.issues[0].effect, ConversionEffect::Unknown);
            assert_eq!(
                report.issues[0].disposition,
                ConversionDisposition::ExcludeTarget
            );
        } else {
            assert_eq!(
                report.issues[0].reason,
                ConversionReason::NativeReasoningUnrepresentable
            );
            assert_eq!(
                report.issues[0].disposition,
                ConversionDisposition::ExcludeTarget
            );
        }
        let serialized = serde_json::to_value(&report)?;
        assert!(!serialized.to_string().contains("secret"));
        assert!(!format!("{report:?}").contains("visible"));
        assert_eq!(
            serde_json::from_value::<bitrouter_ai::conversion::ConversionReport>(serialized)?,
            report
        );
        assert!(matches!(
            report.require_admitted(),
            Err(ModelError::Incompatible { .. })
        ));
    }
    assert_eq!(source, original);
    Ok(())
}

#[test]
fn malformed_native_ingress_is_a_safe_request_error() -> TestResult {
    let input = reasoning("rs-secret", "", json!({"unexpected":"opaque-secret"}));
    let error = ResponsesAdapter
        .parse_request(json!({"model":"fixture","input":[input]}))
        .err()
        .ok_or("malformed ingress was accepted")?;
    assert!(matches!(error, ModelError::InvalidRequest { .. }));
    assert!(!format!("{error:?}").contains("secret"));
    Ok(())
}
