use super::generate_content::{caps, input};
use super::*;

#[tokio::test]
async fn native_embedded_json_objects_stay_opaque_under_consumer_features() -> TestResult {
    let opaque = json!({"raw":{"$serde_json::private::RawValue":"{\"injected\":true}"},
        "number":{"$serde_json::private::Number":"1.25"},
        "mixed":{"$serde_json::private::Number":"1.25","retained":true},
        "nested":[{"$serde_json::private::RawValue":"{\"error\":{\"code\":500}}"},
            {"$serde_json::private::Number":"18446744073709551616"}]});
    let value = json!({"candidates":[{"finishReason":"STOP","content":{"parts":[{
        "functionCall":{"name":"lookup","args":{"opaque":opaque}}}]}}]});
    for (version, wire) in versions() {
        for streaming in [false, true] {
            let response = Response::builder()
                .header("x-bitrouter-request-id", "execution")
                .header(
                    "content-type",
                    if streaming {
                        "text/event-stream"
                    } else {
                        "application/json"
                    },
                )
                .body(HttpBody::from(if streaming {
                    format!("data: {value}\n\n")
                } else {
                    value.to_string()
                }))?;
            let server = server::serve(wire, vec![Some(response)]).await?;
            let client = gateway_client(&server, version)?;
            let reply = client
                .ordered_tenant("scoped-token", &binding(vec![0])?)?
                .with_generate_content(&caps()?)?
                .generate_content_reply(&input(streaming)?)
                .await?;
            match reply.body {
                Body::Json(actual) => assert_eq!(actual, value),
                Body::Stream(mut events) => {
                    let StreamItem::Event(event) =
                        events.next().await.ok_or("missing opaque event")??
                    else {
                        return Err("end preceded opaque value".into());
                    };
                    assert_eq!(event.data, value);
                    assert!(matches!(
                        events.next().await.ok_or("missing native end")??,
                        StreamItem::End(ProtocolEnd::GenerateContentFinished)
                    ));
                }
            }
        }
        let encoded =
            r#"{"error":{"code":500},"error":null,"candidates":[{"finishReason":"STOP"}]}"#;
        let response = Response::builder()
            .header("x-bitrouter-request-id", "execution")
            .header("content-type", "application/json")
            .body(HttpBody::from(
                json!({"$serde_json::private::RawValue":encoded}).to_string(),
            ))?;
        let server = server::serve(wire, vec![Some(response)]).await?;
        let client = gateway_client(&server, version)?;
        let result = client
            .ordered_tenant("scoped-token", &binding(vec![0])?)?
            .with_generate_content(&caps()?)?
            .generate_content_reply(&input(false)?)
            .await;
        assert!(
            matches!(result, Err(Error::Response(ref failure)) if matches!(failure.problem, Some(Problem::Contract)))
        );
        assert_eq!(server.records.lock().await.len(), 1);
    }
    Ok(())
}

#[tokio::test]
async fn native_json_sse_values_match_serde_json_consumer_features() -> TestResult {
    // Run this real SDK test with serde_json/arbitrary_precision as well as the
    // default dependency graph. Expected values use the consumer's own parser.
    let raw = r#"{"responseId":"r","candidates":[{"index":0,"finishReason":"STOP","content":{"parts":[{"functionCall":{"name":"lookup","args":{"amount":1.25,"large":18446744073709551616,"nested":[null,true,-7,2.5e2]}}}]}}],"usageMetadata":{"promptTokenCount":4,"opaque":{"decimal":1.75,"large":18446744073709551616}}}"#;
    let expected: Value = serde_json::from_str(raw)?;
    for (version, wire) in versions() {
        for streaming in [false, true] {
            let response = Response::builder()
                .header("x-bitrouter-request-id", "execution")
                .header(
                    "content-type",
                    if streaming {
                        "text/event-stream"
                    } else {
                        "application/json"
                    },
                )
                .body(HttpBody::from(if streaming {
                    format!("data: {raw}\n\n")
                } else {
                    raw.to_owned()
                }))?;
            let server = server::serve(wire, vec![Some(response)]).await?;
            let client = gateway_client(&server, version)?;
            let reply = client
                .ordered_tenant("scoped-token", &binding(vec![0])?)?
                .with_generate_content(&caps()?)?
                .generate_content_reply(&input(streaming)?)
                .await?;
            match reply.body {
                Body::Json(value) => assert_eq!(value, expected),
                Body::Stream(mut events) => {
                    let StreamItem::Event(event) =
                        events.next().await.ok_or("missing native values")??
                    else {
                        return Err("native end preceded values".into());
                    };
                    assert_eq!(event.data, expected);
                    assert!(matches!(
                        events.next().await.ok_or("missing native end")??,
                        StreamItem::End(ProtocolEnd::GenerateContentFinished)
                    ));
                    assert!(events.next().await.is_none());
                }
            }
        }
    }
    Ok(())
}

