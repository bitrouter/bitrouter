//! Actual typed canonical defaults, protocol rendering, and accepted identity.
use super::*;
use bitrouter_ai::tenancy::generation_defaults::{BODY_MEMBER, CanonicalGenerationDefaultsV1};

pub(super) fn provider_stream(protocol: &str, usage: Value) -> Result<String> {
    let events = match protocol {
        "messages" => vec![
            json!({"type":"message_start","message":{"id":"msg_receipts","type":"message","role":"assistant","model":"model-0","content":[],"stop_reason":null,"stop_sequence":null,"usage":usage}}),
            json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"receipt reply"}}),
            json!({"type":"content_block_stop","index":0}),
            json!({"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":7}}),
            json!({"type":"message_stop"}),
        ],
        "responses" => vec![
            json!({"type":"response.created","response":{"id":"resp_receipts","status":"in_progress"}}),
            json!({"type":"response.output_text.delta","delta":"receipt reply"}),
            json!({"type":"response.completed","response":json_response(protocol,usage)?}),
        ],
        _ => return super::super::wire::chat_stream(usage, true),
    };
    events
        .iter()
        .map(|event| {
            Ok(format!(
                "event: {}\ndata: {}\n\n",
                event["type"].as_str().context("event type")?,
                serde_json::to_string(event)?
            ))
        })
        .collect::<Result<Vec<_>>>()
        .map(|events| events.concat())
}

fn defaults() -> Result<CanonicalGenerationDefaultsV1> {
    Ok(serde_json::from_value(
        json!({"schema_version":1,"temperature":0.7,"top_p":0.8,
        "max_tokens":23,"reasoning_effort":"high","top_k":40,"seed":7,"stop":["END"],
        "presence_penalty":0.2,"frequency_penalty":0.3}),
    )?)
}

