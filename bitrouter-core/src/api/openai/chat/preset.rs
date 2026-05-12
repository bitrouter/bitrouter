//! Apply an [`AppliedPreset`] onto an OpenAI Chat Completions request.
//!
//! Semantics (OpenRouter-style shallow merge): for each field the preset
//! provides a default — the request wins on any field it explicitly sets.
//! The system prompt is special-cased because OpenAI Chat carries system
//! content inside `messages[]` rather than as a top-level field.

use crate::routers::routing_table::AppliedPreset;

use super::types::{ChatCompletionRequest, ChatMessage, ChatMessageContent};

/// Shallow-merges `preset` defaults onto `request`.
///
/// Returns immediately if the preset has no fields populated.
pub fn apply(request: &mut ChatCompletionRequest, preset: &AppliedPreset) {
    if preset.is_empty() {
        return;
    }

    // Scalars: request wins when set.
    if request.temperature.is_none() {
        request.temperature = preset.temperature;
    }
    if request.top_p.is_none() {
        request.top_p = preset.top_p;
    }
    if request.max_tokens.is_none() && request.max_completion_tokens.is_none() {
        request.max_tokens = preset.max_tokens;
    }
    if request.stop.is_none() {
        request.stop = preset.stop_sequences.clone();
    }
    if request.presence_penalty.is_none() {
        request.presence_penalty = preset.presence_penalty;
    }
    if request.frequency_penalty.is_none() {
        request.frequency_penalty = preset.frequency_penalty;
    }
    if request.reasoning_effort.is_none()
        && let Some(effort) = preset.reasoning_effort
    {
        request.reasoning_effort = Some(effort.as_openai_str().to_owned());
    }

    // System prompt: only inject when the request has no system message of its own.
    if let Some(system) = &preset.system
        && !request.messages.iter().any(|m| m.role == "system")
    {
        request.messages.insert(
            0,
            ChatMessage {
                role: "system".to_owned(),
                content: Some(ChatMessageContent::Text(system.clone())),
                tool_call_id: None,
                tool_calls: None,
                name: None,
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::language::call_options::ReasoningEffort;

    fn empty_request() -> ChatCompletionRequest {
        ChatCompletionRequest {
            model: "gpt-4o".into(),
            messages: vec![ChatMessage {
                role: "user".into(),
                content: Some(ChatMessageContent::Text("hi".into())),
                tool_call_id: None,
                tool_calls: None,
                name: None,
            }],
            temperature: None,
            top_p: None,
            max_tokens: None,
            max_completion_tokens: None,
            stop: None,
            presence_penalty: None,
            frequency_penalty: None,
            seed: None,
            stream: None,
            stream_options: None,
            tools: None,
            tool_choice: None,
            parallel_tool_calls: None,
            response_format: None,
            reasoning_effort: None,
        }
    }

    #[test]
    fn preset_fills_unset_scalars() {
        let mut req = empty_request();
        let preset = AppliedPreset {
            temperature: Some(0.2),
            ..Default::default()
        };
        apply(&mut req, &preset);
        assert_eq!(req.temperature, Some(0.2));
    }

    #[test]
    fn request_temperature_wins() {
        let mut req = empty_request();
        req.temperature = Some(0.9);
        let preset = AppliedPreset {
            temperature: Some(0.2),
            ..Default::default()
        };
        apply(&mut req, &preset);
        assert_eq!(req.temperature, Some(0.9));
    }

    #[test]
    fn preset_system_prepended_when_absent() {
        let mut req = empty_request();
        let preset = AppliedPreset {
            system: Some("Reason carefully.".into()),
            ..Default::default()
        };
        apply(&mut req, &preset);
        assert_eq!(req.messages.len(), 2);
        assert_eq!(req.messages[0].role, "system");
    }

    #[test]
    fn preset_reasoning_effort_fills_when_request_unset() {
        let mut req = empty_request();
        let preset = AppliedPreset {
            reasoning_effort: Some(ReasoningEffort::High),
            ..Default::default()
        };
        apply(&mut req, &preset);
        assert_eq!(req.reasoning_effort.as_deref(), Some("high"));
    }

    #[test]
    fn request_reasoning_effort_wins() {
        let mut req = empty_request();
        req.reasoning_effort = Some("xhigh".into());
        let preset = AppliedPreset {
            reasoning_effort: Some(ReasoningEffort::Low),
            ..Default::default()
        };
        apply(&mut req, &preset);
        assert_eq!(req.reasoning_effort.as_deref(), Some("xhigh"));
    }

    #[test]
    fn preset_system_skipped_when_request_has_system() {
        let mut req = empty_request();
        req.messages.insert(
            0,
            ChatMessage {
                role: "system".into(),
                content: Some(ChatMessageContent::Text("Be friendly.".into())),
                tool_call_id: None,
                tool_calls: None,
                name: None,
            },
        );
        let preset = AppliedPreset {
            system: Some("Reason carefully.".into()),
            ..Default::default()
        };
        apply(&mut req, &preset);
        // Still only one system message — the existing one.
        assert_eq!(
            req.messages.iter().filter(|m| m.role == "system").count(),
            1
        );
        // And it's the original.
        let sys = &req.messages[0];
        assert_eq!(sys.role, "system");
        match sys.content.as_ref() {
            Some(ChatMessageContent::Text(t)) => assert_eq!(t, "Be friendly."),
            _ => panic!("expected text content"),
        }
    }
}
