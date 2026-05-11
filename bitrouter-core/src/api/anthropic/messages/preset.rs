//! Apply an [`AppliedPreset`] onto an Anthropic Messages request.
//!
//! Anthropic carries the system prompt as a top-level field
//! (`Option<SystemPrompt>`). Shallow merge: preset's system is used only
//! when the request leaves the field unset; numeric fields follow the
//! same "preset fills None" rule. `max_tokens` is required on the request
//! struct (not Option) so the preset cannot override it.

use crate::routers::routing_table::AppliedPreset;

use super::types::{MessagesRequest, SystemPrompt};

/// Shallow-merges `preset` defaults onto `request`.
pub fn apply(request: &mut MessagesRequest, preset: &AppliedPreset) {
    if preset.is_empty() {
        return;
    }

    if request.temperature.is_none() {
        request.temperature = preset.temperature;
    }
    if request.top_p.is_none() {
        request.top_p = preset.top_p;
    }
    if request.top_k.is_none() {
        request.top_k = preset.top_k;
    }
    if request.stop_sequences.is_none() {
        request.stop_sequences = preset.stop_sequences.clone();
    }

    // System prompt: only set when request has none.
    if request.system.is_none()
        && let Some(system) = &preset.system
    {
        request.system = Some(SystemPrompt::Text(system.clone()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::anthropic::messages::types::AnthropicMessage;

    fn empty_request() -> MessagesRequest {
        MessagesRequest {
            model: "claude-sonnet-4".into(),
            messages: vec![AnthropicMessage {
                role: "user".into(),
                content: None,
            }],
            max_tokens: 1024,
            system: None,
            temperature: None,
            top_p: None,
            top_k: None,
            stop_sequences: None,
            stream: None,
            tools: None,
            tool_choice: None,
            metadata: None,
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
    fn preset_system_used_when_request_has_none() {
        let mut req = empty_request();
        let preset = AppliedPreset {
            system: Some("Reason carefully.".into()),
            ..Default::default()
        };
        apply(&mut req, &preset);
        match req.system {
            Some(SystemPrompt::Text(t)) => assert_eq!(t, "Reason carefully."),
            _ => panic!("expected text system prompt"),
        }
    }

    #[test]
    fn request_system_wins() {
        let mut req = empty_request();
        req.system = Some(SystemPrompt::Text("Be friendly.".into()));
        let preset = AppliedPreset {
            system: Some("Reason carefully.".into()),
            ..Default::default()
        };
        apply(&mut req, &preset);
        match req.system {
            Some(SystemPrompt::Text(t)) => assert_eq!(t, "Be friendly."),
            _ => panic!("expected text system prompt"),
        }
    }
}
