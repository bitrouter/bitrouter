//! Tenant dispatch against real HTTP/1.1 and prior-knowledge HTTP/2 sockets.

use std::{error::Error as StdError, time::Duration};

use axum::{
    body::Body as HttpBody,
    http::{HeaderValue, Version},
    response::Response,
};
use bitrouter_ai::{
    decisions::DecisionRequest,
    protocol::{
        chat_completions::ChatRequest, messages::MessagesRequest, responses::ResponsesRequest,
    },
    stream::{ProtocolEnd, StreamItem},
    tenancy::operation::{
        DispatchBinding, DispatchState, Error as TenancyError, ErrorCode, TenantCapabilities,
    },
};
use bitrouter_sdk::gateway::{
    GatewayClient, HttpVersion, TransportOptions,
    tenancy::{Body, Error, Failure, Problem},
};
use futures_util::StreamExt;
use serde_json::{Value, json};

// Reuse the existing socket fixture without changing its protocol selection.
#[path = "transport_protocol/server.rs"]
mod server;

type TestResult<T = ()> = Result<T, Box<dyn StdError + Send + Sync>>;
const BUDGET: Duration = Duration::from_secs(5);

fn versions() -> Vec<(HttpVersion, Version)> {
    vec![
        (HttpVersion::Http1Only, Version::HTTP_11),
        #[cfg(feature = "http2")]
        (HttpVersion::Http2PriorKnowledge, Version::HTTP_2),
    ]
}

fn binding(correlation: &str) -> TestResult<DispatchBinding> {
    let capabilities = TenantCapabilities {
        contract_version: 1,
        installation_id: "installation".try_into()?,
        instance_id: "instance".try_into()?,
        capability_revision: "capability-1".try_into()?,
        revision_bound_dispatch: true,
        strict_receipts: true,
        dispatch_enabled: true,
    };
    Ok(capabilities.bind("execution-revision-1".try_into()?, correlation.try_into()?)?)
}

fn client(server: &server::Server, version: HttpVersion) -> TestResult<GatewayClient> {
    Ok(GatewayClient::with_options(
        &format!("{}/prefix/v1", server.endpoint),
        Some("default-token"),
        TransportOptions {
            http_version: version,
            connect_timeout: BUDGET,
            read_timeout: Some(BUDGET),
            request_timeout: Some(BUDGET),
            ..Default::default()
        },
    )?)
}

fn chat(stream: bool) -> TestResult<ChatRequest> {
    Ok(serde_json::from_value(
        json!({"model":"route","messages":[],"stream":stream}),
    )?)
}
fn messages(stream: bool) -> TestResult<MessagesRequest> {
    Ok(serde_json::from_value(
        json!({"model":"route","messages":[],"max_tokens":20,"stream":stream}),
    )?)
}
fn responses(stream: bool) -> TestResult<ResponsesRequest> {
    Ok(serde_json::from_value(
        json!({"model":"route","input":"hi","stream":stream}),
    )?)
}
fn chat_json() -> Value {
    json!({"id":"provider-id","object":"chat.completion","model":"route","choices":[],"extension":null})
}
fn json_reply(status: u16, id: Option<&str>, value: Value) -> TestResult<Response> {
    let mut response = Response::builder()
        .status(status)
        .header("content-type", "application/json");
    if let Some(id) = id {
        response = response.header("x-bitrouter-request-id", id);
    }
    Ok(response.body(HttpBody::from(value.to_string()))?)
}
fn sse(value: &str) -> TestResult<Response> {
    // Deliberately fragment framing and UTF-8 across transport body chunks.
    let chunks: Vec<_> = value
        .as_bytes()
        .iter()
        .map(|b| Ok::<_, std::io::Error>(bytes::Bytes::from(vec![*b])))
        .collect();
    Ok(Response::builder()
        .header("content-type", "text/event-stream; charset=utf-8")
        .header("x-bitrouter-request-id", "execution-1")
        .body(HttpBody::from_stream(futures_util::stream::iter(chunks)))?)
}
fn tenancy_error(id: Option<&str>, state: &str) -> Value {
    json!({"code":"capability_mismatch","message":"redacted explanation","dispatch_state":state,"operation_id":null,"execution_id":id})
}
fn failure<T>(result: Result<T, Error>) -> TestResult<Failure> {
    match result {
        Err(Error::Response(failure)) => Ok(*failure),
        _ => Err("expected typed response failure".into()),
    }
}

