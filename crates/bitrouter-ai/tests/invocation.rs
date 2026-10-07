//! AI-only HTTP fixtures: no router, SDK, server framework or ambient credentials.

use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use bitrouter_ai::client::{HttpTimeouts, ModelClient, ModelStream, parse_retry_after};
use bitrouter_ai::error::ModelError;
use bitrouter_ai::protocol::{
    InboundAdapter, OutboundDispatch, Transport, chat_completions::ChatCompletionsAdapter,
};
use bitrouter_ai::target::ModelTarget;
use bitrouter_ai::types::{ApiProtocol, AuthScheme, Content, FinishReason, Prompt, StreamPart};
use futures::StreamExt;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Notify, oneshot};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error + Send + Sync>>;
const DEADLINE: Duration = Duration::from_secs(5);

#[derive(Clone, Copy)]
enum Pause {
    Headers,
    Body,
}

struct CapturedRequest {
    headers: String,
    body: Value,
}

struct HeldAuthentication {
    started: Arc<Notify>,
    release: Arc<Notify>,
    completed: Arc<AtomicBool>,
}

#[async_trait::async_trait]
impl Transport for HeldAuthentication {
    fn protocol(&self) -> ApiProtocol {
        ApiProtocol::ChatCompletions
    }

    fn endpoint_url(&self, target: &ModelTarget, _stream: bool) -> String {
        format!("{}/chat/completions", target.api_base)
    }

    async fn authorise(
        &self,
        request: reqwest::Request,
        _target: &ModelTarget,
    ) -> bitrouter_ai::error::Result<reqwest::Request> {
        self.started.notify_one();
        self.release.notified().await;
        self.completed.store(true, Ordering::SeqCst);
        Ok(request)
    }
}

struct Fixture {
    base: String,
    received: oneshot::Receiver<CapturedRequest>,
    disconnected: oneshot::Receiver<()>,
    release: Option<oneshot::Sender<()>>,
    server: JoinHandle<TestResult>,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.server.abort();
    }
}

async fn read_request(socket: &mut TcpStream) -> TestResult<CapturedRequest> {
    let mut bytes = Vec::new();
    let (header_end, length) = loop {
        let mut buffer = [0_u8; 4096];
        let count = socket.read(&mut buffer).await?;
        if count == 0 || bytes.len() > 64 * 1024 {
            return Err(io::Error::other("missing fixture request headers").into());
        }
        bytes.extend_from_slice(&buffer[..count]);
        if let Some(end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            let headers = std::str::from_utf8(&bytes[..end])?;
            let length = headers
                .lines()
                .filter_map(|line| line.split_once(':'))
                .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                .ok_or_else(|| io::Error::other("missing fixture content-length"))?
                .1
                .trim()
                .parse::<usize>()?;
            break (end + 4, length);
        }
    };
    while bytes.len() < header_end + length {
        let mut buffer = [0_u8; 4096];
        let count = socket.read(&mut buffer).await?;
        if count == 0 {
            return Err(io::Error::other("truncated fixture request").into());
        }
        bytes.extend_from_slice(&buffer[..count]);
    }
    Ok(CapturedRequest {
        headers: String::from_utf8(bytes[..header_end].to_vec())?,
        body: serde_json::from_slice(&bytes[header_end..header_end + length])?,
    })
}

async fn write_chunk(socket: &mut TcpStream, data: &str) -> io::Result<()> {
    socket
        .write_all(format!("{:x}\r\n{data}\r\n", data.len()).as_bytes())
        .await
}

/// Paused fixtures hold the connection after the first body chunk. A channel
/// releases the tail, or a client disconnect is observed directly on TCP.
async fn fixture(
    status: u16,
    body: String,
    sse: bool,
    pause: Option<Pause>,
) -> TestResult<Fixture> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let base = format!("http://{}", listener.local_addr()?);
    let (received_tx, received) = oneshot::channel();
    let (disconnected_tx, disconnected) = oneshot::channel();
    let (release_tx, release_rx) = oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut socket, _) = tokio::time::timeout(DEADLINE, listener.accept()).await??;
        let request = tokio::time::timeout(DEADLINE, read_request(&mut socket)).await??;
        let content_type = if sse {
            "text/event-stream"
        } else {
            "application/json"
        };
        let headers = format!(
            "HTTP/1.1 {status} Fixture\r\nContent-Type: {content_type}\r\nTransfer-Encoding: chunked\r\nConnection: close\r\nRetry-After: 7\r\n\r\n"
        );
        if !matches!(pause, Some(Pause::Headers)) {
            socket.write_all(headers.as_bytes()).await?;
            write_chunk(&mut socket, &body).await?;
        }
        received_tx
            .send(request)
            .map_err(|_| io::Error::other("fixture request receiver dropped"))?;
        if pause.is_some() {
            let mut buffer = [0_u8; 1];
            tokio::select! {
                result = release_rx => result?,
                result = socket.read(&mut buffer) => {
                    match result {
                        Ok(0) => {},
                        Err(error) if error.kind() == io::ErrorKind::ConnectionReset => {},
                        other => return Err(io::Error::other(format!("expected disconnect, got {other:?}")).into()),
                    }
                    disconnected_tx.send(()).map_err(|_| io::Error::other("fixture disconnect receiver dropped"))?;
                    return Ok(());
                }
            }
        }
        if matches!(pause, Some(Pause::Headers)) {
            socket.write_all(headers.as_bytes()).await?;
            write_chunk(&mut socket, &body).await?;
        }
        socket.write_all(b"0\r\n\r\n").await?;
        socket.shutdown().await?;
        Ok(())
    });
    Ok(Fixture {
        base,
        received,
        disconnected,
        release: Some(release_tx),
        server,
    })
}

