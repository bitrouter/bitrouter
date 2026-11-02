use super::*;
use bitrouter_ai::tenancy::ordered::{
    OrderedCandidateSelectionV1, OrderedCapabilitiesV2, OrderedDispatchBindingV2,
};
use bitrouter_sdk::gateway::tenancy::Reply;

fn ordered_binding(correlation: &str) -> TestResult<OrderedDispatchBindingV2> {
    let capabilities: OrderedCapabilitiesV2 = serde_json::from_value(json!({
        "contract_version":2,"installation_id":"installation","instance_id":"instance",
        "capability_revision":"capability-1","authority_revision":7,"attempt_limit":64,
        "revision_bound_dispatch":true,"strict_receipts":true,"dispatch_enabled":true
    }))?;
    let selection: OrderedCandidateSelectionV1 = serde_json::from_value(json!({
        "schema_version":1,"admission_mode":"legacy","route":{"owner_kind":"tenant","owner_id":"tenant",
        "resource_kind":"route","resource_id":"route","revision_id":"route-revision"},
        "candidate_indices":[255,8,0]
    }))?;
    Ok(capabilities.bind(
        "execution-revision-1".try_into()?,
        correlation.try_into()?,
        selection,
    )?)
}

fn output(protocol: usize) -> Value {
    match protocol {
        0 => chat_json(),
        1 => json!({"id":"m","type":"message","role":"assistant","model":"route",
            "content":[],"stop_reason":"end_turn","usage":{"input_tokens":1,"output_tokens":1}}),
        _ => json!({"id":"r","object":"response","model":"route","status":"completed","output":[]}),
    }
}

fn reply(protocol: usize, stream: bool) -> TestResult<Response> {
    if !stream {
        return json_reply(200, Some("execution-1"), output(protocol));
    }
    let text = match protocol {
        0 => "data: [DONE]\n\n".to_owned(),
        1 => "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n".to_owned(),
        _ => format!(
            "event: response.completed\ndata: {{\"type\":\"response.completed\",\"sequence_number\":1,\"response\":{}}}\n\n",
            output(protocol)
        ),
    };
    sse(&text)
}

async fn consume<T, E>(
    reply: Reply<Body<T, E>>,
    streaming: bool,
    expected: ProtocolEnd,
) -> TestResult {
    assert_eq!(reply.metadata.status, 200);
    assert_eq!(
        reply.metadata.execution_id.as_ref().map(|id| id.as_str()),
        Some("execution-1")
    );
    match reply.body {
        Body::Json(_) => assert!(!streaming),
        Body::Stream(mut events) => {
            assert!(streaming);
            let mut terminal = None;
            while let Some(event) = events.next().await {
                if let StreamItem::End(end) = event? {
                    assert!(terminal.replace(end).is_none());
                }
            }
            assert_eq!(terminal, Some(expected));
        }
    }
    Ok(())
}

async fn dispatch(
    client: &GatewayClient,
    ordered: bool,
    protocol: usize,
    stream: bool,
    correlation: &str,
    values: Option<&[HeaderValue]>,
) -> TestResult {
    let credential = format!("credential-{correlation}");
    macro_rules! send {
        ($scoped:expr) => {{
            let scoped = $scoped;
            let scoped = match values {
                Some(values) => scoped
                    .with_anthropic_beta(&[HeaderValue::from_static("replaced-beta")])
                    .with_anthropic_beta(values),
                None => scoped,
            };
            let debug = format!("{scoped:?}");
            assert!(!debug.contains(&credential));
            assert!(!debug.contains("private-beta"));
            match protocol {
                0 => {
                    consume(
                        scoped.chat_reply(&chat(stream)?).await?,
                        stream,
                        ProtocolEnd::ChatDone,
                    )
                    .await
                }
                1 => {
                    consume(
                        scoped.messages_reply(&messages(stream)?).await?,
                        stream,
                        ProtocolEnd::MessagesStop,
                    )
                    .await
                }
                _ => {
                    consume(
                        scoped.responses_reply(&responses(stream)?).await?,
                        stream,
                        ProtocolEnd::ResponsesCompleted,
                    )
                    .await
                }
            }
        }};
    }
    if ordered {
        send!(client.ordered_tenant(&credential, &ordered_binding(correlation)?)?)
    } else {
        send!(client.tenant(&credential, &binding(correlation)?)?)
    }
}

fn beta_bytes(headers: &axum::http::HeaderMap) -> Vec<&[u8]> {
    headers
        .get_all("anthropic-beta")
        .iter()
        .map(HeaderValue::as_bytes)
        .collect()
}

