use std::{convert::Infallible, sync::Arc};

use axum::{body::Body, response::Response};
use bitrouter_ai::{
    decisions::DecisionRequest,
    gateway::operator::{Command, CredentialImport, CredentialInput, Mutation},
    protocol::{
        chat_completions::ChatRequest, messages::MessagesRequest, responses::ResponsesRequest,
    },
};
use bitrouter_sdk::{
    error::Error,
    gateway::execution::{Submission, SubmissionOptions},
    stream::EventStream,
};
use futures_util::StreamExt;
use serde_json::Value;
use tokio::sync::Barrier;

use super::*;

fn chat_request(stream: bool) -> TestResult<ChatRequest> {
    Ok(serde_json::from_value(json!({
        "model":"route", "stream":stream,
        "messages":[{"role":"user","content":"hello","extra_content":{"native":"retain"}}]
    }))?)
}

fn messages_request(stream: bool) -> TestResult<MessagesRequest> {
    Ok(serde_json::from_value(json!({
        "model":"route", "stream":stream, "messages":[{"role":"user","content":"hello"}]
    }))?)
}

fn responses_request(stream: bool) -> TestResult<ResponsesRequest> {
    Ok(serde_json::from_value(
        json!({"model":"route", "stream":stream, "input":"hello"}),
    )?)
}

fn chat_response() -> Value {
    json!({"id":"chat", "object":"chat.completion", "model":"route", "choices":[{
        "index":0, "message":{"role":"assistant","content":"hello"}, "finish_reason":"stop"
    }]})
}

fn gateway(server: &server::Server) -> TestResult<GatewayClient> {
    Ok(GatewayClient::with_options(
        &format!("{}/prefix/v1", server.endpoint),
        Some(TOKEN),
        transport_options(HttpVersion::Http2PriorKnowledge),
    )?)
}

#[tokio::test]
async fn h2c_preserves_json_apis_submission_metadata_and_reuses_the_pool() -> TestResult {
    let decisions = json!({"model":"judge", "answers":[{"type":"predicate","name":null,"probability":0.75}],
        "usage":{"input_tokens":4,"output_tokens":2,"total_tokens":6,
            "input_tokens_details":{"cached_tokens":1,"cache_write_tokens":0},
            "output_tokens_details":{"reasoning_tokens":1}}});
    let mut submitted = server::json(chat_response())?;
    submitted
        .headers_mut()
        .insert("x-bitrouter-execution-id", "execution-1".parse()?);
    submitted
        .headers_mut()
        .insert("x-bitrouter-request-id", "request-1".parse()?);
    submitted
        .headers_mut()
        .insert("location", "/v1/gateway/executions/execution-1".parse()?);
    let server = server::serve(Version::HTTP_2, vec![
        Some(server::json(chat_response())?),
        Some(server::json(json!({"id":"message", "type":"message", "role":"assistant",
            "model":"route", "content":[], "stop_reason":"end_turn",
            "usage":{"input_tokens":3,"output_tokens":2}}))?),
        Some(server::json(json!({"id":"response", "object":"response", "model":"route",
            "status":"completed", "output":[]}))?),
        Some(server::json(decisions.clone())?),
        Some(server::json(json!({"object":"list", "data":[{"id":"route", "object":"model", "providers":["one"]}]}))?),
        Some(server::json(json!({"status":"ok"}))?),
        Some(Response::new(Body::from("requests_total 1\n"))),
        Some(submitted),
    ]).await?;
    let client = gateway(&server)?;
    assert_eq!(
        client.chat_completions(&chat_request(false)?).await?.id,
        "chat"
    );
    let cloned = client.clone();
    assert_eq!(
        cloned
            .messages(&messages_request(false)?)
            .await?
            .usage
            .input_tokens,
        3
    );
    assert_eq!(
        client.responses(&responses_request(false)?).await?.id,
        "response"
    );
    let request: DecisionRequest =
        serde_json::from_value(json!({"model":"judge", "input":"evidence",
        "questions":[{"type":"predicate","instructions":"Is it correct?"}]}))?;
    assert_eq!(
        serde_json::to_value(client.decisions(&request).await?)?,
        decisions
    );
    assert_eq!(client.models().await?.data[0].providers, vec!["one"]);
    assert_eq!(cloned.health().await?.status, "ok");
    assert_eq!(client.metrics().await?, "requests_total 1\n");
    let submission = client
        .submit_chat_completions(
            &chat_request(false)?,
            &SubmissionOptions {
                correlation_id: Some("request-1".into()),
                idempotency_key: Some("once".into()),
                ..Default::default()
            },
        )
        .await?;
    let Submission::Response { response, metadata } = submission else {
        return Err("expected first submission response".into());
    };
    assert_eq!(response.id, "chat");
    assert_eq!(metadata.execution_id, "execution-1");
    let records = server.records.lock().await;
    assert_eq!(records.len(), 8);
    assert_eq!(
        records.iter().map(|r| r.path.as_str()).collect::<Vec<_>>(),
        vec![
            "/prefix/v1/chat/completions",
            "/prefix/v1/messages",
            "/prefix/v1/responses",
            "/prefix/v1/decisions",
            "/prefix/v1/models",
            "/prefix/health",
            "/prefix/metrics",
            "/prefix/v1/chat/completions",
        ]
    );
    for record in records.iter() {
        assert_eq!(record.version, Version::HTTP_2);
        assert_eq!(
            record.connection, records[0].connection,
            "clones must share the h2 pool"
        );
        assert_eq!(record.headers["authorization"], format!("Bearer {TOKEN}"));
    }
    assert_eq!(records[0].method, "POST");
    assert_eq!(
        serde_json::from_slice::<Value>(&records[0].body)?["messages"][0]["extra_content"]["native"],
        "retain"
    );
    assert_eq!(records[7].headers["idempotency-key"], "once");
    Ok(())
}

