use super::*;
use bitrouter_ai::stream::{ProtocolEnd, StreamItem};
use bitrouter_sdk::gateway::reply::Body as ReplyBody;

fn chat_request(stream: bool) -> TestResult<ChatRequest> {
    Ok(serde_json::from_value(
        json!({"model":"test","messages":[],"stream":stream}),
    )?)
}

#[tokio::test]
async fn background_replay_uses_the_market_base_and_preserves_the_cursor() -> TestResult {
    let server = serve(vec![
        Reply::sse("data: [DONE]\n\n"),
        Reply::sse("event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"),
    ])
    .await?;
    let client = GatewayClient::new(&format!("{}/market", server.url), Some("buyer"))?;
    let id = "a7ca5730-7ca2-48ae-ad66-ad7ca88a582f";
    assert!(
        client
            .resume_background_chat("invalid", None)
            .await
            .is_err()
    );
    client.resume_background_chat(id, Some(0)).await?;
    client.resume_background_messages(id, None).await?;
    let captured = server.records.lock().await;
    assert_eq!(captured.len(), 2);
    assert_eq!(
        captured[0].target,
        format!("/market/v1/requests/{id}/events?after=0")
    );
    assert_eq!(
        captured[1].target,
        format!("/market/v1/requests/{id}/events")
    );
    assert!(
        captured
            .iter()
            .all(|r| r.method == "GET" && r.credential.as_deref() == Some("Bearer buyer"))
    );
    Ok(())
}

#[tokio::test]
async fn relay_retains_done_and_only_allowlisted_metadata() -> TestResult {
    let mut reply = Reply::sse(
        "id: frame-1\ndata: {\"id\":\"r\",\"object\":\"chat.completion.chunk\",\"model\":\"test\",\"choices\":[],\"usage\":null,\"extension\":null}\n\ndata: [DONE]\n\n",
    );
    reply.headers.extend([
        ("x-bitrouter-request-id", "request-1".into()),
        ("x-bitrouter-execution-id", "execution-1".into()),
        ("location", "http://private/control".into()),
        ("set-cookie", "secret".into()),
    ]);
    let server = serve(vec![reply]).await?;
    let client = GatewayClient::new(&server.url, None)?;
    let reply = client.chat_reply(&chat_request(true)?).await?;
    assert_eq!(reply.metadata.status, 200);
    assert_eq!(reply.metadata.request_id.as_deref(), Some("request-1"));
    assert_eq!(reply.metadata.execution_id.as_deref(), Some("execution-1"));
    let ReplyBody::Stream(mut stream) = reply.body else {
        return Err("expected streaming reply".into());
    };
    match stream.next().await.transpose()? {
        Some(StreamItem::Event(event)) => {
            assert_eq!(event.id, "frame-1");
            assert!(event.data.extra.contains_key("extension"));
        }
        _ => return Err("expected event".into()),
    }
    assert!(matches!(
        stream.next().await.transpose()?,
        Some(StreamItem::End(ProtocolEnd::ChatDone))
    ));
    assert!(stream.next().await.is_none());
    Ok(())
}

#[tokio::test]
async fn relay_rejects_invalid_opening_before_returning_a_body() -> TestResult {
    let replies = vec![
        Reply {
            status: 202,
            headers: vec![("content-type", "text/event-stream".into())],
            chunks: vec![],
        },
        Reply::json(json!({"not":"an SSE stream"})),
        Reply {
            status: 429,
            headers: vec![("content-type", "application/json".into())],
            chunks: vec![br#"{"error":{"message":"limited"}}"#.to_vec()],
        },
        Reply {
            status: 200,
            headers: vec![
                ("content-type", "text/event-stream".into()),
                ("x-bitrouter-execution-id", "a".into()),
                ("x-bitrouter-execution-id", "b".into()),
            ],
            chunks: vec![],
        },
    ];
    let server = serve(replies).await?;
    let client = GatewayClient::new(&server.url, None)?;
    for _ in 0..4 {
        assert!(client.chat_reply(&chat_request(true)?).await.is_err());
    }
    assert_eq!(server.records.lock().await.len(), 4);
    Ok(())
}

#[tokio::test]
async fn sdk_truncation_and_error_never_become_an_explicit_end() -> TestResult {
    let server = serve(vec![Reply::sse("data: {\"id\":\"r\",\"object\":\"chat.completion.chunk\",\"model\":\"test\",\"choices\":[]}\n\n"), Reply::sse("data: {\"error\":{\"message\":\"bad\"}}\n\ndata: [DONE]\n\n")]).await?;
    let client = GatewayClient::new(&server.url, None)?;
    for _ in 0..2 {
        let reply = client.chat_reply(&chat_request(true)?).await?;
        let ReplyBody::Stream(mut stream) = reply.body else {
            return Err("expected stream".into());
        };
        let mut failed = false;
        while let Some(item) = stream.next().await {
            match item {
                Ok(StreamItem::End(_)) => return Err("unexpected end".into()),
                Err(_) => failed = true,
                _ => {}
            }
        }
        assert!(failed);
    }
    Ok(())
}
