use super::*;
use bitrouter_ai::tenancy::native_controls::{
    HEADER, OrderedNativeControlsCapabilitiesV1, PROVIDER_NATIVE_V1,
};

fn native_capabilities() -> TestResult<OrderedNativeControlsCapabilitiesV1> {
    Ok(serde_json::from_value(json!({"contract_version":1,
        "installation_id":"installation", "instance_id":"instance",
        "capability_revision":"capability", "authority_revision":7,
        "enabled":true,"controls":["responses.previous_response_id",
            "responses.conversation","messages.context_management"]}))?)
}

#[tokio::test]
async fn explicit_discovery_and_call_local_native_controls_h1_h2_json_sse() -> TestResult {
    for (version, wire) in versions() {
        for protocol in [1, 2] {
            for stream in [false, true] {
                let server = server::serve(
                    wire,
                    vec![
                        Some(server::json(serde_json::to_value(native_capabilities()?)?)?),
                        Some(reply(protocol, stream)?),
                        Some(reply(protocol, stream)?),
                    ],
                )
                .await?;
                let discovery = control_client(&server, version)?
                    .ordered_native_controls_capabilities()
                    .await?;
                let client = gateway_client(&server, version)?;
                let binding = binding(vec![255, 3, 0])?;
                let native = client
                    .ordered_tenant("scoped-token", &binding)?
                    .with_native_controls(&discovery)?;
                let mut input = request(protocol, stream);
                let (field, value) = if protocol == 2 {
                    ("previous_response_id", json!("brc_literal_provider_value"))
                } else {
                    ("context_management", json!({"edits":[{"type":"opaque"}]}))
                };
                input[field] = value.clone();
                if protocol == 2 {
                    let response = native
                        .responses_reply(&serde_json::from_value(input)?)
                        .await?;
                    if let Body::Stream(events) = response.body {
                        assert_eq!(end(events).await?, ProtocolEnd::ResponsesCompleted);
                    }
                } else {
                    let response = native
                        .messages_reply(&serde_json::from_value(input)?)
                        .await?;
                    if let Body::Stream(events) = response.body {
                        assert_eq!(end(events).await?, ProtocolEnd::MessagesStop);
                    }
                }
                dispatch(&client, &binding, protocol, stream).await?;
                let records = server.records.lock().await;
                assert_eq!(records.len(), 3);
                assert_eq!(records[0].method, "GET");
                assert_eq!(
                    records[0].path,
                    "/control/v1/tenancy/ordered/native-controls/capabilities"
                );
                assert!(!records[0].headers.contains_key("bitrouter-installation-id"));
                assert_eq!(records[1].version, wire);
                assert_eq!(records[1].headers[HEADER], PROVIDER_NATIVE_V1);
                assert_eq!(records[1].headers.get_all(HEADER).iter().count(), 1);
                assert_eq!(
                    serde_json::from_slice::<Value>(&records[1].body)?[field],
                    value
                );
                assert!(!records[2].headers.contains_key(HEADER));
                assert_eq!(
                    decode_selection(
                        records[1].headers["bitrouter-candidate-selection"].to_str()?
                    )?["candidate_indices"],
                    json!([255, 3, 0])
                );
            }
        }
    }
    Ok(())
}

#[tokio::test]
async fn disabled_stale_or_malformed_native_discovery_never_sends_or_downgrades() -> TestResult {
    let server = server::serve(Version::HTTP_11, vec![]).await?;
    let client = gateway_client(&server, HttpVersion::Http1Only)?;
    let binding = binding(vec![0])?;
    for field in [
        "enabled",
        "instance_id",
        "installation_id",
        "capability_revision",
        "contract_version",
    ] {
        let mut value = serde_json::to_value(native_capabilities()?)?;
        value[field] = match field {
            "enabled" => json!(false),
            "contract_version" => json!(2),
            _ => json!("different"),
        };
        let caps = serde_json::from_value(value)?;
        assert!(
            client
                .ordered_tenant("scoped-token", &binding)?
                .with_native_controls(&caps)
                .is_err()
        );
    }
    assert!(server.records.lock().await.is_empty());
    let absent = server::serve(
        Version::HTTP_11,
        vec![Some(
            Response::builder().status(404).body(HttpBody::empty())?,
        )],
    )
    .await?;
    assert!(
        control_client(&absent, HttpVersion::Http1Only)?
            .ordered_native_controls_capabilities()
            .await
            .is_err()
    );
    assert_eq!(absent.records.lock().await.len(), 1);
    Ok(())
}
