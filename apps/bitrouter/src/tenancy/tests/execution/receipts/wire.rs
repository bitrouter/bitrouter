use super::*;
use bitrouter_ai::tenancy::{
    execution::{Outcome, Provenance, ReceiptState},
    validation::Validate,
};

#[tokio::test]
async fn all_four_actual_json_protocols_persist_original_usage_before_http_completion() -> Result<()>
{
    for protocol in ["chat_completions", "messages", "responses", "decisions"] {
        let mut harness = Harness::new(
            protocol,
            StatusCode::OK,
            json_response(protocol, full_usage(protocol))?,
        )
        .await?;
        let modes: &[bool] = if cfg!(feature = "http2") {
            &[false, true]
        } else {
            &[false]
        };
        let mut identities = std::collections::HashSet::new();
        for &h2 in modes {
            let client = Harness::client(h2)?;
            let response = harness.send(&client).send().await?;
            ensure!(
                response.status() == StatusCode::OK,
                "{protocol}: {}",
                response.status()
            );
            ensure!(
                response.version()
                    == if h2 {
                        ::http::Version::HTTP_2
                    } else {
                        ::http::Version::HTTP_11
                    }
            );
            let id = execution_id(&response)?;
            ensure!(identities.insert(id.clone()));
            let body = response.bytes().await?;
            ensure!(!body.is_empty());
            // A new authenticated HTTP lookup immediately after caller completion
            // must already find durable bytes, without a polling/retry loop.
            let receipt = harness.receipt(&client, &id).await?;
            receipt.validate()?;
            ensure!(
                receipt.execution_id == id
                    && receipt.tenant_id == harness.fixture.tenant
                    && receipt.principal_id == harness.fixture.principal
            );
            ensure!(receipt.execution_revision == harness.revision.revision_id);
            ensure!(receipt.outcome == Outcome::Completed && receipt.attempts.len() == 1);
            let attempt = &receipt.attempts[0];
            ensure!(attempt.outcome == Outcome::Completed);
            let target = attempt.target.as_ref().context("actual target")?;
            ensure!(
                target.provider_id.as_str() == "mock"
                    && target.model_id == "model-a"
                    && target.credential_version.as_str() == "r1"
            );
            ensure!(
                target.account_id.as_str().starts_with("acct1-")
                    && target.credential_id.as_str().starts_with("cred1-")
            );
            ensure!(
                receipt.usage.input_tokens == Some(if protocol == "messages" { 16 } else { 11 })
                    && receipt.usage.output_tokens == Some(7)
            );
            ensure!(
                receipt.usage.cache_read_tokens == Some(3)
                    && receipt.usage.cache_write_tokens == Some(2)
            );
            ensure!(
                receipt.usage.reasoning_tokens
                    == if protocol == "messages" {
                        None
                    } else {
                        Some(4)
                    }
            );
            ensure!(receipt.usage.provenance == Provenance::Provider);
            ensure!(receipt.usage.complete == (protocol != "messages"));
            ensure!(
                receipt.state
                    == if protocol == "messages" {
                        ReceiptState::Incomplete
                    } else {
                        ReceiptState::Committed
                    }
            );
            ensure!(attempt.native_usage.len() == if protocol == "messages" { 4 } else { 5 });
            ensure!(serde_json::to_vec(&attempt.native_usage)?.len() <= 8192);
            ensure!(!serde_json::to_string(&receipt)?.contains("owned-upstream-secret"));
            let observed = tokio::time::timeout(TIMEOUT, harness.received.recv())
                .await?
                .context("actual provider POST")?;
            ensure!(observed["model"] == "model-a");
        }
        let page = harness.page().await?;
        ensure!(
            page.execution_ids.len() == modes.len()
                && page.multiple_executions == (modes.len() > 1)
        );
        ensure!(page.execution_ids.iter().all(|id| identities.contains(id)));
        ensure!(harness.calls.load(Ordering::SeqCst) == modes.len());
        harness.fixture.service.shutdown().await?;
    }
    Ok(())
}