fn prompt() -> bitrouter_ai::error::Result<Prompt> {
    ChatCompletionsAdapter.parse_request(json!({
        "model": "source-model", "stream": false,
        "messages": [{"role": "user", "content": "hello"}]
    }))
}

fn target(base: &str, protocol: ApiProtocol) -> ModelTarget {
    ModelTarget {
        provider_name: "fixture".into(),
        service_id: "selected-model".into(),
        api_protocol: protocol,
        api_base: base.into(),
        api_key: "selected-secret".into(),
        credential_priority: Default::default(),
        account_label: None,
        auth_scheme: AuthScheme::XApiKey,
        compatibility: Default::default(),
    }
}

fn protocols() -> [ApiProtocol; 3] {
    [
        ApiProtocol::ChatCompletions,
        ApiProtocol::Responses,
        ApiProtocol::Messages,
    ]
}

#[test]
fn retry_after_accepts_seconds_http_date_and_rejects_invalid_values() -> TestResult {
    let seconds = reqwest::header::HeaderValue::from_static("42");
    assert_eq!(parse_retry_after(Some(&seconds)), Some(42));
    let future = std::time::SystemTime::now() + Duration::from_secs(120);
    let date = reqwest::header::HeaderValue::from_str(&httpdate::fmt_http_date(future))?;
    let delay = parse_retry_after(Some(&date))
        .ok_or_else(|| io::Error::other("missing Retry-After delay"))?;
    assert!((119..=120).contains(&delay), "parsed delay was {delay}");
    let past = reqwest::header::HeaderValue::from_str(&httpdate::fmt_http_date(
        std::time::SystemTime::now() - Duration::from_secs(120),
    ))?;
    assert_eq!(parse_retry_after(Some(&past)), Some(0));
    assert_eq!(
        parse_retry_after(Some(&reqwest::header::HeaderValue::from_static("soon-ish"))),
        None
    );
    assert_eq!(parse_retry_after(None), None);
    Ok(())
}

fn response(protocol: &ApiProtocol) -> Value {
    match protocol {
        ApiProtocol::ChatCompletions => {
            json!({"id":"chat_fixture","choices":[{"index":0,"message":{"role":"assistant","content":"hello"},"finish_reason":"stop"}],"usage":{"prompt_tokens":3,"completion_tokens":2}})
        }
        ApiProtocol::Responses => {
            json!({"id":"resp_fixture","status":"completed","output":[{"type":"message","id":"msg_fixture","role":"assistant","content":[{"type":"output_text","text":"hello"}]}],"usage":{"input_tokens":3,"output_tokens":2}})
        }
        ApiProtocol::Messages => {
            json!({"id":"msg_fixture","type":"message","role":"assistant","content":[{"type":"text","text":"hello"}],"stop_reason":"end_turn","usage":{"input_tokens":3,"output_tokens":2}})
        }

        ApiProtocol::Custom(_) | ApiProtocol::Decisions => Value::Null,
    }
}

fn event(body: Value) -> String {
    let name = body
        .get("type")
        .and_then(Value::as_str)
        .map(|name| format!("event: {name}\n"))
        .unwrap_or_default();
    format!("{name}data: {body}\n\n")
}

