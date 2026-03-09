//! Conversion between Google Generative AI format and core LanguageModel types.

use bitrouter_core::models::language::{
    call_options::LanguageModelCallOptions,
    content::LanguageModelContent,
    finish_reason::LanguageModelFinishReason,
    generate_result::LanguageModelGenerateResult,
    prompt::{LanguageModelMessage, LanguageModelUserContent},
    stream_part::LanguageModelStreamPart,
};

use super::types::*;

/// Extracts the model name from a generate content request body.
pub fn extract_model_name(request: &GenerateContentRequest) -> &str {
    &request.model
}

/// Converts a [`GenerateContentRequest`] into [`LanguageModelCallOptions`].
pub fn to_call_options(request: GenerateContentRequest) -> LanguageModelCallOptions {
    let mut prompt: Vec<LanguageModelMessage> = Vec::new();

    // Google system instruction is a top-level field.
    if let Some(system) = request.system_instruction
        && let Some(parts) = system.parts
    {
        let system_text: String = parts
            .into_iter()
            .filter_map(|p| p.text)
            .collect::<Vec<_>>()
            .join("");
        if !system_text.is_empty() {
            prompt.push(LanguageModelMessage::System {
                content: system_text,
                provider_options: None,
            });
        }
    }

    for content in request.contents {
        let message = match content.role.as_str() {
            "model" => LanguageModelMessage::Assistant {
                content: vec![
                    bitrouter_core::models::language::prompt::LanguageModelAssistantContent::Text {
                        text: google_parts_to_string(content.parts),
                        provider_options: None,
                    },
                ],
                provider_options: None,
            },
            _ => LanguageModelMessage::User {
                content: google_parts_to_user_content(content.parts),
                provider_options: None,
            },
        };
        prompt.push(message);
    }

    let (max_output_tokens, temperature, top_p, top_k, stop_sequences) =
        if let Some(config) = request.generation_config {
            (
                config.max_output_tokens,
                config.temperature,
                config.top_p,
                config.top_k,
                config.stop_sequences,
            )
        } else {
            (None, None, None, None, None)
        };

    LanguageModelCallOptions {
        prompt,
        stream: request.stream,
        max_output_tokens,
        temperature,
        top_p,
        top_k,
        stop_sequences,
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

/// Converts a [`LanguageModelGenerateResult`] into a [`GenerateContentResponse`].
pub fn from_generate_result(
    model_id: &str,
    result: LanguageModelGenerateResult,
) -> GenerateContentResponse {
    let text = extract_text_content(&result.content);
    let finish_reason = map_finish_reason(&result.finish_reason);
    let input_tokens = result.usage.input_tokens.total.unwrap_or(0);
    let output_tokens = result.usage.output_tokens.total.unwrap_or(0);

    GenerateContentResponse {
        candidates: vec![GenerateContentCandidate {
            content: GenerateContentCandidateContent {
                role: "model".to_owned(),
                parts: vec![GenerateContentPart { text }],
            },
            finish_reason,
            index: 0,
        }],
        usage_metadata: GenerateContentUsageMetadata {
            prompt_token_count: input_tokens,
            candidates_token_count: output_tokens,
            total_token_count: input_tokens + output_tokens,
        },
        model_version: Some(model_id.to_owned()),
    }
}

/// Converts a [`LanguageModelStreamPart`] into a [`GenerateContentStreamChunk`].
pub fn stream_part_to_chunk(
    model_id: &str,
    part: &LanguageModelStreamPart,
) -> Option<GenerateContentStreamChunk> {
    match part {
        LanguageModelStreamPart::TextDelta { delta, .. } => Some(GenerateContentStreamChunk {
            candidates: vec![GenerateContentStreamCandidate {
                content: GenerateContentCandidateContent {
                    role: "model".to_owned(),
                    parts: vec![GenerateContentPart {
                        text: delta.clone(),
                    }],
                },
                finish_reason: None,
                index: 0,
            }],
            usage_metadata: None,
            model_version: None,
        }),
        LanguageModelStreamPart::Finish {
            finish_reason,
            usage,
            ..
        } => {
            let input_tokens = usage.input_tokens.total.unwrap_or(0);
            let output_tokens = usage.output_tokens.total.unwrap_or(0);
            Some(GenerateContentStreamChunk {
                candidates: vec![GenerateContentStreamCandidate {
                    content: GenerateContentCandidateContent {
                        role: "model".to_owned(),
                        parts: vec![GenerateContentPart {
                            text: String::new(),
                        }],
                    },
                    finish_reason: Some(map_finish_reason(finish_reason)),
                    index: 0,
                }],
                usage_metadata: Some(GenerateContentStreamUsageMetadata {
                    prompt_token_count: input_tokens,
                    candidates_token_count: output_tokens,
                    total_token_count: input_tokens + output_tokens,
                }),
                model_version: Some(model_id.to_owned()),
            })
        }
        _ => None,
    }
}

// ── Helpers ─────────────────────────────────────────────────────────────────

fn google_parts_to_string(parts: Option<Vec<GooglePart>>) -> String {
    parts
        .unwrap_or_default()
        .into_iter()
        .filter_map(|p| p.text)
        .collect::<Vec<_>>()
        .join("")
}

fn google_parts_to_user_content(parts: Option<Vec<GooglePart>>) -> Vec<LanguageModelUserContent> {
    parts
        .unwrap_or_default()
        .into_iter()
        .filter_map(|p| {
            p.text.map(|text| LanguageModelUserContent::Text {
                text,
                provider_options: None,
            })
        })
        .collect()
}

fn extract_text_content(content: &LanguageModelContent) -> String {
    match content {
        LanguageModelContent::Text { text, .. } => text.clone(),
        _ => String::new(),
    }
}

fn map_finish_reason(reason: &LanguageModelFinishReason) -> String {
    match reason {
        LanguageModelFinishReason::Stop => "STOP".to_owned(),
        LanguageModelFinishReason::Length => "MAX_TOKENS".to_owned(),
        LanguageModelFinishReason::FunctionCall => "STOP".to_owned(),
        LanguageModelFinishReason::ContentFilter => "SAFETY".to_owned(),
        LanguageModelFinishReason::Error => "OTHER".to_owned(),
        LanguageModelFinishReason::Other(other) => other.clone(),
    }
}
