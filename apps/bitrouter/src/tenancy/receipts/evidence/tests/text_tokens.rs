use super::*;

fn raw() -> Value {
    json!({"prompt_tokens":11,"completion_tokens":7,"total_tokens":18,
        "prompt_tokens_details":{"cached_tokens":3,"text_tokens":11},
        "completion_tokens_details":{"reasoning_tokens":2,"text_tokens":7}})
}

#[test]
fn text_tokens_redundancy_preserves_absence_and_repeated_snapshots() -> Result<()> {
    let mut evidence = Evidence::default();
    for _ in 0..3 {
        evidence.observe(Protocol::ChatCompletions, &raw());
    }
    ensure!(!evidence.incomplete);
    let projected = evidence.project();
    ensure!(projected.usage.input_tokens == Some(11));
    ensure!(projected.usage.output_tokens == Some(7));
    ensure!(projected.usage.cache_read_tokens == Some(3));
    ensure!(projected.usage.reasoning_tokens == Some(2));
    ensure!(projected.usage.cache_write_tokens.is_none() && !projected.usage.complete);
    ensure!(projected.native_usage.len() == 4);
    projected.usage.validate()?;
    // Completeness still requires each existing dimension, never an implicit zero.
    let mut complete = raw();
    complete["prompt_tokens_details"]["cache_write_tokens"] = json!(0);
    evidence.observe(Protocol::ChatCompletions, &complete);
    ensure!(evidence.project().usage.complete);
    Ok(())
}

#[test]
fn text_tokens_invalid_or_unknown_details_latch_incompleteness() -> Result<()> {
    for bad in [
        json!(null),
        json!(-1),
        json!(11.0),
        json!("11"),
        json!(true),
        json!([]),
        json!({}),
        json!(AuthorityRevision::MAX + 1),
        json!(10),
        json!(12),
    ] {
        for details in ["prompt_tokens_details", "completion_tokens_details"] {
            let mut invalid = raw();
            invalid[details]["text_tokens"] = bad.clone();
            let mut evidence = Evidence::default();
            evidence.observe(Protocol::ChatCompletions, &invalid);
            evidence.observe(Protocol::ChatCompletions, &raw());
            ensure!(evidence.incomplete && !evidence.project().usage.complete);
            ensure!(evidence.project().usage.input_tokens == Some(11));
        }
    }
    for field in ["unqualified_tokens", "audio_tokens", "image_tokens"] {
        let mut unsupported = raw();
        unsupported["prompt_tokens_details"][field] = json!(0);
        let mut evidence = Evidence::default();
        evidence.observe(Protocol::ChatCompletions, &unsupported);
        ensure!(
            evidence.incomplete,
            "existing projection guard lost: {field}"
        );
    }
    let mut evidence = Evidence::default();
    evidence.observe(
        Protocol::Responses,
        &json!({"input_tokens":11,"input_tokens_details":{"text_tokens":11}}),
    );
    ensure!(evidence.incomplete);
    Ok(())
}
