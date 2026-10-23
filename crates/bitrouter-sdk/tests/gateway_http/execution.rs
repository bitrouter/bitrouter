//! Shared submission uses HTTP only and never replays an existing execution.

use super::*;
use bitrouter_sdk::gateway::execution::{ExecutionMetadata, Submission, SubmissionOptions};

#[path = "execution/failures.rs"]
mod failures;
#[path = "execution/options.rs"]
mod options;
#[path = "execution/validation.rs"]
mod validation;

fn status(id: &str, operation: &str) -> Value {
    json!({
        "execution_id":id, "correlation_id":"original", "revision":3,
        "operation":operation, "config_generation":7, "accepted_at_ms":1,
        "updated_at_ms":2, "state":{"status":"unknown","reason":"owner_lost"},
        "attempts":[]
    })
}

fn shared(mut reply: Reply, id: &str, correlation: &str) -> Reply {
    reply.headers.extend([
        ("x-bitrouter-execution-id", id.into()),
        ("x-bitrouter-request-id", correlation.into()),
        ("location", format!("/v1/gateway/executions/{id}")),
    ]);
    reply
}

fn duplicate(operation: &str) -> Reply {
    let mut reply = shared(
        Reply::json(status("execution-1", operation)),
        "execution-1",
        "current",
    );
    reply.status = 202;
    reply
}

fn submitted<T>(value: Submission<T>) -> TestResult<(T, ExecutionMetadata)> {
    match value {
        Submission::Response { response, metadata } => Ok((response, metadata)),
        _ => Err("expected first response".into()),
    }
}

fn existing<T>(value: Submission<T>) -> TestResult {
    let Submission::Existing {
        execution,
        metadata,
    } = value
    else {
        return Err("expected existing execution".into());
    };
    assert_eq!(execution.execution_id, "execution-1");
    assert_eq!(execution.correlation_id, "original");
    assert_eq!(metadata.correlation_id, "current");
    Ok(())
}

fn decisions_request() -> TestResult<DecisionRequest> {
    Ok(serde_json::from_value(json!({
        "model":"judge", "input":"evidence",
        "questions":[{"type":"predicate","instructions":"Is it correct?"}]
    }))?)
}

#[tokio::test]
async fn all_json_submissions_return_metadata_and_preserve_wire_fields() -> TestResult {
    let messages = json!({"id":"m-1","type":"message","role":"assistant","model":"route","content":[],"stop_reason":"end_turn","usage":{"input_tokens":3,"output_tokens":2}});
    let responses = json!({"id":"continuation-1","object":"response","model":"route","status":"completed","output":[]});
    let decisions = json!({"model":"judge","answers":[{"type":"predicate","name":null,"probability":0.75}],"usage":{"input_tokens":4,"output_tokens":2,"total_tokens":6,"input_tokens_details":{"cached_tokens":1,"cache_write_tokens":0},"output_tokens_details":{"reasoning_tokens":1}}});
    let server = serve(
        [chat_response(), messages, responses, decisions.clone()]
            .into_iter()
            .map(|body| shared(Reply::json(body), "execution-1", "current"))
            .collect(),
    )
    .await?;
    let client = GatewayClient::new(&format!("{}/proxy/v1", server.url), Some("credential"))?;
    let options = SubmissionOptions {
        correlation_id: Some("current".into()),
        ..Default::default()
    };
    let (chat, metadata) = submitted(
        client
            .submit_chat_completions(&chat_request(false)?, &options)
            .await?,
    )?;
    assert_eq!(
        chat.choices[0].message.extra["extra_content"]["signature"],
        "opaque"
    );
    assert_eq!(metadata.execution_id, "execution-1");
    assert_eq!(
        metadata.status_location,
        format!("{}/proxy/v1/gateway/executions/execution-1", server.url)
    );
    assert_eq!(
        submitted(
            client
                .submit_messages(&messages_request(false)?, &options)
                .await?
        )?
        .0
        .usage
        .input_tokens,
        3
    );
    assert_eq!(
        submitted(
            client
                .submit_responses(&responses_request(false)?, &options)
                .await?
        )?
        .0
        .id,
        "continuation-1"
    );
    assert_eq!(
        serde_json::to_value(
            submitted(
                client
                    .submit_decisions(&decisions_request()?, &options)
                    .await?
            )?
            .0
        )?,
        decisions
    );
    let records = server.records.lock().await;
    assert_eq!(records.len(), 4);
    assert!(records.iter().all(|record| record.method == "POST"
        && record.credential.as_deref() == Some("Bearer credential")));
    assert_eq!(
        serde_json::from_slice::<Value>(&records[0].body)?["messages"][0]["extra_content"]["native"],
        "retain"
    );
    Ok(())
}

