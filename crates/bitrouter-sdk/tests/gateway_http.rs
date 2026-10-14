//! Real HTTP coverage without importing or assembling the BitRouter service.

use std::{collections::VecDeque, error::Error as StdError, sync::Arc};

use axum::{body::Body, extract::Request, response::Response, routing::any};
use bitrouter_ai::{
    decisions::DecisionRequest,
    protocol::{
        chat_completions::ChatRequest, messages::MessagesRequest, responses::ResponsesRequest,
    },
};
use bitrouter_sdk::{
    error::Error,
    gateway::{GatewayClient, TransportOptions},
};
use futures_util::StreamExt;
use serde_json::{Value, json};
use tokio::{net::TcpListener, sync::Mutex, task::JoinHandle};

type TestResult<T = ()> = Result<T, Box<dyn StdError + Send + Sync>>;

struct Reply {
    status: u16,
    headers: Vec<(&'static str, String)>,
    chunks: Vec<Vec<u8>>,
}
impl Reply {
    fn json(body: Value) -> Self {
        Self {
            status: 200,
            headers: vec![("content-type", "application/json".into())],
            chunks: vec![body.to_string().into_bytes()],
        }
    }
    fn sse(body: &str) -> Self {
        Self {
            status: 200,
            headers: vec![("content-type", "text/event-stream; charset=utf-8".into())],
            chunks: body.as_bytes().iter().map(|byte| vec![*byte]).collect(),
        }
    }
}
struct Recorded {
    path: String,
    credential: Option<String>,
    body: Vec<u8>,
}
struct Server {
    url: String,
    records: Arc<Mutex<Vec<Recorded>>>,
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
    let records = Arc::new(Mutex::new(Vec::new()));
    let captured = records.clone();
    let replies = Arc::new(Mutex::new(VecDeque::from(replies)));
    let app = axum::Router::new().fallback(any(move |request: Request| {
        let records = captured.clone();
        let replies = replies.clone();
        async move {
            let result = async {
                let (parts, body) = request.into_parts();
                let body = axum::body::to_bytes(body, 1024 * 1024).await?;
                records.lock().await.push(Recorded {
                    path: parts.uri.path().into(),
                    credential: parts
                        .headers
                        .get("authorization")
                        .and_then(|v| v.to_str().ok())
                        .map(str::to_owned),
                    body: body.to_vec(),
                });
                let reply = replies
                    .lock()
                    .await
                    .pop_front()
                    .ok_or_else(|| std::io::Error::other("unexpected request"))?;
                let mut response = Response::builder().status(reply.status);
                for (name, value) in reply.headers {
                    response = response.header(name, value);
                }
                Ok::<_, Box<dyn StdError + Send + Sync>>(response.body(Body::from_stream(
                    futures_util::stream::iter(
                        reply.chunks.into_iter().map(Ok::<_, std::io::Error>),
                    ),
                ))?)
            }
            .await;
            match result {
                Ok(response) => response,
                Err(_) => {
                    let mut response = Response::new(Body::from("fixture failed"));
                    *response.status_mut() = axum::http::StatusCode::INTERNAL_SERVER_ERROR;
                    response
                }
            }
        }
    }));
    let task = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    Ok(Server { url, records, task })
}
fn chat_request(stream: bool) -> TestResult<ChatRequest> {
    Ok(serde_json::from_value(
        json!({"model":"route", "stream":stream, "messages":[{"role":"user","content":"hello", "extra_content":{"native":"retain"}}]}),
    )?)
}
fn messages_request(stream: bool) -> TestResult<MessagesRequest> {
    Ok(serde_json::from_value(
        json!({"model":"route", "stream":stream, "messages":[{"role":"user","content":"hello"}]}),
    )?)
}
fn responses_request(stream: bool) -> TestResult<ResponsesRequest> {
    Ok(serde_json::from_value(
        json!({"model":"route", "stream":stream, "input":"hello"}),
    )?)
}
fn chat_response() -> Value {
    json!({"id":"c-1","object":"chat.completion","model":"route","choices":[{"index":0,"message":{"role":"assistant","content":"hello","extra_content":{"signature":"opaque"}},"finish_reason":"stop"}]})
}
fn chat_chunk() -> Value {
    json!({"id":"c-1","object":"chat.completion.chunk","model":"route","choices":[{"index":0,"delta":{"content":"你好"},"finish_reason":null}]})
}

#[tokio::test]
async fn typed_calls_preserve_wire_data_and_use_only_the_service_endpoint() -> TestResult {
    let decisions = json!({"model":"judge","answers":[{"type":"predicate","name":null,"probability":0.75}],"usage":{"input_tokens":4,"output_tokens":2,"total_tokens":6,"input_tokens_details":{"cached_tokens":1,"cache_write_tokens":0,"source_detail":"keep"},"output_tokens_details":{"reasoning_tokens":1}},"extra_receipt":"keep"});
    let server = serve(vec![
        Reply::json(chat_response()),
        Reply::json(json!({"id":"m-1","type":"message","role":"assistant","model":"route","content":[{"type":"thinking","thinking":"reason","signature":"opaque"}],"stop_reason":"end_turn","usage":{"input_tokens":3,"output_tokens":2,"cache_read_input_tokens":5}})),
        Reply::json(json!({"id":"r-1","object":"response","model":"route","status":"completed","output":[{"type":"reasoning","encrypted_content":"opaque","summary":[{"type":"summary_text","text":"reason"}],"native_marker":"keep"}]})),
        Reply::json(decisions.clone()),
        Reply::json(json!({"object":"list","data":[{"id":"route","object":"model","providers":["one","two"]}]})),
        Reply::json(json!({"status":"ok"})),
        Reply { status: 200, headers: vec![("content-type","text/plain".into())], chunks: vec![b"requests_total 1\n".to_vec()] },
    ]).await?;
    let client = GatewayClient::new(
        &format!("{}/proxy/v1/", server.url),
        Some("private-credential"),
    )?;
    assert!(!format!("{client:?}").contains("private-credential"));
    let chat = client.chat_completions(&chat_request(false)?).await?;
    assert!(chat.usage.is_none());
    assert_eq!(
        chat.choices[0].message.extra["extra_content"]["signature"],
        "opaque"
    );
    let messages = client.messages(&messages_request(false)?).await?;
    assert_eq!(messages.usage.input_tokens, 3); // Do not add cache tokens in a wire DTO.
    assert_eq!(messages.usage.cache_read_input_tokens, Some(Some(5)));
    let responses = client.responses(&responses_request(false)?).await?;
    assert!(responses.usage.is_none());
    assert_eq!(
        serde_json::to_value(responses)?["output"][0]["native_marker"],
        "keep"
    );
    let request: DecisionRequest = serde_json::from_value(
        json!({"model":"judge","input":"evidence","questions":[{"type":"predicate","instructions":"Is it correct?"}]}),
    )?;
    assert_eq!(
        serde_json::to_value(client.decisions(&request).await?)?,
        decisions
    );
    assert_eq!(client.models().await?.data[0].providers, vec!["one", "two"]);
    assert_eq!(client.health().await?.status, "ok");
    assert_eq!(client.metrics().await?, "requests_total 1\n");
    let records = server.records.lock().await;
    assert_eq!(
        records.iter().map(|r| r.path.as_str()).collect::<Vec<_>>(),
        vec![
            "/proxy/v1/chat/completions",
            "/proxy/v1/messages",
            "/proxy/v1/responses",
            "/proxy/v1/decisions",
            "/proxy/v1/models",
            "/proxy/health",
            "/proxy/metrics"
        ]
    );
    assert!(
        records
            .iter()
            .all(|r| r.credential.as_deref() == Some("Bearer private-credential"))
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&records[0].body)?["messages"][0]["extra_content"]["native"],
        "retain"
    );
    Ok(())
}

