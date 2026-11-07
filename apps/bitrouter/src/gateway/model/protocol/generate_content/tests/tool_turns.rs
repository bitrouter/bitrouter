//! Canonical conversion preserves complete tool ownership through source EOF.
use super::*;

fn responses_projection(arguments: &str, item_status: Option<&str>) -> Result<Vec<Value>> {
    let item = json!({"id":"tool_item","type":"function_call","call_id":"call_c",
        "name":"lookup","arguments":arguments,"status":"completed"});
    let mut done = item.clone();
    if item_status.is_none() {
        done.as_object_mut().context("item")?.remove("status");
    }
    let events = [
        json!({"type":"response.output_item.added","output_index":0,
            "item":{"id":"tool_item","type":"function_call","call_id":"call_c",
            "name":"lookup","arguments":"","status":"in_progress"}}),
        json!({"type":"response.function_call_arguments.delta","item_id":"tool_item","output_index":0,"delta":arguments}),
        json!({"type":"response.function_call_arguments.done","item_id":"tool_item","output_index":0,"arguments":arguments}),
        json!({"type":"response.output_item.done","output_index":0,"item":done}),
        json!({"type":"response.completed","response":{"id":"response_r","status":"completed","output":[item]}}),
    ];
    let mut source = ResponsesAdapter.stream_decoder();
    let mut target = GenerateContentAdapter.stream_encoder("fixture", "fixture-model");
    let mut output = Vec::new();
    for body in events {
        let label = body["type"].as_str().context("event type")?.to_owned();
        for part in source.decode(&SseEvent {
            event: Some(label),
            data: body.to_string(),
        })? {
            output.extend(strict_frames(target.encode(&part)?)?);
        }
    }
    for part in source.finish()? {
        output.extend(strict_frames(target.encode(&part)?)?);
    }
    output.extend(strict_frames(target.finish()?)?);
    Ok(output)
}

fn strict_frames(frames: Vec<SseFrame>) -> Result<Vec<Value>> {
    frames
        .into_iter()
        .map(|frame| match frame {
            SseFrame::Event { data, .. } => Ok(native::value(&data)?),
            _ => anyhow::bail!("unexpected non-native data frame"),
        })
        .collect()
}

#[test]
fn ut_responses_tool_completion_accepts_matching_eof_confirmation_once() -> Result<()> {
    for item_status in [Some("completed"), None] {
        let output = responses_projection("{}", item_status)?;
        assert_eq!(
            output
                .iter()
                .filter(|frame| frame
                    .pointer("/candidates/0/content/parts/0/functionCall")
                    .is_some())
                .count(),
            1
        );
        assert!(
            output
                .iter()
                .any(|frame| frame["candidates"][0]["finishReason"] == "STOP")
        );
    }
    for (id, status) in [("call_c", "failed"), ("unknown", "completed")] {
        let mut encoder = GenerateContentAdapter.stream_encoder("fixture", "fixture-model");
        encoder.encode(&call("call_c", Some("lookup"), "{}"))?;
        encoder.encode(&StreamPart::ToolCallEnd {
            id: "call_c".into(),
            boundary: ToolCallBoundary::ResponsesItem {
                status: Some("completed".into()),
                terminal_only: false,
            },
        })?;
        assert!(
            encoder
                .encode(&StreamPart::ToolCallEnd {
                    id: id.into(),
                    boundary: ToolCallBoundary::ResponsesItem {
                        status: Some(status.into()),
                        terminal_only: true
                    }
                })
                .is_err()
        );
        assert!(encoder.finish().is_err());
    }
    Ok(())
}