fn response_stream(protocol: &ApiProtocol) -> String {
    match protocol {
        ApiProtocol::ChatCompletions => {
            event(
                json!({"id":"chat_fixture","choices":[{"index":0,"delta":{"content":"hello"},"finish_reason":null}]}),
            ) + &event(
                json!({"id":"chat_fixture","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}),
            ) + &event(
                json!({"id":"chat_fixture","choices":[],"usage":{"prompt_tokens":3,"completion_tokens":2}}),
            ) + "data: [DONE]\n\n"
        }
        ApiProtocol::Responses => {
            event(
                json!({"type":"response.created","response":{"id":"resp_fixture","status":"in_progress"}}),
            ) + &event(
                json!({"type":"response.output_text.delta","delta":"hello","item_id":"msg_fixture","output_index":0,"content_index":0}),
            ) + &event(json!({"type":"response.completed","response":response(protocol)}))
        }
        ApiProtocol::Messages => {
            event(
                json!({"type":"message_start","message":{"id":"msg_fixture","usage":{"input_tokens":3,"output_tokens":0}}}),
            ) + &event(
                json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
            ) + &event(
                json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hello"}}),
            ) + &event(json!({"type":"content_block_stop","index":0}))
                + &event(
                    json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":2}}),
                )
                + &event(json!({"type":"message_stop"}))
        }

        ApiProtocol::Custom(_) | ApiProtocol::Decisions => String::new(),
    }
}

async fn collect(stream: ModelStream) -> TestResult<Vec<StreamPart>> {
    tokio::time::timeout(DEADLINE, stream.collect::<Vec<_>>())
        .await?
        .into_iter()
        .collect::<Result<Vec<_>, _>>()
        .map_err(Into::into)
}

#[tokio::test]
async fn codex_generate_uses_sse_without_changing_the_source_prompt() -> TestResult {
    let client = ModelClient::new(HttpTimeouts::default())?;
    let mut server = fixture(200, response_stream(&ApiProtocol::Responses), true, None).await?;
    let mut selected = target(&server.base, ApiProtocol::Responses);
    selected.provider_name = "openai-codex".into();
    let source = prompt()?;
    let original = source.clone();
    let result = client
        .generate(&selected, &source, &CancellationToken::new())
        .await?;
    let request = tokio::time::timeout(DEADLINE, &mut server.received).await??;
    assert_eq!(request.body["stream"], true);
    assert_eq!(source, original);
    assert_eq!(result.response_id.as_deref(), Some("resp_fixture"));
    assert_eq!(result.finish_reason, Some(FinishReason::Stop));
    assert!(matches!(result.content.as_slice(), [Content::Text { text, .. }] if text == "hello"));
    let usage = result.usage.ok_or("missing provider usage")?;
    assert_eq!(usage.prompt_tokens, 3);
    assert_eq!(usage.completion_tokens, 2);
    tokio::time::timeout(DEADLINE, &mut server.server).await???;
    Ok(())
}

#[tokio::test]
async fn codex_generate_rejects_truncated_failed_and_post_terminal_streams() -> TestResult {
    let client = ModelClient::new(HttpTimeouts::default())?;
    let delta = event(json!({"type":"response.output_text.delta","delta":"partial"}));
    let terminal = event(
        json!({"type":"response.completed","response":{"id":"response","status":"completed","output":[]}}),
    );
    for (body, provider_error) in [
        (delta.clone(), false),
        (
            delta.clone()
                + &event(
                    json!({"type":"response.failed","response":{"error":{"type":"rate_limit_error","message":"late selected-secret failure"}}}),
                ),
            true,
        ),
        (delta.clone() + &terminal + &delta, false),
    ] {
        let mut server = fixture(200, body, true, None).await?;
        let mut selected = target(&server.base, ApiProtocol::Responses);
        selected.provider_name = "openai-codex".into();
        let result = client
            .generate(&selected, &prompt()?, &CancellationToken::new())
            .await;
        if provider_error {
            assert!(
                matches!(result, Err(ModelError::Provider { status: 429, ref message }) if !message.contains("selected-secret"))
            );
        } else {
            assert!(matches!(result, Err(ModelError::InvalidResponse { .. })));
        }
        tokio::time::timeout(DEADLINE, &mut server.server).await???;
    }
    Ok(())
}

#[tokio::test]
async fn codex_generate_preserves_native_blocks_and_custom_tool_input() -> TestResult {
    let events = vec![
        json!({"type":"response.created","response":{"id":"native-result"}}),
        json!({"type":"response.output_item.added","item":{"id":"reasoning","type":"reasoning"}}),
        json!({"type":"response.reasoning_summary_text.delta","delta":"reason"}),
        json!({"type":"response.output_item.done","item":{"id":"reasoning","type":"reasoning"}}),
        json!({"type":"response.output_item.added","item":{"id":"first-text","type":"message"}}),
        json!({"type":"response.output_text.delta","delta":"first"}),
        json!({"type":"response.output_item.done","item":{"id":"first-text","type":"message"}}),
        json!({"type":"response.output_item.added","item":{"id":"native-item","type":"custom_tool_call","call_id":"native-call","name":"apply_patch","namespace":"tools"}}),
        json!({"type":"response.custom_tool_call_input.delta","item_id":"native-item","delta":"raw "}),
        json!({"type":"response.custom_tool_call_input.delta","item_id":"native-item","delta":"patch"}),
        json!({"type":"response.custom_tool_call_input.done","item_id":"native-item","input":"raw patch"}),
        json!({"type":"response.output_item.done","item":{"id":"native-item","type":"custom_tool_call"}}),
        json!({"type":"response.output_item.added","item":{"id":"last-text","type":"message"}}),
        json!({"type":"response.output_text.delta","delta":"last"}),
        json!({"type":"response.output_item.done","item":{"id":"last-text","type":"message"}}),
        json!({"type":"response.incomplete","response":{"id":"native-result","status":"incomplete","output":[],"usage":{"input_tokens":12,"output_tokens":4,"input_tokens_details":{"cached_tokens":5},"output_tokens_details":{"reasoning_tokens":1}}}}),
    ];
    let body = events.into_iter().map(event).collect::<String>();
    let mut server = fixture(200, body, true, None).await?;
    let mut selected = target(&server.base, ApiProtocol::Responses);
    selected.provider_name = "openai-codex".into();
    let client = ModelClient::new(HttpTimeouts::default())?;
    let result = client
        .generate(&selected, &prompt()?, &CancellationToken::new())
        .await?;
    assert_eq!(result.content.len(), 4);
    assert!(matches!(&result.content[0], Content::Reasoning { text, .. } if text == "reason"));
    assert!(matches!(&result.content[1], Content::Text { text, .. } if text == "first"));
    assert!(
        matches!(&result.content[2], Content::ToolCall { id, name, arguments, provider_metadata, .. } if id == "native-call" && name == "apply_patch" && arguments == "raw patch" && provider_metadata["openai"]["type"] == "custom_tool_call" && provider_metadata["openai"]["namespace"] == "tools")
    );
    assert!(matches!(&result.content[3], Content::Text { text, .. } if text == "last"));
    assert_eq!(result.finish_reason, Some(FinishReason::Length));
    let usage = result.usage.ok_or("missing native usage")?;
    assert_eq!(usage.prompt_tokens, 12);
    assert_eq!(usage.completion_tokens, 4);
    assert_eq!(usage.cache_read_tokens, 5);
    assert_eq!(usage.reasoning_tokens, 1);
    tokio::time::timeout(DEADLINE, &mut server.server).await???;
    Ok(())
}

#[tokio::test]
async fn codex_generate_cancellation_and_drop_release_the_upstream_connection() -> TestResult {
    let client = ModelClient::new(HttpTimeouts::default())?;
    for cancel in [true, false] {
        let mut server = fixture(
            200,
            event(json!({"type":"response.output_text.delta","delta":"partial"})),
            true,
            Some(Pause::Body),
        )
        .await?;
        let mut selected = target(&server.base, ApiProtocol::Responses);
        selected.provider_name = "openai-codex".into();
        let source = prompt()?;
        let cancellation = CancellationToken::new();
        let mut call = Box::pin(client.generate(&selected, &source, &cancellation));
        tokio::select! {
            result = &mut call => return Err(io::Error::other(format!("Codex call ended before cancellation/drop: {result:?}")).into()),
            request = tokio::time::timeout(DEADLINE, &mut server.received) => { assert_eq!(request??.body["stream"], true); }
        }
        if cancel {
            cancellation.cancel();
            assert!(matches!(
                tokio::time::timeout(DEADLINE, &mut call).await?,
                Err(ModelError::Cancelled)
            ));
        }
        drop(call);
        tokio::time::timeout(DEADLINE, &mut server.disconnected).await??;
        tokio::time::timeout(DEADLINE, &mut server.server).await???;
    }
    Ok(())
}

#[tokio::test]
async fn http_failures_keep_status_and_retry_hint_and_scrub_credentials() -> TestResult {
    let client = ModelClient::new(HttpTimeouts::default())?;
    for (status, streaming) in [(401, false), (429, true)] {
        let body = json!({"error":{"message":"rejected Bearer selected-secret","selected-secret":"selected-secret"}});
        let mut server = fixture(status, body.to_string(), false, None).await?;
        let selected = target(&server.base, ApiProtocol::ChatCompletions);
        let source = prompt()?;
        let cancellation = CancellationToken::new();
        let error = if streaming {
            client.stream(&selected, &source, &cancellation).await.err()
        } else {
            client
                .generate(&selected, &source, &cancellation)
                .await
                .err()
        }
        .ok_or_else(|| io::Error::other("provider failure was accepted"))?;
        assert!(
            matches!(&error, ModelError::HttpResponse { status: actual, retry_after: Some(7), body } if *actual == status && !body.contains("selected-secret") && body.contains("[redacted credential]")),
            "{error}"
        );
        // The server handles one request. Any implicit refresh/retry changes
        // this HTTP error into a transport failure and fails the assertion.
        tokio::time::timeout(DEADLINE, &mut server.server).await???;
    }
    Ok(())
}

#[tokio::test]
async fn invalid_success_and_responses_terminal_contract_are_typed() -> TestResult {
    let client = ModelClient::new(HttpTimeouts::default())?;
    for (protocol, body, decode) in [
        (ApiProtocol::ChatCompletions, "not json".into(), true),
        (ApiProtocol::Messages, "{}".into(), false),
        (
            ApiProtocol::Responses,
            json!({"id":"resp_fixture","status":"failed","output":[]}).to_string(),
            false,
        ),
        (
            ApiProtocol::Responses,
            json!({"status":"completed","output":[]}).to_string(),
            false,
        ),
    ] {
        let server = fixture(200, body, false, None).await?;
        let error = client
            .generate(
                &target(&server.base, protocol),
                &prompt()?,
                &CancellationToken::new(),
            )
            .await
            .err()
            .ok_or_else(|| io::Error::other("invalid success was accepted"))?;
        assert!(
            if decode {
                matches!(error, ModelError::Decode { .. })
            } else {
                matches!(error, ModelError::InvalidResponse { .. })
            },
            "{error}"
        );
    }
    Ok(())
}

#[tokio::test]
async fn truncated_and_late_error_streams_preserve_text_and_usage() -> TestResult {
    let client = ModelClient::new(HttpTimeouts::default())?;
    let prefix =
        event(json!({"choices":[{"index":0,"delta":{"content":"partial"},"finish_reason":null}]}))
            + &event(json!({"choices":[],"usage":{"prompt_tokens":3,"completion_tokens":2}}));
    for tail in [
        String::new(),
        event(json!({"error":{"status":401,"message":"selected-secret rejected"}})),
    ] {
        let server = fixture(200, prefix.clone() + &tail, true, None).await?;
        let stream = client
            .stream(
                &target(&server.base, ApiProtocol::ChatCompletions),
                &prompt()?,
                &CancellationToken::new(),
            )
            .await?;
        let parts = tokio::time::timeout(DEADLINE, stream.collect::<Vec<_>>()).await?;
        assert!(
            parts.iter().any(
                |part| matches!(part, Ok(StreamPart::TextDelta { text }) if text == "partial")
            )
        );
        assert!(parts.iter().any(|part| matches!(part, Ok(StreamPart::Usage { usage }) if usage.prompt_tokens == 3 && usage.completion_tokens == 2)));
        assert!(
            !parts
                .iter()
                .any(|part| part.as_ref().is_ok_and(StreamPart::is_terminal))
        );
        let error = parts
            .last()
            .ok_or_else(|| io::Error::other("missing terminal error"))?;
        if tail.is_empty() {
            assert!(matches!(error, Err(ModelError::InvalidResponse { .. })));
        } else {
            assert!(
                matches!(error, Err(ModelError::Provider { status: 401, message }) if !message.contains("selected-secret")),
                "{error:?}"
            );
        }
    }
    Ok(())
}

#[tokio::test]
async fn responses_stream_rejects_mismatched_id_and_post_terminal_data() -> TestResult {
    let client = ModelClient::new(HttpTimeouts::default())?;
    let created = event(json!({"type":"response.created","response":{"id":"resp_fixture"}}));
    let terminal =
        event(json!({"type":"response.completed","response":response(&ApiProtocol::Responses)}));
    let wrong = event(
        json!({"type":"response.completed","response":{"id":"resp_other","status":"completed","output":[]}}),
    );
    for body in [
        created.clone() + &wrong,
        created + &terminal + &event(json!({"type":"response.output_text.delta","delta":"late"})),
    ] {
        let server = fixture(200, body, true, None).await?;
        let stream = client
            .stream(
                &target(&server.base, ApiProtocol::Responses),
                &prompt()?,
                &CancellationToken::new(),
            )
            .await?;
        let parts = tokio::time::timeout(DEADLINE, stream.collect::<Vec<_>>()).await?;
        assert!(matches!(
            parts.last(),
            Some(Err(ModelError::InvalidResponse { .. }))
        ));
        assert!(
            !parts
                .iter()
                .any(|part| part.as_ref().is_ok_and(StreamPart::is_terminal))
        );
    }
    Ok(())
}

#[tokio::test]
async fn cancellation_and_missing_credentials_prevent_dispatch() -> TestResult {
    let client = ModelClient::new(HttpTimeouts::default())?;
    let mut server = fixture(200, "{}".into(), false, None).await?;
    let mut selected = target(&server.base, ApiProtocol::ChatCompletions);
    let cancelled = CancellationToken::new();
    cancelled.cancel();
    assert!(matches!(
        client.generate(&selected, &prompt()?, &cancelled).await,
        Err(ModelError::Cancelled)
    ));
    selected.api_key.clear();
    assert!(matches!(
        client
            .generate(&selected, &prompt()?, &CancellationToken::new())
            .await,
        Err(ModelError::InvalidCredential { .. })
    ));
    assert!(matches!(
        server.received.try_recv(),
        Err(oneshot::error::TryRecvError::Empty)
    ));
    Ok(())
}

#[tokio::test]
async fn token_cancellation_waits_for_custom_auth_then_prevents_dispatch() -> TestResult {
    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let completed = Arc::new(AtomicBool::new(false));
    let mut dispatch = OutboundDispatch::builtin();
    dispatch.register(
        Arc::new(ChatCompletionsAdapter),
        Arc::new(HeldAuthentication {
            started: Arc::clone(&started),
            release: Arc::clone(&release),
            completed: Arc::clone(&completed),
        }),
    );
    let client = ModelClient::with_dispatch(HttpTimeouts::default(), Arc::new(dispatch))?;
    let mut server = fixture(200, "{}".into(), false, None).await?;
    let selected = target(&server.base, ApiProtocol::ChatCompletions);
    let source = prompt()?;
    let cancellation = CancellationToken::new();
    let call = client.generate(&selected, &source, &cancellation);
    tokio::pin!(call);
    tokio::select! {
        result = &mut call => return Err(io::Error::other(format!("auth ended before release: {result:?}")).into()),
        result = tokio::time::timeout(DEADLINE, started.notified()) => { result?; },
    }
    cancellation.cancel();
    assert!(
        tokio::time::timeout(Duration::from_millis(25), &mut call)
            .await
            .is_err()
    );
    assert!(!completed.load(Ordering::SeqCst));
    release.notify_one();
    assert!(matches!(
        tokio::time::timeout(DEADLINE, &mut call).await?,
        Err(ModelError::Cancelled)
    ));
    assert!(completed.load(Ordering::SeqCst));
    assert!(matches!(
        server.received.try_recv(),
        Err(oneshot::error::TryRecvError::Empty)
    ));
    Ok(())
}

#[tokio::test]
async fn cancellation_stops_pending_body_and_releases_connection() -> TestResult {
    let client = ModelClient::new(HttpTimeouts::default())?;
    let mut server = fixture(200, "{".into(), false, Some(Pause::Body)).await?;
    let selected = target(&server.base, ApiProtocol::ChatCompletions);
    let source = prompt()?;
    let cancellation = CancellationToken::new();
    let call = client.generate(&selected, &source, &cancellation);
    tokio::pin!(call);
    tokio::select! {
        result = &mut call => return Err(io::Error::other(format!("body call ended before cancellation: {result:?}")).into()),
        request = tokio::time::timeout(DEADLINE, &mut server.received) => { request??; }
    }
    cancellation.cancel();
    assert!(matches!(
        tokio::time::timeout(DEADLINE, &mut call).await?,
        Err(ModelError::Cancelled)
    ));
    tokio::time::timeout(DEADLINE, &mut server.disconnected).await??;
    tokio::time::timeout(DEADLINE, &mut server.server).await???;
    Ok(())
}

#[tokio::test]
async fn cancellation_and_idle_timeout_work_before_response_headers() -> TestResult {
    for cancel in [true, false] {
        let client = ModelClient::new(HttpTimeouts {
            read: Duration::from_millis(250),
            ..Default::default()
        })?;
        let mut server = fixture(200, "{}".into(), false, Some(Pause::Headers)).await?;
        let selected = target(&server.base, ApiProtocol::ChatCompletions);
        let source = prompt()?;
        let cancellation = CancellationToken::new();
        let call = client.generate(&selected, &source, &cancellation);
        tokio::pin!(call);
        tokio::select! {
            result = &mut call => return Err(io::Error::other(format!("call ended before fixture accepted request: {result:?}")).into()),
            request = tokio::time::timeout(DEADLINE, &mut server.received) => { request??; }
        }
        if cancel {
            cancellation.cancel();
        }
        let result = tokio::time::timeout(DEADLINE, &mut call).await?;
        assert!(
            if cancel {
                matches!(result, Err(ModelError::Cancelled))
            } else {
                matches!(result, Err(ModelError::Timeout))
            },
            "{result:?}"
        );
        tokio::time::timeout(DEADLINE, &mut server.disconnected).await??;
        tokio::time::timeout(DEADLINE, &mut server.server).await???;
    }
    Ok(())
}

#[tokio::test]
async fn stream_cancellation_and_drop_release_connection() -> TestResult {
    let client = ModelClient::new(HttpTimeouts::default())?;
    let prefix = event(json!({"choices":[{"delta":{"content":"partial"},"finish_reason":null}]}));
    for cancel in [true, false] {
        let mut server = fixture(200, prefix.clone(), true, Some(Pause::Body)).await?;
        let cancellation = CancellationToken::new();
        let mut stream = client
            .stream(
                &target(&server.base, ApiProtocol::ChatCompletions),
                &prompt()?,
                &cancellation,
            )
            .await?;
        assert!(
            matches!(tokio::time::timeout(DEADLINE, stream.next()).await?, Some(Ok(StreamPart::TextDelta { text })) if text == "partial")
        );
        if cancel {
            cancellation.cancel();
            assert!(matches!(
                tokio::time::timeout(DEADLINE, stream.next()).await?,
                Some(Err(ModelError::Cancelled))
            ));
            assert!(stream.next().await.is_none());
        }
        drop(stream);
        tokio::time::timeout(DEADLINE, &mut server.disconnected).await??;
        tokio::time::timeout(DEADLINE, &mut server.server).await???;
    }
    Ok(())
}

#[tokio::test]
async fn read_timeout_and_total_deadline_are_typed_for_body_and_stream() -> TestResult {
    for total in [false, true] {
        let timeouts = HttpTimeouts {
            read: if total {
                DEADLINE
            } else {
                Duration::from_millis(250)
            },
            total: total.then_some(Duration::from_millis(250)),
            ..Default::default()
        };
        let client = ModelClient::new(timeouts)?;
        for streaming in [false, true] {
            let body = if streaming {
                event(json!({"choices":[{"delta":{"content":"partial"},"finish_reason":null}]}))
            } else {
                "{".into()
            };
            let mut server = fixture(200, body, streaming, Some(Pause::Body)).await?;
            let selected = target(&server.base, ApiProtocol::ChatCompletions);
            let source = prompt()?;
            let cancellation = CancellationToken::new();
            if streaming {
                let stream = client.stream(&selected, &source, &cancellation).await?;
                let parts = tokio::time::timeout(DEADLINE, stream.collect::<Vec<_>>()).await?;
                assert!(matches!(
                    parts.first(),
                    Some(Ok(StreamPart::TextDelta { .. }))
                ));
                assert!(
                    matches!(parts.last(), Some(Err(ModelError::Timeout))),
                    "{parts:?}"
                );
            } else {
                assert!(matches!(
                    tokio::time::timeout(
                        DEADLINE,
                        client.generate(&selected, &source, &cancellation)
                    )
                    .await?,
                    Err(ModelError::Timeout)
                ));
            }
            tokio::time::timeout(DEADLINE, &mut server.disconnected).await??;
            tokio::time::timeout(DEADLINE, &mut server.server).await???;
        }
    }
    Ok(())
}

#[tokio::test]
async fn caller_can_complete_held_stream_without_total_cap() -> TestResult {
    // Explicitly releasing a held body verifies the no-total-cap path. The
    // preceding test proves the independently configured overall cap.
    let client = ModelClient::new(HttpTimeouts {
        total: None,
        ..Default::default()
    })?;
    let mut server = fixture(
        200,
        response_stream(&ApiProtocol::ChatCompletions),
        true,
        Some(Pause::Body),
    )
    .await?;
    let stream = client
        .stream(
            &target(&server.base, ApiProtocol::ChatCompletions),
            &prompt()?,
            &CancellationToken::new(),
        )
        .await?;
    let release = server
        .release
        .take()
        .ok_or_else(|| io::Error::other("missing fixture release"))?;
    release
        .send(())
        .map_err(|_| io::Error::other("fixture release receiver dropped"))?;
    assert!(collect(stream).await?.iter().any(StreamPart::is_terminal));
    tokio::time::timeout(DEADLINE, &mut server.server).await???;
    Ok(())
}

#[derive(Default)]
struct RetryAuth {
    refreshes: std::sync::atomic::AtomicUsize,
    preparations: std::sync::atomic::AtomicUsize,
}

#[async_trait::async_trait]
impl bitrouter_ai::auth::AuthApplier for RetryAuth {
    async fn apply(
        &self,
        mut request: reqwest::Request,
        target: &ModelTarget,
    ) -> bitrouter_ai::error::Result<reqwest::Request> {
        assert_eq!(target.account_label.as_deref(), Some("selected-account"));
        let token = if self.refreshes.load(Ordering::SeqCst) == 0 {
            "Bearer old-dynamic-secret"
        } else {
            "Bearer new-dynamic-secret"
        };
        request.headers_mut().insert(
            reqwest::header::AUTHORIZATION,
            reqwest::header::HeaderValue::from_static(token),
        );
        Ok(request)
    }
    async fn prepare_body(
        &self,
        body: &mut Value,
        _target: &ModelTarget,
    ) -> bitrouter_ai::error::Result<()> {
        self.preparations.fetch_add(1, Ordering::SeqCst);
        body["provider_field"] = json!("prepared");
        Ok(())
    }
    async fn refresh_after_unauthorized(
        &self,
        _target: &ModelTarget,
        rejected: Option<&reqwest::header::HeaderValue>,
    ) -> bitrouter_ai::error::Result<bool> {
        assert_eq!(
            rejected.and_then(|value| value.to_str().ok()),
            Some("Bearer old-dynamic-secret")
        );
        self.refreshes.fetch_add(1, Ordering::SeqCst);
        Ok(true)
    }
}

#[tokio::test]
async fn registered_auth_shapes_and_rebuilds_once_after_401_in_both_modes() -> TestResult {
    for streaming in [false, true] {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let base = format!("http://{}", listener.local_addr()?);
        let server = tokio::spawn(async move {
            let mut requests = Vec::new();
            for _ in 0..2 {
                let (mut socket, _) = tokio::time::timeout(DEADLINE, listener.accept()).await??;
                requests.push(read_request(&mut socket).await?);
                let body = "old-dynamic-secret new-dynamic-secret rejected";
                socket.write_all(format!("HTTP/1.1 401 Unauthorized\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await?;
                socket.shutdown().await?;
            }
            Ok::<_, Box<dyn std::error::Error + Send + Sync>>(requests)
        });
        let auth = Arc::new(RetryAuth::default());
        let client = ModelClient::new(HttpTimeouts::default())?.with_auth_appliers(
            bitrouter_ai::auth::AuthAppliers::new().with("fixture", auth.clone()),
        );
        let mut selected = target(&base, ApiProtocol::ChatCompletions);
        selected.api_key.clear();
        selected.account_label = Some("selected-account".into());
        let source = prompt()?;
        let before = serde_json::to_value(&source)?;
        let error = if streaming {
            client
                .stream(&selected, &source, &CancellationToken::new())
                .await
                .err()
        } else {
            client
                .generate(&selected, &source, &CancellationToken::new())
                .await
                .err()
        }
        .ok_or_else(|| io::Error::other("second 401 was reported as success"))?;
        assert!(matches!(
            error,
            ModelError::HttpResponse { status: 401, .. }
        ));
        assert!(!format!("{error:?}\n{error}").contains("dynamic-secret"));
        assert_eq!(auth.refreshes.load(Ordering::SeqCst), 1);
        assert_eq!(auth.preparations.load(Ordering::SeqCst), 2);
        let requests = tokio::time::timeout(DEADLINE, server).await???;
        assert_eq!(requests.len(), 2);
        assert!(requests[0].headers.contains("Bearer old-dynamic-secret"));
        assert!(requests[1].headers.contains("Bearer new-dynamic-secret"));
        assert!(
            requests
                .iter()
                .all(|request| request.body["provider_field"] == "prepared")
        );
        assert_eq!(serde_json::to_value(&source)?, before);
    }
    Ok(())
}

#[tokio::test]
async fn three_protocols_invoke_without_sdk_and_keep_source_prompt() -> TestResult {
    let client = ModelClient::new(HttpTimeouts::default())?;
    let source = prompt()?;
    let original = source.clone();
    for protocol in protocols() {
        for stream in [false, true] {
            let body = if stream {
                response_stream(&protocol)
            } else {
                response(&protocol).to_string()
            };
            let mut server = fixture(200, body, stream, None).await?;
            let selected = target(&server.base, protocol.clone());
            let cancellation = CancellationToken::new();
            if stream {
                let parts =
                    collect(client.stream(&selected, &source, &cancellation).await?).await?;
                assert_eq!(
                    parts
                        .iter()
                        .filter_map(|part| match part {
                            StreamPart::TextDelta { text } => Some(text.as_str()),
                            _ => None,
                        })
                        .collect::<String>(),
                    "hello"
                );
                assert!(parts.iter().any(StreamPart::is_terminal), "{protocol}");
                let usage = parts
                    .iter()
                    .find_map(|part| match part {
                        StreamPart::Usage { usage }
                        | StreamPart::ResponseCompleted {
                            usage: Some(usage), ..
                        } => Some(usage),
                        _ => None,
                    })
                    .ok_or_else(|| io::Error::other("missing stream usage"))?;
                assert_eq!((usage.prompt_tokens, usage.completion_tokens), (3, 2));
            } else {
                let result = client.generate(&selected, &source, &cancellation).await?;
                assert!(
                    matches!(result.content.as_slice(), [Content::Text { text, .. }] if text == "hello")
                );
                assert_eq!(result.finish_reason, Some(FinishReason::Stop));
                let usage = result
                    .usage
                    .ok_or_else(|| io::Error::other("missing usage"))?;
                assert_eq!((usage.prompt_tokens, usage.completion_tokens), (3, 2));
            }
            let request = tokio::time::timeout(DEADLINE, &mut server.received).await??;
            let headers = request.headers.to_ascii_lowercase();
            match protocol {
                ApiProtocol::ChatCompletions => assert!(
                    headers.starts_with("post /chat/completions ")
                        && headers.contains("authorization: bearer selected-secret")
                ),
                ApiProtocol::Responses => assert!(
                    headers.starts_with("post /responses ")
                        && headers.contains("authorization: bearer selected-secret")
                ),
                ApiProtocol::Messages => assert!(
                    headers.starts_with("post /messages ")
                        && headers.contains("x-api-key: selected-secret")
                        && !headers.contains("authorization:")
                ),

                ApiProtocol::Custom(_) | ApiProtocol::Decisions => {}
            }
            assert_eq!(request.body["model"], "selected-model");
            assert_eq!(request.body["stream"], stream);
            assert_eq!(source, original);
            tokio::time::timeout(DEADLINE, &mut server.server).await???;
        }
    }
    Ok(())
}
