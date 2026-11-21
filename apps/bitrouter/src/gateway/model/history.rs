//! Shared model-history validation and tool-result projection.
//!
//! These helpers prepare model input without modifying the durable harness
//! journal. Both scheduling and execution consume the same validation rules.

use bitrouter_ai::types::{Content, Message};
use std::collections::HashSet;

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
