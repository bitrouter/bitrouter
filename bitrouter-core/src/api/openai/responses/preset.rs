//! Apply an [`AppliedPreset`] onto an OpenAI Responses request.
//!
//! Responses carries the system prompt in the top-level `instructions` field.
//! Shallow merge: preset fills any field the request leaves unset.

use crate::routers::routing_table::AppliedPreset;

use super::types::ResponsesRequest;

/// Shallow-merges `preset` defaults onto `request`.
///
/// The Responses request struct has fewer generation params than Chat or
/// Anthropic (no top_k, presence/frequency_penalty, stop). Preset fields
/// for those parameters are silently dropped on this protocol.
pub fn apply(request: &mut ResponsesRequest, preset: &AppliedPreset) {
    if preset.is_empty() {
        return;
    }

    if request.temperature.is_none() {
        request.temperature = preset.temperature;
    }
    if request.top_p.is_none() {
        request.top_p = preset.top_p;
    }
    if request.max_output_tokens.is_none() {
        request.max_output_tokens = preset.max_tokens;
    }

    if request.instructions.is_none()
        && let Some(system) = &preset.system
    {
        request.instructions = Some(system.clone());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::openai::responses::types::ResponsesInput;

    fn empty_request() -> ResponsesRequest {
        ResponsesRequest {
            model: "gpt-5".into(),
            input: ResponsesInput::Text("hi".into()),
            instructions: None,
            temperature: None,
            top_p: None,
            max_output_tokens: None,
            stream: None,
            tools: None,
            tool_choice: None,
            parallel_tool_calls: None,
            text: None,
        }
    }

    #[test]
    fn preset_fills_unset_temperature() {
        let mut req = empty_request();
        let preset = AppliedPreset {
            temperature: Some(0.2),
            ..Default::default()
        };
        apply(&mut req, &preset);
        assert_eq!(req.temperature, Some(0.2));
    }

    #[test]
    fn preset_instructions_used_when_request_has_none() {
        let mut req = empty_request();
        let preset = AppliedPreset {
            system: Some("Reason carefully.".into()),
            ..Default::default()
        };
        apply(&mut req, &preset);
        assert_eq!(req.instructions.as_deref(), Some("Reason carefully."));
    }

    #[test]
    fn request_instructions_wins() {
        let mut req = empty_request();
        req.instructions = Some("Be friendly.".into());
        let preset = AppliedPreset {
            system: Some("Reason carefully.".into()),
            ..Default::default()
        };
        apply(&mut req, &preset);
        assert_eq!(req.instructions.as_deref(), Some("Be friendly."));
    }
}