#[tokio::test]
async fn pooled_concurrent_credentials_have_exact_bindings_and_do_not_mutate_defaults() -> TestResult
{
    for (version, wire) in versions() {
        let mut replies = vec![Some(server::json(json!({"status":"ok"}))?)];
        for _ in 0..13 {
            replies.push(Some(json_reply(200, Some("execution-1"), chat_json())?));
        }
        replies.push(Some(server::json(json!({"status":"ok"}))?));
        let server = server::serve(wire, replies).await?;
        let client = client(&server, version)?;
        assert_eq!(client.health().await?.status, "ok");
        let request = chat(false)?;
        client
            .tenant("warm-token", &binding("warm")?)?
            .chat_reply(&request)
            .await?;
        let replies = futures_util::future::try_join_all((0..12).map(|index| {
            let client = &client;
            let request = &request;
            async move {
                let token = format!("credential-{index}");
                let binding = binding(&format!("correlation-{index}"))?;
                let scoped = client.tenant(&token, &binding)?;
                assert!(!format!("{scoped:?}").contains(&token));
                let reply = scoped.chat_reply(request).await?;
                assert_eq!(reply.metadata.status, 200);
                assert_eq!(
                    reply.metadata.execution_id.as_ref().map(|id| id.as_str()),
                    Some("execution-1")
                );
                Ok::<_, Box<dyn StdError + Send + Sync>>(reply)
            }
        }))
        .await?;
        assert_eq!(replies.len(), 12);
        assert_eq!(client.health().await?.status, "ok");
        let records = server.records.lock().await;
        assert_eq!(records.len(), 15);
        assert_eq!(
            records[0].connection, records[1].connection,
            "tenant call must reuse the ordinary gateway pool"
        );
        if wire == Version::HTTP_2 {
            assert!(
                records
                    .iter()
                    .all(|record| record.connection == records[0].connection)
            );
        }
        for record in records.iter() {
            assert_eq!(record.version, wire);
            assert_eq!(record.headers.get_all("authorization").iter().count(), 1);
            if record.path == "/prefix/health" {
                assert_eq!(record.headers["authorization"], "Bearer default-token");
                assert!(
                    !record
                        .headers
                        .keys()
                        .any(|name| name.as_str().starts_with("bitrouter-"))
                );
                assert_eq!(record.method, "GET");
                assert!(record.body.is_empty());
                continue;
            }
            assert_eq!(record.method, "POST");
            assert_eq!(record.path, "/prefix/v1/chat/completions");
            assert_eq!(
                serde_json::from_slice::<Value>(&record.body)?,
                serde_json::to_value(&request)?
            );
            let mut actual: Vec<_> = record
                .headers
                .keys()
                .filter(|name| name.as_str().starts_with("bitrouter-"))
                .map(|name| name.as_str())
                .collect();
            actual.sort();
            assert_eq!(
                actual,
                [
                    "bitrouter-capability-revision",
                    "bitrouter-correlation-id",
                    "bitrouter-execution-revision",
                    "bitrouter-installation-id",
                    "bitrouter-instance-id",
                    "bitrouter-receipt-mode",
                    "bitrouter-tenancy-version"
                ]
            );
            for (name, value) in [
                ("bitrouter-tenancy-version", "1"),
                ("bitrouter-installation-id", "installation"),
                ("bitrouter-instance-id", "instance"),
                ("bitrouter-capability-revision", "capability-1"),
                ("bitrouter-execution-revision", "execution-revision-1"),
                ("bitrouter-receipt-mode", "strict"),
            ] {
                assert_eq!(record.headers[name], value);
            }
            let correlation = record.headers["bitrouter-correlation-id"].to_str()?;
            if let Some(index) = correlation.strip_prefix("correlation-") {
                assert_eq!(
                    record.headers["authorization"],
                    format!("Bearer credential-{index}")
                );
            } else {
                assert_eq!(correlation, "warm");
                assert_eq!(record.headers["authorization"], "Bearer warm-token");
            }
            for name in [
                "x-bitrouter-request-id",
                "x-bitrouter-execution-id",
                "x-bitrouter-tenant-id",
                "x-bitrouter-principal-id",
                "idempotency-key",
            ] {
                assert!(!record.headers.contains_key(name));
            }
        }
    }
    Ok(())
}