#[tokio::test]
async fn repeated_opaque_beta_values_survive_all_protocols_and_body_modes() -> TestResult {
    let values = [
        HeaderValue::from_static("private-beta-A,  private-beta-B"),
        HeaderValue::from_bytes(b"opaque=\x80\xff")?,
        HeaderValue::from_static("private-beta-A,  private-beta-B"),
        HeaderValue::from_static(""),
        HeaderValue::from_static("interior\tspacing"),
    ];
    for (version, wire) in versions() {
        for ordered in [false, true] {
            let mut replies = Vec::new();
            for protocol in 0..3 {
                for stream in [false, true] {
                    for _ in 0..3 {
                        replies.push(Some(reply(protocol, stream)?));
                    }
                }
            }
            let server = server::serve(wire, replies).await?;
            let client = client(&server, version)?;
            for protocol in 0..3 {
                for stream in [false, true] {
                    for beta in [Some(values.as_slice()), Some([].as_slice()), None] {
                        dispatch(&client, ordered, protocol, stream, "correlation", beta).await?;
                    }
                }
            }
            let records = server.records.lock().await;
            assert_eq!(records.len(), 18);
            for (index, record) in records.iter().enumerate() {
                assert_eq!(record.version, wire);
                let expected = if index % 3 == 0 {
                    values.iter().map(HeaderValue::as_bytes).collect()
                } else {
                    Vec::new()
                };
                assert_eq!(beta_bytes(&record.headers), expected);
                assert_eq!(
                    record.headers["authorization"],
                    "Bearer credential-correlation"
                );
                assert_eq!(
                    record.headers["bitrouter-tenancy-version"],
                    if ordered { "2" } else { "1" }
                );
                assert_eq!(record.headers["bitrouter-correlation-id"], "correlation");
                assert_eq!(record.headers["bitrouter-receipt-mode"], "strict");
                for name in [
                    "bitrouter-installation-id",
                    "bitrouter-instance-id",
                    "bitrouter-capability-revision",
                    "bitrouter-execution-revision",
                    "bitrouter-candidate-selection",
                ] {
                    assert_eq!(record.headers.get(name), records[2].headers.get(name));
                }
                assert_eq!(
                    record.headers.contains_key("bitrouter-candidate-selection"),
                    ordered
                );
                assert!(!record.headers.contains_key("anthropic-version"));
                assert!(!record.headers.contains_key("openai-beta"));
                let protocol = index / 6;
                assert_eq!(record.method, "POST");
                assert_eq!(
                    record.path,
                    [
                        "/prefix/v1/chat/completions",
                        "/prefix/v1/messages",
                        "/prefix/v1/responses",
                    ][protocol]
                );
                let stream = index % 6 >= 3;
                let expected = match protocol {
                    0 => serde_json::to_value(chat(stream)?)?,
                    1 => serde_json::to_value(messages(stream)?)?,
                    _ => serde_json::to_value(responses(stream)?)?,
                };
                assert_eq!(serde_json::from_slice::<Value>(&record.body)?, expected);
            }
        }
    }
    Ok(())
}

#[tokio::test]
async fn concurrent_beta_values_are_per_call_and_do_not_change_client_defaults() -> TestResult {
    for (version, wire) in versions() {
        for ordered in [false, true] {
            let mut replies = vec![Some(server::json(json!({"status":"ok"}))?)];
            for _ in 0..12 {
                replies.push(Some(reply(0, false)?));
            }
            replies.push(Some(server::json(json!({"status":"ok"}))?));
            let server = server::serve(wire, replies).await?;
            let client = client(&server, version)?;
            client.health().await?;
            futures_util::future::try_join_all((0..12).map(|index| {
                let client = &client;
                async move {
                    let correlation = format!("call-{index}");
                    let beta =
                        HeaderValue::from_bytes(format!("private-beta-{index},  flag").as_bytes())?;
                    dispatch(
                        client,
                        ordered,
                        0,
                        false,
                        &correlation,
                        Some(&[beta.clone(), beta]),
                    )
                    .await
                }
            }))
            .await?;
            client.health().await?;
            let records = server.records.lock().await;
            assert_eq!(records.len(), 14);
            let mut correlations = std::collections::HashSet::new();
            for record in records.iter() {
                assert_eq!(record.version, wire);
                if wire == Version::HTTP_2 {
                    assert_eq!(record.connection, records[0].connection);
                }
                if record.path == "/prefix/health" {
                    assert_eq!(record.headers["authorization"], "Bearer default-token");
                    assert!(beta_bytes(&record.headers).is_empty());
                    continue;
                }
                let correlation = record.headers["bitrouter-correlation-id"].to_str()?;
                assert!(correlations.insert(correlation));
                let index = correlation
                    .strip_prefix("call-")
                    .ok_or("unexpected correlation")?;
                let expected = format!("private-beta-{index},  flag");
                assert_eq!(beta_bytes(&record.headers), vec![expected.as_bytes(); 2]);
                assert_eq!(
                    record.headers["authorization"],
                    format!("Bearer credential-{correlation}")
                );
            }
            assert_eq!(correlations.len(), 12);
        }
    }
    Ok(())
}

async fn dispatch_bytes(client: &GatewayClient, ordered: bool, raw: &[u8]) -> TestResult {
    let values = [HeaderValue::from_bytes(raw)?];
    dispatch(client, ordered, 0, false, "invalid", Some(&values)).await
}

#[tokio::test]
async fn invalid_beta_field_values_fail_construction_before_any_request() -> TestResult {
    for (version, wire) in versions() {
        let server = server::serve(wire, vec![]).await?;
        let client = client(&server, version)?;
        for ordered in [false, true] {
            for raw in [
                b"private-beta\r\ninjected: value".as_slice(),
                b"private-beta\rvalue",
                b"private-beta\nvalue",
                b"private-beta\0value",
                b"private-beta\x01value",
                b"private-beta\x7fvalue",
            ] {
                let error = dispatch_bytes(&client, ordered, raw)
                    .await
                    .err()
                    .ok_or("invalid value accepted")?;
                assert!(
                    error
                        .downcast_ref::<reqwest::header::InvalidHeaderValue>()
                        .is_some()
                );
                assert!(!error.to_string().contains("private-beta"));
            }
        }
        assert!(server.records.lock().await.is_empty());
    }
    Ok(())
}
