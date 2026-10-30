use super::*;
use axum::response::IntoResponse;
use bitrouter_ai::{
    axum::{error::Error as PublicError, response::Reply as HttpReply},
    stream::{ProtocolEnd, StreamItem},
};
use bitrouter_sdk::gateway::reply::Body as ReplyBody;
use futures_util::TryStreamExt;

fn chat_request(stream: bool) -> TestResult<ChatRequest> {
    Ok(serde_json::from_value(
        json!({"model":"test","messages":[],"stream":stream}),
    )?)
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
    // The former SDK path is exactly the AI-owned type, not another DTO.
    let compatibility_event: bitrouter_sdk::stream::ServerEvent<()> =
        bitrouter_ai::stream::ServerEvent {
            event: "message".into(),
            id: String::new(),
            data: (),
        };
    assert_eq!(compatibility_event.event, "message");
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

async fn proxy(client: GatewayClient) -> TestResult<Server> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let url = format!("http://{}", listener.local_addr()?);
    let app = axum::Router::new().route(
        "/v1/chat/completions",
        axum::routing::post(move |axum::Json(request): axum::Json<ChatRequest>| {
            let client = client.clone();
            async move {
                let reply = client
                    .chat_reply(&request)
                    .await
                    .map_err(|_| PublicError::bad_gateway())?;
                // Deliberately no blanket metadata/header forwarding.
                match reply.body {
                    ReplyBody::Json(json) => {
                        HttpReply::json(&json, 8192).map_err(|_| PublicError::bad_gateway())
                    }
                    ReplyBody::Stream(stream) => Ok(HttpReply::chat_stream(
                        stream.map_err(|_| PublicError::bad_gateway()),
                        8192,
                    )),
                }
            }
        }),
    );
    let task = tokio::spawn(async move {
        if let Err(error) = axum::serve(listener, app).await {
            eprintln!("fixture failed: {error}");
        }
    });
    Ok(Server {
        url,
        records: Arc::new(Mutex::new(vec![])),
        task,
    })
}

#[tokio::test]
async fn real_http_sdk_to_axum_roundtrip_preserves_json_stream_and_errors() -> TestResult {
    let json = json!({"id":"r","object":"chat.completion","model":"test","choices":[],"usage":null,"extension":{"a":null}});
    let mut stream = Reply::sse(
        "data: {\"id\":\"r\",\"object\":\"chat.completion.chunk\",\"model\":\"test\",\"choices\":[],\"extension\":null}\n\ndata: [DONE]\n\n",
    );
    stream.headers.extend([
        ("set-cookie", "secret".into()),
        ("location", "http://private/control".into()),
    ]);
    let upstream = serve(vec![
        Reply::json(json.clone()),
        stream,
        Reply::sse("data: {\"error\":{\"message\":\"PRIVATE\"}}\n\ndata: [DONE]\n\n"),
    ])
    .await?;
    let proxy = proxy(GatewayClient::new(&upstream.url, None)?).await?;
    let client = GatewayClient::new(&proxy.url, None)?;
    assert_eq!(
        serde_json::to_value(client.chat_completions(&chat_request(false)?).await?)?,
        json
    );
    let reply = reqwest::Client::new()
        .post(format!("{}/v1/chat/completions", proxy.url))
        .json(&chat_request(true)?)
        .send()
        .await?;
    assert!(reply.headers().get("set-cookie").is_none());
    assert!(reply.headers().get("location").is_none());
    let text = reply.text().await?;
    assert!(text.contains("\"extension\":null"));
    assert_eq!(text.matches("[DONE]").count(), 1);
    let mut stream = client.stream_chat_completions(&chat_request(true)?).await?;
    let error = stream
        .next()
        .await
        .ok_or("expected failure")?
        .err()
        .ok_or("expected error")?;
    assert!(matches!(error, Error::Stream(_)));
    assert!(!format!("{error:?}").contains("PRIVATE"));
    assert!(stream.next().await.is_none());
    Ok(())
}

#[tokio::test]
async fn messages_and_responses_roundtrip_keep_distinct_ends() -> TestResult {
    let mut replies = vec![Reply::sse(
        "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
    )];
    for status in ["completed", "incomplete", "failed"] {
        replies.push(Reply::sse(&format!("event: response.{status}\ndata: {{\"type\":\"response.{status}\",\"sequence_number\":4,\"response\":{{\"id\":\"r\",\"object\":\"response\",\"model\":\"test\",\"status\":\"{status}\",\"output\":[]}}}}\n\n")));
    }
    let server = serve(replies).await?;
    let client = GatewayClient::new(&server.url, None)?;
    let request: MessagesRequest = serde_json::from_value(
        json!({"model":"test","messages":[],"max_tokens":10,"stream":true}),
    )?;
    let ReplyBody::Stream(stream) = client.messages_reply(&request).await?.body else {
        return Err("expected stream".into());
    };
    let response = HttpReply::messages_stream(stream.map_err(|_| PublicError::bad_gateway()), 8192)
        .into_response();
    let text = String::from_utf8(
        axum::body::to_bytes(response.into_body(), 8192)
            .await?
            .to_vec(),
    )?;
    assert!(text.contains("event: message_stop\n"));
    for status in ["completed", "incomplete", "failed"] {
        let request: ResponsesRequest =
            serde_json::from_value(json!({"model":"test","input":"hi","stream":true}))?;
        let ReplyBody::Stream(stream) = client.responses_reply(&request).await?.body else {
            return Err("expected stream".into());
        };
        let response =
            HttpReply::responses_stream(stream.map_err(|_| PublicError::bad_gateway()), 8192)
                .into_response();
        let text = String::from_utf8(
            axum::body::to_bytes(response.into_body(), 8192)
                .await?
                .to_vec(),
        )?;
        assert!(text.contains(&format!("event: response.{status}\n")));
        assert!(!text.contains("[DONE]"));
    }
    Ok(())
}
