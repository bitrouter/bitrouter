use std::collections::HashSet;

#[cfg(test)]
use bitrouter_sdk::language_model::types::ReasoningEffort;
use bitrouter_sdk::language_model::{Content, Message};
#[cfg(test)]
use bitrouter_sdk::language_model::{GenerationParams, Prompt, Tool, ToolChoice};

/// Gemini's absent wire call ID must survive the corresponding result. Do not
/// copy private assistant signatures or origin seals into a tool message.
/// <https://ai.google.dev/gemini-api/docs/function-calling>
pub(crate) fn tool_result_metadata(
    messages: &[Message],
    call_id: &str,
) -> bitrouter_sdk::language_model::ProviderMetadata {
    messages
        .iter()
        .rev()
        .flat_map(|message| message.content.iter().rev())
        .find_map(|content| match content {
            Content::ToolCall {
                id,
                provider_metadata,
                ..
            } if id == call_id => Some(provider_metadata),
            _ => None,
        })
        .and_then(|metadata| metadata.get("google"))
        .and_then(|metadata| metadata.get("functionCallId"))
        .map(|id| {
            std::collections::BTreeMap::from([(
                "google".into(),
                serde_json::json!({"functionCallId":id}),
            )])
        })
        .unwrap_or_default()
}

#[cfg(test)]
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
        system: Some(format!(
            "{instructions}\n\n{}",
            crate::harness::instructions::POLICY
        )),
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
                } if id.is_empty() || !pending.insert(id.clone()) => {
                    return Err("missing or duplicate tool call identity".into());
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
