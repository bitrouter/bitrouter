use super::generate_content::{caps, input};
use super::*;

#[tokio::test]
async fn native_anonymous_and_indexed_identity_modes_preserve_wire_presence() -> TestResult {
    let anonymous = vec![
        json!({"candidates":[{"content":{"parts":[{"text":"anonymous"}]}}]}),
        json!({"candidates":[{"finishReason":"STOP"}]}),
    ];
    let indexed = vec![
        json!({"candidates":[{"index":2147483647,"content":{"parts":[{"text":"large index"}]}},
            {"index":0,"content":{"parts":[{"text":"zero"}]}}]}),
        json!({"candidates":[{"index":0,"finishReason":"STOP"},
            {"index":2147483647,"finishReason":"MAX_TOKENS"}]}),
    ];
    for (version, wire) in versions() {
        for frames in [&anonymous, &indexed] {
            let body: String = frames
                .iter()
                .map(|frame| format!("data: {frame}\n\n"))
                .collect();
            let response = Response::builder()
                .header("x-bitrouter-request-id", "execution")
                .header("content-type", "text/event-stream")
                .body(HttpBody::from(body))?;
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
            for expected in frames {
                let StreamItem::Event(actual) =
                    events.next().await.ok_or("missing native identity")??
                else {
                    return Err("end preceded native identity".into());
                };
                assert_eq!(&actual.data, expected);
            }
            assert!(matches!(
                events.next().await.ok_or("missing native end")??,
                StreamItem::End(ProtocolEnd::GenerateContentFinished)
            ));
            assert!(events.next().await.is_none());
        }
    }
    Ok(())
}

#[tokio::test]
async fn native_index_range_and_present_null_identities_refuse_json_and_sse() -> TestResult {
    let mut invalid: Vec<Value> = [
        json!(2147483648_u64),
        json!(u64::MAX),
        json!(-1),
        json!(0.5),
        Value::Null,
    ]
    .into_iter()
    .map(|index| json!({"candidates":[{"index":index,"finishReason":"STOP"}]}))
    .collect();
    for key in ["responseId", "modelVersion"] {
        let mut value = json!({"candidates":[{"finishReason":"STOP"}]});
        value[key] = Value::Null;
        invalid.push(value);
    }
    invalid.push(json!({"candidates":[{"index":0,"error":{"code":500},"finishReason":"STOP"}]}));
    for (version, wire) in versions() {
        for value in &invalid {
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
                    .await;
                let error = if streaming {
                    let Body::Stream(mut events) = result?.body else {
                        return Err("expected native SSE".into());
                    };
                    let error = events
                        .next()
                        .await
                        .ok_or("missing native identity refusal")?
                        .err()
                        .ok_or("invalid native identity emitted a finish")?;
                    assert!(events.next().await.is_none());
                    error
                } else {
                    result.err().ok_or("invalid JSON identity was accepted")?
                };
                let Error::Response(failure) = error else {
                    return Err("native identity lost metadata".into());
                };
                assert!(matches!(failure.problem, Some(Problem::Contract)));
                assert_eq!(
                    failure.metadata.execution_id.as_ref().map(|id| id.as_str()),
                    Some("execution")
                );
            }
        }
    }
    Ok(())
}

#[tokio::test]
async fn native_index_mode_and_sealed_finish_cannot_be_changed() -> TestResult {
    let cases = [
        (
            json!({"content":{"parts":[{"text":"anonymous"}]}}),
            json!({"index":0,"finishReason":"STOP"}),
        ),
        (
            json!({"index":0,"content":{"parts":[{"text":"zero"}]}}),
            json!({"finishReason":"STOP"}),
        ),
        (
            json!({"index":7,"content":{"parts":[{"text":"seven"}]}}),
            json!({"finishReason":"STOP"}),
        ),
        (
            json!({"index":0,"finishReason":"STOP"}),
            json!({"index":0,"finishReason":null}),
        ),
        (json!({"finishReason":"STOP"}), json!({"finishReason":null})),
    ];
    for (version, wire) in versions() {
        for (initial, invalid) in &cases {
            let first = json!({"candidates":[initial]});
            let last = json!({"candidates":[invalid]});
            let response = Response::builder()
                .header("x-bitrouter-request-id", "execution")
                .header("content-type", "text/event-stream")
                .body(HttpBody::from(format!("data: {first}\n\ndata: {last}\n\n")))?;
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
            let mut refused = false;
            while let Some(event) = events.next().await {
                match event {
                    Ok(StreamItem::Event(event)) => {
                        assert!(event.data["candidates"][0]["finishReason"].is_null())
                    }
                    Err(Error::Response(failure)) => {
                        assert!(matches!(failure.problem, Some(Problem::Contract)));
                        refused = true;
                        break;
                    }
                    _ => return Err("native identity change published an end".into()),
                }
            }
            assert!(refused);
            assert!(events.next().await.is_none());
        }
    }
    Ok(())
}
