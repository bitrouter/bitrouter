//! Positive counterparts of independently reproduced malformed/unsafe inputs.
use super::*;
use crate::gateway::model::client::ModelClient;

// UT: Invalid discriminated parts and partial argument previews never become executable content.
#[test]
fn ut_unknown_parts_and_partial_preview_refused() -> Result<()> {
    for part in [
        json!(42),
        json!({"unrecognized":{"x":1}}),
        json!({"text":7}),
        json!({"text":"a","fileData":{}}),
        json!({"functionCall":{"name":"run","partialArgs":[{"jsonPath":"$.x","numberValue":7}],"willContinue":true}}),
        json!({"functionCall":{"name":"run","args":{},"willContinue":true}}),
    ] {
        assert!(request(json!({"contents":[{"parts":[part]}]}), false).is_err());
        assert!(
            GenerateContentAdapter
                .parse_response(reply(json!([part])))
                .is_err()
        );
    }
    assert!(request(json!({"contents":[{"parts":[],"unknown":true}]}), false).is_err());
    assert!(request(json!({"systemInstruction":{"parts":[{"text":"x","thoughtSignature":"proof"}]},"contents":[{"parts":[{"text":"q"}]}]}), false).is_err());
    assert!(request(json!({"contents":[{"parts":[{"text":"q"}]}],"tools":[{"functionDeclarations":[{"name":"f","unknownConstraint":true}]}]}), false).is_err());
    Ok(())
}

// UT: Explicit IDs (including name-equal IDs) and original omission round-trip independently.
#[test]
fn ut_native_call_result_identity_and_omission() -> Result<()> {
    for id in [Some("a"), Some("lookup"), None] {
        let mut call = json!({"functionCall":{"name":"lookup","args":{}}});
        let mut result = json!({"functionResponse":{"name":"lookup","response":{}}});
        if let Some(id) = id {
            call["functionCall"]["id"] = id.into();
            result["functionResponse"]["id"] = id.into();
        }
        let prompt = request(
            json!({"contents":[{"role":"model","parts":[call]},{"role":"user","parts":[result]}]}),
            false,
        )?;
        let wire = GenerateContentAdapter.render_request(&prompt)?;
        assert_eq!(wire["contents"][0]["parts"][0], call);
        assert_eq!(wire["contents"][1]["parts"][0], result);
        let Content::ToolCall { id: canonical, .. } = &prompt.messages[0].content[0] else {
            anyhow::bail!("missing call")
        };
        assert!(!canonical.is_empty());
        assert!(
            matches!(&prompt.messages[1].content[0], Content::ToolResult { call_id, .. } if call_id == canonical)
        );
    }
    let duplicate = json!([{"functionCall":{"id":"a","name":"f","args":{}}},{"functionCall":{"id":"a","name":"g","args":{}}}]);
    assert!(
        GenerateContentAdapter
            .parse_response(reply(duplicate))
            .is_err()
    );
    let unbound = request(
        json!({"contents":[{"parts":[{"functionResponse":{"id":"absent","name":"f","response":{}}}]}]}),
        false,
    )?;
    for target in targets() {
        assert!(target.render_request(&unbound).is_err());
    }
    Ok(())
}

// UT: Cache context, restricted ANY and native extensions cannot disappear on foreign dispatch.
#[test]
fn ut_native_constraints_preserved_or_target_refused() -> Result<()> {
    for extra in [
        json!({"cachedContent":"cachedContents/required"}),
        json!({"safetySettings":[]}),
        json!({"toolConfig":{"functionCallingConfig":{"mode":"ANY","allowedFunctionNames":["lookup","clock"]}}}),
        json!({"toolConfig":{"functionCallingConfig":{"mode":"ANY","allowedFunctionNames":[7]}}}),
        json!({"toolConfig":{"functionCallingConfig":{"mode":"ANY","streamFunctionCallArguments":true}}}),
        json!({"generationConfig":{"responseModalities":["TEXT","UNKNOWN"]}}),
    ] {
        let mut body = minimal();
        for (key, value) in extra.as_object().context("extras")? {
            body[key] = value.clone();
        }
        let prompt = request(body, false)?;
        let rendered = GenerateContentAdapter.render_request(&prompt)?;
        for (key, value) in extra.as_object().context("extras")? {
            assert_eq!(&rendered[key], value);
        }
        for target in targets() {
            assert!(!target.admission(&prompt).issues.is_empty());
            assert!(target.render_request(&prompt).is_err());
        }
    }
    Ok(())
}

