//! Scripted model upstream for the cross-client product behavior suite.
//!
//! The process is intentionally small and stateful: the first streamed model
//! turn asks the client to read a fixture, the second turn accepts only the
//! matching tool result and asks for a tool result, and one final turn answers.
//! Every request is written to stdout so the TypeScript harness can assert the
//! complete outbound wire shape.

use std::fmt::Write as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

const CALL_ID: &str = "call_behavior_1";
const ITEM_ID: &str = "item_behavior_1";
const TOOL_MARKER: &str = "BITROUTER_BEHAVIOR_TOOL_OK";
const FINAL_TEXT: &str = "BITROUTER_BEHAVIOR_OK";

#[derive(Clone, Copy)]
enum Protocol {
    Responses,
    Messages,
}

impl Protocol {
    fn parse(value: &str) -> Result<Self> {
        match value {
            "responses" => Ok(Self::Responses),
            "messages" => Ok(Self::Messages),
            other => bail!("unsupported outbound protocol '{other}'"),
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Responses => "responses",
            Self::Messages => "messages",
        }
    }
}

struct State {
    protocol: Protocol,
    model: String,
    requests: AtomicUsize,
}

struct Responder {
    state: Arc<State>,
}

impl Responder {
    fn reply(&self, request: &Request) -> ResponseTemplate {
        let sequence = self.state.requests.fetch_add(1, Ordering::SeqCst) + 1;
        let body: Value = match serde_json::from_slice(&request.body) {
            Ok(body) => body,
            Err(error) => {
                return invalid(format!("request {sequence} is not JSON: {error}"));
            }
        };
        println!(
            "REQUEST {}",
            json!({
                "sequence": sequence,
                "method": request.method.to_string(),
                "path": request.url.path(),
                "body": body,
            })
        );
        if body["stream"].as_bool() != Some(true) {
            return invalid(format!("request {sequence} did not set stream=true"));
        }
        if body["model"].as_str() != Some(self.state.model.as_str()) {
            return invalid(format!(
                "request {sequence} used model {:?}, expected {:?}",
                body["model"], self.state.model
            ));
        }

        let has_tool_result = contains_string(&body, CALL_ID);
        match (sequence, has_tool_result) {
            (1, false) => {
                let Some(tool) = first_tool_name(&body) else {
                    return invalid("first request did not declare a tool");
                };
                let arguments = if tool == "read" {
                    json!({ "path": "fixture.txt" }).to_string()
                } else {
                    json!({}).to_string()
                };
                stream(match self.state.protocol {
                    Protocol::Responses => responses_tool_stream(&tool, &arguments),
                    Protocol::Messages => messages_tool_stream(&tool, &arguments),
                })
            }
            (2, true) => {
                if !contains_string(&body, TOOL_MARKER) {
                    return invalid("second request did not carry the tool marker");
                }
                stream(match self.state.protocol {
                    Protocol::Responses => responses_final_stream(),
                    Protocol::Messages => messages_final_stream(),
                })
            }
            (1, true) => invalid("first request unexpectedly carried a tool result"),
            (2, false) => invalid("second request did not carry the tool result"),
            _ => invalid(format!(
                "unexpected extra model request {sequence} for {}",
                self.state.protocol.as_str()
            )),
        }
    }
}

impl Respond for Responder {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        self.reply(request)
    }
}

fn invalid(message: impl Into<String>) -> ResponseTemplate {
    let message = message.into();
    println!("ERROR {}", json!({ "message": message }));
    ResponseTemplate::new(400).set_body_json(json!({
        "error": {
            "type": "invalid_request_error",
            "message": message,
        }
    }))
}

fn stream(body: String) -> ResponseTemplate {
    ResponseTemplate::new(200)
        .insert_header("content-type", "text/event-stream")
        .set_body_string(body)
}

fn event(name: &str, body: Value) -> String {
    format!("event: {name}\ndata: {body}\n\n")
}

