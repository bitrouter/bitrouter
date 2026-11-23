//! Configured Chat transport preserves ingress identity and original evidence.

use super::*;
use bitrouter_ai::tenancy::{
    execution::ReceiptState,
    native_controls::{HEADER, PROVIDER_NATIVE_V1},
    usage_evidence::{Dimension, ExecutionUsageEvidence, Protocol, Source},
};

async fn chat_only(url: &str) -> Result<Harness> {
    let mut harness = Harness::new(url).await?;
    let provider = harness
        .config
        .providers
        .get_mut("mock")
        .context("configured provider")?;
    // The model override must win over an otherwise native-capable provider.
    provider.api_protocol = serde_json::from_value(json!([
        {"*":["chat_completions","messages","responses"]}
    ]))?;
    provider
        .models
        .first_mut()
        .context("configured model")?
        .api_protocol = Some(serde_json::from_value(json!(["chat_completions"]))?);
    harness.config.tenancy.ordered_native_controls = true;
    harness.restart().await
}

fn request(protocol: GenerationProtocol, stream: bool) -> Value {
    match protocol {
        GenerationProtocol::Responses => {
            json!({"model":ROUTE,"input":"hello","max_output_tokens":7,"stream":stream})
        }
        _ => json!({"model":ROUTE,"messages":[{"role":"user","content":"hello"}],
            "max_tokens":7,"stream":stream}),
    }
}

async fn send(
    fixture: &Fixture,
    headers: HeaderMap,
    path: &str,
    body: Value,
) -> Result<(StatusCode, Option<ExecutionId>, String)> {
    let router = Router::new()
        .fallback(|| async { StatusCode::NOT_FOUND })
        .layer(axum::middleware::from_fn_with_state(
            fixture.service.clone(),
            crate::tenancy::http::dispatch,
        ));
    let mut request = Request::post(path).body(Body::from(serde_json::to_vec(&body)?))?;
    *request.headers_mut() = headers;
    request
        .headers_mut()
        .insert("content-type", "application/json".parse()?);
    let response = tokio::time::timeout(TIMEOUT, router.oneshot(request)).await??;
    let status = response.status();
    let id = response
        .headers()
        .get("x-bitrouter-request-id")
        .map(|value| -> Result<ExecutionId> { Ok(value.to_str()?.try_into()?) })
        .transpose()?;
    let body = tokio::time::timeout(
        TIMEOUT,
        axum::body::to_bytes(response.into_body(), 64 * 1024),
    )
    .await??;
    tokio::time::timeout(TIMEOUT, fixture.service.admission.wait_drained()).await?;
    Ok((status, id, String::from_utf8(body.to_vec())?))
}

