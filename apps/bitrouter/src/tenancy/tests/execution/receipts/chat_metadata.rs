//! Provider-shaped JSON/SSE through the real decoder and immutable receipt store.
use super::{
    text_tokens::{evidence, response},
    *,
};
use bitrouter_ai::tenancy::{
    execution::Outcome,
    usage_evidence::{Dimension, ExecutionUsageEvidence, Source},
    validation::Validate,
};

#[tokio::test]
async fn chat_metadata_json_sse_capture_preserves_facts_and_durable_refusals() -> Result<()> {
    for stream in [false, true] {
        for case in [
            "qualified",
            "bad_cost",
            "positive_creation",
            "null_creation",
            "populated_modality",
            "unknown",
        ] {
            let mut raw = json!({"prompt_tokens":417,"completion_tokens":6,"total_tokens":423,
                "prompt_tokens_details":{"cached_tokens":0,"created_cache_tokens":0,"multimodal_tokens":null},
                "completion_tokens_details":{"reasoning_tokens":0},"cost":0.00015264});
            match case {
                "bad_cost" => raw["cost"] = json!("0.00015264"),
                "positive_creation" => {
                    raw["prompt_tokens_details"]["created_cache_tokens"] = json!(1)
                }
                "null_creation" => {
                    raw["prompt_tokens_details"]["created_cache_tokens"] = Value::Null
                }
                "populated_modality" => {
                    raw["prompt_tokens_details"]["multimodal_tokens"] = json!({"image":0})
                }
                "unknown" => raw["unqualified_tokens"] = json!(0),
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
                ensure!(
                    receipt.usage.input_tokens == Some(417)
                        && receipt.usage.output_tokens == Some(6)
                );
                ensure!(receipt.usage.cache_write_tokens.is_none() && !receipt.usage.complete);
                ensure!(receipt.attempts[0].native_usage.len() == 4);
                let saved = evidence(&harness, &id).await?;
                let document: ExecutionUsageEvidence = serde_json::from_slice(&saved)?;
                document.validate()?;
                ensure!(
                    document.execution_id == receipt.execution_id
                        && document.extractor_version == 1
                );
                ensure!(document.attempts[0].terminal_observed);
                ensure!(document.frames.len() == 1);
                let frame = &document.frames[0];
                ensure!(frame.attempt_id == receipt.attempts[0].attempt_id);
                ensure!(
                    frame.source
                        == if stream {
                            Source::ChatChunk
                        } else {
                            Source::Json
                        }
                );
                ensure!(frame.invalid == (case == "bad_cost"));
                ensure!(frame.unsupported == !matches!(case, "qualified" | "bad_cost"));
                ensure!(frame.quantities.len() == 5);
                ensure!(
                    !frame
                        .quantities
                        .iter()
                        .any(|q| q.dimension == Dimension::CacheWriteTokens)
                );
                ensure!(
                    frame
                        .quantities
                        .iter()
                        .find(|q| q.dimension == Dimension::TotalTokens)
                        .and_then(|q| q.value)
                        == Some(423)
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
