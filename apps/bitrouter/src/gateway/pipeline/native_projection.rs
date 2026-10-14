//! Deterministic, source-bound execution views for portable Responses history.
//! This module does not authenticate provenance or grant context authority.

use bitrouter_ai::types::{Content, Message, NativeReasoning, Role};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::native_context::{ORIGIN_FIELD, ORIGIN_NAMESPACE, requires_origin_validation};
use super::native_continuation::{CONTINUATION_FIELD, clear_continuations, requires_stored_state};
use crate::gateway::error::BitrouterError;
use crate::gateway::error::Result;

/// Coordinates in the original message sequence, before any removal.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PartLocation {
    /// Zero-based source message index.
    pub message: usize,
    /// Zero-based content index in that message.
    pub part: usize,
}

/// Audit commitments contain no reasoning payload or provenance secret.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectionRecord {
    /// SHA-256 of the complete original message sequence.
    pub source_sha256: String,
    /// SHA-256 of the exact projected message sequence.
    pub view_sha256: String,
    /// Removed Responses reasoning, in source order.
    pub reasoning_parts: Vec<PartLocation>,
    /// Source message indices made empty by the projection.
    pub removed_messages: Vec<usize>,
}

/// A candidate execution view; the caller must retain its immutable source.
pub struct Projection {
    /// Candidate messages, without a grant of execution authority.
    pub messages: Vec<Message>,
    /// Original and projected commitments plus removed coordinates.
    pub record: ProjectionRecord,
}

/// Remove only supported Responses reasoning, retaining every public/tool part.
/// Reasoning summaries remain in the immutable source, never relabelled as
/// public text that a semantic decision backend could read.
/// Callers must authenticate the entire original
/// history and obtain explicit owner permission before activating this view.
/// Remote state and other signed/provider-executed semantics are not portable.
/// Responses represents reasoning separately from ordinary messages. This view
/// starts a new model context without replaying a different model's sealed items.
/// <https://developers.openai.com/api/docs/guides/reasoning>
pub fn responses_reasoning(messages: &[Message]) -> Result<Option<Projection>> {
    let mut projected = Vec::with_capacity(messages.len());
    let mut reasoning_parts = Vec::new();
    let mut removed_messages = Vec::new();
    for (message_index, message) in messages.iter().enumerate() {
        let mut content = Vec::with_capacity(message.content.len());
        for (part_index, part) in message.content.iter().enumerate() {
            if requires_stored_state(part)
                || matches!(
                    part,
                    Content::ToolCall {
                        provider_executed: true,
                        ..
                    } | Content::ToolCall { dynamic: true, .. }
                        | Content::ToolResult { dynamic: true, .. }
                )
            {
                return Ok(None);
            }
            if let Content::Reasoning {
                native: Some(NativeReasoning::Responses(item)),
                provider_metadata,
                ..
            } = part
            {
                if message.role != Role::Assistant
                    || item.get("type").and_then(serde_json::Value::as_str) != Some("reasoning")
                    || item.as_object().is_none_or(|fields| {
                        fields.keys().any(|key| {
                            !matches!(
                                key.as_str(),
                                "type"
                                    | "id"
                                    | "summary"
                                    | "content"
                                    | "encrypted_content"
                                    | "status"
                            )
                        })
                    })
                    || provider_metadata.iter().any(|(namespace, fields)| {
                        namespace != ORIGIN_NAMESPACE
                            || fields.as_object().is_none_or(|fields| {
                                fields
                                    .keys()
                                    .any(|key| key != ORIGIN_FIELD && key != CONTINUATION_FIELD)
                            })
                    })
                {
                    return Ok(None);
                }
                reasoning_parts.push(PartLocation {
                    message: message_index,
                    part: part_index,
                });
            } else {
                if requires_origin_validation(part) {
                    return Ok(None);
                }
                content.push(part.clone());
            }
        }
        // A changed causal prefix cannot reuse a native continuation handle.
        clear_continuations(&mut content);
        if content.is_empty() && !message.content.is_empty() {
            removed_messages.push(message_index);
        } else {
            projected.push(Message {
                role: message.role,
                content,
            });
        }
    }
    if reasoning_parts.is_empty() {
        return Ok(None);
    }
    Ok(Some(Projection {
        record: ProjectionRecord {
            source_sha256: commitment(messages)?,
            view_sha256: commitment(&projected)?,
            reasoning_parts,
            removed_messages,
        },
        messages: projected,
    }))
}