fn check_output(
    protocol: GenerationProtocol,
    stream: bool,
    body: &str,
    id: &ExecutionId,
) -> Result<()> {
    if !stream {
        let response: Value = serde_json::from_str(body)?;
        if protocol == GenerationProtocol::Responses {
            ensure!(response["status"] == "completed");
            ensure!(response["output"][0]["summary"][0]["text"] == "shared reasoning");
            ensure!(response["output"][1]["content"][0]["text"] == "shared reply");
            ensure!(response["usage"]["input_tokens"] == 2);
            ensure!(response["usage"]["output_tokens"] == 1);
            ensure!(
                crate::gateway::model::protocol::responses::decode_gateway_continuation_id(
                    response["id"].as_str().context("Responses id")?
                )? == Some(id.to_string())
            );
        } else {
            ensure!(response["type"] == "message" && response["stop_reason"] == "end_turn");
            ensure!(response["content"] == json!([{"type":"text","text":"shared reply"}]));
            ensure!(response["usage"]["input_tokens"] == 2);
            ensure!(response["usage"]["output_tokens"] == 1);
        }
        return Ok(());
    }
    let events = body
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .map(serde_json::from_str::<Value>)
        .collect::<std::result::Result<Vec<_>, _>>()?;
    ensure!(!events.is_empty() && !body.contains("[DONE]"));
    ensure!(
        events
            .iter()
            .all(|event| { !matches!(event["type"].as_str(), Some("error" | "response.failed")) })
    );
    if protocol == GenerationProtocol::Responses {
        for (sequence, event) in events.iter().enumerate() {
            ensure!(event["sequence_number"] == u64::try_from(sequence)?);
        }
        let reasoning = events
            .iter()
            .filter(|event| event["type"] == "response.reasoning_text.delta")
            .filter_map(|event| event["delta"].as_str())
            .collect::<String>();
        ensure!(reasoning == "shared reasoning");
        ensure!(
            !events
                .iter()
                .any(|event| { event["type"] == "response.reasoning_summary_text.delta" })
        );
        let closed = events
            .iter()
            .find(|event| {
                event["type"] == "response.output_item.done" && event["item"]["type"] == "reasoning"
            })
            .context("closed reasoning item")?;
        ensure!(closed["output_index"] == 0);
        ensure!(closed["item"]["content"] == json!([{"type":"reasoning_text","text":reasoning}]));
        ensure!(closed["item"]["summary"] == json!([]));
        for event in events.iter().filter(|event| {
            matches!(
                event["type"].as_str(),
                Some("response.reasoning_text.delta" | "response.reasoning_text.done")
            )
        }) {
            ensure!(event["item_id"] == closed["item"]["id"]);
            ensure!(event["output_index"] == closed["output_index"]);
            ensure!(event["content_index"] == 0);
            if event["type"] == "response.reasoning_text.done" {
                ensure!(event["text"] == reasoning);
            }
        }
        let terminal = events.last().context("Responses terminal")?;
        ensure!(terminal["type"] == "response.completed");
        ensure!(terminal["response"]["output"][0] == closed["item"]);
        ensure!(terminal["response"]["output"][1]["content"][0]["text"] == "shared reply");
        ensure!(terminal["response"]["usage"]["input_tokens"] == 2);
        ensure!(terminal["response"]["usage"]["output_tokens"] == 1);
        ensure!(
            crate::gateway::model::protocol::responses::decode_gateway_continuation_id(
                terminal["response"]["id"]
                    .as_str()
                    .context("Responses terminal id")?
            )? == Some(id.to_string())
        );
    } else {
        ensure!(events.first().context("Messages start")?["type"] == "message_start");
        ensure!(events.last().context("Messages stop")?["type"] == "message_stop");
        let text = events
            .iter()
            .filter(|event| event["delta"]["type"] == "text_delta")
            .filter_map(|event| event["delta"]["text"].as_str())
            .collect::<String>();
        ensure!(text == "shared reply");
        // Unsigned Chat reasoning must not invent replayable Anthropic thinking.
        ensure!(!body.contains("shared reasoning") && !body.contains("signature_delta"));
        ensure!(
            events
                .iter()
                .filter(|event| event["type"] == "content_block_start")
                .all(|event| event["content_block"]["type"] == "text")
        );
        let terminal = events
            .iter()
            .find(|event| event["type"] == "message_delta")
            .context("Messages completion")?;
        ensure!(terminal["delta"]["stop_reason"] == "end_turn");
        ensure!(terminal["usage"]["input_tokens"] == 2);
        ensure!(terminal["usage"]["output_tokens"] == 1);
        // Chat reports usage at the end, after the provisional Messages start.
        ensure!(events.first().context("Messages usage")?["message"]["usage"]["input_tokens"] == 0);
    }
    Ok(())
}

