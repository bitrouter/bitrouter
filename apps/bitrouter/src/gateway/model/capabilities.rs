//! Request capability projection shared by gateway routing and core.

use bitrouter_ai::types::{
    Capability, Content, Modality, Prompt, ToolResultContentPart, ToolResultOutput,
};

/// Derive the request requirements used to assess provider routes.
///
/// Response format, tools, reasoning hints and input/output media contribute
/// capabilities in their existing stable order. Web-search and logprobs extras
/// remain catalog-only and are not inferred here. This projection does not
/// select a route or alter the request.
pub(crate) fn required_capabilities(prompt: &Prompt) -> Vec<Capability> {
    let mut caps = Vec::new();
    if prompt.response_format.is_some() {
        caps.push(Capability::StructuredOutputs);
    }
    if !prompt.tools.is_empty()
        || prompt
            .messages
            .iter()
            .flat_map(|message| &message.content)
            .any(|content| {
                matches!(
                    content,
                    Content::ToolCall { .. } | Content::ToolResult { .. }
                )
            })
    {
        caps.push(Capability::Tools);
    }
    if prompt.params.reasoning_effort.is_some() {
        caps.push(Capability::Reasoning);
    }
    // Input modalities: each file part contributes the capability implied by
    // its media type, deduplicated (one `image_input` for several images).
    for content in prompt.messages.iter().flat_map(|m| &m.content) {
        let mut add_media = |media_type: &str| {
            let cap = from_input_media_type(media_type);
            if !caps.contains(&cap) {
                caps.push(cap);
            }
        };
        match content {
            Content::File { media_type, .. } => add_media(media_type),
            Content::ToolResult {
                output: ToolResultOutput::Content { value },
                ..
            } => {
                for part in value {
                    match part {
                        ToolResultContentPart::Media { media_type, .. } => add_media(media_type),
                        ToolResultContentPart::FileId { media_type, .. } => {
                            add_media(media_type.as_deref().unwrap_or("application/octet-stream"))
                        }
                        ToolResultContentPart::Text { .. } => {}
                    }
                }
            }
            _ => {}
        }
    }
    // Output modalities: from the requested response modalities.
    for modality in &prompt.params.response_modalities {
        if let Some(cap) = from_output_modality(*modality)
            && !caps.contains(&cap)
        {
            caps.push(cap);
        }
    }
    caps
}

fn from_input_media_type(media_type: &str) -> Capability {
    match media_type.split('/').next() {
        Some("image") => Capability::ImageInput,
        Some("audio") => Capability::AudioInput,
        Some("video") => Capability::VideoInput,
        _ => Capability::FileInput,
    }
}

