use super::generate_content::{caps, input};
use super::*;

#[tokio::test]
async fn native_duplicate_json_members_and_answer_bearing_tails_never_complete() -> TestResult {
    let finish = "data: {\"responseId\":\"r\",\"modelVersion\":\"v\",\"candidates\":[{\"index\":0,\"finishReason\":\"STOP\"}]}\n\n";
    let tails = [
        r#"{"candidates":[{"index":0,"finishReason":"STOP","groundingMetadata":{"groundingChunks":[{"web":{"uri":"https://late.invalid"}}]}}]}"#,
        r#"{"candidates":[{"index":0,"citationMetadata":{"citationSources":[{"uri":"https://late.invalid"}]}}]}"#,
        r#"{"groundingMetadata":{"groundingChunks":[]}}"#,
        r#"{"responseId":"changed","responseId":"r","usageMetadata":{}}"#,
        r#"{"responseId":"changed","response\u0049d":"r"}"#,
        r#"{"candidates":[{"index":1,"index":0,"finishReason":"STOP"}]}"#,
        r#"{"candidates":[{"index":0,"finishReason":"MAX_TOKENS","finishReason":"STOP"}]}"#,
        r#"{"usageMetadata":{"promptTokenCount":40,"promptTokenCount":0}}"#,
        r#"{"modelVersion":"changed"}"#,
    ];
    let json = [
        r#"{"error":{"code":500},"error":null,"candidates":[{"finishReason":"STOP"}]}"#,
        r#"{"candidates":[{"finishReason":"MAX_TOKENS","finishReason":"STOP"}]}"#,
        r#"{"responseId":"a","responseId":"b","candidates":[{"finishReason":"STOP"}]}"#,
        r#"{"responseId":"a","response\u0049d":"b","candidates":[{"finishReason":"STOP"}]}"#,
        r#"{"candidates":[{"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":40,"promptTokenCount":0}}"#,
    ];
    for (version, wire) in versions() {
        for (streaming, raw) in tails
            .iter()
            .map(|tail| (true, format!("{finish}data: {tail}\n\n")))
            .chain(json.iter().map(|json| (false, (*json).to_owned())))
        {
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
                .body(HttpBody::from(raw))?;
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
                let error = events
                    .next()
                    .await
                    .ok_or("missing native refusal")?
                    .err()
                    .ok_or("native invalid boundary emitted a held event")?;
                assert!(events.next().await.is_none());
                error
            } else {
                result.err().ok_or("duplicate native JSON was accepted")?
            };
            let Error::Response(failure) = error else {
                return Err("native refusal lost execution metadata".into());
            };
            assert_eq!(
                failure.metadata.execution_id.as_ref().map(|id| id.as_str()),
                Some("execution")
            );
            assert!(matches!(failure.problem, Some(Problem::Contract)));
            assert_eq!(server.records.lock().await.len(), 1);
        }
    }
    Ok(())
}

#[tokio::test]
async fn native_initial_grounding_and_citations_survive_with_late_usage_and_stable_terminal()
-> TestResult {
    let initial = json!({"responseId":"r","modelVersion":"v","candidates":[{"index":0,
        "content":{"parts":[{"text":"answer"}]},"finishReason":"STOP",
        "groundingMetadata":{"groundingChunks":[{"web":{"uri":"https://initial.invalid"}}]},
        "citationMetadata":{"citationSources":[{"uri":"https://initial.invalid"}]}}]});
    let usage = json!({"responseId":"r","modelVersion":"v","usageMetadata":{
        "promptTokenCount":4,"candidatesTokenCount":2,"nested":[true,null,-1,1.25,"x"]}});
    let terminal = json!({"candidates":[{"index":0,"finishReason":"STOP"}]});
    for (version, wire) in versions() {
        let response = Response::builder()
            .header("x-bitrouter-request-id", "execution")
            .header("content-type", "text/event-stream")
            .body(HttpBody::from(format!(
                "data: {initial}\n\ndata: {usage}\n\ndata: {terminal}\n\n"
            )))?;
        let server = server::serve(wire, vec![Some(response)]).await?;
        let client = gateway_client(&server, version)?;
        let reply = client
            .ordered_tenant("scoped-token", &binding(vec![0])?)?
            .with_generate_content(&caps()?)?
            .generate_content_reply(&input(true)?)
            .await?;
        let Body::Stream(mut events) = reply.body else {
            return Err("expected SSE".into());
        };
        for expected in [&initial, &usage, &terminal] {
            let StreamItem::Event(event) =
                events.next().await.ok_or("missing retained event")??
            else {
                return Err("native end preceded retained frames".into());
            };
            assert_eq!(&event.data, expected);
        }
        assert!(matches!(
            events.next().await.ok_or("missing native end")??,
            StreamItem::End(ProtocolEnd::GenerateContentFinished)
        ));
        assert!(events.next().await.is_none());
    }
    Ok(())
}
