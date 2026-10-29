//! Independent Claude embedded-context-to-user-message contract fixture.
use serde_json::{Value, json};
use std::io::{BufRead, Write};

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

fn emit(stream: &mut impl Write, value: &Value) -> TestResult {
    writeln!(stream, "{}", serde_json::to_string(value)?)?;
    stream.flush()?;
    Ok(())
}

fn main() -> TestResult {
    let args: Vec<_> = std::env::args().collect();
    let path = args
        .windows(2)
        .find(|pair| pair[0] == "--journal")
        .map(|pair| pair[1].clone())
        .ok_or("missing Claude journal")?;
    let declared = !args.iter().any(|arg| arg == "--no-context");
    let hold = args.iter().any(|arg| arg == "--hold");
    let reject = args.iter().any(|arg| arg == "--reject");
    let needs_permission = args.iter().any(|arg| arg == "--permission");
    let effect = std::path::PathBuf::from(&path).with_extension("effect");
    let mut journal = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    let input = std::io::stdin();
    let mut wire = std::io::stdout().lock();
    let mut pending = None;
    for line in input.lock().lines() {
        let call: Value = serde_json::from_str(&line?)?;
        emit(&mut journal, &call)?;
        let id = &call["id"];
        if call["method"].is_null() && id == "claude-permission" {
            if call["result"]["outcome"]["optionId"] == "allow" {
                std::fs::write(&effect, "approved Claude fixture effect")?;
            }
            if let Some(original) = pending.take() {
                emit(
                    &mut wire,
                    &json!({"jsonrpc":"2.0","id":original,"result":{"stopReason":"end_turn"}}),
                )?;
            }
            continue;
        }
        match call["method"].as_str() {
            Some("initialize") => emit(
                &mut wire,
                &json!({"jsonrpc":"2.0","id":id,"result":{
                    "protocolVersion":1,"agentCapabilities":{"promptCapabilities":{"embeddedContext":declared}},
                    "agentInfo":{"name":"claude-agent-acp-fixture","version":"0.75.1-contract"},"authMethods":[]
                }}),
            )?,
            Some("session/new") => emit(
                &mut wire,
                &json!({"jsonrpc":"2.0","id":id,"result":{"sessionId":"claude-skill-session"}}),
            )?,
            Some("session/prompt") => {
                let prompt = call["params"]["prompt"]
                    .as_array()
                    .ok_or("missing prompt")?;
                let mut content = Vec::new();
                let mut context = Vec::new();
                for chunk in prompt {
                    match chunk["type"].as_str() {
                        Some("text") => content.push(json!({"type":"text","text":chunk["text"]})),
                        Some("resource") if declared => {
                            let uri = chunk["resource"]["uri"].as_str().ok_or("resource uri")?;
                            let text = chunk["resource"]["text"].as_str().ok_or("resource text")?;
                            // The pinned promptToClaude separates referenced text
                            // from direct user text, then appends its context.
                            content.push(json!({"type":"text","text":uri}));
                            context.push(json!({"type":"text","text":format!("\n<context ref=\"{uri}\">\n{text}\n</context>")}));
                        }
                        _ => return Err("unnegotiated Claude content type".into()),
                    }
                }
                content.extend(context);
                emit(
                    &mut journal,
                    &json!({"event":"claude_user_message","message":{"role":"user","content":content}}),
                )?;
                if reject {
                    emit(
                        &mut wire,
                        &json!({"jsonrpc":"2.0","id":id,"error":{"code":-32603,"message":"Claude fixture result unavailable after input"}}),
                    )?;
                } else if needs_permission {
                    pending = Some(id.clone());
                    emit(
                        &mut wire,
                        &json!({"jsonrpc":"2.0","id":"claude-permission","method":"session/request_permission","params":{
                            "sessionId":"claude-skill-session","toolCall":{"toolCallId":"claude-execute","title":"shell fixture effect","kind":"execute"},
                            "options":[{"optionId":"allow","name":"Allow once","kind":"allow_once"},{"optionId":"deny","name":"Reject once","kind":"reject_once"}]
                        }}),
                    )?;
                } else if hold {
                    pending = Some(id.clone());
                } else {
                    emit(
                        &mut wire,
                        &json!({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"claude-skill-session","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"claude fixture accepted"}}}}),
                    )?;
                    emit(
                        &mut wire,
                        &json!({"jsonrpc":"2.0","id":id,"result":{"stopReason":"end_turn"}}),
                    )?;
                }
            }
            Some("session/cancel") => {
                if let Some(id) = pending.take() {
                    emit(
                        &mut wire,
                        &json!({"jsonrpc":"2.0","id":id,"result":{"stopReason":"cancelled"}}),
                    )?;
                }
            }
            _ if !id.is_null() => emit(
                &mut wire,
                &json!({"jsonrpc":"2.0","id":id,"error":{"code":-32601,"message":"Claude contract fixture unsupported method"}}),
            )?,
            _ => {}
        }
    }
    Ok(())
}
