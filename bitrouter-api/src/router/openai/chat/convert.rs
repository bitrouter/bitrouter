//! Conversion between OpenAI Chat Completions format and core LanguageModel types.

use bitrouter_core::models::language::{
    call_options::LanguageModelCallOptions,
    content::LanguageModelContent,
    finish_reason::LanguageModelFinishReason,
    generate_result::LanguageModelGenerateResult,
    prompt::{LanguageModelMessage, LanguageModelUserContent},
    stream_part::LanguageModelStreamPart,
};

use super::types::*;
use crate::util::{generate_id, now_unix};

/// Extracts the model name from a chat completion request body.
pub fn extract_model_name(request: &ChatCompletionRequest) -> &str {
    &request.model
}

/// Converts a [`ChatCompletionRequest`] into [`LanguageModelCallOptions`].
pub fn to_call_options(request: ChatCompletionRequest) -> LanguageModelCallOptions {
    let prompt = request
        .messages
        .into_iter()
        .map(|msg| match msg.role.as_str() {
            "system" => LanguageModelMessage::System {
                content: content_to_string(msg.content),
                provider_options: None,
            },
            "assistant" => LanguageModelMessage::Assistant {
                content: vec![
                    bitrouter_core::models::language::prompt::LanguageModelAssistantContent::Text {
                        text: content_to_string(msg.content),
                        provider_options: None,
                    },
                ],
                provider_options: None,
            },
            _ => LanguageModelMessage::User {
                content: content_to_parts(msg.content),
                provider_options: None,
            },
        })
        .collect();

    LanguageModelCallOptions {
        prompt,
        stream: request.stream,
        max_output_tokens: request.max_completion_tokens.or(request.max_tokens),
        temperature: request.temperature,
        top_p: request.top_p,
        top_k: None,
        stop_sequences: request.stop,
        presence_penalty: request.presence_penalty,
        frequency_penalty: request.frequency_penalty,
        response_format: None,
        seed: request.seed,
        tools: None,
        tool_choice: None,
        include_raw_chunks: None,
        abort_signal: None,
        headers: None,
        provider_options: None,
    }
}

/// Converts a [`LanguageModelGenerateResult`] into a [`ChatCompletionResponse`].
pub fn from_generate_result(
    model_id: &str,
    result: LanguageModelGenerateResult,
) -> ChatCompletionResponse {
    let text = extract_text_content(&result.content);
    let finish_reason = map_finish_reason(&result.finish_reason);
    let prompt_tokens = result.usage.input_tokens.total.unwrap_or(0);
    let completion_tokens = result.usage.output_tokens.total.unwrap_or(0);

    ChatCompletionResponse {
        id: format!("chatcmpl-{}", generate_id()),
        object: "chat.completion".to_owned(),
        created: now_unix(),
        model: model_id.to_owned(),
        choices: vec![ChatCompletionChoice {
            index: 0,
            message: ChatCompletionChoiceMessage {
                role: "assistant".to_owned(),
                content: Some(text),
            },
            finish_reason: Some(finish_reason),
        }],
        usage: Some(ChatCompletionUsage {
            prompt_tokens,
            completion_tokens,
            total_tokens: prompt_tokens + completion_tokens,
        }),
    }
}

/// Converts a [`LanguageModelStreamPart`] into a [`ChatCompletionChunk`].
pub fn stream_part_to_chunk(
    model_id: &str,
    stream_id: &str,
    part: &LanguageModelStreamPart,
) -> Option<ChatCompletionChunk> {
    match part {
        LanguageModelStreamPart::TextDelta { delta, .. } => Some(ChatCompletionChunk {
            id: stream_id.to_owned(),
            object: "chat.completion.chunk".to_owned(),
            created: now_unix(),
            model: model_id.to_owned(),
            choices: vec![ChatCompletionChunkChoice {
                index: 0,
                delta: ChatCompletionChunkDelta {
                    role: None,
                    content: Some(delta.clone()),
                },
                finish_reason: None,
            }],
            usage: None,
        }),
        LanguageModelStreamPart::Finish {
            finish_reason,
            usage,
            ..
        } => {
            let prompt_tokens = usage.input_tokens.total.unwrap_or(0);
            let completion_tokens = usage.output_tokens.total.unwrap_or(0);
            Some(ChatCompletionChunk {
                id: stream_id.to_owned(),
                object: "chat.completion.chunk".to_owned(),
                created: now_unix(),
                model: model_id.to_owned(),
                choices: vec![ChatCompletionChunkChoice {
                    index: 0,
                    delta: ChatCompletionChunkDelta {
                        role: None,
                        content: None,
                    },
                    finish_reason: Some(map_finish_reason(finish_reason)),
                }],
                usage: Some(ChatCompletionUsage {
                    prompt_tokens,
                    completion_tokens,
                    total_tokens: prompt_tokens + completion_tokens,
                }),
            })
        }
        _ => None,
    }
}

// ── Helpers ─────────────────────────────────────────────────────────────────

fn content_to_string(content: Option<ChatMessageContent>) -> String {
    match content {
        Some(ChatMessageContent::Text(s)) => s,
        Some(ChatMessageContent::Parts(parts)) => parts
            .into_iter()
            .map(|p| match p {
                ChatContentPart::Text { text } => text,
            })
            .collect::<Vec<_>>()
            .join(""),
        None => String::new(),
    }
}

fn content_to_parts(content: Option<ChatMessageContent>) -> Vec<LanguageModelUserContent> {
    match content {
        Some(ChatMessageContent::Text(s)) => vec![LanguageModelUserContent::Text {
            text: s,
            provider_options: None,
        }],
        Some(ChatMessageContent::Parts(parts)) => parts
            .into_iter()
            .map(|p| match p {
                ChatContentPart::Text { text } => LanguageModelUserContent::Text {
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
        LanguageModelFinishReason::Stop => "stop".to_owned(),
        LanguageModelFinishReason::Length => "length".to_owned(),
        LanguageModelFinishReason::FunctionCall => "tool_calls".to_owned(),
        LanguageModelFinishReason::ContentFilter => "content_filter".to_owned(),
        LanguageModelFinishReason::Error => "stop".to_owned(),
        LanguageModelFinishReason::Other(_) => "stop".to_owned(),
    }
}
