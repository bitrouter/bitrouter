//! Collect canonical stream events into one model result.
//!
//! Only information carried by `StreamPart` can be retained. The collector does
//! not reconstruct provider-native fields absent from that representation.

use std::collections::HashMap;

use futures::{Stream, StreamExt};

use crate::error::ModelError;
use crate::types::{
    Content, FinishReason, GenerateResult, ReasoningTextKind, StreamPart, set_provider_metadata,
};

/// Consume through EOF so a late failure cannot become a successful result.
/// Dropping this future drops its owned stream. Cancellation and HTTP deadlines
/// are supplied by the stream's owner; collection starts no I/O or retry itself.
///
/// The error type is preserved for callers that already apply gateway policy.
/// Usage reports are snapshots: the last report wins rather than being summed.
pub async fn collect_generate<S, E>(mut stream: S) -> std::result::Result<GenerateResult, E>
where
    S: Stream<Item = std::result::Result<StreamPart, E>> + Unpin,
    E: From<ModelError>,
{
    let mut content = Vec::new();
    let mut tool_indices = HashMap::<String, usize>::new();
    let mut text_block: Option<(String, usize)> = None;
    let mut reasoning_block: Option<(String, usize)> = None;
    let mut reasoning_summaries = HashMap::<usize, String>::new();
    let mut usage = None;
    let mut finish_reason = None;
    let mut response_id = None;

    while let Some(part) = stream.next().await {
        let part = part?;
        if finish_reason.is_some() && !matches!(part, StreamPart::Usage { .. }) {
            return Err(invalid(
                "model stream emitted content or another terminal after completion",
            )
            .into());
        }
        match part {
            StreamPart::TextStart { id } => {
                text_block = Some((id, content.len()));
                content.push(Content::Text {
                    text: String::new(),
                    provider_metadata: Default::default(),
                });
            }
            StreamPart::TextDelta { text } => {
                append_text(
                    &mut content,
                    text_block.as_ref().map(|(_, index)| *index),
                    text,
                    false,
                )?;
            }
            StreamPart::TextEnd { id } => {
                close_block(&mut text_block, &id)?;
            }
            StreamPart::ReasoningStart { id, .. } => {
                reasoning_block = Some((id, content.len()));
                content.push(Content::Reasoning {
                    text: String::new(),
                    provider_metadata: Default::default(),
                    native: None,
                });
            }
            StreamPart::ReasoningDelta { text, source_kind } => {
                let index = append_text(
                    &mut content,
                    reasoning_block.as_ref().map(|(_, index)| *index),
                    String::new(),
                    true,
                )?;
                if source_kind == Some(ReasoningTextKind::Summary) {
                    // Summary suffixes can arrive after reasoning-text deltas.
                    // Accumulate each lane independently until the block closes.
                    reasoning_summaries
                        .entry(index)
                        .or_default()
                        .push_str(&text);
                } else {
                    append_text(&mut content, Some(index), text, true)?;
                }
            }
            StreamPart::ReasoningEnd {
                id,
                signature,
                native,
            } => {
                let index = close_block(&mut reasoning_block, &id)?;
                if let Some(index) = index
                    && let Some(summary) = reasoning_summaries.remove(&index)
                {
                    prepend_reasoning_summary(&mut content, index, summary)?;
                }
                if let Some(native) = native {
                    let Some(Content::Reasoning {
                        native: saved,
                        text,
                        ..
                    }) = index.and_then(|index| content.get_mut(index))
                    else {
                        return Err(invalid("native reasoning has no matching block").into());
                    };
                    if native.visible_text() != *text {
                        return Err(invalid(
                            "native reasoning text differs from the observed block",
                        )
                        .into());
                    }
                    *saved = Some(native);
                }
                if let Some(signature) = signature {
                    let Some(Content::Reasoning {
                        provider_metadata, ..
                    }) = index.and_then(|index| content.get_mut(index))
                    else {
                        return Err(invalid("reasoning signature has no matching block").into());
                    };
                    set_provider_metadata(
                        provider_metadata,
                        "anthropic",
                        "signature",
                        signature.into(),
                    );
                }
            }
            StreamPart::ToolCallDelta {
                id,
                name,
                arguments,
                provider_metadata,
            } => {
                let index = *tool_indices.entry(id.clone()).or_insert_with(|| {
                    let index = content.len();
                    content.push(Content::ToolCall {
                        id,
                        name: String::new(),
                        arguments: String::new(),
                        provider_executed: false,
                        dynamic: false,
                        provider_metadata: Default::default(),
                    });
                    index
                });
                let Some(Content::ToolCall {
                    name: current_name,
                    arguments: current_arguments,
                    provider_metadata: current_metadata,
                    ..
                }) = content.get_mut(index)
                else {
                    return Err(invalid("tool delta has no matching call").into());
                };
                if let Some(name) = name {
                    if !current_name.is_empty() && *current_name != name {
                        return Err(invalid("tool delta changed the selected call name").into());
                    }
                    *current_name = name;
                }
                for (namespace, value) in provider_metadata {
                    if current_metadata
                        .get(&namespace)
                        .is_some_and(|existing| *existing != value)
                    {
                        return Err(invalid("tool delta changed native call metadata").into());
                    }
                    current_metadata.insert(namespace, value);
                }
                current_arguments.push_str(&arguments);
            }
            StreamPart::ServerToolCall {
                id,
                name,
                arguments,
                server_name,
                dynamic,
            } => {
                let mut provider_metadata = Default::default();
                if dynamic {
                    set_provider_metadata(
                        &mut provider_metadata,
                        "anthropic",
                        "type",
                        "mcp-tool-use".into(),
                    );
                    if let Some(server_name) = server_name {
                        set_provider_metadata(
                            &mut provider_metadata,
                            "anthropic",
                            "serverName",
                            server_name.into(),
                        );
                    }
                }
                content.push(Content::ToolCall {
                    id,
                    name,
                    arguments,
                    provider_executed: true,
                    dynamic,
                    provider_metadata,
                });
            }
            StreamPart::ServerToolResult {
                call_id,
                tool_name,
                output,
                dynamic,
            } => content.push(Content::ToolResult {
                call_id,
                tool_name,
                output,
                dynamic,
                provider_metadata: Default::default(),
            }),
            StreamPart::File { media_type, data } => content.push(Content::File {
                media_type,
                data,
                filename: None,
                provider_metadata: Default::default(),
            }),
            StreamPart::Source { source } => content.push(Content::Source {
                source,
                provider_metadata: Default::default(),
            }),
            StreamPart::Usage { usage: reported } => usage = Some(reported),
            StreamPart::ResponseStarted { id, .. } => response_id = Some(id),
            StreamPart::Finish { reason } => finish_reason = Some(reason),
            StreamPart::ResponseCompleted {
                id,
                status,
                usage: reported,
                ..
            } => {
                if response_id.as_ref().is_some_and(|started| *started != id) {
                    return Err(
                        invalid("terminal response id differs from the started response").into(),
                    );
                }
                finish_reason = Some(match status.as_str() {
                    "completed" => FinishReason::Stop,
                    "incomplete" => FinishReason::Length,
                    _ => {
                        return Err(
                            invalid("model stream has no successful terminal status").into()
                        );
                    }
                });
                response_id = Some(id);
                if reported.is_some() {
                    usage = reported;
                }
            }
        }
    }
    if finish_reason.is_none() {
        return Err(invalid("model stream ended without a terminal part").into());
    }
    for (index, summary) in reasoning_summaries {
        prepend_reasoning_summary(&mut content, index, summary)?;
    }
    Ok(GenerateResult {
        content,
        usage,
        finish_reason,
        response_id,
        stop_details: None,
        provider_metadata: Default::default(),
    })
}

