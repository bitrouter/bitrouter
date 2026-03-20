//! Conversion between Google Generative AI format and core LanguageModel types.

use std::collections::HashMap;

use bitrouter_core::models::{
    language::{
        call_options::LanguageModelCallOptions,
        content::LanguageModelContent,
        finish_reason::LanguageModelFinishReason,
        generate_result::LanguageModelGenerateResult,
        prompt::{
            LanguageModelAssistantContent, LanguageModelMessage, LanguageModelToolResult,
            LanguageModelToolResultOutput, LanguageModelUserContent,
        },
        stream_part::LanguageModelStreamPart,
        tool::LanguageModelTool,
        tool_choice::LanguageModelToolChoice,
    },
    shared::types::JsonSchema,
};

use super::types::*;
use crate::util::generate_id;

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
        match content.role.as_str() {
            "model" => {
                let assistant_content = convert_model_parts(content.parts);
                prompt.push(LanguageModelMessage::Assistant {
                    content: assistant_content,
                    provider_options: None,
                });
            }
            _ => {
                let (user_parts, tool_results) = split_google_parts(content.parts);
                if !tool_results.is_empty() {
                    prompt.push(LanguageModelMessage::Tool {
                        content: tool_results,
                        provider_options: None,
                    });
                }
                if !user_parts.is_empty() {
                    prompt.push(LanguageModelMessage::User {
                        content: user_parts,
                        provider_options: None,
                    });
                }
            }
        }
    }

    let tools = request.tools.map(|tool_groups| {
        tool_groups
            .into_iter()
            .flat_map(|t| t.function_declarations)
            .map(|fd| {
                let schema_value = fd.parameters.unwrap_or(serde_json::json!({}));
                let input_schema: JsonSchema =
                    serde_json::from_value(schema_value).unwrap_or_default();
                LanguageModelTool::Function {
                    name: fd.name,
                    description: fd.description,
                    input_schema,
                    input_examples: vec![],
                    strict: None,
                    provider_options: None,
                }
            })
            .collect()
    });

    let tool_choice = request.tool_config.and_then(convert_tool_config);

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
        tools,
        tool_choice,
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
    let parts = extract_response_parts(&result.content);
    let finish_reason = map_finish_reason(&result.finish_reason);
    let input_tokens = result.usage.input_tokens.total.unwrap_or(0);
    let output_tokens = result.usage.output_tokens.total.unwrap_or(0);

    GenerateContentResponse {
        candidates: vec![GenerateContentCandidate {
            content: GenerateContentCandidateContent {
                role: "model".to_owned(),
                parts,
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

// ── Streaming ───────────────────────────────────────────────────────────────

/// Stateful converter that accumulates incremental tool-call data.
pub struct StreamConverter {
    model_id: String,
    pending_calls: HashMap<String, PendingFunctionCall>,
}

struct PendingFunctionCall {
    name: String,
    args_buffer: String,
}

impl StreamConverter {
    pub fn new(model_id: String) -> Self {
        Self {
            model_id,
            pending_calls: HashMap::new(),
        }
    }

    /// Converts a [`LanguageModelStreamPart`] into a [`GenerateContentStreamChunk`].
    pub fn convert(
        &mut self,
        part: &LanguageModelStreamPart,
    ) -> Option<GenerateContentStreamChunk> {
        match part {
            LanguageModelStreamPart::TextDelta { delta, .. } => Some(self.make_chunk(
                vec![GenerateContentPart {
                    text: Some(delta.clone()),
                    function_call: None,
                }],
                None,
                None,
                None,
            )),
            LanguageModelStreamPart::ToolCall {
                tool_name,
                tool_input,
                ..
            } => {
                let args: serde_json::Value = serde_json::from_str(tool_input).unwrap_or_default();
                Some(self.make_chunk(
                    vec![GenerateContentPart {
                        text: None,
                        function_call: Some(GoogleFunctionCall {
                            name: tool_name.clone(),
                            args,
                        }),
                    }],
                    None,
                    None,
                    None,
                ))
            }
            LanguageModelStreamPart::ToolInputStart { id, tool_name, .. } => {
                self.pending_calls.insert(
                    id.clone(),
                    PendingFunctionCall {
                        name: tool_name.clone(),
                        args_buffer: String::new(),
                    },
                );
                None
            }
            LanguageModelStreamPart::ToolInputDelta { id, delta, .. } => {
                if let Some(pending) = self.pending_calls.get_mut(id) {
                    pending.args_buffer.push_str(delta);
                }
                None
            }
            LanguageModelStreamPart::ToolInputEnd { id, .. } => {
                if let Some(pending) = self.pending_calls.remove(id) {
                    let args: serde_json::Value =
                        serde_json::from_str(&pending.args_buffer).unwrap_or_default();
                    Some(self.make_chunk(
                        vec![GenerateContentPart {
                            text: None,
                            function_call: Some(GoogleFunctionCall {
                                name: pending.name,
                                args,
                            }),
                        }],
                        None,
                        None,
                        None,
                    ))
                } else {
                    None
                }
            }
            LanguageModelStreamPart::Finish {
                finish_reason,
                usage,
                ..
            } => {
                let input_tokens = usage.input_tokens.total.unwrap_or(0);
                let output_tokens = usage.output_tokens.total.unwrap_or(0);
                Some(self.make_chunk(
                    vec![GenerateContentPart {
                        text: Some(String::new()),
                        function_call: None,
                    }],
                    Some(map_finish_reason(finish_reason)),
                    Some(GenerateContentStreamUsageMetadata {
                        prompt_token_count: input_tokens,
                        candidates_token_count: output_tokens,
                        total_token_count: input_tokens + output_tokens,
                    }),
                    Some(self.model_id.clone()),
                ))
            }
            _ => None,
        }
    }

    fn make_chunk(
        &self,
        parts: Vec<GenerateContentPart>,
        finish_reason: Option<String>,
        usage_metadata: Option<GenerateContentStreamUsageMetadata>,
        model_version: Option<String>,
    ) -> GenerateContentStreamChunk {
        GenerateContentStreamChunk {
            candidates: vec![GenerateContentStreamCandidate {
                content: GenerateContentCandidateContent {
                    role: "model".to_owned(),
                    parts,
                },
                finish_reason,
                index: 0,
            }],
            usage_metadata,
            model_version,
        }
    }
}

// ── Helpers ─────────────────────────────────────────────────────────────────

fn convert_model_parts(parts: Option<Vec<GooglePart>>) -> Vec<LanguageModelAssistantContent> {
    parts
        .unwrap_or_default()
        .into_iter()
        .filter_map(|p| {
            if let Some(fc) = p.function_call {
                Some(LanguageModelAssistantContent::ToolCall {
                    tool_call_id: format!("call-{}", generate_id()),
                    tool_name: fc.name,
                    input: fc.args,
                    provider_executed: None,
                    provider_options: None,
                })
            } else {
                p.text.map(|text| LanguageModelAssistantContent::Text {
                    text,
                    provider_options: None,
                })
            }
        })
        .collect()
}

fn split_google_parts(
    parts: Option<Vec<GooglePart>>,
) -> (Vec<LanguageModelUserContent>, Vec<LanguageModelToolResult>) {
    let mut user_parts = Vec::new();
    let mut tool_results = Vec::new();
    for part in parts.unwrap_or_default() {
        if let Some(fr) = part.function_response {
            let output_text = match fr.response {
                serde_json::Value::String(s) => s,
                other => serde_json::to_string(&other).unwrap_or_default(),
            };
            tool_results.push(LanguageModelToolResult::ToolResult {
                tool_call_id: String::new(),
                tool_name: fr.name,
                output: LanguageModelToolResultOutput::Text {
                    value: output_text,
                    provider_options: None,
                },
                provider_options: None,
            });
        } else if let Some(text) = part.text {
            user_parts.push(LanguageModelUserContent::Text {
                text,
                provider_options: None,
            });
        }
    }
    (user_parts, tool_results)
}

fn convert_tool_config(config: GoogleToolConfig) -> Option<LanguageModelToolChoice> {
    let fcc = config.function_calling_config?;
    let mode = fcc.mode?;
    match mode.as_str() {
        "AUTO" => Some(LanguageModelToolChoice::Auto),
        "NONE" => Some(LanguageModelToolChoice::None),
        "ANY" => {
            if let Some(names) = fcc.allowed_function_names
                && names.len() == 1
            {
                Some(LanguageModelToolChoice::Tool {
                    tool_name: names.into_iter().next().unwrap_or_default(),
                })
            } else {
                Some(LanguageModelToolChoice::Required)
            }
        }
        _ => None,
    }
}

fn extract_response_parts(content: &LanguageModelContent) -> Vec<GenerateContentPart> {
    match content {
        LanguageModelContent::Text { text, .. } => vec![GenerateContentPart {
            text: Some(text.clone()),
            function_call: None,
        }],
        LanguageModelContent::ToolCall {
            tool_name,
            tool_input,
            ..
        } => {
            let args: serde_json::Value = serde_json::from_str(tool_input).unwrap_or_default();
            vec![GenerateContentPart {
                text: None,
                function_call: Some(GoogleFunctionCall {
                    name: tool_name.clone(),
                    args,
                }),
            }]
        }
        _ => vec![],
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