async fn roundtrip(protocol: GenerationProtocol, path: &str) -> Result<()> {
    let mut provider = Provider::start().await?;
    let mut harness = chat_only(&provider.url).await?;
    let descriptor = harness
        .fixture
        .service
        .catalog_candidates(
            &harness.fixture.operator,
            &harness.fixture.tenant,
            ROUTE,
            protocol,
        )
        .await
        .map_err(failure)?;
    descriptor.validate()?;
    ensure!(descriptor.manifest.inbound_protocol == protocol);
    ensure!(descriptor.manifest.candidates.len() == 1 && descriptor.sources.len() == 1);
    ensure!(descriptor.sources[0].outbound_protocol == ApiProtocol::ChatCompletions);
    ensure!(
        descriptor
            .templates
            .iter()
            .all(|template| template.outbound_protocol == ApiProtocol::ChatCompletions)
    );
    let candidate = descriptor
        .manifest
        .candidates
        .first()
        .context("configured candidate")?;
    ensure!(candidate.admission_profile.is_some());
    let credential = grant(
        &mut harness.fixture,
        scope(&descriptor),
        TenantRole::TenantAdmin.permissions().to_vec(),
        "chat-only-conversion",
    )
    .await?;
    for stream in [false, true] {
        let (status, id, body) = send(
            &harness.fixture,
            headers(&harness.fixture, &credential, &descriptor).await?,
            path,
            request(protocol, stream),
        )
        .await?;
        ensure!(
            status == StatusCode::OK,
            "configured conversion: {status} {body}"
        );
        let id = id.context("configured execution id")?;
        check_output(protocol, stream, &body, &id)?;
        let (authorization, sent) = tokio::time::timeout(TIMEOUT, provider.received.recv())
            .await?
            .context("actual Chat send")?;
        ensure!(authorization == format!("Bearer {SECRET}"));
        ensure!(sent["model"] == "native-shared" && sent["stream"] == stream);
        ensure!(sent["messages"][0]["role"] == "user" && sent["messages"][0]["content"] == "hello");
        ensure!(sent["max_tokens"] == 7 && sent.get("input").is_none());
        let receipt = receipt(&harness.fixture, id.clone()).await?;
        ensure!(receipt.outcome == Outcome::Completed && receipt.attempts.len() == 1);
        let accepted = accepted_selection(&receipt.dispatch)?;
        ensure!(accepted.inbound_protocol == protocol && accepted.candidate_indices == vec![0]);
        ensure!(accepted.admission_mode == descriptor.manifest.admission_mode);
        ensure!(accepted.route == descriptor.manifest.route);
        let attempt = receipt.attempts.first().context("actual attempt")?;
        ensure!(attempt.candidate_index == Some(0));
        ensure!(
            serde_json::to_value(attempt.target.as_ref().context("actual target")?)?
                == serde_json::to_value(&candidate.target)?
        );
        ensure!(receipt.usage.input_tokens == Some(2) && receipt.usage.output_tokens == Some(1));
        ensure!(
            receipt.usage.cache_read_tokens.is_none()
                && receipt.usage.cache_write_tokens.is_none()
                && receipt.usage.reasoning_tokens.is_none()
                && !receipt.usage.complete
        );
        ensure!(receipt.state == ReceiptState::Incomplete);
        let bytes = harness
            .fixture
            .service
            .read_receipt(
                harness.fixture.operator.clone(),
                harness.fixture.tenant.clone(),
                crate::tenancy::receipts::read::Request::UsageEvidence(id),
            )
            .await
            .map_err(failure)?;
        let evidence: ExecutionUsageEvidence = serde_json::from_slice(&bytes)?;
        evidence.validate_against(&receipt)?;
        ensure!(evidence.frames.len() == 1 && !evidence.truncated && evidence.dropped_frames == 0);
        ensure!(evidence.attempts.len() == 1 && evidence.attempts[0].terminal_observed);
        let frame = evidence.frames.first().context("original usage frame")?;
        ensure!(
            frame.protocol == Protocol::ChatCompletions && !frame.unsupported && !frame.invalid
        );
        ensure!(frame.attempt_id == attempt.attempt_id);
        ensure!(
            frame.source
                == if stream {
                    Source::ChatChunk
                } else {
                    Source::Json
                }
        );
        ensure!(frame.quantities.len() == 3);
        for (dimension, value) in [
            (Dimension::InputTokens, 2),
            (Dimension::OutputTokens, 1),
            (Dimension::TotalTokens, 3),
        ] {
            ensure!(
                frame.quantities.iter().any(
                    |quantity| quantity.dimension == dimension && quantity.value == Some(value)
                )
            );
        }
    }
    ensure!(provider.calls() == 2);
    harness.fixture.service.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn configured_chat_only_model_serves_messages_json_and_sse() -> Result<()> {
    roundtrip(GenerationProtocol::Messages, "/v1/messages").await
}

#[tokio::test]
async fn configured_chat_only_model_serves_responses_json_and_sse() -> Result<()> {
    roundtrip(GenerationProtocol::Responses, "/v1/responses").await
}

#[tokio::test]
async fn configured_chat_only_model_rejects_native_controls_before_provider_send() -> Result<()> {
    let mut provider = Provider::start().await?;
    let mut harness = chat_only(&provider.url).await?;
    for (protocol, path, controls) in [
        (
            GenerationProtocol::Responses,
            "/v1/responses",
            vec![
                ("previous_response_id", json!("opaque-provider-response")),
                ("conversation", json!("opaque-provider-conversation")),
                (
                    "cachedContent",
                    json!("cachedContents/opaque-provider-cache"),
                ),
            ],
        ),
        (
            GenerationProtocol::Messages,
            "/v1/messages",
            vec![("context_management", json!({"edits":[]}))],
        ),
    ] {
        let descriptor = harness
            .fixture
            .service
            .catalog_candidates(
                &harness.fixture.operator,
                &harness.fixture.tenant,
                ROUTE,
                protocol,
            )
            .await
            .map_err(failure)?;
        let credential = grant(
            &mut harness.fixture,
            scope(&descriptor),
            TenantRole::TenantAdmin.permissions().to_vec(),
            if protocol == GenerationProtocol::Responses {
                "responses-controls"
            } else {
                "messages-controls"
            },
        )
        .await?;
        let mut bound = headers(&harness.fixture, &credential, &descriptor).await?;
        bound.insert(HEADER, PROVIDER_NATIVE_V1.parse()?);
        // The same native-mode authority succeeds when the request has no control.
        let (status, id, _) = send(
            &harness.fixture,
            bound.clone(),
            path,
            request(protocol, false),
        )
        .await?;
        ensure!(status == StatusCode::OK);
        ensure!(
            receipt(&harness.fixture, id.context("baseline execution")?)
                .await?
                .outcome
                == Outcome::Completed
        );
        provider.observed(SECRET).await?;
        let before = provider.calls();
        for (field, value) in controls {
            for stream in [false, true] {
                let mut body = request(protocol, stream);
                body[field] = value.clone();
                let (status, id, _) = send(&harness.fixture, bound.clone(), path, body).await?;
                ensure!(
                    !status.is_success(),
                    "{field} silently reached Chat: {status}"
                );
                ensure!(provider.calls() == before, "{field} reached the provider");
                if let Some(id) = id {
                    ensure!(receipt(&harness.fixture, id).await?.attempts.is_empty());
                }
            }
        }
    }
    ensure!(provider.calls() == 2);
    harness.fixture.service.shutdown().await?;
    Ok(())
}