#[tokio::test]
async fn every_submission_method_classifies_202_before_model_or_sse_decoding() -> TestResult {
    let server = serve(vec![
        duplicate("generation"),
        duplicate("generation"),
        duplicate("generation"),
        duplicate("decisions"),
        duplicate("generation"),
        duplicate("generation"),
        duplicate("generation"),
    ])
    .await?;
    let client = GatewayClient::new(&server.url, None)?;
    let options = SubmissionOptions {
        correlation_id: Some("current".into()),
        idempotency_key: Some("binding".into()),
        ..Default::default()
    };
    existing(
        client
            .submit_chat_completions(&chat_request(false)?, &options)
            .await?,
    )?;
    existing(
        client
            .submit_messages(&messages_request(false)?, &options)
            .await?,
    )?;
    existing(
        client
            .submit_responses(&responses_request(false)?, &options)
            .await?,
    )?;
    existing(
        client
            .submit_decisions(&decisions_request()?, &options)
            .await?,
    )?;
    existing(
        client
            .submit_stream_chat_completions(&chat_request(true)?, &options)
            .await?,
    )?;
    existing(
        client
            .submit_stream_messages(&messages_request(true)?, &options)
            .await?,
    )?;
    existing(
        client
            .submit_stream_responses(&responses_request(true)?, &options)
            .await?,
    )?;
    let records = server.records.lock().await;
    assert_eq!(records.len(), 7);
    assert!(
        records
            .iter()
            .all(|record| record.headers["idempotency-key"] == "binding")
    );
    Ok(())
}

async fn drain<T>(mut stream: bitrouter_sdk::stream::EventStream<T>) -> TestResult<usize> {
    let mut count = 0;
    while let Some(event) = stream.next().await {
        event?;
        count += 1;
    }
    Ok(count)
}

#[tokio::test]
async fn all_streams_deliver_metadata_before_incremental_consumption() -> TestResult {
    let chat = format!("data: {}\n\ndata: [DONE]\n\n", chat_chunk());
    let messages = "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";
    let responses = format!(
        "event: response.completed\ndata: {}\n\n",
        json!({
            "type":"response.completed","sequence_number":1,
            "response":{"id":"r","object":"response","model":"route","status":"completed","output":[]}
        })
    );
    let server = serve(
        [chat.as_str(), messages, responses.as_str()]
            .into_iter()
            .map(|body| shared(Reply::sse(body), "execution-1", "current"))
            .collect(),
    )
    .await?;
    let client = GatewayClient::new(&server.url, None)?;
    let options = SubmissionOptions::default();
    let (stream, metadata) = submitted(
        client
            .submit_stream_chat_completions(&chat_request(true)?, &options)
            .await?,
    )?;
    assert_eq!(metadata.execution_id, "execution-1");
    assert!(drain(stream).await? > 0);
    assert!(
        drain(
            submitted(
                client
                    .submit_stream_messages(&messages_request(true)?, &options)
                    .await?
            )?
            .0
        )
        .await?
            > 0
    );
    assert!(
        drain(
            submitted(
                client
                    .submit_stream_responses(&responses_request(true)?, &options)
                    .await?
            )?
            .0
        )
        .await?
            > 0
    );
    assert_eq!(server.records.lock().await.len(), 3);
    Ok(())
}

#[tokio::test]
async fn query_encodes_one_id_segment_and_preserves_proxy_prefix() -> TestResult {
    for (id, encoded) in [
        ("execution-1", "execution-1"),
        ("a/b", "a%2Fb"),
        ("%2e%2e", "%252e%252e"),
        ("a?b#c", "a%3Fb%23c"),
        ("a b", "a%20b"),
        ("https://other.test/x", "https:%2F%2Fother.test%2Fx"),
    ] {
        let mut reply = shared(Reply::json(status(id, "generation")), id, "original");
        for (name, value) in &mut reply.headers {
            if *name == "location" {
                *value = format!("/v1/gateway/executions/{encoded}");
            }
        }
        let server = serve(vec![reply]).await?;
        let client = GatewayClient::new(&format!("{}/proxy/v1/", server.url), Some("key"))?;
        assert_eq!(client.execution(id).await?.execution_id, id);
        let records = server.records.lock().await;
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].method, "GET");
        assert_eq!(
            records[0].target,
            format!("/proxy/v1/gateway/executions/{encoded}")
        );
        assert_eq!(records[0].credential.as_deref(), Some("Bearer key"));
    }
    Ok(())
}
