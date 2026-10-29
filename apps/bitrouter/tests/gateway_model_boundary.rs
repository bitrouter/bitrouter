//! Regression coverage for SDK policy and resolved-target boundaries.

use bitrouter::gateway::error::BitrouterError;
use bitrouter::gateway::model::error::ModelError;
use bitrouter::gateway::model::protocol::inbound_adapter_for;
use bitrouter::gateway::model::stream::SseFrame;
use bitrouter::gateway::model::target::CredentialPriority;
use bitrouter::gateway::pipeline::hooks::FallbackDecision;
use bitrouter::gateway::pipeline::routing::{DefaultFallbackPolicy, FallbackPolicy};
use bitrouter::gateway::pipeline::types::{OutboundHeaderRule, RoutingTarget};
use bitrouter_ai::types::{ApiProtocol, ChatTokenLimitField};

fn all_protocols() -> [ApiProtocol; 3] {
    [
        ApiProtocol::ChatCompletions,
        ApiProtocol::Messages,
        ApiProtocol::Responses,
    ]
}

#[test]
fn model_diagnostics_are_sanitized_by_sdk_before_sse() -> bitrouter::gateway::error::Result<()> {
    let cases = [
        (
            ModelError::Provider {
                status: 401,
                message: "provider secret stack trace".into(),
            },
            502,
            "upstream request failed",
            "upstream_bad_gateway",
        ),
        (
            ModelError::InvalidResponse {
                message: "provider secret invalid body".into(),
                usage: None,
            },
            502,
            "upstream returned an invalid response",
            "upstream_invalid_response",
        ),
        (
            ModelError::PolicyViolation {
                message: "provider secret policy detail".into(),
            },
            403,
            "upstream content policy violation",
            "upstream_policy_violation",
        ),
    ];
    for (domain_error, status, message, code) in cases {
        let error = BitrouterError::from(domain_error);
        let public = error.stream_error();
        assert_eq!(public.status, status);
        assert_eq!(public.message, message);
        assert_eq!(public.code, code);
        for protocol in all_protocols() {
            let adapter = inbound_adapter_for(&protocol)
                .ok_or_else(|| BitrouterError::internal("missing built-in adapter"))?;
            let wire = adapter
                .stream_encoder("request", "model")
                .encode_stream_error(&public)
                .iter()
                .map(SseFrame::to_wire)
                .collect::<String>();
            assert!(!wire.contains("secret"), "{protocol:?}: {wire}");
            assert!(wire.contains(message), "{protocol:?}: {wire}");
            assert!(wire.contains(code), "{protocol:?}: {wire}");
        }
    }
    let credential_error =
        BitrouterError::from(ModelError::invalid_credential("invalid header value"));
    assert_eq!(credential_error.status(), 500);
    let invalid = BitrouterError::from(ModelError::invalid_request("missing model"));
    assert_eq!(invalid.status(), 400);
    Ok(())
}

#[test]
fn rate_limit_presentation_survives_all_sse_codecs() -> bitrouter::gateway::error::Result<()> {
    let error = BitrouterError::UpstreamRateLimited {
        retry_after: Some(9),
        detail: Some("secret provider diagnostic".into()),
    };
    let public = error.stream_error();
    for protocol in all_protocols() {
        let adapter = inbound_adapter_for(&protocol)
            .ok_or_else(|| BitrouterError::internal("missing built-in adapter"))?;
        let wire = adapter
            .stream_encoder("request", "model")
            .encode_stream_error(&public)
            .iter()
            .map(SseFrame::to_wire)
            .collect::<String>();
        assert!(
            wire.contains("upstream_rate_limited"),
            "{protocol:?}: {wire}"
        );
        assert!(!wire.contains("secret"), "{protocol:?}: {wire}");
        match protocol {
            ApiProtocol::ChatCompletions | ApiProtocol::Messages => {
                assert!(wire.contains("rate_limit_error"), "{wire}");
            }
            ApiProtocol::Responses => assert!(wire.contains("response.failed"), "{wire}"),
            ApiProtocol::Decisions | ApiProtocol::Custom(_) => {}
        }
    }
    Ok(())
}