#[tokio::test]
async fn streaming_preserves_events_usage_and_distinct_terminal_protocols() -> TestResult {
    // Multiline JSON and byte-fragmented UTF-8, including the optional initial BOM.
    let chat = format!(
        "\u{feff}: heartbeat\r\nid: first\r\ndata: {}\r\n\r\n",
        chat_chunk()
    ) + "data: {\r\ndata: \"id\":\"c-1\",\"object\":\"chat.completion.chunk\",\"model\":\"route\",\"choices\":[],\"usage\":{\"prompt_tokens\":9,\"completion_tokens\":2,\"total_tokens\":11}}\r\n\r\ndata: [DONE]\r\n\r\n";
    let server = serve(vec![Reply::sse(&chat),Reply::sse("event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"signature_delta\",\"signature\":\"opaque\"}}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"),Reply::sse("event: response.failed\ndata: {\"type\":\"response.failed\",\"sequence_number\":3,\"response\":{\"id\":\"r\",\"object\":\"response\",\"model\":\"route\",\"status\":\"failed\",\"error\":{\"code\":\"upstream_error\",\"message\":\"failed\"}}}\n\n")]).await?;
    let client = GatewayClient::new(&server.url, None)?;
    let mut stream = client.stream_chat_completions(&chat_request(true)?).await?;
    let first = stream.next().await.ok_or("missing chat delta")??;
    assert_eq!(first.id, "first");
    assert_eq!(
        first.data.choices[0].delta.content,
        Some(Some(json!("你好")))
    );
    let usage = stream
        .next()
        .await
        .ok_or("missing usage")??
        .data
        .usage
        .flatten()
        .ok_or("missing usage payload")?;
    assert_eq!(usage.total_tokens, 11);
    assert!(stream.next().await.is_none());
    let mut stream = client.stream_messages(&messages_request(true)?).await?;
    assert_eq!(
        stream
            .next()
            .await
            .ok_or("missing message delta")??
            .data
            .delta
            .flatten()
            .ok_or("missing delta")?
            .signature
            .as_ref()
            .and_then(|value| value.as_deref()),
        Some("opaque")
    );
    assert_eq!(
        stream
            .next()
            .await
            .ok_or("missing message stop")??
            .data
            .kind,
        "message_stop"
    );
    assert!(stream.next().await.is_none());
    let mut stream = client.stream_responses(&responses_request(true)?).await?;
    let response = stream
        .next()
        .await
        .ok_or("missing response failure")??
        .data
        .response
        .flatten()
        .ok_or("missing response")?;
    assert_eq!(response.status, "failed");
    assert!(response.output.is_none());
    assert!(response.error.is_some());
    assert!(stream.next().await.is_none());
    Ok(())
}

