//! Conversion between Anthropic Messages format and core LanguageModel types.

use bitrouter_core::models::language::{
    call_options::LanguageModelCallOptions,
    content::LanguageModelContent,
    finish_reason::LanguageModelFinishReason,
    generate_result::LanguageModelGenerateResult,
    prompt::{LanguageModelMessage, LanguageModelUserContent},
    stream_part::LanguageModelStreamPart,
};

use super::types::*;
use crate::util::generate_id;

/// Extracts the model name from a messages request body.
pub fn extract_model_name(request: &MessagesRequest) -> &str {
    &request.model
}

/// Converts a [`MessagesRequest`] into [`LanguageModelCallOptions`].
pub fn to_call_options(request: MessagesRequest) -> LanguageModelCallOptions {
    let mut prompt: Vec<LanguageModelMessage> = Vec::new();

    // Anthropic system message is a top-level field, not part of messages.
    if let Some(system) = request.system {
        prompt.push(LanguageModelMessage::System {
            content: system,
            provider_options: None,
        });
    }

    for msg in request.messages {
        let message = match msg.role.as_str() {
            "assistant" => LanguageModelMessage::Assistant {
                content: vec![
                    bitrouter_core::models::language::prompt::LanguageModelAssistantContent::Text {
                        text: anthropic_content_to_string(msg.content),
                        provider_options: None,
                    },
                ],
                provider_options: None,
            },
            _ => LanguageModelMessage::User {
                content: anthropic_content_to_parts(msg.content),
                provider_options: None,
            },
        };
        prompt.push(message);
    }

    LanguageModelCallOptions {
        prompt,
        stream: request.stream,
        max_output_tokens: Some(request.max_tokens),
        temperature: request.temperature,
        top_p: request.top_p,
        top_k: request.top_k,
        stop_sequences: request.stop_sequences,
        presence_penalty: None,
        frequency_penalty: None,
        response_format: None,
        seed: None,
        tools: None,
        tool_choice: None,
        include_raw_chunks: None,
        abort_signal: None,
        headers: None,
        provider_options: None,
    }
}

/// Converts a [`LanguageModelGenerateResult`] into a [`MessagesResponse`].
pub fn from_generate_result(
    model_id: &str,
    result: LanguageModelGenerateResult,
) -> MessagesResponse {
    let text = extract_text_content(&result.content);
    let stop_reason = map_finish_reason(&result.finish_reason);
    let input_tokens = result.usage.input_tokens.total.unwrap_or(0);
    let output_tokens = result.usage.output_tokens.total.unwrap_or(0);

    MessagesResponse {
        id: format!("msg-{}", generate_id()),
        response_type: "message".to_owned(),
        role: "assistant".to_owned(),
        content: vec![MessagesResponseContent {
            content_type: "text".to_owned(),
            text,
        }],
        model: model_id.to_owned(),
        stop_reason: Some(stop_reason),
        stop_sequence: None,
        usage: MessagesUsage {
            input_tokens,
            output_tokens,
        },
    }
}

/// Converts a [`LanguageModelStreamPart`] into a [`MessagesStreamEvent`].
pub fn stream_part_to_event(
    model_id: &str,
    part: &LanguageModelStreamPart,
) -> Option<MessagesStreamEvent> {
    match part {
        LanguageModelStreamPart::TextDelta { delta, .. } => Some(MessagesStreamEvent {
            event_type: "content_block_delta".to_owned(),
            index: Some(0),
            delta: Some(MessagesStreamDelta {
                delta_type: "text_delta".to_owned(),
                text: Some(delta.clone()),
                stop_reason: None,
            }),
            message: None,
        }),
        LanguageModelStreamPart::Finish { finish_reason, .. } => Some(MessagesStreamEvent {
            event_type: "message_delta".to_owned(),
            index: None,
            delta: Some(MessagesStreamDelta {
                delta_type: "message_delta".to_owned(),
                text: None,
                stop_reason: Some(map_finish_reason(finish_reason)),
            }),
            message: Some(MessagesStreamMessage {
                id: format!("msg-{}", generate_id()),
                model: model_id.to_owned(),
                role: "assistant".to_owned(),
            }),
        }),
        _ => None,
    }
}

// ── Helpers ─────────────────────────────────────────────────────────────────

fn anthropic_content_to_string(content: Option<AnthropicMessageContent>) -> String {
    match content {
        Some(AnthropicMessageContent::Text(s)) => s,
        Some(AnthropicMessageContent::Blocks(blocks)) => blocks
            .into_iter()
            .map(|b| match b {
                AnthropicContentBlock::Text { text } => text,
            })
            .collect::<Vec<_>>()
            .join(""),
        None => String::new(),
    }
}

fn anthropic_content_to_parts(
    content: Option<AnthropicMessageContent>,
) -> Vec<LanguageModelUserContent> {
    match content {
        Some(AnthropicMessageContent::Text(s)) => vec![LanguageModelUserContent::Text {
            text: s,
            provider_options: None,
        }],
        Some(AnthropicMessageContent::Blocks(blocks)) => blocks
            .into_iter()
            .map(|b| match b {
                AnthropicContentBlock::Text { text } => LanguageModelUserContent::Text {
                    text,
                    provider_options: None,
                },
            })
            .collect(),
        None => vec![],
    }
}

fn extract_text_content(content: &LanguageModelContent) -> String {
    match content {
        LanguageModelContent::Text { text, .. } => text.clone(),
        _ => String::new(),
    }
}

fn map_finish_reason(reason: &LanguageModelFinishReason) -> String {
    match reason {
        LanguageModelFinishReason::Stop => "end_turn".to_owned(),
        LanguageModelFinishReason::Length => "max_tokens".to_owned(),
        LanguageModelFinishReason::FunctionCall => "tool_use".to_owned(),
        LanguageModelFinishReason::ContentFilter => "end_turn".to_owned(),
        LanguageModelFinishReason::Error => "end_turn".to_owned(),
        LanguageModelFinishReason::Other(_) => "end_turn".to_owned(),
    }
}
