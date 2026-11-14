use super::super::{
    InboundAdapter, OutboundAdapter, SseEvent, messages::MessagesAdapter,
    responses::ResponsesAdapter,
};
use crate::gateway::http::{AppState, build_router};
use crate::gateway::model::stream::SseFrame;
use crate::gateway::model::target::AuthScheme;
use crate::gateway::pipeline::{
    builder::PipelineBuilder, executor::HttpExecutor, routing::StaticRoutingTable,
    types::RoutingTarget,
};
use anyhow::Result;
use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use bitrouter_ai::types::{ApiProtocol, FinishReason, StreamPart};
use serde_json::{Value, json};
use std::{collections::BTreeMap, sync::Arc};
use tower::ServiceExt;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

const A: &str = r#"{"prompt":"alpha"}"#;
const B: &str = r#"{"prompt":"beta"}"#;

fn event(kind: &str, mut body: Value) -> SseEvent {
    body["type"] = json!(kind);
    SseEvent {
        event: Some(kind.to_owned()),
        data: body.to_string(),
    }
}

fn round_trip<T: InboundAdapter + OutboundAdapter>(
    adapter: T,
    input: &[SseEvent],
    expected_b: &str,
) -> Result<Vec<Value>> {
    let mut decoder = adapter.stream_decoder();
    let mut encoder = adapter.stream_encoder("round-trip", "fixture");
    let mut decoded = Vec::new();
    for event in input {
        decoded.extend(decoder.decode(event)?);
    }
    decoded.extend(decoder.finish()?);

    for (expected_id, expected_arguments) in [("call-a", A), ("call-b", expected_b)] {
        let arguments: String = decoded
            .iter()
            .filter_map(|part| match part {
                StreamPart::ToolCallDelta { id, arguments, .. } if id == expected_id => {
                    Some(arguments.as_str())
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            arguments, expected_arguments,
            "decoder changed {expected_id}"
        );
    }

    let mut frames = Vec::new();
    for part in &decoded {
        frames.extend(encoder.encode(part)?);
    }
    frames.extend(encoder.finish()?);
    frames
        .into_iter()
        .filter_map(|frame| match frame {
            SseFrame::Event { data, .. } => Some(serde_json::from_str(&data)),
            SseFrame::Comment(_) => None,
        })
        .collect::<serde_json::Result<Vec<_>>>()
        .map_err(Into::into)
}

fn responses_input(interleaved: bool) -> Result<Vec<SseEvent>> {
    let start = |index, id, call_id| {
        event(
            "response.output_item.added",
            json!({
                "output_index": index,
                "item": {"type":"function_call","id":id,"call_id":call_id,"name":"lookup","arguments":"","status":"in_progress"}
            }),
        )
    };
    let delta = |index, id, text| {
        event(
            "response.function_call_arguments.delta",
            json!({
                "output_index":index,"item_id":id,"delta":text
            }),
        )
    };
    let complete = |id, call_id, args| json!({"type":"function_call","id":id,"call_id":call_id,"name":"lookup","arguments":args,"status":"completed"});
    let first = complete("item-a", "call-a", A);
    let second = complete("item-b", "call-b", B);
    let mut events = vec![
        event(
            "response.created",
            json!({"response":{"id":"resp_fixture","status":"in_progress","output":[]}}),
        ),
        start(0, "item-a", "call-a"),
        delta(0, "item-a", r#"{"prompt":""#),
    ];
    if interleaved {
        events.push(start(1, "item-b", "call-b"));
    }
    events.push(delta(0, "item-a", r#"alpha"}"#));
    for (index, id, args, item) in [
        (0, "item-a", A, first.clone()),
        (1, "item-b", B, second.clone()),
    ] {
        if index == 1 {
            if !interleaved {
                events.push(start(1, "item-b", "call-b"));
            }
            events.push(delta(1, "item-b", B));
        }
        events.push(event(
            "response.function_call_arguments.done",
            json!({
                "output_index":index,"item_id":id,"name":"lookup","arguments":args
            }),
        ));
        events.push(event(
            "response.output_item.done",
            json!({"output_index":index,"item":item}),
        ));
    }
    events.push(event("response.completed", json!({
        "response":{"id":"resp_fixture","object":"response","status":"completed","output":[first,second],
        "usage":{"input_tokens":3,"output_tokens":5,"total_tokens":8}}
    })));
    events
        .into_iter()
        .enumerate()
        .map(|(sequence, mut event)| {
            let mut payload: Value = serde_json::from_str(&event.data)?;
            payload["sequence_number"] = json!(sequence);
            event.data = payload.to_string();
            Ok(event)
        })
        .collect()
}

fn messages_input(interleaved: bool) -> Vec<SseEvent> {
    let start = |index, id| {
        event(
            "content_block_start",
            json!({
                "index":index,"content_block":{"type":"tool_use","id":id,"name":"lookup","input":{}}
            }),
        )
    };
    let stop = |index| event("content_block_stop", json!({"index":index}));
    let mut events = vec![
        event(
            "message_start",
            json!({"message":{
                "id":"msg_fixture","type":"message","role":"assistant","content":[],
                "model":"fixture","stop_reason":null,"stop_sequence":null,
                "usage":{"input_tokens":3,"output_tokens":0}
            }}),
        ),
        start(0, "call-a"),
    ];
    if interleaved {
        events.push(start(1, "call-b"));
    }
    events.push(event(
        "content_block_delta",
        json!({
            "index":0,"delta":{"type":"input_json_delta","partial_json":A}
        }),
    ));
    events.push(stop(0));
    if !interleaved {
        events.push(start(1, "call-b"));
    }
    events.push(stop(1));
    events.push(event(
        "message_delta",
        json!({
            "delta":{"stop_reason":"tool_use","stop_sequence":null},"usage":{"output_tokens":5}
        }),
    ));
    events.push(event("message_stop", json!({})));
    events
}

fn responses_arguments(output: &[Value]) -> Result<Vec<(Value, Value)>> {
    let terminal = output
        .iter()
        .find(|event| event["type"] == "response.completed")
        .ok_or_else(|| anyhow::anyhow!("Responses terminal absent"))?;
    let items = terminal["response"]["output"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("Responses output absent"))?;
    for (index, item) in items.iter().enumerate() {
        let matches_item = |event: &&Value| {
            event["output_index"] == json!(index) && event["item_id"] == item["id"]
        };
        let fragments: String = output
            .iter()
            .filter(|event| event["type"] == "response.function_call_arguments.delta")
            .filter(matches_item)
            .filter_map(|event| event["delta"].as_str())
            .collect();
        let done = output
            .iter()
            .filter(|event| event["type"] == "response.function_call_arguments.done")
            .find(matches_item)
            .ok_or_else(|| anyhow::anyhow!("arguments done missing for item {index}"))?;
        assert_eq!(done["arguments"], item["arguments"]);
        assert_eq!(fragments, item["arguments"]);
    }
    Ok(items
        .iter()
        .map(|item| (item["call_id"].clone(), item["arguments"].clone()))
        .collect())
}

fn messages_arguments(output: &[Value]) -> Result<Vec<(Value, Value)>> {
    assert!(output.iter().any(|event| event["type"] == "message_stop"));
    let mut calls = BTreeMap::new();
    let mut open = None;
    for event in output {
        if event["type"] == "content_block_start" && event["content_block"]["type"] == "tool_use" {
            let index = event["index"]
                .as_u64()
                .ok_or_else(|| anyhow::anyhow!("call index absent"))?;
            assert_eq!(open.replace(index), None, "Messages blocks must be serial");
            calls.insert(index, (event["content_block"]["id"].clone(), String::new()));
        } else if event["type"] == "content_block_delta"
            && event["delta"]["type"] == "input_json_delta"
        {
            let index = event["index"]
                .as_u64()
                .ok_or_else(|| anyhow::anyhow!("delta index absent"))?;
            assert_eq!(open, Some(index), "tool fragment after its block closed");
            let (_, args) = calls
                .get_mut(&index)
                .ok_or_else(|| anyhow::anyhow!("delta call absent"))?;
            args.push_str(
                event["delta"]["partial_json"]
                    .as_str()
                    .ok_or_else(|| anyhow::anyhow!("delta absent"))?,
            );
        } else if event["type"] == "content_block_stop" {
            assert_eq!(open.take(), event["index"].as_u64());
        }
    }
    assert_eq!(open, None);
    calls
        .into_values()
        .map(|(id, args)| {
            let input = if args.is_empty() {
                json!({})
            } else {
                serde_json::from_str(&args)?
            };
            Ok((id, input))
        })
        .collect()
}

#[test]
fn sequential_tool_fragments_keep_their_call_association() -> Result<()> {
    assert_eq!(
        responses_arguments(&round_trip(ResponsesAdapter, &responses_input(false)?, B)?)?,
        [(json!("call-a"), json!(A)), (json!("call-b"), json!(B))]
    );
    assert_eq!(
        messages_arguments(&round_trip(MessagesAdapter, &messages_input(false), "")?)?,
        [
            (json!("call-a"), json!({"prompt":"alpha"})),
            (json!("call-b"), json!({}))
        ]
    );
    Ok(())
}

#[test]
fn responses_interleaved_tool_fragments_keep_their_call_association() -> Result<()> {
    assert_eq!(
        responses_arguments(&round_trip(ResponsesAdapter, &responses_input(true)?, B)?)?,
        [(json!("call-a"), json!(A)), (json!("call-b"), json!(B))]
    );
    Ok(())
}

#[test]
fn messages_interleaved_tool_fragments_keep_their_call_association() -> Result<()> {
    assert_eq!(
        messages_arguments(&round_trip(MessagesAdapter, &messages_input(true), "")?)?,
        [
            (json!("call-a"), json!({"prompt":"alpha"})),
            (json!("call-b"), json!({}))
        ]
    );
    Ok(())
}

async fn mounted_output(
    protocol: ApiProtocol,
    endpoint: &str,
    request: Value,
    events: Vec<SseEvent>,
) -> Result<Vec<Value>> {
    let upstream = MockServer::start().await;
    let mut sse = String::new();
    for event in events {
        sse.push_str(
            &SseFrame::Event {
                event: event.event,
                data: event.data,
            }
            .to_wire()?,
        );
    }
    Mock::given(method("POST"))
        .and(path(endpoint))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(sse),
        )
        .expect(1)
        .mount(&upstream)
        .await;

    let table = StaticRoutingTable::new();
    table.insert(
        "fixture",
        vec![RoutingTarget {
            registry: None,
            provider_name: "loopback-fixture".to_owned(),
            service_id: "fixture".to_owned(),
            api_base: upstream.uri(),
            api_key: "fixture".to_owned(),
            api_protocol: protocol,
            chat_token_limit_field: None,
            chat_supports_store: None,
            chat_supports_stream_options: None,
            chat_google_extensions: false,
            reasoning_effort: None,
            model_constraints: Default::default(),
            account_label: None,
            api_key_override: None,
            api_base_override: None,
            auth_scheme: AuthScheme::XApiKey,
            headers: Vec::new(),
        }],
    );
    let mut pipeline = PipelineBuilder::new();
    pipeline
        .routing_table(Arc::new(table))
        .executor(Arc::new(HttpExecutor::with_defaults()?));
    let router = build_router(AppState {
        language_model: Arc::new(pipeline.build()?),
        mcp: None,
        skip_auth: true,
        metrics_renderer: None,
        prompt_transforms: Vec::new(),
    });
    let response = router
        .oneshot(
            Request::post(format!("/v1{endpoint}"))
                .header("content-type", "application/json")
                .body(Body::from(request.to_string()))?,
        )
        .await?;
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 1 << 16).await?;
    let body = std::str::from_utf8(&bytes)?;
    assert_eq!(status, StatusCode::OK, "{body}");
    upstream.verify().await;
    body.lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .map(|data| serde_json::from_str(data).map_err(Into::into))
        .collect()
}

#[tokio::test]
async fn mounted_responses_interleaving_keeps_its_call_association() -> Result<()> {
    let output = mounted_output(
        ApiProtocol::Responses,
        "/responses",
        json!({"model":"fixture","input":"look up alpha and beta","stream":true,
            "parallel_tool_calls":true,"tools":[{"type":"function","name":"lookup","strict":false,
            "parameters":{"type":"object","properties":{"prompt":{"type":"string"}}}}]}),
        responses_input(true)?,
    )
    .await?;
    assert_eq!(
        responses_arguments(&output)?,
        [(json!("call-a"), json!(A)), (json!("call-b"), json!(B))]
    );
    Ok(())
}

#[tokio::test]
async fn mounted_messages_interleaving_keeps_its_call_association() -> Result<()> {
    let output = mounted_output(
        ApiProtocol::Messages,
        "/messages",
        messages_request(),
        messages_input(true),
    )
    .await?;
    assert_eq!(
        messages_arguments(&output)?,
        [
            (json!("call-a"), json!({"prompt":"alpha"})),
            (json!("call-b"), json!({}))
        ]
    );
    Ok(())
}

fn messages_request() -> Value {
    json!({"model":"fixture","max_tokens":256,"stream":true,
        "messages":[{"role":"user","content":"look up alpha, then use the default"}],
        "tools":[{"name":"lookup","input_schema":{"type":"object","properties":{"prompt":{"type":"string"}}}}]})
}

#[tokio::test]
async fn mounted_messages_does_not_publish_tools_from_truncated_or_invalid_streams() -> Result<()> {
    let mut truncated = messages_input(true);
    assert!(
        truncated
            .pop()
            .is_some_and(|event| event.event.as_deref() == Some("message_stop"))
    );
    let mut late_delta = messages_input(true);
    let stop = late_delta
        .iter()
        .position(|event| event.event.as_deref() == Some("content_block_stop"))
        .ok_or_else(|| anyhow::anyhow!("tool stop absent from fixture"))?;
    late_delta.insert(
        stop + 1,
        event(
            "content_block_delta",
            json!({
                "index":0,"delta":{"type":"input_json_delta","partial_json":" "}
            }),
        ),
    );
    let mut after_message_stop = messages_input(true);
    after_message_stop.push(event(
        "content_block_delta",
        json!({
            "index":0,"delta":{"type":"input_json_delta","partial_json":" "}
        }),
    ));
    let mut reused_index = messages_input(true);
    reused_index[2] = event(
        "content_block_start",
        json!({
            "index":0,"content_block":{"type":"tool_use","id":"call-b","name":"lookup","input":{}}
        }),
    );
    let mut false_stop = truncated.clone();
    false_stop.push(SseEvent {
        event: Some("message_stop".to_owned()),
        data: json!({"type":"ping"}).to_string(),
    });
    for input in [
        truncated,
        late_delta,
        after_message_stop,
        reused_index,
        false_stop,
    ] {
        let output = mounted_output(
            ApiProtocol::Messages,
            "/messages",
            messages_request(),
            input,
        )
        .await?;
        assert!(
            output.iter().any(|event| event["type"] == "error"),
            "{output:?}"
        );
        assert!(
            !output.iter().any(|event| event["type"] == "message_stop"),
            "{output:?}"
        );
        assert!(
            !output
                .iter()
                .any(|event| event["content_block"]["id"] == "call-b"),
            "{output:?}"
        );
    }
    Ok(())
}

#[test]
fn messages_provider_executed_arguments_never_open_a_client_tool() -> Result<()> {
    let mut decoder = MessagesAdapter.stream_decoder();
    for wire_event in [
        event(
            "content_block_start",
            json!({"index":0,"content_block":{
                "type":"server_tool_use","id":"srvtoolu_fixture","name":"web_search","input":{}
            }}),
        ),
        event(
            "content_block_delta",
            json!({"index":0,"delta":{
                "type":"input_json_delta","partial_json":"{\"query\":\"alpha\"}"
            }}),
        ),
        event("content_block_stop", json!({"index":0})),
    ] {
        assert!(decoder.decode(&wire_event)?.is_empty());
    }
    Ok(())
}

#[test]
fn responses_terminal_retains_output_order_and_marks_truncated_tools_incomplete() -> Result<()> {
    let tool = |id: &str, name: Option<&str>, arguments: &str| StreamPart::ToolCallDelta {
        id: id.to_owned(),
        name: name.map(str::to_owned),
        arguments: arguments.to_owned(),
        provider_metadata: Default::default(),
    };
    let mut encoder = ResponsesAdapter.stream_encoder("terminal-fixture", "fixture");
    for part in [
        tool("call-a", Some("lookup"), r#"{"prompt":""#),
        StreamPart::TextDelta {
            text: "between calls".to_owned(),
        },
        tool("call-b", Some("lookup"), B),
        tool("call-a", None, "partial"),
    ] {
        assert!(!encoder.encode(&part)?.iter().any(|frame| matches!(frame,
            SseFrame::Event { event: Some(event), .. } if event == "response.function_call_arguments.done")));
    }
    let terminal = encoder.encode(&StreamPart::Finish {
        reason: FinishReason::Length,
    })?;
    assert!(!terminal.iter().any(
        |frame| matches!(frame, SseFrame::Event { event: Some(event), .. }
        if event == "response.function_call_arguments.done")
    ));
    let data = terminal
        .iter()
        .find_map(|frame| match frame {
            SseFrame::Event {
                event: Some(event),
                data,
            } if event == "response.incomplete" => Some(data),
            _ => None,
        })
        .ok_or_else(|| anyhow::anyhow!("incomplete terminal absent"))?;
    let terminal: Value = serde_json::from_str(data)?;
    let output = &terminal["response"]["output"];
    assert_eq!(output[0]["call_id"], "call-a");
    assert_eq!(output[0]["status"], "incomplete");
    assert_eq!(output[0]["arguments"], r#"{"prompt":"partial"#);
    assert_eq!(output[1]["content"][0]["text"], "between calls");
    assert_eq!(output[2]["call_id"], "call-b");
    assert_eq!(output[2]["status"], "incomplete");
    Ok(())
}

#[test]
fn messages_stop_holds_turn_success_but_not_usage() -> Result<()> {
    let mut decoder = MessagesAdapter.stream_decoder();
    let mut received_usage = None;
    for wire in messages_input(false) {
        for part in decoder.decode(&wire)? {
            assert!(!part.is_terminal());
            if let StreamPart::Usage { usage } = part {
                received_usage = Some((usage.prompt_tokens, usage.completion_tokens));
            }
        }
        assert!(decoder.pending_terminal().is_none());
    }
    assert_eq!(received_usage, Some((3, 5)));
    assert!(decoder.finish()?.iter().any(StreamPart::is_terminal));
    Ok(())
}

#[test]
fn native_responses_keeps_early_item_closes_and_mixed_terminal_statuses() -> Result<()> {
    let mut input = responses_input(false)?;
    // A remains completed. B is explicitly incomplete, even though an
    // arguments.done event reports the exact bytes available at its boundary.
    for wire in &mut input {
        let mut payload: Value = serde_json::from_str(&wire.data)?;
        match wire.event.as_deref() {
            Some("response.output_item.done") if payload["output_index"] == 1 => {
                payload["item"]["status"] = json!("incomplete");
            }
            Some("response.completed") => {
                wire.event = Some("response.incomplete".to_owned());
                payload["type"] = json!("response.incomplete");
                payload["response"]["status"] = json!("incomplete");
                payload["response"]["output"][1]["status"] = json!("incomplete");
            }
            _ => {}
        }
        wire.data = payload.to_string();
    }
    let output = round_trip(ResponsesAdapter, &input, B)?;
    let early_done = output
        .iter()
        .position(|value| {
            value["type"] == "response.output_item.done" && value["item"]["call_id"] == "call-a"
        })
        .ok_or_else(|| anyhow::anyhow!("missing early item close"))?;
    let second_open = output
        .iter()
        .position(|value| {
            value["type"] == "response.output_item.added" && value["item"]["call_id"] == "call-b"
        })
        .ok_or_else(|| anyhow::anyhow!("missing second item open"))?;
    assert!(early_done < second_open);
    let terminal = output
        .last()
        .ok_or_else(|| anyhow::anyhow!("missing terminal"))?;
    assert_eq!(terminal["type"], "response.incomplete");
    assert_eq!(terminal["response"]["output"][0]["status"], "completed");
    assert_eq!(terminal["response"]["output"][1]["status"], "incomplete");
    assert_eq!(terminal["response"]["usage"]["output_tokens"], 5);
    Ok(())
}

#[test]
fn native_responses_rejects_tool_identity_and_completion_contradictions() -> Result<()> {
    for (kind, index, field, replacement) in [
        (
            "response.output_item.added",
            1,
            "duplicate_call",
            json!("call-a"),
        ),
        (
            "response.function_call_arguments.delta",
            1,
            "item_id",
            json!("item-a"),
        ),
        (
            "response.function_call_arguments.done",
            0,
            "arguments",
            json!("{}"),
        ),
        (
            "response.output_item.done",
            0,
            "item_arguments",
            json!("{}"),
        ),
        (
            "response.completed",
            0,
            "terminal_status",
            json!("incomplete"),
        ),
    ] {
        let mut decoder = ResponsesAdapter.stream_decoder();
        let mut changed = false;
        for mut wire in responses_input(true)? {
            let mut payload: Value = serde_json::from_str(&wire.data)?;
            if !changed
                && wire.event.as_deref() == Some(kind)
                && payload["output_index"].as_u64().unwrap_or(0) == index
            {
                match field {
                    "duplicate_call" => payload["item"]["call_id"] = replacement.clone(),
                    "item_arguments" => payload["item"]["arguments"] = replacement.clone(),
                    "terminal_status" => {
                        payload["response"]["output"][0]["status"] = replacement.clone()
                    }
                    _ => payload[field] = replacement.clone(),
                }
                wire.data = payload.to_string();
                changed = true;
            }
            let parts = decoder.decode(&wire);
            if changed {
                assert!(parts.is_err(), "accepted contradiction {field}");
                assert!(decoder.pending_terminal().is_none());
                break;
            }
            assert!(parts.is_ok());
        }
        assert!(changed, "missing fixture event {kind}");
    }
    Ok(())
}

#[test]
fn responses_snapshot_only_tools_do_not_amplify_the_terminal_wire_batch() -> Result<()> {
    use bitrouter_ai::wire::DEFAULT_JSON_LIMIT;
    let mut decoder = ResponsesAdapter.stream_decoder();
    let mut encoder = ResponsesAdapter.stream_encoder("large-response", "fixture");
    let mut output = Vec::new();
    let arguments = format!(r#"{{"blob":"{}"}}"#, "x".repeat(3 * 1024 * 1024));
    assert!(arguments.len() * 6 > DEFAULT_JSON_LIMIT);
    let mut ingress = 0;
    for index in 0..2 {
        let item_id = format!("item-{index}");
        let call_id = format!("call-{index}");
        let item = json!({
            "type":"function_call","id":item_id,"call_id":call_id,"name":"lookup","arguments":"","status":"in_progress"
        });
        for wire in [
            event(
                "response.output_item.added",
                json!({"output_index":index,"item":item}),
            ),
            event(
                "response.function_call_arguments.delta",
                json!({"output_index":index,"item_id":item_id,"delta":arguments}),
            ),
        ] {
            ingress += wire.data.len();
            for part in decoder.decode(&wire)? {
                encoder.encode(&part)?;
            }
        }
        let mut item = item;
        item["arguments"] = json!(arguments);
        item["status"] = json!("completed");
        output.push(item);
    }
    let wire = event(
        "response.completed",
        json!({"response":{"id":"resp_large","status":"completed","output":output}}),
    );
    ingress += wire.data.len();
    assert!(ingress < DEFAULT_JSON_LIMIT);
    assert!(decoder.decode(&wire)?.is_empty());
    let mut terminals = 0;
    for part in decoder.finish()? {
        let batch = encoder.encode(&part)?;
        let size: usize = batch
            .iter()
            .map(SseFrame::to_wire)
            .collect::<std::result::Result<Vec<_>, _>>()?
            .iter()
            .map(String::len)
            .sum();
        assert!(size < DEFAULT_JSON_LIMIT);
        if part.is_terminal() {
            assert_eq!(batch.len(), 1);
            terminals += 1;
        } else {
            assert!(
                batch.is_empty(),
                "native snapshot generated synthetic done frames"
            );
        }
    }
    assert_eq!(terminals, 1);
    Ok(())
}

#[test]
fn chat_turn_boundaries_keep_large_responses_batches_bounded_and_names_stable() -> Result<()> {
    use super::super::chat_completions::ChatCompletionsAdapter;
    use bitrouter_ai::wire::DEFAULT_JSON_LIMIT;

    let arguments = format!(r#"{{"blob":"{}"}}"#, "x".repeat(3 * 1024 * 1024));
    let mut decoder = ChatCompletionsAdapter.stream_decoder();
    let mut encoder = ResponsesAdapter.stream_encoder("chat-large", "fixture");
    let mut openings = 0;
    let mut completed = 0;
    let mut terminals = 0;
    let mut encode = |parts: Vec<StreamPart>| -> Result<()> {
        for part in parts {
            if matches!(part, StreamPart::ToolCallDelta { name: Some(_), .. }) {
                openings += 1;
            }
            let batch = encoder.encode(&part)?;
            let mut size = 0;
            for frame in &batch {
                size += frame.to_wire()?.len();
                if let SseFrame::Event {
                    event: Some(kind), ..
                } = frame
                {
                    completed += usize::from(kind == "response.output_item.done");
                    terminals += usize::from(kind == "response.completed");
                }
            }
            assert!(size < DEFAULT_JSON_LIMIT);
        }
        Ok(())
    };
    for index in 0..2 {
        for text in [
            &arguments[..arguments.len() - 1],
            &arguments[arguments.len() - 1..],
        ] {
            encode(decoder.decode(&SseEvent { event: None, data: json!({
                "choices":[{"index":0,"delta":{"tool_calls":[{
                    "index":index,"id":format!("call-{index}"),"function":{"name":"lookup","arguments":text}
                }]}}]
            }).to_string() })?)?;
        }
    }
    encode(
        decoder.decode(&SseEvent {
            event: None,
            data: json!({
                "choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]
            })
            .to_string(),
        })?,
    )?;
    encode(decoder.decode(&SseEvent {
        event: None,
        data: "[DONE]".to_owned(),
    })?)?;
    encode(decoder.finish()?)?;
    assert_eq!((openings, completed, terminals), (2, 2, 1));
    Ok(())
}

#[test]
fn messages_releases_closed_tools_and_following_text_before_turn_eof() -> Result<()> {
    let mut input = messages_input(false);
    let terminal = input
        .iter()
        .position(|event| event.event.as_deref() == Some("message_delta"))
        .ok_or_else(|| anyhow::anyhow!("missing fixture terminal"))?;
    input.splice(
        terminal..terminal,
        [
            event(
                "content_block_start",
                json!({"index":2,"content_block":{"type":"text","text":""}}),
            ),
            event(
                "content_block_delta",
                json!({"index":2,"delta":{"type":"text_delta","text":"already visible"}}),
            ),
            event("content_block_stop", json!({"index":2})),
        ],
    );
    let mut decoder = MessagesAdapter.stream_decoder();
    let mut encoder = MessagesAdapter.stream_encoder("latency", "fixture");
    let mut closed_tools = 0;
    let mut text_seen = false;
    for wire in input {
        let before_terminal = wire.event.as_deref() != Some("message_delta")
            && wire.event.as_deref() != Some("message_stop");
        for part in decoder.decode(&wire)? {
            for frame in encoder.encode(&part)? {
                if let SseFrame::Event { data, .. } = frame {
                    let frame: Value = serde_json::from_str(&data)?;
                    if frame["type"] == "content_block_stop" && before_terminal {
                        closed_tools += 1;
                    }
                    if frame["delta"]["text"] == "already visible" {
                        assert!(before_terminal);
                        assert!(closed_tools >= 2);
                        text_seen = true;
                    }
                    assert_ne!(frame["type"], "message_stop");
                }
            }
        }
    }
    assert!(text_seen);
    assert!(decoder.pending_terminal().is_none());
    let mut stopped = false;
    for part in decoder.finish()? {
        for frame in encoder.encode(&part)? {
            if let SseFrame::Event { event, .. } = frame {
                stopped |= event.as_deref() == Some("message_stop");
            }
        }
    }
    assert!(stopped);
    Ok(())
}

#[test]
fn messages_statusless_terminal_snapshot_waits_for_response_outcome() -> Result<()> {
    use bitrouter_ai::types::ToolCallBoundary;
    for status in ["completed", "incomplete"] {
        let mut encoder = MessagesAdapter.stream_encoder("statusless", "fixture");
        let mut frames = Vec::new();
        for part in [
            StreamPart::ToolCallDelta {
                id: "call-statusless".into(),
                name: Some("lookup".into()),
                arguments: "{}".into(),
                provider_metadata: Default::default(),
            },
            StreamPart::ToolCallEnd {
                id: "call-statusless".into(),
                boundary: ToolCallBoundary::Arguments,
            },
            StreamPart::ToolCallEnd {
                id: "call-statusless".into(),
                boundary: ToolCallBoundary::ResponsesItem {
                    status: None,
                    terminal_only: true,
                },
            },
        ] {
            frames.extend(encoder.encode(&part)?);
        }
        assert!(!frames.iter().any(|frame| matches!(frame,
            SseFrame::Event { event: Some(event), .. } if event == "content_block_start"
        )));
        frames.extend(encoder.encode(&StreamPart::ResponseCompleted {
            id: "resp-statusless".into(),
            source_protocol: ApiProtocol::Responses,
            status: status.into(),
            usage: None,
            response_output_commitment: None,
        })?);
        let tool_starts = frames
            .iter()
            .filter(|frame| match frame {
                SseFrame::Event { data, .. } => data.contains("call-statusless"),
                _ => false,
            })
            .count();
        assert_eq!(tool_starts, usize::from(status == "completed"));
        assert!(frames.iter().any(|frame| matches!(frame,
            SseFrame::Event { event: Some(event), .. } if event == "message_stop"
        )));
    }
    Ok(())
}

#[test]
fn nine_mib_projected_close_uses_individually_bounded_sse_events() -> Result<()> {
    use bitrouter_ai::{types::ToolCallBoundary, wire::DEFAULT_JSON_LIMIT};
    let args = format!(r#"{{"x":"{}"}}"#, "x".repeat(9 * 1024 * 1024));
    let mut encoder = ResponsesAdapter.stream_encoder("single-large", "fixture");
    encoder.encode(&StreamPart::ToolCallDelta {
        id: "large".into(),
        name: Some("lookup".into()),
        arguments: args,
        provider_metadata: Default::default(),
    })?;
    let frames = encoder.encode(&StreamPart::ToolCallEnd {
        id: "large".into(),
        boundary: ToolCallBoundary::Turn { complete: true },
    })?;
    let events = frames
        .iter()
        .map(SseFrame::to_wire)
        .collect::<std::result::Result<Vec<_>, _>>()?;
    assert_eq!(events.len(), 2);
    assert!(events.iter().map(String::len).sum::<usize>() > DEFAULT_JSON_LIMIT);
    assert!(events.iter().all(|event| event.len() <= DEFAULT_JSON_LIMIT));
    Ok(())
}