fn gated_sse(first: String, last: &'static str, gate: Arc<Barrier>) -> TestResult<Response> {
    let body = async_stream::stream! {
        // Fragment UTF-8 and SSE delimiters independently of HTTP/2 framing.
        for byte in first.into_bytes() {
            yield Ok::<_, Infallible>(vec![byte]);
        }
        gate.wait().await;
        yield Ok::<_, Infallible>(last.as_bytes().to_vec());
    };
    Ok(Response::builder()
        .header("content-type", "text/event-stream")
        .body(Body::from_stream(body))?)
}

async fn drain<T>(mut stream: EventStream<T>) -> TestResult<usize> {
    let mut count = 0;
    while let Some(event) = stream.next().await {
        event?;
        count += 1;
    }
    Ok(count)
}

#[tokio::test]
async fn h2c_multiplexes_incremental_streams_and_a_read_on_one_connection() -> TestResult {
    let gate = Arc::new(Barrier::new(4));
    let server = server::serve(Version::HTTP_2, vec![
        Some(gated_sse(format!("id: first\ndata: {}\n\n", json!({"id":"chat", "object":"chat.completion.chunk",
            "model":"route", "choices":[{"index":0,"delta":{"content":"你好"},"finish_reason":null}]})),
            "data: [DONE]\n\n", gate.clone())?),
        Some(gated_sse("event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hello\"}}\n\n".into(),
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n", gate.clone())?),
        Some(gated_sse("event: response.created\ndata: {\"type\":\"response.created\",\"sequence_number\":0}\n\n".into(),
            "event: response.completed\ndata: {\"type\":\"response.completed\",\"sequence_number\":1,\"response\":{\"id\":\"response\",\"object\":\"response\",\"model\":\"route\",\"status\":\"completed\",\"output\":[]}}\n\n", gate.clone())?),
        Some(server::json(json!({"status":"ok"}))?),
    ]).await?;
    let client = gateway(&server)?;
    let mut chat = client.stream_chat_completions(&chat_request(true)?).await?;
    let mut messages = client
        .clone()
        .stream_messages(&messages_request(true)?)
        .await?;
    let mut responses = client
        .clone()
        .stream_responses(&responses_request(true)?)
        .await?;
    let first = chat.next().await.ok_or("missing chat delta")??;
    assert_eq!(first.id, "first");
    assert_eq!(
        first.data.choices[0].delta.content,
        Some(Some(json!("你好")))
    );
    assert_eq!(
        messages
            .next()
            .await
            .ok_or("missing messages delta")??
            .event,
        "content_block_delta"
    );
    assert_eq!(
        responses
            .next()
            .await
            .ok_or("missing response event")??
            .event,
        "response.created"
    );
    // All three bodies are still open. A fourth request must make progress.
    assert_eq!(client.health().await?.status, "ok");
    tokio::time::timeout(BUDGET, gate.wait()).await?;
    let counts = tokio::try_join!(drain(chat), drain(messages), drain(responses))?;
    assert_eq!(counts, (0, 1, 1));
    let records = server.records.lock().await;
    assert_eq!(records.len(), 4);
    assert!(
        records
            .iter()
            .all(|r| r.version == Version::HTTP_2 && r.connection == records[0].connection)
    );
    Ok(())
}

#[tokio::test]
async fn h2c_lost_mutations_are_not_retried_and_operator_lookup_is_explicit() -> TestResult {
    let server = server::serve(
        Version::HTTP_2,
        vec![
            None,
            None,
            Some(server::json(
                json!({"status":"unresolved","operation_id":"op"}),
            )?),
        ],
    )
    .await?;
    assert!(matches!(
        gateway(&server)?
            .chat_completions(&chat_request(false)?)
            .await,
        Err(Error::Transport(_))
    ));
    let operator = OperatorClient::with_options(
        &format!("{}/prefix/operator/v1", server.endpoint),
        TOKEN,
        operator_options(HttpVersion::Http2PriorKnowledge),
    )?;
    let mutation = Mutation {
        operation_id: "op".into(),
        command: Command::ImportCredential(CredentialImport {
            provider: "fixture".into(),
            account: "default".into(),
            resource_pool_id: "pool".into(),
            expected_version: None,
            credential: CredentialInput::ApiKey {
                value: "secret".into(),
            },
        }),
    };
    assert!(matches!(operator.apply(&mutation).await,
        Err(Error::OperatorUnknown { operation_id, .. }) if operation_id == "op"));
    assert_eq!(
        server.records.lock().await.len(),
        2,
        "no replay after lost replies"
    );
    assert!(matches!(
        operator.operation("op").await?,
        OperationLookup::Unresolved { .. }
    ));
    let records = server.records.lock().await;
    assert_eq!(records.len(), 3);
    assert!(records.iter().all(|r| r.version == Version::HTTP_2));
    assert_eq!(records[1].method, "POST");
    assert_eq!(records[1].path, "/prefix/operator/v1/mutations");
    assert_eq!(
        records[1].headers["authorization"],
        format!("Bearer {TOKEN}")
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&records[1].body)?["operation_id"],
        "op"
    );
    assert_eq!(records[1].connection, records[2].connection);
    Ok(())
}

#[tokio::test]
async fn h2c_keeps_error_metadata_and_stream_limits() -> TestResult {
    let mut failure = server::json(json!({"error":{"message":"busy","type":"rate_limit_error"}}))?;
    *failure.status_mut() = axum::http::StatusCode::TOO_MANY_REQUESTS;
    failure.headers_mut().insert("retry-after", "7".parse()?);
    let server = server::serve(
        Version::HTTP_2,
        vec![
            Some(failure),
            Some(
                Response::builder()
                    .header("content-type", "text/event-stream")
                    .body(Body::from(format!("data: {}", "x".repeat(300))))?,
            ),
            Some(
                Response::builder()
                    .header("content-type", "text/event-stream")
                    .body(Body::from(": heartbeat\n\n"))?,
            ),
            Some(server::json(
                json!({"status":"ok","extra":"x".repeat(300)}),
            )?),
        ],
    )
    .await?;
    let client = GatewayClient::with_options(
        &server.endpoint,
        None,
        TransportOptions {
            max_response_bytes: 128,
            max_event_bytes: 64,
            ..transport_options(HttpVersion::Http2PriorKnowledge)
        },
    )?;
    let Err(Error::Service(failure)) = client.health().await else {
        return Err("missing typed HTTP/2 service error".into());
    };
    assert_eq!(failure.status, 429);
    assert_eq!(failure.headers["retry-after"], "7");
    assert!(failure.body.is_some());
    let mut stream = client.stream_chat_completions(&chat_request(true)?).await?;
    assert!(matches!(stream.next().await, Some(Err(Error::Limit))));
    assert!(stream.next().await.is_none());
    let mut stream = client.stream_chat_completions(&chat_request(true)?).await?;
    assert!(matches!(
        stream.next().await,
        Some(Err(Error::IncompleteStream))
    ));
    assert!(stream.next().await.is_none());
    assert!(matches!(client.health().await, Err(Error::Limit)));
    assert_eq!(server.records.lock().await.len(), 4);
    Ok(())
}

#[tokio::test]
async fn prior_knowledge_does_not_fall_back_to_http1() -> TestResult {
    let server =
        server::serve(Version::HTTP_11, vec![Some(server::json(chat_response())?)]).await?;
    assert!(matches!(
        gateway(&server)?
            .chat_completions(&chat_request(false)?)
            .await,
        Err(Error::Transport(_))
    ));
    assert!(
        server.records.lock().await.is_empty(),
        "must not send an HTTP/1 mutation"
    );
    Ok(())
}