#[tokio::test]
async fn malformed_bindings_credentials_and_shadowed_stream_never_send() -> TestResult {
    let server = server::serve(Version::HTTP_11, vec![]).await?;
    let client = client(&server, HttpVersion::Http1Only)?;
    let valid = binding("correlation")?;
    for token in [
        "",
        " ",
        "Bearer token",
        "token\r\nspoof:value",
        "a=b",
        "==",
        "é",
    ] {
        assert!(matches!(
            client.tenant(token, &valid),
            Err(Error::Request(_))
        ));
    }
    assert!(client.tenant(&"a".repeat(4097), &valid).is_err());
    for value in ["", "2", "1\r\nInjected: true"] {
        let mut invalid = valid.clone();
        invalid.contract_version = value.into();
        assert!(client.tenant("token", &invalid).is_err());
        invalid = valid.clone();
        invalid.receipt_mode = value.into();
        assert!(client.tenant("token", &invalid).is_err());
    }
    for field in [
        "Bitrouter-Installation-Id",
        "Bitrouter-Instance-Id",
        "Bitrouter-Capability-Revision",
        "Bitrouter-Execution-Revision",
        "Bitrouter-Correlation-Id",
    ] {
        for value in ["", "bad,identity", "a\r\nb", "a b"] {
            let mut invalid = serde_json::to_value(&valid)?;
            invalid[field] = json!(value);
            assert!(serde_json::from_value::<DispatchBinding>(invalid).is_err());
        }
    }
    let mut request = chat(false)?;
    request.extra.insert("stream".into(), json!(true));
    assert!(matches!(
        client.tenant("token", &valid)?.chat_reply(&request).await,
        Err(Error::Request(_))
    ));
    let mut request = messages(true)?;
    request.extra.insert("stream".into(), json!(false));
    assert!(matches!(
        client
            .tenant("token", &valid)?
            .messages_reply(&request)
            .await,
        Err(Error::Request(_))
    ));
    let mut request = responses(false)?;
    request.extra.insert("stream".into(), json!(true));
    assert!(matches!(
        client
            .tenant("token", &valid)?
            .responses_reply(&request)
            .await,
        Err(Error::Request(_))
    ));
    assert!(server.records.lock().await.is_empty());
    Ok(())
}

#[tokio::test]
async fn all_four_json_protocols_preserve_typed_bodies_and_execution_identity() -> TestResult {
    for (version, wire) in versions() {
        let bodies = [
            chat_json(),
            json!({"id":"m","type":"message","role":"assistant","model":"route","content":[],"stop_reason":"end_turn","usage":{"input_tokens":3,"output_tokens":2},"extension":null}),
            json!({"id":"r","object":"response","model":"route","status":"completed","output":[],"extension":null}),
            json!({"model":"route","answers":[],"usage":{"input_tokens":4,"output_tokens":2,"total_tokens":6,"input_tokens_details":{"cached_tokens":1,"cache_write_tokens":0},"output_tokens_details":{"reasoning_tokens":1}},"extension":null}),
        ];
        let mut replies = Vec::new();
        for body in &bodies {
            let mut reply = json_reply(200, Some("execution-1"), body.clone())?;
            reply.headers_mut().insert(
                "x-bitrouter-execution-id",
                HeaderValue::from_static("unrelated-shared-identity"),
            );
            reply.headers_mut().insert(
                "location",
                HeaderValue::from_static("https://other.example/control"),
            );
            reply
                .headers_mut()
                .insert("set-cookie", HeaderValue::from_static("secret"));
            replies.push(Some(reply));
        }
        let server = server::serve(wire, replies).await?;
        let client = client(&server, version)?;
        let bound = binding("correlation")?;
        let reply = client
            .tenant("token", &bound)?
            .chat_reply(&chat(false)?)
            .await?;
        assert_eq!(
            reply.metadata.execution_id.as_ref().map(|id| id.as_str()),
            Some("execution-1")
        );
        let Body::Json(body) = reply.body else {
            return Err("expected JSON".into());
        };
        assert_eq!(serde_json::to_value(body)?, bodies[0]);
        let reply = client
            .tenant("token", &bound)?
            .messages_reply(&messages(false)?)
            .await?;
        let Body::Json(body) = reply.body else {
            return Err("expected JSON".into());
        };
        assert_eq!(serde_json::to_value(body)?, bodies[1]);
        let reply = client
            .tenant("token", &bound)?
            .responses_reply(&responses(false)?)
            .await?;
        let Body::Json(body) = reply.body else {
            return Err("expected JSON".into());
        };
        assert_eq!(serde_json::to_value(body)?, bodies[2]);
        let request: DecisionRequest =
            serde_json::from_value(json!({"model":"route","input":"hi","questions":[]}))?;
        let reply = client.tenant("token", &bound)?.decisions(&request).await?;
        assert_eq!(
            reply.metadata.execution_id.as_ref().map(|id| id.as_str()),
            Some("execution-1")
        );
        assert_eq!(serde_json::to_value(reply.body)?, bodies[3]);
        let records = server.records.lock().await;
        assert_eq!(
            records.iter().map(|r| r.path.as_str()).collect::<Vec<_>>(),
            [
                "/prefix/v1/chat/completions",
                "/prefix/v1/messages",
                "/prefix/v1/responses",
                "/prefix/v1/decisions"
            ]
        );
        assert!(
            records
                .iter()
                .all(|r| r.headers["accept"] == "application/json")
        );
    }
    Ok(())
}

