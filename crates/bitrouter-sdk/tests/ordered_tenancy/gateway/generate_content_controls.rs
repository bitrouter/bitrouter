use super::generate_content::{caps, input, native_response};
use super::*;
use bitrouter_ai::tenancy::native_controls::OrderedNativeControlsCapabilitiesV1;

#[tokio::test]
async fn native_programmatic_depth_refuses_before_transport() -> TestResult {
    let server = server::serve(Version::HTTP_11, vec![]).await?;
    let client = gateway_client(&server, HttpVersion::Http1Only)?;
    let mut request = input(false)?;
    let mut nested = Value::Null;
    for _ in 0..256 {
        nested = Value::Array(vec![nested]);
    }
    request.request.insert("opaque".into(), nested);
    let result = client
        .ordered_tenant("scoped-token", &binding(vec![0])?)?
        .with_generate_content(&caps()?)?
        .generate_content_reply(&request)
        .await;
    assert!(matches!(result, Err(Error::Request(_))));
    assert!(server.records.lock().await.is_empty());
    Ok(())
}

fn native_controls() -> TestResult<OrderedNativeControlsCapabilitiesV1> {
    Ok(serde_json::from_value(
        json!({"contract_version":1,"installation_id":"installation",
        "instance_id":"instance","capability_revision":"capability","authority_revision":7,
        "enabled":true,"controls":["responses.previous_response_id","responses.conversation",
            "messages.context_management"]}),
    )?)
}

#[tokio::test]
async fn native_cache_requires_both_observed_policy_and_request_local_opt_in() -> TestResult {
    for (version, wire) in versions() {
        let server = server::serve(wire, vec![Some(native_response(false)?)]).await?;
        let client = gateway_client(&server, version)?;
        let binding = binding(vec![0])?;
        let mut request = input(false)?;
        request.request.insert(
            "cachedContent".into(),
            json!("cachedContents/exact-caller-value"),
        );
        let mut allowed = caps()?;
        allowed.cached_content_enabled = true;
        assert!(
            client
                .ordered_tenant("scoped-token", &binding)?
                .with_generate_content(&caps()?)?
                .with_native_controls(&native_controls()?)?
                .generate_content_reply(&request)
                .await
                .is_err()
        );
        assert!(
            client
                .ordered_tenant("scoped-token", &binding)?
                .with_generate_content(&allowed)?
                .generate_content_reply(&request)
                .await
                .is_err()
        );
        assert!(server.records.lock().await.is_empty());
        client
            .ordered_tenant("scoped-token", &binding)?
            .with_generate_content(&allowed)?
            .with_native_controls(&native_controls()?)?
            .generate_content_reply(&request)
            .await?;
        let records = server.records.lock().await;
        assert_eq!(records.len(), 1);
        assert_eq!(
            records[0].headers[bitrouter_ai::tenancy::native_controls::HEADER],
            bitrouter_ai::tenancy::native_controls::PROVIDER_NATIVE_V1
        );
        assert_eq!(
            serde_json::from_slice::<Value>(&records[0].body)?["request"]["cachedContent"],
            "cachedContents/exact-caller-value"
        );
    }
    Ok(())
}

#[tokio::test]
async fn native_json_success_requires_valid_candidate_finish_and_keeps_failure_identity()
-> TestResult {
    for (version, wire) in versions() {
        for value in [
            json!(null),
            json!([]),
            json!({}),
            json!({"candidates":[null]}),
            json!({"candidates":[{"content":{"parts":[{"text":"unfinished"}]}}]}),
            json!({"error":{"code":500},"candidates":[{"finishReason":"STOP"}]}),
            json!({"promptFeedback":{"blockReason":"SAFETY"},"candidates":[{"finishReason":"STOP"}]}),
        ] {
            let response = Response::builder()
                .header("x-bitrouter-request-id", "execution")
                .header("content-type", "application/json")
                .body(HttpBody::from(value.to_string()))?;
            let server = server::serve(wire, vec![Some(response)]).await?;
            let client = gateway_client(&server, version)?;
            let error = client
                .ordered_tenant("scoped-token", &binding(vec![0])?)?
                .with_generate_content(&caps()?)?
                .generate_content_reply(&input(false)?)
                .await
                .err()
                .ok_or("invalid native JSON was accepted")?;
            let Error::Response(failure) = error else {
                return Err("JSON refusal lost execution metadata".into());
            };
            assert!(matches!(failure.problem, Some(Problem::Contract)));
            assert_eq!(
                failure.metadata.execution_id.as_ref().map(|id| id.as_str()),
                Some("execution")
            );
            assert_eq!(server.records.lock().await.len(), 1);
        }
    }
    Ok(())
}