async fn invoke(
    harness: &OrderedHarness,
    binding: &OrderedDispatchBindingV2,
    defaults: CanonicalGenerationDefaultsV1,
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
        .with_generation_defaults(defaults)?;
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
            let id = reply.metadata.execution_id.context("execution identity")?;
            match reply.body {
                Body::Json(_) => ensure!(!stream),
                Body::Stream(mut events) => {
                    let mut terminal = None;
                    while let Some(event) = events.next().await {
                        if let StreamItem::End(end) = event? {
                            ensure!(terminal.is_none());
                            terminal = Some(end);
                        }
                    }
                    ensure!(terminal == Some($end));
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

fn rendered(body: &Value, wire: &str) -> Result<()> {
    ensure!(body.get(BODY_MEMBER).is_none());
    ensure!(body["temperature"] == 0.7 && body["top_p"] == 0.8);
    match wire {
        "messages" => {
            ensure!(
                body["max_tokens"] == 23
                    && body["top_k"] == 40
                    && body["stop_sequences"] == json!(["END"])
            );
            ensure!(body["output_config"]["effort"] == "high");
            for field in [
                "seed",
                "stop",
                "presence_penalty",
                "frequency_penalty",
                "reasoning_effort",
            ] {
                ensure!(body.get(field).is_none(), "{field}");
            }
        }
        "responses" => {
            ensure!(body["max_output_tokens"] == 23 && body["reasoning"]["effort"] == "high");
            for field in [
                "top_k",
                "seed",
                "stop",
                "presence_penalty",
                "frequency_penalty",
                "reasoning_effort",
            ] {
                ensure!(body.get(field).is_none(), "{field}");
            }
        }
        _ => {
            ensure!(
                body["max_tokens"] == 23 && body["seed"] == 7 && body["stop"] == json!(["END"])
            );
            ensure!(
                body["reasoning_effort"] == "high"
                    && body["presence_penalty"] == 0.2
                    && body["frequency_penalty"] == 0.3
            );
            ensure!(body.get("top_k").is_none());
        }
    }
    Ok(())
}

#[tokio::test]
async fn canonical_defaults_actual_three_ingress_three_egress_json_sse_h1_h2_retain_digest()
-> Result<()> {
    for wire in ["chat_completions", "messages", "responses"] {
        let harness =
            OrderedHarness::with_resource_edits(1, 8, None, Value::Null, None, |resources| {
                let route = resources
                    .iter_mut()
                    .find(|row| row["reference"]["resource_kind"] == "route")
                    .context("route")?;
                route["definition"]["routing"]["providers"]["provider-0"]["api_protocol"] =
                    json!([{"*":wire}]);
                Ok(())
            })
            .await?;
        for protocol in [
            GenerationProtocol::ChatCompletions,
            GenerationProtocol::Messages,
            GenerationProtocol::Responses,
        ] {
            let defaults = defaults()?;
            let binding = harness
                .binding(vec![0], protocol)
                .await?
                .with_generation_defaults(&defaults)?;
            for version in [
                HttpVersion::Http1Only,
                #[cfg(feature = "http2")]
                HttpVersion::Http2PriorKnowledge,
            ] {
                for stream in [false, true] {
                    let id = invoke(
                        &harness,
                        &binding,
                        defaults.clone(),
                        protocol,
                        stream,
                        version,
                        json!({"temperature":null}),
                    )
                    .await
                    .with_context(|| {
                        format!("canonical {protocol:?} -> {wire}, {version:?}, stream={stream}")
                    })?;
                    let receipt = harness.receipt(&id).await?;
                    ensure!(
                        receipt.outcome == Outcome::Completed
                            && receipt.accepted_selection.generation_defaults_digest
                                == Some(defaults.digest()?)
                    );
                    let evidence = harness
                        .control(true, HttpVersion::Http1Only)?
                        .usage_evidence_v2(&receipt)
                        .await?;
                    evidence.validate_against(&receipt)?;
                    rendered(bodies(&harness)?.last().context("provider body")?, wire)?;
                }
            }
        }
        harness.fixture.service.shutdown().await?;
    }
    Ok(())
}

#[tokio::test]
async fn canonical_defaults_fallback_retains_one_identity_and_protocol_specific_rendering()
-> Result<()> {
    let harness = OrderedHarness::with_resource_edits(2, 8, None, Value::Null, None, |resources| {
        let route = resources
            .iter_mut()
            .find(|row| row["reference"]["resource_kind"] == "route")
            .context("route")?;
        route["definition"]["routing"]["providers"]["provider-1"]["api_protocol"] =
            json!([{"*":"messages"}]);
        Ok(())
    })
    .await?;
    let defaults = defaults()?;
    let binding = harness
        .binding(vec![1, 0], GenerationProtocol::ChatCompletions)
        .await?
        .with_generation_defaults(&defaults)?;
    let id = invoke(
        &harness,
        &binding,
        defaults.clone(),
        GenerationProtocol::ChatCompletions,
        false,
        HttpVersion::Http1Only,
        json!({"stop":[]}),
    )
    .await?;
    let receipt = harness.receipt(&id).await?;
    ensure!(
        receipt
            .attempts
            .iter()
            .map(|attempt| attempt.candidate_index)
            .collect::<Vec<_>>()
            == [1, 0]
    );
    ensure!(receipt.accepted_selection.generation_defaults_digest == Some(defaults.digest()?));
    let bodies = bodies(&harness)?;
    ensure!(bodies.len() == 2);
    rendered(&bodies[0], "messages")?;
    rendered(&bodies[1], "chat_completions")?;
    harness.fixture.service.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn canonical_defaults_equivalent_integer_spelling_keeps_bound_identity() -> Result<()> {
    let harness = OrderedHarness::new(1, 8, None).await?;
    let defaults: CanonicalGenerationDefaultsV1 = serde_json::from_value(
        json!({"schema_version":1,"max_tokens":25,"top_k":0,"seed":i64::MIN}),
    )?;
    let binding = harness
        .binding(vec![0], GenerationProtocol::ChatCompletions)
        .await?
        .with_generation_defaults(&defaults)?;
    let canonical = serde_json::to_string(&defaults)?;
    let equivalent =
        r#"{"schema_version":1.0,"max_tokens":2.5e1,"top_k":0.0,"seed":-9.223372036854775808e18}"#;
    for payload in [canonical.as_str(), equivalent] {
        let body = format!(
            "{{\"model\":\"{MODEL}\",\"messages\":[{{\"role\":\"user\",\"content\":\"hello\"}}],\"{BODY_MEMBER}\":{payload}}}"
        );
        let response = reqwest::Client::new()
            .post(format!("{}/v1/chat/completions", harness.endpoint))
            .headers(caller_timeout::ordered_headers(&harness, &binding)?)
            .header("content-type", "application/json")
            .body(body)
            .send()
            .await?;
        ensure!(response.status() == StatusCode::OK);
        let id: ExecutionId = response
            .headers()
            .get("x-bitrouter-request-id")
            .context("execution identity")?
            .to_str()?
            .try_into()?;
        let _: Value = response.json().await?;
        let receipt = harness.receipt(&id).await?;
        ensure!(receipt.outcome == Outcome::Completed);
        ensure!(receipt.accepted_selection.generation_defaults_digest == Some(defaults.digest()?));
        let observed = bodies(&harness)?;
        let body = observed.last().context("provider body")?;
        ensure!(body["max_tokens"] == 25 && body["seed"] == json!(i64::MIN));
        ensure!(body.get(BODY_MEMBER).is_none());
    }
    ensure!(harness.observed()?.len() == 2);
    harness.fixture.service.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn canonical_defaults_missing_changed_duplicate_or_unbound_body_never_reaches_admission()
-> Result<()> {
    let harness = OrderedHarness::new(1, 8, None).await?;
    let original = harness
        .binding(vec![0], GenerationProtocol::ChatCompletions)
        .await?;
    let defaults = defaults()?;
    let bound = original.clone().with_generation_defaults(&defaults)?;
    let payload = serde_json::to_string(&defaults)?;
    let valid = format!("{{\"model\":\"{MODEL}\",\"messages\":[],\"{BODY_MEMBER}\":{payload}}}");
    let changed = valid.replace("\"seed\":7", "\"seed\":8");
    let duplicate = valid.replace("\"seed\":7", "\"seed\":7,\"seed\":7");
    let double_member = format!("{{\"{BODY_MEMBER}\":{payload},{}", &valid[1..]);
    for (binding, body) in [
        (&bound, json!({"model":MODEL,"messages":[]}).to_string()),
        (&bound, changed),
        (&bound, duplicate),
        (&bound, double_member),
        (
            &bound,
            valid.replace(
                "\"schema_version\":1",
                "\"schema_version\":1.00000000000000001",
            ),
        ),
        (
            &bound,
            valid.replace("\"seed\":7", "\"seed\":7.000000000000001"),
        ),
        (
            &bound,
            valid.replace("\"seed\":7", "\"seed\":9.223372036854775808e18"),
        ),
        (&original, valid),
    ] {
        let response = reqwest::Client::new()
            .post(format!("{}/v1/chat/completions", harness.endpoint))
            .headers(caller_timeout::ordered_headers(&harness, binding)?)
            .header("content-type", "application/json")
            .body(body)
            .send()
            .await?;
        ensure!(response.status() == StatusCode::BAD_REQUEST);
        ensure!(!response.headers().contains_key("x-bitrouter-request-id"));
    }
    ensure!(harness.observed()?.is_empty());
    let page = harness
        .control(true, HttpVersion::Http1Only)?
        .executions(
            &harness.fixture.installation,
            &harness.fixture.tenant,
            &"ordered-correlation".try_into()?,
            None,
            100,
        )
        .await?;
    ensure!(page.execution_ids.is_empty());
    harness.fixture.service.shutdown().await?;
    Ok(())
}
