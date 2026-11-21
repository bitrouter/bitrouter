use super::Connection;
use crate::checkpoint::{Invocation, ModelState, Thread, ToolState, Turn};
use agent_client_protocol_schema::{v1, v2};
use anyhow::{Context, Result, ensure};
use bitrouter_ai::harness::turn::{InputRequest, TurnStatus};
use bitrouter_ai::protocol::chat_completions::ChatFunctionCall;
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

pub(super) fn initialize(version: u64) -> Value {
    let info = json!({"name":"bro","version":env!("CARGO_PKG_VERSION")});
    if version == 1 {
        json!({"protocolVersion":1,"agentInfo":info,"authMethods":[],"agentCapabilities":{"loadSession":true,"promptCapabilities":{"embeddedContext":true},"sessionCapabilities":{"list":{},"resume":{}}}})
    } else {
        json!({"protocolVersion":2,"info":info,"capabilities":{"session":{"prompt":{"embeddedContext":{}}}}})
    }
}

pub(super) fn prompt(version: u64, params: &Value) -> Result<String> {
    let blocks = params["prompt"]
        .as_array()
        .context("prompt blocks required")?;
    let accepted = if version == 1 {
        v1::PromptRequest::deserialize(params)?.prompt.len()
    } else {
        v2::PromptRequest::deserialize(params)?.prompt.len()
    };
    ensure!(accepted == blocks.len(), "invalid prompt content");
    let mut parts = Vec::new();
    for block in blocks {
        parts.push(match block["type"].as_str() {
            Some("text") => block["text"].as_str().context("text required")?.to_owned(),
            Some("resource_link") => {
                format!("ACP untrusted resource link (no implicit fetch):\n{block}")
            }
            Some("resource") if block["resource"]["text"].is_string() => {
                format!("ACP untrusted embedded text resource:\n{block}")
            }
            _ => anyhow::bail!("unsupported prompt content"),
        });
    }
    Ok(parts.join("\n\n"))
}

pub(super) fn prompt_result(version: u64, turn: &Turn) -> Result<Value> {
    let meta = json!({"bitrouter":{"turnId":turn.turn_id}});
    if version == 2 {
        return Ok(json!({"messageId":turn.turn_id,"_meta":meta}));
    }
    let outcome = turn.outcome.as_ref().context("turn still pending")?;
    let reason = match outcome.status {
        TurnStatus::Completed => "end_turn",
        TurnStatus::Cancelled => "cancelled",
        _ => anyhow::bail!("turn requires inspection"),
    };
    Ok(json!({"stopReason":reason,"_meta":meta}))
}

pub(super) fn state(turn: &Turn) -> Value {
    let status = turn.status(false);
    let (state, reason) = match status {
        TurnStatus::Completed => ("idle", Some("end_turn")),
        TurnStatus::Cancelled => ("idle", Some("cancelled")),
        TurnStatus::Failed | TurnStatus::Interrupted => ("idle", Some("_bitrouter_failed")),
        TurnStatus::Running | TurnStatus::Queued | TurnStatus::Accepted => ("running", None),
        _ => ("requires_action", None),
    };
    let mut frame = json!({"sessionUpdate":"state_update","state":state,"_meta":{"bitrouter":{"turnId":turn.turn_id,"status":status}}});
    if let Some(reason) = reason {
        frame["stopReason"] = reason.into();
    }
    frame
}

pub(super) fn message(
    version: u64,
    who: &str,
    id: impl serde::Serialize,
    text: &str,
    complete: bool,
) -> Value {
    if version == 2 && complete {
        json!({"sessionUpdate":format!("{who}_message"),"messageId":id,"content":[{"type":"text","text":text}]})
    } else {
        json!({"sessionUpdate":format!("{who}_message_chunk"),"messageId":id,
            "content":{"type":"text","text":text},"_meta":{"bitrouter":{"itemId":id}}})
    }
}

pub(super) fn permission(
    version: u64,
    session: Uuid,
    input: &InputRequest,
    id: &str,
) -> Result<Value> {
    let tool = json!({"toolCallId":input.tool_id,"title":input.tool_name,"status":"pending",
        "rawInput":serde_json::from_str::<Value>(&input.arguments)?});
    let mut params = json!({"sessionId":session,"options":[
        {"optionId":"allow_once","name":"Allow once","kind":"allow_once"},
        {"optionId":"reject_once","name":"Reject once","kind":"reject_once"}],
        "_meta":{"bitrouter":{"version":1,"requestId":input.request_id,"toolItemId":input.tool_id}}});
    if version == 1 {
        params["toolCall"] = tool;
    } else {
        params["subject"] = json!({"type":"tool_call","toolCall":tool});
        params["title"] = input.tool_name.clone().into();
    }
    Ok(json!({"jsonrpc":"2.0","id":id,"method":"session/request_permission","params":params}))
}

pub(super) fn tool_status(call: &Invocation) -> &'static str {
    match &call.state {
        ToolState::Proposed => "pending",
        ToolState::ToolIntent | ToolState::Child => "in_progress",
        ToolState::ToolResult {
            effect: bitrouter_ai::harness::runtime::EffectStatus::Completed,
            ..
        } => "completed",
        ToolState::ToolResult { .. } => "failed",
    }
}

pub(super) fn tool(
    version: u64,
    call: &Invocation,
    function: &ChatFunctionCall,
    first: bool,
) -> Value {
    let mut value = json!({"sessionUpdate": if version == 1 && first {"tool_call"} else {"tool_call_update"},
        "toolCallId":call.item_id,"title":function.name,"status":tool_status(call)});
    if first {
        value["rawInput"] = serde_json::from_str(&function.arguments)
            .unwrap_or(Value::String(function.arguments.clone()));
    }
    if let ToolState::ToolResult { output, .. } = &call.state {
        value["rawOutput"] = json!(output);
    }
    value
}

async fn replay(c: &Connection, session: Uuid, update: Value) -> Result<()> {
    c.output.send(json!({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":session,"update":update}})).await?;
    Ok(())
}

pub(super) async fn history(c: &Connection, version: u64, thread: &Thread) -> Result<()> {
    let publish = |update| replay(c, thread.thread_id, update);
    for turn in &thread.turns {
        publish(message(version, "user", turn.turn_id, &turn.prompt, true)).await?;
        for step in &turn.steps {
            if let ModelState::ModelResponse { response } = &step.state {
                if let Some(text) = response.message.content.as_ref().and_then(Value::as_str) {
                    publish(message(version, "agent", step.step_id, text, true)).await?;
                }
                for (call, definition) in response.calls.iter().zip(&response.message.tool_calls) {
                    publish(tool(version, call, &definition.function, true)).await?;
                }
            } else if let Some(text) = &step.partial {
                publish(message(version, "agent", step.step_id, text, true)).await?;
            }
        }
        if version == 2 {
            publish(state(turn)).await?;
        }
    }
    Ok(())
}