#[tokio::test]
async fn canonical_default_zeroes_and_arbitrary_metadata_never_become_receipt_evidence()
-> Result<()> {
    let usage = json!({"prompt_tokens":11,"provider_private_extension":{"arbitrary":"not-retained".repeat(10000)}});
    let harness = Harness::new(
        "chat_completions",
        StatusCode::OK,
        json_response("chat_completions", usage)?,
    )
    .await?;
    let client = Harness::client(false)?;
    let response = harness.send(&client).send().await?;
    ensure!(response.status() == StatusCode::OK);
    let id = execution_id(&response)?;
    let _ = response.bytes().await?;
    let receipt = harness.receipt(&client, &id).await?;
    ensure!(receipt.outcome == Outcome::Completed && !receipt.usage.complete);
    ensure!(receipt.usage.input_tokens == Some(11) && receipt.usage.output_tokens.is_none());
    ensure!(
        receipt.usage.cache_read_tokens.is_none()
            && receipt.usage.cache_write_tokens.is_none()
            && receipt.usage.reasoning_tokens.is_none()
    );
    ensure!(receipt.attempts[0].native_usage.len() == 1);
    let encoded = serde_json::to_string(&receipt)?;
    ensure!(!encoded.contains("not-retained") && encoded.len() < 8192);
    harness.fixture.service.shutdown().await?;
    Ok(())
}

pub(super) fn chat_stream(usage: Value, terminal: bool) -> Result<String> {
    let first = json!({"id":"chatcmpl-receipts","object":"chat.completion.chunk","model":"model-a","choices":[{"index":0,"delta":{"role":"assistant","content":"receipt reply"},"finish_reason":null}]});
    let last = json!({"id":"chatcmpl-receipts","object":"chat.completion.chunk","model":"model-a","choices":[{"index":0,"delta":{},"finish_reason":if terminal{json!("stop")}else{Value::Null}}],"usage":usage});
    Ok(format!(
        "data: {}\n\ndata: {}\n\n{}",
        serde_json::to_string(&first)?,
        serde_json::to_string(&last)?,
        if terminal { "data: [DONE]\n\n" } else { "" }
    ))
}
#[tokio::test]
async fn json_and_sse_evidence_share_original_numbers_and_unresolved_stream_is_unknown()
-> Result<()> {
    for terminal in [true, false] {
        let harness = Harness::with_reply(
            "chat_completions",
            StatusCode::OK,
            chat_stream(full_usage("chat_completions"), terminal)?,
            true,
            None,
        )
        .await?;
        let client = Harness::client(false)?;
        let response = harness.send(&client).send().await?;
        ensure!(response.status() == StatusCode::OK);
        let id = execution_id(&response)?;
        let body = response.text().await?;
        let receipt = harness.receipt(&client, &id).await?;
        ensure!(receipt.attempts.len() == 1);
        if terminal {
            ensure!(body.contains("[DONE]"));
            ensure!(
                receipt.attempts[0].outcome == Outcome::Completed
                    && receipt.usage.input_tokens == Some(11)
                    && receipt.usage.complete
            );
        } else {
            ensure!(
                receipt.attempts[0].outcome == Outcome::Unknown
                    && receipt.state == ReceiptState::Unknown
            );
            ensure!(
                receipt.usage.provenance == Provenance::Provider
                    && receipt.usage.input_tokens == Some(11)
                    && receipt.usage.output_tokens == Some(7)
                    && !receipt.usage.complete,
                "unknown completion must preserve observed partial provider counts"
            );
            ensure!(
                receipt.attempts[0].native_usage.len() == 5,
                "observed numeric partial evidence was lost"
            );
        }
        ensure!(harness.calls.load(Ordering::SeqCst) == 1);
        harness.fixture.service.shutdown().await?;
    }
    Ok(())
}

