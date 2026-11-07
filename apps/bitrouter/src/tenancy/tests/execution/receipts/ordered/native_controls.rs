//! Provider-owned opaque controls use current ordered authority over real HTTP.
use super::*;
use bitrouter_ai::tenancy::{
    generation_defaults::GenerationDefaultsEnvelopeV2,
    native_controls::{HEADER, PROVIDER_NATIVE_V1},
};

mod boundaries;
mod predecessor;

pub(super) fn provider_reply(
    controls: &ProviderControls,
    protocol: &str,
    body: &Value,
    usage: Value,
) -> Response {
    let Ok(mut state) = controls.native_state.lock() else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    let text = if let Some(previous) = body.get("previous_response_id").filter(|v| !v.is_null()) {
        if previous != "resp_receipts" || state.is_none() {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error":{
                "type":"invalid_request_error", "code":"unknown_previous_response",
                "message":"unknown previous response"}})),
            )
                .into_response();
        }
        match state.as_ref() {
            Some(text) => text.clone(),
            None => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        }
    } else {
        // Only a fixture initialization request creates retained provider state.
        // Subsequent input contains no copy of the remembered text.
        if body.to_string().contains("remember:violet") {
            *state = Some("violet".into());
        }
        "stored".into()
    };
    drop(state);
    if body["stream"] == true {
        return match canonical::provider_stream(protocol, usage) {
            Ok(events) => (
                [("content-type", "text/event-stream")],
                events.replace("receipt reply", &text),
            )
                .into_response(),
            Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        };
    }
    match json_response(protocol, usage) {
        Ok(mut response) => {
            if protocol == "responses" {
                response["output"][0]["content"][0]["text"] = json!(text);
            } else {
                response["content"][0]["text"] = json!(text);
            }
            Json(response).into_response()
        }
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

async fn harness(
    protocols: &[&str],
    enabled: bool,
    malformed: Option<usize>,
) -> Result<OrderedHarness> {
    OrderedHarness::with_native_mode(
        protocols.len(),
        8,
        malformed,
        Value::Null,
        None,
        enabled,
        |resources| set_protocols(resources, protocols),
    )
    .await
}

fn set_protocols(resources: &mut [Value], protocols: &[&str]) -> Result<()> {
    let route = resources
        .iter_mut()
        .find(|row| row["reference"]["resource_kind"] == "route")
        .context("route")?;
    for (index, protocol) in protocols.iter().enumerate() {
        route["definition"]["routing"]["providers"][format!("provider-{index}")]["api_protocol"] =
            json!([{"*":protocol}]);
    }
    Ok(())
}

fn bodies(harness: &OrderedHarness) -> Result<Vec<Value>> {
    ensure!(
        harness
            .observed
            .lock()
            .map_err(|_| anyhow::anyhow!("headers"))?
            .iter()
            .all(|(_, headers)| !headers.contains_key(HEADER))
    );
    Ok(harness
        .controls
        .bodies
        .lock()
        .map_err(|_| anyhow::anyhow!("bodies"))?
        .clone())
}

async fn invoke(
    harness: &OrderedHarness,
    binding: &OrderedDispatchBindingV2,
    protocol: GenerationProtocol,
    mut input: Value,
    defaults: Option<GenerationDefaultsEnvelopeV2>,
) -> Result<(ExecutionId, String)> {
    let capabilities = harness
        .control(false, HttpVersion::Http1Only)?
        .ordered_native_controls_capabilities()
        .await?;
    ensure!(capabilities.enabled);
    let client = GatewayClient::new(&harness.endpoint, None)?;
    let mut call = client
        .ordered_tenant(harness.token()?, binding)?
        .with_native_controls(&capabilities)?;
    if let Some(defaults) = defaults {
        call = call.with_generation_defaults_envelope_v2(defaults)?;
    }
    input["model"] = json!(MODEL);
    let stream = input["stream"] == true;
    macro_rules! send {
        ($method:ident,$request:ty,$end:expr) => {{
            let reply = call
                .$method(&serde_json::from_value::<$request>(input)?)
                .await?;
            ensure!(reply.metadata.status == 200);
            let id = reply.metadata.execution_id.context("execution")?;
            let mut text = String::new();
            match reply.body {
                Body::Json(body) => {
                    ensure!(!stream);
                    text = serde_json::to_string(&body)?;
                }
                Body::Stream(mut events) => {
                    ensure!(stream);
                    let mut terminal = None;
                    while let Some(event) = events.next().await {
                        match event? {
                            StreamItem::Event(value) => {
                                text.push_str(&serde_json::to_string(&value.data)?)
                            }
                            StreamItem::End(end) => {
                                ensure!(terminal.is_none());
                                terminal = Some(end);
                            }
                        }
                    }
                    ensure!(terminal == Some($end));
                }
            }
            (id, text)
        }};
    }
    Ok(match protocol {
        GenerationProtocol::Responses => send!(
            responses_reply,
            ResponsesRequest,
            ProtocolEnd::ResponsesCompleted
        ),
        GenerationProtocol::Messages => {
            send!(messages_reply, MessagesRequest, ProtocolEnd::MessagesStop)
        }
        GenerationProtocol::ChatCompletions => {
            send!(chat_reply, ChatRequest, ProtocolEnd::ChatDone)
        }
    })
}

fn defaults(tokens: u32, supplemental: Value) -> Result<GenerationDefaultsEnvelopeV2> {
    Ok(serde_json::from_value(json!({"schema_version":2,
        "configuration_defaults":"preserve_absence",
        "canonical":{"schema_version":1,"max_tokens":tokens},"supplemental":supplemental}))?)
}

#[tokio::test]
async fn native_provider_state_json_sse_and_literal_public_prefix_are_not_managed_handles()
-> Result<()> {
    for stream in [false, true] {
        let harness = harness(&["responses"], true, None).await?;
        harness.controls.stateful.store(true, Ordering::SeqCst);
        let binding = harness
            .binding(vec![0], GenerationProtocol::Responses)
            .await?;
        let (_, first) = invoke(
            &harness,
            &binding,
            GenerationProtocol::Responses,
            json!({"input":"remember:violet","stream":stream}),
            None,
        )
        .await?;
        ensure!(first.contains("stored"));
        // The ordinary gateway response identity is unchanged. Raw provider
        // selectors are supplied independently; this is not owned-handle support.
        ensure!(first.contains("brc_"));
        let (id, next) = invoke(&harness, &binding, GenerationProtocol::Responses,
            json!({"input":"recall the remembered word","previous_response_id":"resp_receipts","stream":stream}), None).await?;
        ensure!(next.contains("violet"));
        let receipt = harness.receipt(&id).await?;
        ensure!(receipt.outcome == Outcome::Completed && receipt.attempts.len() == 1);
        let caps = harness
            .control(false, HttpVersion::Http1Only)?
            .ordered_native_controls_capabilities()
            .await?;
        let client = GatewayClient::new(&harness.endpoint, None)?;
        let error = client
            .ordered_tenant(harness.token()?, &binding)?
            .with_native_controls(&caps)?
            .responses_reply(&serde_json::from_value(
                json!({"model":MODEL,"input":"recall",
                "previous_response_id":"brc_literal_unknown","stream":stream}),
            )?)
            .await
            .err()
            .context("provider refusal")?;
        let bitrouter_sdk::gateway::tenancy::Error::Response(error) = error else {
            anyhow::bail!("expected provider HTTP refusal");
        };
        ensure!(error.metadata.status == 400 && error.metadata.execution_id.is_some());
        ensure!(serde_json::to_string(&error.gateway_error)?.contains("unknown previous response"));
        let captured = bodies(&harness)?;
        ensure!(captured.len() == 3);
        ensure!(!captured[1].to_string().contains("violet"));
        ensure!(captured[1]["previous_response_id"] == "resp_receipts");
        ensure!(captured[2]["previous_response_id"] == "brc_literal_unknown");
        ensure!(harness.observed()? == ["model-0", "model-0", "model-0"]);
        harness.fixture.service.shutdown().await?;
    }
    Ok(())
}

#[tokio::test]
async fn native_conversation_and_messages_context_survive_json_sse_unchanged() -> Result<()> {
    for stream in [false, true] {
        for (protocol, wire, field, value) in [
            (
                GenerationProtocol::Responses,
                "responses",
                "conversation",
                json!("conv_native"),
            ),
            (
                GenerationProtocol::Responses,
                "responses",
                "conversation",
                json!({"id":"conv_native"}),
            ),
            (
                GenerationProtocol::Messages,
                "messages",
                "context_management",
                json!({"edits":[{"type":"clear_tool_uses_20250919","keep":{"type":"tool_uses","value":3}}]}),
            ),
        ] {
            let harness = harness(&[wire], true, None).await?;
            let binding = harness.binding(vec![0], protocol).await?;
            let mut input = if protocol == GenerationProtocol::Responses {
                json!({"input":"next","stream":stream})
            } else {
                json!({"messages":[{"role":"user","content":"next"}],"max_tokens":17,"stream":stream})
            };
            input[field] = value.clone();
            let (id, _) = invoke(&harness, &binding, protocol, input, None).await?;
            ensure!(harness.receipt(&id).await?.outcome == Outcome::Completed);
            let captured = bodies(&harness)?;
            ensure!(captured.len() == 1 && captured[0][field] == value);
            harness.fixture.service.shutdown().await?;
        }
    }
    Ok(())
}

#[tokio::test]
async fn native_effective_null_precedence_skips_foreign_candidates_without_renumbering()
-> Result<()> {
    for (source, supplemental, expected) in [
        // The existing post-parse merge suppresses a supplemental key when
        // source extras already contain it, including a source null. It never
        // reaches the foreign target as a non-null effective control.
        (Some(Value::Null), json!("default-id"), vec![1, 2, 0]),
        (Some(Value::Null), Value::Null, vec![1, 2, 0]),
        (None, json!("default-id"), vec![2, 0]),
        (Some(json!("source-id")), json!("default-id"), vec![2, 0]),
    ] {
        let harness = harness(&["responses", "messages", "responses"], true, None).await?;
        let original = harness
            .binding(vec![1, 2, 0], GenerationProtocol::Responses)
            .await?;
        let envelope = defaults(17, json!({"previous_response_id":supplemental}))?;
        let binding = original.with_generation_defaults_envelope_v2(&envelope)?;
        let mut input = json!({"input":"next"});
        if let Some(value) = &source {
            input["previous_response_id"] = value.clone();
        }
        let (id, _) = invoke(
            &harness,
            &binding,
            GenerationProtocol::Responses,
            input,
            Some(envelope),
        )
        .await?;
        let receipt = harness.receipt(&id).await?;
        ensure!(receipt.accepted_selection.candidate_indices == [1, 2, 0]);
        ensure!(
            receipt
                .attempts
                .iter()
                .map(|a| a.candidate_index)
                .collect::<Vec<_>>()
                == expected,
            "source={source:?} supplemental={supplemental:?}; actual={:?}; sends={:?}",
            receipt
                .attempts
                .iter()
                .map(|a| a.candidate_index)
                .collect::<Vec<_>>(),
            harness.observed()?
        );
        let captured = bodies(&harness)?;
        ensure!(captured.len() == expected.len());
        for body in captured {
            let expected_value = match source.as_ref() {
                Some(value) if body["model"] != "model-1" => Some(value),
                Some(_) => None,
                None => Some(&supplemental),
            };
            ensure!(body.get("previous_response_id") == expected_value);
        }
        harness.fixture.service.shutdown().await?;
    }
    Ok(())
}