#[test]
fn selected_model_target_uses_overrides_and_redacts_credentials()
-> bitrouter::gateway::error::Result<()> {
    let mut target = RoutingTarget {
        provider_name: "provider".into(),
        service_id: "native-model".into(),
        api_base: "https://original.invalid".into(),
        api_key: "original-secret".into(),
        api_protocol: ApiProtocol::ChatCompletions,
        chat_token_limit_field: Some(ChatTokenLimitField::MaxCompletionTokens),
        chat_supports_store: Some(false),
        chat_supports_stream_options: Some(true),
        chat_google_extensions: false,
        model_constraints: Default::default(),
        reasoning_effort: None,
        account_label: Some("selected-account".into()),
        api_key_override: Some("override-secret".into()),
        api_base_override: Some("https://override.invalid".into()),
        auth_scheme: Default::default(),
        headers: vec![OutboundHeaderRule::new(
            "x-custom",
            Some("header-secret"),
            false,
        )?],
    };
    let selected = target.model_target();
    assert_eq!(selected.credential_priority, CredentialPriority::Explicit);
    assert_eq!(selected.api_key, "override-secret");
    assert_eq!(selected.api_base, "https://override.invalid");
    assert_eq!(selected.service_id, "native-model");
    assert_eq!(selected.api_protocol, target.api_protocol);
    assert_eq!(selected.account_label.as_deref(), Some("selected-account"));
    let compatibility = &selected.compatibility.chat_completions;
    assert_eq!(
        compatibility.token_limit_field,
        target.chat_token_limit_field
    );
    assert_eq!(compatibility.supports_store, Some(false));
    assert_eq!(compatibility.supports_stream_options, Some(true));
    let diagnostic = format!("{selected:?}");
    assert!(!diagnostic.contains("secret"));
    assert!(!diagnostic.contains("selected-account"));
    assert_eq!(target.effective_api_key(), "override-secret");
    assert_eq!(target.api_key, "original-secret");
    target.api_key_override = None;
    assert_eq!(
        target.model_target().credential_priority,
        CredentialPriority::Fallback
    );
    target.provider_name = "bitrouter".into();
    assert_eq!(
        target.model_target().explicit_credential(),
        Some("original-secret")
    );
    target.api_key.clear();
    assert_eq!(target.model_target().explicit_credential(), None);
    Ok(())
}

#[test]
fn invocation_failures_retain_sdk_policy_and_cancellation_does_not_fallback() {
    let target = RoutingTarget {
        provider_name: "provider".into(),
        service_id: "model".into(),
        api_base: "https://fixture.invalid".into(),
        api_key: "secret".into(),
        api_protocol: ApiProtocol::ChatCompletions,
        chat_token_limit_field: None,
        chat_supports_store: None,
        chat_supports_stream_options: None,
        chat_google_extensions: false,
        model_constraints: Default::default(),
        reasoning_effort: None,
        account_label: None,
        api_key_override: None,
        api_base_override: None,
        auth_scheme: Default::default(),
        headers: Vec::new(),
    };
    let cases = [
        (ModelError::Timeout, 504, true),
        (
            ModelError::Transport {
                message: "connection closed".into(),
            },
            502,
            true,
        ),
        (
            ModelError::Decode {
                message: "invalid JSON".into(),
            },
            502,
            true,
        ),
        (
            ModelError::HttpResponse {
                status: 429,
                body: "busy".into(),
                retry_after: Some(7),
            },
            429,
            true,
        ),
        (
            ModelError::HttpResponse {
                status: 400,
                body: r#"{"error":{"message":"bad input"}}"#.into(),
                retry_after: None,
            },
            400,
            false,
        ),
        (ModelError::Cancelled, 499, false),
        (
            ModelError::CredentialStorage {
                failure: bitrouter::gateway::providers::auth::store::StoreError::Unavailable,
            },
            500,
            false,
        ),
        (
            ModelError::CredentialStorage {
                failure: bitrouter::gateway::providers::auth::store::StoreError::Conflict,
            },
            500,
            false,
        ),
    ];
    for (domain, status, retryable) in cases {
        let error = BitrouterError::from(domain);
        assert_eq!(error.status(), status);
        let decision = DefaultFallbackPolicy.classify(&error, &target);
        assert_eq!(matches!(decision, FallbackDecision::TryNext), retryable);
        if !retryable {
            assert!(matches!(decision, FallbackDecision::Fail(_)));
        }
    }
    let cancelled = BitrouterError::from(ModelError::Cancelled).stream_error();
    assert_eq!(cancelled.message, "request cancelled");
    assert_eq!(cancelled.code, "request_cancelled");
}

