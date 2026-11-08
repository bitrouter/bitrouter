//! Actual post-parse residual defaults, transport provenance and protected state.
use super::*;
use bitrouter_ai::tenancy::generation_defaults::{
    BODY_MEMBER, ENVELOPE_BODY_MEMBER, GenerationDefaultsEnvelopeV1,
};

pub(super) mod absence;
mod edge_cases;
mod precedence;
#[cfg(unix)]
mod predecessor;
mod refusals;

fn defaults(params: Value) -> Result<GenerationDefaultsEnvelopeV1> {
    Ok(serde_json::from_value(
        json!({"schema_version":1,"canonical":{"schema_version":1,"max_tokens":23},"supplemental":params}),
    )?)
}

pub(super) async fn invoke(
    harness: &OrderedHarness,
    binding: &OrderedDispatchBindingV2,
    defaults: GenerationDefaultsEnvelopeV1,
    protocol: GenerationProtocol,
    stream: bool,
    version: HttpVersion,
    fields: Value,
) -> Result<ExecutionId> {
    let client = GatewayClient::with_options(
        &harness.endpoint,
        None,
        TransportOptions {
            http_version: version,
            ..Default::default()
        },
    )?;
    let call = client
        .ordered_tenant(harness.token()?, binding)?
        .with_generation_defaults_envelope(defaults)?;
    let mut request = match protocol {
        GenerationProtocol::Responses => json!({"model":MODEL,"input":"hello","stream":stream}),
        _ => json!({"model":MODEL,"messages":[{"role":"user","content":"hello"}],"stream":stream}),
    };
    request
        .as_object_mut()
        .context("request")?
        .extend(fields.as_object().context("fields")?.clone());
    macro_rules! send {
        ($method:ident,$request:ty,$end:expr) => {{
            let reply = call
                .$method(&serde_json::from_value::<$request>(request)?)
                .await?;
            ensure!(reply.metadata.status == 200);
            let id = reply.metadata.execution_id.context("execution identity")?;
            match reply.body {
                Body::Json(body) => {
                    ensure!(!stream);
                    ensure!(serde_json::to_string(&body)?.contains("receipt reply"));
                }
                Body::Stream(mut events) => {
                    let mut terminal = None;
                    let mut text_seen = false;
                    while let Some(event) = events.next().await {
                        let event = event?;
                        if let StreamItem::Event(ref value) = event {
                            text_seen |=
                                serde_json::to_string(&value.data)?.contains("receipt reply");
                        }
                        if let StreamItem::End(end) = event {
                            ensure!(terminal.is_none());
                            terminal = Some(end);
                        }
                    }
                    ensure!(terminal == Some($end) && text_seen);
                }
            }
            id
        }};
    }
    Ok(match protocol {
        GenerationProtocol::ChatCompletions => {
            send!(chat_reply, ChatRequest, ProtocolEnd::ChatDone)
        }
        GenerationProtocol::Messages => {
            send!(messages_reply, MessagesRequest, ProtocolEnd::MessagesStop)
        }
        GenerationProtocol::Responses => send!(
            responses_reply,
            ResponsesRequest,
            ProtocolEnd::ResponsesCompleted
        ),
    })
}

fn bodies(harness: &OrderedHarness) -> Result<Vec<Value>> {
    Ok(harness
        .controls
        .bodies
        .lock()
        .map_err(|_| anyhow::anyhow!("body capture"))?
        .clone())
}

async fn harness(wire: &str, retained: Value) -> Result<OrderedHarness> {
    OrderedHarness::with_resource_edits(1, 8, None, retained, None, |resources| {
        let route = resources
            .iter_mut()
            .find(|row| row["reference"]["resource_kind"] == "route")
            .context("route")?;
        route["definition"]["routing"]["providers"]["provider-0"]["api_protocol"] =
            json!([{"*":wire}]);
        Ok(())
    })
    .await
}

