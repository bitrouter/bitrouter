use super::generate_content::{caps, input};
use super::*;

#[tokio::test]
async fn native_all_candidates_preserve_signed_and_media_json_values() -> TestResult {
    let value = json!({"responseId":"r","modelVersion":"v","candidates":[
        {"index":7,"content":{"parts":[{"text":"thought","thoughtSignature":"signed"},
            {"inlineData":{"mimeType":"image/png","data":"AA=="}}]},"finishReason":"STOP"},
        {"index":0,"content":{"parts":[{"functionCall":{"id":"native-call", "name":"tool", "args":{"x":1}}}]},
            "finishReason":"MAX_TOKENS","opaque":{"retained":true}}
    ],"usageMetadata":{"promptTokenCount":4,"candidatesTokenCount":8}});
    for (version, wire) in versions() {
        let response = Response::builder()
            .header("x-bitrouter-request-id", "execution")
            .header("content-type", "application/json")
            .body(HttpBody::from(value.to_string()))?;
        let server = server::serve(wire, vec![Some(response)]).await?;
        let client = gateway_client(&server, version)?;
        let result = client
            .ordered_tenant("scoped-token", &binding(vec![0])?)?
            .with_generate_content(&caps()?)?
            .generate_content_reply(&input(false)?)
            .await?;
        let Body::Json(actual) = result.body else {
            return Err("expected native JSON".into());
        };
        assert_eq!(actual, value);
        assert_eq!(server.records.lock().await.len(), 1);
    }
    Ok(())
}