#[tokio::test]
async fn incomplete_stream_is_an_error_and_is_never_replayed() -> TestResult {
    let server = serve(vec![Reply::sse(&format!("data: {}\n\n", chat_chunk()))]).await?;
    let client = GatewayClient::new(&server.url, None)?;
    let mut stream = client.stream_chat_completions(&chat_request(true)?).await?;
    assert!(stream.next().await.ok_or("missing chunk")?.is_ok());
    assert!(matches!(
        stream.next().await,
        Some(Err(Error::IncompleteStream))
    ));
    assert!(stream.next().await.is_none());
    assert_eq!(server.records.lock().await.len(), 1);
    Ok(())
}

#[tokio::test]
async fn streaming_errors_are_typed_and_diagnostics_do_not_echo_bodies() -> TestResult {
    let server = serve(vec![Reply::sse("event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"api_error\",\"message\":\"private-upstream-body\",\"code\":\"provider_failed\"}}\n\n")]).await?;
    let mut stream = GatewayClient::new(&server.url, None)?
        .stream_messages(&messages_request(true)?)
        .await?;
    let result = stream.next().await.ok_or("missing error")?;
    assert!(matches!(&result,Err(Error::Stream(failure)) if failure.body.is_some()));
    assert!(!format!("{result:?}").contains("private-upstream-body"));
    assert!(stream.next().await.is_none());
    Ok(())
}

