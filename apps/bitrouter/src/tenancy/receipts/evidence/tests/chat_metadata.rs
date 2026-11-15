use super::*;
use anyhow::Context;

fn raw() -> Value {
    json!({"prompt_tokens":417,"completion_tokens":6,"total_tokens":423,
        "prompt_tokens_details":{"cached_tokens":0,"created_cache_tokens":0,"multimodal_tokens":null},
        "completion_tokens_details":{"reasoning_tokens":0},"cost":0.00015264})
}

#[test]
fn chat_metadata_projection_preserves_absence_without_advisory_cost() -> Result<()> {
    let mut evidence = Evidence::default();
    for _ in 0..3 {
        evidence.observe(Protocol::ChatCompletions, &raw());
    }
    let projected = evidence.project();
    ensure!(!evidence.incomplete && !projected.usage.complete);
    ensure!(projected.usage.input_tokens == Some(417));
    ensure!(projected.usage.output_tokens == Some(6));
    ensure!(projected.usage.cache_write_tokens.is_none());
    ensure!(projected.native_usage.len() == 4);
    projected.usage.validate()?;
    let mut absent = raw();
    absent["prompt_tokens_details"]
        .as_object_mut()
        .context("details")?
        .remove("created_cache_tokens");
    let mut evidence = Evidence::default();
    evidence.observe(Protocol::ChatCompletions, &absent);
    ensure!(evidence.project().usage.cache_write_tokens.is_none());
    ensure!(!evidence.project().usage.complete);
    Ok(())
}

#[test]
fn chat_metadata_projection_refusals_are_sticky_without_new_cache_semantics() -> Result<()> {
    for (path, bad) in [
        ("/cost", json!(-1)),
        ("/cost", json!("0")),
        ("/cost", Value::Null),
        ("/prompt_tokens_details/created_cache_tokens", json!(1)),
        ("/prompt_tokens_details/created_cache_tokens", json!(0.0)),
        ("/prompt_tokens_details/created_cache_tokens", Value::Null),
        (
            "/prompt_tokens_details/multimodal_tokens",
            json!({"image":0}),
        ),
    ] {
        let mut invalid = raw();
        *invalid.pointer_mut(path).context("fixture pointer")? = bad;
        let mut evidence = Evidence::default();
        evidence.observe(Protocol::ChatCompletions, &invalid);
        evidence.observe(Protocol::ChatCompletions, &raw());
        ensure!(
            evidence.incomplete && !evidence.project().usage.complete,
            "accepted {path}"
        );
        ensure!(evidence.project().usage.input_tokens == Some(417));
    }
    for cache_write in [json!(0), json!(1)] {
        let mut value = raw();
        value["prompt_tokens_details"]["cache_write_tokens"] = cache_write.clone();
        let mut evidence = Evidence::default();
        evidence.observe(Protocol::ChatCompletions, &value);
        ensure!(evidence.project().usage.complete);
        ensure!(evidence.project().usage.cache_write_tokens == cache_write.as_u64());
    }
    Ok(())
}