#[test]
fn ut_idless_results_match_outstanding_calls_and_keep_monotonic_ids() -> Result<()> {
    let prompt = request(
        json!({"contents":[
            {"role":"model","parts":[{"functionCall":{"name":"lookup","args":{}}}]},
            {"role":"user","parts":[{"functionResponse":{"name":"lookup","response":{}}}]},
            {"role":"model","parts":[{"functionCall":{"name":"lookup","args":{}}}]},
            {"role":"user","parts":[{"functionResponse":{"name":"lookup","response":{}}}]}
        ]}),
        false,
    )?;
    let mut calls = Vec::new();
    let mut results = Vec::new();
    for content in prompt.messages.iter().flat_map(|message| &message.content) {
        match content {
            Content::ToolCall { id, .. } => calls.push(id.as_str()),
            Content::ToolResult {
                call_id,
                provider_metadata,
                ..
            } => {
                assert!(
                    bitrouter_ai::types::provider_namespace(provider_metadata, "google")
                        .is_none_or(
                            |fields| fields.get(parts::UNBOUND_RESULT) != Some(&Value::Bool(true))
                        )
                );
                results.push(call_id.as_str());
            }
            _ => (),
        }
    }
    assert_eq!(calls, ["gc-anonymous-0", "gc-anonymous-1"]);
    assert_eq!(results, calls);
    let ordinary = ChatCompletionsAdapter.render_request(&prompt)?;
    let round_trip = ChatCompletionsAdapter.parse_request(ordinary)?;
    let ids = round_trip
        .messages
        .iter()
        .flat_map(|message| &message.content)
        .filter_map(|content| match content {
            Content::ToolResult { call_id, .. } => Some(call_id.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(ids, results);
    Ok(())
}

#[test]
fn ut_tool_result_correlation_keeps_explicit_ids_and_refuses_ambiguous_reuse() -> Result<()> {
    let prompt = request(
        json!({"contents":[
            {"role":"model","parts":[
                {"functionCall":{"id":"first","name":"lookup","args":{}}},
                {"functionCall":{"id":"second","name":"lookup","args":{}}}]},
            {"role":"user","parts":[
                {"functionResponse":{"id":"second","name":"lookup","response":{}}},
                {"functionResponse":{"name":"lookup","response":{}}}]}
        ]}),
        false,
    )?;
    let ids = prompt
        .messages
        .iter()
        .flat_map(|message| &message.content)
        .filter_map(|content| match content {
            Content::ToolResult { call_id, .. } => Some(call_id.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(ids, ["second", "first"]);
    ChatCompletionsAdapter.render_request(&prompt)?;

    for (calls, results) in [
        (
            json!([
            {"functionCall":{"name":"lookup","args":{}}},
            {"functionCall":{"name":"lookup","args":{}}}]),
            json!([{"functionResponse":{"name":"lookup","response":{}}}]),
        ),
        (
            json!([{"functionCall":{"id":"first","name":"lookup","args":{}}}]),
            json!([
             {"functionResponse":{"id":"first","name":"lookup","response":{}}},
             {"functionResponse":{"id":"first","name":"lookup","response":{}}}]),
        ),
    ] {
        let wire = json!({"contents":[{"role":"model","parts":calls},
            {"role":"user","parts":results}]});
        let prompt = request(wire.clone(), false)?;
        assert!(ChatCompletionsAdapter.render_request(&prompt).is_err());
        assert_eq!(
            GenerateContentAdapter.render_request(&prompt)?["contents"],
            wire["contents"]
        );
    }
    Ok(())
}

#[test]
fn ut_responses_arguments_preserve_literal_private_markers_without_expansion() -> Result<()> {
    for arguments in [
        r#"{"x":{"$serde_json::private::RawValue":"{\"mode\":\"changed\"}"}}"#,
        r#"{"x":{"$serde_json::private::Number":"7","keep":true}}"#,
        r#"{"nested":[{"$serde_json::private::RawValue":"{}","keep":2}],"$serde_json::private::Number":"literal"}"#,
    ] {
        let expected = native::value(arguments)?;
        for status in [Some("completed"), None] {
            let output = responses_projection(arguments, status)?;
            let calls = output
                .iter()
                .filter_map(|frame| {
                    frame.pointer("/candidates/0/content/parts/0/functionCall/args")
                })
                .collect::<Vec<_>>();
            assert_eq!(calls.len(), 1);
            assert_eq!(calls[0], &expected, "original arguments: {arguments}");
        }
    }
    Ok(())
}

#[test]
fn ut_canonical_arguments_reject_duplicates_depth_and_nonobject_before_output() -> Result<()> {
    for arguments in [r#"{"x":1,"x":2}"#, r#"{"x":1,"\u0078":2}"#, "[]"] {
        assert!(function_arguments(arguments).is_none());
        assert!(responses_projection(arguments, Some("completed")).is_err());
    }
    let excessive = format!("{{\"x\":{}0{}}}", "[".repeat(129), "]".repeat(129));
    assert!(function_arguments(&excessive).is_none());
    assert!(responses_projection(&excessive, Some("completed")).is_err());
    let bounded = format!("{{\"x\":{}0{}}}", "[".repeat(32), "]".repeat(32));
    assert!(function_arguments(&bounded).is_some());
    responses_projection(&bounded, Some("completed"))?;
    Ok(())
}

#[test]
fn ut_canonical_sse_duplicate_terminal_never_releases_executable_calls() -> Result<()> {
    let mut decoder = GenerateContentAdapter.stream_decoder();
    let event = SseEvent {event:None,data:r#"{"candidates":[{"content":{"parts":[{"functionCall":{"id":"a","name":"lookup","args":{}}}]},"finishReason":"MAX_TOKENS","finishReason":"STOP"}]}"#.into()};
    assert!(decoder.decode(&event).is_err());
    assert!(decoder.pending_terminal().is_none());
    assert!(decoder.finish().is_err());
    Ok(())
}

#[test]
fn ut_named_sse_errors_invalidate_pending_tools_even_without_data() -> Result<()> {
    for data in ["", r#"{"message":"failed"}"#] {
        let mut decoder = GenerateContentAdapter.stream_decoder();
        let first = decoder.decode(&event(reply(
            json!([{"functionCall":{"id":"a","name":"lookup","args":{}}}]),
        )))?;
        assert!(
            !first
                .iter()
                .any(|part| matches!(part, StreamPart::ToolCallDelta { .. }))
        );
        assert!(
            decoder
                .decode(&SseEvent {
                    event: Some("error".into()),
                    data: data.into()
                })
                .is_err()
        );
        assert!(decoder.pending_terminal().is_none());
        assert!(decoder.finish().is_err());
    }
    Ok(())
}
