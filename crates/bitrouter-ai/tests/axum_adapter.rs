#![cfg(feature = "axum")]

use std::{
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    task::{Context, Poll},
};

use axum::{
    body::{Body, to_bytes},
    extract::DefaultBodyLimit,
    http::{Request, StatusCode},
    response::IntoResponse,
    routing::post,
};
use bitrouter_ai::{
    axum::{error::Error, extract::Json, response::Reply},
    protocol::{
        chat_completions::{ChatChunk, ChatRequest},
        messages::MessagesEvent,
        responses::ResponsesEvent,
    },
    stream::{ProtocolEnd, ServerEvent, StreamItem},
};
use futures_core::Stream;
use futures_util::stream;
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error + Send + Sync>>;
const LIMIT: usize = 8192;

fn event<T>(name: &str, data: T) -> StreamItem<T> {
    StreamItem::Event(ServerEvent {
        event: name.into(),
        id: "id-1".into(),
        data,
    })
}
fn chunk() -> TestResult<ChatChunk> {
    Ok(serde_json::from_value(
        json!({"id":"r","object":"chat.completion.chunk","model":"test","choices":[],"usage":null,"extension":{"null":null}}),
    )?)
}
async fn body(reply: Reply) -> TestResult<String> {
    let response = reply.into_response();
    assert_eq!(response.headers()["cache-control"], "no-store");
    Ok(String::from_utf8(
        to_bytes(response.into_body(), LIMIT).await?.to_vec(),
    )?)
}