#[tokio::test]
async fn tenancy_errors_keep_ai_contract_and_require_execution_id_agreement() -> TestResult {
    for (version, wire) in versions() {
        let mut replies = vec![
            Some(json_reply(409, None, tenancy_error(None, "not_started"))?),
            Some(json_reply(
                409,
                Some("execution-1"),
                tenancy_error(Some("execution-1"), "started"),
            )?),
        ];
        for (header, body) in [
            (Some("execution-1"), Some("execution-2")),
            (Some("execution-1"), None),
            (None, Some("execution-1")),
            (Some("bad,identity"), None),
        ] {
            replies.push(Some(json_reply(
                409,
                header,
                tenancy_error(body, "not_started"),
            )?));
        }
        let server = server::serve(wire, replies).await?;
        let client = client(&server, version)?;
        let bound = binding("correlation")?;
        for (index, state) in [
            DispatchState::NotStarted,
            DispatchState::Started,
            DispatchState::Unknown,
            DispatchState::Unknown,
            DispatchState::Unknown,
            DispatchState::Unknown,
        ]
        .into_iter()
        .enumerate()
        {
            let error = client
                .tenant("token", &bound)?
                .chat_reply(&chat(false)?)
                .await
                .err()
                .ok_or("expected failure")?;
            assert_eq!(error.dispatch_state(), state);
            assert!(!format!("{error:?}").contains("redacted explanation"));
            let failed = failure::<()>(Err(error))?;
            let body: TenancyError = failed.tenancy_error.ok_or("missing AI tenancy error")?;
            assert_eq!(body.code, ErrorCode::CapabilityMismatch);
            assert_eq!(body.operation_id, None);
            if index == 0 {
                assert_eq!(failed.metadata.execution_id, None);
            }
            assert_eq!(
                failed.problem,
                if index < 2 {
                    None
                } else {
                    Some(Problem::Metadata)
                }
            );
        }
        assert_eq!(server.records.lock().await.len(), 6);
    }
    Ok(())
}

