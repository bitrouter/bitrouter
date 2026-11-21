//! Provider-private payload classification for request checkers.

use bitrouter_ai::types::{Content, ProviderMetadata};

fn metadata(content: &Content) -> &ProviderMetadata {
    match content {
        Content::Text {
            provider_metadata, ..
        }
        | Content::Reasoning {
            provider_metadata, ..
        }
        | Content::File {
            provider_metadata, ..
        }
        | Content::ToolCall {
            provider_metadata, ..
        }
        | Content::ToolResult {
            provider_metadata, ..
        }
        | Content::Source {
            provider_metadata, ..
        }
        | Content::ToolApprovalRequest {
            provider_metadata, ..
        }
        | Content::ToolApprovalResponse {
            provider_metadata, ..
        } => provider_metadata,
    }
}

/// Opaque encrypted thinking must never be projected as checker-readable text.
pub fn is_opaque_reasoning(content: &Content) -> bool {
    matches!(content, Content::Reasoning { .. })
        && metadata(content)
            .get("anthropic")
            .is_some_and(|value| value.get("redactedThinking").is_some())
}

/// Opaque payloads excluded from checker text, including metadata attached to
/// readable summaries. A readable summary still needs to be checked.
pub(crate) fn has_opaque_payload(content: &Content) -> bool {
    matches!(content, Content::Reasoning { native: Some(bitrouter_ai::types::NativeReasoning::Responses(item)), .. } if item.get("encrypted_content").is_some_and(|value| !value.is_null()))
        || is_opaque_reasoning(content)
        || metadata(content)
            .get("openai")
            .and_then(|fields| fields.get("reasoningItem"))
            .and_then(|item| item.get("encrypted_content"))
            .is_some_and(|value| !value.is_null())
}