async fn completed(
    harness: &OrderedHarness,
    id: &ExecutionId,
    binding: &OrderedDispatchBindingV2,
) -> Result<()> {
    let receipt = harness.receipt(id).await?;
    ensure!(receipt.outcome == Outcome::Completed);
    let messages = harness
        .observed
        .lock()
        .map_err(|_| anyhow::anyhow!("provider history"))?
        .last()
        .context("provider history")?
        .1
        .contains_key("x-api-key");
    ensure!(receipt.usage.input_tokens == Some(if messages { 16 } else { 11 }));
    ensure!(receipt.usage.output_tokens == Some(7));
    ensure!(
        receipt.usage.cache_read_tokens == Some(3) && receipt.usage.cache_write_tokens == Some(2)
    );
    ensure!(
        receipt.accepted_selection.generation_defaults_digest
            == binding.selection.generation_defaults_digest
    );
    ensure!(receipt.accepted_selection.route == binding.selection.route);
    ensure!(receipt.attempts.len() == 1 && receipt.attempts[0].candidate_index == 0);
    let evidence = harness
        .control(true, HttpVersion::Http1Only)?
        .usage_evidence_v2(&receipt)
        .await?;
    evidence.validate_against(&receipt)?;
    Ok(())
}

#[tokio::test]
async fn envelope_three_ingress_three_egress_h1_h2_json_sse_opaque_controls_and_collisions()
-> Result<()> {
    for wire in ["chat_completions", "messages", "responses"] {
        let harness = harness(wire, Value::Null).await?;
        let mut count = 0;
        for protocol in [
            GenerationProtocol::ChatCompletions,
            GenerationProtocol::Messages,
            GenerationProtocol::Responses,
        ] {
            for version in [
                HttpVersion::Http1Only,
                #[cfg(feature = "http2")]
                HttpVersion::Http2PriorKnowledge,
            ] {
                for stream in [false, true] {
                    for caller in [
                        json!({}),
                        json!({"vendor_knob":{"from":"caller"}}),
                        json!({"vendor_knob":null}),
                    ] {
                        let controls = json!({"provider":"opaque-provider", "headers":{"authorization":"opaque-auth","x-injected":"opaque"}, "tenant_id":"opaque-tenant", "api_key":"opaque-key", "api_base":"http://invalid.example", "authorization":"opaque-auth", "route":"opaque-route", "candidate_indices":[255], "extra_protocol":"opaque", "supplemental_extra":{"vendor_knob":"nested"}, "chat_stream_options":{"include_usage":false}, "bitrouter_custom":"opaque", "bitrouter-custom":"opaque", "generation_defaults_digest":"opaque-digest", "vendor_knob":{"from":"preset"}});
                        let defaults = defaults(controls.clone())?;
                        let binding = harness
                            .binding(vec![0], protocol)
                            .await?
                            .with_generation_defaults_envelope(&defaults)?;
                        let id = invoke(&harness, &binding, defaults, protocol, stream, version, caller.clone()).await.with_context(||format!("opaque {protocol:?}->{wire} {version:?} stream={stream} caller={caller}"))?;
                        completed(&harness, &id, &binding).await?;
                        count += 1;
                        let observed = bodies(&harness)?;
                        ensure!(observed.len() == count);
                        let body = observed.last().context("provider body")?;
                        for (key, value) in controls.as_object().context("controls")? {
                            if key != "vendor_knob" {
                                ensure!(body.get(key) == Some(value), "opaque {key}");
                            }
                        }
                        let same = matches!(
                            (protocol, wire),
                            (GenerationProtocol::ChatCompletions, "chat_completions")
                                | (GenerationProtocol::Messages, "messages")
                                | (GenerationProtocol::Responses, "responses")
                        );
                        let expected = if caller.get("vendor_knob").is_some() {
                            if same {
                                caller.get("vendor_knob")
                            } else {
                                None
                            }
                        } else {
                            controls.get("vendor_knob")
                        };
                        ensure!(
                            body.get("vendor_knob") == expected,
                            "caller collision {protocol:?}->{wire}"
                        );
                        ensure!(body["model"] == "model-0" && body["stream"] == stream);
                        ensure!(
                            body.get(BODY_MEMBER).is_none()
                                && body.get(ENVELOPE_BODY_MEMBER).is_none()
                        );
                        let headers = harness
                            .observed
                            .lock()
                            .map_err(|_| anyhow::anyhow!("observed headers"))?
                            .last()
                            .context("observed")?
                            .1
                            .clone();
                        ensure!(
                            !headers.contains_key("x-injected")
                                && !headers.contains_key("tenant_id")
                        );
                        if wire == "messages" {
                            ensure!(
                                headers["x-api-key"] == "owned-upstream-secret"
                                    && !headers.contains_key("authorization")
                            );
                        } else {
                            ensure!(headers["authorization"] == "Bearer owned-upstream-secret");
                        }
                    }
                }
            }
        }
        ensure!(harness.observed()?.len() == count);
        harness.fixture.service.shutdown().await?;
    }
    Ok(())
}