// UT: Source-native null suppresses a native default but does not authorize dropping a foreign default.
#[test]
fn ut_cache_null_and_supplemental_scope() -> Result<()> {
    let mut body = minimal();
    body["cachedContent"] = Value::Null;
    let mut prompt = request(body, false)?;
    prompt
        .params
        .supplemental_extra
        .insert("cachedContent".into(), json!("cachedContents/default"));
    assert_eq!(
        GenerateContentAdapter.render_request(&prompt)?["cachedContent"],
        Value::Null
    );
    for target in targets() {
        assert!(target.render_request(&prompt).is_err());
    }
    prompt.params.supplemental_extra.clear();
    for target in targets() {
        target.render_request(&prompt)?;
    }
    let mut prompt = request(minimal(), false)?;
    prompt
        .params
        .supplemental_extra
        .insert("cachedContent".into(), json!("cachedContents/default"));
    assert_eq!(
        GenerateContentAdapter.render_request(&prompt)?["cachedContent"],
        "cachedContents/default"
    );
    for target in targets() {
        assert!(target.render_request(&prompt).is_err());
    }
    Ok(())
}

// UT: Invalid envelope/candidate/termination never becomes successful STOP; no reported usage stays absent.
#[test]
fn ut_json_shape_terminal_and_missing_usage() -> Result<()> {
    for body in [
        json!({"candidates":[null]}),
        json!({"error":{"code":429},"candidates":[{"finishReason":"STOP"}]}),
        json!({"candidates":[{}]}),
        json!({"candidates":[{"content":{"parts":42},"finishReason":"STOP"}]}),
        json!({"candidates":[{"finishReason":"FINISH_REASON_UNSPECIFIED"}]}),
        json!({"candidates":[{"finishReason":7}]}),
    ] {
        assert!(GenerateContentAdapter.parse_response(body).is_err());
    }
    let prompt = request(minimal(), false)?;
    let mut result = GenerateContentAdapter.parse_response(reply(json!([{"text":"answer"}])))?;
    assert!(result.usage.is_none());
    assert!(
        GenerateContentAdapter
            .render_response(&result, &prompt, "r")?
            .get("usageMetadata")
            .is_none()
    );
    result.finish_reason = None;
    assert!(
        GenerateContentAdapter
            .render_response(&result, &prompt, "r")
            .is_err()
    );
    Ok(())
}

// UT: Provider execution custody and foreign custom tool identities cannot become client function calls.
#[test]
fn ut_provider_executed_json_and_stream_refused() -> Result<()> {
    let prompt = request(minimal(), false)?;
    let server = ResponsesAdapter.parse_response(json!({"id":"r","status":"completed","output":[{"type":"web_search_call","id":"s","status":"completed"}]}))?;
    assert!(matches!(
        &server.content[0],
        Content::ToolCall {
            provider_executed: true,
            ..
        }
    ));
    assert!(
        GenerateContentAdapter
            .render_response(&server, &prompt, "r")
            .is_err()
    );
    for part in [
        StreamPart::ServerToolCall {
            id: "s".into(),
            name: "web_search".into(),
            arguments: "{}".into(),
            server_name: None,
            dynamic: false,
        },
        StreamPart::ServerToolResult {
            call_id: "s".into(),
            tool_name: Some("web_search".into()),
            output: ToolResultOutput::Json { value: json!({}) },
            dynamic: false,
        },
    ] {
        let mut encoder = GenerateContentAdapter.stream_encoder("r", "m");
        assert!(encoder.encode(&part).is_err());
    }
    let mut result = GenerateContentAdapter.parse_response(reply(
        json!([{"functionCall":{"name":"f","id":"a","args":{}}}]),
    ))?;
    if let Content::ToolCall { dynamic, .. } = &mut result.content[0] {
        *dynamic = true;
    }
    assert!(
        GenerateContentAdapter
            .render_response(&result, &prompt, "r")
            .is_err()
    );
    Ok(())
}

fn report() -> Value {
    json!({"promptTokenCount":11,"candidatesTokenCount":3})
}
fn has_usage(error: ModelError) -> bool {
    matches!(error, ModelError::InvalidResponse { usage:Some(usage), .. } if usage.prompt_tokens == 11 && usage.completion_tokens == 3)
}

