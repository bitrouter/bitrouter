use std::collections::HashSet;

use bitrouter_ai::types::ReasoningEffort;
use bitrouter_ai::types::{Content, GenerationParams, Message, Prompt, Tool, ToolChoice};

pub(crate) fn build(
    model: &str,
    effort: Option<ReasoningEffort>,
    instructions: &str,
    messages: &[Message],
    tools: Vec<Tool>,
    max_bytes: usize,
) -> Result<Prompt, String> {
    validate_history(messages)?;
    let mut messages = messages.to_vec();
    prepare_tool_results(&mut messages);
    // Native Responses reasoning is durable evidence, not replay authority.
    // This entry point has no authenticated continuation binding; start from
    // the public assistant/tool history while retaining the original records.
    for message in &mut messages {
        message.content.retain(|content| {
            !matches!(
                content,
                Content::Reasoning {
                    native: Some(_),
                    ..
                }
            )
        });
    }
    messages.retain(|message| !message.content.is_empty());
    let prompt = Prompt {
        model: model.to_string(),
        system: Some(format!(
            "{instructions}\n\n{}",
            crate::harness::instructions::POLICY
        )),
        system_provider_metadata: Default::default(),
        messages,
        tools,
        params: GenerationParams {
            reasoning_effort: effort,
            ..Default::default()
        },
        response_format: None,
        tool_choice: Some(ToolChoice::Auto),
        stream: true,
    };
    let size = serde_json::to_vec(&prompt)
        .map_err(|error| format!("cannot encode model context: {error}"))?
        .len();
    if size > max_bytes {
        return Err(format!(
            "model context is {size} bytes, above the {max_bytes}-byte limit"
        ));
    }
    Ok(prompt)
}

pub(crate) fn validate_history(messages: &[Message]) -> Result<(), String> {
    let mut pending = HashSet::new();
    for message in messages {
        let starts_call_group = message.content.iter().any(|content| {
            matches!(
                content,
                Content::ToolCall {
                    provider_executed: false,
                    ..
                }
            )
        });
        if starts_call_group && !pending.is_empty() {
            return Err("new assistant calls precede settlement of earlier calls".into());
        }
        for content in &message.content {
            match content {
                Content::ToolCall {
                    id,
                    provider_executed: false,
                    ..
                } => {
                    if id.is_empty() || !pending.insert(id.clone()) {
                        return Err("missing or duplicate tool call identity".into());
                    }
                }
                Content::ToolResult { call_id, .. } if !pending.remove(call_id) => {
                    return Err("orphaned or duplicate tool result".into());
                }
                _ => {}
            }
        }
    }
    if pending.is_empty() {
        Ok(())
    } else {
        Err("unsettled tool calls in model context".into())
    }
}

/// Give native harness failures an explicit model-facing representation.
/// The canonical typed result stays in the journal; this changes only the
/// prepared prompt, before byte accounting. Chat/Responses have no native
/// error flag, so the JSON envelope preserves both status and original output.
pub(crate) fn prepare_tool_results(messages: &mut [Message]) {
    use bitrouter_ai::types::{Role, ToolResultOutput};
    for message in messages
        .iter_mut()
        .filter(|message| message.role == Role::Tool)
    {
        for content in &mut message.content {
            if let Content::ToolResult {
                output,
                dynamic: false,
                ..
            } = content
            {
                let status = match output {
                    ToolResultOutput::ErrorText { .. } | ToolResultOutput::ErrorJson { .. } => {
                        "error"
                    }
                    ToolResultOutput::ExecutionDenied { .. } => "execution_denied",
                    _ => continue,
                };
                *output = ToolResultOutput::Json {
                    value: serde_json::json!({"status": status, "output": output}),
                };
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitrouter_ai::types::{ApiProtocol, NativeReasoning, Role, ToolResultOutput};

    #[test]
    fn prompt_preserves_tool_failure_status_without_replaying_native_reasoning()
    -> Result<(), Box<dyn std::error::Error>> {
        for output in [
            ToolResultOutput::ErrorText {
                value: "missing file".into(),
            },
            ToolResultOutput::ErrorJson {
                value: serde_json::json!({"error": "missing file"}),
            },
            ToolResultOutput::ExecutionDenied {
                reason: Some("user refused".into()),
            },
        ] {
            let messages = vec![
                Message {
                    role: Role::Assistant,
                    content: vec![
                        Content::Reasoning {
                            text: "summary".into(),
                            provider_metadata: Default::default(),
                            native: Some(NativeReasoning::Responses(
                                serde_json::json!({"type": "reasoning", "id": "rs_1", "summary": []}),
                            )),
                        },
                        Content::ToolCall {
                            id: "call_1".into(),
                            name: "read".into(),
                            arguments: "{}".into(),
                            provider_executed: false,
                            dynamic: false,
                            provider_metadata: Default::default(),
                        },
                    ],
                },
                Message {
                    role: Role::Tool,
                    content: vec![Content::ToolResult {
                        call_id: "call_1".into(),
                        tool_name: Some("read".into()),
                        output: output.clone(),
                        dynamic: false,
                        provider_metadata: Default::default(),
                    }],
                },
            ];
            let original = messages.clone();
            let prompt = build("model", None, "inspect", &messages, vec![], 8192)?;
            assert_eq!(messages, original);
            assert!(
                !prompt
                    .messages
                    .iter()
                    .flat_map(|message| &message.content)
                    .any(|content| matches!(content, Content::Reasoning { .. }))
            );
            let Content::ToolResult {
                output: ToolResultOutput::Json { value },
                ..
            } = &prompt.messages[1].content[0]
            else {
                return Err("missing result envelope".into());
            };
            assert_eq!(value["output"], serde_json::to_value(&output)?);
            assert_eq!(
                value["status"],
                if matches!(output, ToolResultOutput::ExecutionDenied { .. }) {
                    "execution_denied"
                } else {
                    "error"
                }
            );
            for protocol in [
                ApiProtocol::ChatCompletions,
                ApiProtocol::Responses,
                ApiProtocol::Messages,
            ] {
                bitrouter_ai::conversion::request_admission(&prompt, &protocol)
                    .require_admitted()?;
            }
            assert!(build("model", None, "inspect", &messages, vec![], 32).is_err());
        }
        Ok(())
    }
}