#[tokio::test]
async fn native_content_container_shapes_refuse_in_json_and_sse() -> TestResult {
    for (version, wire) in versions() {
        for streaming in [false, true] {
            for content in [
                json!(42),
                json!("bad"),
                json!([]),
                json!({"parts":42}),
                json!({"parts":{}}),
            ] {
                let value = json!({"candidates":[{"finishReason":"STOP","content":content}]});
                let response = Response::builder()
                    .header("x-bitrouter-request-id", "execution")
                    .header(
                        "content-type",
                        if streaming {
                            "text/event-stream"
                        } else {
                            "application/json"
                        },
                    )
                    .body(HttpBody::from(if streaming {
                        format!("data: {value}\n\n")
                    } else {
                        value.to_string()
                    }))?;
                let server = server::serve(wire, vec![Some(response)]).await?;
                let client = gateway_client(&server, version)?;
                let result = client
                    .ordered_tenant("scoped-token", &binding(vec![0])?)?
                    .with_generate_content(&caps()?)?
                    .generate_content_reply(&input(streaming)?)
                    .await;
                let error = if streaming {
                    let Body::Stream(mut events) = result?.body else {
                        return Err("expected SSE".into());
                    };
                    events
                        .next()
                        .await
                        .ok_or("missing content refusal")?
                        .err()
                        .ok_or("invalid content published a native event")?
                } else {
                    result.err().ok_or("invalid JSON content was accepted")?
                };
                let Error::Response(failure) = error else {
                    return Err("content refusal lost metadata".into());
                };
                assert!(matches!(failure.problem, Some(Problem::Contract)));
                assert_eq!(
                    failure.metadata.execution_id.as_ref().map(|id| id.as_str()),
                    Some("execution")
                );
                assert_eq!(server.records.lock().await.len(), 1);
            }
        }
    }
    Ok(())
}

#[tokio::test]
async fn native_explicit_null_error_has_matching_json_and_sse_semantics() -> TestResult {
    let value = json!({"error":null,"candidates":[{"index":0,"finishReason":"STOP"}]});
    for (version, wire) in versions() {
        for streaming in [false, true] {
            let response = Response::builder()
                .header("x-bitrouter-request-id", "execution")
                .header(
                    "content-type",
                    if streaming {
                        "text/event-stream"
                    } else {
                        "application/json"
                    },
                )
                .body(HttpBody::from(if streaming {
                    format!("data: {value}\n\n")
                } else {
                    value.to_string()
                }))?;
            let server = server::serve(wire, vec![Some(response)]).await?;
            let client = gateway_client(&server, version)?;
            let result = client
                .ordered_tenant("scoped-token", &binding(vec![0])?)?
                .with_generate_content(&caps()?)?
                .generate_content_reply(&input(streaming)?)
                .await?;
            match result.body {
                Body::Json(actual) => assert_eq!(actual, value),
                Body::Stream(mut events) => {
                    let StreamItem::Event(actual) =
                        events.next().await.ok_or("missing null-error event")??
                    else {
                        return Err("native end preceded null-error event".into());
                    };
                    assert_eq!(actual.data, value);
                    assert!(matches!(
                        events.next().await.ok_or("missing native end")??,
                        StreamItem::End(ProtocolEnd::GenerateContentFinished)
                    ));
                    assert!(events.next().await.is_none());
                }
            }
            assert_eq!(server.records.lock().await.len(), 1);
        }
    }
    Ok(())
}

#[tokio::test]
async fn native_null_error_usage_and_repeated_finish_tails_remain_neutral() -> TestResult {
    let finish = json!({"responseId":"r","candidates":[{"index":0,"finishReason":"STOP"}]});
    let tails = [
        json!({"error":null,"usageMetadata":{"totalTokenCount":7}}),
        json!({"error":null,"candidates":[{"index":0,"finishReason":"STOP"}]}),
    ];
    for (version, wire) in versions() {
        for tail in &tails {
            let response = Response::builder()
                .header("x-bitrouter-request-id", "execution")
                .header("content-type", "text/event-stream")
                .body(HttpBody::from(format!(
                    "data: {finish}\n\ndata: {tail}\n\n"
                )))?;
            let server = server::serve(wire, vec![Some(response)]).await?;
            let client = gateway_client(&server, version)?;
            let result = client
                .ordered_tenant("scoped-token", &binding(vec![0])?)?
                .with_generate_content(&caps()?)?
                .generate_content_reply(&input(true)?)
                .await?;
            let Body::Stream(mut events) = result.body else {
                return Err("expected native SSE".into());
            };
            for expected in [&finish, tail] {
                let StreamItem::Event(actual) =
                    events.next().await.ok_or("missing neutral native tail")??
                else {
                    return Err("native end preceded neutral tail".into());
                };
                assert_eq!(&actual.data, expected);
            }
            assert!(matches!(
                events.next().await.ok_or("missing native end")??,
                StreamItem::End(ProtocolEnd::GenerateContentFinished)
            ));
            assert!(events.next().await.is_none());
            assert_eq!(server.records.lock().await.len(), 1);
        }
    }
    Ok(())
}
