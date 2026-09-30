use bitrouter_sdk::language_model::types::ReasoningEffort;
use bitrouter_sdk::language_model::{GenerationParams, Message, Prompt, Tool, ToolChoice};

pub(crate) fn build(
    model: &str,
    effort: Option<ReasoningEffort>,
    instructions: &str,
    messages: &[Message],
    tools: Vec<Tool>,
    max_bytes: usize,
) -> Result<Prompt, String> {
    let prompt = Prompt {
        model: model.to_string(),
        system: Some(instructions.to_string()),
        system_provider_metadata: Default::default(),
        messages: messages.to_vec(),
        tools,
        params: GenerationParams {
            reasoning_effort: effort,
            ..Default::default()
        },
        response_format: None,
        tool_choice: Some(ToolChoice::Auto),
        stream: true,
    };
    let size = serde_json::to_vec(&prompt)
        .map_err(|error| format!("cannot encode model context: {error}"))?
        .len();
    if size > max_bytes {
        return Err(format!(
            "model context is {size} bytes, above the {max_bytes}-byte limit"
        ));
    }
    Ok(prompt)
}