#[tokio::test]
async fn unexpected_status_and_malformed_metadata_never_return_model_output() -> TestResult {
    for (version, wire) in versions() {
        let mut cases = Vec::new();
        for status in [201, 202, 204, 307] {
            let mut reply = json_reply(status, Some("execution-1"), chat_json())?;
            reply.headers_mut().insert(
                "location",
                HeaderValue::from_static("/prefix/v1/chat/completions"),
            );
            cases.push((
                reply,
                if status == 307 {
                    Problem::Contract
                } else {
                    Problem::Status
                },
            ));
        }
        for id in [
            None,
            Some("bad,identity"),
            Some("bad identity"),
            Some("/execution"),
        ] {
            cases.push((json_reply(200, id, chat_json())?, Problem::Metadata));
        }
        let mut duplicate = json_reply(200, Some("execution-1"), chat_json())?;
        duplicate.headers_mut().append(
            "x-bitrouter-request-id",
            HeaderValue::from_static("execution-1"),
        );
        cases.push((duplicate, Problem::Metadata));
        let mut oversized = json_reply(200, None, chat_json())?;
        oversized.headers_mut().insert(
            "x-bitrouter-request-id",
            HeaderValue::from_str(&"a".repeat(129))?,
        );
        cases.push((oversized, Problem::Metadata));
        for content_type in [
            "application/json, text/plain",
            "application/json; charset",
            "application/json; charset=\"unfinished",
            "text/event-stream",
        ] {
            let mut reply = json_reply(200, Some("execution-1"), chat_json())?;
            reply
                .headers_mut()
                .insert("content-type", HeaderValue::from_str(content_type)?);
            cases.push((reply, Problem::ContentType));
        }
        let mut duplicate = json_reply(200, Some("execution-1"), chat_json())?;
        duplicate
            .headers_mut()
            .append("content-type", HeaderValue::from_static("application/json"));
        cases.push((duplicate, Problem::ContentType));
        let mut encoded = json_reply(200, Some("execution-1"), chat_json())?;
        encoded
            .headers_mut()
            .insert("content-encoding", HeaderValue::from_static("gzip"));
        cases.push((encoded, Problem::ContentType));
        let (replies, problems): (Vec<_>, Vec<_>) =
            cases.into_iter().map(|(r, p)| (Some(r), p)).unzip();
        let count = replies.len();
        let server = server::serve(wire, replies).await?;
        let client = client(&server, version)?;
        let bound = binding("correlation")?;
        for problem in problems {
            let failed = failure(
                client
                    .tenant("token", &bound)?
                    .chat_reply(&chat(false)?)
                    .await,
            )?;
            assert_eq!(failed.problem, Some(problem));
            assert_eq!(
                Error::Response(Box::new(failed)).dispatch_state(),
                DispatchState::Unknown
            );
        }
        assert_eq!(server.records.lock().await.len(), count);
    }
    Ok(())
}

async fn ends<T>(
    mut stream: bitrouter_sdk::gateway::tenancy::EventStream<T>,
) -> TestResult<ProtocolEnd> {
    let mut observed = None;
    while let Some(item) = stream.next().await {
        if let StreamItem::End(end) = item? {
            assert!(observed.is_none());
            observed = Some(end);
        }
    }
    observed.ok_or_else(|| "missing explicit terminal".into())
}

#[tokio::test]
async fn sse_protocols_preserve_explicit_terminals() -> TestResult {
    for (version, wire) in versions() {
        let mut replies = vec![
            Some(sse(
                "data: {\"id\":\"p\",\"object\":\"chat.completion.chunk\",\"model\":\"route\",\"choices\":[],\"extra\":\"café\"}\n\ndata: [DONE]\n\n",
            )?),
            Some(sse(
                "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
            )?),
        ];
        for status in ["completed", "incomplete", "failed"] {
            replies.push(Some(sse(&format!("event: response.{status}\ndata: {{\"type\":\"response.{status}\",\"sequence_number\":1,\"response\":{{\"id\":\"p\",\"object\":\"response\",\"model\":\"route\",\"status\":\"{status}\",\"output\":[]}}}}\n\n"))?));
        }
        let server = server::serve(wire, replies).await?;
        let client = client(&server, version)?;
        let bound = binding("correlation")?;
        let reply = client
            .tenant("token", &bound)?
            .chat_reply(&chat(true)?)
            .await?;
        assert_eq!(
            reply.metadata.execution_id.as_ref().map(|id| id.as_str()),
            Some("execution-1")
        );
        let Body::Stream(stream) = reply.body else {
            return Err("expected stream".into());
        };
        assert_eq!(ends(stream).await?, ProtocolEnd::ChatDone);
        let Body::Stream(stream) = client
            .tenant("token", &bound)?
            .messages_reply(&messages(true)?)
            .await?
            .body
        else {
            return Err("expected stream".into());
        };
        assert_eq!(ends(stream).await?, ProtocolEnd::MessagesStop);
        for expected in [
            ProtocolEnd::ResponsesCompleted,
            ProtocolEnd::ResponsesIncomplete,
            ProtocolEnd::ResponsesFailed,
        ] {
            let Body::Stream(stream) = client
                .tenant("token", &bound)?
                .responses_reply(&responses(true)?)
                .await?
                .body
            else {
                return Err("expected stream".into());
            };
            assert_eq!(ends(stream).await?, expected);
        }
        let records = server.records.lock().await;
        assert_eq!(records.len(), 5);
        assert!(
            records
                .iter()
                .all(|r| r.headers["accept"] == "text/event-stream")
        );
    }
    Ok(())
}

