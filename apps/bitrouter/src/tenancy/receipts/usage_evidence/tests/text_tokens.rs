use super::*;

// Anonymous counters with the observed provider JSON/final SSE usage shape.
fn usage() -> Value {
    json!({
        "prompt_tokens":11,"completion_tokens":7,"total_tokens":18,
        "prompt_tokens_details":{"cached_tokens":3,"text_tokens":11},
        "completion_tokens_details":{"reasoning_tokens":2}
    })
}

fn both_details() -> Value {
    let mut raw = usage();
    // The documented output text count includes reasoning, not just visible text.
    raw["completion_tokens_details"]["text_tokens"] = json!(7);
    raw
}

pub(super) fn capture(raw: &Value, source: Source) -> Result<Frame> {
    let mut snapshot = Snapshot::default();
    let id = "attempt".try_into()?;
    if source == Source::ChatChunk {
        snapshot.observe(&id, Protocol::ChatCompletions, source, &Value::Null);
        ensure!(snapshot.frames.is_empty() && snapshot.dropped == 0);
    }
    snapshot.observe(&id, Protocol::ChatCompletions, source, raw);
    ensure!(snapshot.frames.len() == 1);
    snapshot.frames.pop().context("observed frame")
}

#[test]
fn text_tokens_json_and_sse_keep_original_quantities_without_double_counting() -> Result<()> {
    for source in [Source::Json, Source::ChatChunk, Source::HttpError] {
        let frame = capture(&usage(), source)?;
        ensure!(!frame.unsupported && !frame.invalid);
        ensure!(frame.semantics == Semantics::Snapshot && frame.source == source);
        ensure!(
            serde_json::to_value(&frame.quantities)?
                == json!([
                    {"dimension":"input_tokens","value":11},
                    {"dimension":"output_tokens","value":7},
                    {"dimension":"total_tokens","value":18},
                    {"dimension":"cache_read_tokens","value":3},
                    {"dimension":"reasoning_tokens","value":2}
                ])
        );
        ensure!(!serde_json::to_string(&frame)?.contains("text_tokens"));
        let both = capture(&both_details(), source)?;
        ensure!(serde_json::to_value(&both)? == serde_json::to_value(&frame)?);
        let mut absent = both_details();
        for details in ["prompt_tokens_details", "completion_tokens_details"] {
            absent[details]
                .as_object_mut()
                .context("details object")?
                .remove("text_tokens");
            ensure!(
                serde_json::to_value(capture(&absent, source)?)? == serde_json::to_value(&frame)?
            );
        }
        ensure!(
            ExecutionUsageEvidence::extractor_version_for(
                bitrouter_ai::tenancy::ordered::GenerationProtocol::ChatCompletions,
                &[frame],
            ) == 1
        );
    }
    for value in [0, AuthorityRevision::MAX] {
        let raw = json!({"prompt_tokens":value,"completion_tokens":value,
            "prompt_tokens_details":{"text_tokens":value,"cached_tokens":value},
            "completion_tokens_details":{"text_tokens":value,"reasoning_tokens":value}});
        let frame = capture(&raw, Source::Json)?;
        ensure!(!frame.invalid && !frame.unsupported);
        ensure!(frame.quantities[0].value == Some(value));
    }
    Ok(())
}

#[test]
fn text_tokens_invalid_types_ranges_and_relationships_fail_closed() -> Result<()> {
    for source in [Source::Json, Source::ChatChunk] {
        for bad in [
            json!(-1),
            json!(11.0),
            json!(11.5),
            json!("11"),
            Value::Null,
            json!(true),
            json!([]),
            json!({}),
            json!(AuthorityRevision::MAX + 1),
            json!(u64::MAX),
            json!(10),
            json!(12),
        ] {
            for details in ["prompt_tokens_details", "completion_tokens_details"] {
                let mut raw = both_details();
                raw[details]["text_tokens"] = bad.clone();
                let frame = capture(&raw, source)?;
                ensure!(
                    frame.invalid,
                    "accepted {details} malformed/inconsistent text counter: {bad}"
                );
                ensure!(frame.quantities[0].value == Some(11));
            }
        }
        for (pointer, bad) in [
            ("/prompt_tokens", Value::Null),
            ("/prompt_tokens", json!(11.0)),
            ("/prompt_tokens", json!(AuthorityRevision::MAX + 1)),
            ("/prompt_tokens_details/cached_tokens", json!(12)),
            ("/prompt_tokens_details/cached_tokens", json!(-1)),
            ("/prompt_tokens_details/cached_tokens", json!("3")),
            ("/completion_tokens", Value::Null),
            ("/completion_tokens", json!(7.0)),
            ("/completion_tokens", json!(AuthorityRevision::MAX + 1)),
            ("/completion_tokens_details/reasoning_tokens", json!(8)),
            ("/completion_tokens_details/reasoning_tokens", json!(-1)),
            ("/completion_tokens_details/reasoning_tokens", json!("2")),
        ] {
            let mut raw = both_details();
            *raw.pointer_mut(pointer).context("fixture pointer")? = bad;
            ensure!(capture(&raw, source)?.invalid, "accepted {pointer}");
        }
        for total in ["prompt_tokens", "completion_tokens"] {
            let mut raw = both_details();
            raw.as_object_mut().context("usage object")?.remove(total);
            ensure!(capture(&raw, source)?.invalid);
        }
        for details in ["prompt_tokens_details", "completion_tokens_details"] {
            for modality in ["audio_tokens", "image_tokens"] {
                for bad in [json!(1), json!(-1), json!("0"), Value::Null] {
                    let mut raw = both_details();
                    raw[details][modality] = bad;
                    ensure!(capture(&raw, source)?.invalid);
                }
            }
        }
    }
    Ok(())
}

#[test]
fn text_tokens_does_not_whitelist_other_paths_or_extensions() -> Result<()> {
    for path in ["root", "prompt_tokens_details", "completion_tokens_details"] {
        let mut raw = usage();
        let object = if path == "root" {
            &mut raw
        } else {
            &mut raw[path]
        };
        object["unqualified_tokens"] = json!({"private":"never-retained"});
        let frame = capture(&raw, Source::Json)?;
        ensure!(frame.unsupported && !frame.invalid);
        ensure!(!serde_json::to_string(&frame)?.contains("never-retained"));
    }
    let mut raw = usage();
    raw["text_tokens"] = json!(11);
    ensure!(capture(&raw, Source::Json)?.unsupported);
    let mut snapshot = Snapshot::default();
    snapshot.observe(
        &"attempt".try_into()?,
        Protocol::Responses,
        Source::Json,
        &json!({"input_tokens":11,"input_tokens_details":{"text_tokens":11}}),
    );
    ensure!(snapshot.frames[0].unsupported);
    Ok(())
}
