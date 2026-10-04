use std::collections::HashSet;

use bitrouter_sdk::language_model::types::ReasoningEffort;
use bitrouter_sdk::language_model::{Content, GenerationParams, Message, Prompt, Tool, ToolChoice};

pub(crate) fn build(
    model: &str,
    effort: Option<ReasoningEffort>,
    instructions: &str,
    messages: &[Message],
    tools: Vec<Tool>,
    max_bytes: usize,
) -> Result<Prompt, String> {
    validate_history(messages)?;
    let prompt = Prompt {
        model: model.to_string(),
        system: Some(instructions.to_string()),
        system_provider_metadata: Default::default(),
        messages: messages.to_vec(),
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