async fn stream_failure<T>(
    mut stream: bitrouter_sdk::gateway::tenancy::EventStream<T>,
) -> TestResult<Failure> {
    while let Some(item) = stream.next().await {
        match item {
            Err(error) => {
                assert_eq!(error.dispatch_state(), DispatchState::Unknown);
                assert!(stream.next().await.is_none());
                return failure::<()>(Err(error));
            }
            Ok(StreamItem::End(_)) => return Err("error became an explicit terminal".into()),
            _ => {}
        }
    }
    Err("stream silently ended".into())
}

#[tokio::test]
async fn sse_failures_keep_identity_without_fabricating_terminal_or_replay() -> TestResult {
    for (version, wire) in versions() {
        let server = server::serve(wire, vec![
            Some(sse("data: {\"id\":\"p\",\"object\":\"chat.completion.chunk\",\"model\":\"route\",\"choices\":[]}\n\n")?),
            Some(sse("event: error\ndata: {\"error\":{\"message\":\"upstream detail\"}}\n\ndata: [DONE]\n\n")?),
            Some(sse("event: message_stop\ndata: {\"type\":\"message_delta\"}\n\n")?),
            Some(sse("event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"p\",\"object\":\"response\",\"model\":\"route\",\"status\":\"failed\",\"output\":[]}}\n\n")?),
        ]).await?;
        let client = client(&server, version)?;
        let bound = binding("correlation")?;
        for expected in [Some(Problem::IncompleteStream), None] {
            let Body::Stream(stream) = client
                .tenant("token", &bound)?
                .chat_reply(&chat(true)?)
                .await?
                .body
            else {
                return Err("expected stream".into());
            };
            let failed = stream_failure(stream).await?;
            assert_eq!(failed.problem, expected);
            assert_eq!(
                failed.metadata.execution_id.as_ref().map(|id| id.as_str()),
                Some("execution-1")
            );
            if expected.is_none() {
                assert!(failed.gateway_error.is_some());
            }
            assert!(!format!("{failed:?}").contains("upstream detail"));
        }
        let Body::Stream(stream) = client
            .tenant("token", &bound)?
            .messages_reply(&messages(true)?)
            .await?
            .body
        else {
            return Err("expected stream".into());
        };
        assert_eq!(
            stream_failure(stream).await?.problem,
            Some(Problem::Contract)
        );
        let Body::Stream(stream) = client
            .tenant("token", &bound)?
            .responses_reply(&responses(true)?)
            .await?
            .body
        else {
            return Err("expected stream".into());
        };
        assert_eq!(
            stream_failure(stream).await?.problem,
            Some(Problem::Contract)
        );
        assert_eq!(server.records.lock().await.len(), 4);
    }
    Ok(())
}

#[tokio::test]
async fn lost_response_remains_unknown_and_is_never_retried() -> TestResult {
    for (version, wire) in versions() {
        let server = server::serve(
            wire,
            vec![None, Some(server::json(json!({"status":"ok"}))?)],
        )
        .await?;
        let client = client(&server, version)?;
        let error = client
            .tenant("token", &binding("correlation")?)?
            .chat_reply(&chat(false)?)
            .await
            .err()
            .ok_or("expected transport failure")?;
        assert!(matches!(error, Error::Transport));
        assert_eq!(error.dispatch_state(), DispatchState::Unknown);
        assert_eq!(client.health().await?.status, "ok");
        let records = server.records.lock().await;
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].path, "/prefix/v1/chat/completions");
        assert_eq!(records[1].path, "/prefix/health");
    }
    Ok(())
}