#[test]
fn conversion_errors_keep_reports_and_project_request_vs_output_status()
-> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    use bitrouter::gateway::model::conversion::request_admission;
    use bitrouter::gateway::model::error::ModelError;
    use bitrouter::gateway::model::protocol::chat_completions::ChatCompletionsAdapter;
    use bitrouter::gateway::model::protocol::responses::ResponsesAdapter;
    use bitrouter::gateway::model::protocol::{InboundAdapter, OutboundAdapter};
    use bitrouter_ai::conversion::{ConversionDisposition, ConversionLocation, ConversionStage};
    let native = serde_json::json!({"type":"reasoning","id":"rs-secret","summary":[],"encrypted_content":"opaque-secret"});
    let source = ResponsesAdapter
        .parse_request(serde_json::json!({"model":"fixture","input":[native.clone()]}))?;
    let report = request_admission(&source, &ApiProtocol::Responses);
    let request_error = BitrouterError::from(ModelError::Incompatible {
        report: report.clone(),
    });
    assert_eq!(request_error.status(), 400);
    assert_eq!(request_error.error_code(), "model_conversion_incompatible");
    assert_eq!(
        request_error.kind(),
        bitrouter::gateway::error::ErrorKind::Incompatible
    );
    assert!(
        matches!(&request_error,BitrouterError::Incompatible {report: retained} if retained == &report)
    );
    let result = ResponsesAdapter.parse_response(serde_json::json!({"output":[native]}))?;
    let error = ChatCompletionsAdapter
        .render_response(&result, &source, "request-id")
        .err()
        .ok_or("native output lost without error")?;
    let output_error = BitrouterError::from(error);
    assert_eq!(output_error.status(), 502);
    let BitrouterError::Incompatible { report } = &output_error else {
        return Err("output report lost".into());
    };
    assert_eq!(report.issues[0].stage, ConversionStage::ResponseEncoding);
    assert_eq!(
        report.issues[0].location,
        ConversionLocation::OutputContent { block: 0 }
    );
    assert_eq!(
        report.issues[0].disposition,
        ConversionDisposition::FailOutput
    );
    let mut encoder = ChatCompletionsAdapter.stream_encoder("request-id", "fixture");
    let part = bitrouter_ai::types::StreamPart::ReasoningEnd {
        id: "rs-secret".into(),
        signature: None,
        native: Some(bitrouter_ai::types::NativeReasoning::Responses(
            serde_json::json!({"type":"reasoning","encrypted_content":"opaque-secret"}),
        )),
    };
    let error = encoder
        .encode(&part)
        .err()
        .ok_or("native stream lost without error")?;
    let stream_error = BitrouterError::from(error);
    assert_eq!(stream_error.status(), 502);
    let BitrouterError::Incompatible { report } = &stream_error else {
        return Err("stream report lost".into());
    };
    assert_eq!(report.issues[0].stage, ConversionStage::StreamEncoding);
    for error in [request_error, output_error, stream_error] {
        assert!(!format!("{error:?}").contains("secret"));
        assert!(!serde_json::to_string(&error.to_envelope())?.contains("secret"));
        assert!(!error.public_message().contains("secret"));
    }
    Ok(())
}

