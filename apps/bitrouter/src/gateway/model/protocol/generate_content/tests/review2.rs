//! Additional independent-review boundaries: executable turns, continuity, and
//! diagnostic snapshots reaching the real usage accumulator.
use super::*;
use crate::gateway::pipeline::stream::UsageAccumulator;

fn native_call() -> Value {
    json!({"functionCall":{"id":"a","name":"lookup","args":{}}})
}

// UT: Incomplete native JSON calls never escape into canonical consumers. Keep
// the actual function-call data in the refused raw object and retain valid usage.
#[test]
fn ut_incomplete_json_function_calls_refuse_before_projection() -> Result<()> {
    for reason in [
        "MAX_TOKENS",
        "SAFETY",
        "RECITATION",
        "MALFORMED_FUNCTION_CALL",
    ] {
        let mut body = reply(json!([native_call()]));
        body["candidates"][0]["finishReason"] = json!(reason);
        body["usageMetadata"] = json!({"promptTokenCount":11,"candidatesTokenCount":3});
        let error = GenerateContentAdapter
            .parse_response(body.clone())
            .err()
            .context("incomplete call must refuse")?;
        assert!(
            matches!(error,ModelError::InvalidResponse { usage:Some(usage), .. } if usage.prompt_tokens==11 && usage.completion_tokens==3)
        );
        assert_eq!(body["candidates"][0]["content"]["parts"][0], native_call());
        let mut decoder = GenerateContentAdapter.stream_decoder();
        let parts = decoder.decode(&event(body))?;
        assert!(
            !parts
                .iter()
                .any(|part| matches!(part, StreamPart::ToolCallDelta { .. }))
        );
        assert!(decoder.finish().is_err());
    }
    let mut body = reply(json!([{"text":"valid truncated text"}]));
    body["candidates"][0]["finishReason"] = json!("MAX_TOKENS");
    let result = GenerateContentAdapter.parse_response(body)?;
    assert_eq!(result.finish_reason, Some(FinishReason::Length));
    assert!(
        matches!(&result.content[0],Content::Text { text, .. } if text=="valid truncated text")
    );
    Ok(())
}

// UT: Exercise the actual production tool-classification boundary. The complete
// STOP control is executable; the incomplete input cannot reach that boundary.
#[cfg(feature = "server-tools")]
#[test]
fn ut_json_call_guard_protects_production_tool_classifier() -> Result<()> {
    use crate::gateway::pipeline::server_tools::classify::{TurnDisposition, classify_turn};
    let owned = std::collections::BTreeSet::from(["lookup".to_owned()]);
    let complete = GenerateContentAdapter.parse_response(reply(json!([native_call()])))?;
    assert!(
        matches!(classify_turn(&complete.content,&owned),TurnDisposition::Execute(calls)
        if calls.len()==1 && calls[0].id=="a" && calls[0].name=="lookup" && calls[0].arguments=="{}")
    );
    let mut raw = reply(json!([native_call()]));
    raw["candidates"][0]["finishReason"] = json!("MAX_TOKENS");
    assert!(GenerateContentAdapter.parse_response(raw).is_err());
    Ok(())
}

// UT: All nine signed plain/empty-text/file response conversions refuse before
// continuity is erased, while native JSON and existing Chat tool metadata survive.
#[test]
fn ut_signed_json_output_refuses_ordinary_encoders() -> Result<()> {
    let prompt = request(minimal(), false)?;
    for part in [
        json!({"text":"signed","thoughtSignature":"proof"}),
        json!({"text":"","thoughtSignature":"proof"}),
        json!({"inlineData":{"mimeType":"image/png","data":"AA=="},"thoughtSignature":"proof"}),
    ] {
        let result = GenerateContentAdapter.parse_response(reply(json!([part.clone()])))?;
        assert_eq!(
            GenerateContentAdapter.render_response(&result, &prompt, "r")?["candidates"][0]["content"]
                ["parts"][0],
            part
        );
        for target in [
            &ChatCompletionsAdapter as &dyn InboundAdapter,
            &MessagesAdapter,
            &ResponsesAdapter,
        ] {
            assert!(matches!(
                target.render_response(&result, &prompt, "r"),
                Err(ModelError::Incompatible { .. })
            ));
        }
    }
    let mut call = native_call();
    call["thoughtSignature"] = json!("proof");
    let result = GenerateContentAdapter.parse_response(reply(json!([call])))?;
    let chat = ChatCompletionsAdapter.render_response(&result, &prompt, "r")?;
    assert!(chat.to_string().contains("proof"));
    Ok(())
}

// UT: Rejected error/refusal snapshots pass the same cumulative checks as normal
// events before they can enter the real unpriced accumulator. Increasing/equal
// diagnostics remain available, rather than suppressing all error usage.
#[test]
fn ut_all_diagnostic_usage_paths_preserve_monotonic_accumulation() -> Result<()> {
    for failure in [
        json!({"error":{"code":429}}),
        json!({"promptFeedback":{"blockReason":"SAFETY"}}),
        json!({"candidates":[{"index":0,"error":{"code":429}}]}),
        json!({"candidates":[{"index":0,"content":{"parts":42}}]}),
    ] {
        for count in [1, 3, 5] {
            let mut decoder = GenerateContentAdapter.stream_decoder();
            let first=decoder.decode(&event(json!({"responseId":"r","usageMetadata":{"promptTokenCount":11,"candidatesTokenCount":3}})))?;
            let mut accumulator = UsageAccumulator::with_prompt_chars(0);
            for part in &first {
                accumulator.observe(part);
            }
            let mut body = failure.clone();
            body["responseId"] = json!("r");
            body["usageMetadata"] = json!({"promptTokenCount":11,"candidatesTokenCount":count});
            let error = decoder
                .decode(&event(body.clone()))
                .err()
                .context("expected diagnostic refusal")?;
            let ModelError::InvalidResponse { usage, .. } = error else {
                anyhow::bail!("unexpected error category")
            };
            if count < 3 {
                assert!(usage.is_none());
            } else {
                let usage = usage
                    .as_ref()
                    .context("qualified nondecreasing error usage")?;
                assert_eq!(usage.raw.as_deref(), Some(&body["usageMetadata"]));
            }
            if let Some(usage) = usage {
                accumulator.observe(&StreamPart::Usage { usage: *usage });
            }
            let final_usage = accumulator.finalized().context("prior usage retained")?;
            assert_eq!(final_usage.prompt_tokens, 11);
            assert_eq!(
                final_usage.completion_tokens,
                if count < 3 { 3 } else { count }
            );
            assert!(decoder.finish().is_err());
        }
    }
    Ok(())
}