fn responses_tool_stream(tool: &str, arguments: &str) -> String {
    let response = json!({
        "id": "resp_behavior_1",
        "object": "response",
        "status": "in_progress",
        "model": "test-model",
        "output": [],
    });
    let item = json!({
        "id": ITEM_ID,
        "type": "function_call",
        "call_id": CALL_ID,
        "name": tool,
        "arguments": "",
        "status": "in_progress",
    });
    let completed_item = json!({
        "id": ITEM_ID,
        "type": "function_call",
        "call_id": CALL_ID,
        "name": tool,
        "arguments": arguments,
        "status": "completed",
    });
    let terminal = json!({
        "id": "resp_behavior_1",
        "object": "response",
        "status": "completed",
        "model": "test-model",
        "output": [completed_item],
        "usage": { "input_tokens": 11, "output_tokens": 7, "total_tokens": 18 },
    });
    let split = arguments.len().saturating_div(2);
    let (first, second) = arguments.split_at(split);
    let mut body = String::new();
    let _ = write!(
        body,
        "{}{}{}{}{}{}{}",
        event(
            "response.created",
            json!({ "type": "response.created", "sequence_number": 0, "response": response })
        ),
        event(
            "response.output_item.added",
            json!({
                "type": "response.output_item.added",
                "sequence_number": 1,
                "output_index": 0,
                "item": item,
            })
        ),
        event(
            "response.function_call_arguments.delta",
            json!({
                "type": "response.function_call_arguments.delta",
                "sequence_number": 2,
                "output_index": 0,
                "item_id": ITEM_ID,
                "delta": first,
            })
        ),
        event(
            "response.function_call_arguments.delta",
            json!({
                "type": "response.function_call_arguments.delta",
                "sequence_number": 3,
                "output_index": 0,
                "item_id": ITEM_ID,
                "delta": second,
            })
        ),
        event(
            "response.function_call_arguments.done",
            json!({
                "type": "response.function_call_arguments.done",
                "sequence_number": 4,
                "output_index": 0,
                "item_id": ITEM_ID,
                "name": tool,
                "arguments": arguments,
            })
        ),
        event(
            "response.output_item.done",
            json!({
                "type": "response.output_item.done",
                "sequence_number": 5,
                "output_index": 0,
                "item": completed_item,
            })
        ),
        event(
            "response.completed",
            json!({ "type": "response.completed", "sequence_number": 6, "response": terminal })
        ),
    );
    body
}

fn responses_final_stream() -> String {
    let item = json!({
        "id": "msg_behavior_final",
        "type": "message",
        "status": "completed",
        "role": "assistant",
        "content": [{ "type": "output_text", "text": FINAL_TEXT }],
    });
    let terminal = json!({
        "id": "resp_behavior_2",
        "object": "response",
        "status": "completed",
        "model": "test-model",
        "output": [item],
        "usage": { "input_tokens": 13, "output_tokens": 3, "total_tokens": 16 },
    });
    let mut body = String::new();
    let _ = write!(
        body,
        "{}{}{}{}{}{}{}{}",
        event(
            "response.created",
            json!({
                "type": "response.created",
                "sequence_number": 0,
                "response": {
                    "id": "resp_behavior_2",
                    "object": "response",
                    "status": "in_progress",
                    "model": "test-model",
                    "output": [],
                },
            })
        ),
        event(
            "response.output_item.added",
            json!({
                "type": "response.output_item.added",
                "sequence_number": 1,
                "output_index": 0,
                "item": {
                    "id": "msg_behavior_final",
                    "type": "message",
                    "status": "in_progress",
                    "role": "assistant",
                    "content": [],
                },
            })
        ),
        event(
            "response.content_part.added",
            json!({
                "type": "response.content_part.added",
                "sequence_number": 2,
                "item_id": "msg_behavior_final",
                "output_index": 0,
                "content_index": 0,
                "part": { "type": "output_text", "text": "" },
            })
        ),
        event(
            "response.output_text.delta",
            json!({
                "type": "response.output_text.delta",
                "sequence_number": 3,
                "item_id": "msg_behavior_final",
                "output_index": 0,
                "content_index": 0,
                "delta": FINAL_TEXT,
            })
        ),
        event(
            "response.output_text.done",
            json!({
                "type": "response.output_text.done",
                "sequence_number": 4,
                "item_id": "msg_behavior_final",
                "output_index": 0,
                "content_index": 0,
                "text": FINAL_TEXT,
            })
        ),
        event(
            "response.content_part.done",
            json!({
                "type": "response.content_part.done",
                "sequence_number": 5,
                "item_id": "msg_behavior_final",
                "output_index": 0,
                "content_index": 0,
                "part": { "type": "output_text", "text": FINAL_TEXT },
            })
        ),
        event(
            "response.output_item.done",
            json!({
                "type": "response.output_item.done",
                "sequence_number": 6,
                "output_index": 0,
                "item": item,
            })
        ),
        event(
            "response.completed",
            json!({ "type": "response.completed", "sequence_number": 7, "response": terminal })
        ),
    );
    body
}