#[tokio::test]
async fn native_interleaved_candidates_hold_every_finish_until_clean_eof() -> TestResult {
    use bytes::Bytes;
    use std::convert::Infallible;
    use tokio::sync::oneshot;

    let initial = json!({"responseId":"r","modelVersion":"v","candidates":[
        {"index":7,"content":{"parts":[{"text":"seven"}]}},
        {"index":0,"content":{"parts":[{"text":"zero","thoughtSignature":"sig"}]}}
    ]});
    let first_finish = json!({"candidates":[{"index":0,"finishReason":"STOP"},
        {"index":7,"content":{"parts":[{"inlineData":{"mimeType":"image/png","data":"AA=="}}]}}]});
    let last_finish = json!({"candidates":[{"index":7,"finishReason":"MAX_TOKENS"}]});
    let usage = json!({"usageMetadata":{"promptTokenCount":4,"candidatesTokenCount":8}});
    for (version, wire) in versions() {
        let (release, held) = oneshot::channel::<()>();
        let frames = [initial.clone(), first_finish.clone(), last_finish.clone()];
        let final_usage = usage.clone();
        let body = async_stream::stream! {
            for frame in frames {
                yield Ok::<_, Infallible>(Bytes::from(format!("data: {frame}\n\n")));
            }
            let _ = held.await;
            yield Ok(Bytes::from(format!("data: {final_usage}\n\n")));
        };
        let response = Response::builder()
            .header("x-bitrouter-request-id", "execution")
            .header("content-type", "text/event-stream")
            .body(HttpBody::from_stream(body))?;
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
        let StreamItem::Event(actual) = events.next().await.ok_or("missing initial")?? else {
            return Err("initial native data was terminal".into());
        };
        assert_eq!(actual.data, initial);
        let next = events.next();
        tokio::pin!(next);
        assert!(
            tokio::time::timeout(Duration::from_millis(25), &mut next)
                .await
                .is_err()
        );
        release.send(()).map_err(|_| "native EOF owner closed")?;
        let StreamItem::Event(actual) = next.await.ok_or("missing first finish")?? else {
            return Err("end preceded held candidate events".into());
        };
        assert_eq!(actual.data, first_finish);
        for expected in [&last_finish, &usage] {
            let StreamItem::Event(actual) = events.next().await.ok_or("missing held event")??
            else {
                return Err("end preceded held candidate events".into());
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
    Ok(())
}

#[tokio::test]
async fn native_ambiguous_unfinished_or_reopened_candidates_refuse_without_end() -> TestResult {
    let cases = [
        json!({"candidates":[{"index":0,"finishReason":"STOP"},{"index":0,"finishReason":"STOP"}]}),
        json!({"candidates":[{"finishReason":"STOP"},{"index":1,"finishReason":"STOP"}]}),
        json!({"candidates":[{"index":0,"finishReason":"STOP"},{"index":1,"content":{"parts":[{"text":"unfinished"}]}}]}),
        json!({"candidates":[{"index":0,"finishReason":"STOP"},{"index":1,"finishReason":"FINISH_REASON_UNSPECIFIED"}]}),
    ];
    for (version, wire) in versions() {
        for value in &cases {
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
                if streaming {
                    let Body::Stream(mut events) = result?.body else {
                        return Err("expected native SSE".into());
                    };
                    assert!(
                        events
                            .next()
                            .await
                            .ok_or("missing candidate refusal")?
                            .is_err()
                    );
                    assert!(events.next().await.is_none());
                } else {
                    assert!(result.is_err());
                }
                assert_eq!(server.records.lock().await.len(), 1);
            }
        }
        let finish = json!({"candidates":[{"index":0,"finishReason":"STOP"},
            {"index":1,"content":{"parts":[{"text":"open"}]}}]});
        let invalid = json!({"candidates":[{"index":0,"content":{"parts":[{"text":"reopened"}]}},
            {"index":1,"finishReason":"STOP"}]});
        let response = Response::builder()
            .header("x-bitrouter-request-id", "execution")
            .header("content-type", "text/event-stream")
            .body(HttpBody::from(format!(
                "data: {finish}\n\ndata: {invalid}\n\n"
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
        assert!(
            events
                .next()
                .await
                .ok_or("missing reopened candidate refusal")?
                .is_err()
        );
        assert!(events.next().await.is_none());
    }
    Ok(())
}

#[tokio::test]
async fn native_candidate_identity_state_is_bounded_across_unfinished_events() -> TestResult {
    let body: String = (0..128).map(|index| {
        format!("data: {{\"candidates\":[{{\"index\":{index},\"content\":{{\"parts\":[{{\"text\":\"x\"}}]}}}}]}}\n\n")
    }).collect();
    for (version, wire) in versions() {
        let response = Response::builder()
            .header("x-bitrouter-request-id", "execution")
            .header("content-type", "text/event-stream")
            .body(HttpBody::from(body.clone()))?;
        let server = server::serve(wire, vec![Some(response)]).await?;
        let client = GatewayClient::with_options(
            &server.endpoint,
            None,
            TransportOptions {
                http_version: version,
                max_event_bytes: 1024,
                ..Default::default()
            },
        )?;
        let result = client
            .ordered_tenant("scoped-token", &binding(vec![0])?)?
            .with_generate_content(&caps()?)?
            .generate_content_reply(&input(true)?)
            .await?;
        let Body::Stream(mut events) = result.body else {
            return Err("expected native SSE".into());
        };
        let mut delivered = 0;
        while let Some(event) = events.next().await {
            match event {
                Ok(StreamItem::Event(_)) => delivered += 1,
                Err(Error::Response(failure)) => {
                    assert!(matches!(failure.problem, Some(Problem::Limit)));
                    break;
                }
                _ => return Err("identity limit lost its contract failure".into()),
            }
        }
        assert!(delivered > 0 && delivered < 128);
        assert!(events.next().await.is_none());
        assert_eq!(server.records.lock().await.len(), 1);
    }
    Ok(())
}

#[tokio::test]
async fn native_root_metadata_survives_open_and_new_candidates_after_first_finish() -> TestResult {
    for (version, wire) in versions() {
        for already_open in [false, true] {
            let mut initial =
                json!({"responseId":"r","candidates":[{"index":0,"finishReason":"STOP"}]});
            if already_open {
                initial["candidates"]
                    .as_array_mut()
                    .ok_or("candidate array")?
                    .push(json!({"index":1,"content":{"parts":[{"text":"first"}]}}));
            }
            let next = json!({"responseId":"r","promptFeedback":{"safetyRatings":[]},
                "candidates":[{"index":1,"content":{"parts":[{"text":"last","thoughtSignature":"sig"}]},
                    "finishReason":"STOP"}]});
            let usage = json!({"usageMetadata":{"promptTokenCount":4,"candidatesTokenCount":8}});
            let response = Response::builder()
                .header("x-bitrouter-request-id", "execution")
                .header("content-type", "text/event-stream")
                .body(HttpBody::from(format!(
                    "data: {initial}\n\ndata: {next}\n\ndata: {usage}\n\n"
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
            for expected in [&initial, &next, &usage] {
                let StreamItem::Event(actual) =
                    events.next().await.ok_or("missing retained metadata")??
                else {
                    return Err("native end preceded metadata".into());
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

#[tokio::test]
async fn native_json_and_sse_identity_budget_failures_retain_limit_and_execution() -> TestResult {
    let value = json!({"candidates":(0..12).map(|index| {
        json!({"index":index,"finishReason":"STOP"})
    }).collect::<Vec<_>>()});
    assert!(value.to_string().len() < 1024);
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
            let client = GatewayClient::with_options(
                &server.endpoint,
                None,
                TransportOptions {
                    http_version: version,
                    max_event_bytes: 1024,
                    max_response_bytes: 1024,
                    ..Default::default()
                },
            )?;
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
                    .ok_or("missing identity limit")?
                    .err()
                    .ok_or("identity limit emitted a native finish")?;
                assert!(events.next().await.is_none());
                error
            } else {
                result.err().ok_or("JSON identity limit was accepted")?
            };
            let Error::Response(failure) = error else {
                return Err("identity limit lost failure metadata".into());
            };
            assert!(matches!(failure.problem, Some(Problem::Limit)));
            assert_eq!(
                failure.metadata.execution_id.as_ref().map(|id| id.as_str()),
                Some("execution")
            );
            assert_eq!(server.records.lock().await.len(), 1);
        }
    }
    Ok(())
}
