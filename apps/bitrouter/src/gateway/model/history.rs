//! Shared model-history validation and tool-result projection.
//!
//! These helpers prepare model input without modifying the durable harness
//! journal. Both scheduling and execution consume the same validation rules.

use bitrouter_ai::types::{Content, Message};
use std::collections::HashSet;

/// Gemini's absent wire call ID must survive the corresponding result. Do not
/// copy private assistant signatures or origin seals into a tool message.
/// <https://ai.google.dev/gemini-api/docs/function-calling>
#[cfg(any(feature = "core", feature = "harness"))]
pub(crate) fn tool_result_metadata(
    messages: &[Message],
    call_id: &str,
) -> bitrouter_ai::types::ProviderMetadata {
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

/// Give native harness failures an explicit model-facing representation.
/// The canonical typed result stays in the journal; this changes only the
/// prepared prompt, before byte accounting. Chat/Responses have no native
/// error flag, so the JSON envelope preserves both status and original output.
#[cfg(any(feature = "core", test))]
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