#[tokio::test]
async fn redirects_do_not_forward_credentials_and_errors_retain_http_metadata() -> TestResult {
    let destination = serve(vec![]).await?;
    let origin = serve(vec![Reply {status:307,headers:vec![("location",format!("{}/collect",destination.url))],chunks:vec![]},Reply {status:429,headers:vec![("content-type","application/json".into()),("retry-after","7".into()),("www-authenticate","Bearer scope=inference".into())],chunks:vec![br#"{"error":{"message":"private-error","type":"rate_limit_error","code":"rate_limited"}}"#.to_vec()]}]).await?;
    let client = GatewayClient::new(&origin.url, Some("private-credential"))?;
    assert!(matches!(client.models().await,Err(Error::Service(failure)) if failure.status==307));
    assert!(destination.records.lock().await.is_empty());
    let error = client
        .models()
        .await
        .err()
        .ok_or("expected service failure")?;
    assert!(!format!("{error:?}").contains("private-error"));
    match error {
        Error::Service(failure) => {
            assert_eq!(failure.status, 429);
            assert_eq!(failure.headers["retry-after"], "7");
            assert!(failure.headers.contains_key("www-authenticate"));
            assert!(failure.body.is_some());
        }
        other => return Err(other.into()),
    }
    assert_eq!(origin.records.lock().await.len(), 2);
    Ok(())
}

#[tokio::test]
async fn response_and_unterminated_event_limits_are_enforced() -> TestResult {
    let server = serve(vec![
        Reply::json(json!({"object":"list","data":[],"extra":"x".repeat(400)})),
        Reply::sse(&format!("data: {}", "x".repeat(300))),
    ])
    .await?;
    let options = TransportOptions {
        max_response_bytes: 128,
        max_event_bytes: 64,
        ..Default::default()
    };
    let client = GatewayClient::with_options(&server.url, None, options)?;
    assert!(matches!(client.models().await, Err(Error::Limit)));
    let mut stream = client.stream_chat_completions(&chat_request(true)?).await?;
    assert!(matches!(stream.next().await, Some(Err(Error::Limit))));
    assert!(stream.next().await.is_none());
    Ok(())
}

#[tokio::test]
async fn terminal_event_stops_before_later_bytes_in_the_same_body() -> TestResult {
    let mut reply = Reply::sse("");
    reply.chunks = vec![format!("data: [DONE]\r\n\r\ndata: {}", "x".repeat(500)).into_bytes()];
    let server = serve(vec![reply]).await?;
    let client = GatewayClient::with_options(
        &server.url,
        None,
        TransportOptions {
            max_event_bytes: 64,
            ..Default::default()
        },
    )?;
    assert!(
        client
            .stream_chat_completions(&chat_request(true)?)
            .await?
            .next()
            .await
            .is_none()
    );
    Ok(())
}

#[tokio::test]
async fn invalid_options_and_wrong_stream_methods_fail_before_network() -> TestResult {
    for endpoint in [
        "file:///tmp/service",
        "https://user:private@host/",
        "https://host/?secret=private",
        "https://host/#private",
    ] {
        let error = GatewayClient::new(endpoint, None)
            .err()
            .ok_or("invalid endpoint accepted")?;
        assert!(!format!("{error:?}").contains("private"));
    }
    let server = serve(vec![]).await?;
    let client = GatewayClient::new(&server.url, None)?;
    assert!(matches!(
        client.chat_completions(&chat_request(true)?).await,
        Err(Error::Request(_))
    ));
    assert!(matches!(
        client.stream_messages(&messages_request(false)?).await,
        Err(Error::Request(_))
    ));
    assert!(server.records.lock().await.is_empty());
    Ok(())
}

#[tokio::test]
async fn mismatched_event_names_and_invalid_success_media_types_are_rejected() -> TestResult {
    let server = serve(vec![
        Reply::sse("event: message_stop\ndata: {\"type\":\"content_block_delta\"}\n\n"),
        Reply {
            status: 200,
            headers: vec![("content-type", "text/html".into())],
            chunks: vec![b"<html>login</html>".to_vec()],
        },
    ])
    .await?;
    let client = GatewayClient::new(&server.url, None)?;
    let mut stream = client.stream_messages(&messages_request(true)?).await?;
    assert!(matches!(stream.next().await, Some(Err(Error::Response(_)))));
    assert!(matches!(client.models().await, Err(Error::Response(_))));
    Ok(())
}

#[tokio::test]
async fn malformed_utf8_is_rejected_before_blank_lines_can_reset_the_bound() -> TestResult {
    let server = serve(vec![Reply {
        status: 200,
        headers: vec![("content-type", "text/event-stream".into())],
        chunks: vec![vec![0xff, b'\n', b'\n']; 100],
    }])
    .await?;
    let client = GatewayClient::with_options(
        &server.url,
        None,
        TransportOptions {
            max_event_bytes: 32,
            ..Default::default()
        },
    )?;
    let mut stream = client.stream_chat_completions(&chat_request(true)?).await?;
    assert!(matches!(
        stream.next().await,
        Some(Err(Error::Response("invalid SSE UTF-8")))
    ));
    assert!(stream.next().await.is_none());
    Ok(())
}

#[tokio::test]
async fn oversized_error_body_retains_status_and_retry_metadata() -> TestResult {
    let server = serve(vec![Reply {
        status: 429,
        headers: vec![("retry-after", "9".into())],
        chunks: vec![vec![b'x'; 200]],
    }])
    .await?;
    let client = GatewayClient::with_options(
        &server.url,
        None,
        TransportOptions {
            max_response_bytes: 64,
            ..Default::default()
        },
    )?;
    let error = client.models().await.err().ok_or("expected failure")?;
    match error {
        Error::Service(failure) => {
            assert_eq!(failure.status, 429);
            assert_eq!(failure.headers["retry-after"], "9");
            assert!(failure.body.is_none());
            assert!(matches!(failure.body_error, Some(Error::Limit)));
        }
        other => return Err(other.into()),
    }
    Ok(())
}

#[tokio::test]
async fn a_disconnected_post_is_sent_only_once() -> TestResult {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let url = format!("http://{}", listener.local_addr()?);
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await?;
        let mut received = Vec::new();
        let mut buffer = [0; 1024];
        loop {
            let count = socket.read(&mut buffer).await?;
            if count == 0 {
                break;
            }
            received.extend_from_slice(&buffer[..count]);
            if received.windows(4).any(|window| window == b"\r\n\r\n") {
                break;
            }
        }
        socket.shutdown().await?;
        drop(socket);
        let replay =
            tokio::time::timeout(std::time::Duration::from_millis(200), listener.accept()).await;
        Ok::<_, std::io::Error>((received, replay.is_ok()))
    });
    let client = GatewayClient::new(&url, None)?;
    let outcome = client.chat_completions(&chat_request(false)?).await;
    assert!(matches!(&outcome, Err(Error::Transport(_))), "{outcome:?}");
    let (request, replayed) = server.await??;
    assert!(request.starts_with(b"POST /v1/chat/completions "));
    assert!(!replayed, "ambiguous POST was replayed");
    Ok(())
}

#[tokio::test]
async fn a_truncated_error_body_preserves_the_known_http_failure() -> TestResult {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let url = format!("http://{}", listener.local_addr()?);
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await?;
        let mut buffer = [0; 1024];
        let _ = socket.read(&mut buffer).await?;
        socket.write_all(b"HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Bearer\r\nContent-Length: 1000\r\nConnection: close\r\n\r\n{\"err").await?;
        socket.shutdown().await
    });
    let error = GatewayClient::new(&url, None)?
        .models()
        .await
        .err()
        .ok_or("expected failure")?;
    server.await??;
    match error {
        Error::Service(failure) => {
            assert_eq!(failure.status, 401);
            assert_eq!(failure.headers["www-authenticate"], "Bearer");
            assert!(matches!(failure.body_error, Some(Error::Transport(_))));
        }
        other => return Err(other.into()),
    }
    Ok(())
}

