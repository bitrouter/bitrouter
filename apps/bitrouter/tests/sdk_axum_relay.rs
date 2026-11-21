//! SDK streams composed with application-owned HTTP adapters.

use axum::{body::Body, response::IntoResponse};
use bitrouter::gateway::http_adapter::{error::Error as PublicError, response::Reply as HttpReply};
use bitrouter_ai::protocol::{
    chat_completions::ChatRequest, messages::MessagesRequest, responses::ResponsesRequest,
};
use bitrouter_sdk::{
    error::Error,
    gateway::{GatewayClient, reply::Body as ReplyBody},
};
use futures::{StreamExt, TryStreamExt};
use serde_json::{Value, json};
use std::{collections::VecDeque, sync::Arc};
use tokio::{net::TcpListener, sync::Mutex, task::JoinHandle};

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error + Send + Sync>>;
struct Reply {
    headers: Vec<(&'static str, String)>,
    body: String,
}
impl Reply {
    fn json(value: Value) -> Self {
        Self {
            headers: vec![("content-type", "application/json".into())],
            body: value.to_string(),
        }
    }
    fn sse(body: &str) -> Self {
        Self {
            headers: vec![("content-type", "text/event-stream".into())],
            body: body.into(),
        }
    }
}
struct Server {
    url: String,
    task: JoinHandle<()>,
}
impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}
async fn serve(replies: Vec<Reply>) -> TestResult<Server> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let url = format!("http://{}", listener.local_addr()?);
    let replies = Arc::new(Mutex::new(VecDeque::from(replies)));
    let app = axum::Router::new().fallback(axum::routing::any(move || {
        let replies = replies.clone();
        async move {
            let Some(reply) = replies.lock().await.pop_front() else {
                return axum::http::StatusCode::INTERNAL_SERVER_ERROR.into_response();
            };
            let mut response = axum::response::Response::builder();
            for (key, value) in reply.headers {
                response = response.header(key, value);
            }
            response
                .body(Body::from(reply.body))
                .unwrap_or_else(|_| axum::http::StatusCode::INTERNAL_SERVER_ERROR.into_response())
        }
    }));
    let task = tokio::spawn(async move {
        if let Err(error) = axum::serve(listener, app).await {
            eprintln!("fixture failed: {error}");
        }
    });
    Ok(Server { url, task })
}
fn chat_request(stream: bool) -> TestResult<ChatRequest> {
    Ok(serde_json::from_value(
        json!({"model":"test", "messages":[], "stream":stream}),
    )?)
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
    Ok(Server { url, task })
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