#[tokio::test]
async fn extractor_preserves_public_payload_and_redacts_rejections() -> TestResult {
    async fn echo(Json(request): Json<ChatRequest>) -> Result<Reply, Error> {
        Reply::json(&request, LIMIT).map_err(|_| Error::bad_gateway())
    }
    let app = axum::Router::new()
        .route("/", post(echo))
        .layer(DefaultBodyLimit::max(256));
    let payload =
        json!({"model":"test","messages":[{"role":"user","content":"hello"}],"extension":null});
    let response = app
        .clone()
        .oneshot(
            Request::post("/")
                .header("content-type", "application/json")
                .body(Body::from(payload.to_string()))?,
        )
        .await?;
    assert_eq!(response.status(), StatusCode::OK);
    let decoded: Value = serde_json::from_slice(&to_bytes(response.into_body(), LIMIT).await?)?;
    assert_eq!(decoded["extension"], Value::Null);
    assert_eq!(decoded["messages"][0]["content"], "hello");
    for (content_type, data, status) in [
        (
            "application/json",
            "{\"model\":\"DO-NOT-LEAK\"}".into(),
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
        (
            "application/json",
            "{secret".into(),
            StatusCode::BAD_REQUEST,
        ),
        (
            "text/plain",
            "{}".into(),
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
        ),
        (
            "application/json",
            "x".repeat(257),
            StatusCode::PAYLOAD_TOO_LARGE,
        ),
    ] {
        let response = app
            .clone()
            .oneshot(
                Request::post("/")
                    .header("content-type", content_type)
                    .body(Body::from(data))?,
            )
            .await?;
        assert_eq!(response.status(), status);
        let bytes = to_bytes(response.into_body(), LIMIT).await?;
        let public: Value = serde_json::from_slice(&bytes)?;
        assert!(public["error"]["message"].is_string());
        assert!(!String::from_utf8(bytes.to_vec())?.contains("DO-NOT-LEAK"));
    }
    Ok(())
}

#[tokio::test]
async fn chat_preserves_fields_and_requires_explicit_done() -> TestResult {
    let text = body(Reply::chat_stream(
        stream::iter(vec![
            Ok(event("message", chunk()?)),
            Ok(StreamItem::End(ProtocolEnd::ChatDone)),
        ]),
        LIMIT,
    ))
    .await?;
    assert!(text.contains("id: id-1\n"));
    assert!(text.contains("\"usage\":null"));
    assert!(text.contains("\"extension\":{\"null\":null}"));
    assert!(text.ends_with("data: [DONE]\n\n"));
    for ending in [
        None,
        Some(Ok(StreamItem::End(ProtocolEnd::MessagesStop))),
        Some(Err(Error::bad_gateway())),
    ] {
        let mut items = vec![Ok(event("message", chunk()?))];
        items.extend(ending);
        let text = body(Reply::chat_stream(stream::iter(items), LIMIT)).await?;
        assert!(text.contains("\"error\""));
        assert!(!text.contains("[DONE]"));
    }
    Ok(())
}

#[tokio::test]
async fn messages_stop_waits_for_matching_end() -> TestResult {
    for end in [
        Some(ProtocolEnd::MessagesStop),
        None,
        Some(ProtocolEnd::ChatDone),
    ] {
        let completed = end == Some(ProtocolEnd::MessagesStop);
        let terminal: MessagesEvent =
            serde_json::from_value(json!({"type":"message_stop","extension":null}))?;
        let mut items = vec![Ok(event("message_stop", terminal))];
        if let Some(end) = end {
            items.push(Ok(StreamItem::End(end)));
        }
        let text = body(Reply::messages_stream(stream::iter(items), LIMIT)).await?;
        assert_eq!(text.contains("event: message_stop\n"), completed);
        assert_eq!(text.contains("event: error\n"), !completed);
        assert!(!text.contains("[DONE]"));
    }
    Ok(())
}

#[tokio::test]
async fn finish_and_usage_remain_ordered_before_done() -> TestResult {
    let finish: ChatChunk = serde_json::from_value(
        json!({"id":"r","object":"chat.completion.chunk","model":"test","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}),
    )?;
    let usage: ChatChunk = serde_json::from_value(
        json!({"id":"r","object":"chat.completion.chunk","model":"test","choices":[],"usage":{"prompt_tokens":3,"completion_tokens":2,"total_tokens":5}}),
    )?;
    let text = body(Reply::chat_stream(
        stream::iter(vec![
            Ok(event("message", finish)),
            Ok(event("message", usage)),
            Ok(StreamItem::End(ProtocolEnd::ChatDone)),
        ]),
        LIMIT,
    ))
    .await?;
    assert!(
        text.find("finish_reason").ok_or("missing finish")?
            < text.find("prompt_tokens").ok_or("missing usage")?
    );
    assert!(
        text.find("prompt_tokens").ok_or("missing usage")?
            < text.find("[DONE]").ok_or("missing end")?
    );
    assert_eq!(text.matches("[DONE]").count(), 1);
    Ok(())
}

#[tokio::test]
async fn responses_preserve_each_terminal_outcome() -> TestResult {
    for (status, end) in [
        ("completed", ProtocolEnd::ResponsesCompleted),
        ("incomplete", ProtocolEnd::ResponsesIncomplete),
        ("failed", ProtocolEnd::ResponsesFailed),
    ] {
        let kind = format!("response.{status}");
        let terminal: ResponsesEvent = serde_json::from_value(
            json!({"type":kind,"sequence_number":2,"response":{"id":"r","object":"response","model":"test","status":status,"output":[]},"extension":null}),
        )?;
        let text = body(Reply::responses_stream(
            stream::iter(vec![Ok(event(&kind, terminal)), Ok(StreamItem::End(end))]),
            LIMIT,
        ))
        .await?;
        assert!(text.contains(&format!("event: {kind}\n")));
        assert!(text.contains("\"extension\":null"));
        assert!(!text.contains("[DONE]"));
    }
    Ok(())
}

#[tokio::test]
async fn pending_terminal_cannot_mask_later_failure() -> TestResult {
    let terminal: ResponsesEvent = serde_json::from_value(
        json!({"type":"response.completed","sequence_number":9,"response":{"id":"r","object":"response","model":"test","status":"completed","output":[]}}),
    )?;
    let text = body(Reply::responses_stream(
        stream::iter(vec![
            Ok(event("response.completed", terminal)),
            Err(Error::bad_gateway()),
            Ok(StreamItem::End(ProtocolEnd::ResponsesCompleted)),
        ]),
        LIMIT,
    ))
    .await?;
    assert!(!text.contains("response.completed"));
    assert!(text.contains("event: error\n"));
    assert!(text.contains("\"sequence_number\":10"));
    Ok(())
}

#[tokio::test]
async fn flattened_control_fields_cannot_publish_unconfirmed_terminals() -> TestResult {
    for key in ["type", "response", "sequence_number"] {
        let mut payload: ResponsesEvent = serde_json::from_value(json!({
            "type":"response.created", "response":{"id":"r", "object":"response", "model":"test", "status":"completed"}
        }))?;
        payload
            .extra
            .insert(key.into(), json!("response.completed"));
        let text = body(Reply::responses_stream(
            stream::iter(vec![Ok(event("message", payload))]),
            LIMIT,
        ))
        .await?;
        assert!(!text.contains("response.completed"));
        assert!(text.contains("event: error\n"));
    }
    let mut payload: ResponsesEvent = serde_json::from_value(json!({
        "type":"response.completed", "response":{"id":"r", "object":"response", "model":"test", "status":"completed"}
    }))?;
    payload
        .response
        .as_mut()
        .and_then(Option::as_mut)
        .ok_or("missing response")?
        .extra
        .insert("status".into(), json!("failed"));
    let text = body(Reply::responses_stream(
        stream::iter(vec![
            Ok(event("response.completed", payload)),
            Ok(StreamItem::End(ProtocolEnd::ResponsesCompleted)),
        ]),
        LIMIT,
    ))
    .await?;
    assert!(!text.contains("event: response.completed\n"));
    let mut payload: MessagesEvent = serde_json::from_value(json!({"type":"message_start"}))?;
    payload.extra.insert("type".into(), json!("message_stop"));
    let text = body(Reply::messages_stream(
        stream::iter(vec![Ok(event("message", payload))]),
        LIMIT,
    ))
    .await?;
    assert!(!text.contains("message_stop"));
    assert!(text.contains("event: error\n"));
    Ok(())
}

#[tokio::test]
async fn oversized_public_errors_are_replaced_before_retention() -> TestResult {
    let large = "PRIVATE".repeat(1000);
    let error = Error::new(StatusCode::BAD_REQUEST, "invalid_request", "bad", &large);
    assert_eq!(error.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let response = error.clone().into_response();
    let bytes = to_bytes(response.into_body(), 1024).await?;
    assert!(!std::str::from_utf8(&bytes)?.contains("PRIVATE"));
    let text = body(Reply::chat_stream(
        stream::iter(vec![Err::<StreamItem<ChatChunk>, _>(error)]),
        LIMIT,
    ))
    .await?;
    assert!(!text.contains("PRIVATE"));
    assert!(!text.contains("[DONE]"));
    Ok(())
}

#[tokio::test]
async fn native_error_payloads_latch_failure_and_end_stops_polling() -> TestResult {
    let mut payload = chunk()?;
    payload
        .extra
        .insert("error".into(), json!({"message":"PRIVATE"}));
    let polled = Arc::new(AtomicUsize::new(0));
    let watched = polled.clone();
    let stream = futures_util::StreamExt::inspect(
        stream::iter(vec![
            Ok(event("message", payload)),
            Ok(StreamItem::End(ProtocolEnd::ChatDone)),
        ]),
        move |_| {
            watched.fetch_add(1, Ordering::SeqCst);
        },
    );
    let text = body(Reply::chat_stream(stream, LIMIT)).await?;
    assert!(!text.contains("[DONE]"));
    assert!(!text.contains("PRIVATE"));
    assert_eq!(polled.load(Ordering::SeqCst), 1);

    let polled = Arc::new(AtomicUsize::new(0));
    let watched = polled.clone();
    let stream = futures_util::StreamExt::inspect(
        stream::iter(vec![
            Ok(StreamItem::<ChatChunk>::End(ProtocolEnd::ChatDone)),
            Err(Error::bad_gateway()),
        ]),
        move |_| {
            watched.fetch_add(1, Ordering::SeqCst);
        },
    );
    let text = body(Reply::chat_stream(stream, LIMIT)).await?;
    assert_eq!(text, "data: [DONE]\n\n");
    assert_eq!(polled.load(Ordering::SeqCst), 1);
    Ok(())
}

#[tokio::test]
async fn invalid_frame_metadata_and_bounds_never_emit_done() -> TestResult {
    let bad = StreamItem::Event(ServerEvent {
        event: "message".into(),
        id: "evil\ndata: [DONE]".into(),
        data: chunk()?,
    });
    let text = body(Reply::chat_stream(
        stream::iter(vec![Ok(bad), Ok(StreamItem::End(ProtocolEnd::ChatDone))]),
        LIMIT,
    ))
    .await?;
    assert!(!text.contains("[DONE]"));
    assert!(text.contains("\"error\""));
    let response =
        Reply::chat_stream(stream::iter(vec![Ok(event("message", chunk()?))]), 1).into_response();
    assert!(to_bytes(response.into_body(), LIMIT).await.is_err());
    assert!(Reply::json(&json!({"large":"x".repeat(100)}), 10).is_err());
    Ok(())
}

struct Observed {
    polls: Arc<AtomicUsize>,
    dropped: Arc<AtomicBool>,
    next: Option<StreamItem<ChatChunk>>,
}
impl Stream for Observed {
    type Item = Result<StreamItem<ChatChunk>, Error>;
    fn poll_next(mut self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.polls.fetch_add(1, Ordering::SeqCst);
        Poll::Ready(self.next.take().map(Ok))
    }
}
impl Drop for Observed {
    fn drop(&mut self) {
        self.dropped.store(true, Ordering::SeqCst);
    }
}

#[tokio::test]
async fn adapter_is_pull_driven_and_drop_releases_source() -> TestResult {
    let polls = Arc::new(AtomicUsize::new(0));
    let dropped = Arc::new(AtomicBool::new(false));
    let response = Reply::chat_stream(
        Observed {
            polls: polls.clone(),
            dropped: dropped.clone(),
            next: Some(event("message", chunk()?)),
        },
        LIMIT,
    )
    .into_response();
    assert_eq!(polls.load(Ordering::SeqCst), 0);
    let mut body = response.into_body();
    assert!(body.frame().await.transpose()?.is_some());
    assert_eq!(polls.load(Ordering::SeqCst), 1);
    drop(body);
    assert!(dropped.load(Ordering::SeqCst));
    Ok(())
}

#[tokio::test]
async fn json_reply_retains_decisions_shape_and_does_not_hide_encoding_failure() -> TestResult {
    let value = json!({"model":"judge","answers":[{"type":"predicate","name":null,"probability":0.75}],"usage":{"input_tokens":4,"output_tokens":2,"total_tokens":6,"input_tokens_details":{"cached_tokens":1,"cache_write_tokens":0},"output_tokens_details":{"reasoning_tokens":1}},"extension":null});
    let response: bitrouter_ai::protocol::decisions::DecisionsResponse =
        serde_json::from_value(value.clone())?;
    let reply = Reply::json(&response, LIMIT)?.into_response();
    assert_eq!(reply.headers()["content-type"], "application/json");
    assert_eq!(
        serde_json::from_slice::<Value>(&to_bytes(reply.into_body(), LIMIT).await?)?,
        value
    );
    let error = Error::new(StatusCode::OK, "unsafe", "unsafe", "DO-NOT-LEAK").into_response();
    assert_eq!(error.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert!(
        !String::from_utf8(to_bytes(error.into_body(), LIMIT).await?.to_vec())?
            .contains("DO-NOT-LEAK")
    );
    Ok(())
}
