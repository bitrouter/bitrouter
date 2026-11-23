use super::*;
use bitrouter_ai::tenancy::{
    admission::AdmissionMode,
    catalog::{BODY_MEMBER, CatalogSelectionRequestV1},
};

fn instructions(protocol: GenerationProtocol, count: u16) -> TestResult<CatalogSelectionRequestV1> {
    Ok(serde_json::from_value(json!({
        "schema_version":1,"catalog_revision":"catalog-revision","route_id":"route",
        "inbound_protocol":protocol,"candidates":(0..count).map(|index| json!({
            "template_id":format!("template-{index}"),"credential_ref":{
                "owner_kind":"tenant","owner_id":"tenant","resource_kind":"credential",
                "resource_id":format!("credential-{index}-{}", "x".repeat(80)),
                "revision_id":"credential-revision"
            }
        })).collect::<Vec<_>>()
    }))?)
}

fn configured_binding(count: u16) -> TestResult<OrderedDispatchBinding> {
    let mut value = binding((0..count).collect())?;
    value.selection.admission_mode = AdmissionMode::Profiled;
    value.selection.route.owner_kind = bitrouter_ai::tenancy::authority::OwnerKind::OperatorPool;
    value.selection.route.owner_id = "platform".try_into()?;
    Ok(value)
}

#[tokio::test]
async fn configured_body_h1_h2_keeps_one_call_for_json_and_streaming() -> TestResult {
    for (version, wire) in versions() {
        for protocol in 0..3 {
            for stream in [false, true] {
                let server = server::serve(wire, vec![Some(reply(protocol, stream)?)]).await?;
                let client = gateway_client(&server, version)?;
                let protocol_kind = match protocol {
                    0 => GenerationProtocol::ChatCompletions,
                    1 => GenerationProtocol::Messages,
                    _ => GenerationProtocol::Responses,
                };
                let selected = instructions(protocol_kind, 64)?;
                assert!(serde_json::to_vec(&selected)?.len() > 16 * 1024);
                let bound = configured_binding(64)?;
                let call = client
                    .ordered_tenant("scoped-token", &bound)?
                    .with_catalog_selection(&selected)?;
                macro_rules! send {
                    ($method:ident, $ty:ty) => {{
                        let result = call
                            .$method(&serde_json::from_value::<$ty>(request(protocol, stream))?)
                            .await?;
                        if let Body::Stream(events) = result.body {
                            end(events).await?;
                        }
                    }};
                }
                match protocol {
                    0 => send!(chat_reply, ChatRequest),
                    1 => send!(messages_reply, MessagesRequest),
                    _ => send!(responses_reply, ResponsesRequest),
                }
                let records = server.records.lock().await;
                assert_eq!(records.len(), 1);
                assert_eq!(records[0].version, wire);
                assert!(
                    records[0]
                        .headers
                        .get("bitrouter-catalog-selection")
                        .is_none()
                );
                let mut body: Value = serde_json::from_slice(&records[0].body)?;
                assert_eq!(
                    body.as_object_mut()
                        .ok_or("request object missing")?
                        .remove(BODY_MEMBER),
                    Some(serde_json::to_value(selected)?)
                );
                assert_eq!(body, serialized_request(protocol, stream)?);
            }
        }
    }
    Ok(())
}

#[tokio::test]
async fn configured_instruction_conflicts_are_rejected_before_transport() -> TestResult {
    let server = server::serve(Version::HTTP_11, vec![]).await?;
    let client = gateway_client(&server, HttpVersion::Http1Only)?;
    let selected = instructions(GenerationProtocol::ChatCompletions, 1)?;
    assert!(
        client
            .ordered_tenant("scoped-token", &binding(vec![0])?)?
            .with_catalog_selection(&selected)
            .is_err()
    );
    assert!(
        client
            .ordered_tenant("scoped-token", &configured_binding(2)?)?
            .with_catalog_selection(&selected)
            .is_err()
    );
    let bound = configured_binding(1)?;
    let mut request: ChatRequest = serde_json::from_value(request(0, false))?;
    request
        .extra
        .insert(BODY_MEMBER.into(), json!({"shadow":true}));
    assert!(
        client
            .ordered_tenant("scoped-token", &bound)?
            .with_catalog_selection(&selected)?
            .chat_reply(&request)
            .await
            .is_err()
    );
    let request: MessagesRequest = serde_json::from_value(super::request(1, false))?;
    assert!(
        client
            .ordered_tenant("scoped-token", &bound)?
            .with_catalog_selection(&selected)?
            .messages_reply(&request)
            .await
            .is_err()
    );
    assert!(server.records.lock().await.is_empty());
    Ok(())
}

#[tokio::test]
async fn configured_native_metadata_preserves_original_request_tokens() -> TestResult {
    use bitrouter_ai::tenancy::generate_content::OrderedGenerateContentRequestV1;
    let raw = r#"{"schema_version":1,"model":"route","stream":false,"request":{"contents":[{"role":"user","parts":[{"functionResponse":{"name":"lookup","response":{"number":1.2300e+2,"large":18446744073709551616}}}]}]}}"#;
    let request: OrderedGenerateContentRequestV1 = serde_json::from_str(raw)?;
    let original = serde_json::to_vec(&request)?;
    for (version, wire) in versions() {
        let server =
            server::serve(wire, vec![Some(generate_content::native_response(false)?)]).await?;
        let client = gateway_client(&server, version)?;
        client
            .ordered_tenant("scoped-token", &configured_binding(1)?)?
            .with_catalog_selection(&instructions(GenerationProtocol::GenerateContent, 1)?)?
            .with_generate_content(&generate_content::caps()?)?
            .generate_content_reply(&request)
            .await?;
        let records = server.records.lock().await;
        assert_eq!(records.len(), 1);
        // The complete original object is the prefix, except its final brace.
        assert!(records[0].body.starts_with(&original[..original.len() - 1]));
    }
    Ok(())
}
