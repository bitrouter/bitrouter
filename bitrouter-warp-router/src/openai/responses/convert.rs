//! Conversion between OpenAI Responses format and core LanguageModel types.

use bitrouter_core::models::language::{
    call_options::LanguageModelCallOptions,
    content::LanguageModelContent,
    generate_result::LanguageModelGenerateResult,
    prompt::{LanguageModelMessage, LanguageModelUserContent},
    stream_part::LanguageModelStreamPart,
};

use super::types::*;
use crate::util::{generate_id, now_unix};

/// Extracts the model name from a responses request body.
pub fn extract_model_name(request: &ResponsesRequest) -> &str {
    &request.model
}

/// Converts a [`ResponsesRequest`] into [`LanguageModelCallOptions`].
pub fn to_call_options(request: ResponsesRequest) -> LanguageModelCallOptions {
    let prompt = match request.input {
        ResponsesInput::Text(text) => vec![LanguageModelMessage::User {
            content: vec![LanguageModelUserContent::Text {
                text,
                provider_options: None,
            }],
            provider_options: None,
        }],
        ResponsesInput::Messages(messages) => messages
            .into_iter()
            .map(|msg| match msg.role.as_str() {
                "system" | "developer" => LanguageModelMessage::System {
                    content: input_content_to_string(msg.content),
                    provider_options: None,
                },
                _ => LanguageModelMessage::User {
                    content: input_content_to_parts(msg.content),
                    provider_options: None,
                },
            })
            .collect(),
    };

    LanguageModelCallOptions {
        prompt,
        stream: request.stream,
        max_output_tokens: request.max_output_tokens,
        temperature: request.temperature,
        top_p: request.top_p,
        top_k: None,
        stop_sequences: None,
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

/// Converts a [`LanguageModelGenerateResult`] into a [`ResponsesResponse`].
pub fn from_generate_result(
    model_id: &str,
    result: LanguageModelGenerateResult,
) -> ResponsesResponse {
    let text = extract_text_content(&result.content);
    let input_tokens = result.usage.input_tokens.total.unwrap_or(0);
    let output_tokens = result.usage.output_tokens.total.unwrap_or(0);

    ResponsesResponse {
        id: format!("resp-{}", generate_id()),
        object: "response".to_owned(),
        created_at: now_unix(),
        model: model_id.to_owned(),
        output: vec![ResponsesOutputItem {
            id: format!("msg-{}", generate_id()),
            item_type: "message".to_owned(),
            role: "assistant".to_owned(),
            content: vec![ResponsesOutputContent {
                content_type: "output_text".to_owned(),
                text,
            }],
            status: "completed".to_owned(),
        }],
        usage: Some(ResponsesUsage {
            input_tokens,
            output_tokens,
            total_tokens: input_tokens + output_tokens,
        }),
        status: "completed".to_owned(),
    }
}

/// Converts a [`LanguageModelStreamPart`] into a [`ResponsesStreamEvent`].
pub fn stream_part_to_event(part: &LanguageModelStreamPart) -> Option<ResponsesStreamEvent> {
    match part {
        LanguageModelStreamPart::TextDelta { delta, .. } => Some(ResponsesStreamEvent {
            event_type: "response.output_text.delta".to_owned(),
            item_id: None,
            output_index: Some(0),
            content_index: Some(0),
            delta: Some(delta.clone()),
        }),
        LanguageModelStreamPart::Finish { .. } => Some(ResponsesStreamEvent {
            event_type: "response.completed".to_owned(),
            item_id: None,
            output_index: None,
            content_index: None,
            delta: None,
        }),
        _ => None,
    }
}

// ── Helpers ─────────────────────────────────────────────────────────────────

fn input_content_to_string(content: Option<ResponsesInputContent>) -> String {
    match content {
        Some(ResponsesInputContent::Text(s)) => s,
        Some(ResponsesInputContent::Parts(parts)) => parts
            .into_iter()
            .map(|p| match p {
                ResponsesInputContentPart::InputText { text } => text,
            })
            .collect::<Vec<_>>()
            .join(""),
        None => String::new(),
    }
}

fn input_content_to_parts(content: Option<ResponsesInputContent>) -> Vec<LanguageModelUserContent> {
    match content {
        Some(ResponsesInputContent::Text(s)) => vec![LanguageModelUserContent::Text {
            text: s,
            provider_options: None,
        }],
        Some(ResponsesInputContent::Parts(parts)) => parts
            .into_iter()
            .map(|p| match p {
                ResponsesInputContentPart::InputText { text } => LanguageModelUserContent::Text {
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