#[tokio::test]
async fn response_and_event_budgets_apply_to_success_and_error_bodies() -> TestResult {
    for (version, wire) in versions() {
        let server = server::serve(
            wire,
            vec![
                Some(json_reply(200, Some("execution-1"), chat_json())?),
                Some(json_reply(409, None, tenancy_error(None, "not_started"))?),
                Some(sse(&format!("data: {}\n\n", "a".repeat(300)))?),
            ],
        )
        .await?;
        let client = GatewayClient::with_options(
            &server.endpoint,
            None,
            TransportOptions {
                http_version: version,
                max_response_bytes: 32,
                max_event_bytes: 64,
                connect_timeout: BUDGET,
                request_timeout: Some(BUDGET),
                ..Default::default()
            },
        )?;
        let bound = binding("correlation")?;
        for id in [Some("execution-1"), None] {
            let failed = failure(
                client
                    .tenant("token", &bound)?
                    .chat_reply(&chat(false)?)
                    .await,
            )?;
            assert_eq!(failed.problem, Some(Problem::Limit));
            assert_eq!(
                failed.metadata.execution_id.as_ref().map(|id| id.as_str()),
                id
            );
        }
        let Body::Stream(stream) = client
            .tenant("token", &bound)?
            .chat_reply(&chat(true)?)
            .await?
            .body
        else {
            return Err("expected stream".into());
        };
        assert_eq!(stream_failure(stream).await?.problem, Some(Problem::Limit));
        assert_eq!(server.records.lock().await.len(), 3);
    }
    Ok(())
}

struct PendingBody {
    sent: bool,
    dropped: Option<tokio::sync::oneshot::Sender<()>>,
}
impl futures_core::Stream for PendingBody {
    type Item = Result<bytes::Bytes, std::io::Error>;
    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        if self.sent {
            return std::task::Poll::Pending;
        }
        self.sent = true;
        std::task::Poll::Ready(Some(Ok(bytes::Bytes::from_static(b"data: {\"id\":\"p\",\"object\":\"chat.completion.chunk\",\"model\":\"route\",\"choices\":[]}\n\n"))))
    }
}
impl Drop for PendingBody {
    fn drop(&mut self) {
        if let Some(sender) = self.dropped.take()
            && sender.send(()).is_err()
        {
            eprintln!("cancellation observer already closed");
        }
    }
}

#[tokio::test]
async fn dropping_stream_closes_transport_without_cancel_endpoint_or_replay() -> TestResult {
    for (version, wire) in versions() {
        let (sender, dropped) = tokio::sync::oneshot::channel();
        let reply = Response::builder()
            .header("content-type", "text/event-stream")
            .header("x-bitrouter-request-id", "execution-1")
            .body(HttpBody::from_stream(PendingBody {
                sent: false,
                dropped: Some(sender),
            }))?;
        let server = server::serve(
            wire,
            vec![Some(reply), Some(server::json(json!({"status":"ok"}))?)],
        )
        .await?;
        let client = client(&server, version)?;
        let reply = client
            .tenant("token", &binding("correlation")?)?
            .chat_reply(&chat(true)?)
            .await?;
        let Body::Stream(mut stream) = reply.body else {
            return Err("expected stream".into());
        };
        assert!(matches!(
            stream.next().await.transpose()?,
            Some(StreamItem::Event(_))
        ));
        drop(stream);
        tokio::time::timeout(BUDGET, dropped).await??;
        assert_eq!(client.health().await?.status, "ok");
        let records = server.records.lock().await;
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].path, "/prefix/v1/chat/completions");
        assert_eq!(records[1].path, "/prefix/health");
        if wire == Version::HTTP_2 {
            assert_eq!(records[0].connection, records[1].connection);
        }
    }
    Ok(())
}

#[tokio::test]
async fn streaming_calls_preserve_preadmission_errors_and_reject_202() -> TestResult {
    for (version, wire) in versions() {
        let mut accepted_stream = sse("data: [DONE]\n\n")?;
        *accepted_stream.status_mut() = axum::http::StatusCode::ACCEPTED;
        let server = server::serve(
            wire,
            vec![
                Some(json_reply(409, None, tenancy_error(None, "not_started"))?),
                Some(json_reply(202, None, tenancy_error(None, "not_started"))?),
                Some(accepted_stream),
            ],
        )
        .await?;
        let client = client(&server, version)?;
        let bound = binding("correlation")?;
        let error = client
            .tenant("token", &bound)?
            .messages_reply(&messages(true)?)
            .await
            .err()
            .ok_or("expected rejection")?;
        assert_eq!(error.dispatch_state(), DispatchState::NotStarted);
        assert!(failure::<()>(Err(error))?.tenancy_error.is_some());
        let failed = failure(
            client
                .tenant("token", &bound)?
                .responses_reply(&responses(true)?)
                .await,
        )?;
        assert_eq!(failed.metadata.status, 202);
        assert!(failed.tenancy_error.is_some());
        assert_eq!(failed.problem, Some(Problem::Status));
        assert_eq!(
            Error::Response(Box::new(failed)).dispatch_state(),
            DispatchState::Unknown
        );
        let failed = failure(
            client
                .tenant("token", &bound)?
                .chat_reply(&chat(true)?)
                .await,
        )?;
        assert_eq!(failed.problem, Some(Problem::Status));
        assert_eq!(
            failed.metadata.execution_id.as_ref().map(|id| id.as_str()),
            Some("execution-1")
        );
        assert_eq!(server.records.lock().await.len(), 3);
    }
    Ok(())
}