#[tokio::test]
async fn evaluation_errors_keep_http_metadata_instead_of_an_empty_ledger() -> TestResult {
    for case in 0..3 {
        let payload = match case {
            0 => json!({"error":"private evaluation detail"}).to_string(),
            1 => "malformed private response".into(),
            _ => "x".repeat(2048),
        };
        let server = serve(vec![Reply {
            status: 404,
            headers: vec![
                ("www-authenticate", "Bearer".into()),
                ("retry-after", "3".into()),
            ],
            chunks: vec![payload.into_bytes()],
        }])
        .await?;
        let client = GatewayClient::with_options(
            &server.url,
            Some("service-key"),
            TransportOptions {
                max_response_bytes: 1024,
                ..Default::default()
            },
        )?;
        let error = client
            .eval_status()
            .await
            .err()
            .ok_or("missing evaluation HTTP error")?;
        assert!(!format!("{error:?}").contains("private"));
        let Error::Evaluation(failure) = error else {
            return Err("evaluation failure lost its HTTP envelope".into());
        };
        assert_eq!(failure.status, 404);
        assert_eq!(failure.headers["www-authenticate"], "Bearer");
        assert_eq!(failure.headers["retry-after"], "3");
        assert_eq!(failure.body.is_some(), case == 0);
        assert_eq!(matches!(failure.body_error, Some(Error::Limit)), case == 2);
        assert_eq!(server.records.lock().await.len(), 1);
    }
    Ok(())
}

