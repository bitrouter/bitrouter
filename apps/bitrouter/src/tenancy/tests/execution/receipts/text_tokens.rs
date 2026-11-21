//! Anonymous provider-shaped JSON/SSE through decoding, capture and durable reads.
use super::*;
use bitrouter_ai::tenancy::{
    execution::Outcome,
    usage_evidence::{Dimension, ExecutionUsageEvidence, Source},
    validation::Validate,
};

fn usage() -> Value {
    json!({"prompt_tokens":11,"completion_tokens":7,"total_tokens":18,
        "prompt_tokens_details":{"cached_tokens":3,"text_tokens":11},
        "completion_tokens_details":{"reasoning_tokens":2}})
}

pub(super) fn response(usage: Value, stream: bool) -> Result<String> {
    if !stream {
        return Ok(serde_json::to_string(&json_response(
            "chat_completions",
            usage,
        )?)?);
    }
    let mut body = String::new();
    for (choices, usage) in [
        (
            json!([{"index":0,"delta":{"role":"assistant","content":"reply"},"finish_reason":null}]),
            Value::Null,
        ),
        (
            json!([{"index":0,"delta":{},"finish_reason":"stop"}]),
            Value::Null,
        ),
        (json!([]), usage),
    ] {
        let chunk = json!({"id":"chatcmpl-fixture","object":"chat.completion.chunk",
            "created":0,"model":"model-a","choices":choices,"usage":usage});
        body.push_str(&format!("data: {}\n\n", serde_json::to_string(&chunk)?));
    }
    body.push_str("data: [DONE]\n\n");
    Ok(body)
}

pub(super) async fn evidence(harness: &Harness, id: &ExecutionId) -> Result<Vec<u8>> {
    harness
        .fixture
        .service
        .read_receipt(
            harness.fixture.operator.clone(),
            harness.fixture.tenant.clone(),
            read::Request::UsageEvidence(id.clone()),
        )
        .await
        .map_err(failure)
}

#[tokio::test]
async fn text_tokens_json_sse_capture_exact_facts_and_keep_refusals_durable() -> Result<()> {
    for stream in [false, true] {
        for case in [
            "actual_prompt",
            "documented_both",
            "inconsistent_prompt",
            "inconsistent_completion",
            "unknown",
        ] {
            let mut raw = usage();
            match case {
                "documented_both" => raw["completion_tokens_details"]["text_tokens"] = json!(7),
                "inconsistent_prompt" => raw["prompt_tokens_details"]["text_tokens"] = json!(10),
                "inconsistent_completion" => {
                    raw["completion_tokens_details"]["text_tokens"] = json!(5)
                }
                "unknown" => raw["prompt_tokens_details"]["unqualified_tokens"] = json!(1),
                _ => {}
            }
            let harness = Harness::with_reply(
                "chat_completions",
                StatusCode::OK,
                response(raw, stream)?,
                stream,
                None,
            )
            .await?;
            let result: Result<()> = async {
                let client = Harness::client(false)?;
                let response = harness.send(&client).send().await?;
                ensure!(response.status() == StatusCode::OK);
                let id = execution_id(&response)?;
                let body = response.text().await?;
                ensure!(!stream || body.contains("[DONE]"));
                let receipt = harness.receipt(&client, &id).await?;
                ensure!(receipt.outcome == Outcome::Completed && receipt.attempts.len() == 1);
                ensure!(receipt.usage.input_tokens == Some(11));
                ensure!(receipt.usage.output_tokens == Some(7));
                ensure!(receipt.usage.cache_write_tokens.is_none() && !receipt.usage.complete);
                ensure!(receipt.attempts[0].native_usage.len() == 4);
                let saved = evidence(&harness, &id).await?;
                let document: ExecutionUsageEvidence = serde_json::from_slice(&saved)?;
                document.validate()?;
                ensure!(document.execution_id == receipt.execution_id);
                ensure!(document.attempts[0].attempt_id == receipt.attempts[0].attempt_id);
                ensure!(document.extractor_version == 1 && document.frames.len() == 1);
                let frame = &document.frames[0];
                ensure!(
                    frame.source
                        == if stream {
                            Source::ChatChunk
                        } else {
                            Source::Json
                        }
                );
                ensure!(frame.invalid == case.starts_with("inconsistent_"));
                ensure!(frame.unsupported == (case == "unknown"));
                ensure!(frame.quantities.len() == 5);
                ensure!(
                    frame
                        .quantities
                        .iter()
                        .filter(|q| q.dimension == Dimension::InputTokens)
                        .map(|q| q.value)
                        .collect::<Vec<_>>()
                        == vec![Some(11)]
                );
                ensure!(evidence(&harness, &id).await? == saved);
                ensure!(harness.calls.load(Ordering::SeqCst) == 1);
                Ok(())
            }
            .await;
            let stopped = harness.fixture.service.shutdown().await;
            result?;
            stopped?;
        }
    }
    Ok(())
}