fn messages_tool_stream(tool: &str, arguments: &str) -> String {
    let split = arguments.len().saturating_div(2);
    let (first, second) = arguments.split_at(split);
    let mut body = String::new();
    let _ = write!(
        body,
        "{}{}{}{}{}{}{}{}",
        event(
            "message_start",
            json!({
                "type": "message_start",
                "message": {
                    "id": "msg_behavior_1",
                    "type": "message",
                    "role": "assistant",
                    "model": "test-model",
                    "content": [],
                    "stop_reason": null,
                    "stop_sequence": null,
                    "usage": { "input_tokens": 11, "output_tokens": 0 },
                },
            })
        ),
        event(
            "content_block_start",
            json!({
                "type": "content_block_start",
                "index": 0,
                "content_block": {
                    "type": "tool_use",
                    "id": CALL_ID,
                    "name": tool,
                    "input": {},
                },
            })
        ),
        event(
            "content_block_delta",
            json!({
                "type": "content_block_delta",
                "index": 0,
                "delta": { "type": "input_json_delta", "partial_json": first },
            })
        ),
        event(
            "content_block_delta",
            json!({
                "type": "content_block_delta",
                "index": 0,
                "delta": { "type": "input_json_delta", "partial_json": second },
            })
        ),
        event(
            "content_block_stop",
            json!({ "type": "content_block_stop", "index": 0 })
        ),
        event(
            "message_delta",
            json!({
                "type": "message_delta",
                "delta": { "stop_reason": "tool_use", "stop_sequence": null },
                "usage": { "output_tokens": 7 },
            })
        ),
        event("message_stop", json!({ "type": "message_stop" })),
        String::new(),
    );
    body
}

fn messages_final_stream() -> String {
    let mut body = String::new();
    let _ = write!(
        body,
        "{}{}{}{}{}{}{}",
        event(
            "message_start",
            json!({
                "type": "message_start",
                "message": {
                    "id": "msg_behavior_2",
                    "type": "message",
                    "role": "assistant",
                    "model": "test-model",
                    "content": [],
                    "stop_reason": null,
                    "stop_sequence": null,
                    "usage": { "input_tokens": 13, "output_tokens": 0 },
                },
            })
        ),
        event(
            "content_block_start",
            json!({
                "type": "content_block_start",
                "index": 0,
                "content_block": { "type": "text", "text": "" },
            })
        ),
        event(
            "content_block_delta",
            json!({
                "type": "content_block_delta",
                "index": 0,
                "delta": { "type": "text_delta", "text": FINAL_TEXT },
            })
        ),
        event(
            "content_block_stop",
            json!({ "type": "content_block_stop", "index": 0 })
        ),
        event(
            "message_delta",
            json!({
                "type": "message_delta",
                "delta": { "stop_reason": "end_turn", "stop_sequence": null },
                "usage": { "output_tokens": 3 },
            })
        ),
        event("message_stop", json!({ "type": "message_stop" })),
        String::new(),
    );
    body
}

fn first_tool_name(body: &Value) -> Option<String> {
    body.get("tools")
        .and_then(Value::as_array)
        .and_then(|tools| {
            tools.iter().find_map(|tool| {
                tool.get("name")
                    .and_then(Value::as_str)
                    .or_else(|| {
                        tool.get("function")
                            .and_then(|function| function.get("name"))
                            .and_then(Value::as_str)
                    })
                    .map(str::to_owned)
            })
        })
}

fn contains_string(value: &Value, needle: &str) -> bool {
    match value {
        Value::String(value) => value.contains(needle),
        Value::Array(values) => values.iter().any(|value| contains_string(value, needle)),
        Value::Object(values) => values.values().any(|value| contains_string(value, needle)),
        Value::Null | Value::Bool(_) | Value::Number(_) => false,
    }
}

fn argument(args: &mut impl Iterator<Item = String>, name: &str) -> Result<String> {
    let mut values = args.skip_while(|value| value != name);
    ensure!(values.next().is_some(), "missing {name}");
    values
        .next()
        .with_context(|| format!("missing value for {name}"))
}

#[tokio::main]
async fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let protocol = Protocol::parse(&argument(&mut args, "--protocol")?)?;
    let model = argument(&mut args, "--model")?;
    let state = Arc::new(State {
        protocol,
        model,
        requests: AtomicUsize::new(0),
    });
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(Responder {
            state: Arc::clone(&state),
        })
        .mount(&server)
        .await;
    println!(
        "READY {}",
        json!({
            "uri": server.uri(),
            "protocol": protocol.as_str(),
            "call_id": CALL_ID,
            "tool_marker": TOOL_MARKER,
            "final_text": FINAL_TEXT,
        })
    );
    tokio::signal::ctrl_c()
        .await
        .context("waiting for shutdown signal")?;
    Ok(())
}