// UT: Diagnostic usage survives provider refusals, parse errors and the actual model-client wrapper.
#[test]
fn ut_error_usage_retained_through_client_and_stream() -> Result<()> {
    for failure in [
        json!({"error":{"code":429}}),
        json!({"promptFeedback":{"blockReason":"SAFETY"}}),
    ] {
        let mut body = failure;
        body["usageMetadata"] = report();
        assert!(has_usage(
            GenerateContentAdapter
                .parse_response(body.clone())
                .err()
                .context("expected refusal")?
        ));
        assert!(has_usage(
            ModelClient::parse_response(&GenerateContentAdapter, &protocol(), &body.to_string())
                .err()
                .context("client refusal")?
        ));
        let mut decoder = GenerateContentAdapter.stream_decoder();
        decoder.decode(&event(json!({"candidates":[{"finishReason":"STOP"}]})))?;
        assert!(matches!(
            decoder.pending_terminal(),
            Some(StreamPart::Finish {
                reason: FinishReason::Stop
            })
        ));
        assert!(has_usage(
            decoder
                .decode(&event(body))
                .err()
                .context("stream refusal")?
        ));
        assert!(decoder.finish().is_err());
    }
    Ok(())
}

// UT: Nonterminal usage is independent; identity switches cannot admit tail facts or successful EOF.
#[test]
fn ut_nonterminal_usage_and_stream_identity_binding() -> Result<()> {
    for switched in [
        json!({"responseId":"r","candidates":[{"index":1,"finishReason":"STOP"}],"usageMetadata":report()}),
        json!({"responseId":"other","candidates":[{"index":0,"finishReason":"STOP"}],"usageMetadata":report()}),
    ] {
        let mut decoder = GenerateContentAdapter.stream_decoder();
        let parts = decoder.decode(&event(json!({"responseId":"r","candidates":[{"index":0,"content":{"parts":[{"text":"a"}]}}],"usageMetadata":report()})))?;
        assert!(parts.iter().any(|part| matches!(part,StreamPart::Usage { usage } if usage.raw.as_deref() == Some(&report()))));
        assert!(!parts.iter().any(StreamPart::is_terminal));
        let error = decoder
            .decode(&event(switched))
            .err()
            .context("identity refusal")?;
        assert!(matches!(
            error,
            ModelError::InvalidResponse { usage: None, .. }
        ));
        assert!(decoder.finish().is_err());
    }
    let mut decoder = GenerateContentAdapter.stream_decoder();
    assert!(
        decoder
            .decode(&event(json!({"candidates":[{"index":0},{"index":1}]})))
            .is_err()
    );
    Ok(())
}

// UT: Native call/signature remains buffered through true source completion, then precedes later text.
#[test]
fn ut_native_stream_call_order_signature_and_eof() -> Result<()> {
    let mut decoder = GenerateContentAdapter.stream_decoder();
    let parts = decoder.decode(&event(json!({"candidates":[{"content":{"parts":[{"functionCall":{"id":"a","name":"f","args":{"x":7}},"thoughtSignature":"proof"},{"text":"later"}]},"finishReason":"STOP"}]})))?;
    assert!(parts.is_empty());
    assert!(decoder.pending_terminal().is_some());
    let tail = decoder.finish()?;
    assert!(matches!(&tail[0],StreamPart::ToolCallDelta { id, .. } if id=="a"));
    assert!(matches!(
        &tail[1],
        StreamPart::ToolCallEnd {
            boundary: ToolCallBoundary::Turn { complete: true },
            ..
        }
    ));
    assert!(matches!(&tail[2],StreamPart::TextDelta { text } if text=="later"));
    let mut encoder = GenerateContentAdapter.stream_encoder("r", "m");
    let mut output = Vec::new();
    for part in tail {
        output.extend(frames(encoder.encode(&part)?)?);
    }
    assert_eq!(
        output[0]["candidates"][0]["content"]["parts"][0],
        json!({"functionCall":{"id":"a","name":"f","args":{"x":7}},"thoughtSignature":"proof"})
    );
    assert_eq!(
        output[1]["candidates"][0]["content"]["parts"][0]["text"],
        "later"
    );
    assert!(encoder.finish()?.is_empty());
    Ok(())
}