#[tokio::test]
async fn known_http_rejection_and_invalid_completed_decisions_preserve_actual_paid_attempt()
-> Result<()> {
    for protocol in ["chat_completions", "decisions"] {
        let harness = Harness::new(
            protocol,
            StatusCode::FORBIDDEN,
            json!({"error":{"message":"denied"},"usage":full_usage(protocol)}),
        )
        .await?;
        let client = Harness::client(false)?;
        let response = harness.send(&client).send().await?;
        ensure!(response.status() == StatusCode::BAD_GATEWAY);
        let id = execution_id(&response)?;
        let _ = response.bytes().await?;
        let receipt = harness.receipt(&client, &id).await?;
        ensure!(
            receipt.outcome == Outcome::Failed
                && receipt.attempts.len() == 1
                && receipt.attempts[0].outcome == Outcome::Failed
        );
        ensure!(receipt.usage.input_tokens == Some(11));
        harness.fixture.service.shutdown().await?;
    }
    let harness=Harness::new("decisions",StatusCode::OK,json!({"model":"model-a","answers":[{"type":"predicate","name":"q","probability":2.0}],"usage":full_usage("decisions")})).await?;
    let client = Harness::client(false)?;
    let response = harness.send(&client).send().await?;
    ensure!(!response.status().is_success());
    let id = execution_id(&response)?;
    let _ = response.bytes().await?;
    let receipt = harness.receipt(&client, &id).await?;
    ensure!(receipt.attempts.len() == 1 && receipt.attempts[0].outcome == Outcome::Failed);
    ensure!(receipt.usage.input_tokens == Some(11) && receipt.usage.output_tokens == Some(7));
    ensure!(harness.calls.load(Ordering::SeqCst) == 1);
    harness.fixture.service.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn messages_partial_snapshots_and_responses_terminal_usage_are_observed_on_actual_sse_wires()
-> Result<()> {
    let messages = vec![
        json!({"type":"message_start","message":{"id":"msg_receipts","type":"message","role":"assistant","model":"model-a","content":[],"stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":11,"output_tokens":0,"cache_read_input_tokens":3,"cache_creation_input_tokens":2}}}),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"receipt reply"}}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":7}}),
        json!({"type":"message_stop"}),
    ];
    let response = json_response("responses", full_usage("responses"))?;
    let responses = vec![
        json!({"type":"response.created","response":{"id":"resp_receipts","status":"in_progress"}}),
        json!({"type":"response.output_text.delta","delta":"receipt reply"}),
        json!({"type":"response.completed","response":response}),
    ];
    for (protocol, events) in [("messages", messages), ("responses", responses)] {
        let body = events
            .iter()
            .map(|event| {
                Ok(format!(
                    "event: {}\ndata: {}\n\n",
                    event["type"].as_str().context("event type")?,
                    serde_json::to_string(event)?
                ))
            })
            .collect::<Result<Vec<_>>>()?
            .concat();
        let harness = Harness::with_reply(protocol, StatusCode::OK, body, true, None).await?;
        let modes: &[bool] = if cfg!(feature = "http2") {
            &[false, true]
        } else {
            &[false]
        };
        for &h2 in modes {
            let client = Harness::client(h2)?;
            let response = harness.send(&client).send().await?;
            ensure!(response.status() == StatusCode::OK);
            ensure!(
                response.version()
                    == if h2 {
                        ::http::Version::HTTP_2
                    } else {
                        ::http::Version::HTTP_11
                    }
            );
            let id = execution_id(&response)?;
            let text = response.text().await?;
            ensure!(text.contains("receipt reply"), "{protocol}: {text}");
            let receipt = harness.receipt(&client, &id).await?;
            ensure!(receipt.outcome == Outcome::Completed && receipt.attempts.len() == 1);
            ensure!(
                receipt.usage.input_tokens == Some(if protocol == "messages" { 16 } else { 11 })
                    && receipt.usage.output_tokens == Some(7)
            );
            ensure!(
                receipt.usage.cache_read_tokens == Some(3)
                    && receipt.usage.cache_write_tokens == Some(2)
            );
            ensure!(receipt.usage.complete == (protocol == "responses"));
            ensure!(
                receipt.usage.reasoning_tokens
                    == if protocol == "responses" {
                        Some(4)
                    } else {
                        None
                    }
            );
        }
        ensure!(harness.calls.load(Ordering::SeqCst) == modes.len());
        harness.fixture.service.shutdown().await?;
    }
    Ok(())
}
