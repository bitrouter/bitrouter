//! Apply an [`AppliedPreset`] onto an OpenAI Responses request.
//!
//! Responses carries the system prompt in the top-level `instructions` field.
//! Shallow merge: preset fills any field the request leaves unset.

use crate::routers::routing_table::AppliedPreset;

use super::types::{ResponsesReasoning, ResponsesRequest};

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
    // Responses API nests effort under a `reasoning` object. Inject the
    // whole object when the request omits it; if the request has its own
    // `reasoning` (even with effort unset), leave it alone — the request
    // already opted into reasoning configuration explicitly.
    if request.reasoning.is_none()
        && let Some(effort) = preset.reasoning_effort
    {
        request.reasoning = Some(ResponsesReasoning {
            effort: Some(effort.as_openai_str().to_owned()),
            summary: None,
        });
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
    use crate::models::language::call_options::ReasoningEffort;

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
            reasoning: None,
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
    fn preset_reasoning_effort_wraps_in_object_when_request_unset() {
        let mut req = empty_request();
        let preset = AppliedPreset {
            reasoning_effort: Some(ReasoningEffort::High),
            ..Default::default()
        };
        apply(&mut req, &preset);
        let r = req.reasoning.expect("reasoning object created");
        assert_eq!(r.effort.as_deref(), Some("high"));
    }

    #[test]
    fn request_reasoning_object_wins() {
        let mut req = empty_request();
        req.reasoning = Some(ResponsesReasoning {
            effort: Some("minimal".into()),
            summary: None,
        });
        let preset = AppliedPreset {
            reasoning_effort: Some(ReasoningEffort::High),
            ..Default::default()
        };
        apply(&mut req, &preset);
        let r = req.reasoning.expect("reasoning preserved");
        assert_eq!(r.effort.as_deref(), Some("minimal"));
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