#[tokio::test]
async fn tenancy_error_validation_rejects_duplicate_fields_and_invalid_evidence() -> TestResult {
    for (version, wire) in versions() {
        let mut replies = Vec::new();
        for invalid in [
            format!("{{\"code\":\"forbidden\",{}", &tenancy_error(None, "not_started").to_string()[1..]),
            tenancy_error(None, "unknown-state").to_string(),
            json!({"code":"capability_mismatch","message":"rejected","dispatch_state":"not_started","operation_id":null}).to_string(),
            { let mut error = tenancy_error(None, "not_started"); error["message"] = json!("a".repeat(513)); error.to_string() },
        ] {
            replies.push(Some(Response::builder().status(409).header("content-type", "application/json").body(HttpBody::from(invalid))?));
        }
        let server = server::serve(wire, replies).await?;
        let client = client(&server, version)?;
        let bound = binding("correlation")?;
        for _ in 0..4 {
            let failed = failure(
                client
                    .tenant("token", &bound)?
                    .chat_reply(&chat(false)?)
                    .await,
            )?;
            assert_eq!(failed.problem, Some(Problem::Contract));
            assert!(failed.tenancy_error.is_none());
            assert_eq!(
                Error::Response(Box::new(failed)).dispatch_state(),
                DispatchState::Unknown
            );
        }
    }
    Ok(())
}

#[tokio::test]
async fn chunked_json_is_bounded_and_mid_body_transport_loss_keeps_identity() -> TestResult {
    for (version, wire) in versions() {
        let chunks = (0..3).map(|_| Ok::<_, std::io::Error>(bytes::Bytes::from(vec![b'a'; 20])));
        let bounded = Response::builder()
            .header("content-type", "application/json")
            .header("x-bitrouter-request-id", "execution-1")
            .body(HttpBody::from_stream(futures_util::stream::iter(chunks)))?;
        let broken = async_stream::stream! {
            yield Ok::<_, std::io::Error>(bytes::Bytes::from_static(b"data: {\"id\":\"p\",\"object\":\"chat.completion.chunk\",\"model\":\"route\",\"choices\":[]}\n\n"));
            tokio::time::sleep(Duration::from_millis(20)).await;
            yield Err(std::io::Error::other("fixture interrupted body"));
        };
        let broken = Response::builder()
            .header("content-type", "text/event-stream")
            .header("x-bitrouter-request-id", "execution-1")
            .body(HttpBody::from_stream(broken))?;
        let server = server::serve(wire, vec![Some(bounded), Some(broken)]).await?;
        let client = GatewayClient::with_options(
            &server.endpoint,
            None,
            TransportOptions {
                http_version: version,
                max_response_bytes: 32,
                connect_timeout: BUDGET,
                request_timeout: Some(BUDGET),
                ..Default::default()
            },
        )?;
        let bound = binding("correlation")?;
        assert_eq!(
            failure(
                client
                    .tenant("token", &bound)?
                    .chat_reply(&chat(false)?)
                    .await
            )?
            .problem,
            Some(Problem::Limit)
        );
        let Body::Stream(stream) = client
            .tenant("token", &bound)?
            .chat_reply(&chat(true)?)
            .await?
            .body
        else {
            return Err("expected stream".into());
        };
        let failed = stream_failure(stream).await?;
        assert_eq!(failed.problem, Some(Problem::Transport));
        assert_eq!(
            failed.metadata.execution_id.as_ref().map(|id| id.as_str()),
            Some("execution-1")
        );
        assert_eq!(server.records.lock().await.len(), 2);
    }
    Ok(())
}
