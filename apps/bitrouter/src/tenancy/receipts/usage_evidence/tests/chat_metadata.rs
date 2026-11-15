use super::{text_tokens::capture, *};

fn usage() -> Value {
    json!({"prompt_tokens":417,"completion_tokens":6,"total_tokens":423,
        "prompt_tokens_details":{"cached_tokens":0,"created_cache_tokens":0,"multimodal_tokens":null},
        "completion_tokens_details":{"reasoning_tokens":0},"cost":0.00015264})
}

#[test]
fn chat_metadata_json_sse_preserve_quantities_without_new_dimensions() -> Result<()> {
    for source in [Source::Json, Source::ChatChunk, Source::HttpError] {
        let frame = capture(&usage(), source)?;
        ensure!(!frame.invalid && !frame.unsupported);
        ensure!(frame.source == source && frame.semantics == Semantics::Snapshot);
        ensure!(
            serde_json::to_value(&frame.quantities)?
                == json!([
                    {"dimension":"input_tokens","value":417},
                    {"dimension":"output_tokens","value":6},
                    {"dimension":"total_tokens","value":423},
                    {"dimension":"cache_read_tokens","value":0},
                    {"dimension":"reasoning_tokens","value":0}
                ])
        );
        let serialized = serde_json::to_string(&frame)?;
        for omitted in [
            "cost",
            "multimodal_tokens",
            "created_cache_tokens",
            "0.00015264",
        ] {
            ensure!(!serialized.contains(omitted));
        }
        for cost in [json!(0), json!(0.0), json!(0.25), json!(1e300)] {
            let mut raw = usage();
            raw["cost"] = cost;
            ensure!(serde_json::to_value(capture(&raw, source)?)? == serde_json::to_value(&frame)?);
        }
        let mut absent = usage();
        absent.as_object_mut().context("usage")?.remove("cost");
        absent["prompt_tokens_details"]
            .as_object_mut()
            .context("details")?
            .remove("multimodal_tokens");
        ensure!(serde_json::to_value(capture(&absent, source)?)? == serde_json::to_value(&frame)?);
        absent["prompt_tokens_details"]
            .as_object_mut()
            .context("details")?
            .remove("created_cache_tokens");
        let missing = capture(&absent, source)?;
        ensure!(serde_json::to_value(&missing)? == serde_json::to_value(&frame)?);
        ensure!(!missing.unsupported && !missing.invalid);
        ensure!(missing.quantities.len() == 5);
        ensure!(
            !missing
                .quantities
                .iter()
                .any(|q| q.dimension == Dimension::CacheWriteTokens)
        );
    }
    Ok(())
}

#[test]
fn chat_metadata_malformed_cost_and_unqualified_details_fail_closed() -> Result<()> {
    for source in [Source::Json, Source::ChatChunk] {
        for cost in [
            json!(null),
            json!(-1),
            json!(-0.1),
            json!("0.1"),
            json!(true),
            json!([]),
            json!({}),
        ] {
            let mut raw = usage();
            raw["cost"] = cost;
            let frame = capture(&raw, source)?;
            ensure!(frame.invalid && frame.quantities[0].value == Some(417));
        }
        for value in [
            json!(1),
            json!(AuthorityRevision::MAX),
            json!(u64::MAX),
            json!(-1),
            json!(0.0),
            json!(0.5),
            json!("0"),
            json!(null),
            json!(false),
            json!([]),
            json!({}),
        ] {
            let mut raw = usage();
            raw["prompt_tokens_details"]["created_cache_tokens"] = value;
            let frame = capture(&raw, source)?;
            ensure!(frame.unsupported);
            ensure!(
                !frame
                    .quantities
                    .iter()
                    .any(|q| q.dimension == Dimension::CacheWriteTokens)
            );
        }
        for value in [
            json!({}),
            json!({"image":0}),
            json!({"image":1}),
            json!({"unknown":0}),
            json!([]),
            json!(0),
            json!(false),
            json!("null"),
        ] {
            let mut raw = usage();
            raw["prompt_tokens_details"]["multimodal_tokens"] = value;
            ensure!(capture(&raw, source)?.unsupported);
        }
    }
    Ok(())
}

#[test]
fn chat_metadata_exact_paths_do_not_accept_other_extensions_or_protocols() -> Result<()> {
    for (container, name, value) in [
        ("prompt_tokens_details", "cost", json!(0)),
        ("completion_tokens_details", "cost", json!(0)),
        ("root", "created_cache_tokens", json!(0)),
        (
            "completion_tokens_details",
            "created_cache_tokens",
            json!(0),
        ),
        ("root", "multimodal_tokens", Value::Null),
        (
            "completion_tokens_details",
            "multimodal_tokens",
            Value::Null,
        ),
        ("root", "unqualified", json!(0)),
        ("prompt_tokens_details", "unqualified", json!(0)),
        (
            "completion_tokens_details",
            "accepted_prediction_tokens",
            json!(0),
        ),
    ] {
        let mut raw = usage();
        let object = if container == "root" {
            &mut raw
        } else {
            &mut raw[container]
        };
        object[name] = value;
        for source in [Source::Json, Source::ChatChunk] {
            ensure!(
                capture(&raw, source)?.unsupported,
                "accepted {container}.{name}"
            );
        }
    }
    for protocol in [
        Protocol::Responses,
        Protocol::Messages,
        Protocol::Decisions,
        Protocol::GenerateContent,
    ] {
        let mut snapshot = Snapshot::default();
        snapshot.observe(
            &"attempt".try_into()?,
            protocol,
            Source::Json,
            &json!({"cost":0}),
        );
        ensure!(snapshot.frames[0].unsupported);
    }
    Ok(())
}
