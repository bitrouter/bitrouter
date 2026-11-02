use super::*;
use bitrouter_ai::tenancy::generation_defaults::{BODY_MEMBER, CanonicalGenerationDefaultsV1};

async fn send(
    client: &GatewayClient,
    binding: &OrderedDispatchBindingV2,
    defaults: CanonicalGenerationDefaultsV1,
    protocol: usize,
    stream: bool,
) -> TestResult {
    let call = client
        .ordered_tenant("scoped-token", binding)?
        .with_generation_defaults(defaults)?;
    macro_rules! invoke {
        ($method:ident, $ty:ty, $terminal:expr) => {{
            let response = call
                .$method(&serde_json::from_value::<$ty>(request(protocol, stream))?)
                .await?;
            assert_eq!(
                response
                    .metadata
                    .execution_id
                    .as_ref()
                    .map(|id| id.as_str()),
                Some("execution")
            );
            if let Body::Stream(events) = response.body {
                assert_eq!(end(events).await?, $terminal);
            } else {
                assert!(!stream);
            }
        }};
    }
    match protocol {
        0 => invoke!(chat_reply, ChatRequest, ProtocolEnd::ChatDone),
        1 => invoke!(messages_reply, MessagesRequest, ProtocolEnd::MessagesStop),
        _ => invoke!(
            responses_reply,
            ResponsesRequest,
            ProtocolEnd::ResponsesCompleted
        ),
    }
    Ok(())
}

#[tokio::test]
async fn defaults_h1_h2_three_protocols_json_sse_are_bound_and_call_local() -> TestResult {
    for (transport, wire) in versions() {
        for protocol in 0..3 {
            for stream in [false, true] {
                let server = server::serve(
                    wire,
                    vec![
                        Some(reply(protocol, stream)?),
                        Some(reply(protocol, stream)?),
                        Some(reply(protocol, stream)?),
                    ],
                )
                .await?;
                let client = gateway_client(&server, transport)?;
                let a: CanonicalGenerationDefaultsV1 = serde_json::from_value(
                    json!({"schema_version":1,"top_k":0,"seed":7,"stop":["a".repeat(8192)]}),
                )?;
                let b: CanonicalGenerationDefaultsV1 =
                    serde_json::from_value(json!({"schema_version":1,"seed":0}))?;
                let original = binding(vec![255, 3, 0])?;
                let bound_a = original.clone().with_generation_defaults(&a)?;
                let bound_b = original.clone().with_generation_defaults(&b)?;
                let (first, second) = tokio::join!(
                    send(&client, &bound_a, a.clone(), protocol, stream),
                    send(&client, &bound_b, b.clone(), protocol, stream)
                );
                first?;
                second?;
                dispatch(&client, &original, protocol, stream).await?;
                let records = server.records.lock().await;
                assert_eq!(records.len(), 3);
                let mut seen = std::collections::HashSet::new();
                for record in &records[..2] {
                    assert_eq!(record.version, wire);
                    let body: Value = serde_json::from_slice(&record.body)?;
                    let actual: CanonicalGenerationDefaultsV1 =
                        serde_json::from_value(body[BODY_MEMBER].clone())?;
                    assert!(actual == a || actual == b);
                    assert!(seen.insert(actual.digest()?.as_str().to_owned()));
                    let selection = decode_selection(
                        record.headers["bitrouter-candidate-selection"].to_str()?,
                    )?;
                    assert_eq!(
                        selection["generation_defaults_digest"],
                        actual.digest()?.as_str()
                    );
                    assert_eq!(selection["candidate_indices"], json!([255, 3, 0]));
                }
                let raw = request(protocol, stream);
                let original_bytes = match protocol {
                    0 => serde_json::to_vec(&serde_json::from_value::<ChatRequest>(raw)?)?,
                    1 => serde_json::to_vec(&serde_json::from_value::<MessagesRequest>(raw)?)?,
                    _ => serde_json::to_vec(&serde_json::from_value::<ResponsesRequest>(raw)?)?,
                };
                assert_eq!(records[2].body, original_bytes);
                let selection = decode_selection(
                    records[2].headers["bitrouter-candidate-selection"].to_str()?,
                )?;
                assert!(selection.get("generation_defaults_digest").is_none());
            }
        }
    }
    Ok(())
}

#[tokio::test]
async fn missing_changed_competing_and_oversized_defaults_never_send() -> TestResult {
    let server = server::serve(Version::HTTP_11, vec![]).await?;
    let client = gateway_client(&server, HttpVersion::Http1Only)?;
    let defaults: CanonicalGenerationDefaultsV1 =
        serde_json::from_value(json!({"schema_version":1,"seed":7}))?;
    let original = binding(vec![0])?;
    let bound = original.clone().with_generation_defaults(&defaults)?;
    let request: ChatRequest = serde_json::from_value(request(0, false))?;
    assert!(matches!(
        client
            .ordered_tenant("scoped-token", &bound)?
            .chat_reply(&request)
            .await,
        Err(Error::Request(_))
    ));
    assert!(
        client
            .ordered_tenant("scoped-token", &original)?
            .with_generation_defaults(defaults.clone())
            .is_err()
    );
    let mut changed = defaults.clone();
    changed.seed = Some(8);
    assert!(
        client
            .ordered_tenant("scoped-token", &bound)?
            .with_generation_defaults(changed)
            .is_err()
    );
    let mut competing: ChatRequest = serde_json::from_value(serde_json::to_value(&request)?)?;
    competing.extra.insert(BODY_MEMBER.into(), json!(null));
    assert!(matches!(
        client
            .ordered_tenant("scoped-token", &bound)?
            .with_generation_defaults(defaults.clone())?
            .chat_reply(&competing)
            .await,
        Err(Error::Request(_))
    ));
    assert!(matches!(
        client
            .ordered_tenant("scoped-token", &original)?
            .chat_reply(&competing)
            .await,
        Err(Error::Request(_))
    ));
    // The defaults object itself fits; the entire request does not.
    let mut large = defaults;
    large.stop = Some(vec!["x".repeat(16 * 1024 * 1024 - 64)]);
    let bound = original.with_generation_defaults(&large)?;
    assert!(matches!(
        client
            .ordered_tenant("scoped-token", &bound)?
            .with_generation_defaults(large)?
            .chat_reply(&request)
            .await,
        Err(Error::Request(_))
    ));
    assert!(server.records.lock().await.is_empty());
    Ok(())
}