#[tokio::test]
async fn envelope_special_fields_fallthrough_aliases_and_raw_tool_choice_matrix() -> Result<()> {
    for wire in ["chat_completions", "messages", "responses"] {
        let harness = harness(wire, Value::Null).await?;
        for protocol in [
            GenerationProtocol::ChatCompletions,
            GenerationProtocol::Messages,
            GenerationProtocol::Responses,
        ] {
            for version in [
                HttpVersion::Http1Only,
                #[cfg(feature = "http2")]
                HttpVersion::Http2PriorKnowledge,
            ] {
                for stream in [false, true] {
                    for raw in [false, true] {
                        let params = if raw {
                            json!({"store":null,"parallel_tool_calls":"invalid","stream_options":null,"tool_choice":{"type":"function","function":{"name":"lookup"}},"max_completion_tokens":77,"max_output_tokens":88})
                        } else {
                            json!({"store":false,"parallel_tool_calls":false,"stream_options":{"include_usage":false,"include_obfuscation":false,"vendor_option":7},"max_completion_tokens":77,"max_output_tokens":88})
                        };
                        let defaults = defaults(params.clone())?;
                        let binding = harness
                            .binding(vec![0], protocol)
                            .await?
                            .with_generation_defaults_envelope(&defaults)?;
                        let id = invoke(
                            &harness,
                            &binding,
                            defaults,
                            protocol,
                            stream,
                            version,
                            json!({}),
                        )
                        .await
                        .with_context(|| {
                            format!(
                                "special {protocol:?}->{wire} {version:?} stream={stream} raw={raw}"
                            )
                        })?;
                        completed(&harness, &id, &binding).await?;
                        let observed = bodies(&harness)?;
                        let body = observed.last().context("provider body")?;
                        if raw {
                            ensure!(body["store"].is_null() && body.get("store").is_some());
                            ensure!(body["parallel_tool_calls"] == "invalid");
                            ensure!(body["tool_choice"] == params["tool_choice"]);
                            if wire == "chat_completions" && stream {
                                ensure!(body["stream_options"]["include_usage"] == true);
                            } else {
                                ensure!(body.get("stream_options") == Some(&Value::Null));
                            }
                        } else if wire == "messages" {
                            ensure!(
                                body.get("store").is_none() && body.get("stream_options").is_none()
                            );
                            ensure!(body["tool_choice"]["disable_parallel_tool_use"] == true);
                        } else {
                            ensure!(body["store"] == false && body["parallel_tool_calls"] == false);
                            if wire == "chat_completions" {
                                ensure!(body["stream_options"]["include_usage"] == stream);
                                ensure!(
                                    body["stream_options"]["include_obfuscation"] == false
                                        && body["stream_options"]["vendor_option"] == 7
                                );
                            } else {
                                ensure!(body.get("stream_options").is_none());
                            }
                        }
                        if wire == "chat_completions" {
                            ensure!(body.get("max_completion_tokens").is_none());
                        } else {
                            ensure!(body["max_completion_tokens"] == 77);
                        }
                        ensure!(
                            body["max_output_tokens"] == if wire == "responses" { 23 } else { 88 }
                        );
                    }
                }
            }
        }
        harness.fixture.service.shutdown().await?;
    }
    Ok(())
}

#[tokio::test]
async fn envelope_retained_typed_defaults_fill_after_invalid_raw_values() -> Result<()> {
    let harness = harness("chat_completions", json!({"store":false,"parallel_tool_calls":false,"stream_options":{"include_usage":false},"vendor_knob":"retained"})).await?;
    let defaults = defaults(
        json!({"store":null,"parallel_tool_calls":"invalid","stream_options":null,"vendor_knob":"request-local"}),
    )?;
    let binding = harness
        .binding(vec![0], GenerationProtocol::ChatCompletions)
        .await?
        .with_generation_defaults_envelope(&defaults)?;
    let id = invoke(
        &harness,
        &binding,
        defaults,
        GenerationProtocol::ChatCompletions,
        false,
        HttpVersion::Http1Only,
        json!({}),
    )
    .await?;
    completed(&harness, &id, &binding).await?;
    let bodies = bodies(&harness)?;
    let body = bodies.last().context("provider body")?;
    ensure!(body["store"] == false && body["parallel_tool_calls"] == false);
    ensure!(
        body["stream_options"]["include_usage"] == false && body["vendor_knob"] == "request-local"
    );
    harness.fixture.service.shutdown().await?;
    Ok(())
}