/// Response DTOs are checked against service codecs, rather than only synthetic
/// SDK fixtures. No codec or canonical accounting implementation lives in AI.
#[test]
fn public_response_contracts_decode_gateway_rendering() -> anyhow::Result<()> {
    use bitrouter::gateway::model::protocol::OutboundDispatch;
    use bitrouter_ai::protocol::{
        chat_completions::ChatResponse, messages::MessagesResponse, responses::ResponsesResponse,
    };
    use serde_json::json;
    let cases = [
        (
            ApiProtocol::ChatCompletions,
            json!({"model":"route","messages":[{"role":"user","content":"hello"}]}),
            json!({"id":"upstream","model":"native","choices":[{"message":{"role":"assistant","content":"hello"},"finish_reason":"stop"}],"usage":{"prompt_tokens":9,"completion_tokens":2,"total_tokens":11,"prompt_tokens_details":{"cached_tokens":3}}}),
        ),
        (
            ApiProtocol::Messages,
            json!({"model":"route","messages":[{"role":"user","content":"hello"}]}),
            json!({"id":"upstream","type":"message","role":"assistant","model":"native","content":[{"type":"text","text":"hello"}],"stop_reason":"end_turn","usage":{"input_tokens":6,"output_tokens":2,"cache_read_input_tokens":3}}),
        ),
        (
            ApiProtocol::Responses,
            json!({"model":"route","input":"hello"}),
            json!({"id":"upstream","object":"response","model":"native","status":"completed","output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"hello","annotations":[]}]}],"usage":{"input_tokens":9,"output_tokens":2,"total_tokens":11,"input_tokens_details":{"cached_tokens":3},"output_tokens_details":{"reasoning_tokens":0}}}),
        ),
    ];
    for (protocol, request, response) in cases {
        let inbound = inbound_adapter_for(&protocol)
            .ok_or_else(|| anyhow::anyhow!("missing inbound codec"))?;
        let dispatch = OutboundDispatch::builtin();
        let (outbound, _) = dispatch
            .lookup(&protocol)
            .ok_or_else(|| anyhow::anyhow!("missing outbound codec"))?;
        let prompt = inbound.parse_request(request)?;
        let result = outbound.parse_response(response)?;
        let wire = inbound.render_response(&result, &prompt, "request")?;
        let round_trip = match protocol {
            ApiProtocol::ChatCompletions => {
                serde_json::to_value(serde_json::from_value::<ChatResponse>(wire.clone())?)?
            }
            ApiProtocol::Messages => {
                serde_json::to_value(serde_json::from_value::<MessagesResponse>(wire.clone())?)?
            }
            ApiProtocol::Responses => {
                serde_json::to_value(serde_json::from_value::<ResponsesResponse>(wire.clone())?)?
            }
            _ => return Err(anyhow::anyhow!("unexpected protocol")),
        };
        assert_eq!(round_trip, wire, "{protocol:?}");
    }
    Ok(())
}

#[test]
fn public_event_contracts_decode_gateway_lifecycle_and_failure_frames() -> anyhow::Result<()> {
    use bitrouter_ai::{
        gateway::GatewayErrorEnvelope,
        protocol::{
            chat_completions::ChatChunk, messages::MessagesEvent, responses::ResponsesEvent,
        },
        types::{FinishReason, StreamPart, Usage},
    };
    for protocol in all_protocols() {
        let adapter =
            inbound_adapter_for(&protocol).ok_or_else(|| anyhow::anyhow!("missing codec"))?;
        let mut encoder = adapter.stream_encoder("request", "route");
        let mut frames = Vec::new();
        for part in [
            StreamPart::TextDelta {
                text: "hello".into(),
            },
            StreamPart::Usage {
                usage: Usage {
                    prompt_tokens: 9,
                    completion_tokens: 2,
                    cache_read_tokens: 3,
                    ..Default::default()
                },
            },
            StreamPart::Finish {
                reason: FinishReason::Stop,
            },
        ] {
            frames.extend(encoder.encode(&part)?);
        }
        frames.extend(encoder.finish()?);
        frames.extend(
            adapter
                .stream_encoder("request", "route")
                .encode_stream_error(&BitrouterError::UpstreamUnavailable.stream_error()),
        );
        for frame in frames {
            let SseFrame::Event { data, .. } = frame else {
                continue;
            };
            if data == "[DONE]" {
                continue;
            }
            let wire: serde_json::Value = serde_json::from_str(&data)?;
            if wire.get("error").is_some() {
                let _: GatewayErrorEnvelope = serde_json::from_value(wire)?;
                continue;
            }
            let round_trip = match protocol {
                ApiProtocol::ChatCompletions => {
                    serde_json::to_value(serde_json::from_value::<ChatChunk>(wire.clone())?)?
                }
                ApiProtocol::Messages => {
                    serde_json::to_value(serde_json::from_value::<MessagesEvent>(wire.clone())?)?
                }
                ApiProtocol::Responses => {
                    serde_json::to_value(serde_json::from_value::<ResponsesEvent>(wire.clone())?)?
                }
                _ => return Err(anyhow::anyhow!("unexpected protocol")),
            };
            assert_eq!(round_trip, wire, "{protocol:?}");
        }
    }
    Ok(())
}