#[tokio::test]
async fn evaluation_resource_ids_cannot_change_the_service_path_or_prefix() -> TestResult {
    let server = serve(vec![Reply {
        status: 404,
        headers: vec![],
        chunks: vec![br#"{"error":"not found"}"#.to_vec()],
    }])
    .await?;
    let client = GatewayClient::new(&format!("{}/proxy/v1", server.url), Some("service-key"))?;
    for id in ["", ".", ".."] {
        assert!(matches!(
            client.eval_subject(id).await,
            Err(Error::Request(_))
        ));
    }
    assert!(server.records.lock().await.is_empty());
    let result = client.eval_subject("eval/with ?#%").await;
    assert!(matches!(result, Err(Error::Evaluation(failure)) if failure.status == 404));
    let records = server.records.lock().await;
    assert_eq!(records.len(), 1);
    assert_eq!(
        records[0].path,
        "/proxy/v1/evals/subjects/eval%2Fwith%20%3F%23%25"
    );
    assert_eq!(records[0].credential.as_deref(), Some("Bearer service-key"));
    Ok(())
}

#[tokio::test]
async fn a_terminal_response_event_requires_its_response_status() -> TestResult {
    let server = serve(vec![Reply::sse(
        "event: response.completed\ndata: {\"type\":\"response.completed\"}\n\n",
    )])
    .await?;
    let mut stream = GatewayClient::new(&server.url, None)?
        .stream_responses(&responses_request(true)?)
        .await?;
    assert!(matches!(stream.next().await, Some(Err(Error::Response(_)))));
    assert!(stream.next().await.is_none());
    Ok(())
}

#[tokio::test]
async fn only_one_leading_bom_is_removed_and_ids_follow_sse_rules() -> TestResult {
    let body = format!(
        "\u{feff}\u{feff}data: not-json\r\rid: keep\rid: ignore\0this\revent: ignored-without-data\r\rdata: {}\r\rdata: [DONE]\r\r",
        chat_chunk()
    );
    let server = serve(vec![Reply::sse(&body)]).await?;
    let mut stream = GatewayClient::new(&server.url, None)?
        .stream_chat_completions(&chat_request(true)?)
        .await?;
    let event = stream.next().await.ok_or("missing chat event")??;
    assert_eq!(event.id, "keep");
    assert_eq!(event.event, "message");
    assert!(stream.next().await.is_none());
    Ok(())
}

#[tokio::test]
async fn nullable_native_data_and_diagnostics_remain_distinct() -> TestResult {
    use bitrouter_ai::gateway::GatewayErrorEnvelope;
    let response = json!({"id":"r","object":"response","model":"route","status":"completed","output":[{"type":"reasoning","id":null,"content":null,"summary":null,"encrypted_content":null,"status":null,"opaque":"private-replay-proof"}]});
    let messages = json!({"id":"m","type":"message","role":"assistant","model":"route","content":[{"type":"tool_use","id":"call","name":"tool","input":null,"signature":"private-replay-proof"}],"stop_reason":"tool_use","usage":{"input_tokens":2,"output_tokens":1}});
    let server=serve(vec![Reply::json(response.clone()),Reply::json(messages.clone()),Reply::sse(&format!("data: {}\n\ndata: [DONE]\n\n",json!({"id":"c","object":"chat.completion.chunk","model":"route","choices":[{"index":0,"delta":{"extra_content":{"signature":"private-replay-proof"}},"finish_reason":null}]})))]).await?;
    let client = GatewayClient::new(&server.url, None)?;
    let typed = client.responses(&responses_request(false)?).await?;
    assert!(!format!("{typed:?}").contains("private-replay-proof"));
    assert_eq!(serde_json::to_value(typed)?, response);
    let typed = client.messages(&messages_request(false)?).await?;
    assert!(!format!("{typed:?}").contains("private-replay-proof"));
    assert_eq!(serde_json::to_value(typed)?, messages);
    let event = client
        .stream_chat_completions(&chat_request(true)?)
        .await?
        .next()
        .await
        .ok_or("missing chat chunk")??;
    assert!(!format!("{event:?}").contains("private-replay-proof"));
    assert_eq!(
        serde_json::to_value(event.data)?["choices"][0]["delta"]["extra_content"]["signature"],
        "private-replay-proof"
    );
    let error = json!({"error":{"message":"private-replay-proof","type":null,"code":null}});
    let typed: GatewayErrorEnvelope = serde_json::from_value(error.clone())?;
    assert!(!format!("{typed:?}").contains("private-replay-proof"));
    assert_eq!(serde_json::to_value(typed)?, error);
    Ok(())
}