fn from_output_modality(modality: Modality) -> Option<Capability> {
    match modality {
        Modality::Image => Some(Capability::ImageOutput),
        Modality::Audio => Some(Capability::AudioOutput),
        Modality::Text => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitrouter_ai::types::{
        DataContent, GenerationParams, Message, ReasoningEffort, ResponseFormat, Role, Tool,
    };

    fn bare_prompt() -> Prompt {
        Prompt {
            model: "m".into(),
            system: None,
            system_provider_metadata: Default::default(),
            messages: vec![],
            tools: vec![],
            params: GenerationParams::default(),
            response_format: None,
            tool_choice: None,
            stream: false,
        }
    }

    #[test]
    fn plain_request_requires_no_capabilities() {
        assert!(required_capabilities(&bare_prompt()).is_empty());
    }

    #[test]
    fn response_format_requires_structured_outputs() {
        let mut p = bare_prompt();
        p.response_format = Some(ResponseFormat::JsonSchema {
            name: None,
            description: None,
            strict: None,
            schema: serde_json::json!({ "type": "object" }),
        });
        assert_eq!(
            required_capabilities(&p),
            vec![Capability::StructuredOutputs]
        );
    }

    #[test]
    fn tools_require_tools_capability() {
        let mut p = bare_prompt();
        p.tools = vec![Tool::Function {
            name: "get_weather".into(),
            description: None,
            parameters: serde_json::json!({ "type": "object" }),
            strict: None,
            provider_metadata: Default::default(),
        }];
        assert_eq!(required_capabilities(&p), vec![Capability::Tools]);
    }

    #[test]
    fn reasoning_effort_requires_reasoning_capability() {
        let mut p = bare_prompt();
        p.params.reasoning_effort = Some(ReasoningEffort::High);
        assert_eq!(required_capabilities(&p), vec![Capability::Reasoning]);
    }

    #[test]
    fn multiple_features_require_all_their_capabilities() {
        let mut p = bare_prompt();
        p.response_format = Some(ResponseFormat::JsonSchema {
            name: None,
            description: None,
            strict: None,
            schema: serde_json::json!({ "type": "object" }),
        });
        p.tools = vec![Tool::Function {
            name: "t".into(),
            description: None,
            parameters: serde_json::json!({}),
            strict: None,
            provider_metadata: Default::default(),
        }];
        p.params.reasoning_effort = Some(ReasoningEffort::Low);
        let caps = required_capabilities(&p);
        assert!(caps.contains(&Capability::StructuredOutputs));
        assert!(caps.contains(&Capability::Tools));
        assert!(caps.contains(&Capability::Reasoning));
        assert_eq!(caps.len(), 3);
    }

    fn user_file_prompt(media_type: &str) -> Prompt {
        let mut p = bare_prompt();
        p.messages = vec![Message {
            role: Role::User,
            content: vec![Content::File {
                media_type: media_type.to_string(),
                data: DataContent::Base64 {
                    data: "AAAA".to_string(),
                },
                filename: None,
                provider_metadata: Default::default(),
            }],
        }];
        p
    }

    #[test]
    fn image_file_requires_image_input() {
        assert_eq!(
            required_capabilities(&user_file_prompt("image/png")),
            vec![Capability::ImageInput]
        );
    }

    #[test]
    fn audio_file_requires_audio_input() {
        assert_eq!(
            required_capabilities(&user_file_prompt("audio/mpeg")),
            vec![Capability::AudioInput]
        );
    }

    #[test]
    fn video_file_requires_video_input() {
        assert_eq!(
            required_capabilities(&user_file_prompt("video/mp4")),
            vec![Capability::VideoInput]
        );
    }

    #[test]
    fn document_file_requires_file_input() {
        assert_eq!(
            required_capabilities(&user_file_prompt("application/pdf")),
            vec![Capability::FileInput]
        );
    }

    #[test]
    fn repeated_image_parts_dedupe_to_one_image_input() {
        let img = || Content::File {
            media_type: "image/png".to_string(),
            data: DataContent::Url {
                url: "https://example.invalid/a.png".to_string(),
            },
            filename: None,
            provider_metadata: Default::default(),
        };
        let mut p = bare_prompt();
        p.messages = vec![Message {
            role: Role::User,
            content: vec![img(), img()],
        }];
        assert_eq!(required_capabilities(&p), vec![Capability::ImageInput]);
    }

    #[test]
    fn response_modalities_require_output_capabilities() {
        let mut p = bare_prompt();
        p.params.response_modalities = vec![Modality::Text, Modality::Image, Modality::Audio];
        // Text implies no capability; image / audio each map to an output cap.
        assert_eq!(
            required_capabilities(&p),
            vec![Capability::ImageOutput, Capability::AudioOutput]
        );
    }

    #[test]
    fn extra_only_features_are_not_auto_detected() {
        // web_search / logprobs ride `params.extra` and are advertise-only (not
        // gated) — a request carrying them must still require no capabilities.
        let mut p = bare_prompt();
        p.params
            .extra
            .insert("logprobs".into(), serde_json::json!(true));
        p.params
            .extra
            .insert("web_search_options".into(), serde_json::json!({}));
        assert!(required_capabilities(&p).is_empty());
    }
}