// UT: Parseable JSON is not a source boundary; later content cannot leapfrog an open call.
#[test]
fn ut_call_boundaries_incomplete_interleave_and_terminal_guards() -> Result<()> {
    for arguments in ["{\"x\":", "{}"] {
        let mut encoder = GenerateContentAdapter.stream_encoder("r", "m");
        assert!(encoder.encode(&call("a", Some("f"), arguments))?.is_empty());
        assert!(
            encoder
                .encode(&StreamPart::TextDelta {
                    text: "later".into()
                })
                .is_err()
        );
        assert!(encoder.finish().is_err());
    }
    for boundary in [
        ToolCallBoundary::Turn { complete: false },
        ToolCallBoundary::MessagesBlock { complete: false },
        ToolCallBoundary::ResponsesItem {
            status: Some("incomplete".into()),
            terminal_only: true,
        },
        ToolCallBoundary::ResponsesItem {
            status: None,
            terminal_only: true,
        },
    ] {
        let mut encoder = GenerateContentAdapter.stream_encoder("r", "m");
        encoder.encode(&call("a", Some("f"), "{}"))?;
        assert!(
            encoder
                .encode(&StreamPart::ToolCallEnd {
                    id: "a".into(),
                    boundary
                })
                .is_err()
        );
    }
    // A statusless item-done proves only payload completion. Hold the call
    // without executable output; missing concrete completion at EOF refuses.
    let mut statusless = GenerateContentAdapter.stream_encoder("r", "m");
    statusless.encode(&call("a", Some("f"), "{}"))?;
    assert!(
        statusless
            .encode(&StreamPart::ToolCallEnd {
                id: "a".into(),
                boundary: ToolCallBoundary::ResponsesItem {
                    status: None,
                    terminal_only: false,
                },
            })?
            .is_empty()
    );
    assert!(statusless.finish().is_err());
    for boundary in [
        ToolCallBoundary::Turn { complete: true },
        ToolCallBoundary::MessagesBlock { complete: true },
        ToolCallBoundary::ResponsesItem {
            status: Some("completed".into()),
            terminal_only: false,
        },
    ] {
        let mut encoder = GenerateContentAdapter.stream_encoder("r", "m");
        encoder.encode(&call("a", Some("f"), "{}"))?;
        assert!(
            encoder
                .encode(&StreamPart::ToolCallEnd {
                    id: "a".into(),
                    boundary: ToolCallBoundary::Arguments
                })?
                .is_empty()
        );
        assert_eq!(
            encoder
                .encode(&StreamPart::ToolCallEnd {
                    id: "a".into(),
                    boundary
                })?
                .len(),
            1
        );
        assert!(encoder.encode(&call("a", None, " ")).is_err());
    }
    let mut encoder = GenerateContentAdapter.stream_encoder("r", "m");
    encoder.encode(&terminal())?;
    assert!(
        encoder
            .encode(&StreamPart::TextDelta {
                text: "after terminal".into()
            })
            .is_err()
    );
    Ok(())
}

// UT: Native uint inputs cannot overflow canonical usage or an outbound total.
#[test]
fn ut_usage_range_aggregate_consistency_and_checked_render() -> Result<()> {
    for usage in [
        json!({"promptTokenCount":u64::MAX,"candidatesTokenCount":1}),
        json!({"promptTokenCount":i32::MAX,"candidatesTokenCount":1}),
        json!({"promptTokenCount":11,"cachedContentTokenCount":12}),
        json!({"promptTokenCount":11,"totalTokenCount":900}),
        json!({"promptTokenCount":11,"thoughtsTokenCount":-1}),
        json!({"promptTokenCount":11,"toolUsePromptTokenCount":5}),
    ] {
        assert!(parse_usage(&usage).is_none());
    }
    let prompt = request(minimal(), false)?;
    let mut result = GenerateContentAdapter.parse_response(reply(json!([])))?;
    result.usage = Some(Usage {
        prompt_tokens: u64::MAX,
        completion_tokens: 1,
        ..Usage::default()
    });
    assert!(
        GenerateContentAdapter
            .render_response(&result, &prompt, "r")
            .is_err()
    );
    let raw = json!({"promptTokenCount":11,"candidatesTokenCount":3,"promptTokensDetails":[{"modality":"TEXT","tokenCount":11}]});
    let usage = parse_usage(&raw).context("implicit optional zeros")?;
    assert_eq!(usage.raw.as_deref(), Some(&raw));
    assert_eq!(usage.reasoning_tokens, 0);
    assert!(
        usage
            .raw
            .as_ref()
            .is_some_and(|value| value.get("thoughtsTokenCount").is_none())
    );
    Ok(())
}