fn commitment(messages: &[Message]) -> Result<String> {
    serde_json::to_vec(messages)
        .map(|bytes| {
            Sha256::digest(bytes)
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect()
        })
        .map_err(|error| BitrouterError::bad_request(error.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn reasoning() -> Content {
        Content::Reasoning {
            text: "private summary".into(),
            native: Some(NativeReasoning::Responses(json!({
                "type": "reasoning", "summary": [{"type": "summary_text", "text": "private summary"}],
                "encrypted_content": "private payload"
            }))),
            provider_metadata: Default::default(),
        }
    }

    #[test]
    fn public_history_survives_without_relabelling_reasoning() -> Result<()> {
        let source = vec![
            Message::text(Role::User, "initial request"),
            Message {
                role: Role::Assistant,
                content: vec![reasoning()],
            },
            Message {
                role: Role::Assistant,
                content: vec![
                    reasoning(),
                    Content::Text {
                        text: "public answer".into(),
                        provider_metadata: Default::default(),
                    },
                ],
            },
            Message::text(Role::User, "continue"),
        ];
        let projected = responses_reasoning(&source)?
            .ok_or_else(|| BitrouterError::internal("missing projection"))?;
        assert_eq!(
            projected.messages,
            vec![
                source[0].clone(),
                Message::text(Role::Assistant, "public answer"),
                source[3].clone()
            ]
        );
        assert_eq!(projected.record.removed_messages, [1]);
        assert_eq!(
            projected.record.reasoning_parts,
            [
                PartLocation {
                    message: 1,
                    part: 0
                },
                PartLocation {
                    message: 2,
                    part: 0
                }
            ]
        );
        assert_ne!(projected.record.source_sha256, projected.record.view_sha256);
        assert!(responses_reasoning(&projected.messages)?.is_none());
        assert_eq!(source[1].content, [reasoning()]);
        Ok(())
    }

    #[test]
    fn nonportable_metadata_cannot_be_hidden() -> Result<()> {
        for (namespace, fields) in [
            ("bitrouter", json!({"nativeContextRequired":"remote"})),
            ("google", json!({"thoughtSignature":"signed"})),
            ("unknown", json!({"private":"future semantic"})),
        ] {
            let mut part = reasoning();
            super::super::native_context::metadata_mut(&mut part).insert(namespace.into(), fields);
            assert!(
                responses_reasoning(&[Message {
                    role: Role::Assistant,
                    content: vec![part]
                }])?
                .is_none()
            );
        }
        let mut part = reasoning();
        if let Content::Reasoning {
            native: Some(NativeReasoning::Responses(item)),
            ..
        } = &mut part
        {
            item["future_remote_state"] = json!("nonportable");
        }
        assert!(
            responses_reasoning(&[Message {
                role: Role::Assistant,
                content: vec![part]
            }])?
            .is_none()
        );
        Ok(())
    }

    #[test]
    fn tool_arguments_and_results_are_exact_and_remote_tools_are_rejected() -> Result<()> {
        let call = Content::ToolCall {
            id: "call-1".into(),
            name: "shell".into(),
            arguments: "{\"command\":\"pwd\"}".into(),
            provider_executed: false,
            dynamic: false,
            provider_metadata: Default::default(),
        };
        let result = Message {
            role: Role::Tool,
            content: vec![Content::ToolResult {
                call_id: "call-1".into(),
                tool_name: Some("shell".into()),
                output: bitrouter_ai::types::ToolResultOutput::Text {
                    value: "/workspace".into(),
                },
                dynamic: false,
                provider_metadata: Default::default(),
            }],
        };
        let source = vec![
            Message {
                role: Role::Assistant,
                content: vec![reasoning(), call.clone()],
            },
            result.clone(),
        ];
        let projected = responses_reasoning(&source)?
            .ok_or_else(|| BitrouterError::internal("missing projection"))?;
        assert_eq!(
            projected.messages,
            [
                Message {
                    role: Role::Assistant,
                    content: vec![call.clone()]
                },
                result
            ]
        );
        let mut remote = call;
        if let Content::ToolCall {
            provider_executed, ..
        } = &mut remote
        {
            *provider_executed = true;
        }
        assert!(
            responses_reasoning(&[Message {
                role: Role::Assistant,
                content: vec![reasoning(), remote]
            }])?
            .is_none()
        );
        Ok(())
    }
}