fn invalid(message: &str) -> ModelError {
    ModelError::InvalidResponse {
        message: message.into(),
    }
}

fn prepend_reasoning_summary(
    content: &mut [Content],
    index: usize,
    mut summary: String,
) -> Result<(), ModelError> {
    let Some(Content::Reasoning { text, .. }) = content.get_mut(index) else {
        return Err(invalid("reasoning summary has no matching content block"));
    };
    summary.push_str(text);
    *text = summary;
    Ok(())
}

fn close_block(block: &mut Option<(String, usize)>, id: &str) -> Result<Option<usize>, ModelError> {
    match block.take() {
        Some((opened, index)) if opened == id => Ok(Some(index)),
        Some(_) => Err(invalid("stream block end does not match its start")),
        None => Ok(None),
    }
}

fn append_text(
    content: &mut Vec<Content>,
    index: Option<usize>,
    text: String,
    reasoning: bool,
) -> Result<usize, ModelError> {
    let actual_index = index.unwrap_or_else(|| content.len().saturating_sub(1));
    let block = if let Some(index) = index {
        content.get_mut(index)
    } else {
        content.last_mut()
    };
    match block {
        Some(Content::Text { text: current, .. }) if !reasoning => current.push_str(&text),
        Some(Content::Reasoning { text: current, .. }) if reasoning => current.push_str(&text),
        _ if index.is_some() => return Err(invalid("stream delta has no matching content block")),
        _ => {
            let index = content.len();
            content.push(if reasoning {
                Content::Reasoning {
                    text,
                    provider_metadata: Default::default(),
                    native: None,
                }
            } else {
                Content::Text {
                    text,
                    provider_metadata: Default::default(),
                }
            });
            return Ok(index);
        }
    }
    Ok(actual_index)
}

#[cfg(test)]
mod tests;