// UT: Duplicate IDs and every independent accumulation budget refuse before executable output.
#[test]
fn ut_call_and_buffer_budgets_and_duplicate_stream_ids() -> Result<()> {
    let many = (0..65)
        .map(|i| json!({"functionCall":{"id":format!("id-{i}"),"name":"f","args":{}}}))
        .collect::<Vec<_>>();
    for parts in [
        json!([{"functionCall":{"id":"duplicate","name":"f","args":{}}},{"functionCall":{"id":"duplicate","name":"f","args":{}}}]),
        json!(many),
    ] {
        let mut decoder = GenerateContentAdapter.stream_decoder();
        assert!(decoder.decode(&event(reply(parts))).is_err());
        assert!(decoder.finish().is_err());
    }
    let mut decoder = GenerateContentAdapter.stream_decoder();
    let many_empty = vec![json!({"text":""}); parts::MAX_PARTS + 1];
    assert!(decoder.decode(&event(reply(json!(many_empty)))).is_err());
    let mut encoder = GenerateContentAdapter.stream_encoder("r", "m");
    assert!(
        encoder
            .encode(&call("a", Some("f"), &" ".repeat(17 * 1024 * 1024)))
            .is_err()
    );
    let mut encoder = GenerateContentAdapter.stream_encoder("r", "m");
    for i in 0..64 {
        encoder.encode(&call(&format!("id-{i}"), Some("f"), "{}"))?;
    }
    assert!(encoder.encode(&call("id-64", Some("f"), "{}")).is_err());
    Ok(())
}

// UT: Signature-bearing stream text/file requires native custody, never a lossy canonical success.
#[test]
fn ut_signed_stream_text_media_and_post_terminal_content_refused() -> Result<()> {
    for part in [
        json!({"text":"","thoughtSignature":"proof"}),
        json!({"text":"signed","thought":true,"thoughtSignature":"proof"}),
        json!({"inlineData":{"mimeType":"image/png","data":"AA=="},"thoughtSignature":"proof"}),
    ] {
        let mut decoder = GenerateContentAdapter.stream_decoder();
        assert!(decoder.decode(&event(reply(json!([part])))).is_err());
        assert!(decoder.finish().is_err());
    }
    let mut decoder = GenerateContentAdapter.stream_decoder();
    decoder.decode(&event(json!({"candidates":[{"finishReason":"STOP"}]})))?;
    assert!(
        decoder
            .decode(&event(reply(json!([{"text":"late"}]))))
            .is_err()
    );
    assert!(decoder.finish().is_err());
    Ok(())
}

// UT: Valid cumulative snapshots are retained; malformed or decreasing tails
// cannot replace earlier observations and then authorize clean completion.
#[test]
fn ut_usage_tail_conflict_and_malformed_tail_refuse_clean_eof() -> Result<()> {
    for raw in [
        json!({"promptTokenCount":11,"candidatesTokenCount":2}),
        json!({"promptTokenCount":11,"candidatesTokenCount":"bad"}),
        json!({"promptTokenCount":11,"candidatesTokenCount":3,"promptTokensDetails":[{"modality":"TEXT","tokenCount":12}]}),
    ] {
        let mut decoder = GenerateContentAdapter.stream_decoder();
        let first = decoder.decode(&event(json!({"responseId":"r","usageMetadata":report()})))?;
        assert!(
            first
                .iter()
                .any(|part| matches!(part, StreamPart::Usage { .. }))
        );
        decoder.decode(&event(
            json!({"responseId":"r","candidates":[{"finishReason":"STOP"}]}),
        ))?;
        assert!(decoder.pending_terminal().is_some());
        assert!(
            decoder
                .decode(&event(json!({"responseId":"r","usageMetadata":raw})))
                .is_err()
        );
        assert!(decoder.finish().is_err());
    }
    Ok(())
}

// UT: A native projection cannot erase foreign part metadata before replay admission.
#[test]
fn ut_foreign_part_metadata_refused() -> Result<()> {
    let prompt = request(minimal(), false)?;
    let mut result = GenerateContentAdapter.parse_response(reply(json!([{"text":"answer"}])))?;
    if let Content::Text {
        provider_metadata, ..
    } = &mut result.content[0]
    {
        set_provider_metadata(provider_metadata, "anthropic", "signature", json!("proof"));
    }
    assert!(
        GenerateContentAdapter
            .render_response(&result, &prompt, "r")
            .is_err()
    );
    Ok(())
}

// UT: Promoting a native knob does not authorize dropping it on a destination
// whose renderer has no corresponding field.
#[test]
fn ut_promoted_controls_require_an_actual_target_renderer() -> Result<()> {
    for (config, refused) in [
        (json!({"topK":4}), vec![0, 2]),
        (json!({"seed":9}), vec![1, 2]),
        (json!({"stopSequences":["end"]}), vec![2]),
        (json!({"presencePenalty":0.0}), vec![1, 2]),
        (json!({"frequencyPenalty":0.0}), vec![1, 2]),
        (json!({"responseModalities":["TEXT"]}), vec![1, 2]),
        (json!({"responseModalities":["IMAGE"]}), vec![0, 1, 2]),
    ] {
        let mut body = minimal();
        body["generationConfig"] = config.clone();
        let prompt = request(body, false)?;
        assert_eq!(
            GenerateContentAdapter.render_request(&prompt)?["generationConfig"],
            config
        );
        for index in refused {
            assert!(targets()[index].render_request(&prompt).is_err());
        }
    }
    Ok(())
}

// UT: A provider-owned file URI is not transferable credential/source authority.
#[test]
fn ut_native_file_reference_requires_native_target_custody() -> Result<()> {
    let part =
        json!({"fileData":{"mimeType":"application/pdf","fileUri":"gs://fixture-only/report.pdf"}});
    let prompt = request(json!({"contents":[{"parts":[part]}]}), false)?;
    assert_eq!(
        GenerateContentAdapter.render_request(&prompt)?["contents"][0]["parts"][0],
        part
    );
    for target in targets() {
        assert!(target.render_request(&prompt).is_err());
    }
    Ok(())
}

// UT: Anonymous native calls have compatibility IDs, but those IDs never become
// invented native identity. Native signature and original omission survive EOF.
#[test]
fn ut_anonymous_stream_call_preserves_native_omission() -> Result<()> {
    let native = json!({"functionCall":{"name":"f","args":{}},"thoughtSignature":"proof"});
    let mut decoder = GenerateContentAdapter.stream_decoder();
    assert!(decoder.decode(&event(reply(json!([native]))))?.is_empty());
    let parts = decoder.finish()?;
    assert!(matches!(&parts[0],StreamPart::ToolCallDelta { id, .. } if !id.is_empty()));
    let mut encoder = GenerateContentAdapter.stream_encoder("r", "m");
    let mut output = Vec::new();
    for part in parts {
        output.extend(frames(encoder.encode(&part)?)?);
    }
    assert_eq!(output[0]["candidates"][0]["content"]["parts"][0], native);
    Ok(())
}

// UT: Selection of the first canonical candidate cannot conceal a malformed
// secondary candidate, error, nonterminal, duplicate index or invalid identity.
#[test]
fn ut_secondary_json_candidates_are_validated_without_merging() -> Result<()> {
    for second in [
        Value::Null,
        json!({}),
        json!({"finishReason":"STOP","error":{"code":429}}),
        json!({"finishReason":"STOP","content":{"parts":7}}),
        json!({"finishReason":"STOP","index":0}),
    ] {
        assert!(
            GenerateContentAdapter
                .parse_response(json!({"candidates":[{"index":0,"finishReason":"STOP"},second]}))
                .is_err()
        );
    }
    let result = GenerateContentAdapter.parse_response(json!({"responseId":"r","candidates":[
        {"index":0,"finishReason":"STOP","content":{"parts":[{"text":"chosen"}]}},
        {"index":1,"finishReason":"STOP","content":{"parts":[{"text":"separate"}]}}
    ]}))?;
    assert_eq!(result.content.len(), 1);
    assert!(matches!(&result.content[0],Content::Text { text, .. } if text == "chosen"));
    Ok(())
}

// UT: A candidate-local error, malformed feedback or wrong output role is not
// a successful terminal. Stream identities stay within the native int32 range.
#[test]
fn ut_stream_error_candidates_and_invalid_envelope_refused() -> Result<()> {
    for body in [
        json!({"candidates":[{"error":{"code":429},"finishReason":"STOP"}]}),
        json!({"promptFeedback":[],"candidates":[{"finishReason":"STOP"}]}),
        json!({"candidates":[{"content":{"role":"user","parts":[{"text":"x"}]},"finishReason":"STOP"}]}),
        json!({"candidates":[{"index":u64::MAX,"finishReason":"STOP"}]}),
    ] {
        assert!(GenerateContentAdapter.parse_response(body.clone()).is_err());
        let mut decoder = GenerateContentAdapter.stream_decoder();
        assert!(decoder.decode(&event(body)).is_err());
        assert!(decoder.finish().is_err());
    }
    Ok(())
}
